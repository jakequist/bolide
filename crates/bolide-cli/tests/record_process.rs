//! `bolide connect --record` as a user runs it: the real binary, daemonised, against the
//! in-process fake desktop. `bolide disconnect` (a SIGTERM to the daemon) leaves a
//! finished video behind; without ffmpeg, `connect` stops before it starts anything.

mod common;
mod ffmpeg;

use std::path::Path;
use std::process::{Command, Output};

use bolide_testkit::{FakeServer, Screen};

use common::TempDir;

fn bolide(home: &Path, path: Option<&std::ffi::OsStr>, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bolide"));
    command
        .args(args)
        .current_dir(home)
        .env("HOME", home)
        .env("XDG_STATE_HOME", home.join("state"))
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

    let log = std::fs::read_to_string(home.path().join("state/bolide/bolide.log"))
        .expect("the daemon's log");
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
        !home.path().join("state/bolide/bolide.log").exists(),
        "it started a daemon anyway"
    );
}
