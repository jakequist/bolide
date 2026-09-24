//! `vnc://[user[:password]@]host[:port]`, and the credentials a URL may carry.

use bolide_cli::cli::{parse_target, Target};

#[test]
fn a_bare_vnc_url_defaults_to_5900() {
    assert_eq!(
        parse_target("vnc://mac01.local").unwrap(),
        Target::new("mac01.local", 5900)
    );
}

#[test]
fn an_explicit_port_wins() {
    assert_eq!(
        parse_target("vnc://mac01.local:5901").unwrap(),
        Target::new("mac01.local", 5901)
    );
}

#[test]
fn a_bare_host_port_works_because_people_will_type_it() {
    assert_eq!(
        parse_target("127.0.0.1:5901").unwrap(),
        Target::new("127.0.0.1", 5901)
    );
    assert_eq!(
        parse_target("mac01.local").unwrap(),
        Target::new("mac01.local", 5900)
    );
}

#[test]
fn an_ipv6_literal_keeps_its_colons() {
    assert_eq!(
        parse_target("vnc://[::1]:5901").unwrap(),
        Target::new("::1", 5901)
    );
    assert_eq!(
        parse_target("vnc://[fe80::1]").unwrap(),
        Target::new("fe80::1", 5900)
    );
    // Unbracketed, no scheme: the colons are the address, not a port.
    assert_eq!(parse_target("::1").unwrap(), Target::new("::1", 5900));
}

#[test]
fn display_brackets_an_ipv6_host() {
    let t = Target::new("::1", 5901);
    assert_eq!(t.to_string(), "[::1]:5901");
    let t = Target::new("mac01.local", 5900);
    assert_eq!(t.to_string(), "mac01.local:5900");
}

#[test]
fn another_scheme_is_refused() {
    let err = parse_target("http://mac01.local").unwrap_err();
    assert!(err.contains("vnc"), "unhelpful: {err}");
}

#[test]
fn a_password_in_the_url_is_accepted_and_kept_out_of_the_address() {
    let t = parse_target("vnc://jake:hunter2@mac01.local:5901").unwrap();
    assert_eq!(t.host, "mac01.local");
    assert_eq!(t.port, 5901);
    assert_eq!(t.username.as_deref(), Some("jake"));
    assert_eq!(t.password.as_ref().map(|p| p.expose()), Some("hunter2"));
    // Display is what becomes `remote` in the state file and in /status.
    assert_eq!(t.to_string(), "mac01.local:5901");
    let debug = format!("{t:?}");
    assert!(!debug.contains("hunter2"), "Debug leaked it: {debug}");
}

#[test]
fn a_url_password_is_percent_decoded() {
    let t = parse_target("vnc://jake:p%40ss%3Aw%2Fd@mac01.local").unwrap();
    assert_eq!(t.password.as_ref().map(|p| p.expose()), Some("p@ss:w/d"));
}

#[test]
fn a_password_may_contain_an_at_sign_left_unencoded() {
    // The last `@` separates userinfo from host, so an unencoded one in the password
    // still parses as the person meant it.
    let t = parse_target("vnc://jake:a@b@mac01.local").unwrap();
    assert_eq!(t.host, "mac01.local");
    assert_eq!(t.password.as_ref().map(|p| p.expose()), Some("a@b"));
}

#[test]
fn an_empty_url_password_is_a_password_only_the_url_can_say_nothing_about() {
    let t = parse_target("vnc://:hunter2@mac01.local").unwrap();
    assert_eq!(t.username, None, "an empty username is no username");
    assert_eq!(t.password.as_ref().map(|p| p.expose()), Some("hunter2"));
}

#[test]
fn a_bad_percent_escape_is_refused_without_echoing_the_password() {
    let err = parse_target("vnc://jake:hunter2%zz@mac01.local").unwrap_err();
    assert!(!err.contains("hunter2"), "leaked the password: {err}");
    assert!(err.contains('%'), "did not say what is wrong: {err}");
}

#[test]
fn a_username_in_the_url_is_accepted() {
    let t = parse_target("vnc://jake@mac01.local").unwrap();
    assert_eq!(t.username.as_deref(), Some("jake"));
    assert_eq!(t.password, None);
    assert_eq!(t.to_string(), "mac01.local:5900");
}

#[test]
fn userinfo_with_no_host_is_refused() {
    let err = parse_target("vnc://jake:hunter2@").unwrap_err();
    assert!(!err.contains("hunter2"), "leaked the password: {err}");
}

#[test]
fn nonsense_is_refused_rather_than_guessed() {
    assert!(parse_target("").is_err());
    assert!(parse_target("vnc://").is_err());
    assert!(parse_target("vnc://mac01.local:not-a-port").is_err());
    assert!(parse_target("vnc://mac01.local:99999").is_err());
    assert!(parse_target("vnc://[::1").is_err());
}
