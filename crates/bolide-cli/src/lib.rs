//! `bolide` — the CLI, as a library so its decisions can be tested without a process.
//!
//! # The commands
//!
//! ```text
//! bolide connect vnc://HOST[:PORT] [--username U] [--password-file F] [--listen ADDR]
//!                                 [--token T] [--foreground]
//! bolide status
//! bolide disconnect
//! bolide screenshot [--out FILE.png]
//! bolide click X Y [--button left|right|middle]
//! bolide move X Y
//! bolide type TEXT
//! bolide key CHORD
//! bolide scroll X Y --dir up|down [--amount N]
//! bolide clipboard push [TEXT | --stdin]
//! bolide clipboard pull
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
//! **`--password` on the command line is refused.** Not warned about — refused, with
//! exit code 2 and a message naming `--password-file` and `BOLIDE_PASSWORD`. The reason
//! is that argv is world-readable on every machine bolide targets: `ps aux` shows it,
//! the shell writes it to history, and a CI log that echoes a command line publishes
//! it. A warning would leave the password exposed and the user reassured. The flag
//! exists in the parser *only* so the error can be specific; its value is never read.
//! A password inside the target URL (`vnc://user:pw@host`) is refused for the same
//! reason, by [`cli::parse_target`].
//!
//! The password never reaches: the state file, `/status`, any log line, or `Debug` of
//! `bolide_rfb::Config` (which redacts it). It is read from the file or the environment,
//! handed to the RFB handshake, and dropped.
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
//! PNG bytes to stdout **only when stdout is not a terminal**; to a terminal it is an
//! error naming `--out`, because a megabyte of PNG through a tty is not a thing anyone
//! meant. Exit codes: 0 success, 1 failure, 2 usage/refused-password, 3 not connected.

#![deny(missing_docs)]

use std::path::PathBuf;

pub mod api;
pub mod cli;
pub mod daemon;
pub mod error;
pub mod run;
pub mod state;

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
