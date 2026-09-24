//! `bolide` — the CLI, as a library so its decisions can be tested without a process.
//!
//! # The commands
//!
//! ```text
//! bolide connect vnc://[USER[:PASSWORD]@]HOST[:PORT] [--username U]
//!                [--password PW | --password-file F] [--listen ADDR] [--token T]
//!                [--foreground]
//! bolide status
//! bolide disconnect
//! bolide screenshot [--out FILE.png | --out -]
//! bolide click X Y [--button left|right|middle]
//! bolide move X Y
//! bolide type TEXT
//! bolide key CHORD
//! bolide scroll X Y --dir up|down [--amount N]
//! bolide clipboard push [TEXT | --stdin]
//! bolide clipboard pull
//! bolide update [--check]
//! ```
//!
//! `connect` is the only command that touches RFB. Everything else is an HTTP call to
//! the running server, found through the state file — which is what makes `bolide
//! screenshot` and an agent's `POST /computer` the same code path rather than two
//! implementations of the same idea. The one exception is `disconnect`, which has no
//! route to call: the server exposes no shutdown endpoint (by design — an agent holding
//! the endpoint must not be able to end the session), so the CLI signals the pid it
//! wrote down and clears the state file.
//!
//! # Passwords
//!
//! A password comes from, in order: the target URL (`vnc://user:pw@host`,
//! percent-decoded) or `--password PW` — giving both, or either with
//! `--password-file`, is a usage error — then `--password-file F`, then the
//! `BOLIDE_PASSWORD` environment variable. All four are supported. The help notes that
//! argv is visible to other local users and to shell history and suggests the file or the
//! variable on a shared machine; that is advice, and the choice is the user's.
//!
//! What bolide guarantees is that the password goes nowhere else. It never reaches the
//! state file, `/status` (whose `remote` is `host:port`, never the URL), any log line, an
//! error message ([`cli::redact_password`] cuts it out of a parse error that quotes
//! argv), or `Debug` ([`cli::Password`] and `bolide_rfb::Config` both redact). And the
//! daemon `connect` re-execs gets it **through its environment, not its argv**
//! ([`daemon::child_args`] rebuilds the command line from what was parsed and has no
//! password field to write), so a process that lives for hours does not show it in `ps`
//! for all of them.
//!
//! # The state file
//!
//! `<state dir>/session.json`, mode 0600:
//!
//! ```json
//! {
//!   "endpoint": "http://127.0.0.1:53211",
//!   "pid": 40122,
//!   "remote": "mac01.local:5900",
//!   "username": "jake",
//!   "token": "…",
//!   "started_at": "2026-09-15T22:41:03Z"
//! }
//! ```
//!
//! No password, ever. The `token` is the bearer `--token` set, and it is in the file
//! because the CLI has to present it — the file's 0600 mode is the boundary, and it is
//! the same boundary the endpoint itself has.
//!
//! State dir, by platform ([`state_dir_for`], a pure function over an environment lookup
//! so it is testable on either platform from either platform):
//!
//! - macOS: `$HOME/Library/Application Support/bolide`
//! - otherwise: `$XDG_STATE_HOME/bolide`, or `$HOME/.local/state/bolide`
//!
//! # Daemonising
//!
//! Without `--foreground`, `connect` re-execs itself with `--foreground`, detached
//! (`setsid`) with stdio redirected to `<state dir>/bolide.log`, then **waits for the
//! child to write the state file** before printing the endpoint and exiting 0. Waiting
//! is the point: a `connect` that returned before the server was listening would make
//! every scripted `bolide connect && bolide screenshot` a race, and that is the first
//! thing anybody will type.
//!
//! If the child dies first, the parent reports the child's error — from the log — and
//! exits non-zero. A stale state file (pid gone) is replaced, not honoured.
//!
//! # Output
//!
//! Human-readable by default, one fact per line. `screenshot` without `--out` writes
//! PNG bytes to stdout when stdout is not a terminal; to a terminal it stops and names
//! `--out`, because a megabyte of PNG through a tty is rarely what was meant — and
//! `--out -` writes it there anyway. Exit codes: 0 success, 1 failure, 2 usage, 3 not
//! connected.

#![deny(missing_docs)]

use std::path::PathBuf;

pub mod api;
pub mod cli;
pub mod daemon;
pub mod error;
pub mod run;
pub mod state;
pub mod update;

pub use error::CliError;
pub use state::{state_dir, SessionState};

/// Where bolide keeps its state, given an environment lookup and a platform.
///
/// Takes the lookup as an argument rather than reading `std::env` so a Linux CI box can
/// test the macOS branch and vice versa — the alternative is a `#[cfg]`'d test that
/// only ever runs on the platform that was already working.
///
/// An environment variable set to the empty string counts as unset: `XDG_STATE_HOME=`
/// in a stripped-down environment is how a state directory of `/bolide` gets created at
/// the root of somebody's filesystem.
pub fn state_dir_for(platform: Platform, env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let get = |key: &str| env(key).filter(|v| !v.is_empty());
    match platform {
        Platform::MacOs => Some(
            PathBuf::from(get("HOME")?)
                .join("Library")
                .join("Application Support")
                .join("bolide"),
        ),
        Platform::Xdg => match get("XDG_STATE_HOME") {
            Some(xdg) => Some(PathBuf::from(xdg).join("bolide")),
            None => Some(
                PathBuf::from(get("HOME")?)
                    .join(".local")
                    .join("state")
                    .join("bolide"),
            ),
        },
    }
}

/// Which platform's convention to use. See [`state_dir_for`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    /// `$HOME/Library/Application Support/bolide`.
    MacOs,
    /// `$XDG_STATE_HOME/bolide`, else `$HOME/.local/state/bolide`.
    Xdg,
}

/// This build's platform.
pub const THIS_PLATFORM: Platform = if cfg!(target_os = "macos") {
    Platform::MacOs
} else {
    Platform::Xdg
};
