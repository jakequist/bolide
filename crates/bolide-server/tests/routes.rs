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
use bolide_server::{Running, ServerConfig};
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
    last_cut: Mutex<Option<String>>,
    events: broadcast::Sender<ServerEvent>,
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
            last_cut: Mutex::new(None),
            events: broadcast::channel(16).0,
        })
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
        _timeout: Duration,
    ) -> bolide_rfb::Result<Framebuffer> {
        self.log.lock().unwrap().push(Call::Refresh { incremental });
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
