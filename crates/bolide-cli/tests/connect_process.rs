//! The real binary, daemonising for real, against the in-process fake desktop: a
//! password given on the command line authenticates, and then appears nowhere — not in
//! the daemon's argv as `ps` shows it, not in the state file, not in the log, not in
//! `bolide status`.

mod common;

use std::path::Path;
use std::process::{Command, Output};

use bolide_testkit::{FakeServer, Screen};

use common::TempDir;

const PASSWORD: &str = "hunter2-7f3a";

fn bolide(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bolide"))
        .args(args)
        .env("HOME", home)
        .env("XDG_STATE_HOME", home.join("state"))
        .env_remove("BOLIDE_PASSWORD")
        .output()
        .expect("bolide runs")
}

/// Disconnects on drop, so a failed assertion does not leave a daemon behind.
struct Session<'a>(&'a Path);

impl Drop for Session<'_> {
    fn drop(&mut self) {
        let _ = bolide(self.0, &["disconnect"]);
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Everything readable under `dir`, concatenated.
fn everything_in(dir: &Path) -> String {
    let mut all = String::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                all.push_str(&everything_in(&path));
            } else if let Ok(body) = std::fs::read(&path) {
                all.push_str(&text(&body));
            }
        }
    }
    all
}

async fn connect_and_check(tag: &str, target_for: impl Fn(u16) -> Vec<String>) {
    let server = FakeServer::builder()
        .screen(Screen::solid(64, 48, [0, 128, 255, 255]))
        .password(PASSWORD)
        .start()
        .await
        .expect("fake desktop");
    let port = server.addr().port();
    let home = TempDir::new(tag);
    let home_path = home.path().to_path_buf();

    let args = target_for(port);
    let connected = tokio::task::spawn_blocking(move || {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        bolide(&home_path, &args)
    })
    .await
    .unwrap();
    let _session = Session(home.path());
    assert!(
        connected.status.success(),
        "connect failed: {}{}",
        text(&connected.stdout),
        text(&connected.stderr)
    );

    let state =
        everything_in(&home.path().join("state")) + &everything_in(&home.path().join("Library"));
    assert!(!state.is_empty(), "no state file was written");
    assert!(
        !state.contains(PASSWORD),
        "the password reached disk: {state}"
    );

    let session: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(find_session_json(home.path()).expect("session.json")).unwrap(),
    )
    .unwrap();
    let pid = session["pid"].as_u64().expect("a pid").to_string();
    assert_eq!(session["remote"], format!("127.0.0.1:{port}"));

    let ps = Command::new("ps")
        .args(["-ww", "-o", "args=", "-p", &pid])
        .output()
        .expect("ps runs");
    let daemon_argv = text(&ps.stdout);
    assert!(
        daemon_argv.contains("--foreground"),
        "that is not the daemon: {daemon_argv:?}"
    );
    assert!(
        !daemon_argv.contains(PASSWORD),
        "the daemon's argv shows the password to every local user: {daemon_argv}"
    );

    let status = bolide(home.path(), &["status"]);
    let status_out = text(&status.stdout) + &text(&status.stderr);
    assert!(status.status.success(), "{status_out}");
    assert!(!status_out.contains(PASSWORD), "{status_out}");
    assert!(
        status_out.contains(&format!("127.0.0.1:{port}")),
        "{status_out}"
    );

    drop(_session);
    server.shutdown().await;
}

fn find_session_json(dir: &Path) -> Option<std::path::PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_session_json(&path) {
                return Some(found);
            }
        } else if path.file_name().is_some_and(|n| n == "session.json") {
            return Some(path);
        }
    }
    None
}

#[tokio::test(flavor = "multi_thread")]
async fn a_password_flag_authenticates_and_goes_nowhere_else() {
    connect_and_check("proc-flag", |port| {
        vec![
            "connect".into(),
            format!("vnc://127.0.0.1:{port}"),
            "--password".into(),
            PASSWORD.into(),
        ]
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_url_password_authenticates_and_goes_nowhere_else() {
    connect_and_check("proc-url", |port| {
        vec![
            "connect".into(),
            format!("vnc://jake:{PASSWORD}@127.0.0.1:{port}"),
        ]
    })
    .await;
}
