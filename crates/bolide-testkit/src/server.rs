//! The fake server itself: a real RFB 3.8 server on loopback that happens to live in
//! the test process. See the crate docs for why.
//!
//! Every constant and fixed-shape message comes from [`bolide_rfb::proto`]. Nothing about
//! the wire is spelled out twice here — a fake with its own idea of the protocol would
//! agree with bolide's client and disagree with TigerVNC.
//!
//! # Shape
//!
//! One task owns the listener, the screen and the connection. It selects between two
//! sources: messages the reader task lifted off the socket, and [`Directive`]s a test
//! pushed through [`ServerControl`]. The socket is *split* rather than selected on
//! directly, because a half-read message dropped by a cancelled `select!` branch is
//! silent corruption; the reader task's channel is cancel-safe, a `TcpStream` read is
//! not.
//!
//! Recording happens in the reader task, before the message is forwarded, so
//! [`FakeServer::events`] and [`FakeServer::wait_for`] see a client message whether or
//! not it needed an answer.

use std::net::SocketAddr;
use std::ops::ControlFlow;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bolide_rfb::proto::{self, client_msg, server_msg, PixelFormat};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc};

use crate::encode::{copy_rect, raw_rect, ZrleWriter};
use crate::{ClientEvent, Encoding, Result, Screen, Security, TestkitError, ZrleTiling};

/// The VNC-auth challenge the fake sends. A constant rather than random bytes: a test
/// that fails should fail the same way twice, and the client proves it can do the DES
/// transform, not that it can do it over an unpredictable input.
const CHALLENGE: [u8; 16] = [
    0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
];

/// The reason string a failed SecurityResult carries. RFB 3.8 sends one; 3.7 does not.
const AUTH_FAILED: &str = "authentication failed";

const BLACK: [u8; 4] = [0, 0, 0, 255];

/// A running fake RFB server on loopback.
pub struct FakeServer {
    pub(crate) addr: SocketAddr,
    shared: Arc<Shared>,
    control: ServerControl,
    task: Option<tokio::task::JoinHandle<()>>,
}

/// Everything a test can look at while the server runs.
pub(crate) struct Shared {
    events: Mutex<Vec<ClientEvent>>,
    /// Kept alive for the life of the [`FakeServer`], so [`FakeServer::wait_for`] can
    /// never see a closed channel and return early instead of timing out.
    announce: broadcast::Sender<ClientEvent>,
}

impl Shared {
    fn record(&self, event: ClientEvent) {
        self.events
            .lock()
            .expect("the event log is never held across a panic")
            .push(event.clone());
        let _ = self.announce.send(event);
    }
}

/// Configure a [`FakeServer`] before starting it.
#[derive(Clone, Debug)]
pub struct FakeServerBuilder {
    pub(crate) security: Security,
    pub(crate) encoding: Encoding,
    pub(crate) screen: Screen,
    pub(crate) desktop_name: String,
    pub(crate) zrle_tiling: ZrleTiling,
}

impl Default for FakeServerBuilder {
    fn default() -> FakeServerBuilder {
        FakeServerBuilder {
            security: Security::default(),
            encoding: Encoding::default(),
            screen: Screen::solid(640, 480, BLACK),
            desktop_name: "bolide-testkit".to_string(),
            zrle_tiling: ZrleTiling::default(),
        }
    }
}

/// Drive a running fake from a test: change the screen, push clipboard, ring, resize,
/// drop the connection. Cloneable and `Send`.
#[derive(Clone)]
pub struct ServerControl {
    pub(crate) tx: mpsc::UnboundedSender<Directive>,
}

/// Something a test told the fake to do. Implementation detail.
#[derive(Debug)]
pub(crate) enum Directive {
    SetScreen(Box<Screen>),
    CutText(String),
    Bell,
    Resize(u16, u16),
    CopyRect {
        src_x: u16,
        src_y: u16,
        dst_x: u16,
        dst_y: u16,
        width: u16,
        height: u16,
    },
    Drop,
    Shutdown,
}

impl ServerControl {
    /// Replace the screen and push a FramebufferUpdate covering what changed.
    pub async fn set_screen(&self, screen: Screen) {
        let _ = self.tx.send(Directive::SetScreen(Box::new(screen)));
    }

    /// Send ServerCutText.
    pub async fn send_cut_text(&self, text: &str) {
        let _ = self.tx.send(Directive::CutText(text.to_string()));
    }

    /// Ring the bell.
    pub async fn send_bell(&self) {
        let _ = self.tx.send(Directive::Bell);
    }

    /// Resize via the DesktopSize pseudo-encoding.
    ///
    /// The new framebuffer is black: a real server repaints after a resize and a client
    /// is expected to ask for an update, so inventing content here would let a test
    /// assert on pixels nobody sent.
    pub async fn resize(&self, width: u16, height: u16) {
        let _ = self.tx.send(Directive::Resize(width, height));
    }

    /// Copy a region of the framebuffer somewhere else and tell the client with a
    /// CopyRect rect — a scroll, a window drag, a menu closing.
    ///
    /// The fake's own screen moves too, so "what the client decoded" and "what the
    /// server is showing" stay comparable. Overlapping source and destination is the
    /// interesting case and is handled: the copy reads a snapshot.
    pub async fn copy_rect(
        &self,
        src_x: u16,
        src_y: u16,
        dst_x: u16,
        dst_y: u16,
        width: u16,
        height: u16,
    ) {
        let _ = self.tx.send(Directive::CopyRect {
            src_x,
            src_y,
            dst_x,
            dst_y,
            width,
            height,
        });
    }

    /// Drop the client's connection mid-session, so its disconnect path is reachable.
    pub async fn drop_connection(&self) {
        let _ = self.tx.send(Directive::Drop);
    }
}

impl FakeServer {
    /// Start configuring.
    pub fn builder() -> FakeServerBuilder {
        FakeServerBuilder::default()
    }

    /// The loopback address it bound.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// A handle for pushing things at the client.
    pub fn control(&self) -> ServerControl {
        self.control.clone()
    }

    /// Everything the client has sent so far, in order.
    pub async fn events(&self) -> Vec<ClientEvent> {
        self.shared
            .events
            .lock()
            .expect("the event log is never held across a panic")
            .clone()
    }

    /// Wait until a recorded event matches, or give up.
    ///
    /// Takes a timeout because a hung test on CI is indistinguishable from a dead
    /// runner, and "the click never arrived" should read as a failed assertion.
    ///
    /// Events already recorded count: subscribing *before* scanning the log is what
    /// keeps a message that lands between the two from being missed.
    pub async fn wait_for<F>(&self, pred: F, timeout: Duration) -> Result<ClientEvent>
    where
        F: Fn(&ClientEvent) -> bool + Send,
    {
        let mut rx = self.shared.announce.subscribe();
        if let Some(found) = self
            .shared
            .events
            .lock()
            .expect("the event log is never held across a panic")
            .iter()
            .find(|e| pred(e))
            .cloned()
        {
            return Ok(found);
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Err(_) => return Err(TestkitError::Timeout(timeout)),
                Ok(Ok(event)) if pred(&event) => return Ok(event),
                Ok(Ok(_)) => continue,
                // Lagged: the log is still authoritative, so re-scan it.
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => {
                    if let Some(found) = self
                        .shared
                        .events
                        .lock()
                        .expect("the event log is never held across a panic")
                        .iter()
                        .find(|e| pred(e))
                        .cloned()
                    {
                        return Ok(found);
                    }
                }
                // Unreachable while `self` holds the sender, and a timeout is the
                // honest answer if it ever happens.
                Ok(Err(broadcast::error::RecvError::Closed)) => {
                    return Err(TestkitError::Timeout(timeout))
                }
            }
        }
    }

    /// Stop the server and its connection tasks.
    pub async fn shutdown(self) {
        let mut this = self;
        let _ = this.control.tx.send(Directive::Shutdown);
        if let Some(task) = this.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        // A test that forgets `shutdown()` still stops the server rather than leaving
        // a task holding a socket for the rest of the run.
        let _ = self.control.tx.send(Directive::Shutdown);
    }
}

impl FakeServerBuilder {
    /// The screen the fake shows.
    pub fn screen(mut self, screen: Screen) -> FakeServerBuilder {
        self.screen = screen;
        self
    }

    /// Require this password (security type VncAuth).
    pub fn password(mut self, password: &str) -> FakeServerBuilder {
        self.security = Security::VncAuth(password.to_string());
        self
    }

    /// Pick the security behaviour outright.
    pub fn security(mut self, security: Security) -> FakeServerBuilder {
        self.security = security;
        self
    }

    /// Force an encoding rather than honouring the client's preference.
    pub fn encoding(mut self, encoding: Encoding) -> FakeServerBuilder {
        self.encoding = encoding;
        self
    }

    /// Force one ZRLE tile subencoding, so a test can aim at one decoder path.
    pub fn zrle_tiling(mut self, tiling: ZrleTiling) -> FakeServerBuilder {
        self.zrle_tiling = tiling;
        self
    }

    /// ServerInit's desktop name.
    pub fn desktop_name(mut self, name: &str) -> FakeServerBuilder {
        self.desktop_name = name.to_string();
        self
    }

    /// Bind `127.0.0.1:0` and start serving.
    pub async fn start(self) -> Result<FakeServer> {
        let listener = TcpListener::bind(crate::loopback_any()).await?;
        let addr = listener.local_addr()?;
        let (announce, _) = broadcast::channel(256);
        let shared = Arc::new(Shared {
            events: Mutex::new(Vec::new()),
            announce,
        });
        let (tx, rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(run(listener, self, Arc::clone(&shared), rx));
        Ok(FakeServer {
            addr,
            shared,
            control: ServerControl { tx },
            task: Some(task),
        })
    }
}

// ---------------------------------------------------------------------------
// The server task
// ---------------------------------------------------------------------------

async fn run(
    listener: TcpListener,
    cfg: FakeServerBuilder,
    shared: Arc<Shared>,
    mut rx: mpsc::UnboundedReceiver<Directive>,
) {
    let mut screen = cfg.screen.clone();
    loop {
        // Between connections, directives that only change state still apply; the ones
        // that push bytes have nobody to push them at.
        let stream = loop {
            tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => break stream,
                    Err(_) => continue,
                },
                directive = rx.recv() => match directive {
                    None | Some(Directive::Shutdown) => return,
                    Some(other) => apply_offline(&mut screen, other),
                },
            }
        };
        if serve(stream, &cfg, &shared, &mut rx, &mut screen)
            .await
            .is_break()
        {
            return;
        }
    }
}

/// The state-changing half of a directive, for when no client is connected.
fn apply_offline(screen: &mut Screen, directive: Directive) {
    match directive {
        Directive::SetScreen(new) => *screen = *new,
        Directive::Resize(w, h) => *screen = Screen::solid(w, h, BLACK),
        Directive::CopyRect {
            src_x,
            src_y,
            dst_x,
            dst_y,
            width,
            height,
        } => blit(screen, src_x, src_y, dst_x, dst_y, width, height),
        _ => {}
    }
}

/// How the opening exchange ended.
enum Setup {
    /// Handshake and ServerInit done; the session can start.
    Ready,
    /// The client went away, or was told to.
    Closed,
    /// The whole server was told to stop.
    Stop,
}

/// Serve one connection. `Break` means the whole server was told to stop.
async fn serve(
    stream: TcpStream,
    cfg: &FakeServerBuilder,
    shared: &Arc<Shared>,
    rx: &mut mpsc::UnboundedReceiver<Directive>,
    screen: &mut Screen,
) -> ControlFlow<()> {
    let (mut read, mut write) = stream.into_split();
    // ServerInit is fixed at connect time, the way a real server's is, which also keeps
    // the setup future from borrowing the screen a directive may be changing.
    let init = server_init(screen, &cfg.desktop_name);

    // The opening exchange has to race the shutdown signal. A client that answers the
    // banner and then goes quiet would otherwise leave this task blocked on a read
    // forever, and `FakeServer::shutdown` awaits this task — so one incurious test
    // would hang the whole run.
    let status = {
        let setup = async {
            handshake(&mut read, &mut write, cfg).await?;
            // ClientInit is one byte: the shared flag.
            let mut shared_flag = [0u8; 1];
            read.read_exact(&mut shared_flag).await?;
            write.write_all(&init).await
        };
        tokio::pin!(setup);
        loop {
            tokio::select! {
                result = &mut setup => break match result {
                    Ok(()) => Setup::Ready,
                    Err(_) => Setup::Closed,
                },
                directive = rx.recv() => match directive {
                    None | Some(Directive::Shutdown) => break Setup::Stop,
                    Some(Directive::Drop) => break Setup::Closed,
                    Some(other) => apply_offline(screen, other),
                },
            }
        }
    };
    match status {
        Setup::Stop => return ControlFlow::Break(()),
        Setup::Closed => return ControlFlow::Continue(()),
        Setup::Ready => {}
    }

    let (msg_tx, mut msg_rx) = mpsc::unbounded_channel();
    let reader = tokio::spawn(read_loop(read, Arc::clone(shared), msg_tx));

    let mut fmt = PixelFormat::RGBX_32;
    let mut advertised: Vec<i32> = Vec::new();
    let mut zrle = ZrleWriter::with_tiling(cfg.zrle_tiling);

    let outcome = loop {
        let write_result = tokio::select! {
            message = msg_rx.recv() => match message {
                None => break ControlFlow::Continue(()),
                Some(ClientEvent::SetPixelFormat(f)) => { fmt = f; Ok(()) }
                Some(ClientEvent::SetEncodings(list)) => { advertised = list; Ok(()) }
                Some(ClientEvent::UpdateRequest { .. }) => {
                    let update = framebuffer_update(screen, &fmt, cfg, &advertised, &mut zrle);
                    write.write_all(&update).await
                }
                Some(_) => Ok(()),
            },
            directive = rx.recv() => match directive {
                None | Some(Directive::Shutdown) => break ControlFlow::Break(()),
                Some(Directive::Drop) => break ControlFlow::Continue(()),
                Some(directive) => {
                    apply_online(directive, screen, &fmt, cfg, &advertised, &mut zrle, &mut write).await
                }
            },
        };
        if write_result.is_err() {
            break ControlFlow::Continue(());
        }
    };

    // Dropping the write half alone leaves the reader holding the socket open, so a
    // `drop_connection` would not actually reach the client as a close.
    let _ = write.shutdown().await;
    reader.abort();
    outcome
}

/// Apply a directive to a live connection, writing whatever it owes the client.
#[allow(clippy::too_many_arguments)]
async fn apply_online(
    directive: Directive,
    screen: &mut Screen,
    fmt: &PixelFormat,
    cfg: &FakeServerBuilder,
    advertised: &[i32],
    zrle: &mut ZrleWriter,
    write: &mut OwnedWriteHalf,
) -> std::io::Result<()> {
    match directive {
        Directive::SetScreen(new) => {
            *screen = *new;
            let update = framebuffer_update(screen, fmt, cfg, advertised, zrle);
            write.write_all(&update).await
        }
        Directive::CutText(text) => write.write_all(&server_cut_text(&text)).await,
        Directive::Bell => write.write_all(&[server_msg::BELL]).await,
        Directive::Resize(w, h) => {
            *screen = Screen::solid(w, h, BLACK);
            let mut out = update_header(1);
            out.extend_from_slice(&rect_header(0, 0, w, h, proto::ENC_DESKTOP_SIZE));
            write.write_all(&out).await
        }
        Directive::CopyRect {
            src_x,
            src_y,
            dst_x,
            dst_y,
            width,
            height,
        } => {
            blit(screen, src_x, src_y, dst_x, dst_y, width, height);
            let mut out = update_header(1);
            out.extend_from_slice(&rect_header(
                dst_x,
                dst_y,
                width,
                height,
                proto::ENC_COPY_RECT,
            ));
            out.extend_from_slice(&copy_rect(src_x, src_y));
            write.write_all(&out).await
        }
        // Handled by the caller: they end the connection rather than write to it.
        Directive::Drop | Directive::Shutdown => Ok(()),
    }
}

/// Move a region of the screen, reading a snapshot so an overlapping copy — which is
/// what a scroll *is* — does not smear its own source.
fn blit(screen: &mut Screen, src_x: u16, src_y: u16, dst_x: u16, dst_y: u16, w: u16, h: u16) {
    let mut taken = Vec::with_capacity(w as usize * h as usize);
    for row in 0..h {
        for col in 0..w {
            taken.push(
                screen
                    .pixel(src_x.saturating_add(col), src_y.saturating_add(row))
                    .unwrap_or(BLACK),
            );
        }
    }
    for row in 0..h {
        for col in 0..w {
            let rgba = taken[row as usize * w as usize + col as usize];
            screen.fill_rect(
                dst_x.saturating_add(col),
                dst_y.saturating_add(row),
                1,
                1,
                rgba,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Handshake
// ---------------------------------------------------------------------------

async fn handshake(
    read: &mut OwnedReadHalf,
    write: &mut OwnedWriteHalf,
    cfg: &FakeServerBuilder,
) -> std::io::Result<()> {
    write.write_all(proto::VERSION_3_8).await?;
    let mut version = [0u8; 12];
    read.read_exact(&mut version).await?;

    let offered: &[u8] = match cfg.security {
        Security::None => &[proto::SECURITY_NONE],
        Security::VncAuth(_) | Security::AlwaysFail => &[proto::SECURITY_VNC_AUTH],
        // A type bolide does not implement, so its "no security type in common" path is
        // reachable without inventing a number.
        Security::Unsupported => &[proto::SECURITY_ARD],
    };
    let mut list = vec![offered.len() as u8];
    list.extend_from_slice(offered);
    write.write_all(&list).await?;

    let mut chosen = [0u8; 1];
    read.read_exact(&mut chosen).await?;
    if !offered.contains(&chosen[0]) {
        return fail_security(write, "no security type in common").await;
    }

    match &cfg.security {
        Security::None => {
            write
                .write_all(&proto::SECURITY_RESULT_OK.to_be_bytes())
                .await?;
            Ok(())
        }
        Security::Unsupported => fail_security(write, "no security type in common").await,
        Security::VncAuth(password) => {
            write.write_all(&CHALLENGE).await?;
            let mut response = [0u8; 16];
            read.read_exact(&mut response).await?;
            if response == proto::vnc_auth_response(password, &CHALLENGE) {
                write
                    .write_all(&proto::SECURITY_RESULT_OK.to_be_bytes())
                    .await?;
                Ok(())
            } else {
                fail_security(write, AUTH_FAILED).await
            }
        }
        Security::AlwaysFail => {
            write.write_all(&CHALLENGE).await?;
            let mut response = [0u8; 16];
            read.read_exact(&mut response).await?;
            fail_security(write, AUTH_FAILED).await
        }
    }
}

/// A non-zero SecurityResult with the 3.8 reason string, then an error so the caller
/// drops the connection.
async fn fail_security(write: &mut OwnedWriteHalf, reason: &str) -> std::io::Result<()> {
    let mut out = 1u32.to_be_bytes().to_vec();
    out.extend_from_slice(&(reason.len() as u32).to_be_bytes());
    out.extend_from_slice(reason.as_bytes());
    write.write_all(&out).await?;
    let _ = write.shutdown().await;
    Err(std::io::Error::other(reason.to_string()))
}

fn server_init(screen: &Screen, name: &str) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&screen.width.to_be_bytes());
    out.extend_from_slice(&screen.height.to_be_bytes());
    out.extend_from_slice(&PixelFormat::RGBX_32.encode());
    out.extend_from_slice(&(name.len() as u32).to_be_bytes());
    out.extend_from_slice(name.as_bytes());
    out
}

// ---------------------------------------------------------------------------
// Reading the client
// ---------------------------------------------------------------------------

/// Lift client messages off the socket and record each one before forwarding it.
///
/// Its own task, so the serve loop can `select!` on a cancel-safe channel: a partially
/// read message dropped by a cancelled read would desynchronise the stream silently.
async fn read_loop(
    mut read: OwnedReadHalf,
    shared: Arc<Shared>,
    tx: mpsc::UnboundedSender<ClientEvent>,
) {
    loop {
        let event = match read_message(&mut read).await {
            Ok(event) => event,
            Err(_) => return,
        };
        shared.record(event.clone());
        if tx.send(event).is_err() {
            return;
        }
    }
}

async fn read_message(read: &mut OwnedReadHalf) -> std::io::Result<ClientEvent> {
    let mut kind = [0u8; 1];
    read.read_exact(&mut kind).await?;
    match kind[0] {
        client_msg::SET_PIXEL_FORMAT => {
            let mut rest = [0u8; 19];
            read.read_exact(&mut rest).await?;
            let mut block = [0u8; 16];
            block.copy_from_slice(&rest[3..]);
            Ok(ClientEvent::SetPixelFormat(PixelFormat::decode(&block)))
        }
        client_msg::SET_ENCODINGS => {
            let mut head = [0u8; 3];
            read.read_exact(&mut head).await?;
            let count = u16::from_be_bytes([head[1], head[2]]) as usize;
            let mut body = vec![0u8; count * 4];
            read.read_exact(&mut body).await?;
            Ok(ClientEvent::SetEncodings(
                body.chunks_exact(4)
                    .map(|c| i32::from_be_bytes([c[0], c[1], c[2], c[3]]))
                    .collect(),
            ))
        }
        client_msg::FRAMEBUFFER_UPDATE_REQUEST => {
            let mut rest = [0u8; 9];
            read.read_exact(&mut rest).await?;
            Ok(ClientEvent::UpdateRequest {
                incremental: rest[0] != 0,
                x: u16::from_be_bytes([rest[1], rest[2]]),
                y: u16::from_be_bytes([rest[3], rest[4]]),
                width: u16::from_be_bytes([rest[5], rest[6]]),
                height: u16::from_be_bytes([rest[7], rest[8]]),
            })
        }
        client_msg::KEY_EVENT => {
            let mut rest = [0u8; 7];
            read.read_exact(&mut rest).await?;
            Ok(ClientEvent::Key {
                down: rest[0] != 0,
                keysym: u32::from_be_bytes([rest[3], rest[4], rest[5], rest[6]]),
            })
        }
        client_msg::POINTER_EVENT => {
            let mut rest = [0u8; 5];
            read.read_exact(&mut rest).await?;
            Ok(ClientEvent::Pointer {
                buttons: rest[0],
                x: u16::from_be_bytes([rest[1], rest[2]]),
                y: u16::from_be_bytes([rest[3], rest[4]]),
            })
        }
        client_msg::CLIENT_CUT_TEXT => {
            let mut head = [0u8; 7];
            read.read_exact(&mut head).await?;
            let len = u32::from_be_bytes([head[3], head[4], head[5], head[6]]) as usize;
            let mut body = vec![0u8; len];
            read.read_exact(&mut body).await?;
            Ok(ClientEvent::CutText(proto::from_latin1(&body)))
        }
        other => Err(std::io::Error::other(
            TestkitError::Protocol(format!("client message type {other}")).to_string(),
        )),
    }
}

// ---------------------------------------------------------------------------
// Writing the screen
// ---------------------------------------------------------------------------

fn update_header(rects: u16) -> Vec<u8> {
    let mut out = vec![server_msg::FRAMEBUFFER_UPDATE, 0];
    out.extend_from_slice(&rects.to_be_bytes());
    out
}

fn rect_header(x: u16, y: u16, w: u16, h: u16, encoding: i32) -> Vec<u8> {
    let mut out = Vec::with_capacity(12);
    out.extend_from_slice(&x.to_be_bytes());
    out.extend_from_slice(&y.to_be_bytes());
    out.extend_from_slice(&w.to_be_bytes());
    out.extend_from_slice(&h.to_be_bytes());
    out.extend_from_slice(&encoding.to_be_bytes());
    out
}

fn server_cut_text(text: &str) -> Vec<u8> {
    let latin1 = proto::to_latin1(text);
    let mut out = vec![server_msg::SERVER_CUT_TEXT, 0, 0, 0];
    out.extend_from_slice(&(latin1.len() as u32).to_be_bytes());
    out.extend_from_slice(&latin1);
    out
}

/// A whole FramebufferUpdate carrying the current screen as one rect.
///
/// The fake always answers with the *whole* screen rather than the requested region:
/// a real server may send more than was asked for, and a test that scripted a screen
/// wants to see that screen rather than reason about clipping.
fn framebuffer_update(
    screen: &Screen,
    fmt: &PixelFormat,
    cfg: &FakeServerBuilder,
    advertised: &[i32],
    zrle: &mut ZrleWriter,
) -> Vec<u8> {
    let encoding = choose_encoding(cfg.encoding, advertised);
    let payload = match encoding {
        proto::ENC_ZRLE => zrle.rect(screen, 0, 0, screen.width, screen.height, fmt),
        _ => raw_rect(screen, 0, 0, screen.width, screen.height, fmt),
    };
    let mut out = update_header(1);
    out.extend_from_slice(&rect_header(0, 0, screen.width, screen.height, encoding));
    out.extend_from_slice(&payload);
    out
}

/// Which encoding a rect goes out in.
///
/// `Negotiated` takes the client's list in order, the way a real server does, and skips
/// what cannot paint a fresh screen: CopyRect needs a source the client already has, and
/// DesktopSize is a pseudo-encoding. Raw is the floor when the client advertised nothing
/// usable, because RFB requires every client to understand it.
fn choose_encoding(encoding: Encoding, advertised: &[i32]) -> i32 {
    match encoding {
        Encoding::Raw => proto::ENC_RAW,
        Encoding::Zrle => proto::ENC_ZRLE,
        Encoding::Negotiated => advertised
            .iter()
            .copied()
            .find(|e| *e == proto::ENC_ZRLE || *e == proto::ENC_RAW)
            .unwrap_or(proto::ENC_RAW),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiation_takes_the_clients_first_usable_encoding() {
        // bolide's own list: ZRLE leads, and CopyRect/DesktopSize cannot paint a screen.
        assert_eq!(
            choose_encoding(Encoding::Negotiated, proto::ADVERTISED_ENCODINGS),
            proto::ENC_ZRLE
        );
        assert_eq!(
            choose_encoding(
                Encoding::Negotiated,
                &[proto::ENC_COPY_RECT, proto::ENC_RAW, proto::ENC_ZRLE]
            ),
            proto::ENC_RAW
        );
        // Nothing usable advertised, or nothing advertised at all → Raw, the floor.
        assert_eq!(
            choose_encoding(Encoding::Negotiated, &[proto::ENC_DESKTOP_SIZE]),
            proto::ENC_RAW
        );
        assert_eq!(choose_encoding(Encoding::Negotiated, &[]), proto::ENC_RAW);
        // An explicit choice ignores the client entirely.
        assert_eq!(
            choose_encoding(Encoding::Zrle, &[proto::ENC_RAW]),
            proto::ENC_ZRLE
        );
        assert_eq!(
            choose_encoding(Encoding::Raw, &[proto::ENC_ZRLE]),
            proto::ENC_RAW
        );
    }

    #[test]
    fn an_overlapping_blit_does_not_smear_its_own_source() {
        // A one-row scroll: rows 1..3 move up to 0..2, so the destination overlaps the
        // source. Reading a snapshot is what keeps row 1 from being row 0 repeated.
        let mut s = Screen::solid(1, 4, [0, 0, 0, 255]);
        s.fill_rect(0, 1, 1, 1, [1, 1, 1, 255]);
        s.fill_rect(0, 2, 1, 1, [2, 2, 2, 255]);
        s.fill_rect(0, 3, 1, 1, [3, 3, 3, 255]);
        blit(&mut s, 0, 1, 0, 0, 1, 3);
        assert_eq!(s.pixel(0, 0), Some([1, 1, 1, 255]));
        assert_eq!(s.pixel(0, 1), Some([2, 2, 2, 255]));
        assert_eq!(s.pixel(0, 2), Some([3, 3, 3, 255]));
    }

    #[test]
    fn server_cut_text_is_latin1_with_a_u32_length() {
        // "hé" is two latin-1 bytes, not the three UTF-8 ones — the length counts what
        // goes on the wire.
        assert_eq!(
            server_cut_text("hé"),
            vec![server_msg::SERVER_CUT_TEXT, 0, 0, 0, 0, 0, 0, 2, b'h', 0xe9]
        );
    }
}
