//! Daemonising `connect`, and waiting for the result.
//!
//! The protocol is three steps, and only the middle one is hard to test:
//!
//! 1. [`child_args`] — the same command line with `--foreground` appended. Pure.
//! 2. [`spawn_detached`] — fork/exec with `setsid` and stdio pointed at the log.
//!    **Not unit-tested**: it is a `pre_exec` closure in a forked child, and there is no
//!    seam that would let a test observe it without actually forking. It is kept as
//!    small and as boring as possible for exactly that reason, and the end-to-end
//!    acceptance test is what covers it.
//! 3. [`await_startup`] — poll for the child's state file, or the child's death.
//!    Pure over two injected closures, so every branch is a test.
//!
//! Waiting is the point of the whole dance: a `connect` that returned before the server
//! was listening would make every `bolide connect && bolide screenshot` a race.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crate::error::CliError;
use crate::state::{self, SessionState};

/// How long the parent waits for the child, as a number of [`NAP`]s.
pub const STARTUP_ATTEMPTS: usize = 600;

/// How long a single poll waits before looking again.
pub const NAP: std::time::Duration = std::time::Duration::from_millis(25);

/// How many lines of the daemon's log a failure quotes back.
pub const LOG_TAIL_LINES: usize = 20;

/// Whether the child is still running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChildStatus {
    /// Still going.
    Running,
    /// Gone, with this exit code (`-1` when a signal killed it).
    Exited(i32),
}

/// The command line the detached child is given: ours, plus `--foreground`.
///
/// `args` is everything after the program name.
pub fn child_args(args: &[String]) -> Vec<String> {
    let mut out = args.to_vec();
    if !out.iter().any(|a| a == "--foreground") {
        out.push("--foreground".to_string());
    }
    out
}

/// Start `exe args…` in its own session, with stdio pointed at `log`.
///
/// `setsid` is what makes it a daemon rather than a background job: the child leaves the
/// terminal's session, so closing the terminal does not `SIGHUP` the desktop session out
/// from under an agent that is using it.
///
/// **Not unit-tested** — see the module docs.
pub fn spawn_detached(exe: &Path, args: &[String], log: &Path) -> io::Result<Child> {
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)?;
    let err = out.try_clone()?;

    let mut command = Command::new(exe);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err));

    // SAFETY: `setsid` is async-signal-safe and allocates nothing; it is the only thing
    // this closure does between fork and exec.
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn()
}

/// Wait for the child to write its state file, or to die trying.
///
/// `poll` reports whether the child is still alive; `nap` is the pause between looks.
/// Both are injected so the whole loop — the happy path, the dead child, the timeout —
/// is a unit test that neither forks nor sleeps.
///
/// The state file must carry `child_pid`: a file left by a *different* live bolide is
/// somebody else's session, and reporting its endpoint would point every later command
/// at the wrong desktop.
pub fn await_startup(
    dir: &Path,
    child_pid: u32,
    poll: &mut dyn FnMut() -> io::Result<ChildStatus>,
    nap: &dyn Fn(),
    attempts: usize,
    log: &Path,
) -> Result<SessionState, CliError> {
    for _ in 0..attempts {
        if let Some(state) = ours(dir, child_pid) {
            return Ok(state);
        }
        let status = poll()
            .map_err(|e| CliError::failure(format!("lost track of the bolide daemon: {e}")))?;
        if let ChildStatus::Exited(code) = status {
            // One last look: the child may have written the file and exited between the
            // check above and this one.
            if let Some(state) = ours(dir, child_pid) {
                return Ok(state);
            }
            return Err(CliError::failure(format!(
                "bolide connect failed (the daemon exited {code}); its log is {}{}",
                log.display(),
                quoted_tail(log)
            )));
        }
        nap();
    }
    Err(CliError::failure(format!(
        "timed out waiting for the bolide server to start; its log is {}{}",
        log.display(),
        quoted_tail(log)
    )))
}

fn ours(dir: &Path, child_pid: u32) -> Option<SessionState> {
    state::load_in(dir).filter(|s| s.pid == child_pid)
}

fn quoted_tail(log: &Path) -> String {
    let tail = log_tail(log, LOG_TAIL_LINES);
    if tail.is_empty() {
        String::new()
    } else {
        format!(":\n{tail}")
    }
}

/// The last `lines` lines of a file, without a trailing newline. `""` if there is no
/// file or nothing in it.
pub fn log_tail(path: &Path, lines: usize) -> String {
    let body = std::fs::read_to_string(path).unwrap_or_default();
    let kept: Vec<&str> = body
        .lines()
        .rev()
        .take(lines)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    kept.join("\n")
}

/// This executable, for the re-exec.
pub fn current_exe() -> Result<PathBuf, CliError> {
    std::env::current_exe().map_err(|e| {
        CliError::failure(format!("bolide cannot find its own binary to re-exec: {e}"))
    })
}

/// [`ChildStatus`] for a real `std::process::Child`.
pub fn status_of(child: &mut Child) -> io::Result<ChildStatus> {
    match child.try_wait()? {
        None => Ok(ChildStatus::Running),
        Some(status) => Ok(ChildStatus::Exited(status.code().unwrap_or(-1))),
    }
}
