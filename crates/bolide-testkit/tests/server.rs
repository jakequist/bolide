//! The fake server, driven by a hand-written client (see `common`).
//!
//! Nothing here uses `bolide_rfb::Session`. The fake has to agree with the *protocol*,
//! and a fake checked only against bolide's own client would be free to be wrong in
//! exactly the ways bolide is.

mod common;

use std::time::Duration;

use bolide_rfb::proto::{self, PixelFormat};
use bolide_testkit::{ClientEvent, Encoding, FakeServer, Screen, Security, ZrleTiling};
use common::{Client, ServerMessage};

const RED: [u8; 4] = [255, 0, 0, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];
const WAIT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Handshake
// ---------------------------------------------------------------------------

#[tokio::test]
async fn announces_3_8_and_offers_none_when_no_password_is_configured() {
    let server = FakeServer::builder().start().await.unwrap();
    let mut client = Client::connect(server.addr()).await;

    assert_eq!(client.version().await, proto::VERSION_3_8.to_vec());
    assert_eq!(client.security_types().await, vec![proto::SECURITY_NONE]);
    client.send(&[proto::SECURITY_NONE]).await;
    assert_eq!(client.security_result().await, proto::SECURITY_RESULT_OK);

    server.shutdown().await;
}

#[tokio::test]
async fn shutdown_does_not_wait_for_a_client_that_never_finished_the_handshake() {
    // A client that answers the banner and then goes quiet leaves the fake blocked on
    // a read. If that read is not racing the shutdown signal, `shutdown()` never
    // returns — and a test that only wanted to check the security list hangs the whole
    // run, which is indistinguishable on CI from a dead runner.
    let server = FakeServer::builder().start().await.unwrap();
    let mut client = Client::connect(server.addr()).await;
    client.version().await;
    client.security_types().await;

    tokio::time::timeout(WAIT, server.shutdown())
        .await
        .expect("shutdown must not wait on the client");
}

#[tokio::test]
async fn a_password_means_vnc_auth_and_the_right_response_is_accepted() {
    let server = FakeServer::builder()
        .password("hunter2")
        .start()
        .await
        .unwrap();
    let mut client = Client::connect(server.addr()).await;

    client.version().await;
    assert_eq!(
        client.security_types().await,
        vec![proto::SECURITY_VNC_AUTH]
    );
    client.send(&[proto::SECURITY_VNC_AUTH]).await;

    let challenge: [u8; 16] = client.read_n(16).await.try_into().unwrap();
    client
        .send(&proto::vnc_auth_response("hunter2", &challenge))
        .await;
    assert_eq!(client.security_result().await, proto::SECURITY_RESULT_OK);

    // And the session continues: a wrong DES transform would have stopped here.
    assert_eq!(client.init().await.name, "bolide-testkit");
    server.shutdown().await;
}

#[tokio::test]
async fn a_wrong_password_fails_the_security_result_with_a_reason() {
    let server = FakeServer::builder()
        .password("hunter2")
        .start()
        .await
        .unwrap();
    let mut client = Client::connect(server.addr()).await;

    client.version().await;
    client.security_types().await;
    client.send(&[proto::SECURITY_VNC_AUTH]).await;
    let challenge: [u8; 16] = client.read_n(16).await.try_into().unwrap();
    client
        .send(&proto::vnc_auth_response("not hunter2", &challenge))
        .await;

    assert_ne!(client.security_result().await, proto::SECURITY_RESULT_OK);
    // 3.8 sends a reason; the client's error message is only as good as this string.
    assert!(!client.failure_reason().await.is_empty());
    assert!(client.at_eof().await, "a failed handshake ends the session");

    server.shutdown().await;
}

#[tokio::test]
async fn always_fail_rejects_whatever_the_client_sends() {
    let server = FakeServer::builder()
        .security(Security::AlwaysFail)
        .start()
        .await
        .unwrap();
    let mut client = Client::connect(server.addr()).await;

    client.version().await;
    assert_eq!(
        client.security_types().await,
        vec![proto::SECURITY_VNC_AUTH]
    );
    client.send(&[proto::SECURITY_VNC_AUTH]).await;
    let challenge: [u8; 16] = client.read_n(16).await.try_into().unwrap();
    // Even a response that would be right under *some* password is refused.
    client
        .send(&proto::vnc_auth_response("anything", &challenge))
        .await;

    assert_ne!(client.security_result().await, proto::SECURITY_RESULT_OK);
    assert!(!client.failure_reason().await.is_empty());

    server.shutdown().await;
}

#[tokio::test]
async fn unsupported_offers_a_type_the_client_cannot_use() {
    let server = FakeServer::builder()
        .security(Security::Unsupported)
        .start()
        .await
        .unwrap();
    let mut client = Client::connect(server.addr()).await;

    client.version().await;
    assert_eq!(client.security_types().await, vec![proto::SECURITY_ARD]);
    // A client that picks something not on offer is told so, rather than hanging.
    client.send(&[proto::SECURITY_NONE]).await;
    assert_ne!(client.security_result().await, proto::SECURITY_RESULT_OK);
    assert!(!client.failure_reason().await.is_empty());

    server.shutdown().await;
}

#[tokio::test]
async fn server_init_carries_the_size_the_name_and_a_decodable_pixel_format() {
    let server = FakeServer::builder()
        .screen(Screen::solid(321, 201, RED))
        .desktop_name("a scripted desktop")
        .start()
        .await
        .unwrap();
    let mut client = Client::connect(server.addr()).await;

    let init = client.open().await;
    assert_eq!((init.width, init.height), (321, 201));
    assert_eq!(init.name, "a scripted desktop");
    // The block round-trips through proto's own decoder, so it is a pixel format and
    // not sixteen plausible bytes.
    assert_eq!(init.format, PixelFormat::RGBX_32);

    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Recording what the client sent
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_client_message_lands_in_events_in_order() {
    let server = FakeServer::builder()
        .screen(Screen::solid(4, 2, RED))
        .encoding(Encoding::Raw)
        .start()
        .await
        .unwrap();
    let mut client = Client::connect(server.addr()).await;
    client.open().await;

    let fmt = PixelFormat::RGBX_32;
    client.send(&proto::set_pixel_format(&fmt)).await;
    client
        .send(&proto::set_encodings(proto::ADVERTISED_ENCODINGS))
        .await;
    client
        .send(&proto::framebuffer_update_request(true, 1, 2, 3, 4))
        .await;
    client.send(&proto::pointer_event(420, 180, 1)).await;
    client.send(&proto::key_event(0xff0d, true)).await;
    client.send(&proto::client_cut_text("héllo")).await;

    server
        .wait_for(|e| matches!(e, ClientEvent::CutText(_)), WAIT)
        .await
        .expect("the cut text should arrive");

    assert_eq!(
        server.events().await,
        vec![
            ClientEvent::SetPixelFormat(fmt),
            ClientEvent::SetEncodings(proto::ADVERTISED_ENCODINGS.to_vec()),
            ClientEvent::UpdateRequest {
                incremental: true,
                x: 1,
                y: 2,
                width: 3,
                height: 4
            },
            ClientEvent::Pointer {
                x: 420,
                y: 180,
                buttons: proto::BUTTON_LEFT
            },
            ClientEvent::Key {
                keysym: 0xff0d,
                down: true
            },
            // Lifted back out of latin-1, so the string reads as the client wrote it.
            ClientEvent::CutText("héllo".to_string()),
        ]
    );

    server.shutdown().await;
}

#[tokio::test]
async fn wait_for_gives_up_rather_than_hanging() {
    let server = FakeServer::builder().start().await.unwrap();
    let mut client = Client::connect(server.addr()).await;
    client.open().await;
    client.send(&proto::pointer_event(1, 1, 0)).await;

    // The event that did happen is found, whether it arrived before or after the call.
    server
        .wait_for(|e| matches!(e, ClientEvent::Pointer { .. }), WAIT)
        .await
        .expect("the pointer event should be recorded");

    let timeout = Duration::from_millis(250);
    let started = std::time::Instant::now();
    let outcome = server
        .wait_for(|e| matches!(e, ClientEvent::Key { .. }), timeout)
        .await;
    let elapsed = started.elapsed();

    assert!(
        matches!(outcome, Err(bolide_testkit::TestkitError::Timeout(t)) if t == timeout),
        "expected a Timeout, got {outcome:?}"
    );
    // Not instant (it really waited) and not forever (it really gave up).
    assert!(
        elapsed >= Duration::from_millis(200),
        "gave up after {elapsed:?}"
    );
    assert!(elapsed < Duration::from_secs(3), "waited {elapsed:?}");

    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Answering update requests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_update_request_gets_a_well_formed_raw_rect() {
    let mut screen = Screen::solid(3, 2, RED);
    screen.fill_rect(2, 1, 1, 1, BLUE);
    let server = FakeServer::builder()
        .screen(screen)
        .encoding(Encoding::Raw)
        .start()
        .await
        .unwrap();
    let mut client = Client::connect(server.addr()).await;
    client.open().await;

    let fmt = PixelFormat::RGBX_32;
    client
        .send(&proto::framebuffer_update_request(false, 0, 0, 3, 2))
        .await;

    let ServerMessage::Update(rects) = client.message(&fmt).await else {
        panic!("expected a FramebufferUpdate");
    };
    assert_eq!(rects.len(), 1);
    let rect = &rects[0];
    assert_eq!((rect.x, rect.y, rect.width, rect.height), (0, 0, 3, 2));
    assert_eq!(rect.encoding, proto::ENC_RAW);
    // 3 × 2 pixels of four bytes, and the scripted blue is where it was painted.
    assert_eq!(rect.payload.len(), 3 * 2 * 4);
    assert_eq!(&rect.payload[20..24], &[0, 0, 255, 0]);

    server.shutdown().await;
}

#[tokio::test]
async fn the_rect_is_written_in_the_pixel_format_the_client_asked_for() {
    let server = FakeServer::builder()
        .screen(Screen::solid(2, 1, RED))
        .encoding(Encoding::Raw)
        .start()
        .await
        .unwrap();
    let mut client = Client::connect(server.addr()).await;
    client.open().await;

    // 16bpp 565 little-endian: red becomes 0xf800, two bytes a pixel.
    let fmt = PixelFormat {
        bits_per_pixel: 16,
        depth: 16,
        big_endian: false,
        true_colour: true,
        red_max: 31,
        green_max: 63,
        blue_max: 31,
        red_shift: 11,
        green_shift: 5,
        blue_shift: 0,
    };
    client.send(&proto::set_pixel_format(&fmt)).await;
    server
        .wait_for(|e| matches!(e, ClientEvent::SetPixelFormat(_)), WAIT)
        .await
        .unwrap();
    client
        .send(&proto::framebuffer_update_request(false, 0, 0, 2, 1))
        .await;

    let ServerMessage::Update(rects) = client.message(&fmt).await else {
        panic!("expected a FramebufferUpdate");
    };
    assert_eq!(
        rects[0].payload,
        vec![0x00, 0xf8, 0x00, 0xf8],
        "the fake must honour SetPixelFormat, not assume RGBA"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn an_update_request_gets_a_well_formed_zrle_rect() {
    let server = FakeServer::builder()
        .screen(Screen::solid(4, 4, GREEN))
        .encoding(Encoding::Zrle)
        .zrle_tiling(ZrleTiling::Solid)
        .start()
        .await
        .unwrap();
    let mut client = Client::connect(server.addr()).await;
    client.open().await;

    let fmt = PixelFormat::RGBX_32;
    client
        .send(&proto::framebuffer_update_request(false, 0, 0, 4, 4))
        .await;

    let ServerMessage::Update(rects) = client.message(&fmt).await else {
        panic!("expected a FramebufferUpdate");
    };
    assert_eq!(rects[0].encoding, proto::ENC_ZRLE);
    // The builder's tiling choice reaches the wire: one solid tile, one CPIXEL.
    let mut inf = flate2::Decompress::new(true);
    assert_eq!(
        common::inflate(&mut inf, rects[0].zrle_stream()),
        vec![1, 0, 255, 0]
    );

    server.shutdown().await;
}

#[tokio::test]
async fn two_zrle_rects_share_one_zlib_stream() {
    let server = FakeServer::builder()
        .screen(Screen::solid(4, 4, GREEN))
        .encoding(Encoding::Zrle)
        .zrle_tiling(ZrleTiling::Solid)
        .start()
        .await
        .unwrap();
    let mut client = Client::connect(server.addr()).await;
    client.open().await;
    let fmt = PixelFormat::RGBX_32;

    client
        .send(&proto::framebuffer_update_request(false, 0, 0, 4, 4))
        .await;
    let ServerMessage::Update(first) = client.message(&fmt).await else {
        panic!("expected a FramebufferUpdate");
    };
    server.control().set_screen(Screen::solid(4, 4, BLUE)).await;
    let ServerMessage::Update(second) = client.message(&fmt).await else {
        panic!("expected a FramebufferUpdate");
    };

    // One inflater across both rects, as a real client keeps one per connection. A
    // server that reset its deflater between rects would break here and nowhere else.
    let mut inf = flate2::Decompress::new(true);
    assert_eq!(
        common::inflate(&mut inf, first[0].zrle_stream()),
        vec![1, 0, 255, 0]
    );
    assert_eq!(
        common::inflate(&mut inf, second[0].zrle_stream()),
        vec![1, 0, 0, 255]
    );

    server.shutdown().await;
}

#[tokio::test]
async fn negotiated_takes_the_clients_first_usable_encoding() {
    let server = FakeServer::builder()
        .screen(Screen::solid(2, 1, RED))
        .start()
        .await
        .unwrap();
    let mut client = Client::connect(server.addr()).await;
    client.open().await;
    let fmt = PixelFormat::RGBX_32;

    // Raw first, ZRLE second: a real server takes the client's order, not its own.
    client
        .send(&proto::set_encodings(&[proto::ENC_RAW, proto::ENC_ZRLE]))
        .await;
    server
        .wait_for(|e| matches!(e, ClientEvent::SetEncodings(_)), WAIT)
        .await
        .unwrap();
    client
        .send(&proto::framebuffer_update_request(false, 0, 0, 2, 1))
        .await;

    let ServerMessage::Update(rects) = client.message(&fmt).await else {
        panic!("expected a FramebufferUpdate");
    };
    assert_eq!(rects[0].encoding, proto::ENC_RAW);

    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// What the server pushes unprompted
// ---------------------------------------------------------------------------

#[tokio::test]
async fn set_screen_pushes_an_update_nobody_asked_for() {
    let server = FakeServer::builder()
        .screen(Screen::solid(2, 1, RED))
        .encoding(Encoding::Raw)
        .start()
        .await
        .unwrap();
    let mut client = Client::connect(server.addr()).await;
    client.open().await;

    server.control().set_screen(Screen::solid(2, 1, BLUE)).await;

    let ServerMessage::Update(rects) = client.message(&PixelFormat::RGBX_32).await else {
        panic!("expected a FramebufferUpdate");
    };
    assert_eq!(rects[0].payload, vec![0, 0, 255, 0, 0, 0, 255, 0]);
    // No FramebufferUpdateRequest was ever sent, and the fake did not invent one.
    assert!(server.events().await.is_empty());

    server.shutdown().await;
}

#[tokio::test]
async fn send_cut_text_is_a_well_formed_server_cut_text() {
    let server = FakeServer::builder().start().await.unwrap();
    let mut client = Client::connect(server.addr()).await;
    client.open().await;

    server.control().send_cut_text("from the desktop").await;

    assert_eq!(
        client.message(&PixelFormat::RGBX_32).await,
        ServerMessage::CutText("from the desktop".to_string())
    );

    server.shutdown().await;
}

#[tokio::test]
async fn send_bell_is_message_two() {
    let server = FakeServer::builder().start().await.unwrap();
    let mut client = Client::connect(server.addr()).await;
    client.open().await;

    server.control().send_bell().await;

    assert_eq!(client.read_n(1).await, vec![proto::server_msg::BELL]);

    server.shutdown().await;
}

#[tokio::test]
async fn resize_sends_a_desktop_size_rect_with_the_new_dimensions() {
    let server = FakeServer::builder()
        .screen(Screen::solid(4, 4, RED))
        .encoding(Encoding::Raw)
        .start()
        .await
        .unwrap();
    let mut client = Client::connect(server.addr()).await;
    client.open().await;
    let fmt = PixelFormat::RGBX_32;

    server.control().resize(800, 600).await;

    let ServerMessage::Update(rects) = client.message(&fmt).await else {
        panic!("expected a FramebufferUpdate");
    };
    assert_eq!(rects.len(), 1);
    assert_eq!(rects[0].encoding, proto::ENC_DESKTOP_SIZE);
    assert_eq!((rects[0].width, rects[0].height), (800, 600));
    assert!(
        rects[0].payload.is_empty(),
        "a pseudo-encoding carries no pixels"
    );

    // And the framebuffer really is the new size: the next update says so.
    client
        .send(&proto::framebuffer_update_request(false, 0, 0, 800, 600))
        .await;
    let ServerMessage::Update(rects) = client.message(&fmt).await else {
        panic!("expected a FramebufferUpdate");
    };
    assert_eq!((rects[0].width, rects[0].height), (800, 600));

    server.shutdown().await;
}

#[tokio::test]
async fn copy_rect_is_four_bytes_and_moves_the_fakes_own_screen() {
    // A one-row scroll of a 1×4 screen: the destination overlaps the source.
    let mut screen = Screen::solid(1, 4, RED);
    screen.fill_rect(0, 1, 1, 1, GREEN);
    screen.fill_rect(0, 2, 1, 1, BLUE);
    let server = FakeServer::builder()
        .screen(screen)
        .encoding(Encoding::Raw)
        .start()
        .await
        .unwrap();
    let mut client = Client::connect(server.addr()).await;
    client.open().await;
    let fmt = PixelFormat::RGBX_32;

    server.control().copy_rect(0, 1, 0, 0, 1, 3).await;

    let ServerMessage::Update(rects) = client.message(&fmt).await else {
        panic!("expected a FramebufferUpdate");
    };
    assert_eq!(rects[0].encoding, proto::ENC_COPY_RECT);
    assert_eq!((rects[0].x, rects[0].y), (0, 0));
    assert_eq!(rects[0].payload, vec![0, 0, 0, 1], "the source x and y");

    // The fake's screen scrolled too, so a client's decode stays comparable to it.
    client
        .send(&proto::framebuffer_update_request(false, 0, 0, 1, 4))
        .await;
    let ServerMessage::Update(rects) = client.message(&fmt).await else {
        panic!("expected a FramebufferUpdate");
    };
    assert_eq!(
        &rects[0].payload[..12],
        &[0, 255, 0, 0, 0, 0, 255, 0, 255, 0, 0, 0]
    );

    server.shutdown().await;
}

#[tokio::test]
async fn drop_connection_closes_the_socket() {
    let server = FakeServer::builder().start().await.unwrap();
    let mut client = Client::connect(server.addr()).await;
    client.open().await;

    server.control().drop_connection().await;

    assert!(client.at_eof().await, "the client should see EOF");

    // And the fake goes back to listening rather than dying with the connection.
    let mut second = Client::connect(server.addr()).await;
    assert_eq!(second.open().await.name, "bolide-testkit");

    server.shutdown().await;
}
