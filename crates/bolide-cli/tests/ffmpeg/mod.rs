//! What the recording tests need from a real `ffmpeg`: whether there is one, what
//! `ffprobe` says about a file, and the colour at the centre of its last frame.
//!
//! These tests need `ffmpeg` and `ffprobe` on PATH. Without them a test says so on
//! stderr and passes, so a laptop without ffmpeg still runs the rest of the suite —
//! unless `BOLIDE_REQUIRE_FFMPEG=1`, which CI sets: there a missing ffmpeg is a
//! failure, not a silent skip.

use std::path::Path;
use std::process::Command;

use bolide_cli::record::find_on_path;

/// Whether to run an ffmpeg test. Panics when ffmpeg is required and missing.
pub fn have_ffmpeg(test: &str) -> bool {
    let path = std::env::var_os("PATH");
    let found = ["ffmpeg", "ffprobe"]
        .iter()
        .all(|p| find_on_path(p, path.as_deref()).is_some());
    if found {
        return true;
    }
    if std::env::var("BOLIDE_REQUIRE_FFMPEG").as_deref() == Ok("1") {
        panic!("BOLIDE_REQUIRE_FFMPEG=1, and there is no ffmpeg and ffprobe on PATH");
    }
    eprintln!(
        "skipping {test}: no ffmpeg and ffprobe on PATH (BOLIDE_REQUIRE_FFMPEG=1 makes this \
         a failure)"
    );
    false
}

/// What `ffprobe` says about a file's first video stream.
#[derive(Debug)]
pub struct Probe {
    pub codec: String,
    pub width: u64,
    pub height: u64,
    pub frames: u64,
    pub duration: f64,
}

pub fn probe(file: &Path) -> Probe {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-count_frames",
            "-show_entries",
            "stream=codec_name,width,height,nb_read_frames:format=duration",
            "-of",
            "json",
        ])
        .arg(file)
        .output()
        .expect("ffprobe runs");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "ffprobe rejected {}: {}{text}",
        file.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    let json: serde_json::Value = serde_json::from_str(&text).expect("ffprobe's json");
    let stream = &json["streams"][0];
    let number = |v: &serde_json::Value| -> f64 {
        v.as_str()
            .and_then(|s| s.parse().ok())
            .or_else(|| v.as_f64())
            .unwrap_or_else(|| panic!("not a number: {v} in {json}"))
    };
    Probe {
        codec: stream["codec_name"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        width: stream["width"].as_u64().unwrap_or_default(),
        height: stream["height"].as_u64().unwrap_or_default(),
        frames: number(&stream["nb_read_frames"]) as u64,
        duration: number(&json["format"]["duration"]),
    }
}

/// The RGB at the centre of the last frame `file` decodes to.
pub fn last_frame_centre(file: &Path, width: usize, height: usize) -> [u8; 3] {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(file)
        .args(["-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
        .output()
        .expect("ffmpeg runs");
    assert!(
        out.status.success(),
        "ffmpeg could not decode {}: {}",
        file.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    let size = width * height * 3;
    assert!(
        out.stdout.len() >= size && out.stdout.len() % size == 0,
        "{} bytes is not a whole number of {width}x{height} frames",
        out.stdout.len()
    );
    let last = &out.stdout[out.stdout.len() - size..];
    let i = ((height / 2) * width + width / 2) * 3;
    [last[i], last[i + 1], last[i + 2]]
}

/// Whether two colours are equal within what a lossy codec is allowed to move them.
pub fn close_to(got: [u8; 3], want: [u8; 3]) -> bool {
    got.iter().zip(want).all(|(g, w)| g.abs_diff(w) <= 24)
}
