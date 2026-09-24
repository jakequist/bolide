//! Where state lives, what it holds, and when it is stale.

mod common;

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use bolide_cli::state::{self, SessionState};
use bolide_cli::{state_dir_for, Platform};

use common::TempDir;

fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    move |k: &str| map.get(k).cloned()
}

#[test]
fn macos_state_lives_under_application_support() {
    let env = env_of(&[("HOME", "/Users/jake"), ("XDG_STATE_HOME", "/ignored")]);
    assert_eq!(
        state_dir_for(Platform::MacOs, &env),
        Some(PathBuf::from(
            "/Users/jake/Library/Application Support/bolide"
        ))
    );
}

#[test]
fn xdg_state_home_wins_when_it_is_set() {
    let env = env_of(&[("HOME", "/home/jake"), ("XDG_STATE_HOME", "/run/state")]);
    assert_eq!(
        state_dir_for(Platform::Xdg, &env),
        Some(PathBuf::from("/run/state/bolide"))
    );
}

#[test]
fn xdg_falls_back_to_dot_local_state() {
    let env = env_of(&[("HOME", "/home/jake")]);
    assert_eq!(
        state_dir_for(Platform::Xdg, &env),
        Some(PathBuf::from("/home/jake/.local/state/bolide"))
    );
    // An empty variable is unset, not a path of "".
    let env = env_of(&[("HOME", "/home/jake"), ("XDG_STATE_HOME", "")]);
    assert_eq!(
        state_dir_for(Platform::Xdg, &env),
        Some(PathBuf::from("/home/jake/.local/state/bolide"))
    );
}

#[test]
fn without_a_home_there_is_nowhere_to_put_it() {
    let env = env_of(&[]);
    assert_eq!(state_dir_for(Platform::MacOs, &env), None);
    assert_eq!(state_dir_for(Platform::Xdg, &env), None);
    // XDG_STATE_HOME alone is enough, though: it does not need $HOME.
    let env = env_of(&[("XDG_STATE_HOME", "/run/state")]);
    assert_eq!(
        state_dir_for(Platform::Xdg, &env),
        Some(PathBuf::from("/run/state/bolide"))
    );
}

fn a_state() -> SessionState {
    SessionState {
        endpoint: "http://127.0.0.1:53211".into(),
        pid: std::process::id(),
        remote: "mac01.local:5900".into(),
        username: Some("jake".into()),
        token: Some("t0ken".into()),
        started_at: "2026-09-15T22:41:03Z".into(),
    }
}

#[test]
fn the_state_file_round_trips() {
    let dir = TempDir::new("roundtrip");
    let state = a_state();
    state::save_in(dir.path(), &state).unwrap();
    assert_eq!(state::load_in(dir.path()), Some(state));
}

#[test]
fn the_serialized_state_has_no_password_field_under_any_construction() {
    for state in [
        a_state(),
        SessionState {
            username: None,
            token: None,
            ..a_state()
        },
    ] {
        let json = serde_json::to_string(&state).unwrap();
        assert!(
            !json.contains("password"),
            "a password field appeared: {json}"
        );
        assert!(json.contains("\"endpoint\""), "{json}");
        assert!(json.contains("\"pid\""), "{json}");
        assert!(json.contains("\"started_at\""), "{json}");
    }
    // And nothing that looks like a secret survives a save.
    let dir = TempDir::new("nopassword");
    state::save_in(dir.path(), &a_state()).unwrap();
    let on_disk = std::fs::read_to_string(state::session_path(dir.path())).unwrap();
    assert!(!on_disk.contains("password"), "{on_disk}");
}

#[test]
fn the_state_file_is_0600() {
    let dir = TempDir::new("perms");
    state::save_in(dir.path(), &a_state()).unwrap();
    let mode = std::fs::metadata(state::session_path(dir.path()))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "mode was {mode:o}");
}

#[test]
fn saving_creates_the_directory() {
    let dir = TempDir::new("mkdir");
    let nested = dir.path().join("a/b/c");
    state::save_in(&nested, &a_state()).unwrap();
    assert!(state::session_path(&nested).exists());
}

#[test]
fn a_dead_pid_makes_the_file_stale_and_it_is_removed() {
    let dir = TempDir::new("stale");
    state::save_in(dir.path(), &a_state()).unwrap();
    let dead = |_pid: u32| false;
    assert_eq!(state::load_in_with(dir.path(), &dead), None);
    assert!(
        !state::session_path(dir.path()).exists(),
        "a stale state file must be removed, not left to time out against a dead port"
    );
}

#[test]
fn a_corrupt_state_file_is_treated_as_absent() {
    let dir = TempDir::new("corrupt");
    std::fs::create_dir_all(dir.path()).unwrap();
    std::fs::write(state::session_path(dir.path()), b"{not json").unwrap();
    assert_eq!(state::load_in(dir.path()), None);
    assert!(!state::session_path(dir.path()).exists());
}

#[test]
fn no_file_at_all_is_simply_none() {
    let dir = TempDir::new("absent");
    assert_eq!(state::load_in(dir.path()), None);
}

#[test]
fn clearing_is_idempotent() {
    let dir = TempDir::new("clear");
    state::clear_in(dir.path()).unwrap();
    state::save_in(dir.path(), &a_state()).unwrap();
    state::clear_in(dir.path()).unwrap();
    state::clear_in(dir.path()).unwrap();
    assert!(!state::session_path(dir.path()).exists());
}

#[test]
fn our_own_pid_is_alive_and_pid_zero_is_not_us() {
    assert!(state::pid_is_alive(std::process::id()));
}

#[test]
fn started_at_is_rfc_3339_utc() {
    assert_eq!(state::rfc3339_from_unix(0), "1970-01-01T00:00:00Z");
    assert_eq!(
        state::rfc3339_from_unix(1_600_000_000),
        "2020-09-13T12:26:40Z"
    );
    // A leap day, because the civil-date arithmetic is the part that goes wrong.
    assert_eq!(
        state::rfc3339_from_unix(1_709_164_800),
        "2024-02-29T00:00:00Z"
    );
}
