//! An async RFB 3.8 (VNC) client.
//!
//! # The shape of it
//!
//! A [`Session`] is a *live desktop*: it owns a background task that reads the socket
//! forever, keeps one RGBA framebuffer painted, and turns the two things a server says
//! unprompted — the clipboard changed, the bell rang — into [`ServerEvent`]s. Callers
//! hold a cheap, cloneable [`SessionHandle`] and talk to that task over a channel. There
//! is no "poll the socket" method and no borrow of the connection, because a
//! computer-use server has two independent jobs (answer HTTP, keep the screen current)
//! and neither may block the other.
//!
//! ```no_run
//! # async fn demo() -> Result<(), bolide_rfb::Error> {
//! use bolide_rfb::{connect, Config, Session};
//! let session = connect("127.0.0.1:5900", Config::default().with_password("hunter2")).await?;
//! let frame = session.refresh(false, std::time::Duration::from_secs(5)).await?;
//! println!("{}x{} — {} bytes of RGBA", frame.width, frame.height, frame.rgba.len());
//! # Ok(())
//! # }
//! ```
//!
//! # What is tested, and how
//!
//! Nothing in this crate opens a socket to anything real in a test. Two seams make that
//! possible, and both are load-bearing:
//!
//! - [`connect_stream`] takes *any* `AsyncRead + AsyncWrite`, so a unit test drives the
//!   client against `tokio::io::duplex` and a hand-written script of server bytes. That
//!   is where handshake, auth and decoder tests live.
//! - `bolide-testkit` is a real RFB server that happens to be in-process. That is where
//!   the end-to-end tests live — and it depends on [`proto`] rather than restating the
//!   wire, so the fake cannot agree with the client about a protocol neither speaks.

#![deny(missing_docs)]

use std::fmt;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::broadcast;

pub mod keysym;
pub mod proto;

mod client;
mod session;
mod zrle;

pub use session::SessionHandle;

/// A complete screen, RGBA8888, row-major, `width * height * 4` bytes.
///
/// Alpha is always 255 — RFB carries no alpha, and a screenshot that claimed
/// transparency would render as nothing.
#[derive(Clone, PartialEq, Eq)]
pub struct Framebuffer {
    /// Width in pixels.
    pub width: u16,
    /// Height in pixels.
    pub height: u16,
    /// `width * height * 4` bytes, row-major, R,G,B,A.
    pub rgba: Vec<u8>,
}

impl fmt::Debug for Framebuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Framebuffer({}x{}, {} bytes)",
            self.width,
            self.height,
            self.rgba.len()
        )
    }
}

impl Framebuffer {
    /// A black framebuffer of the given size.
    pub fn black(width: u16, height: u16) -> Framebuffer {
        let mut rgba = vec![0u8; width as usize * height as usize * 4];
        for px in rgba.chunks_exact_mut(4) {
            px[3] = 255;
        }
        Framebuffer {
            width,
            height,
            rgba,
        }
    }

    /// The pixel at `(x, y)`, or `None` when it is off-screen.
    pub fn pixel(&self, x: u16, y: u16) -> Option<[u8; 4]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let i = (y as usize * self.width as usize + x as usize) * 4;
        Some([
            self.rgba[i],
            self.rgba[i + 1],
            self.rgba[i + 2],
            self.rgba[i + 3],
        ])
    }
}

/// Something the server said on its own initiative.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerEvent {
    /// A FramebufferUpdate was applied; `generation` counts them from 1.
    Painted {
        /// Monotonic count of applied updates.
        generation: u64,
    },
    /// The remote desktop resized (DesktopSize pseudo-encoding). The framebuffer has
    /// already been re-allocated and is unpainted until the next [`ServerEvent::Painted`].
    Resized {
        /// New width in pixels.
        width: u16,
        /// New height in pixels.
        height: u16,
    },
    /// ServerCutText: the remote clipboard now holds this text.
    CutText(String),
    /// The remote rang the bell.
    Bell,
    /// The session ended. No further events will arrive.
    Disconnected {
        /// Why, in words a CLI can print. `None` for an orderly [`Session::close`].
        reason: Option<String>,
    },
}

/// Everything that can go wrong between a TCP connect and a painted screen.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The socket failed.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// The greeting was not an RFB ProtocolVersion string.
    #[error("not an RFB server: the greeting was not a ProtocolVersion banner")]
    NotRfb,
    /// The server speaks a version bolide does not.
    #[error("the server speaks RFB {major}.{minor}; bolide needs 3.3 or newer")]
    UnsupportedVersion {
        /// Major version the server announced.
        major: u32,
        /// Minor version the server announced.
        minor: u32,
    },
    /// No offered security type is implemented here.
    #[error("the server offers no security type bolide implements (offered: {offered:?})")]
    NoSupportedSecurity {
        /// The security-type bytes the server listed.
        offered: Vec<u8>,
    },
    /// The server wants VNC authentication and no password was supplied.
    ///
    /// Distinct from [`Error::AuthFailed`] on purpose: this one is fixed by supplying a
    /// password, that one by supplying a *different* one, and a CLI should say which.
    #[error("this desktop requires a password (VNC authentication)")]
    PasswordRequired,
    /// The server rejected the credentials.
    #[error("authentication failed: {0}")]
    AuthFailed(String),
    /// The server closed the connection during the handshake, with a reason.
    #[error("the server refused the connection: {0}")]
    Refused(String),
    /// The bytes on the wire did not mean anything.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// A rect arrived in an encoding bolide never advertised.
    #[error("the server used encoding {0}, which bolide did not advertise")]
    UnsupportedEncoding(i32),
    /// The server owed us bytes and did not send them.
    ///
    /// **Retryable, and named so a caller can say so.** A TCP dial can be accepted by
    /// something that is not yet an RFB server — docker's userland proxy accepts a
    /// published port before the container's VNC server is listening — and the greeting
    /// then never arrives. That is worth redialling; an auth failure is not.
    #[error("timed out after {0:?} waiting for the server")]
    Timeout(Duration),
    /// The session task is gone.
    #[error("the session is closed")]
    Closed,
}

impl Error {
    /// Whether redialling could plausibly succeed. See [`Error::Timeout`].
    pub fn is_retryable(&self) -> bool {
        matches!(self, Error::Timeout(_) | Error::Io(_))
    }
}

/// `Result` with this crate's [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

/// How to connect.
///
/// `Debug` redacts the password. That is not politeness: a `Config` reaches a
/// `tracing` field or an `anyhow` chain by accident far more easily than it reaches a
/// deliberate `println!`, and a password in a log file is a password on disk.
#[derive(Clone)]
pub struct Config {
    /// Carried for auth schemes that take one. Classic VNC authentication has no
    /// username; supplying one is not an error, it is simply unused.
    pub username: Option<String>,
    /// The VNC password, if the desktop wants one.
    pub password: Option<String>,
    /// ClientInit's shared flag: let other viewers stay connected. Default `true` —
    /// bolide is an *extra* pair of hands on a desktop somebody may be watching.
    pub shared: bool,
    /// How long to wait for bytes the server owes us. Bounds the handshake and each
    /// blocking read. Not a heartbeat: silence on an idle screen is not an error.
    pub timeout: Duration,
    /// Keep asking for incremental updates so the framebuffer stays current without a
    /// caller having to ask. Default `true`.
    pub continuous: bool,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            username: None,
            password: None,
            shared: true,
            timeout: Duration::from_secs(10),
            continuous: true,
        }
    }
}

impl Config {
    /// Builder: set the password.
    pub fn with_password(mut self, password: impl Into<String>) -> Config {
        self.password = Some(password.into());
        self
    }

    /// Builder: set the username.
    pub fn with_username(mut self, username: impl Into<String>) -> Config {
        self.username = Some(username.into());
        self
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("shared", &self.shared)
            .field("timeout", &self.timeout)
            .field("continuous", &self.continuous)
            .finish()
    }
}

/// A live desktop.
///
/// Object-safe on purpose: `bolide-server` holds a `dyn Session`, so its handlers can be
/// tested against a recording fake without an RFB connection anywhere in sight.
#[async_trait::async_trait]
pub trait Session: Send + Sync {
    /// Current framebuffer size. Changes on a [`ServerEvent::Resized`].
    fn size(&self) -> (u16, u16);

    /// The desktop name from ServerInit.
    fn desktop_name(&self) -> String;

    /// Whether at least one FramebufferUpdate has been applied since connect (or since
    /// the last resize).
    ///
    /// The framebuffer is allocated black and painted later, so between ServerInit and
    /// the first update `framebuffer()` is a *black screen at exactly screen
    /// dimensions* — which is indistinguishable from a real black screen and has been
    /// handed to a model as one. Anything that presents a frame as the truth must check
    /// this and wait.
    fn painted(&self) -> bool;

    /// A snapshot of the framebuffer right now, painted or not.
    async fn framebuffer(&self) -> Result<Framebuffer>;

    /// Request an update and wait until one has been applied, then return the frame.
    ///
    /// `incremental: false` asks for the whole screen — the right call when the frame
    /// is stale or unpainted. `incremental: true` asks for what changed, and returns as
    /// soon as anything does; on a still screen it will hit `timeout`, which is not an
    /// error condition callers should treat as fatal.
    async fn refresh(&self, incremental: bool, timeout: Duration) -> Result<Framebuffer>;

    /// Send a PointerEvent: move to `(x, y)` holding `buttons` (see [`proto::BUTTON_LEFT`]).
    async fn pointer(&self, x: u16, y: u16, buttons: u8) -> Result<()>;

    /// Send a KeyEvent.
    async fn key(&self, keysym: u32, down: bool) -> Result<()>;

    /// Send ClientCutText: replace the remote clipboard.
    async fn cut_text(&self, text: &str) -> Result<()>;

    /// The most recent ServerCutText, if the desktop has sent one.
    ///
    /// RFB clipboards are push-only: there is no "read the remote clipboard" message,
    /// so this is a cache of what the server volunteered. A caller that needs the
    /// *current* remote selection has to make the desktop copy something first.
    fn last_cut_text(&self) -> Option<String>;

    /// Subscribe to [`ServerEvent`]s. Each subscriber gets its own receiver; a slow one
    /// lags rather than blocking the session.
    fn events(&self) -> broadcast::Receiver<ServerEvent>;

    /// Whether the session task is still running.
    fn is_connected(&self) -> bool;

    /// Close the connection. Idempotent.
    async fn close(&self) -> Result<()>;
}

/// Connect over TCP to `addr` (`"host:port"`).
pub async fn connect(addr: &str, config: Config) -> Result<SessionHandle> {
    client::connect_tcp(addr, config).await
}

/// Connect over an already-open byte stream.
///
/// The seam every offline test uses: pass a `tokio::io::duplex` half and script the
/// server side by hand.
pub async fn connect_stream<S>(stream: S, config: Config) -> Result<SessionHandle>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    client::connect_stream(stream, config).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_debug_never_prints_the_password() {
        let rendered = format!("{:?}", Config::default().with_password("hunter2"));
        assert!(!rendered.contains("hunter2"), "leaked: {rendered}");
        assert!(rendered.contains("redacted"), "unhelpful: {rendered}");
    }

    #[test]
    fn a_black_framebuffer_is_opaque() {
        let fb = Framebuffer::black(2, 2);
        assert_eq!(fb.rgba.len(), 16);
        assert_eq!(fb.pixel(1, 1), Some([0, 0, 0, 255]));
        assert_eq!(fb.pixel(2, 0), None);
    }
}
