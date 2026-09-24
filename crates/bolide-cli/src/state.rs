//! The state file: where the daemon is, and whether it is still there.
//!
//! Every function that touches the filesystem has an `_in(dir)` twin, and the twin is
//! what the tests call. The `$HOME`-reading versions are one line of wiring on top.

use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// What `connect` writes so the other subcommands can find the daemon.
///
/// **There is no password field and there must never be one.** If a future version
/// needs to reconnect unattended, it re-reads the password file; it does not cache the
/// secret.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionState {
    /// Base URL of the loopback server, e.g. `http://127.0.0.1:53211`.
    pub endpoint: String,
    /// The daemon's pid, so a stale file can be recognised.
    pub pid: u32,
    /// The desktop, `host:port`. No credentials.
    pub remote: String,
    /// The username `connect` was given, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Bearer token for the server, if one was generated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// RFC 3339, UTC.
    pub started_at: String,
}

/// The state directory for this build. `None` when `$HOME` is unset.
pub fn state_dir() -> Option<PathBuf> {
    crate::state_dir_for(crate::THIS_PLATFORM, &|k| std::env::var(k).ok())
}

/// The state directory, or an error a user can act on.
pub fn require_state_dir() -> io::Result<PathBuf> {
    state_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "bolide has nowhere to keep its state: neither $HOME nor $XDG_STATE_HOME is set",
        )
    })
}

/// `<dir>/session.json`.
pub fn session_path(dir: &Path) -> PathBuf {
    dir.join("session.json")
}

/// `<dir>/bolide.log` — where a daemonised `connect` sends its stdio.
pub fn log_path(dir: &Path) -> PathBuf {
    dir.join("bolide.log")
}

/// Read the state file, if there is a live one.
pub fn load() -> Option<SessionState> {
    load_in(&state_dir()?)
}

/// Read the state file in `dir`, if there is a live one.
pub fn load_in(dir: &Path) -> Option<SessionState> {
    load_in_with(dir, &pid_is_alive)
}

/// Read the state file in `dir`, deciding liveness with `alive`.
///
/// A file whose pid is gone is **stale**: it is removed and treated as absent, so a
/// crashed daemon does not make every later command time out against a dead port. So is
/// a file that does not parse — a half-written or hand-edited one is not evidence of a
/// running server either.
pub fn load_in_with(dir: &Path, alive: &dyn Fn(u32) -> bool) -> Option<SessionState> {
    let path = session_path(dir);
    let raw = std::fs::read(&path).ok()?;
    let state: SessionState = match serde_json::from_slice(&raw) {
        Ok(state) => state,
        Err(_) => {
            let _ = std::fs::remove_file(&path);
            return None;
        }
    };
    if !alive(state.pid) {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    Some(state)
}

/// Write the state file with mode 0600, creating the directory.
pub fn save(state: &SessionState) -> io::Result<()> {
    save_in(&require_state_dir()?, state)
}

/// Write `dir/session.json` with mode 0600, creating `dir`.
///
/// Written to a temporary file and renamed, so a reader never sees half a document —
/// the parent process is polling for exactly this file while it is being written.
pub fn save_in(dir: &Path, state: &SessionState) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));

    let tmp = dir.join(format!("session.json.tmp.{}", std::process::id()));
    let body = serde_json::to_vec_pretty(state).map_err(io::Error::other)?;
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(&body)?;
        f.write_all(b"\n")?;
        f.sync_all()?;
    }
    // `mode` on the open only applies when the file is created; a leftover temp file
    // from a crashed run would keep its old mode, so say it again.
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&tmp, session_path(dir))
}

/// Remove the state file.
pub fn clear() -> io::Result<()> {
    match state_dir() {
        Some(dir) => clear_in(&dir),
        None => Ok(()),
    }
}

/// Remove `dir/session.json`. Idempotent.
pub fn clear_in(dir: &Path) -> io::Result<()> {
    match std::fs::remove_file(session_path(dir)) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Whether a process with this pid exists.
///
/// `kill(pid, 0)` asks the kernel without sending anything. `EPERM` means the process
/// exists and belongs to somebody else — still alive, so still not stale.
pub fn pid_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: signal 0 sends nothing; this is a permission-and-existence probe.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Now, as RFC 3339 UTC.
pub fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    rfc3339_from_unix(secs)
}

/// A Unix timestamp as RFC 3339 UTC, e.g. `1970-01-01T00:00:00Z`.
///
/// Hand-rolled rather than pulling in a date crate for one timestamp; it is Howard
/// Hinnant's `civil_from_days`, and it is a pure function with a test.
pub fn rfc3339_from_unix(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let time_of_day = secs.rem_euclid(86_400);
    let (h, m, s) = (
        time_of_day / 3600,
        (time_of_day % 3600) / 60,
        time_of_day % 60,
    );

    // civil_from_days: days since 1970-01-01 → (y, m, d).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };

    format!("{year:04}-{month:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}
