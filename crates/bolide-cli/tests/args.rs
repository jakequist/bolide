//! Argument parsing, and the one argument bolide refuses to accept.

use std::path::PathBuf;

use bolide_cli::cli::{
    command_line_credentials, parse_args, parse_target, redact_password, Button, Command,
    ConnectArgs, Dir, Target,
};
use bolide_cli::error::{Stream, EXIT_USAGE};

fn args(line: &[&str]) -> Vec<String> {
    std::iter::once("bolide")
        .chain(line.iter().copied())
        .map(String::from)
        .collect()
}

#[test]
fn a_password_on_the_command_line_is_accepted_by_connect() {
    for line in [
        vec!["connect", "vnc://mac01.local", "--password", "hunter2"],
        vec!["connect", "--password=hunter2", "vnc://mac01.local"],
    ] {
        let cli = parse_args(&args(&line)).expect("--password is a supported way in");
        match cli.command {
            Command::Connect(c) => {
                assert_eq!(c.target, "vnc://mac01.local", "for {line:?}");
                assert_eq!(
                    c.password.as_ref().map(|p| p.expose()),
                    Some("hunter2"),
                    "for {line:?}"
                );
            }
            other => panic!("wrong command: {other:?}"),
        }
    }
}

#[test]
fn a_password_may_start_with_a_dash() {
    let cli = parse_args(&args(&["connect", "vnc://h", "--password", "-hunter2"]))
        .expect("a leading dash is a legal password character");
    match cli.command {
        Command::Connect(c) => {
            assert_eq!(c.password.as_ref().map(|p| p.expose()), Some("-hunter2"))
        }
        other => panic!("wrong command: {other:?}"),
    }
}

#[test]
fn a_bare_password_flag_is_a_usage_error_that_names_what_is_missing() {
    let err = parse_args(&args(&["connect", "vnc://mac01.local", "--password"]))
        .expect_err("--password with no value has nothing to use");
    assert_eq!(err.code, EXIT_USAGE);
    assert_eq!(err.stream, Stream::Err);
    assert!(err.message.contains("--password"), "{}", err.message);
    assert!(err.message.contains("value"), "{}", err.message);
}

#[test]
fn password_and_password_file_are_mutually_exclusive() {
    let err = parse_args(&args(&[
        "connect",
        "vnc://h",
        "--password",
        "hunter2",
        "--password-file",
        "/tmp/p",
    ]))
    .expect_err("two sources for one password is a question bolide will not guess at");
    assert_eq!(err.code, EXIT_USAGE);
    assert!(
        !err.message.contains("hunter2"),
        "the error leaked it: {}",
        err.message
    );
    assert!(err.message.contains("--password-file"), "{}", err.message);
}

#[test]
fn the_password_flag_belongs_to_connect_alone() {
    // Nothing else talks RFB, so a stray `--password` elsewhere is a mistake, and the
    // error for it must not echo the value.
    let err = parse_args(&args(&["status", "--password", "hunter2"])).unwrap_err();
    assert_eq!(err.code, EXIT_USAGE);
    assert!(
        !err.message.contains("hunter2"),
        "the error leaked it: {}",
        err.message
    );
}

#[test]
fn debug_of_a_parsed_command_line_never_shows_the_password() {
    let cli = parse_args(&args(&["connect", "vnc://h", "--password", "hunter2"])).unwrap();
    let debug = format!("{cli:?}");
    assert!(!debug.contains("hunter2"), "Debug leaked it: {debug}");
}

#[test]
fn an_error_message_that_quotes_argv_has_the_password_redacted() {
    let argv = args(&[
        "connect",
        "--password",
        "hunter2",
        "--password=s3cret",
        "vnc://jake:p%40ss@h",
        "x",
    ]);
    let out = redact_password("saw 'hunter2', 's3cret', 'p%40ss' and 'p@ss' near x", &argv);
    for secret in ["hunter2", "s3cret", "p%40ss", "p@ss"] {
        assert!(!out.contains(secret), "{secret} survived: {out}");
    }
    assert!(out.contains("near x"), "redacted too much: {out}");
}

#[test]
fn redaction_cuts_out_whole_values_not_letters_inside_words() {
    // A one-letter password must not turn "unexpected" into "une<redacted>pected".
    let argv = args(&["status", "--password", "x"]);
    assert_eq!(
        redact_password("error: unexpected argument '--password' found", &argv),
        "error: unexpected argument '--password' found"
    );
    assert_eq!(
        redact_password("error: unexpected value 'x' found", &argv),
        "error: unexpected value '<redacted>' found"
    );
}

fn connect_line(line: &[&str]) -> (Target, ConnectArgs) {
    match parse_args(&args(line)).unwrap().command {
        Command::Connect(c) => (parse_target(&c.target).unwrap(), c),
        other => panic!("wrong command: {other:?}"),
    }
}

#[test]
fn the_url_and_the_flags_combine_into_one_set_of_credentials() {
    let (t, c) = connect_line(&["connect", "vnc://jake:hunter2@h"]);
    let creds = command_line_credentials(&t, &c).unwrap();
    assert_eq!(creds.username.as_deref(), Some("jake"));
    assert_eq!(creds.password.as_ref().map(|p| p.expose()), Some("hunter2"));

    let (t, c) = connect_line(&[
        "connect",
        "vnc://h",
        "--username",
        "jake",
        "--password",
        "pw",
    ]);
    let creds = command_line_credentials(&t, &c).unwrap();
    assert_eq!(creds.username.as_deref(), Some("jake"));
    assert_eq!(creds.password.as_ref().map(|p| p.expose()), Some("pw"));

    let (t, c) = connect_line(&["connect", "vnc://jake@h", "--username", "jake"]);
    assert!(
        command_line_credentials(&t, &c).is_ok(),
        "saying it twice, the same, is fine"
    );
}

#[test]
fn a_url_password_and_a_password_flag_together_are_a_usage_error() {
    for line in [
        vec!["connect", "vnc://jake:hunter2@h", "--password", "other1"],
        vec![
            "connect",
            "vnc://jake:hunter2@h",
            "--password-file",
            "/tmp/p",
        ],
    ] {
        let (t, c) = connect_line(&line);
        let err = command_line_credentials(&t, &c).expect_err("two passwords");
        assert_eq!(err.code, EXIT_USAGE, "for {line:?}");
        assert!(!err.message.contains("hunter2"), "leaked: {}", err.message);
        assert!(!err.message.contains("other1"), "leaked: {}", err.message);
    }
}

#[test]
fn two_different_usernames_are_a_usage_error() {
    let (t, c) = connect_line(&["connect", "vnc://jake@h", "--username", "bob"]);
    let err = command_line_credentials(&t, &c).expect_err("which one?");
    assert_eq!(err.code, EXIT_USAGE);
}

#[test]
fn connect_help_advises_the_private_ways_in() {
    let err = parse_args(&args(&["connect", "--help"])).unwrap_err();
    assert_eq!(err.code, 0);
    assert!(err.message.contains("--password-file"), "{}", err.message);
    assert!(err.message.contains("BOLIDE_PASSWORD"), "{}", err.message);
}

#[test]
fn version_prints_the_cargo_version_on_stdout() {
    let err = parse_args(&args(&["--version"])).unwrap_err();
    assert_eq!(err.code, 0);
    assert_eq!(err.stream, Stream::Out);
    assert_eq!(err.message, format!("bolide {}", env!("CARGO_PKG_VERSION")));
}

#[test]
fn update_parses_with_and_without_check() {
    assert!(matches!(
        parse_args(&args(&["update"])).unwrap().command,
        Command::Update { check: false }
    ));
    assert!(matches!(
        parse_args(&args(&["update", "--check"])).unwrap().command,
        Command::Update { check: true }
    ));
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
fn connect_listens_wherever_it_is_told() {
    // Loopback is the default, not a rule: a non-loopback address is taken as given.
    let cli = parse_args(&args(&["connect", "vnc://h", "--listen", "0.0.0.0:8000"])).unwrap();
    match cli.command {
        Command::Connect(c) => assert_eq!(c.listen.to_string(), "0.0.0.0:8000"),
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
