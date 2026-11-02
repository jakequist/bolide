//! `connect --record FILE`: the session's desktop, as a video.
//!
//! The pure half: which container a file name asks for, the size of the video, how a
//! frame of whatever size the desktop has now is fitted onto it, and the `ffmpeg`
//! command line that encodes it.
//!
//! # Choices
//!
//! - **The extension decides**: `.mp4` is H.264 (`libx264`, `yuv420p`, so every player
//!   opens it), `.webm` is VP8 (`libvpx`). Any other extension is malformed input and a
//!   usage error, rather than a guess at what was meant.
//! - **The MP4 is fragmented** (`+frag_keyframe+empty_moov`) with a keyframe every two
//!   seconds, so a recording cut off by a crash or a `kill -9` still plays up to its last
//!   fragment. An ordinary MP4 writes its index at the end, and without one it is noise.
//! - **The video's size is the desktop's size when the session started**, each side
//!   rounded up to even ([`Geometry`]), because 4:2:0 chroma cannot encode an odd one.
//! - **A resize mid-session does not change the video's size**: a video stream has one.
//!   A frame that no longer matches is fitted ([`fit`]): one that fits is centred
//!   unscaled, one that does not is scaled down, aspect kept, and centred. The bars are
//!   black.

use std::ffi::{OsStr, OsString};
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bolide_rfb::{Framebuffer, ServerEvent, Session};
use tokio::sync::broadcast::error::RecvError;

use crate::error::CliError;

/// Frames per second a recording samples the framebuffer at.
pub const FPS: u32 = 10;

/// The container, and with it the codec, a recording is written in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Container {
    /// H.264 in a fragmented MP4.
    Mp4,
    /// VP8 in WebM.
    Webm,
}

impl Container {
    /// The container `path`'s extension names, case-insensitively.
    pub fn for_path(path: &Path) -> Result<Container, String> {
        let extension = path
            .extension()
            .and_then(OsStr::to_str)
            .map(str::to_ascii_lowercase);
        match extension.as_deref() {
            Some("mp4") => Ok(Container::Mp4),
            Some("webm") => Ok(Container::Webm),
            _ => Err(format!(
                "a recording is a .mp4 or a .webm file, and {} is neither",
                path.display()
            )),
        }
    }
}

/// `--record`'s value parser: a path whose extension is one bolide can write.
pub fn parse_record_path(value: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(value);
    Container::for_path(&path)?;
    Ok(path)
}

/// The video's frame size. Both sides are even, because H.264 in 4:2:0 cannot encode
/// an odd one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    /// Width in pixels; even.
    pub width: u16,
    /// Height in pixels; even.
    pub height: u16,
}

impl Geometry {
    /// The video size for a desktop of `width` x `height`: each side rounded up to even
    /// (down, at the very top of `u16`, where up does not exist).
    pub fn for_screen(width: u16, height: u16) -> Geometry {
        fn even(side: u16) -> u16 {
            match side {
                0 | 1 => 2,
                u16::MAX => u16::MAX - 1,
                n => n + n % 2,
            }
        }
        Geometry {
            width: even(width),
            height: even(height),
        }
    }
}

/// `frame` as RGBA at exactly `canvas`'s size: centred unscaled when it fits, scaled
/// down (nearest neighbour, aspect kept) when it does not, on black. See the module
/// docs for why.
pub fn fit(frame: &Framebuffer, canvas: Geometry) -> Vec<u8> {
    let (cw, ch) = (canvas.width as usize, canvas.height as usize);
    let (fw, fh) = (frame.width as usize, frame.height as usize);
    if (fw, fh) == (cw, ch) {
        return frame.rgba.clone();
    }
    let mut out = [0, 0, 0, 255].repeat(cw * ch);
    if fw == 0 || fh == 0 {
        return out;
    }
    // Scaled size: the frame's own when it fits, else the largest that does with the
    // aspect kept. Integer arithmetic, so a frame always lands on the same pixels.
    let (sw, sh) = if fw <= cw && fh <= ch {
        (fw, fh)
    } else if fw * ch >= fh * cw {
        (cw, (fh * cw / fw).max(1))
    } else {
        ((fw * ch / fh).max(1), ch)
    };
    let (ox, oy) = ((cw - sw) / 2, (ch - sh) / 2);
    for dy in 0..sh {
        let sy = dy * fh / sh;
        let row = (oy + dy) * cw;
        if sw == fw {
            let src = sy * fw * 4;
            let dst = (row + ox) * 4;
            out[dst..dst + sw * 4].copy_from_slice(&frame.rgba[src..src + fw * 4]);
            continue;
        }
        for dx in 0..sw {
            let src = (sy * fw + dx * fw / sw) * 4;
            let dst = (row + ox + dx) * 4;
            out[dst..dst + 4].copy_from_slice(&frame.rgba[src..src + 4]);
        }
    }
    out
}

/// The arguments (without the program name) of the `ffmpeg` that encodes raw RGBA
/// frames of `geometry`, at `fps`, from its stdin into `out`.
pub fn ffmpeg_args(
    container: Container,
    geometry: Geometry,
    fps: u32,
    out: &Path,
) -> Vec<OsString> {
    let size = format!("{}x{}", geometry.width, geometry.height);
    let rate = fps.to_string();
    // A keyframe every two seconds: what a cut-off MP4 loses at most, and how far a
    // player decodes to seek.
    let gop = (fps * 2).max(1).to_string();
    let mut args: Vec<&str> = vec![
        "-hide_banner",
        "-nostats",
        "-loglevel",
        "error",
        "-y",
        "-f",
        "rawvideo",
        "-pix_fmt",
        "rgba",
        "-s",
        &size,
        "-r",
        &rate,
        "-i",
        "-",
    ];
    match container {
        Container::Mp4 => args.extend([
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-pix_fmt",
            "yuv420p",
            "-g",
            &gop,
            "-movflags",
            "+frag_keyframe+empty_moov+default_base_moof",
            "-f",
            "mp4",
        ]),
        Container::Webm => args.extend([
            "-c:v",
            "libvpx",
            "-deadline",
            "realtime",
            "-cpu-used",
            "8",
            "-b:v",
            "2M",
            "-pix_fmt",
            "yuv420p",
            "-g",
            &gop,
            "-f",
            "webm",
        ]),
    }
    let mut args: Vec<OsString> = args.into_iter().map(OsString::from).collect();
    args.push(out.as_os_str().to_owned());
    args
}

/// The first executable called `program` in a `PATH`-style list of directories.
pub fn find_on_path(program: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(path?)
        .map(|dir| dir.join(program))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// `ffmpeg`, or the error that says `--record` needs it. Checked before the session
/// starts, so a missing encoder is a clear failure up front rather than a session that
/// runs for an hour and records nothing.
pub fn require_ffmpeg(path: Option<&OsStr>) -> Result<PathBuf, CliError> {
    find_on_path("ffmpeg", path).ok_or_else(|| {
        CliError::failure(
            "--record needs ffmpeg, and there is none on PATH: install it (`apt install \
             ffmpeg`, `brew install ffmpeg`) or connect without --record",
        )
    })
}

// ---------------------------------------------------------------- the recorder

/// Where frames go: `ffmpeg`'s stdin in a session, a channel in a test.
pub trait FrameSink: Send {
    /// One frame, RGBA, exactly the recording's [`Geometry`].
    fn write_frame(&mut self, rgba: &[u8]) -> impl Future<Output = io::Result<()>> + Send;
    /// No more frames: finalise the file.
    fn finish(self) -> impl Future<Output = io::Result<()>> + Send;
}

/// When to take the next frame: a fixed-rate clock in a session, the test's say-so in a
/// test. `false` means there will be no more ticks.
pub trait Ticker: Send {
    /// Wait for the next tick.
    fn tick(&mut self) -> impl Future<Output = bool> + Send;
}

/// Record `session` until `stop` resolves, the desktop goes away or `ticker` runs out:
/// on each tick the framebuffer, [`fit`] to `canvas`, goes to `sink`. Then `sink` is
/// finished. Returns how many frames were written.
///
/// A recording always has at least one frame: one stopped before its first tick gets the
/// framebuffer as it is then, because a file with no frames is not a video.
///
/// A sink that fails (`ffmpeg` died, the disk filled) ends the recording, not the
/// session: the sink is still finished, so the child is reaped, and the error returned.
pub async fn record<T: Ticker, S: FrameSink>(
    session: Arc<dyn Session>,
    canvas: Geometry,
    mut ticker: T,
    mut sink: S,
    stop: impl Future<Output = ()> + Send,
) -> io::Result<u64> {
    let mut events = session.events();
    let mut frames = 0u64;
    let mut failed = None;
    tokio::pin!(stop);
    loop {
        let take = tokio::select! {
            biased;
            () = &mut stop => false,
            event = events.recv() => match event {
                Ok(ServerEvent::Disconnected { .. }) | Err(RecvError::Closed) => false,
                Ok(_) | Err(RecvError::Lagged(_)) => continue,
            },
            more = ticker.tick() => more,
        };
        if !take {
            break;
        }
        if let Err(e) = write_one(&*session, canvas, &mut sink).await {
            failed = Some(e);
            break;
        }
        frames += 1;
    }
    if frames == 0 && failed.is_none() {
        match write_one(&*session, canvas, &mut sink).await {
            Ok(()) => frames = 1,
            Err(e) => failed = Some(e),
        }
    }
    let finished = sink.finish().await;
    match failed {
        Some(e) => Err(e),
        None => finished.map(|()| frames),
    }
}

async fn write_one<S: FrameSink>(
    session: &dyn Session,
    canvas: Geometry,
    sink: &mut S,
) -> io::Result<()> {
    // `framebuffer` answers even after a disconnect (the last frame is the last true
    // thing we know), so the only error here is a sink's.
    let frame = session
        .framebuffer()
        .await
        .map_err(|e| io::Error::other(e.to_string()))?;
    sink.write_frame(&fit(&frame, canvas)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::EXIT_FAILURE;

    const BLACK: [u8; 4] = [0, 0, 0, 255];
    const RED: [u8; 4] = [255, 0, 0, 255];

    fn solid(width: u16, height: u16, rgba: [u8; 4]) -> Framebuffer {
        Framebuffer {
            width,
            height,
            rgba: rgba.repeat(width as usize * height as usize),
        }
    }

    fn pixel(rgba: &[u8], canvas: Geometry, x: u16, y: u16) -> [u8; 4] {
        let i = (y as usize * canvas.width as usize + x as usize) * 4;
        [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
    }

    fn strings(args: &[OsString]) -> Vec<&str> {
        args.iter().map(|a| a.to_str().unwrap()).collect()
    }

    #[test]
    fn the_extension_picks_the_container_whatever_its_case() {
        for (name, want) in [
            ("x.mp4", Container::Mp4),
            ("/tmp/X.MP4", Container::Mp4),
            ("run.webm", Container::Webm),
            ("run.WebM", Container::Webm),
        ] {
            assert_eq!(Container::for_path(Path::new(name)), Ok(want), "{name}");
        }
    }

    #[test]
    fn another_extension_is_rejected_and_the_error_names_the_two_that_work() {
        for name in ["x.mkv", "x.gif", "x", "mp4", "x.mp4.tmp"] {
            let err = Container::for_path(Path::new(name)).unwrap_err();
            assert!(
                err.contains(".mp4") && err.contains(".webm"),
                "{name}: {err}"
            );
            assert!(err.contains(name), "should quote what it got: {err}");
        }
    }

    #[test]
    fn the_value_parser_keeps_the_path_as_given() {
        assert_eq!(
            parse_record_path("out/x.mp4"),
            Ok(PathBuf::from("out/x.mp4"))
        );
        assert!(parse_record_path("x.avi").is_err());
    }

    #[test]
    fn the_video_size_is_the_screen_size_rounded_up_to_even() {
        assert_eq!(
            Geometry::for_screen(64, 48),
            Geometry {
                width: 64,
                height: 48
            }
        );
        assert_eq!(
            Geometry::for_screen(65, 47),
            Geometry {
                width: 66,
                height: 48
            }
        );
        assert_eq!(
            Geometry::for_screen(1, 1),
            Geometry {
                width: 2,
                height: 2
            }
        );
        assert_eq!(
            Geometry::for_screen(u16::MAX, 1),
            Geometry {
                width: u16::MAX - 1,
                height: 2
            },
            "rounding down is the only way to stay even at the top of u16"
        );
    }

    #[test]
    fn a_frame_of_the_canvas_size_is_copied_unchanged() {
        let mut frame = solid(4, 2, RED);
        frame.rgba[4..8].copy_from_slice(&[1, 2, 3, 255]);
        let canvas = Geometry {
            width: 4,
            height: 2,
        };
        assert_eq!(fit(&frame, canvas), frame.rgba);
    }

    #[test]
    fn an_odd_frame_is_padded_with_black_on_the_right_and_bottom() {
        let frame = solid(3, 3, RED);
        let canvas = Geometry::for_screen(3, 3);
        let out = fit(&frame, canvas);
        assert_eq!(out.len(), 4 * 4 * 4);
        for y in 0..4 {
            for x in 0..4 {
                let want = if x < 3 && y < 3 { RED } else { BLACK };
                assert_eq!(pixel(&out, canvas, x, y), want, "({x}, {y})");
            }
        }
    }

    #[test]
    fn a_larger_frame_is_scaled_down_to_fit_and_letterboxed() {
        // 8x2 into 4x4: half size, 4x1, centred vertically on row 1.
        let mut frame = solid(8, 2, RED);
        frame.rgba[..8].copy_from_slice(&[0, 255, 0, 255, 0, 255, 0, 255]);
        let canvas = Geometry {
            width: 4,
            height: 4,
        };
        let out = fit(&frame, canvas);
        assert_eq!(out.len(), 4 * 4 * 4);
        for x in 0..4 {
            assert_eq!(pixel(&out, canvas, x, 0), BLACK, "bar above, x {x}");
            assert_eq!(pixel(&out, canvas, x, 2), BLACK, "bar below, x {x}");
            assert_eq!(pixel(&out, canvas, x, 3), BLACK, "bar below, x {x}");
        }
        assert_eq!(
            pixel(&out, canvas, 0, 1),
            [0, 255, 0, 255],
            "sampled from (0, 0)"
        );
        assert_eq!(pixel(&out, canvas, 3, 1), RED);
    }

    #[test]
    fn a_smaller_frame_is_centred_unscaled() {
        let frame = solid(2, 2, RED);
        let canvas = Geometry {
            width: 4,
            height: 4,
        };
        let out = fit(&frame, canvas);
        for y in 0..4 {
            for x in 0..4 {
                let want = if (1..3).contains(&x) && (1..3).contains(&y) {
                    RED
                } else {
                    BLACK
                };
                assert_eq!(pixel(&out, canvas, x, y), want, "({x}, {y})");
            }
        }
    }

    #[test]
    fn mp4_is_h264_yuv420p_and_fragmented_so_a_cut_off_file_still_plays() {
        let args = ffmpeg_args(
            Container::Mp4,
            Geometry {
                width: 64,
                height: 48,
            },
            10,
            Path::new("/tmp/run.mp4"),
        );
        assert_eq!(
            strings(&args),
            vec![
                "-hide_banner",
                "-nostats",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgba",
                "-s",
                "64x48",
                "-r",
                "10",
                "-i",
                "-",
                "-c:v",
                "libx264",
                "-preset",
                "veryfast",
                "-pix_fmt",
                "yuv420p",
                "-g",
                "20",
                "-movflags",
                "+frag_keyframe+empty_moov+default_base_moof",
                "-f",
                "mp4",
                "/tmp/run.mp4",
            ]
        );
    }

    #[test]
    fn webm_is_vp8() {
        let args = ffmpeg_args(
            Container::Webm,
            Geometry {
                width: 1920,
                height: 1080,
            },
            5,
            Path::new("run.webm"),
        );
        assert_eq!(
            strings(&args),
            vec![
                "-hide_banner",
                "-nostats",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgba",
                "-s",
                "1920x1080",
                "-r",
                "5",
                "-i",
                "-",
                "-c:v",
                "libvpx",
                "-deadline",
                "realtime",
                "-cpu-used",
                "8",
                "-b:v",
                "2M",
                "-pix_fmt",
                "yuv420p",
                "-g",
                "10",
                "-f",
                "webm",
                "run.webm",
            ]
        );
    }

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let path = std::env::temp_dir()
                .join(format!("bolide-record-unit-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Scratch(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn file_with_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn a_program_is_found_in_the_first_directory_that_has_it_executable() {
        let scratch = Scratch::new("path");
        let (a, b, c) = (
            scratch.0.join("a"),
            scratch.0.join("b"),
            scratch.0.join("c"),
        );
        for dir in [&a, &b, &c] {
            std::fs::create_dir_all(dir).unwrap();
        }
        file_with_mode(&a.join("ffmpeg"), 0o644);
        file_with_mode(&b.join("ffmpeg"), 0o755);
        file_with_mode(&c.join("ffmpeg"), 0o755);
        let path = std::env::join_paths([&a, &b, &c]).unwrap();

        assert_eq!(find_on_path("ffmpeg", Some(&path)), Some(b.join("ffmpeg")));
        assert_eq!(find_on_path("ffprobe", Some(&path)), None);
        assert_eq!(find_on_path("ffmpeg", None), None);
    }

    #[test]
    fn without_ffmpeg_the_error_says_record_needs_it() {
        let scratch = Scratch::new("missing");
        let err = require_ffmpeg(Some(scratch.0.as_os_str())).unwrap_err();
        assert_eq!(err.code, EXIT_FAILURE);
        assert!(
            err.message.contains("--record") && err.message.contains("ffmpeg"),
            "{}",
            err.message
        );
        assert!(require_ffmpeg(None).is_err());
    }
}
