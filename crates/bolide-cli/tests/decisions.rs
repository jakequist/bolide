//! The decisions a command makes before it touches the network: is there a session, may
//! a PNG go to this stdout, where does the password come from, what gets printed.

mod common;

use std::path::{Path, PathBuf};

use bolide_cli::error::{EXIT_FAILURE, EXIT_NOT_CONNECTED, EXIT_USAGE};
use bolide_cli::run::{self, Sink};
use bolide_cli::state::SessionState;
use bolide_server::wire::StatusResponse;

use common::TempDir;

fn a_state() -> SessionState {
    SessionState {
        endpoint: "http://127.0.0.1:53211".into(),
        pid: std::process::id(),
        remote: "mac01.local:5900".into(),
        username: Some("jake".into()),
        token: None,
        started_at: "2026-09-15T22:41:03Z".into(),
    }
}

#[test]
fn no_state_file_is_exit_3_and_says_how_to_fix_it() {
    let err = run::require_session(None).unwrap_err();
    assert_eq!(err.code, EXIT_NOT_CONNECTED);
    assert!(
        err.message.contains("bolide connect"),
        "did not say what to do: {}",
        err.message
    );
}

#[test]
fn a_live_state_file_is_the_session() {
    assert_eq!(run::require_session(Some(a_state())).unwrap(), a_state());
}

#[test]
fn a_png_does_not_go_down_a_tty() {
    let err = run::screenshot_sink(None, true).unwrap_err();
    assert_eq!(err.code, EXIT_USAGE);
    assert!(err.message.contains("--out"), "no way out: {}", err.message);
    assert!(
        err.message.contains("--out -"),
        "no way to say you meant it: {}",
        err.message
    );
}

#[test]
fn out_dash_sends_the_png_to_stdout_even_on_a_tty() {
    // The default protects a terminal from a megabyte of binary; asking for it by name
    // gets it.
    assert_eq!(
        run::screenshot_sink(Some(Path::new("-")), true).unwrap(),
        Sink::Stdout
    );
    assert_eq!(
        run::screenshot_sink(Some(Path::new("-")), false).unwrap(),
        Sink::Stdout
    );
}

#[test]
fn a_png_goes_to_a_pipe_or_to_the_file_you_named() {
    assert_eq!(run::screenshot_sink(None, false).unwrap(), Sink::Stdout);
    assert_eq!(
        run::screenshot_sink(Some(Path::new("s.png")), true).unwrap(),
        Sink::File(PathBuf::from("s.png"))
    );
    assert_eq!(
        run::screenshot_sink(Some(Path::new("s.png")), false).unwrap(),
        Sink::File(PathBuf::from("s.png"))
    );
}

#[test]
fn a_password_comes_from_the_flag_the_file_or_the_environment() {
    let dir = TempDir::new("password");
    let file = dir.path().join("pw");
    std::fs::write(&file, "hunter2\n").unwrap();

    assert_eq!(
        run::resolve_password(None, Some(&file), None)
            .unwrap()
            .as_deref(),
        Some("hunter2"),
        "a trailing newline is the editor's, not the password's"
    );
    assert_eq!(
        run::resolve_password(None, None, Some("from-env".into()))
            .unwrap()
            .as_deref(),
        Some("from-env")
    );
    assert_eq!(
        run::resolve_password(None, Some(&file), Some("from-env".into()))
            .unwrap()
            .as_deref(),
        Some("hunter2"),
        "an explicit --password-file wins over an inherited variable"
    );
    assert_eq!(run::resolve_password(None, None, None).unwrap(), None);
    assert_eq!(
        run::resolve_password(None, None, Some(String::new())).unwrap(),
        None,
        "an empty BOLIDE_PASSWORD is unset, not an empty password"
    );
}

#[test]
fn an_explicit_password_flag_wins_over_an_inherited_variable() {
    assert_eq!(
        run::resolve_password(Some("from-flag"), None, Some("from-env".into()))
            .unwrap()
            .as_deref(),
        Some("from-flag")
    );
}

#[test]
fn a_missing_password_file_says_so_without_guessing() {
    let err = run::resolve_password(None, Some(Path::new("/nope/nothing-here")), None).unwrap_err();
    assert_eq!(err.code, EXIT_FAILURE);
    assert!(
        err.message.contains("/nope/nothing-here"),
        "did not name the file: {}",
        err.message
    );
}

#[test]
fn clipboard_push_takes_text_or_stdin_but_not_both_and_not_neither() {
    let from_stdin = || Ok("piped".to_string());
    assert_eq!(
        run::clipboard_text(Some("typed".into()), false, &from_stdin).unwrap(),
        "typed"
    );
    assert_eq!(
        run::clipboard_text(None, true, &from_stdin).unwrap(),
        "piped"
    );
    assert_eq!(
        run::clipboard_text(Some("typed".into()), true, &from_stdin)
            .unwrap_err()
            .code,
        EXIT_USAGE
    );
    assert_eq!(
        run::clipboard_text(None, false, &from_stdin)
            .unwrap_err()
            .code,
        EXIT_USAGE
    );
}

#[test]
fn status_prints_one_fact_per_line() {
    let status = StatusResponse {
        connected: true,
        remote: "mac01.local:5900".into(),
        desktop_name: "mac01".into(),
        width: 1728,
        height: 1117,
        painted: true,
        version: "0.0.0".into(),
    };
    let lines = run::status_lines(&status, &a_state());
    let joined = lines.join("\n");
    assert!(lines.len() >= 4, "too terse: {joined}");
    assert!(
        lines.iter().all(|l| !l.contains('\n')),
        "one fact per line: {joined}"
    );
    assert!(joined.contains("mac01.local:5900"), "{joined}");
    assert!(joined.contains("1728x1117"), "{joined}");
    assert!(joined.contains("http://127.0.0.1:53211"), "{joined}");
    assert!(joined.contains(&a_state().pid.to_string()), "{joined}");
}

#[test]
fn a_disconnected_status_says_so_rather_than_reporting_a_screen() {
    let status = StatusResponse {
        connected: false,
        remote: "mac01.local:5900".into(),
        desktop_name: String::new(),
        width: 0,
        height: 0,
        painted: false,
        version: "0.0.0".into(),
    };
    let joined = run::status_lines(&status, &a_state()).join("\n");
    assert!(joined.contains("not connected"), "{joined}");
}

#[test]
fn an_unpainted_screen_is_reported_as_unpainted() {
    let status = StatusResponse {
        connected: true,
        remote: "mac01.local:5900".into(),
        desktop_name: "mac01".into(),
        width: 800,
        height: 600,
        painted: false,
        version: "0.0.0".into(),
    };
    let joined = run::status_lines(&status, &a_state()).join("\n");
    assert!(
        joined.contains("not painted") || joined.contains("unpainted"),
        "a black framebuffer is indistinguishable from a black screen; say so: {joined}"
    );
}
