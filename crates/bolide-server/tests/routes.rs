//! Every route, against a recording fake [`Session`]. No RFB anywhere.
//!
//! The fake is the point: `bolide_rfb::Session` is a trait precisely so the HTTP layer
//! can be asserted without a socket, a handshake or a clock. What a click *becomes* is
//! read straight off the fake's log, and a timeout is a value the fake returns rather
//! than time a test has to spend.
//!
//! These go through [`bolide_server::serve`] on `127.0.0.1:0`, so the bound address, the
//! status codes and the headers are the real ones a client sees.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use bolide_rfb::{Error, Framebuffer, ServerEvent, Session};
use bolide_server::wire::{
    ClipboardResponse, ComputerAction, ComputerResponse, ErrorCode, ErrorResponse, ScrollDirection,
    StatusResponse,
};
use bolide_server::{
    ActionLog, Clock, LoggedRequest, PointerMode, Running, ServerConfig, TypingMode,
};
use tokio::sync::broadcast;

// ---------------------------------------------------------------- the fake session

/// Everything the fake was asked to do, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Call {
    Pointer { x: u16, y: u16, buttons: u8 },
    Key { keysym: u32, down: bool },
    CutText(String),
    Refresh { incremental: bool },
    Framebuffer,
}

/// What `refresh` should do. A fake that *returns* a timeout is why no test sleeps.
#[derive(Clone, Copy, Debug)]
enum Refresh {
    /// Paint the framebuffer and hand it back.
    Paints,
    /// The desktop produced nothing in time.
    TimesOut,
    /// Something else went wrong, carrying detail that must never reach a body.
    Fails,
}

/// The detail an injected failure carries. If this string turns up in an HTTP body,
/// bolide is leaking its own innards to a caller.
const INJECTED_DETAIL: &str = "rfb-internals-tile-27-of-zlib-stream";

struct Fake {
    width: u16,
    height: u16,
    desktop_name: String,
    painted: AtomicBool,
    connected: AtomicBool,
    refresh: Refresh,
    fb: Mutex<Framebuffer>,
    log: Mutex<Vec<Call>>,
    /// The `timeout` of every `refresh`, in order: how long a screenshot was willing to
    /// wait for the desktop.
    waits: Mutex<Vec<Duration>>,
    last_cut: Mutex<Option<String>>,
    events: broadcast::Sender<ServerEvent>,
    /// Like a real session, an event is only queued when `pointer` or `key` returns; a
    /// `flush` is when everything logged so far has gone out. Each flush, as how many
    /// calls it covered and when, on `clock` if one was given.
    flushes: Mutex<Vec<(usize, Option<std::time::Instant>)>>,
    clock: Option<Arc<dyn Clock>>,
}

impl Fake {
    fn new(width: u16, height: u16) -> Arc<Fake> {
        let mut fb = Framebuffer::black(width, height);
        // A recognisable, non-black screen so a decoded PNG proves it came from here.
        for (i, px) in fb.rgba.chunks_exact_mut(4).enumerate() {
            px[0] = (i % 251) as u8;
            px[1] = 0x40;
            px[2] = 0xc0;
            px[3] = 255;
        }
        Arc::new(Fake {
            width,
            height,
            desktop_name: "fake-desktop".to_string(),
            painted: AtomicBool::new(true),
            connected: AtomicBool::new(true),
            refresh: Refresh::Paints,
            fb: Mutex::new(fb),
            log: Mutex::new(Vec::new()),
            waits: Mutex::new(Vec::new()),
            last_cut: Mutex::new(None),
            events: broadcast::channel(16).0,
            flushes: Mutex::new(Vec::new()),
            clock: None,
        })
    }

    /// A fake that stamps each flush with `clock`'s time: the server's, in a test of when
    /// an answer comes.
    fn on_clock(mut self: Arc<Fake>, clock: Arc<dyn Clock>) -> Arc<Fake> {
        Arc::get_mut(&mut self).expect("sole owner").clock = Some(clock);
        self
    }

    /// The last flush: how many calls had gone out by then, and when.
    fn last_flush(&self) -> Option<(usize, Option<std::time::Instant>)> {
        self.flushes.lock().unwrap().last().copied()
    }

    /// An unpainted fake: the framebuffer is allocated but the desktop has not drawn.
    fn unpainted(width: u16, height: u16) -> Arc<Fake> {
        let fake = Fake::new(width, height);
        fake.painted.store(false, Ordering::SeqCst);
        fake
    }

    fn with_refresh(mut self: Arc<Fake>, refresh: Refresh) -> Arc<Fake> {
        Arc::get_mut(&mut self).expect("sole owner").refresh = refresh;
        self
    }

    fn disconnected(self: Arc<Fake>) -> Arc<Fake> {
        self.connected.store(false, Ordering::SeqCst);
        self
    }

    fn set_last_cut(self: Arc<Fake>, text: &str) -> Arc<Fake> {
        *self.last_cut.lock().unwrap() = Some(text.to_string());
        self
    }

    fn calls(&self) -> Vec<Call> {
        self.log.lock().unwrap().clone()
    }

    fn alive(&self) -> bolide_rfb::Result<()> {
        if self.connected.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(Error::Closed)
        }
    }
}

#[async_trait::async_trait]
impl Session for Fake {
    fn size(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    fn desktop_name(&self) -> String {
        self.desktop_name.clone()
    }

    fn painted(&self) -> bool {
        self.painted.load(Ordering::SeqCst)
    }

    async fn framebuffer(&self) -> bolide_rfb::Result<Framebuffer> {
        self.log.lock().unwrap().push(Call::Framebuffer);
        self.alive()?;
        Ok(self.fb.lock().unwrap().clone())
    }

    async fn refresh(
        &self,
        incremental: bool,
        timeout: Duration,
    ) -> bolide_rfb::Result<Framebuffer> {
        self.log.lock().unwrap().push(Call::Refresh { incremental });
        self.waits.lock().unwrap().push(timeout);
        self.alive()?;
        match self.refresh {
            Refresh::Paints => {
                self.painted.store(true, Ordering::SeqCst);
                Ok(self.fb.lock().unwrap().clone())
            }
            Refresh::TimesOut => Err(Error::Timeout(Duration::from_millis(1))),
            Refresh::Fails => Err(Error::Protocol(INJECTED_DETAIL.to_string())),
        }
    }

    async fn pointer(&self, x: u16, y: u16, buttons: u8) -> bolide_rfb::Result<()> {
        self.log
            .lock()
            .unwrap()
            .push(Call::Pointer { x, y, buttons });
        tokio::task::yield_now().await;
        self.alive()
    }

    async fn key(&self, keysym: u32, down: bool) -> bolide_rfb::Result<()> {
        self.log.lock().unwrap().push(Call::Key { keysym, down });
        // A real session awaits here — the command goes down a channel to the socket
        // task. Yielding makes that await point real for the scheduler, which is what
        // lets `two_actions_never_braid_their_events_together` observe a braid if the
        // server ever stops holding the desktop for the length of an action.
        tokio::task::yield_now().await;
        self.alive()
    }

    async fn flush(&self) -> bolide_rfb::Result<()> {
        self.alive()?;
        let covered = self.log.lock().unwrap().len();
        let at = self.clock.as_ref().map(|c| c.now());
        self.flushes.lock().unwrap().push((covered, at));
        Ok(())
    }

    async fn cut_text(&self, text: &str) -> bolide_rfb::Result<()> {
        self.log
            .lock()
            .unwrap()
            .push(Call::CutText(text.to_string()));
        self.alive()
    }

    fn last_cut_text(&self) -> Option<String> {
        self.last_cut.lock().unwrap().clone()
    }

    fn events(&self) -> broadcast::Receiver<ServerEvent> {
        self.events.subscribe()
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    async fn close(&self) -> bolide_rfb::Result<()> {
        self.connected.store(false, Ordering::SeqCst);
        Ok(())
    }
}

// ---------------------------------------------------------------------- the harness

struct Harness {
    running: Running,
    base: String,
    session: Arc<Fake>,
    client: reqwest::Client,
}

impl Harness {
    async fn start(session: Arc<Fake>, config: ServerConfig) -> Harness {
        let running = bolide_server::serve(session.clone(), config)
            .await
            .expect("bound");
        let base = format!("http://{}", running.addr);
        Harness {
            running,
            base,
            session,
            client: reqwest::Client::new(),
        }
    }

    async fn open(session: Arc<Fake>) -> Harness {
        Harness::start(
            session,
            ServerConfig {
                remote: "fake01.local:5900".into(),
                frame_timeout: Duration::from_millis(50),
                clock: VirtualClock::new(),
                ..Default::default()
            },
        )
        .await
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    async fn post(&self, path: &str, body: &serde_json::Value) -> reqwest::Response {
        self.client
            .post(self.url(path))
            .json(body)
            .send()
            .await
            .expect("request")
    }

    async fn action(&self, action: &ComputerAction) -> reqwest::Response {
        self.post(
            "/computer",
            &serde_json::to_value(action).expect("serialized"),
        )
        .await
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        self.client
            .get(self.url(path))
            .send()
            .await
            .expect("request")
    }

    async fn shutdown(self) {
        self.running.shutdown().await;
    }
}

/// Read an error body, asserting the status and code together — branching on `error`
/// rather than on the prose is what the protocol asks clients to do.
async fn expect_error(response: reqwest::Response, status: u16, code: ErrorCode) -> ErrorResponse {
    let got = response.status().as_u16();
    let body: serde_json::Value = response.json().await.expect("a JSON error body");
    let parsed: ErrorResponse = serde_json::from_value(body.clone())
        .unwrap_or_else(|e| panic!("body {body} is not an ErrorResponse: {e}"));
    assert_eq!(got, status, "status for {body}");
    assert_eq!(parsed.error, code, "code in {body}");
    assert!(!parsed.ok, "an error body says ok: {body}");
    assert!(!parsed.message.is_empty(), "an error with nothing to read");
    parsed
}

async fn expect_ok(response: reqwest::Response) -> ComputerResponse {
    assert_eq!(response.status().as_u16(), 200, "expected a 200");
    response.json().await.expect("a ComputerResponse")
}

fn png_dimensions(bytes: &[u8]) -> (u32, u32) {
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let reader = decoder.read_info().expect("a decodable PNG");
    let info = reader.info();
    (info.width, info.height)
}

// ------------------------------------------------------------------------- the tests

#[tokio::test]
async fn serve_returns_the_loopback_address_that_answers() {
    let h = Harness::open(Fake::new(800, 600)).await;
    assert!(h.running.addr.ip().is_loopback(), "bound off loopback");
    assert_ne!(
        h.running.addr.port(),
        0,
        "a real port, not the :0 we asked for"
    );
    let response = h.get("/healthz").await;
    assert_eq!(response.status().as_u16(), 200);
    h.shutdown().await;
}

#[tokio::test]
async fn a_click_becomes_move_press_release_on_the_session() {
    let h = Harness::open(Fake::new(800, 600)).await;
    let body = expect_ok(
        h.action(&ComputerAction::LeftClick {
            coordinate: [42, 17],
        })
        .await,
    )
    .await;
    assert!(body.ok);
    assert_eq!(
        h.session.calls(),
        vec![
            Call::Pointer {
                x: 42,
                y: 17,
                buttons: 0
            },
            Call::Pointer {
                x: 42,
                y: 17,
                buttons: bolide_rfb::proto::BUTTON_LEFT
            },
            Call::Pointer {
                x: 42,
                y: 17,
                buttons: 0
            },
        ]
    );
    h.shutdown().await;
}

#[tokio::test]
async fn a_scroll_reaches_the_session_as_wheel_notches() {
    let h = Harness::open(Fake::new(800, 600)).await;
    expect_ok(
        h.action(&ComputerAction::Scroll {
            coordinate: [10, 10],
            scroll_direction: ScrollDirection::Down,
            scroll_amount: 2,
        })
        .await,
    )
    .await;
    assert_eq!(h.session.calls().len(), 5, "a move plus two notches");
    h.shutdown().await;
}

/// RFB cannot be asked where the cursor is, so this is bolide's own last PointerEvent —
/// and `[0, 0]` before anything has moved it.
#[tokio::test]
async fn cursor_position_starts_at_the_origin_and_follows_the_pointer() {
    let h = Harness::open(Fake::new(800, 600)).await;

    let before = expect_ok(h.action(&ComputerAction::CursorPosition).await).await;
    assert_eq!(before.coordinate, Some([0, 0]));

    expect_ok(
        h.action(&ComputerAction::MouseMove {
            coordinate: [420, 180],
        })
        .await,
    )
    .await;

    let after = expect_ok(h.action(&ComputerAction::CursorPosition).await).await;
    assert_eq!(after.coordinate, Some([420, 180]));
    h.shutdown().await;
}

#[tokio::test]
async fn a_screenshot_action_returns_a_base64_png_of_the_right_size() {
    let h = Harness::open(Fake::new(64, 32)).await;
    let body = expect_ok(h.action(&ComputerAction::Screenshot).await).await;
    let image = body.image.expect("a screenshot carries an image");
    assert_eq!(image.format, "png");
    assert_eq!((image.width, image.height), (64, 32));
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&image.data)
        .expect("standard base64");
    assert_eq!(png_dimensions(&bytes), (64, 32));
    h.shutdown().await;
}

#[tokio::test]
async fn get_screenshot_returns_png_bytes_and_json_on_request() {
    let h = Harness::open(Fake::new(48, 24)).await;

    let raw = h.get("/screenshot").await;
    assert_eq!(raw.status().as_u16(), 200);
    assert_eq!(
        raw.headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("image/png")
    );
    let bytes = raw.bytes().await.expect("bytes");
    assert_eq!(png_dimensions(&bytes), (48, 24));

    let json = h.get("/screenshot?format=json").await;
    assert_eq!(json.status().as_u16(), 200);
    let image: bolide_server::wire::Image = json.json().await.expect("an Image");
    assert_eq!((image.width, image.height), (48, 24));
    assert!(!image.data.is_empty());

    h.shutdown().await;
}

/// An unpainted framebuffer is black at exactly screen dimensions — indistinguishable
/// from a real black screen — so bolide asks for the *whole* screen and waits.
#[tokio::test]
async fn an_unpainted_session_is_asked_for_a_full_update() {
    let h = Harness::open(Fake::unpainted(32, 16)).await;
    expect_ok(h.action(&ComputerAction::Screenshot).await).await;
    assert_eq!(
        h.session.calls().first(),
        Some(&Call::Refresh { incremental: false }),
        "an unpainted screen must be refreshed non-incrementally"
    );
    h.shutdown().await;
}

#[tokio::test]
async fn a_painted_session_is_refreshed_incrementally() {
    let h = Harness::open(Fake::new(32, 16)).await;
    expect_ok(h.action(&ComputerAction::Screenshot).await).await;
    assert_eq!(
        h.session.calls().first(),
        Some(&Call::Refresh { incremental: true })
    );
    h.shutdown().await;
}

/// A black image that looks like a real screen is the failure this rule exists to
/// prevent: a model handed one reports the app as blank.
#[tokio::test]
async fn a_screen_that_never_paints_is_a_timeout_not_a_black_image() {
    let h = Harness::open(Fake::unpainted(32, 16).with_refresh(Refresh::TimesOut)).await;
    expect_error(h.get("/screenshot").await, 504, ErrorCode::Timeout).await;
    expect_error(
        h.action(&ComputerAction::Screenshot).await,
        504,
        ErrorCode::Timeout,
    )
    .await;
    h.shutdown().await;
}

/// A still screen produces no incremental update, and that is not an error: the frame
/// we already have is the truth.
#[tokio::test]
async fn a_painted_screen_that_times_out_answers_with_the_frame_it_has() {
    let h = Harness::open(Fake::new(32, 16).with_refresh(Refresh::TimesOut)).await;
    let body = expect_ok(h.action(&ComputerAction::Screenshot).await).await;
    let image = body.image.expect("an image");
    assert_eq!((image.width, image.height), (32, 16));
    assert!(
        h.session.calls().contains(&Call::Framebuffer),
        "it should fall back to the frame it has: {:?}",
        h.session.calls()
    );
    h.shutdown().await;
}

/// A desktop answers an input a few hundred milliseconds later — Apple's Screen Sharing
/// batches its updates on about that period — so the short wait that suits a still
/// screen hands back the frame from *before* the click. A screenshot that follows input
/// waits for the update that follows it; one that follows nothing keeps the short wait.
#[tokio::test]
async fn a_screenshot_after_input_waits_longer_for_the_update_that_follows_it() {
    let h = Harness::start(
        Fake::new(32, 16),
        ServerConfig {
            frame_timeout: Duration::from_secs(5),
            ..Default::default()
        },
    )
    .await;
    let waits = || h.session.waits.lock().unwrap().clone();

    expect_ok(h.action(&ComputerAction::Screenshot).await).await;
    expect_ok(
        h.action(&ComputerAction::LeftClick { coordinate: [3, 4] })
            .await,
    )
    .await;
    expect_ok(h.action(&ComputerAction::Screenshot).await).await;
    // Nothing happened since that one.
    let _ = h.get("/screenshot").await;
    expect_ok(
        h.action(&ComputerAction::Key {
            text: "Return".into(),
        })
        .await,
    )
    .await;
    let _ = h.get("/screenshot").await;

    let short = Duration::from_millis(250);
    let long = Duration::from_secs(2);
    assert_eq!(waits(), vec![short, long, short, long]);
    h.shutdown().await;
}

/// A clock that never waits: it records what it was asked for and moves its own time
/// forward by that much, so "has the desktop had long enough" is a value, not a race.
#[derive(Debug)]
struct VirtualClock {
    start: std::time::Instant,
    slept: Mutex<Vec<Duration>>,
}

impl VirtualClock {
    fn new() -> Arc<VirtualClock> {
        Arc::new(VirtualClock {
            start: std::time::Instant::now(),
            slept: Mutex::new(Vec::new()),
        })
    }

    fn slept(&self) -> Vec<Duration> {
        self.slept.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl Clock for VirtualClock {
    async fn sleep(&self, duration: Duration) {
        self.slept.lock().unwrap().push(duration);
    }

    fn now(&self) -> std::time::Instant {
        self.start + self.slept.lock().unwrap().iter().sum::<Duration>()
    }
}

/// Apple's Screen Sharing streams small updates many times a second, so "the next
/// update after the request" can arrive before the app has redrawn: Spotlight takes
/// about 300 ms to paint after `cmd+space`. A screenshot that follows input therefore
/// lets `input_settle` pass since the input first — only what is left of it, and not at
/// all when the caller already waited.
#[tokio::test]
async fn a_screenshot_right_after_input_lets_the_desktop_settle_first() {
    let clock = VirtualClock::new();
    let h = Harness::start(
        Fake::new(32, 16),
        ServerConfig {
            clock: clock.clone(),
            ..Default::default()
        },
    )
    .await;
    let settle = ServerConfig::default().input_settle;
    assert_eq!(settle, Duration::from_millis(500));

    expect_ok(h.action(&ComputerAction::Screenshot).await).await;
    assert_eq!(clock.slept(), vec![], "no input, no settling");

    expect_ok(
        h.action(&ComputerAction::LeftClick { coordinate: [3, 4] })
            .await,
    )
    .await;
    expect_ok(h.action(&ComputerAction::Screenshot).await).await;
    assert_eq!(clock.slept(), vec![settle], "settled after the click");
    let refreshes = h
        .session
        .calls()
        .into_iter()
        .filter(|c| matches!(c, Call::Refresh { .. }))
        .count();
    assert_eq!(refreshes, 2, "and then asked the desktop for a frame");

    let _ = h.get("/screenshot").await;
    assert_eq!(
        clock.slept(),
        vec![settle],
        "nothing new since, nothing to wait for"
    );

    // A caller that already waited long enough is not made to wait again.
    expect_ok(
        h.action(&ComputerAction::Key {
            text: "Return".into(),
        })
        .await,
    )
    .await;
    expect_ok(h.action(&ComputerAction::Wait { duration: 1.0 }).await).await;
    let _ = h.get("/screenshot").await;
    assert_eq!(clock.slept(), vec![settle, Duration::from_secs(1)]);

    // Part of the way there: only the rest.
    expect_ok(
        h.action(&ComputerAction::Key {
            text: "Return".into(),
        })
        .await,
    )
    .await;
    expect_ok(h.action(&ComputerAction::Wait { duration: 0.2 }).await).await;
    let _ = h.get("/screenshot").await;
    assert_eq!(
        clock.slept(),
        vec![
            settle,
            Duration::from_secs(1),
            Duration::from_millis(200),
            Duration::from_millis(300),
        ]
    );
    h.shutdown().await;
}

/// Zero is a choice the caller may make: the screenshot then goes straight to the
/// desktop, as it did before settling existed.
#[tokio::test]
async fn an_input_settle_of_zero_waits_for_nothing() {
    let clock = VirtualClock::new();
    let h = Harness::start(
        Fake::new(32, 16),
        ServerConfig {
            clock: clock.clone(),
            input_settle: Duration::ZERO,
            ..Default::default()
        },
    )
    .await;
    expect_ok(
        h.action(&ComputerAction::LeftClick { coordinate: [3, 4] })
            .await,
    )
    .await;
    expect_ok(h.action(&ComputerAction::Screenshot).await).await;
    assert_eq!(clock.slept(), vec![]);
    h.shutdown().await;
}

/// The longer wait is still capped by `frame_timeout`, which is the caller's own bound.
#[tokio::test]
async fn the_wait_after_input_never_exceeds_the_frame_timeout() {
    let h = Harness::open(Fake::new(32, 16)).await;
    expect_ok(
        h.action(&ComputerAction::LeftClick { coordinate: [3, 4] })
            .await,
    )
    .await;
    expect_ok(h.action(&ComputerAction::Screenshot).await).await;
    assert_eq!(
        h.session.waits.lock().unwrap().clone(),
        vec![Duration::from_millis(50)]
    );
    h.shutdown().await;
}

#[tokio::test]
async fn the_clipboard_is_null_until_the_desktop_volunteers_one() {
    let h = Harness::open(Fake::new(8, 8)).await;
    let body: ClipboardResponse = h.get("/clipboard").await.json().await.expect("a body");
    assert_eq!(body.text, None);
    h.shutdown().await;
}

#[tokio::test]
async fn the_clipboard_reports_the_last_server_cut_text() {
    let h = Harness::open(Fake::new(8, 8).set_last_cut("copied on the desktop")).await;
    let body: ClipboardResponse = h.get("/clipboard").await.json().await.expect("a body");
    assert_eq!(body.text.as_deref(), Some("copied on the desktop"));
    h.shutdown().await;
}

#[tokio::test]
async fn posting_the_clipboard_reaches_the_session_as_cut_text() {
    let h = Harness::open(Fake::new(8, 8)).await;
    let response = h
        .post(
            "/clipboard",
            &serde_json::json!({"text": "pasted by bolide"}),
        )
        .await;
    assert_eq!(response.status().as_u16(), 200);
    let body: serde_json::Value = response.json().await.expect("a body");
    assert_eq!(body, serde_json::json!({"ok": true}));
    assert_eq!(
        h.session.calls(),
        vec![Call::CutText("pasted by bolide".into())]
    );
    h.shutdown().await;
}

#[tokio::test]
async fn a_clipboard_bigger_than_a_framework_default_limit_is_accepted() {
    // axum caps a body at 2 MiB unless told otherwise; bolide does not cap what a
    // caller may paste.
    let h = Harness::open(Fake::new(8, 8)).await;
    let text = "x".repeat(3 * 1024 * 1024);
    let response = h
        .post("/clipboard", &serde_json::json!({ "text": text }))
        .await;
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(h.session.calls(), vec![Call::CutText(text)]);
    h.shutdown().await;
}

#[tokio::test]
async fn status_reports_the_session_without_credentials() {
    let h = Harness::open(Fake::new(1728, 1117)).await;
    let body: StatusResponse = h.get("/status").await.json().await.expect("a body");
    assert!(body.connected);
    assert_eq!(body.remote, "fake01.local:5900");
    assert_eq!(body.desktop_name, "fake-desktop");
    assert_eq!((body.width, body.height), (1728, 1117));
    assert!(body.painted);
    assert_eq!(body.version, env!("CARGO_PKG_VERSION"));
    h.shutdown().await;
}

#[tokio::test]
async fn an_off_screen_click_is_four_hundred_out_of_bounds() {
    let h = Harness::open(Fake::new(800, 600)).await;
    let body = expect_error(
        h.action(&ComputerAction::LeftClick {
            coordinate: [9999, 10],
        })
        .await,
        400,
        ErrorCode::OutOfBounds,
    )
    .await;
    assert!(
        body.message.contains("9999"),
        "the message should name the coordinate: {}",
        body.message
    );
    assert!(
        h.session.calls().is_empty(),
        "nothing should have been sent"
    );
    h.shutdown().await;
}

#[tokio::test]
async fn a_body_that_does_not_parse_is_four_hundred_bad_request() {
    let h = Harness::open(Fake::new(800, 600)).await;

    let garbage = h
        .client
        .post(h.url("/computer"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body("{not json at all")
        .send()
        .await
        .expect("request");
    expect_error(garbage, 400, ErrorCode::BadRequest).await;

    let unknown = h
        .post(
            "/computer",
            &serde_json::json!({"action": "hold_key", "text": "a"}),
        )
        .await;
    expect_error(unknown, 400, ErrorCode::BadRequest).await;

    let missing_field = h
        .post("/computer", &serde_json::json!({"action": "left_click"}))
        .await;
    expect_error(missing_field, 400, ErrorCode::BadRequest).await;

    let bad_clipboard = h.post("/clipboard", &serde_json::json!({"nope": 1})).await;
    expect_error(bad_clipboard, 400, ErrorCode::BadRequest).await;

    h.shutdown().await;
}

#[tokio::test]
async fn an_unknown_key_name_is_four_hundred_unknown_key() {
    let h = Harness::open(Fake::new(800, 600)).await;
    expect_error(
        h.action(&ComputerAction::Key {
            text: "nosuchkeyatall".into(),
        })
        .await,
        400,
        ErrorCode::UnknownKey,
    )
    .await;
    assert!(
        h.session.calls().is_empty(),
        "no keypress should have been sent"
    );
    h.shutdown().await;
}

#[tokio::test]
async fn a_chord_reaches_the_session_pressed_in_order_and_released_in_reverse() {
    let h = Harness::open(Fake::new(800, 600)).await;
    expect_ok(
        h.action(&ComputerAction::Key {
            text: "ctrl+c".into(),
        })
        .await,
    )
    .await;
    let syms = bolide_rfb::keysym::parse_chord("ctrl+c").expect("a known chord");
    assert_eq!(
        h.session.calls(),
        vec![
            Call::Key {
                keysym: syms[0],
                down: true
            },
            Call::Key {
                keysym: syms[1],
                down: true
            },
            Call::Key {
                keysym: syms[1],
                down: false
            },
            Call::Key {
                keysym: syms[0],
                down: false
            },
        ]
    );
    h.shutdown().await;
}

#[tokio::test]
async fn a_dead_session_is_five_hundred_and_three_disconnected() {
    let h = Harness::open(Fake::new(800, 600).disconnected()).await;
    expect_error(
        h.action(&ComputerAction::LeftClick { coordinate: [1, 1] })
            .await,
        503,
        ErrorCode::Disconnected,
    )
    .await;
    expect_error(h.get("/screenshot").await, 503, ErrorCode::Disconnected).await;
    expect_error(
        h.post("/clipboard", &serde_json::json!({"text": "x"}))
            .await,
        503,
        ErrorCode::Disconnected,
    )
    .await;
    h.shutdown().await;
}

/// `/status` must answer even when the desktop is gone — that is the question it is
/// there to answer.
#[tokio::test]
async fn status_still_answers_when_the_session_is_gone() {
    let h = Harness::open(Fake::new(8, 8).disconnected()).await;
    let response = h.get("/status").await;
    assert_eq!(response.status().as_u16(), 200);
    let body: StatusResponse = response.json().await.expect("a body");
    assert!(!body.connected);
    h.shutdown().await;
}

#[tokio::test]
async fn an_rfb_failure_is_five_hundred_and_leaks_nothing() {
    let h = Harness::open(Fake::new(32, 16).with_refresh(Refresh::Fails)).await;
    let response = h.get("/screenshot").await;
    assert_eq!(response.status().as_u16(), 500);
    let raw = response.text().await.expect("a body");
    assert!(
        !raw.contains(INJECTED_DETAIL),
        "an internal error leaked into the body: {raw}"
    );
    assert!(!raw.contains("Protocol"), "a Debug-shaped leak: {raw}");
    let parsed: ErrorResponse = serde_json::from_str(&raw).expect("an ErrorResponse");
    assert_eq!(parsed.error, ErrorCode::Internal);
    h.shutdown().await;
}

// ------------------------------------------------------------------------ the token

async fn tokened() -> Harness {
    Harness::start(
        Fake::new(64, 64),
        ServerConfig {
            token: Some("s3cret-token".into()),
            remote: "fake01.local:5900".into(),
            frame_timeout: Duration::from_millis(50),
            ..Default::default()
        },
    )
    .await
}

#[tokio::test]
async fn a_correct_bearer_token_is_accepted() {
    let h = tokened().await;
    let response = h
        .client
        .get(h.url("/status"))
        .bearer_auth("s3cret-token")
        .send()
        .await
        .expect("request");
    assert_eq!(response.status().as_u16(), 200);
    h.shutdown().await;
}

#[tokio::test]
async fn a_missing_or_wrong_token_is_four_hundred_and_one() {
    let h = tokened().await;

    expect_error(h.get("/status").await, 401, ErrorCode::Unauthorized).await;

    let wrong = h
        .client
        .get(h.url("/status"))
        .bearer_auth("s3cret-tokeX")
        .send()
        .await
        .expect("request");
    expect_error(wrong, 401, ErrorCode::Unauthorized).await;

    // A prefix of the real token is not the real token.
    let truncated = h
        .client
        .get(h.url("/status"))
        .bearer_auth("s3cret")
        .send()
        .await
        .expect("request");
    expect_error(truncated, 401, ErrorCode::Unauthorized).await;

    // And the gate covers every route, not just the one above.
    expect_error(
        h.action(&ComputerAction::LeftClick { coordinate: [1, 1] })
            .await,
        401,
        ErrorCode::Unauthorized,
    )
    .await;
    expect_error(h.get("/screenshot").await, 401, ErrorCode::Unauthorized).await;
    expect_error(h.get("/clipboard").await, 401, ErrorCode::Unauthorized).await;
    expect_error(
        h.post("/clipboard", &serde_json::json!({"text": "x"}))
            .await,
        401,
        ErrorCode::Unauthorized,
    )
    .await;

    assert!(
        h.session.calls().is_empty(),
        "an unauthorized request reached the desktop: {:?}",
        h.session.calls()
    );
    h.shutdown().await;
}

/// Exempt so a supervisor can probe without holding a token that grants full control
/// of somebody's desktop.
#[tokio::test]
async fn healthz_needs_no_token() {
    let h = tokened().await;
    let response = h.get("/healthz").await;
    assert_eq!(response.status().as_u16(), 200);
    h.shutdown().await;
}

// ------------------------------------------------------- one desktop, one pair of hands

/// Two actions in flight at once must not braid their events together.
///
/// A desktop has ONE pointer and ONE keyboard. If the server lets two `/computer`
/// requests run concurrently, a `type "aaaa"` racing a `type "bbbb"` reaches the
/// desktop as `abab…` — text neither caller asked for, typed into whatever had focus.
/// The same hazard is worse for a drag: a press from one action and a release from
/// another are a drag that never ends.
///
/// So the fix is not "make typing atomic", it is "an action holds the desktop for its
/// whole length", and this test is written at the level that says so: two real HTTP
/// requests, concurrently, against a fake whose every key await yields to the scheduler.
#[tokio::test(flavor = "current_thread")]
async fn two_actions_never_braid_their_events_together() {
    let fake = Fake::new(800, 600);
    let h = Harness::open(Arc::clone(&fake)).await;

    let type_a = ComputerAction::Type {
        text: "aaaa".into(),
    };
    let type_b = ComputerAction::Type {
        text: "bbbb".into(),
    };
    let a = h.action(&type_a);
    let b = h.action(&type_b);
    let (ra, rb) = tokio::join!(a, b);
    assert_eq!(ra.status(), 200);
    assert_eq!(rb.status(), 200);

    let a_sym = bolide_rfb::keysym::keysym_for_char('a');
    let b_sym = bolide_rfb::keysym::keysym_for_char('b');
    let letters: Vec<u32> = fake
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            Call::Key { keysym, down: true } => Some(keysym),
            _ => None,
        })
        .collect();
    assert_eq!(letters.len(), 8, "eight characters were typed: {letters:?}");

    // Whichever action won, its four letters are contiguous.
    let first = letters[0];
    assert!(
        first == a_sym || first == b_sym,
        "unexpected keysym {first}"
    );
    let second = if first == a_sym { b_sym } else { a_sym };
    assert_eq!(
        letters,
        vec![first, first, first, first, second, second, second, second],
        "the two actions braided: {letters:?}"
    );

    h.shutdown().await;
}

// ------------------------------------------------------------- --pointer humanized

/// A harness whose pointer moves like a hand, from `seed`, on a clock that never waits.
async fn humanized(seed: u32) -> Harness {
    Harness::start(
        Fake::new(1280, 800),
        ServerConfig {
            pointer: PointerMode::Humanized,
            seed,
            frame_timeout: Duration::from_millis(50),
            clock: VirtualClock::new(),
            ..Default::default()
        },
    )
    .await
}

fn pointer_calls(calls: &[Call]) -> Vec<(u16, u16, u8)> {
    calls
        .iter()
        .filter_map(|c| match *c {
            Call::Pointer { x, y, buttons } => Some((x, y, buttons)),
            _ => None,
        })
        .collect()
}

/// A few acts a person makes, as the session hears them.
async fn a_few_pointer_acts(h: &Harness) -> Vec<Call> {
    for act in [
        ComputerAction::MouseMove {
            coordinate: [100, 100],
        },
        ComputerAction::LeftClick {
            coordinate: [900, 600],
        },
        ComputerAction::LeftClickDrag {
            start_coordinate: [900, 600],
            coordinate: [300, 700],
        },
        ComputerAction::Scroll {
            coordinate: [640, 400],
            scroll_direction: ScrollDirection::Down,
            scroll_amount: 3,
        },
    ] {
        expect_ok(h.action(&act).await).await;
    }
    h.session.calls()
}

/// The direct default is unchanged: `ServerConfig::default()` teleports, as it always has.
#[test]
fn the_pointer_is_direct_unless_asked_otherwise() {
    assert_eq!(ServerConfig::default().pointer, PointerMode::Direct);
}

/// The pointer starts from where bolide last left it: across four acts, no step from one
/// PointerEvent to the next is a jump, and the first leaves from the screen's centre.
#[tokio::test]
async fn a_humanized_pointer_starts_each_act_from_where_the_last_one_left_it() {
    let h = humanized(1).await;
    let pointers = pointer_calls(&a_few_pointer_acts(&h).await);
    let mut prev = (640i32, 400i32);
    for (i, &(x, y, _)) in pointers.iter().enumerate() {
        let (x, y) = (i32::from(x), i32::from(y));
        let step = f64::from(x - prev.0).hypot(f64::from(y - prev.1));
        assert!(step < 80.0, "event {i} jumped {step} px to ({x}, {y})");
        prev = (x, y);
    }
    assert!(pointers.len() > 40, "only {} PointerEvents", pointers.len());
    h.shutdown().await;
}

#[tokio::test]
async fn the_same_seed_sends_the_same_events_and_another_does_not() {
    let a = humanized(7).await;
    let b = humanized(7).await;
    let c = humanized(8).await;
    let (ca, cb, cc) = (
        a_few_pointer_acts(&a).await,
        a_few_pointer_acts(&b).await,
        a_few_pointer_acts(&c).await,
    );
    assert_eq!(ca, cb);
    assert_ne!(ca, cc);
    for h in [a, b, c] {
        h.shutdown().await;
    }
}

/// `cursor_position` is still bolide's own last PointerEvent: where the hand landed.
#[tokio::test]
async fn cursor_position_reports_where_the_humanized_pointer_landed() {
    let h = humanized(3).await;
    expect_ok(
        h.action(&ComputerAction::LeftClick {
            coordinate: [500, 300],
        })
        .await,
    )
    .await;
    let (x, y, _) = *pointer_calls(&h.session.calls()).last().expect("moved");
    let at = expect_ok(h.action(&ComputerAction::CursorPosition).await).await;
    assert_eq!(at.coordinate, Some([i32::from(x), i32::from(y)]));
    assert!((i32::from(x) - 500).abs() <= 1 && (i32::from(y) - 300).abs() <= 1);
    h.shutdown().await;
}

// ------------------------------------------------------------------- the action log

/// An [`ActionLog`] that keeps every line it is given.
#[derive(Debug, Default)]
struct MemoryLog(Mutex<Vec<LoggedRequest>>);

impl ActionLog for MemoryLog {
    fn record(&self, request: LoggedRequest) {
        self.0.lock().unwrap().push(request);
    }
}

impl MemoryLog {
    fn lines(&self) -> Vec<LoggedRequest> {
        self.0.lock().unwrap().clone()
    }

    /// Each line as `(action, params, error)`, the part a test can state exactly.
    fn said(&self) -> Vec<(String, serde_json::Value, Option<String>)> {
        self.lines()
            .into_iter()
            .map(|l| (l.action, serde_json::Value::Object(l.params), l.error))
            .collect()
    }
}

/// A server on `session` that logs to the returned [`MemoryLog`], on a virtual clock.
async fn logging(session: Arc<Fake>, token: Option<&str>) -> (Harness, Arc<MemoryLog>) {
    let log = Arc::new(MemoryLog::default());
    let h = Harness::start(
        session,
        ServerConfig {
            remote: "fake01.local:5900".into(),
            frame_timeout: Duration::from_millis(50),
            clock: VirtualClock::new(),
            token: token.map(String::from),
            action_log: Some(log.clone()),
            ..Default::default()
        },
    )
    .await;
    (h, log)
}

async fn post_raw(h: &Harness, path: &str, body: &str) -> reqwest::Response {
    h.client
        .post(h.url(path))
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .expect("request")
}

/// One line per request, named as the caller named it, with what was sent in full:
/// typed text included, every byte of it.
#[tokio::test]
async fn each_request_kind_is_logged_with_what_was_sent() {
    let (h, log) = logging(Fake::new(800, 600), None).await;
    let typed = "hello, wörld\n\tpassword: hunter2";
    expect_ok(
        h.action(&ComputerAction::LeftClick {
            coordinate: [42, 17],
        })
        .await,
    )
    .await;
    expect_ok(h.action(&ComputerAction::Type { text: typed.into() }).await).await;
    expect_ok(
        h.action(&ComputerAction::Key {
            text: "ctrl+c".into(),
        })
        .await,
    )
    .await;
    expect_ok(
        h.action(&ComputerAction::LeftClickDrag {
            start_coordinate: [1, 2],
            coordinate: [30, 40],
        })
        .await,
    )
    .await;
    assert_eq!(h.get("/screenshot").await.status().as_u16(), 200);
    assert_eq!(
        h.get("/screenshot?format=json").await.status().as_u16(),
        200
    );
    assert_eq!(h.get("/clipboard").await.status().as_u16(), 200);
    assert_eq!(
        h.post("/clipboard", &serde_json::json!({"text": "pasted"}))
            .await
            .status()
            .as_u16(),
        200
    );

    use serde_json::json;
    assert_eq!(
        log.said(),
        vec![
            ("left_click".into(), json!({"coordinate": [42, 17]}), None),
            ("type".into(), json!({ "text": typed }), None),
            ("key".into(), json!({"text": "ctrl+c"}), None),
            (
                "left_click_drag".into(),
                json!({"start_coordinate": [1, 2], "coordinate": [30, 40]}),
                None
            ),
            ("screenshot".into(), json!({}), None),
            ("screenshot".into(), json!({"format": "json"}), None),
            ("clipboard_get".into(), json!({}), None),
            ("clipboard_set".into(), json!({"text": "pasted"}), None),
        ]
    );
    h.shutdown().await;
}

/// A request that fails is logged with the message its caller was answered with, under
/// the name it asked for — even one the server could not parse.
#[tokio::test]
async fn a_failed_request_is_logged_with_its_error() {
    let (h, log) = logging(Fake::new(64, 48), None).await;
    let out_of_bounds = expect_error(
        h.action(&ComputerAction::LeftClick {
            coordinate: [9000, 1],
        })
        .await,
        400,
        ErrorCode::OutOfBounds,
    )
    .await;
    let unknown_key = expect_error(
        h.action(&ComputerAction::Key {
            text: "ctrl+nosuchkey".into(),
        })
        .await,
        400,
        ErrorCode::UnknownKey,
    )
    .await;
    let unknown_action = expect_error(
        post_raw(&h, "/computer", r#"{"action":"fly","to":"moon"}"#).await,
        400,
        ErrorCode::BadRequest,
    )
    .await;
    let not_json = expect_error(
        post_raw(&h, "/computer", "{not json").await,
        400,
        ErrorCode::BadRequest,
    )
    .await;
    let bad_clipboard = expect_error(
        post_raw(&h, "/clipboard", r#"{"txt":"x"}"#).await,
        400,
        ErrorCode::BadRequest,
    )
    .await;

    use serde_json::json;
    assert_eq!(
        log.said(),
        vec![
            (
                "left_click".into(),
                json!({"coordinate": [9000, 1]}),
                Some(out_of_bounds.message)
            ),
            (
                "key".into(),
                json!({"text": "ctrl+nosuchkey"}),
                Some(unknown_key.message)
            ),
            (
                "fly".into(),
                json!({"to": "moon"}),
                Some(unknown_action.message)
            ),
            (
                "computer".into(),
                json!({"body": "{not json"}),
                Some(not_json.message)
            ),
            (
                "clipboard_set".into(),
                json!({"txt": "x"}),
                Some(bad_clipboard.message)
            ),
        ]
    );
    h.shutdown().await;
}

/// The desktop going away is an error line too.
#[tokio::test]
async fn a_request_to_a_desktop_that_has_gone_is_logged_as_an_error() {
    let (h, log) = logging(Fake::new(64, 48).disconnected(), None).await;
    let gone = expect_error(
        h.post("/clipboard", &serde_json::json!({"text": "x"}))
            .await,
        503,
        ErrorCode::Disconnected,
    )
    .await;
    assert_eq!(
        log.said(),
        vec![(
            "clipboard_set".into(),
            serde_json::json!({"text": "x"}),
            Some(gone.message)
        )]
    );
    h.shutdown().await;
}

/// `/status` and `/healthz` are questions about bolide, not acts on the desktop.
#[tokio::test]
async fn status_and_healthz_are_not_logged() {
    let (h, log) = logging(Fake::new(64, 48), None).await;
    assert_eq!(h.get("/status").await.status().as_u16(), 200);
    assert_eq!(h.get("/healthz").await.status().as_u16(), 200);
    assert_eq!(h.get("/clipboard").await.status().as_u16(), 200);
    let actions: Vec<String> = log.lines().into_iter().map(|l| l.action).collect();
    assert_eq!(actions, vec!["clipboard_get"]);
    h.shutdown().await;
}

/// A request refused for its token never reached the desktop, so it is not a line.
#[tokio::test]
async fn a_request_refused_for_its_token_is_not_logged() {
    let (h, log) = logging(Fake::new(64, 48), Some("s3cret-token")).await;
    let refused = h.get("/clipboard").await;
    assert_eq!(refused.status().as_u16(), 401);
    assert!(log.lines().is_empty(), "{:?}", log.lines());
    let allowed = h
        .client
        .get(h.url("/clipboard"))
        .bearer_auth("s3cret-token")
        .send()
        .await
        .expect("request");
    assert_eq!(allowed.status().as_u16(), 200);
    assert_eq!(log.lines().len(), 1);
    h.shutdown().await;
}

/// A line's start and end are the server's clock when the request arrived and when it
/// was answered, so a `wait` spans exactly its wait, and one request after another reads
/// in order.
#[tokio::test]
async fn a_line_spans_the_time_its_request_took_on_the_server_s_clock() {
    let (h, log) = logging(Fake::new(64, 48), None).await;
    expect_ok(h.action(&ComputerAction::Wait { duration: 2.0 }).await).await;
    expect_ok(
        h.action(&ComputerAction::LeftClick {
            coordinate: [10, 10],
        })
        .await,
    )
    .await;
    expect_ok(h.action(&ComputerAction::Wait { duration: 0.5 }).await).await;
    let lines = log.lines();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0].end - lines[0].start, Duration::from_secs(2));
    assert_eq!(lines[2].end - lines[2].start, Duration::from_millis(500));
    for pair in lines.windows(2) {
        assert!(pair[0].start <= pair[0].end);
        assert!(pair[0].end <= pair[1].start, "{pair:?}");
    }
    h.shutdown().await;
}

/// A server whose action log and fake session share one virtual clock, so a flush's time
/// and a line's end can be compared.
async fn timed(config: ServerConfig) -> (Harness, Arc<MemoryLog>) {
    let clock = VirtualClock::new();
    let log = Arc::new(MemoryLog::default());
    let h = Harness::start(
        Fake::new(640, 480).on_clock(clock.clone()),
        ServerConfig {
            frame_timeout: Duration::from_millis(50),
            clock,
            action_log: Some(log.clone()),
            ..config
        },
    )
    .await;
    (h, log)
}

/// A line's end is true for input: by the time an act is answered, every event it sent
/// has gone out (the session's `flush`), and that was no later than the line's end. A
/// session queues an event when `key` returns; without the flush the answer, and the
/// log, would come before the keys reached the desktop.
async fn assert_answered_after_its_events_went_out(config: ServerConfig) {
    let (h, log) = timed(config).await;
    for act in [
        ComputerAction::Type {
            text: "Hi, Bob!\n".into(),
        },
        ComputerAction::Key {
            text: "ctrl+shift+t".into(),
        },
        ComputerAction::LeftClick {
            coordinate: [10, 20],
        },
    ] {
        expect_ok(h.action(&act).await).await;
        let sent = h.session.calls().len();
        let line = log.lines().pop().expect("logged");
        let (covered, at) = h
            .session
            .last_flush()
            .unwrap_or_else(|| panic!("{act:?} answered without a flush"));
        assert_eq!(
            covered, sent,
            "{act:?} answered before its last event went out"
        );
        let at = at.expect("stamped");
        assert!(line.start <= at && at <= line.end, "{act:?}: {line:?}");
    }
    h.shutdown().await;
}

#[tokio::test]
async fn a_direct_act_is_answered_only_once_its_events_have_gone_out() {
    assert_answered_after_its_events_went_out(ServerConfig::default()).await;
}

#[tokio::test]
async fn a_humanized_type_or_key_is_answered_only_once_its_last_key_has_gone_out() {
    assert_answered_after_its_events_went_out(ServerConfig {
        typing: TypingMode::Humanized,
        seed: 5,
        ..Default::default()
    })
    .await;
}

/// A humanized `type` takes the time a person takes, on the server's clock, and its line
/// spans it: from the first key to the last, every wait between them.
#[tokio::test]
async fn a_humanized_type_line_spans_the_time_its_keys_took() {
    let (h, log) = timed(ServerConfig {
        typing: TypingMode::Humanized,
        seed: 5,
        ..Default::default()
    })
    .await;
    expect_ok(
        h.action(&ComputerAction::Type {
            text: "Hello there".into(),
        })
        .await,
    )
    .await;
    let line = log.lines().pop().expect("logged");
    let took = line.end - line.start;
    assert!(
        took >= Duration::from_millis(10 * 60),
        "eleven keys in {took:?}"
    );
    h.shutdown().await;
}

#[test]
fn the_typing_is_direct_unless_asked_otherwise() {
    assert_eq!(ServerConfig::default().typing, TypingMode::Direct);
}

/// A harness that types like a person, and moves the pointer as `pointer` says.
async fn typist(seed: u32, pointer: PointerMode, typing: TypingMode) -> Harness {
    Harness::start(
        Fake::new(1280, 800),
        ServerConfig {
            pointer,
            typing,
            seed,
            frame_timeout: Duration::from_millis(50),
            clock: VirtualClock::new(),
            ..Default::default()
        },
    )
    .await
}

/// Pointer acts with typing and keys between them.
async fn a_few_mixed_acts(h: &Harness) -> Vec<Call> {
    for act in [
        ComputerAction::LeftClick {
            coordinate: [900, 600],
        },
        ComputerAction::Type {
            text: "Hi, Bob!".into(),
        },
        ComputerAction::Key {
            text: "ctrl+a".into(),
        },
        ComputerAction::MouseMove {
            coordinate: [100, 100],
        },
    ] {
        expect_ok(h.action(&act).await).await;
    }
    h.session.calls()
}

fn key_calls(calls: &[Call]) -> Vec<(u32, bool)> {
    calls
        .iter()
        .filter_map(|c| match *c {
            Call::Key { keysym, down } => Some((keysym, down)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn the_same_seed_types_the_same_and_another_does_not() {
    // The pointer is direct, so only typing draws: what differs by seed is the typing.
    let session = |seed| async move {
        let clock = VirtualClock::new();
        let h = Harness::start(
            Fake::new(1280, 800),
            ServerConfig {
                typing: TypingMode::Humanized,
                seed,
                clock: clock.clone(),
                ..Default::default()
            },
        )
        .await;
        let calls = a_few_mixed_acts(&h).await;
        h.shutdown().await;
        (calls, clock.slept())
    };
    let (a, b, c) = (session(7).await, session(7).await, session(8).await);
    assert!(!a.1.is_empty(), "typed with no waits");
    assert_eq!(a, b);
    assert_ne!(a.1, c.1);
}

/// Humanized typing sends the keys direct typing sends, `ctrl+a` included; only the
/// order of some ups (rollover) and the waits between them differ.
#[tokio::test]
async fn humanized_typing_sends_the_keys_direct_typing_sends() {
    let direct = typist(2, PointerMode::Direct, TypingMode::Direct).await;
    let human = typist(2, PointerMode::Direct, TypingMode::Humanized).await;
    let (mut d, mut h) = (
        key_calls(&a_few_mixed_acts(&direct).await),
        key_calls(&a_few_mixed_acts(&human).await),
    );
    let downs = |keys: &[(u32, bool)]| keys.iter().filter(|k| k.1).map(|k| k.0).collect::<Vec<_>>();
    assert_eq!(downs(&h), downs(&d));
    d.sort_unstable();
    h.sort_unstable();
    assert_eq!(h, d);
    for s in [direct, human] {
        s.shutdown().await;
    }
}

/// With typing direct, a seed moves the pointer as it did before typing could draw from
/// the stream: the type and key between the pointer acts change nothing.
#[tokio::test]
async fn with_typing_direct_a_seed_moves_the_pointer_as_it_always_has() {
    let mixed = typist(7, PointerMode::Humanized, TypingMode::Direct).await;
    let pointer_only = typist(7, PointerMode::Humanized, TypingMode::Direct).await;
    let with_typing = pointer_calls(&a_few_mixed_acts(&mixed).await);
    for act in [
        ComputerAction::LeftClick {
            coordinate: [900, 600],
        },
        ComputerAction::MouseMove {
            coordinate: [100, 100],
        },
    ] {
        expect_ok(pointer_only.action(&act).await).await;
    }
    assert_eq!(with_typing, pointer_calls(&pointer_only.session.calls()));
    for h in [mixed, pointer_only] {
        h.shutdown().await;
    }
}

/// Typing is not humanized: one down and one up per character, as in direct mode.
#[tokio::test]
async fn typing_with_a_humanized_pointer_types_as_direct_mode_does() {
    let h = humanized(1).await;
    expect_ok(h.action(&ComputerAction::Type { text: "hi".into() }).await).await;
    let (hk, ik) = (
        bolide_rfb::keysym::keysym_for_char('h'),
        bolide_rfb::keysym::keysym_for_char('i'),
    );
    assert_eq!(
        h.session.calls(),
        vec![
            Call::Key {
                keysym: hk,
                down: true
            },
            Call::Key {
                keysym: hk,
                down: false
            },
            Call::Key {
                keysym: ik,
                down: true
            },
            Call::Key {
                keysym: ik,
                down: false
            },
        ]
    );
    h.shutdown().await;
}
