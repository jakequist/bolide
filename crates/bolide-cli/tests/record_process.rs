//! `bolide connect --record` as a user runs it: the real binary, daemonised, against the
//! in-process fake desktop. `bolide disconnect` (a SIGTERM to the daemon) leaves a
//! finished video behind; without ffmpeg, `connect` stops before it starts anything.

mod common;
mod ffmpeg;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use bolide_cli::{state, state_dir_for, THIS_PLATFORM};
use bolide_testkit::{FakeServer, Screen};

use common::TempDir;

/// The state-directory variables each `bolide` here runs with.
fn state_env(home: &Path) -> [(&'static str, PathBuf); 2] {
    [
        ("HOME", home.to_path_buf()),
        ("XDG_STATE_HOME", home.join("state")),
    ]
}

/// Where the daemon's log lands under `state_env(home)`, asked of bolide itself: macOS
/// keeps state under `~/Library/Application Support` and ignores `XDG_STATE_HOME`.
fn daemon_log(home: &Path) -> PathBuf {
    let env = state_env(home);
    let lookup = |key: &str| {
        env.iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.to_string_lossy().into_owned())
    };
    state::log_path(&state_dir_for(THIS_PLATFORM, &lookup).expect("a state directory"))
}

fn bolide(home: &Path, path: Option<&std::ffi::OsStr>, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bolide"));
    command
        .args(args)
        .current_dir(home)
        .envs(state_env(home))
        .env_remove("BOLIDE_PASSWORD");
    if let Some(path) = path {
        command.env("PATH", path);
    }
    command.output().expect("bolide runs")
}

fn text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr)
}

#[tokio::test(flavor = "multi_thread")]
async fn disconnect_leaves_a_finished_recording() {
    if !ffmpeg::have_ffmpeg("disconnect_leaves_a_finished_recording") {
        return;
    }
    let server = FakeServer::builder()
        .screen(Screen::solid(64, 48, [255, 0, 0, 255]))
        .start()
        .await
        .unwrap();
    let target = format!("vnc://127.0.0.1:{}", server.addr().port());
    let home = TempDir::new("rec-proc");
    let home_path = home.path().to_path_buf();

    let (connected, disconnected) = tokio::task::spawn_blocking(move || {
        // Relative, so the daemon has to be told where the shell meant.
        let connected = bolide(
            &home_path,
            None,
            &["connect", &target, "--record", "run.mp4"],
        );
        let disconnected = bolide(&home_path, None, &["disconnect"]);
        (connected, disconnected)
    })
    .await
    .unwrap();
    assert!(connected.status.success(), "connect: {}", text(&connected));
    assert!(
        disconnected.status.success(),
        "disconnect: {}",
        text(&disconnected)
    );

    let log = std::fs::read_to_string(daemon_log(home.path())).expect("the daemon's log");
    assert!(
        log.contains("recording to ") && log.contains("frames to "),
        "the daemon did not say it finalised the recording:\n{log}"
    );

    let file = home.path().join("run.mp4");
    assert!(file.is_file(), "no recording at {}", file.display());
    let probe = ffmpeg::probe(&file);
    assert_eq!(probe.codec, "h264", "{probe:?}");
    assert_eq!((probe.width, probe.height), (64, 48), "{probe:?}");
    assert!(probe.frames > 0, "{probe:?}");
    assert!(probe.duration > 0.0, "{probe:?}");
    // Every frame decodes. The last is red, or black when `disconnect` came before the
    // first FramebufferUpdate did: the framebuffer is black until painted, and a
    // recording shows what bolide had. `record_ffmpeg.rs` pins the colours exactly.
    let centre = ffmpeg::last_frame_centre(&file, 64, 48);
    assert!(
        ffmpeg::close_to(centre, [255, 0, 0]) || ffmpeg::close_to(centre, [0, 0, 0]),
        "neither the desktop's red nor the unpainted black: {centre:?}"
    );

    server.shutdown().await;
}

#[test]
fn without_ffmpeg_connect_stops_before_it_starts() {
    let home = TempDir::new("rec-noff");
    let empty = home.path().join("empty-path");
    std::fs::create_dir_all(&empty).unwrap();

    // Nothing listens on port 1; a connect that got as far as dialling would fail with
    // a different message.
    let out = bolide(
        home.path(),
        Some(empty.as_os_str()),
        &["connect", "vnc://127.0.0.1:1", "--record", "run.webm"],
    );
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    let said = text(&out);
    assert!(
        said.contains("ffmpeg") && said.contains("--record"),
        "{said}"
    );
    assert!(
        !daemon_log(home.path()).exists(),
        "it started a daemon anyway"
    );
}

/// A directory holding an `ffmpeg` that writes its arguments, one a line, to the file
/// `BOLIDE_TEST_FFMPEG_ARGS` names, then reads its stdin to the end and exits 0: what a
/// session hands the encoder, without needing one. Returns a PATH with it first.
fn fake_ffmpeg(home: &Path) -> std::ffi::OsString {
    use std::os::unix::fs::PermissionsExt;
    let dir = home.join("fake-bin");
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("ffmpeg");
    std::fs::write(
        &script,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$BOLIDE_TEST_FFMPEG_ARGS\"\ncat > /dev/null\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut path = vec![dir];
    path.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    std::env::join_paths(path).unwrap()
}

/// The arguments the fake ffmpeg was started with, after a session that recorded with
/// `extra` on its `connect`.
async fn ffmpeg_args_for(tag: &str, extra: &[&str]) -> Vec<String> {
    let server = FakeServer::builder()
        .screen(Screen::solid(64, 48, [255, 0, 0, 255]))
        .start()
        .await
        .unwrap();
    let home = TempDir::new(tag);
    let path = fake_ffmpeg(home.path());
    let args_file = home.path().join("ffmpeg-args");
    let mut line: Vec<String> = vec![
        "connect".into(),
        format!("vnc://127.0.0.1:{}", server.addr().port()),
        "--record".into(),
        "run.mp4".into(),
    ];
    line.extend(extra.iter().map(|a| a.to_string()));
    let home_path = home.path().to_path_buf();
    let args_env = args_file.clone();
    let (connected, disconnected) = tokio::task::spawn_blocking(move || {
        let run = |args: &[&str]| {
            Command::new(env!("CARGO_BIN_EXE_bolide"))
                .args(args)
                .current_dir(&home_path)
                .envs(state_env(&home_path))
                .env_remove("BOLIDE_PASSWORD")
                .env("PATH", &path)
                .env("BOLIDE_TEST_FFMPEG_ARGS", &args_env)
                .output()
                .expect("bolide runs")
        };
        let line: Vec<&str> = line.iter().map(String::as_str).collect();
        (run(&line), run(&["disconnect"]))
    })
    .await
    .unwrap();
    assert!(connected.status.success(), "connect: {}", text(&connected));
    assert!(
        disconnected.status.success(),
        "disconnect: {}",
        text(&disconnected)
    );
    server.shutdown().await;
    std::fs::read_to_string(&args_file)
        .unwrap_or_else(|e| panic!("the fake ffmpeg wrote no arguments ({e})"))
        .lines()
        .map(String::from)
        .collect()
}

/// With `--record`, the action log's header names the video and its rate, and its
/// offsets count from the video's first frame, which is no earlier than the connect.
#[tokio::test(flavor = "multi_thread")]
async fn the_action_log_names_the_recording_it_goes_with() {
    let server = FakeServer::builder()
        .screen(Screen::solid(64, 48, [255, 0, 0, 255]))
        .start()
        .await
        .unwrap();
    let home = TempDir::new("rec-alog");
    let path = fake_ffmpeg(home.path());
    let target = format!("vnc://127.0.0.1:{}", server.addr().port());
    let flag = format!("--{}", bolide_cli::action_log::ACTION_LOG_FLAG);
    let home_path = home.path().to_path_buf();
    let args_file = home.path().join("ffmpeg-args");
    let (outputs, log) = tokio::task::spawn_blocking(move || {
        let run = |args: &[&str]| {
            Command::new(env!("CARGO_BIN_EXE_bolide"))
                .args(args)
                .current_dir(&home_path)
                .envs(state_env(&home_path))
                .env_remove("BOLIDE_PASSWORD")
                .env("PATH", &path)
                .env("BOLIDE_TEST_FFMPEG_ARGS", &args_file)
                .output()
                .expect("bolide runs")
        };
        let connected = run(&[
            "connect",
            &target,
            "--record",
            "run.mp4",
            "--record-fps",
            "12",
            &flag,
            "run.jsonl",
        ]);
        let moved = run(&["move", "5", "5"]);
        let log = std::fs::read_to_string(home_path.join("run.jsonl"));
        let disconnected = run(&["disconnect"]);
        ([connected, moved, disconnected], log)
    })
    .await
    .unwrap();
    for output in &outputs {
        assert!(output.status.success(), "{}", text(output));
    }
    server.shutdown().await;

    let log = log.expect("the action log");
    let lines: Vec<serde_json::Value> = log
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let record = &lines[0]["record"];
    assert_eq!(
        record["file"],
        // On macOS the temp dir is under /var, a symlink to /private/var; the
        // child reports its working directory resolved, so compare canonically.
        serde_json::json!(std::fs::canonicalize(home.path())
            .unwrap()
            .join("run.mp4")
            .display()
            .to_string()),
        "{log}"
    );
    assert_eq!(record["fps"], serde_json::json!(12), "{log}");
    let (connected, start) = (
        lines[0]["connected"].as_str().unwrap(),
        record["start"].as_str().unwrap(),
    );
    assert!(connected <= start, "{log}");
    assert_eq!(lines[1]["action"], serde_json::json!("mouse_move"), "{log}");
    assert!(lines[1]["startS"].as_f64().unwrap() >= 0.0, "{log}");
}

fn rate_of(args: &[String]) -> Option<&str> {
    args.windows(2)
        .find(|w| w[0] == "-r")
        .map(|w| w[1].as_str())
}

/// The production start path: `--record-fps` goes through the daemon's re-exec to the
/// encoder, and without it the rate is 30.
#[tokio::test(flavor = "multi_thread")]
async fn the_recording_rate_reaches_ffmpeg() {
    let args = ffmpeg_args_for("rec-fps-default", &[]).await;
    assert_eq!(rate_of(&args), Some("30"), "{args:?}");

    let args = ffmpeg_args_for("rec-fps-12", &["--record-fps", "12"]).await;
    assert_eq!(rate_of(&args), Some("12"), "{args:?}");
}
