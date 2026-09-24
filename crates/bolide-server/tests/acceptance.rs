//! **bolide v0's acceptance test**: an agent's HTTP request reaches a real RFB desktop,
//! and the desktop's screen reaches the agent.
//!
//! ```text
//!   reqwest ──HTTP/JSON──▶ bolide-server ──bolide-rfb──▶ RFB/TCP ──▶ bolide-testkit
//! ```
//!
//! Every layer is the real one. The fake at the far end is a *real RFB server* — it
//! speaks the protocol over a real loopback socket, paints a scripted screen and records
//! every message the client sends — it simply lives in this process, which is what makes
//! the whole stack assertable offline in milliseconds.
//!
//! This is the test that would have to pass before anybody pointed bolide at their Mac.
//! Everything else in the suite is a component test underneath it: `tests/routes.rs`
//! asserts the HTTP layer against a fake `Session`, `bolide-rfb`'s tests assert the
//! protocol against scripted bytes. Only here do the two halves have to agree.
//!
//! The password path is part of the acceptance, not a separate nicety: the desktop bolide
//! exists to unblock is behind VNC authentication, so "connects with a password" is a
//! claim v0 has to make.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use bolide_rfb::{Config, Session, SessionHandle};
use bolide_server::wire::{ClipboardResponse, ComputerAction, ComputerResponse, StatusResponse};
use bolide_server::{Running, ServerConfig};
use bolide_testkit::{ClientEvent, Encoding, FakeServer, Screen};

/// Long enough that a loaded CI box does not fail the test, short enough that a genuine
/// hang is reported as a failure rather than eating the job's timeout.
const PATIENCE: Duration = Duration::from_secs(5);

const WIDTH: u16 = 200;
const HEIGHT: u16 = 120;
const BACKGROUND: [u8; 4] = [0x20, 0x30, 0x40, 0xff];
const BADGE: [u8; 4] = [0xd0, 0x10, 0x60, 0xff];

/// A screen with something on it, so "the pixels came from the desktop" is a real claim
/// rather than "both ends agreed on black".
fn scripted_screen() -> Screen {
    let mut screen = Screen::solid(WIDTH, HEIGHT, BACKGROUND);
    screen.fill_rect(20, 10, 40, 25, BADGE);
    screen
}

/// The whole stack, wired up and torn down together.
struct Stack {
    fake: FakeServer,
    running: Running,
    base: String,
    client: reqwest::Client,
    session: SessionHandle,
}

impl Stack {
    async fn up(password: Option<&str>, encoding: Encoding) -> Stack {
        let mut builder = FakeServer::builder()
            .screen(scripted_screen())
            .desktop_name("acceptance")
            .encoding(encoding);
        if let Some(password) = password {
            builder = builder.password(password);
        }
        let fake = builder.start().await.expect("the fake server bound");

        let config = Config {
            password: password.map(str::to_string),
            timeout: PATIENCE,
            ..Config::default()
        };
        let session = bolide_rfb::connect(&fake.addr().to_string(), config)
            .await
            .expect("bolide connected to the desktop");

        let running = bolide_server::serve(
            Arc::new(session.clone()),
            ServerConfig {
                remote: fake.addr().to_string(),
                frame_timeout: PATIENCE,
                ..Default::default()
            },
        )
        .await
        .expect("the loopback server bound");

        let base = format!("http://{}", running.addr);
        Stack {
            fake,
            running,
            base,
            client: reqwest::Client::new(),
            session,
        }
    }

    async fn action(&self, action: ComputerAction) -> ComputerResponse {
        let response = self
            .client
            .post(format!("{}/computer", self.base))
            .json(&serde_json::to_value(&action).expect("serialized"))
            .send()
            .await
            .expect("request");
        assert_eq!(
            response.status().as_u16(),
            200,
            "POST /computer {action:?} was not accepted"
        );
        response.json().await.expect("a ComputerResponse")
    }

    /// Wait for a recorded client event, failing the test rather than hanging.
    async fn saw(&self, what: &str, pred: impl Fn(&ClientEvent) -> bool + Send) -> ClientEvent {
        match self.fake.wait_for(pred, PATIENCE).await {
            Ok(event) => event,
            Err(e) => panic!(
                "the desktop never saw {what} ({e}); it did see: {:?}",
                self.fake.events().await
            ),
        }
    }

    async fn down(self) {
        self.session.close().await.ok();
        self.running.shutdown().await;
        self.fake.shutdown().await;
    }
}

/// Decode a PNG into `(width, height, rgba)`.
fn decode_png(bytes: &[u8]) -> (u32, u32, Vec<u8>) {
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().expect("a readable PNG");
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).expect("a PNG frame");
    buf.truncate(info.buffer_size());
    (info.width, info.height, buf)
}

fn pixel_at(width: u32, rgba: &[u8], x: u32, y: u32) -> [u8; 4] {
    let i = ((y * width + x) * 4) as usize;
    [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
}

// ---------------------------------------------------------------------------

/// The acceptance itself: four claims, one connection, nothing faked but the desktop.
#[tokio::test]
async fn an_agent_drives_a_real_rfb_desktop_over_http() {
    let stack = Stack::up(None, Encoding::Negotiated).await;

    // 1. A screenshot is the desktop's pixels, not a plausible black rectangle.
    let response = stack.action(ComputerAction::Screenshot).await;
    assert!(response.ok);
    let image = response.image.expect("screenshot returned an image");
    assert_eq!(image.format, "png");
    assert_eq!((image.width, image.height), (WIDTH, HEIGHT));
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&image.data)
        .expect("the image is base64");
    let (w, h, rgba) = decode_png(&bytes);
    assert_eq!((w, h), (WIDTH as u32, HEIGHT as u32));
    assert_eq!(
        pixel_at(w, &rgba, 5, 5),
        BACKGROUND,
        "the background did not survive the round trip"
    );
    assert_eq!(
        pixel_at(w, &rgba, 25, 15),
        BADGE,
        "the badge did not survive the round trip — a screen that is merely the right \
         size proves nothing"
    );
    assert_eq!(
        pixel_at(w, &rgba, 150, 100),
        BACKGROUND,
        "a uniform region of the screen is wrong"
    );

    // 2. A click lands on the desktop, at the coordinate the agent named, as a press
    //    and a release — and the move comes first.
    stack
        .action(ComputerAction::LeftClick {
            coordinate: [42, 17],
        })
        .await;
    stack
        .saw("the button press", |e| {
            matches!(e, ClientEvent::Pointer { x: 42, y: 17, buttons } if *buttons == bolide_rfb::proto::BUTTON_LEFT)
        })
        .await;
    let pointers: Vec<(u16, u16, u8)> = stack
        .fake
        .events()
        .await
        .into_iter()
        .filter_map(|e| match e {
            ClientEvent::Pointer { x, y, buttons } => Some((x, y, buttons)),
            _ => None,
        })
        .collect();
    assert_eq!(
        pointers,
        vec![
            (42, 17, 0),
            (42, 17, bolide_rfb::proto::BUTTON_LEFT),
            (42, 17, 0)
        ],
        "a click is move, press, release — in that order"
    );

    // 3. Typing arrives as key events, one down and one up per character.
    stack
        .action(ComputerAction::Type { text: "hi".into() })
        .await;
    let h = bolide_rfb::keysym::keysym_for_char('h');
    let i = bolide_rfb::keysym::keysym_for_char('i');
    stack
        .saw(
            "the last keystroke released",
            |e| matches!(e, ClientEvent::Key { keysym, down: false } if *keysym == i),
        )
        .await;
    let keys: Vec<(u32, bool)> = stack
        .fake
        .events()
        .await
        .into_iter()
        .filter_map(|e| match e {
            ClientEvent::Key { keysym, down } => Some((keysym, down)),
            _ => None,
        })
        .collect();
    assert_eq!(
        keys,
        vec![(h, true), (h, false), (i, true), (i, false)],
        "typing \"hi\" is h down/up then i down/up"
    );

    // 4. The clipboard crosses in both directions.
    let pushed = "bolide was here";
    let response = stack
        .client
        .post(format!("{}/clipboard", stack.base))
        .json(&serde_json::json!({ "text": pushed }))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status().as_u16(), 200);
    stack
        .saw(
            "the clipboard push",
            |e| matches!(e, ClientEvent::CutText(text) if text == pushed),
        )
        .await;

    stack.control_sends_clipboard("from the desktop").await;

    stack.down().await;
}

impl Stack {
    /// The other direction: the desktop volunteers a clipboard and `GET /clipboard`
    /// reports it. Polled rather than slept on — ServerCutText arrives on the session's
    /// read loop, so there is no request whose completion means "it landed".
    async fn control_sends_clipboard(&self, text: &str) {
        self.fake.control().send_cut_text(text).await;
        let deadline = tokio::time::Instant::now() + PATIENCE;
        loop {
            let body: ClipboardResponse = self
                .client
                .get(format!("{}/clipboard", self.base))
                .send()
                .await
                .expect("request")
                .json()
                .await
                .expect("a ClipboardResponse");
            if body.text.as_deref() == Some(text) {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the desktop's clipboard never reached GET /clipboard (saw {:?})",
                body.text
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

/// The desktop bolide exists to unblock is behind VNC authentication, so connecting
/// through it — and then seeing its pixels — is part of what v0 claims.
#[tokio::test]
async fn the_whole_stack_works_through_vnc_authentication() {
    let stack = Stack::up(Some("hunter2"), Encoding::Negotiated).await;

    let status: StatusResponse = stack
        .client
        .get(format!("{}/status", stack.base))
        .send()
        .await
        .expect("request")
        .json()
        .await
        .expect("a StatusResponse");
    assert!(status.connected);
    assert_eq!(status.desktop_name, "acceptance");
    assert_eq!((status.width, status.height), (WIDTH, HEIGHT));
    assert!(
        !status.remote.contains("hunter2"),
        "the password reached /status: {}",
        status.remote
    );

    let response = stack.action(ComputerAction::Screenshot).await;
    let image = response.image.expect("an image");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&image.data)
        .expect("base64");
    let (w, _, rgba) = decode_png(&bytes);
    assert_eq!(pixel_at(w, &rgba, 25, 15), BADGE);
    assert_eq!(pixel_at(w, &rgba, 150, 100), BACKGROUND);

    stack.down().await;
}

/// The same acceptance with ZRLE forced, because the encoding bolide advertises *first*
/// is the one a real server will pick — and a screenshot that decodes correctly under
/// Raw proves nothing about the decoder that will actually run.
#[tokio::test]
async fn a_screenshot_decodes_when_the_desktop_speaks_zrle() {
    let stack = Stack::up(None, Encoding::Zrle).await;

    let image = stack
        .action(ComputerAction::Screenshot)
        .await
        .image
        .expect("an image");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&image.data)
        .expect("base64");
    let (w, h, rgba) = decode_png(&bytes);
    assert_eq!((w, h), (WIDTH as u32, HEIGHT as u32));
    assert_eq!(pixel_at(w, &rgba, 25, 15), BADGE);
    // Two BACKGROUND pixels, and they are not the same assertion twice. ZRLE codes the
    // screen as 64-pixel tiles and picks a subencoding per tile: (5, 5) sits in the
    // tile the badge intrudes into, so it arrives as a *palette* tile, while (150, 100)
    // sits in a tile that is one colour throughout and arrives as a *solid* one.
    // Asserting only the first left the solid path uncovered — a mutation that swapped
    // red and blue in solid tiles passed the whole acceptance suite.
    assert_eq!(
        pixel_at(w, &rgba, 5, 5),
        BACKGROUND,
        "the palette-coded tile is wrong"
    );
    assert_eq!(
        pixel_at(w, &rgba, 150, 100),
        BACKGROUND,
        "the solid-coded tile is wrong"
    );

    // A second screenshot after the screen changes: the ZRLE zlib stream carries state
    // across rects, so the frame that matters is the one that is not the first.
    let mut moved = scripted_screen();
    moved.fill_rect(100, 60, 30, 30, [0x00, 0xff, 0x00, 0xff]);
    stack.fake.control().set_screen(moved).await;

    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let image = stack
            .action(ComputerAction::Screenshot)
            .await
            .image
            .expect("an image");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&image.data)
            .expect("base64");
        let (w, _, rgba) = decode_png(&bytes);
        if pixel_at(w, &rgba, 110, 70) == [0x00, 0xff, 0x00, 0xff] {
            // And the rest of the screen is still there — an update is not a repaint.
            assert_eq!(pixel_at(w, &rgba, 25, 15), BADGE);
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the second ZRLE frame never arrived"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    stack.down().await;
}
