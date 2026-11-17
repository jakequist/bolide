//! `connect --action-log FILE`'s writer: the header, one line per request, the times,
//! and what happens when the file cannot be written.

mod common;

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use bolide_cli::action_log::{ActionLogFile, Anchor, Header, RecordingInfo};
use bolide_server::{ActionLog, LoggedRequest, PointerMode, TypingMode};
use serde_json::{json, Map, Value};

use common::TempDir;

/// Connected at 2025-11-13T02:13:20.123Z, which is `base` on the monotonic clock.
fn anchor(base: Instant) -> Anchor {
    Anchor {
        instant: base,
        wall: UNIX_EPOCH + Duration::from_millis(1_763_000_000_123),
    }
}

fn header(recording: Option<RecordingInfo>) -> Header {
    Header {
        width: 1280,
        height: 800,
        pointer: PointerMode::Humanized,
        typing: TypingMode::Direct,
        recording,
    }
}

fn request(
    action: &str,
    params: Value,
    start: Instant,
    end: Instant,
    error: Option<&str>,
) -> LoggedRequest {
    let Value::Object(params) = params else {
        panic!("params must be an object");
    };
    LoggedRequest {
        action: action.into(),
        params,
        start,
        end,
        error: error.map(String::from),
    }
}

/// Every line in the file, as JSON, read from disk.
fn lines_of(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .expect("the log is readable")
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{l:?} is not JSON: {e}")))
        .collect()
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

#[test]
fn the_header_says_what_was_connected_and_how() {
    let dir = TempDir::new("alog-header");
    let base = Instant::now();

    let path = dir.path().join("plain.jsonl");
    let _log = ActionLogFile::create(&path, &header(None), anchor(base)).unwrap();
    assert_eq!(
        lines_of(&path),
        vec![json!({
            "bolide": env!("CARGO_PKG_VERSION"),
            "desktop": {"width": 1280, "height": 800},
            "connected": "2025-11-13T02:13:20.123Z",
            "record": null,
            "pointer": "humanized",
            "typing": "direct",
        })]
    );

    let path = dir.path().join("recorded.jsonl");
    let recording = RecordingInfo {
        file: PathBuf::from("/runs/a.mp4"),
        fps: 30,
        zero: base + ms(250),
    };
    let _log = ActionLogFile::create(&path, &header(Some(recording)), anchor(base)).unwrap();
    assert_eq!(
        lines_of(&path)[0]["record"],
        json!({"file": "/runs/a.mp4", "fps": 30, "start": "2025-11-13T02:13:20.373Z"})
    );
}

#[test]
fn a_line_is_what_was_asked_when_and_how_it_ended() {
    let dir = TempDir::new("alog-lines");
    let path = dir.path().join("a.jsonl");
    let base = Instant::now();
    let log = ActionLogFile::create(&path, &header(None), anchor(base)).unwrap();

    log.record(request(
        "type",
        json!({"text": "hunter2\n"}),
        base + ms(1_000),
        base + ms(1_877),
        None,
    ));
    log.record(request(
        "left_click",
        json!({"coordinate": [9000, 1]}),
        base + ms(2_000),
        base + ms(2_001),
        Some("[9000, 1] is outside the 1280x800 screen"),
    ));

    let lines = lines_of(&path);
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert_eq!(
        lines[1],
        json!({
            "seq": 1,
            "action": "type",
            "params": {"text": "hunter2\n"},
            "start": "2025-11-13T02:13:21.123Z",
            "end": "2025-11-13T02:13:22.000Z",
            "startS": 1.0,
            "endS": 1.877,
            "ok": true,
        })
    );
    assert_eq!(
        lines[2],
        json!({
            "seq": 2,
            "action": "left_click",
            "params": {"coordinate": [9000, 1]},
            "start": "2025-11-13T02:13:22.123Z",
            "end": "2025-11-13T02:13:22.124Z",
            "startS": 2.0,
            "endS": 2.001,
            "ok": false,
            "error": "[9000, 1] is outside the 1280x800 screen",
        })
    );
}

/// With a recording, the offsets count from its first frame, so `startS` is where the
/// request is in the video. The wall-clock times still say when it happened.
#[test]
fn with_a_recording_the_offsets_count_from_its_first_frame() {
    let dir = TempDir::new("alog-offsets");
    let path = dir.path().join("a.jsonl");
    let base = Instant::now();
    let recording = RecordingInfo {
        file: PathBuf::from("/runs/a.mp4"),
        fps: 30,
        zero: base + ms(250),
    };
    let log = ActionLogFile::create(&path, &header(Some(recording)), anchor(base)).unwrap();
    log.record(request(
        "screenshot",
        json!({}),
        base + ms(1_000),
        base + ms(1_500),
        None,
    ));
    let line = &lines_of(&path)[1];
    assert_eq!(line["startS"], json!(0.75));
    assert_eq!(line["endS"], json!(1.25));
    assert_eq!(line["start"], json!("2025-11-13T02:13:21.123Z"));
}

/// Each line is on disk as soon as it is recorded, so a crash keeps every line before it.
#[test]
fn each_line_is_on_disk_before_the_next_request() {
    let dir = TempDir::new("alog-flush");
    let path = dir.path().join("a.jsonl");
    let base = Instant::now();
    let log = ActionLogFile::create(&path, &header(None), anchor(base)).unwrap();
    for n in 1..=3u64 {
        log.record(request(
            "key",
            json!({"text": "Return"}),
            base + ms(n * 100),
            base + ms(n * 100 + 10),
            None,
        ));
        let lines = lines_of(&path);
        assert_eq!(lines.len() as u64, n + 1, "line {n} not on disk yet");
        assert_eq!(lines[n as usize]["seq"], json!(n));
    }
}

/// Writes fail while `failing` is set; otherwise they go to `written`.
#[derive(Clone, Default)]
struct Flaky {
    failing: Arc<Mutex<bool>>,
    written: Arc<Mutex<Vec<u8>>>,
}

impl Write for Flaky {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if *self.failing.lock().unwrap() {
            return Err(io::Error::other("disk full"));
        }
        self.written.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A line that cannot be written is lost, and counted; the log goes on, and the gap in
/// `seq` shows where.
#[test]
fn a_line_that_cannot_be_written_is_counted_and_the_log_goes_on() {
    let out = Flaky::default();
    let base = Instant::now();
    let log = ActionLogFile::new(
        out.clone(),
        Path::new("x.jsonl"),
        &header(None),
        anchor(base),
    )
    .unwrap();
    let click = |n: u64| {
        request(
            "left_click",
            json!({"coordinate": [n, n]}),
            base + ms(n),
            base + ms(n),
            None,
        )
    };

    log.record(click(1));
    *out.failing.lock().unwrap() = true;
    log.record(click(2));
    log.record(click(3));
    *out.failing.lock().unwrap() = false;
    log.record(click(4));

    assert_eq!(log.lost(), 2);
    let written = String::from_utf8(out.written.lock().unwrap().clone()).unwrap();
    let seqs: Vec<Value> = written
        .lines()
        .skip(1)
        .map(|l| serde_json::from_str::<Value>(l).unwrap()["seq"].clone())
        .collect();
    assert_eq!(seqs, vec![json!(1), json!(4)]);
}

/// The header is the one write that must succeed: a log that cannot be started is a
/// `connect` that fails, not a session that runs unlogged.
#[test]
fn a_log_whose_header_cannot_be_written_is_an_error() {
    let out = Flaky::default();
    *out.failing.lock().unwrap() = true;
    let err = ActionLogFile::new(
        out,
        Path::new("x.jsonl"),
        &header(None),
        anchor(Instant::now()),
    )
    .unwrap_err();
    assert!(err.to_string().contains("disk full"), "{err}");
}

/// A request without parameters has `{}`, never `null`.
#[test]
fn a_request_without_parameters_has_an_empty_object() {
    let dir = TempDir::new("alog-empty");
    let path = dir.path().join("a.jsonl");
    let base = Instant::now();
    let log = ActionLogFile::create(&path, &header(None), anchor(base)).unwrap();
    log.record(LoggedRequest {
        action: "clipboard_get".into(),
        params: Map::new(),
        start: base,
        end: base,
        error: None,
    });
    assert_eq!(lines_of(&path)[1]["params"], json!({}));
}
