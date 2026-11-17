//! `bolide connect --action-log` as a user runs it: the real binary, daemonised, against
//! the in-process fake desktop, with no recording. Each command that acts on the desktop
//! is a line, on disk while the session still runs; `status` is not.

mod common;

use std::path::Path;
use std::process::{Command, Output};

use bolide_testkit::{FakeServer, Screen};
use serde_json::{json, Value};

use common::TempDir;

fn bolide(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bolide"))
        .args(args)
        .current_dir(home)
        .env("HOME", home)
        .env("XDG_STATE_HOME", home.join("state"))
        .env_remove("BOLIDE_PASSWORD")
        .output()
        .expect("bolide runs")
}

fn text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr)
}

/// Disconnects on drop, so a failed assertion does not leave a daemon behind.
struct Session<'a>(&'a Path);

impl Drop for Session<'_> {
    fn drop(&mut self) {
        let _ = bolide(self.0, &["disconnect"]);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_recording_each_act_is_a_line_on_disk_before_the_session_ends() {
    let server = FakeServer::builder()
        .screen(Screen::solid(64, 48, [0, 128, 255, 255]))
        .start()
        .await
        .expect("fake desktop");
    let target = format!("vnc://127.0.0.1:{}", server.addr().port());
    let home = TempDir::new("alog-proc");
    let home_path = home.path().to_path_buf();
    let flag = format!("--{}", bolide_cli::action_log::ACTION_LOG_FLAG);

    let (outputs, log) = tokio::task::spawn_blocking(move || {
        // Relative, so the daemon has to be told where the shell meant.
        let connected = bolide(&home_path, &["connect", &target, &flag, "run.jsonl"]);
        let _session = Session(&home_path);
        let mut outputs = vec![("connect", connected)];
        for (name, args) in [
            ("click", &["click", "10", "10"][..]),
            ("type", &["type", "hi there"]),
            ("status", &["status"]),
            ("screenshot", &["screenshot", "--out", "s.png"]),
            ("push", &["clipboard", "push", "copied"]),
            ("drag", &["drag", "1", "2", "30", "40"]),
        ] {
            outputs.push((name, bolide(&home_path, args)));
        }
        // Pulled before the session ends: whatever is on disk now is what a crash
        // would leave.
        let log = std::fs::read_to_string(home_path.join("run.jsonl"));
        (outputs, log)
    })
    .await
    .unwrap();
    for (name, output) in &outputs {
        assert!(output.status.success(), "{name}: {}", text(output));
    }
    server.shutdown().await;

    let log = log.expect("the action log exists while the session runs");
    let lines: Vec<Value> = log
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{l:?}: {e}")))
        .collect();
    let header = &lines[0];
    assert_eq!(header["desktop"], json!({"width": 64, "height": 48}));
    assert_eq!(header["record"], Value::Null);
    assert_eq!(header["pointer"], json!("direct"));
    assert_eq!(header["typing"], json!("direct"));

    let body = &lines[1..];
    let said: Vec<(&str, &Value)> = body
        .iter()
        .map(|l| (l["action"].as_str().unwrap(), &l["params"]))
        .collect();
    assert_eq!(
        said,
        vec![
            ("left_click", &json!({"coordinate": [10, 10]})),
            ("type", &json!({"text": "hi there"})),
            ("screenshot", &json!({})),
            ("clipboard_set", &json!({"text": "copied"})),
            (
                "left_click_drag",
                &json!({"start_coordinate": [1, 2], "coordinate": [30, 40]})
            ),
        ],
        "{log}"
    );

    // In order, and each line inside itself in order, on both clocks.
    let mut last_end = (0.0, String::new());
    for (n, line) in body.iter().enumerate() {
        assert_eq!(line["seq"], json!(n + 1), "{line}");
        assert_eq!(line["ok"], json!(true), "{line}");
        let (start_s, end_s) = (
            line["startS"].as_f64().unwrap(),
            line["endS"].as_f64().unwrap(),
        );
        let (start, end) = (
            line["start"].as_str().unwrap().to_string(),
            line["end"].as_str().unwrap().to_string(),
        );
        assert!(last_end.0 <= start_s && start_s <= end_s, "{line}");
        // Same format, fixed width: in order as strings is in order as times.
        assert!(last_end.1 <= start && start <= end, "{line}");
        assert!(start.ends_with('Z') && start.len() == "2025-11-13T02:13:20.123Z".len());
        last_end = (end_s, end);
    }
}

/// The production start path for `--typing humanized --seed 1`: the desktop receives
/// exactly the keys of the text and the chord, and each act's line spans at least the
/// waits its plan holds between its first key and its last — the reply comes once the
/// last key has gone out, not when the first was queued.
#[tokio::test(flavor = "multi_thread")]
async fn humanized_typing_types_exactly_and_each_line_covers_its_keys() {
    let server = FakeServer::builder()
        .screen(Screen::solid(64, 48, [0, 128, 255, 255]))
        .start()
        .await
        .expect("fake desktop");
    let target = format!("vnc://127.0.0.1:{}", server.addr().port());
    let home = TempDir::new("alog-typing");
    let home_path = home.path().to_path_buf();
    let log_flag = format!("--{}", bolide_cli::action_log::ACTION_LOG_FLAG);
    let typing_flag = format!("--{}", bolide_cli::cli::TYPING_FLAG);
    const TEXT: &str = "Hi, Bob! aa";
    const CHORD: &str = "ctrl+shift+t";

    let (outputs, log) = tokio::task::spawn_blocking(move || {
        let connected = bolide(
            &home_path,
            &[
                "connect",
                &target,
                &typing_flag,
                "humanized",
                "--seed",
                "1",
                &log_flag,
                "run.jsonl",
            ],
        );
        let _session = Session(&home_path);
        let outputs = vec![
            ("connect", connected),
            ("type", bolide(&home_path, &["type", TEXT])),
            ("key", bolide(&home_path, &["key", CHORD])),
        ];
        let log = std::fs::read_to_string(home_path.join("run.jsonl"));
        (outputs, log)
    })
    .await
    .unwrap();
    for (name, output) in &outputs {
        assert!(output.status.success(), "{name}: {}", text(output));
    }
    let keys: Vec<(u32, bool)> = server
        .events()
        .await
        .into_iter()
        .filter_map(|e| match e {
            bolide_testkit::ClientEvent::Key { keysym, down } => Some((keysym, down)),
            _ => None,
        })
        .collect();
    server.shutdown().await;

    // The same stream the server drew from: seed 1, the type first, then the key.
    let mut random = bolide_server::SeededRandom::new(1);
    let mut planned = Vec::new();
    let mut waits = Vec::new();
    for act in [
        bolide_server::wire::ComputerAction::Type { text: TEXT.into() },
        bolide_server::wire::ComputerAction::Key { text: CHORD.into() },
    ] {
        let ops = bolide_server::plan_typing(&act, &mut random)
            .expect("planned")
            .expect("a typed act");
        let mut waited = 0;
        for op in ops {
            match op {
                bolide_server::RfbOp::Key { keysym, down } => planned.push((keysym, down)),
                bolide_server::RfbOp::Sleep { ms } => waited += ms,
                bolide_server::RfbOp::Pointer { .. } => panic!("typing moved the pointer"),
            }
        }
        waits.push(waited);
    }
    assert_eq!(keys, planned, "the desktop got other keys than the plan");
    let downs: Vec<u32> = keys.iter().filter(|k| k.1).map(|k| k.0).collect();
    let mut want: Vec<u32> = TEXT
        .chars()
        .map(bolide_rfb::keysym::keysym_for_char)
        .collect();
    want.extend(bolide_rfb::keysym::parse_chord(CHORD).expect("a chord"));
    assert_eq!(
        downs, want,
        "the text that arrived is not the text asked for"
    );

    let log = log.expect("the action log exists while the session runs");
    let lines: Vec<Value> = log
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{l:?}: {e}")))
        .collect();
    assert_eq!(lines[0]["typing"], json!("humanized"));
    let body = &lines[1..];
    assert_eq!(body.len(), 2, "{log}");
    for (line, (name, waited)) in body.iter().zip([("type", waits[0]), ("key", waits[1])]) {
        assert_eq!(line["action"], json!(name), "{line}");
        let took_ms = (line["endS"].as_f64().unwrap() - line["startS"].as_f64().unwrap()) * 1000.0;
        // The log's seconds are rounded to the millisecond; allow that, and no more.
        assert!(
            took_ms + 2.0 >= waited as f64,
            "{name} took {took_ms} ms of its planned {waited} ms: {line}"
        );
        assert!(waited > 100, "{name} planned only {waited} ms");
    }
}
