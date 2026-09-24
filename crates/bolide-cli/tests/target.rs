//! `vnc://host[:port]`, and the credentials bolide refuses to read out of a URL.

use bolide_cli::cli::{parse_target, Target};

#[test]
fn a_bare_vnc_url_defaults_to_5900() {
    assert_eq!(
        parse_target("vnc://mac01.local").unwrap(),
        Target {
            host: "mac01.local".into(),
            port: 5900
        }
    );
}

#[test]
fn an_explicit_port_wins() {
    assert_eq!(
        parse_target("vnc://mac01.local:5901").unwrap(),
        Target {
            host: "mac01.local".into(),
            port: 5901
        }
    );
}

#[test]
fn a_bare_host_port_works_because_people_will_type_it() {
    assert_eq!(
        parse_target("127.0.0.1:5901").unwrap(),
        Target {
            host: "127.0.0.1".into(),
            port: 5901
        }
    );
    assert_eq!(
        parse_target("mac01.local").unwrap(),
        Target {
            host: "mac01.local".into(),
            port: 5900
        }
    );
}

#[test]
fn an_ipv6_literal_keeps_its_colons() {
    assert_eq!(
        parse_target("vnc://[::1]:5901").unwrap(),
        Target {
            host: "::1".into(),
            port: 5901
        }
    );
    assert_eq!(
        parse_target("vnc://[fe80::1]").unwrap(),
        Target {
            host: "fe80::1".into(),
            port: 5900
        }
    );
    // Unbracketed, no scheme: the colons are the address, not a port.
    assert_eq!(
        parse_target("::1").unwrap(),
        Target {
            host: "::1".into(),
            port: 5900
        }
    );
}

#[test]
fn display_brackets_an_ipv6_host() {
    let t = Target {
        host: "::1".into(),
        port: 5901,
    };
    assert_eq!(t.to_string(), "[::1]:5901");
    let t = Target {
        host: "mac01.local".into(),
        port: 5900,
    };
    assert_eq!(t.to_string(), "mac01.local:5900");
}

#[test]
fn another_scheme_is_refused() {
    let err = parse_target("http://mac01.local").unwrap_err();
    assert!(err.contains("vnc"), "unhelpful: {err}");
}

#[test]
fn a_password_in_the_url_is_refused_by_name() {
    let err = parse_target("vnc://jake:hunter2@mac01.local").unwrap_err();
    assert!(!err.contains("hunter2"), "leaked the password: {err}");
    assert!(err.contains("password"), "did not say why: {err}");
    assert!(err.contains("--password-file"), "no way out: {err}");
    assert!(err.contains("BOLIDE_PASSWORD"), "no way out: {err}");
}

#[test]
fn a_username_in_the_url_points_at_the_flag() {
    let err = parse_target("vnc://jake@mac01.local").unwrap_err();
    assert!(err.contains("--username"), "unhelpful: {err}");
}

#[test]
fn nonsense_is_refused_rather_than_guessed() {
    assert!(parse_target("").is_err());
    assert!(parse_target("vnc://").is_err());
    assert!(parse_target("vnc://mac01.local:not-a-port").is_err());
    assert!(parse_target("vnc://mac01.local:99999").is_err());
    assert!(parse_target("vnc://[::1").is_err());
}
