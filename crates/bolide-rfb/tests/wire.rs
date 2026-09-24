//! The client, driven over `tokio::io::duplex` against hand-written server bytes.
//!
//! No socket, no fake server, no clock: the script is a `Vec<u8>` and every wait is a
//! channel under a timeout. What is asserted here is the two things a unit test of the
//! decoders cannot reach — the *handshake*, which is an ordering, and the *session*,
//! which is a set of promises about what the read loop does next.
//!
//! Server bytes are built from `bolide_rfb::proto` where it has a builder and by hand
//! where it does not. By hand is deliberate for the server→client direction: a helper
//! shared with the client would agree with a misreading.

use std::time::Duration;

use bolide_rfb::proto::{self, PixelFormat};
use bolide_rfb::{connect_stream, Config, Error, ServerEvent, Session, SessionHandle};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::broadcast;

const W: u16 = 8;
const H: u16 = 4;
const NAME: &str = "a desktop";

/// Long enough that a loaded box does not flake, short enough that a hang is a failure
/// rather than a coffee break. Never used as synchronisation — only as a deadline.
const PATIENCE: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Server-side script builders
// ---------------------------------------------------------------------------

fn server_init(w: u16, h: u16, name: &str) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&w.to_be_bytes());
    b.extend_from_slice(&h.to_be_bytes());
    b.extend_from_slice(&PixelFormat::RGBX_32.encode());
    b.extend_from_slice(&(name.len() as u32).to_be_bytes());
    b.extend_from_slice(name.as_bytes());
    b
}

/// 3.8, security `None`, SecurityResult OK, ServerInit — the boring happy path.
fn open_3_8() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(proto::VERSION_3_8);
    b.extend_from_slice(&[1, proto::SECURITY_NONE]);
    b.extend_from_slice(&proto::SECURITY_RESULT_OK.to_be_bytes());
    b.extend_from_slice(&server_init(W, H, NAME));
    b
}

fn u32_string(s: &str) -> Vec<u8> {
    let mut b = (s.len() as u32).to_be_bytes().to_vec();
    b.extend_from_slice(s.as_bytes());
    b
}

fn rect_header(x: u16, y: u16, w: u16, h: u16, encoding: i32) -> Vec<u8> {
    let mut b = Vec::new();
    for v in [x, y, w, h] {
        b.extend_from_slice(&v.to_be_bytes());
    }
    b.extend_from_slice(&encoding.to_be_bytes());
    b
}

fn raw_rect(x: u16, y: u16, w: u16, h: u16, pixels: &[[u8; 4]]) -> Vec<u8> {
    let mut b = rect_header(x, y, w, h, proto::ENC_RAW);
    let f = PixelFormat::RGBX_32;
    for p in pixels {
        f.write_pixel(f.from_rgba(*p), &mut b);
    }
    b
}

fn copy_rect(x: u16, y: u16, w: u16, h: u16, src_x: u16, src_y: u16) -> Vec<u8> {
    let mut b = rect_header(x, y, w, h, proto::ENC_COPY_RECT);
    b.extend_from_slice(&src_x.to_be_bytes());
    b.extend_from_slice(&src_y.to_be_bytes());
    b
}

fn zrle_rect(x: u16, y: u16, w: u16, h: u16, payload: &[u8]) -> Vec<u8> {
    let mut b = rect_header(x, y, w, h, proto::ENC_ZRLE);
    b.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    b.extend_from_slice(payload);
    b
}

/// FramebufferUpdate: message 0, a pad byte, the rect count, then the rects.
fn update(rects: &[Vec<u8>]) -> Vec<u8> {
    let mut b = vec![proto::server_msg::FRAMEBUFFER_UPDATE, 0];
    b.extend_from_slice(&(rects.len() as u16).to_be_bytes());
    for r in rects {
        b.extend_from_slice(r);
    }
    b
}

fn server_cut_text(text: &str) -> Vec<u8> {
    let mut b = vec![proto::server_msg::SERVER_CUT_TEXT, 0, 0, 0];
    b.extend_from_slice(&u32_string(text));
    b
}

/// A solid 64x64-or-smaller ZRLE tile, deflated into `c`'s single stream.
fn zrle_solid(c: &mut flate2::Compress, colour: [u8; 4]) -> Vec<u8> {
    let f = PixelFormat::RGBX_32;
    let mut tile = vec![1u8];
    let mut wire = Vec::new();
    f.write_pixel(f.from_rgba(colour), &mut wire);
    tile.extend_from_slice(&wire[f.cpixel_offset()..f.cpixel_offset() + f.cpixel_bytes()]);
    let mut out = Vec::with_capacity(1024);
    c.compress_vec(&tile, &mut out, flate2::FlushCompress::Sync)
        .unwrap();
    out
}

// ---------------------------------------------------------------------------
// Test plumbing
// ---------------------------------------------------------------------------

/// Preload the whole server script, then connect. The duplex buffers it, so no task
/// has to be scheduled in a particular order for the handshake to complete.
async fn dial(script: &[u8], config: Config) -> (bolide_rfb::Result<SessionHandle>, DuplexStream) {
    let (client, mut server) = tokio::io::duplex(1 << 16);
    server.write_all(script).await.unwrap();
    let result = connect_stream(client, config).await;
    (result, server)
}

async fn connected(config: Config) -> (SessionHandle, DuplexStream) {
    let (result, server) = dial(&open_3_8(), config).await;
    (result.expect("the happy path connects"), server)
}

async fn expect(server: &mut DuplexStream, n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    tokio::time::timeout(PATIENCE, server.read_exact(&mut buf))
        .await
        .expect("the client never wrote enough bytes")
        .expect("the client hung up early");
    buf
}

/// Read past everything a connection opens with: the version reply, the chosen security
/// type, ClientInit, then SetPixelFormat and SetEncodings.
async fn skip_setup(server: &mut DuplexStream) {
    let setup = expect(server, 14 + 40).await;
    let mut want = proto::VERSION_3_8.to_vec();
    want.extend_from_slice(&[proto::SECURITY_NONE, 1]);
    want.extend_from_slice(&proto::set_pixel_format(&PixelFormat::RGBX_32));
    want.extend_from_slice(&proto::set_encodings(proto::ADVERTISED_ENCODINGS));
    assert_eq!(setup, want);
}

async fn next_event(rx: &mut broadcast::Receiver<ServerEvent>) -> ServerEvent {
    tokio::time::timeout(PATIENCE, rx.recv())
        .await
        .expect("no event arrived")
        .expect("the event channel closed")
}

/// Wait for the first event matching `f`, so an unrelated `Painted` does not fail a
/// test that is waiting for a `Bell`.
async fn next_matching(
    rx: &mut broadcast::Receiver<ServerEvent>,
    f: impl Fn(&ServerEvent) -> bool,
) -> ServerEvent {
    tokio::time::timeout(PATIENCE, async {
        loop {
            let e = rx.recv().await.expect("the event channel closed");
            if f(&e) {
                return e;
            }
        }
    })
    .await
    .expect("no matching event arrived")
}

const RED: [u8; 4] = [255, 0, 0, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];

// ---------------------------------------------------------------------------
// Handshake
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_3_8_server_with_no_security_connects_and_sets_up_the_pixel_format() {
    let (session, mut server) = connected(Config::default()).await;
    assert_eq!(session.size(), (W, H));
    assert_eq!(session.desktop_name(), NAME);
    assert!(session.is_connected());

    // The client answered with its own version, then chose None, then ClientInit.
    let opening = expect(&mut server, 14).await;
    assert_eq!(&opening[..12], proto::VERSION_3_8);
    assert_eq!(opening[12], proto::SECURITY_NONE);
    assert_eq!(opening[13], 1, "shared defaults to true");

    // And the first two things it says as a client are these, in this order.
    let setup = expect(&mut server, 40).await;
    assert_eq!(
        &setup[..20],
        &proto::set_pixel_format(&PixelFormat::RGBX_32)[..]
    );
    assert_eq!(
        &setup[20..],
        &proto::set_encodings(proto::ADVERTISED_ENCODINGS)[..]
    );
}

#[tokio::test]
async fn vnc_auth_answers_the_challenge_exactly_as_proto_says() {
    let challenge = [0x11u8; 16];
    let mut script = Vec::new();
    script.extend_from_slice(proto::VERSION_3_8);
    script.extend_from_slice(&[1, proto::SECURITY_VNC_AUTH]);
    script.extend_from_slice(&challenge);
    script.extend_from_slice(&proto::SECURITY_RESULT_OK.to_be_bytes());
    script.extend_from_slice(&server_init(W, H, NAME));

    let (result, mut server) = dial(&script, Config::default().with_password("hunter2")).await;
    result.expect("VNC auth with a password connects");

    let written = expect(&mut server, 12 + 1 + 16 + 1).await;
    assert_eq!(written[12], proto::SECURITY_VNC_AUTH);
    assert_eq!(
        &written[13..29],
        &proto::vnc_auth_response("hunter2", &challenge)[..]
    );
}

/// Sending the DES of an empty password is a login attempt. A server that counts them
/// locks the account, so the failure has to happen before the response goes out.
#[tokio::test]
async fn vnc_auth_without_a_password_fails_before_answering_the_challenge() {
    let mut script = Vec::new();
    script.extend_from_slice(proto::VERSION_3_8);
    script.extend_from_slice(&[1, proto::SECURITY_VNC_AUTH]);
    script.extend_from_slice(&[0x22u8; 16]);
    script.extend_from_slice(&proto::SECURITY_RESULT_OK.to_be_bytes());
    script.extend_from_slice(&server_init(W, H, NAME));

    let (result, mut server) = dial(&script, Config::default()).await;
    assert!(
        matches!(result, Err(Error::PasswordRequired)),
        "{:?}",
        result.err()
    );

    let mut all = Vec::new();
    tokio::time::timeout(PATIENCE, server.read_to_end(&mut all))
        .await
        .expect("the client did not hang up")
        .unwrap();
    let mut want = proto::VERSION_3_8.to_vec();
    want.push(proto::SECURITY_VNC_AUTH);
    assert_eq!(all, want, "the client sent something after choosing");
}

#[tokio::test]
async fn a_failed_security_result_carries_the_servers_reason() {
    let mut script = Vec::new();
    script.extend_from_slice(proto::VERSION_3_8);
    script.extend_from_slice(&[1, proto::SECURITY_NONE]);
    script.extend_from_slice(&1u32.to_be_bytes());
    script.extend_from_slice(&u32_string("too many attempts"));

    let (result, _server) = dial(&script, Config::default()).await;
    match result {
        Err(Error::AuthFailed(reason)) => assert_eq!(reason, "too many attempts"),
        other => panic!("expected AuthFailed, got {:?}", other.err()),
    }
}

#[tokio::test]
async fn an_empty_security_list_is_a_refusal_with_a_reason() {
    let mut script = Vec::new();
    script.extend_from_slice(proto::VERSION_3_8);
    script.push(0);
    script.extend_from_slice(&u32_string("no sessions available"));

    let (result, _server) = dial(&script, Config::default()).await;
    match result {
        Err(Error::Refused(reason)) => assert_eq!(reason, "no sessions available"),
        other => panic!("expected Refused, got {:?}", other.err()),
    }
}

/// Naming what was offered is the difference between "bolide cannot connect" and "this
/// is an Apple Remote Desktop server and bolide does not speak ARD".
#[tokio::test]
async fn a_security_list_bolide_cannot_answer_names_what_was_offered() {
    let mut script = Vec::new();
    script.extend_from_slice(proto::VERSION_3_8);
    script.extend_from_slice(&[1, proto::SECURITY_ARD]);

    let (result, _server) = dial(&script, Config::default().with_password("x")).await;
    match result {
        Err(Error::NoSupportedSecurity { offered }) => {
            assert_eq!(offered, vec![proto::SECURITY_ARD]);
        }
        other => panic!("expected NoSupportedSecurity, got {:?}", other.err()),
    }
}

/// 3.3 has no security *list*: the server decides, says so in a `u32`, and — for
/// `None` — sends no SecurityResult at all. Reading one desynchronises ServerInit.
#[tokio::test]
async fn a_3_3_server_connects_on_the_3_3_path() {
    let mut script = Vec::new();
    script.extend_from_slice(proto::VERSION_3_3);
    script.extend_from_slice(&(proto::SECURITY_NONE as u32).to_be_bytes());
    script.extend_from_slice(&server_init(W, H, "old server"));

    let (result, mut server) = dial(&script, Config::default()).await;
    let session = result.expect("a 3.3 server connects");
    assert_eq!(session.desktop_name(), "old server");
    assert_eq!(session.size(), (W, H));

    // It answered 3.3, and chose nothing — the next byte is ClientInit.
    let written = expect(&mut server, 13).await;
    assert_eq!(&written[..12], proto::VERSION_3_3);
    assert_eq!(written[12], 1);
}

#[tokio::test]
async fn a_greeting_that_is_not_a_protocol_version_is_not_rfb() {
    let (result, _server) = dial(b"HTTP/1.1 200\n", Config::default()).await;
    assert!(matches!(result, Err(Error::NotRfb)), "{:?}", result.err());
}

/// docker's userland proxy accepts a published port before the container's VNC server
/// is listening, so this is a real shape, and it is worth redialling.
#[tokio::test]
async fn a_server_that_says_nothing_times_out_and_says_it_is_retryable() {
    let config = Config {
        timeout: Duration::from_millis(80),
        ..Config::default()
    };
    let (client, _server) = tokio::io::duplex(1 << 16);
    let result = connect_stream(client, config).await;
    match result {
        Err(e @ Error::Timeout(_)) => assert!(e.is_retryable(), "{e}"),
        other => panic!("expected Timeout, got {:?}", other.err()),
    }
}

// ---------------------------------------------------------------------------
// Decoders, end to end through the read loop
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_raw_rect_paints_the_framebuffer_at_the_right_offset() {
    let (session, mut server) = connected(Config::default()).await;
    let mut events = session.events();
    skip_setup(&mut server).await;

    server
        .write_all(&update(&[raw_rect(1, 2, 2, 1, &[RED, GREEN])]))
        .await
        .unwrap();
    next_matching(&mut events, |e| matches!(e, ServerEvent::Painted { .. })).await;

    let fb = session.framebuffer().await.unwrap();
    assert_eq!(fb.pixel(1, 2), Some(RED));
    assert_eq!(fb.pixel(2, 2), Some(GREEN));
    assert_eq!(fb.pixel(0, 0), Some([0, 0, 0, 255]));
}

/// A scroll is a CopyRect of a region onto itself, shifted. It is the case a naive
/// row-by-row copy smears.
#[tokio::test]
async fn a_copy_rect_that_overlaps_itself_scrolls_rather_than_smears() {
    let (session, mut server) = connected(Config::default()).await;
    let mut events = session.events();
    skip_setup(&mut server).await;

    // Paint the top three rows of column 0 red/green/blue, then scroll them down one.
    let paint = update(&[raw_rect(0, 0, 1, 3, &[RED, GREEN, BLUE])]);
    server.write_all(&paint).await.unwrap();
    next_matching(&mut events, |e| matches!(e, ServerEvent::Painted { .. })).await;

    server
        .write_all(&update(&[copy_rect(0, 1, 1, 3, 0, 0)]))
        .await
        .unwrap();
    next_matching(&mut events, |e| matches!(e, ServerEvent::Painted { .. })).await;

    let fb = session.framebuffer().await.unwrap();
    assert_eq!(fb.pixel(0, 0), Some(RED));
    assert_eq!(fb.pixel(0, 1), Some(RED));
    assert_eq!(fb.pixel(0, 2), Some(GREEN));
    assert_eq!(fb.pixel(0, 3), Some(BLUE));
}

/// **The bug the design invites, asserted through the whole client.** The server keeps
/// one deflate stream for the connection and flushes between rects; a client that made
/// a new inflater per rect decodes the first and fails on every one after it.
#[tokio::test]
async fn zrle_decodes_a_second_rect_from_a_stream_that_was_never_reset() {
    let (session, mut server) = connected(Config::default()).await;
    let mut events = session.events();
    skip_setup(&mut server).await;

    let mut deflate = flate2::Compress::new(flate2::Compression::default(), true);
    let first = zrle_solid(&mut deflate, RED);
    let second = zrle_solid(&mut deflate, GREEN);

    server
        .write_all(&update(&[zrle_rect(0, 0, W, H, &first)]))
        .await
        .unwrap();
    next_matching(&mut events, |e| matches!(e, ServerEvent::Painted { .. })).await;
    assert_eq!(session.framebuffer().await.unwrap().pixel(4, 2), Some(RED));

    // A separate FramebufferUpdate, so the two rects are not even in the same message.
    server
        .write_all(&update(&[zrle_rect(0, 0, W, H, &second)]))
        .await
        .unwrap();
    next_matching(&mut events, |e| matches!(e, ServerEvent::Painted { .. })).await;
    assert_eq!(
        session.framebuffer().await.unwrap().pixel(4, 2),
        Some(GREEN)
    );

    assert!(
        session.is_connected(),
        "the session died decoding the second rect"
    );
}

#[tokio::test]
async fn a_desktop_size_rect_reallocates_the_framebuffer_and_unpaints_it() {
    let (session, mut server) = connected(Config::default()).await;
    let mut events = session.events();
    skip_setup(&mut server).await;

    server
        .write_all(&update(&[raw_rect(0, 0, 1, 1, &[RED])]))
        .await
        .unwrap();
    next_matching(&mut events, |e| matches!(e, ServerEvent::Painted { .. })).await;
    assert!(session.painted());

    server
        .write_all(&update(&[rect_header(
            0,
            0,
            100,
            50,
            proto::ENC_DESKTOP_SIZE,
        )]))
        .await
        .unwrap();
    let resized = next_matching(&mut events, |e| matches!(e, ServerEvent::Resized { .. })).await;
    assert_eq!(
        resized,
        ServerEvent::Resized {
            width: 100,
            height: 50
        }
    );
    assert_eq!(session.size(), (100, 50));
    assert!(!session.painted(), "a resized screen is black and wrong");
    let fb = session.framebuffer().await.unwrap();
    assert_eq!(fb.rgba.len(), 100 * 50 * 4);
    assert_eq!(fb.pixel(0, 0), Some([0, 0, 0, 255]));

    // And it asks for the whole screen at the *new* size, because there is no baseline
    // any more. Past the opening request and the continuation after the first update.
    let requests = expect(&mut server, 30).await;
    assert_eq!(
        &requests[..10],
        &proto::framebuffer_update_request(false, 0, 0, W, H)[..]
    );
    assert_eq!(
        &requests[10..20],
        &proto::framebuffer_update_request(true, 0, 0, W, H)[..]
    );
    assert_eq!(
        &requests[20..],
        &proto::framebuffer_update_request(false, 0, 0, 100, 50)[..]
    );
}

#[tokio::test]
async fn an_encoding_bolide_never_advertised_ends_the_session_saying_so() {
    let (session, mut server) = connected(Config::default()).await;
    let mut events = session.events();
    skip_setup(&mut server).await;

    server
        .write_all(&update(&[rect_header(0, 0, 1, 1, 5)]))
        .await
        .unwrap();
    match next_matching(&mut events, |e| {
        matches!(e, ServerEvent::Disconnected { .. })
    })
    .await
    {
        ServerEvent::Disconnected { reason } => {
            assert_eq!(reason, Some(Error::UnsupportedEncoding(5).to_string()));
        }
        other => panic!("{other:?}"),
    }
    assert!(!session.is_connected());
}

// ---------------------------------------------------------------------------
// Session semantics
// ---------------------------------------------------------------------------

/// "Incremental" means "changes since the update you last sent me". Before the first
/// one there is nothing for it to mean, and a server answering it sends nothing at all.
#[tokio::test]
async fn the_first_request_is_never_incremental_however_it_was_asked_for() {
    let config = Config {
        continuous: false,
        ..Config::default()
    };
    let (session, mut server) = connected(config).await;
    skip_setup(&mut server).await;

    let refresh = session.refresh(true, PATIENCE);
    let script = async {
        let request = expect(&mut server, 10).await;
        assert_eq!(
            request,
            proto::framebuffer_update_request(false, 0, 0, W, H),
            "the caller asked for incremental and there was no baseline"
        );
        server
            .write_all(&update(&[raw_rect(0, 0, 1, 1, &[BLUE])]))
            .await
            .unwrap();
    };
    let (frame, ()) = tokio::join!(refresh, script);
    assert_eq!(frame.unwrap().pixel(0, 0), Some(BLUE));

    // Now there *is* a baseline, so the caller's word is taken.
    let second = session.refresh(true, PATIENCE);
    let script = async {
        let request = expect(&mut server, 10).await;
        assert_eq!(request, proto::framebuffer_update_request(true, 0, 0, W, H));
        server
            .write_all(&update(&[raw_rect(0, 0, 1, 1, &[GREEN])]))
            .await
            .unwrap();
    };
    let (frame, ()) = tokio::join!(second, script);
    assert_eq!(frame.unwrap().pixel(0, 0), Some(GREEN));
}

/// A frame returned before the update was applied is a stale screenshot handed to a
/// model as the truth.
#[tokio::test]
async fn refresh_resolves_on_the_next_applied_update_and_returns_that_frame() {
    let config = Config {
        continuous: false,
        ..Config::default()
    };
    let (session, mut server) = connected(config).await;
    skip_setup(&mut server).await;

    let refresh = session.refresh(false, PATIENCE);
    let script = async {
        let request = expect(&mut server, 10).await;
        assert_eq!(
            request,
            proto::framebuffer_update_request(false, 0, 0, W, H)
        );
        server
            .write_all(&update(&[raw_rect(3, 3, 1, 1, &[GREEN])]))
            .await
            .unwrap();
    };
    let (frame, ()) = tokio::join!(refresh, script);
    assert_eq!(frame.unwrap().pixel(3, 3), Some(GREEN));
}

#[tokio::test]
async fn refresh_gives_up_with_a_timeout_when_the_screen_never_changes() {
    let config = Config {
        continuous: false,
        ..Config::default()
    };
    let (session, mut server) = connected(config).await;
    skip_setup(&mut server).await;

    let err = session
        .refresh(true, Duration::from_millis(80))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Timeout(_)), "{err:?}");
    assert!(session.is_connected(), "a still screen is not a dead one");
}

/// A black framebuffer at exactly screen dimensions is indistinguishable from a real
/// black screen, which is why this flag exists at all.
#[tokio::test]
async fn painted_is_false_until_the_first_update_is_applied() {
    let config = Config {
        continuous: false,
        ..Config::default()
    };
    let (session, mut server) = connected(config).await;
    let mut events = session.events();
    skip_setup(&mut server).await;

    assert!(!session.painted());
    assert_eq!(
        session.framebuffer().await.unwrap().pixel(0, 0),
        Some([0, 0, 0, 255])
    );

    server
        .write_all(&update(&[raw_rect(0, 0, 1, 1, &[RED])]))
        .await
        .unwrap();
    assert_eq!(
        next_matching(&mut events, |e| matches!(e, ServerEvent::Painted { .. })).await,
        ServerEvent::Painted { generation: 1 }
    );
    assert!(session.painted());
}

/// One queue, so a message is never split by another message's bytes and two commands
/// issued in sequence reach the wire in that sequence.
#[tokio::test]
async fn commands_reach_the_wire_in_the_order_they_were_issued() {
    let config = Config {
        continuous: false,
        ..Config::default()
    };
    let (session, mut server) = connected(config).await;
    skip_setup(&mut server).await;

    session.key(0xff0d, true).await.unwrap();
    session.pointer(10, 20, proto::BUTTON_LEFT).await.unwrap();
    session.key(0xff0d, false).await.unwrap();
    session.cut_text("hi").await.unwrap();

    let mut want = proto::key_event(0xff0d, true);
    want.extend_from_slice(&proto::pointer_event(10, 20, proto::BUTTON_LEFT));
    want.extend_from_slice(&proto::key_event(0xff0d, false));
    want.extend_from_slice(&proto::client_cut_text("hi"));
    assert_eq!(expect(&mut server, want.len()).await, want);
}

/// A `type` is a burst: forty KeyEvents that must arrive with nothing between them. The
/// trait's `key()` cannot say that — two tasks calling it legitimately interleave — so
/// a burst is one command, and this is the test that says the queue keeps it whole
/// against a genuinely concurrent pointer on a multi-threaded runtime.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_key_burst_is_not_interleaved_by_a_concurrent_pointer() {
    let config = Config {
        continuous: false,
        ..Config::default()
    };
    let (session, mut server) = connected(config).await;
    skip_setup(&mut server).await;

    let burst: Vec<(u32, bool)> = (0..40)
        .flat_map(|i| [(0x61 + i as u32, true), (0x61 + i as u32, false)])
        .collect();

    let typist = {
        let session = session.clone();
        let burst = burst.clone();
        tokio::spawn(async move { session.key_burst(&burst).await })
    };
    let mouse = {
        let session = session.clone();
        tokio::spawn(async move { session.pointer(1, 2, 0).await })
    };
    typist.await.unwrap().unwrap();
    mouse.await.unwrap().unwrap();

    let mut burst_bytes = Vec::new();
    for (k, down) in &burst {
        burst_bytes.extend_from_slice(&proto::key_event(*k, *down));
    }
    let pointer_bytes = proto::pointer_event(1, 2, 0);

    let written = expect(&mut server, burst_bytes.len() + pointer_bytes.len()).await;
    // Whichever order the two tasks won, the burst is contiguous.
    let mut burst_then_pointer = burst_bytes.clone();
    burst_then_pointer.extend_from_slice(&pointer_bytes);
    let mut pointer_then_burst = pointer_bytes.clone();
    pointer_then_burst.extend_from_slice(&burst_bytes);
    assert!(
        written == burst_then_pointer || written == pointer_then_burst,
        "the pointer landed inside the burst"
    );
}

/// An idle handle should still hold a current screen, and it should not achieve that by
/// asking in a loop as fast as the socket will take it.
#[tokio::test]
async fn continuous_mode_keeps_exactly_one_request_outstanding() {
    let (session, mut server) = connected(Config::default()).await;
    let mut events = session.events();
    skip_setup(&mut server).await;

    let incremental = proto::framebuffer_update_request(true, 0, 0, W, H);
    assert_eq!(
        expect(&mut server, 10).await,
        proto::framebuffer_update_request(false, 0, 0, W, H),
        "the opening request is a whole screen"
    );

    for round in 1..=5u64 {
        assert_eq!(
            session.outstanding_requests(),
            1,
            "round {round}: asked more than once"
        );
        server
            .write_all(&update(&[raw_rect(0, 0, 1, 1, &[RED])]))
            .await
            .unwrap();
        assert_eq!(
            next_matching(&mut events, |e| matches!(e, ServerEvent::Painted { .. })).await,
            ServerEvent::Painted { generation: round }
        );
        assert_eq!(expect(&mut server, 10).await, incremental);
    }

    // Something that is not an update must not produce a request at all.
    server.write_all(&[proto::server_msg::BELL]).await.unwrap();
    next_matching(&mut events, |e| matches!(e, ServerEvent::Bell)).await;
    let mut stray = [0u8; 1];
    let idle = tokio::time::timeout(Duration::from_millis(120), server.read(&mut stray)).await;
    assert!(idle.is_err(), "the client asked again unprompted");
    assert_eq!(session.outstanding_requests(), 1);
}

/// The failure a daemon must never have is the silent one: a command that hangs
/// forever on a socket that died ten minutes ago.
#[tokio::test]
async fn the_socket_dying_is_an_event_and_every_later_command_is_closed() {
    let (session, server) = connected(Config::default()).await;
    let mut events = session.events();
    drop(server);

    match next_event(&mut events).await {
        ServerEvent::Disconnected { reason } => assert!(reason.is_some(), "an EOF has a reason"),
        other => panic!("{other:?}"),
    }
    assert!(!session.is_connected());

    for result in [
        session.key(0x61, true).await,
        session.pointer(0, 0, 0).await,
        session.cut_text("x").await,
    ] {
        assert!(matches!(result, Err(Error::Closed)), "{result:?}");
    }
    // Promptly — not after the refresh timeout.
    let err = tokio::time::timeout(PATIENCE, session.refresh(true, Duration::from_secs(600)))
        .await
        .expect("refresh hung on a dead session")
        .unwrap_err();
    assert!(matches!(err, Error::Closed), "{err:?}");
}

#[tokio::test]
async fn closing_is_idempotent_and_announces_itself_once() {
    let (session, mut server) = connected(Config::default()).await;
    let mut events = session.events();
    skip_setup(&mut server).await;

    session.close().await.unwrap();
    assert_eq!(
        next_matching(&mut events, |e| matches!(
            e,
            ServerEvent::Disconnected { .. }
        ))
        .await,
        ServerEvent::Disconnected { reason: None }
    );
    assert!(!session.is_connected());
    session.close().await.unwrap();
    assert!(matches!(session.key(0x61, true).await, Err(Error::Closed)));
}

#[tokio::test]
async fn cut_text_goes_out_verbatim_and_an_incoming_one_is_cached() {
    let (session, mut server) = connected(Config::default()).await;
    let mut events = session.events();
    skip_setup(&mut server).await;

    assert_eq!(session.last_cut_text(), None, "nothing was volunteered yet");

    session.cut_text("héllo").await.unwrap();
    // Past the opening FramebufferUpdateRequest.
    let _ = expect(&mut server, 10).await;
    let want = proto::client_cut_text("héllo");
    assert_eq!(expect(&mut server, want.len()).await, want);

    server
        .write_all(&server_cut_text("from the desktop"))
        .await
        .unwrap();
    assert_eq!(
        next_matching(&mut events, |e| matches!(e, ServerEvent::CutText(_))).await,
        ServerEvent::CutText("from the desktop".to_string())
    );
    assert_eq!(
        session.last_cut_text(),
        Some("from the desktop".to_string())
    );
}

#[tokio::test]
async fn the_bell_surfaces_as_an_event() {
    let (session, mut server) = connected(Config::default()).await;
    let mut events = session.events();
    skip_setup(&mut server).await;

    server.write_all(&[proto::server_msg::BELL]).await.unwrap();
    assert_eq!(
        next_matching(&mut events, |e| matches!(e, ServerEvent::Bell)).await,
        ServerEvent::Bell
    );
}

/// bolide negotiates true colour, so a colour map is noise — but it is *sized* noise,
/// and a client that skips the wrong number of bytes reads the rest of the connection
/// off by six.
#[tokio::test]
async fn a_colour_map_message_is_discarded_without_desynchronising_the_stream() {
    let (session, mut server) = connected(Config::default()).await;
    let mut events = session.events();
    skip_setup(&mut server).await;

    let mut colour_map = vec![proto::server_msg::SET_COLOUR_MAP_ENTRIES, 0];
    colour_map.extend_from_slice(&0u16.to_be_bytes()); // first colour
    colour_map.extend_from_slice(&3u16.to_be_bytes()); // three entries
    colour_map.extend_from_slice(&[0xab; 3 * 6]);
    // Whatever comes next must still parse, or the skip was the wrong length.
    colour_map.extend_from_slice(&update(&[raw_rect(2, 2, 1, 1, &[BLUE])]));
    server.write_all(&colour_map).await.unwrap();

    next_matching(&mut events, |e| matches!(e, ServerEvent::Painted { .. })).await;
    assert_eq!(session.framebuffer().await.unwrap().pixel(2, 2), Some(BLUE));
    assert!(session.is_connected());
}

/// A session nobody holds is a leaked socket and a leaked task.
#[tokio::test]
async fn dropping_the_last_handle_closes_the_session() {
    let (session, mut server) = connected(Config::default()).await;
    let mut events = session.events();
    skip_setup(&mut server).await;

    let clone = session.clone();
    drop(session);
    assert!(clone.is_connected(), "one handle is still holding it");
    drop(clone);

    assert_eq!(
        next_matching(&mut events, |e| matches!(
            e,
            ServerEvent::Disconnected { .. }
        ))
        .await,
        ServerEvent::Disconnected { reason: None }
    );
    let mut rest = Vec::new();
    tokio::time::timeout(PATIENCE, server.read_to_end(&mut rest))
        .await
        .expect("the client never hung up")
        .unwrap();
}
