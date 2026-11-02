//! The recorder against a real (in-process) RFB server, with a ticker the test drives
//! and a sink that hands each frame back: what is recorded is what the desktop showed
//! at each tick, and the recording ends — sink finished — when the session does.

use std::future::Future;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use bolide_cli::record::{record, FrameSink, Geometry, Ticker};
use bolide_rfb::{ServerEvent, Session};
use bolide_testkit::{FakeServer, Screen};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

const RED: [u8; 4] = [255, 0, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];
const BLACK: [u8; 4] = [0, 0, 0, 255];
const PATIENCE: Duration = Duration::from_secs(10);
const CANVAS: Geometry = Geometry {
    width: 64,
    height: 48,
};

/// A ticker that ticks when the test says, and ends when the test drops its sender.
struct TestTicker(mpsc::UnboundedReceiver<()>);

impl Ticker for TestTicker {
    async fn tick(&mut self) -> bool {
        self.0.recv().await.is_some()
    }
}

#[derive(Debug, PartialEq)]
enum Sunk {
    Frame(Vec<u8>),
    Finished,
}

/// A sink that hands what it is given back to the test.
struct TestSink(mpsc::UnboundedSender<Sunk>);

impl FrameSink for TestSink {
    fn write_frame(&mut self, rgba: &[u8]) -> impl Future<Output = io::Result<()>> + Send {
        let _ = self.0.send(Sunk::Frame(rgba.to_vec()));
        async { Ok(()) }
    }

    fn finish(self) -> impl Future<Output = io::Result<()>> + Send {
        let _ = self.0.send(Sunk::Finished);
        async { Ok(()) }
    }
}

struct Rig {
    server: FakeServer,
    session: Arc<dyn Session>,
    ticks: Option<mpsc::UnboundedSender<()>>,
    sunk: mpsc::UnboundedReceiver<Sunk>,
    stop: Option<oneshot::Sender<()>>,
    recording: JoinHandle<io::Result<u64>>,
}

impl Rig {
    async fn start(screen: Screen) -> Rig {
        let server = FakeServer::builder().screen(screen).start().await.unwrap();
        let handle = bolide_rfb::connect(&server.addr().to_string(), Default::default())
            .await
            .unwrap();
        handle.refresh(false, PATIENCE).await.expect("first paint");
        let session: Arc<dyn Session> = Arc::new(handle);

        let (ticks, tick_rx) = mpsc::unbounded_channel();
        let (sink_tx, sunk) = mpsc::unbounded_channel();
        let (stop, stop_rx) = oneshot::channel::<()>();
        let recording = tokio::spawn(record(
            Arc::clone(&session),
            CANVAS,
            TestTicker(tick_rx),
            TestSink(sink_tx),
            async {
                let _ = stop_rx.await;
            },
        ));
        Rig {
            server,
            session,
            ticks: Some(ticks),
            sunk,
            stop: Some(stop),
            recording,
        }
    }

    fn tick(&self) {
        self.ticks.as_ref().unwrap().send(()).unwrap();
    }

    async fn next(&mut self) -> Sunk {
        tokio::time::timeout(PATIENCE, self.sunk.recv())
            .await
            .expect("the sink heard nothing")
            .expect("the recorder dropped its sink without finishing it")
    }

    async fn frame(&mut self) -> Vec<u8> {
        match self.next().await {
            Sunk::Frame(rgba) => rgba,
            Sunk::Finished => panic!("finished where a frame was due"),
        }
    }

    /// Show `screen`, and wait until the session's framebuffer has it.
    async fn show(&self, screen: Screen) {
        let mut events = self.session.events();
        let want = screen.rgba.clone();
        let size = (screen.width, screen.height);
        self.server.control().set_screen(screen).await;
        tokio::time::timeout(PATIENCE, async {
            loop {
                let fb = self.session.framebuffer().await.unwrap();
                if (fb.width, fb.height) == size && fb.rgba == want {
                    return;
                }
                match events.recv().await {
                    Ok(ServerEvent::Disconnected { .. }) => panic!("disconnected"),
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(e) => panic!("{e}"),
                }
            }
        })
        .await
        .expect("the screen never arrived");
    }

    async fn stop(mut self) -> (io::Result<u64>, Vec<Sunk>) {
        let _ = self.stop.take().unwrap().send(());
        let result = tokio::time::timeout(PATIENCE, self.recording)
            .await
            .expect("the recorder did not stop")
            .unwrap();
        let mut rest = Vec::new();
        while let Ok(s) = self.sunk.try_recv() {
            rest.push(s);
        }
        self.server.shutdown().await;
        (result, rest)
    }
}

fn solid(canvas: Geometry, rgba: [u8; 4]) -> Vec<u8> {
    rgba.repeat(canvas.width as usize * canvas.height as usize)
}

fn pixel(rgba: &[u8], x: usize, y: usize) -> [u8; 4] {
    let i = (y * CANVAS.width as usize + x) * 4;
    [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
}

#[tokio::test(flavor = "multi_thread")]
async fn each_tick_is_one_frame_of_what_the_desktop_shows() {
    let mut rig = Rig::start(Screen::solid(64, 48, RED)).await;
    for _ in 0..3 {
        rig.tick();
    }
    for n in 0..3 {
        assert!(
            rig.frame().await == solid(CANVAS, RED),
            "frame {n} is not red"
        );
    }

    let (result, rest) = rig.stop().await;
    assert_eq!(rest, vec![Sunk::Finished]);
    assert_eq!(result.unwrap(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_screen_is_in_the_frames_after_it() {
    let mut rig = Rig::start(Screen::solid(64, 48, RED)).await;
    rig.tick();
    assert!(rig.frame().await == solid(CANVAS, RED));

    rig.show(Screen::solid(64, 48, BLUE)).await;
    rig.tick();
    rig.tick();
    assert!(rig.frame().await == solid(CANVAS, BLUE), "not blue");
    assert!(rig.frame().await == solid(CANVAS, BLUE), "not blue");

    let (result, _) = rig.stop().await;
    assert_eq!(result.unwrap(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resized_desktop_is_fitted_into_the_first_size() {
    let mut rig = Rig::start(Screen::solid(64, 48, RED)).await;
    rig.server.control().resize(32, 24).await;
    rig.show(Screen::solid(32, 24, BLUE)).await;
    rig.tick();
    let frame = rig.frame().await;
    assert_eq!(
        frame.len(),
        solid(CANVAS, BLACK).len(),
        "the video keeps its size"
    );
    assert_eq!(pixel(&frame, 32, 24), BLUE, "centred");
    assert_eq!(pixel(&frame, 0, 0), BLACK, "with a border");
    assert_eq!(pixel(&frame, 63, 47), BLACK, "with a border");
    rig.stop().await.0.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_desktop_going_away_finishes_the_recording() {
    let mut rig = Rig::start(Screen::solid(64, 48, RED)).await;
    rig.tick();
    assert!(rig.frame().await == solid(CANVAS, RED));

    rig.server.control().drop_connection().await;
    assert_eq!(rig.next().await, Sunk::Finished);
    let (result, _) = rig.stop().await;
    assert_eq!(result.unwrap(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_recording_stopped_before_its_first_tick_still_has_a_frame() {
    // A video with no frames is not a video; a session that ended at once still showed
    // something, and that is what the file holds.
    let mut rig = Rig::start(Screen::solid(64, 48, RED)).await;
    let _ = rig.stop.take().unwrap().send(());
    assert!(rig.frame().await == solid(CANVAS, RED));
    assert_eq!(rig.next().await, Sunk::Finished);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_ticker_that_runs_out_finishes_the_recording() {
    let mut rig = Rig::start(Screen::solid(64, 48, RED)).await;
    rig.tick();
    rig.frame().await;
    rig.ticks = None;
    assert_eq!(rig.next().await, Sunk::Finished);
}
