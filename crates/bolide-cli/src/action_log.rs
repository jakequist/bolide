//! `connect --action-log FILE`: what was done in the session, as JSON Lines.
//!
//! The first line is a [`Header`]; each line after it is one request the server handled
//! (which ones: [`bolide_server::ActionLog`]), written when it was answered. The format is
//! the README's "Logging what was done", with an example.
//!
//! # Time
//!
//! Every time in the file comes from one monotonic clock, the server's, read against an
//! [`Anchor`]: one reading of the monotonic clock and of the wall clock, taken together
//! at connect. A line's wall-clock time is the anchor's plus how far the monotonic clock
//! has moved since, so a line's `start` and `startS` cannot disagree, and a wall clock
//! stepped mid-session (NTP) cannot put lines out of order. The offsets (`startS`,
//! `endS`) count from the recording's first frame when there is one, so an offset is a
//! position in the video, and from the connect when there is not.
//!
//! # When a line cannot be written
//!
//! The header is written at connect, and if it cannot be the connect fails. After that, a
//! line that cannot be written must not fail the request it describes: it is lost, and
//! counted ([`ActionLogFile::lost`]). The first loss is said on stderr (the daemon's log)
//! at once, and the count when the session ends. Lines keep their `seq` whether or not
//! they were written, so a gap in `seq` is where lines were lost. A write cut off part
//! way (a full disk) can leave a partial line.

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bolide_server::{ActionLog, LoggedRequest, PointerMode};
use serde::Serialize;
use serde_json::{Map, Value};

/// The `connect` flag that names the action log, without its `--`: the one place its
/// name is written. clap, the daemon's re-exec and the tests all read it from here.
pub const ACTION_LOG_FLAG: &str = "action-log";

/// The monotonic clock and the wall clock, read together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Anchor {
    /// The monotonic clock.
    pub instant: Instant,
    /// The wall clock at the same moment.
    pub wall: SystemTime,
}

impl Anchor {
    /// Both clocks, now. `instant` is read from the server's own clock, so it is on the
    /// same clock as the server's [`LoggedRequest`]s.
    pub fn now() -> Anchor {
        use bolide_server::Clock as _;
        Anchor {
            instant: bolide_server::TokioClock.now(),
            wall: SystemTime::now(),
        }
    }

    /// The wall-clock time of `at`, as ISO 8601 UTC with milliseconds.
    fn wall_time(&self, at: Instant) -> String {
        let wall = match at.checked_duration_since(self.instant) {
            Some(after) => self.wall + after,
            None => self.wall - self.instant.duration_since(at),
        };
        iso_millis(wall)
    }
}

/// The recording an action log goes with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordingInfo {
    /// The video file.
    pub file: PathBuf,
    /// Its frames a second.
    pub fps: u32,
    /// When its first frame was taken, on the monotonic clock: what the offsets count
    /// from.
    pub zero: Instant,
}

/// What the first line says about the session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    /// The desktop's width at connect.
    pub width: u16,
    /// The desktop's height at connect.
    pub height: u16,
    /// `--pointer`.
    pub pointer: PointerMode,
    /// The recording, if `--record` is on.
    pub recording: Option<RecordingInfo>,
}

#[derive(Serialize)]
struct HeaderLine<'a> {
    bolide: &'a str,
    desktop: Desktop,
    connected: String,
    record: Option<RecordLine>,
    pointer: &'a str,
}

#[derive(Serialize)]
struct Desktop {
    width: u16,
    height: u16,
}

#[derive(Serialize)]
struct RecordLine {
    file: String,
    fps: u32,
    start: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Line<'a> {
    seq: u64,
    action: &'a str,
    params: &'a Map<String, Value>,
    start: String,
    end: String,
    start_s: f64,
    end_s: f64,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
}

/// An action log being written to `W`: a file in a session.
pub struct ActionLogFile<W> {
    label: PathBuf,
    anchor: Anchor,
    zero: Instant,
    inner: Mutex<Inner<W>>,
}

struct Inner<W> {
    out: W,
    seq: u64,
    lost: u64,
}

impl ActionLogFile<File> {
    /// Create (or empty) the file at `path` and write the header to it.
    pub fn create(path: &Path, header: &Header, anchor: Anchor) -> io::Result<ActionLogFile<File>> {
        ActionLogFile::new(File::create(path)?, path, header, anchor)
    }
}

impl<W: Write + Send> ActionLogFile<W> {
    /// Write the header to `out`; `label` is what a message about it calls it. A header
    /// that cannot be written is an error.
    pub fn new(
        mut out: W,
        label: &Path,
        header: &Header,
        anchor: Anchor,
    ) -> io::Result<ActionLogFile<W>> {
        let zero = header.recording.as_ref().map_or(anchor.instant, |r| r.zero);
        let line = HeaderLine {
            bolide: env!("CARGO_PKG_VERSION"),
            desktop: Desktop {
                width: header.width,
                height: header.height,
            },
            connected: iso_millis(anchor.wall),
            record: header.recording.as_ref().map(|r| RecordLine {
                file: r.file.display().to_string(),
                fps: r.fps,
                start: anchor.wall_time(r.zero),
            }),
            pointer: header.pointer.as_str(),
        };
        write_line(&mut out, &line)?;
        Ok(ActionLogFile {
            label: label.to_path_buf(),
            anchor,
            zero,
            inner: Mutex::new(Inner {
                out,
                seq: 0,
                lost: 0,
            }),
        })
    }

    /// How many lines could not be written.
    pub fn lost(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .lost
    }

    /// The file's name, as given.
    pub fn path(&self) -> &Path {
        &self.label
    }
}

impl<W: Write + Send> ActionLog for ActionLogFile<W> {
    fn record(&self, request: LoggedRequest) {
        // A panic elsewhere while the lock was held left nothing half-done that matters
        // here: the worst is a lost line, which is already allowed for.
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        inner.seq += 1;
        let line = Line {
            seq: inner.seq,
            action: &request.action,
            params: &request.params,
            start: self.anchor.wall_time(request.start),
            end: self.anchor.wall_time(request.end),
            start_s: seconds_since(self.zero, request.start),
            end_s: seconds_since(self.zero, request.end),
            ok: request.error.is_none(),
            error: request.error.as_deref(),
        };
        if let Err(e) = write_line(&mut inner.out, &line) {
            if inner.lost == 0 {
                eprintln!(
                    "cannot write the action log {}: {e}; the session goes on, and each \
                     line that cannot be written is lost (a gap in seq)",
                    self.label.display()
                );
            }
            inner.lost += 1;
        }
    }
}

impl<W> std::fmt::Debug for ActionLogFile<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActionLogFile")
            .field("path", &self.label)
            .finish_non_exhaustive()
    }
}

/// One JSON line, in one write, flushed.
fn write_line(out: &mut impl Write, line: &impl Serialize) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(line).map_err(io::Error::other)?;
    bytes.push(b'\n');
    out.write_all(&bytes)?;
    out.flush()
}

/// Seconds from `zero` to `at`, to the millisecond; negative when `at` is earlier.
fn seconds_since(zero: Instant, at: Instant) -> f64 {
    let millis = |d: Duration| d.as_millis() as f64;
    match at.checked_duration_since(zero) {
        Some(after) => millis(after) / 1000.0,
        None => -millis(zero.duration_since(at)) / 1000.0,
    }
}

/// `at` as ISO 8601 UTC with milliseconds, e.g. `2025-11-13T02:13:20.123Z`.
pub fn iso_millis(at: SystemTime) -> String {
    let since = at.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = crate::state::rfc3339_from_unix(since.as_secs() as i64);
    // `…:SSZ` becomes `…:SS.mmmZ`.
    let whole = seconds.trim_end_matches('Z');
    format!("{whole}.{:03}Z", since.subsec_millis())
}
