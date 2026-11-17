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
