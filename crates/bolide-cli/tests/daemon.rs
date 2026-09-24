//! The daemonising protocol's testable half: what the child is told to do, and how the
//! parent waits for it.
//!
//! `spawn_detached` itself (fork/exec + `setsid`) is not covered here — see its docs.

mod common;

use std::cell::Cell;

use bolide_cli::cli::{command_line_credentials, parse_args, parse_target, Command, Password};
use bolide_cli::daemon::{self, ChildStatus};
use bolide_cli::error::EXIT_FAILURE;
use bolide_cli::run::PASSWORD_ENV;
use bolide_cli::state::{self, SessionState};

use common::TempDir;

fn a_state() -> SessionState {
    SessionState {
        endpoint: "http://127.0.0.1:53211".into(),
        pid: std::process::id(),
        remote: "mac01.local:5900".into(),
        username: None,
        token: None,
        started_at: "2026-09-15T22:41:03Z".into(),
    }
}

/// The child's argv for a `connect` command line (without the program name).
fn child_of(line: &[&str]) -> Vec<String> {
    let argv: Vec<String> = std::iter::once("bolide")
        .chain(line.iter().copied())
        .map(String::from)
        .collect();
    let args = match parse_args(&argv).expect("a valid command line").command {
        Command::Connect(c) => c,
        other => panic!("not connect: {other:?}"),
    };
    let target = parse_target(&args.target).expect("a valid target");
    let creds = command_line_credentials(&target, &args).expect("consistent credentials");
    daemon::child_args(&args, &target, &creds)
}

#[test]
fn the_child_is_re_execed_with_foreground() {
    assert_eq!(
        child_of(&["connect", "vnc://mac01.local"]),
        vec![
            "connect",
            "vnc://mac01.local:5900",
            "--listen",
            "127.0.0.1:0",
            "--foreground"
        ]
    );
}

#[test]
fn the_child_keeps_every_non_secret_option() {
    assert_eq!(
        child_of(&[
            "connect",
            "vnc://[::1]:5901",
            "--username",
            "jake",
            "--password-file",
            "/tmp/p",
            "--listen",
            "127.0.0.1:53211",
            "--token",
            "t",
            "--foreground",
        ]),
        vec![
            "connect",
            "vnc://[::1]:5901",
            "--username",
            "jake",
            "--password-file",
            "/tmp/p",
            "--listen",
            "127.0.0.1:53211",
            "--token",
            "t",
            "--foreground",
        ]
    );
}

#[test]
fn the_child_never_sees_the_password_in_its_argv() {
    for line in [
        vec!["connect", "vnc://h", "--password", "hunter2"],
        vec!["connect", "--password=hunter2", "vnc://h"],
        vec!["connect", "vnc://jake:hunter2@h"],
        vec!["connect", "vnc://jake:hunter%32@h"],
    ] {
        let child = child_of(&line);
        assert!(
            !child
                .iter()
                .any(|a| a.contains("hunter") || a.starts_with("--password")),
            "the daemon's argv would show it in `ps` for its whole life: {child:?}"
        );
        assert!(child.contains(&"vnc://h:5900".to_string()), "{child:?}");
    }
}

#[test]
fn a_url_username_reaches_the_child_as_a_flag() {
    let child = child_of(&["connect", "vnc://jake:hunter2@h"]);
    let at = child
        .iter()
        .position(|a| a == "--username")
        .expect("no --username");
    assert_eq!(child[at + 1], "jake");
}

#[test]
fn a_command_line_password_reaches_the_child_through_its_environment() {
    let password: Password = "hunter2".parse().unwrap();
    assert_eq!(
        daemon::child_env(Some(&password)),
        vec![(PASSWORD_ENV.to_string(), "hunter2".to_string())]
    );
    assert!(daemon::child_env(None).is_empty());
}

#[test]
fn the_parent_waits_until_the_child_has_written_the_state_file() {
    let dir = TempDir::new("await-ok");
    let polls = Cell::new(0);
    let mut poll = || {
        polls.set(polls.get() + 1);
        if polls.get() == 3 {
            state::save_in(dir.path(), &a_state()).unwrap();
        }
        Ok(ChildStatus::Running)
    };
    let naps = Cell::new(0);
    let nap = || naps.set(naps.get() + 1);

    let got = daemon::await_startup(
        dir.path(),
        std::process::id(),
        &mut poll,
        &nap,
        50,
        &dir.path().join("bolide.log"),
    )
    .expect("the state file appeared");
    assert_eq!(got, a_state());
    assert!(
        polls.get() >= 3,
        "returned before the child had written anything"
    );
}

#[test]
fn another_session_s_state_file_is_not_mistaken_for_the_child_s() {
    let dir = TempDir::new("await-other");
    state::save_in(
        dir.path(),
        &SessionState {
            pid: std::process::id(),
            endpoint: "http://127.0.0.1:1".into(),
            ..a_state()
        },
    )
    .unwrap();
    // The child pid we are waiting for is not the one in that file, so waiting continues
    // and eventually times out rather than reporting somebody else's endpoint.
    let mut poll = || Ok(ChildStatus::Running);
    let nap = || {};
    let err = daemon::await_startup(
        dir.path(),
        std::process::id() + 1,
        &mut poll,
        &nap,
        3,
        &dir.path().join("bolide.log"),
    )
    .unwrap_err();
    assert_eq!(err.code, EXIT_FAILURE);
}

#[test]
fn a_child_that_dies_first_is_reported_from_its_log() {
    let dir = TempDir::new("await-dead");
    let log = dir.path().join("bolide.log");
    std::fs::write(
        &log,
        "connecting to mac01.local:5900\nerror: authentication failed\n",
    )
    .unwrap();
    let mut poll = || Ok(ChildStatus::Exited(1));
    let nap = || {};

    let err = daemon::await_startup(dir.path(), 12345, &mut poll, &nap, 50, &log).unwrap_err();
    assert_eq!(err.code, EXIT_FAILURE);
    assert!(
        err.message.contains("authentication failed"),
        "the parent must relay the child's error: {}",
        err.message
    );
    assert!(
        err.message.contains(log.to_str().unwrap()),
        "and name the log: {}",
        err.message
    );
}

#[test]
fn waiting_for_ever_is_not_an_option() {
    let dir = TempDir::new("await-timeout");
    let naps = Cell::new(0);
    let nap = || naps.set(naps.get() + 1);
    let mut poll = || Ok(ChildStatus::Running);

    let err = daemon::await_startup(
        dir.path(),
        12345,
        &mut poll,
        &nap,
        5,
        &dir.path().join("bolide.log"),
    )
    .unwrap_err();
    assert_eq!(err.code, EXIT_FAILURE);
    assert!(err.message.contains("bolide.log"), "{}", err.message);
    assert_eq!(naps.get(), 5, "the attempt budget is the timeout");
}

#[test]
fn the_log_tail_is_the_last_few_lines_and_nothing_else() {
    let dir = TempDir::new("tail");
    let log = dir.path().join("bolide.log");
    let body: String = (1..=20).map(|n| format!("line {n}\n")).collect();
    std::fs::write(&log, body).unwrap();
    let tail = daemon::log_tail(&log, 3);
    assert_eq!(tail, "line 18\nline 19\nline 20");

    assert_eq!(daemon::log_tail(&dir.path().join("missing.log"), 3), "");
}
