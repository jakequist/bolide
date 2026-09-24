//! Argument parsing, and the one argument bolide refuses to accept.

use std::path::PathBuf;

use bolide_cli::cli::{parse_args, Button, Command, Dir};
use bolide_cli::error::{Stream, EXIT_USAGE};

fn args(line: &[&str]) -> Vec<String> {
    std::iter::once("bolide")
        .chain(line.iter().copied())
        .map(String::from)
        .collect()
}

#[test]
fn password_on_the_command_line_is_refused_with_exit_2() {
    for line in [
        vec!["connect", "vnc://mac01.local", "--password", "hunter2"],
        vec!["connect", "--password=hunter2", "vnc://mac01.local"],
        vec!["connect", "vnc://mac01.local", "--password"],
    ] {
        let err = parse_args(&args(&line)).expect_err("--password must be refused");
        assert_eq!(err.code, EXIT_USAGE, "for {line:?}");
        assert_eq!(err.stream, Stream::Err);
        assert!(
            !err.message.contains("hunter2"),
            "the refusal leaked it: {}",
            err.message
        );
        assert!(
            err.message.contains("--password-file"),
            "no way out: {}",
            err.message
        );
        assert!(
            err.message.contains("BOLIDE_PASSWORD"),
            "no way out: {}",
            err.message
        );
    }
}

#[test]
fn the_refusal_reaches_every_subcommand() {
    // `--password` is global, so a stray one anywhere is caught rather than ignored.
    let err = parse_args(&args(&["status", "--password", "hunter2"])).unwrap_err();
    assert_eq!(err.code, EXIT_USAGE);
}

#[test]
fn password_file_and_the_environment_still_work() {
    let cli = parse_args(&args(&[
        "connect",
        "vnc://mac01.local",
        "--username",
        "jake",
        "--password-file",
        "/tmp/p",
    ]))
    .expect("a password file is the supported way");
    match cli.command {
        Command::Connect(c) => {
            assert_eq!(c.target, "vnc://mac01.local");
            assert_eq!(c.username.as_deref(), Some("jake"));
            assert_eq!(c.password_file, Some(PathBuf::from("/tmp/p")));
            assert!(!c.foreground);
            assert_eq!(c.listen.to_string(), "127.0.0.1:0");
            assert_eq!(c.token, None);
        }
        other => panic!("wrong command: {other:?}"),
    }
}

#[test]
fn connect_takes_a_listen_address_a_token_and_foreground() {
    let cli = parse_args(&args(&[
        "connect",
        "vnc://h",
        "--listen",
        "127.0.0.1:53211",
        "--token",
        "t",
        "--foreground",
    ]))
    .unwrap();
    match cli.command {
        Command::Connect(c) => {
            assert_eq!(c.listen.to_string(), "127.0.0.1:53211");
            assert_eq!(c.token.as_deref(), Some("t"));
            assert!(c.foreground);
        }
        other => panic!("wrong command: {other:?}"),
    }
}

#[test]
fn every_command_in_the_table_parses() {
    assert!(matches!(
        parse_args(&args(&["status"])).unwrap().command,
        Command::Status
    ));
    assert!(matches!(
        parse_args(&args(&["disconnect"])).unwrap().command,
        Command::Disconnect
    ));
    assert!(matches!(
        parse_args(&args(&["screenshot"])).unwrap().command,
        Command::Screenshot { out: None }
    ));
    assert!(matches!(
        parse_args(&args(&["screenshot", "--out", "s.png"]))
            .unwrap()
            .command,
        Command::Screenshot { out: Some(_) }
    ));
    assert!(matches!(
        parse_args(&args(&["click", "420", "180"])).unwrap().command,
        Command::Click {
            x: 420,
            y: 180,
            button: Button::Left
        }
    ));
    assert!(matches!(
        parse_args(&args(&["click", "1", "2", "--button", "middle"]))
            .unwrap()
            .command,
        Command::Click {
            button: Button::Middle,
            ..
        }
    ));
    assert!(matches!(
        parse_args(&args(&["move", "3", "4"])).unwrap().command,
        Command::Move { x: 3, y: 4 }
    ));
    assert!(matches!(
        parse_args(&args(&["type", "hello"])).unwrap().command,
        Command::Type { .. }
    ));
    assert!(matches!(
        parse_args(&args(&["key", "cmd+space"])).unwrap().command,
        Command::Key { .. }
    ));
    assert!(matches!(
        parse_args(&args(&["scroll", "1", "2", "--dir", "down"]))
            .unwrap()
            .command,
        Command::Scroll {
            dir: Dir::Down,
            amount: 1,
            ..
        }
    ));
    assert!(parse_args(&args(&["clipboard", "pull"])).is_ok());
    assert!(parse_args(&args(&["clipboard", "push", "hi"])).is_ok());
    assert!(parse_args(&args(&["clipboard", "push", "--stdin"])).is_ok());
}

#[test]
fn a_bad_button_is_a_usage_error_not_a_left_click() {
    let err = parse_args(&args(&["click", "1", "2", "--button", "sideways"])).unwrap_err();
    assert_eq!(err.code, EXIT_USAGE);
}

#[test]
fn scroll_requires_a_direction() {
    let err = parse_args(&args(&["scroll", "1", "2"])).unwrap_err();
    assert_eq!(err.code, EXIT_USAGE);
}

#[test]
fn help_is_an_exit_0_on_stdout() {
    let err = parse_args(&args(&["--help"])).unwrap_err();
    assert_eq!(err.code, 0);
    assert_eq!(err.stream, Stream::Out);
    assert!(
        err.message.contains("connect"),
        "no commands: {}",
        err.message
    );
}
