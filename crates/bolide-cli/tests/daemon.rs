//! The daemonising protocol's testable half: what the child is told to do, and how the
//! parent waits for it.
//!
//! `spawn_detached` itself (fork/exec + `setsid`) is not covered here — see its docs.

mod common;

use std::cell::Cell;

use bolide_cli::daemon::{self, ChildStatus};
use bolide_cli::error::EXIT_FAILURE;
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

#[test]
fn the_child_is_re_execed_with_foreground() {
    assert_eq!(
        daemon::child_args(&["connect".into(), "vnc://mac01.local".into()]),
        vec!["connect", "vnc://mac01.local", "--foreground"]
    );
}

#[test]
fn foreground_is_not_added_twice() {
    assert_eq!(
        daemon::child_args(&["connect".into(), "--foreground".into()]),
        vec!["connect", "--foreground"]
    );
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
