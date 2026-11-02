//! A recording through a real `ffmpeg`, of a real (in-process) RFB server: the file is
//! a video `ffprobe` accepts, the size the module docs promise, and what the desktop
//! showed last is what its last frame shows. See `ffmpeg/mod.rs` for when these run.

mod common;
mod ffmpeg;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bolide_cli::record::{record, Container, FfmpegSink, Geometry, Ticker};
use bolide_rfb::{ServerEvent, Session};
use bolide_testkit::{FakeServer, Screen};
use tokio::sync::mpsc;

use common::TempDir;

const RED: [u8; 4] = [255, 0, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];
const PATIENCE: Duration = Duration::from_secs(10);

struct TestTicker(mpsc::UnboundedReceiver<()>);

impl Ticker for TestTicker {
    async fn tick(&mut self) -> bool {
        self.0.recv().await.is_some()
    }
}

/// Wait until the session's framebuffer is solid `rgba`.
async fn until_solid(session: &dyn Session, rgba: [u8; 4]) {
    let mut events = session.events();
    tokio::time::timeout(PATIENCE, async {
        loop {
            let fb = session.framebuffer().await.unwrap();
            if fb.rgba.chunks_exact(4).all(|px| px == rgba) {
                return;
            }
            if let Ok(ServerEvent::Disconnected { .. }) = events.recv().await {
                panic!("disconnected");
            }
        }
    })
    .await
    .expect("the screen never arrived");
}

/// Record red for five ticks, then blue for five, into `file`.
async fn red_then_blue(file: &Path) {
    // Odd on purpose: the video has to be padded to even.
    let server = FakeServer::builder()
        .screen(Screen::solid(63, 47, RED))
        .start()
        .await
        .unwrap();
    let handle = bolide_rfb::connect(&server.addr().to_string(), Default::default())
        .await
        .unwrap();
    handle.refresh(false, PATIENCE).await.unwrap();
    let session: Arc<dyn Session> = Arc::new(handle);

    let canvas = Geometry::for_screen(63, 47);
    let container = Container::for_path(file).unwrap();
    let sink =
        FfmpegSink::spawn(Path::new("ffmpeg"), container, canvas, 10, file).expect("ffmpeg starts");
    let (ticks, rx) = mpsc::unbounded_channel();
    let recording = tokio::spawn(record(
        Arc::clone(&session),
        canvas,
        TestTicker(rx),
        sink,
        std::future::pending(),
    ));

    for _ in 0..5 {
        ticks.send(()).unwrap();
    }
    server
        .control()
        .set_screen(Screen::solid(63, 47, BLUE))
        .await;
    // The ticks above may not all have been taken yet; once the framebuffer is blue,
    // every later tick is a blue frame.
    until_solid(&*session, BLUE).await;
    for _ in 0..5 {
        ticks.send(()).unwrap();
    }
    drop(ticks);

    let frames = tokio::time::timeout(PATIENCE, recording)
        .await
        .expect("finalising took too long")
        .unwrap()
        .expect("the recording finished cleanly");
    assert_eq!(frames, 10);
    server.shutdown().await;
}

async fn check(name: &str, codec: &str) {
    if !ffmpeg::have_ffmpeg(name) {
        return;
    }
    let dir = TempDir::new("rec-ffmpeg");
    let file = dir.path().join(name);
    red_then_blue(&file).await;

    let probe = ffmpeg::probe(&file);
    assert_eq!(probe.codec, codec, "{probe:?}");
    assert_eq!(
        (probe.width, probe.height),
        (64, 48),
        "padded to even: {probe:?}"
    );
    assert_eq!(probe.frames, 10, "{probe:?}");
    assert!(probe.duration > 0.0, "{probe:?}");

    let centre = ffmpeg::last_frame_centre(&file, 64, 48);
    assert!(
        ffmpeg::close_to(centre, [0, 0, 255]),
        "the last frame is not blue: {centre:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_mp4_recording_is_h264_and_ends_on_the_last_screen() {
    check("run.mp4", "h264").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_webm_recording_is_vp8_and_ends_on_the_last_screen() {
    check("run.webm", "vp8").await;
}
