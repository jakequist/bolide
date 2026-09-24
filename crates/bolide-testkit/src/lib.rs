//! An in-process RFB server, so every bolide test runs offline.
//!
//! CONTRACT — `bolide-testkit`'s to finish. The public shape below is fixed; the crates
//! that consume it are already written against it.
//!
//! # Why this exists
//!
//! bolide's whole job is to talk to somebody else's desktop, so almost everything worth
//! testing is "what happens on the wire". A test that needs a real VNC server is a test
//! that does not run in CI, does not run on a plane, and fails for reasons that have
//! nothing to do with the change. So the fake is a *real* RFB server — it speaks the
//! protocol over a real TCP socket on `127.0.0.1:0` — that happens to live in the test
//! process, show a scripted screen, and **write down every event the client sends**.
//!
//! It depends on [`bolide_rfb::proto`] rather than restating the wire. That is the point:
//! a fake with its own idea of the protocol would let the client and the fake agree
//! with each other and disagree with TigerVNC, and every test would be green.
//!
//! # Shape
//!
//! ```no_run
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! use bolide_testkit::{FakeServer, Screen};
//! let server = FakeServer::builder()
//!     .screen(Screen::solid(320, 200, [255, 0, 0, 255]))
//!     .password("hunter2")
//!     .start()
//!     .await?;
//! let addr = server.addr();
//! // … drive a client at `addr` …
//! server.control().send_cut_text("from the desktop").await;
//! assert!(!server.events().await.is_empty());
//! server.shutdown().await;
//! # Ok(())
//! # }
//! ```
//!
//! # What it must do
//!
//! - **Handshake**: announce 3.8; offer `None` when no password is configured and
//!   `VncAuth` when one is; verify the client's DES response with
//!   [`bolide_rfb::proto::vnc_auth_response`] and fail the SecurityResult (with a 3.8
//!   reason string) when it is wrong. A builder switch forces a failure so the client's
//!   error paths are reachable.
//! - **Record everything**: SetPixelFormat, SetEncodings, every
//!   FramebufferUpdateRequest, PointerEvent, KeyEvent and ClientCutText, in order, as
//!   [`ClientEvent`]s.
//! - **Answer update requests** with the current [`Screen`], in the encoding the
//!   builder selected, honouring the pixel format the client asked for. For ZRLE the
//!   builder also picks the tile subencoding ([`ZrleTiling`]), because a decoder is
//!   only tested for the forms something actually produced.
//! - **Push unprompted**: [`ServerControl`] can change the screen, send ServerCutText,
//!   ring the bell, resize (DesktopSize), and drop the connection mid-session.
//! - **Never block the test**: [`FakeServer::wait_for`] takes a timeout and returns an
//!   error rather than hanging, because a hung test on CI is indistinguishable from a
//!   dead runner.
//!
//! # Tests this crate owes
//!
//! Its own round-trip suite, in `tests/`, since it is the only place `bolide-rfb` and
//! `bolide-testkit` meet: connect with None and with VNC auth (and a wrong password),
//! each encoding decoding to the exact scripted pixels, a resize, a bell, a cut text in
//! each direction, an overlapping CopyRect (a scroll), and **two ZRLE rects in sequence
//! whose second only decodes if the zlib stream was never reset**.

#![deny(missing_docs)]

use std::net::SocketAddr;
use std::time::Duration;

use bolide_rfb::proto::PixelFormat;

mod encode;
mod server;

pub use server::{FakeServer, FakeServerBuilder, ServerControl};

/// Which encoding the fake uses for the rects it sends.
///
/// `Negotiated` picks the first of the client's advertised encodings the fake can
/// write, which is what a real server does; the explicit variants are how a test aims
/// at one decoder.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Encoding {
    /// Whatever the client asked for first and this fake can produce.
    #[default]
    Negotiated,
    /// Always Raw.
    Raw,
    /// Always ZRLE.
    Zrle,
}

/// Which ZRLE tile subencoding the fake spends on every tile.
///
/// A decoder is only tested for the subencodings something actually produced, so a test
/// aiming at one of ZRLE's five tile forms names it here:
/// `FakeServer::builder().encoding(Encoding::Zrle).zrle_tiling(ZrleTiling::PaletteRle)`.
/// The forms themselves are described once, in `bolide_rfb::zrle`'s module docs.
///
/// A tile that cannot express the chosen form — `Solid` on a tile of two colours, a
/// packed palette on a photograph — falls back to [`ZrleTiling::Auto`]'s choice, because
/// the alternative is a rect that lies about its own pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ZrleTiling {
    /// The cheapest form each tile fits into, the way a real server chooses.
    #[default]
    Auto,
    /// Subencoding 0: every pixel, uncompressed inside the zlib stream.
    Raw,
    /// Subencoding 1: one pixel for the whole tile.
    Solid,
    /// Subencodings 2..=16: a palette and packed 1/2/4-bit indices.
    PackedPalette,
    /// Subencoding 128: `(pixel, run length)` pairs.
    PlainRle,
    /// Subencodings 130..=255: a palette, then runs of palette indices.
    PaletteRle,
}

/// How the fake authenticates.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Security {
    /// Offer `None` only.
    #[default]
    None,
    /// Offer `VncAuth` only, and accept exactly this password.
    VncAuth(String),
    /// Offer `VncAuth` and fail the SecurityResult whatever the client sends.
    AlwaysFail,
    /// Offer a security list the client cannot use (so its error path is reachable).
    Unsupported,
}

/// One thing the client sent, recorded in order.
#[derive(Clone, Debug, PartialEq)]
pub enum ClientEvent {
    /// SetPixelFormat.
    SetPixelFormat(PixelFormat),
    /// SetEncodings, in the order advertised.
    SetEncodings(Vec<i32>),
    /// FramebufferUpdateRequest.
    UpdateRequest {
        /// The incremental flag.
        incremental: bool,
        /// Requested region x.
        x: u16,
        /// Requested region y.
        y: u16,
        /// Requested region width.
        width: u16,
        /// Requested region height.
        height: u16,
    },
    /// PointerEvent.
    Pointer {
        /// X.
        x: u16,
        /// Y.
        y: u16,
        /// Button mask.
        buttons: u8,
    },
    /// KeyEvent.
    Key {
        /// X11 keysym.
        keysym: u32,
        /// Down or up.
        down: bool,
    },
    /// ClientCutText, already lifted out of latin-1.
    CutText(String),
}

/// A scripted screen: RGBA8888, row-major.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Screen {
    /// Width in pixels.
    pub width: u16,
    /// Height in pixels.
    pub height: u16,
    /// `width * height * 4` bytes.
    pub rgba: Vec<u8>,
}

impl Screen {
    /// A screen of one colour.
    pub fn solid(width: u16, height: u16, rgba: [u8; 4]) -> Screen {
        let mut data = Vec::with_capacity(width as usize * height as usize * 4);
        for _ in 0..(width as usize * height as usize) {
            data.extend_from_slice(&rgba);
        }
        Screen {
            width,
            height,
            rgba: data,
        }
    }

    /// Paint a rectangle, clipped to the screen.
    pub fn fill_rect(&mut self, x: u16, y: u16, width: u16, height: u16, rgba: [u8; 4]) {
        for row in y..y.saturating_add(height).min(self.height) {
            for col in x..x.saturating_add(width).min(self.width) {
                let i = (row as usize * self.width as usize + col as usize) * 4;
                self.rgba[i..i + 4].copy_from_slice(&rgba);
            }
        }
    }

    /// The pixel at `(x, y)`, or `None` off-screen.
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

    /// Decode a PNG into a screen — the way a test scripts something that looks like a
    /// real desktop rather than a colour field.
    ///
    /// Palette, grayscale and 16-bit images are normalised on the way in, so what comes
    /// back is always RGBA8888; a PNG wider or taller than a `u16` is an error, because
    /// RFB cannot describe one.
    pub fn from_png(bytes: &[u8]) -> std::result::Result<Screen, TestkitError> {
        let mut decoder = png::Decoder::new(bytes);
        decoder.set_transformations(png::Transformations::normalize_to_color8());
        let mut reader = decoder
            .read_info()
            .map_err(|e| TestkitError::Png(e.to_string()))?;
        let mut buf = vec![0u8; reader.output_buffer_size()];
        let info = reader
            .next_frame(&mut buf)
            .map_err(|e| TestkitError::Png(e.to_string()))?;

        if info.width > u16::MAX as u32 || info.height > u16::MAX as u32 {
            return Err(TestkitError::Png(format!(
                "{}x{} does not fit in RFB's u16 dimensions",
                info.width, info.height
            )));
        }
        let pixels = info.width as usize * info.height as usize;
        let src = &buf[..info.buffer_size()];
        let mut rgba = Vec::with_capacity(pixels * 4);
        match info.color_type {
            png::ColorType::Rgba => rgba.extend_from_slice(src),
            png::ColorType::Rgb => {
                for p in src.chunks_exact(3) {
                    rgba.extend_from_slice(&[p[0], p[1], p[2], 255]);
                }
            }
            png::ColorType::Grayscale => {
                for &g in src {
                    rgba.extend_from_slice(&[g, g, g, 255]);
                }
            }
            png::ColorType::GrayscaleAlpha => {
                for p in src.chunks_exact(2) {
                    rgba.extend_from_slice(&[p[0], p[0], p[0], p[1]]);
                }
            }
            other => {
                return Err(TestkitError::Png(format!(
                    "{other:?} survived normalisation, which should be impossible"
                )));
            }
        }
        if rgba.len() != pixels * 4 {
            return Err(TestkitError::Png(format!(
                "decoded {} bytes for {}x{}",
                rgba.len(),
                info.width,
                info.height
            )));
        }
        Ok(Screen {
            width: info.width as u16,
            height: info.height as u16,
            rgba,
        })
    }

    /// A deterministic pattern with runs, repeats and a few distinct colours — the
    /// shape that exercises ZRLE's palette and RLE tiles rather than only its raw ones.
    ///
    /// Four colours in a repeating 2×2 arrangement of `cell`-sized squares: long runs
    /// along a row, a small palette per tile, and a colour change at every cell edge.
    /// A `cell` of 0 reads as 1.
    pub fn checkerboard(width: u16, height: u16, cell: u16) -> Screen {
        const COLOURS: [[u8; 4]; 4] = [
            [0, 0, 0, 255],
            [255, 255, 255, 255],
            [255, 0, 0, 255],
            [0, 0, 255, 255],
        ];
        let cell = cell.max(1) as usize;
        let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
        for y in 0..height as usize {
            for x in 0..width as usize {
                let i = (x / cell) % 2 + 2 * ((y / cell) % 2);
                rgba.extend_from_slice(&COLOURS[i]);
            }
        }
        Screen {
            width,
            height,
            rgba,
        }
    }
}

/// What can go wrong inside the fake.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TestkitError {
    /// The socket failed.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// The client sent something the fake did not understand.
    #[error("the client sent something unexpected: {0}")]
    Protocol(String),
    /// A PNG did not decode.
    #[error("could not read the png: {0}")]
    Png(String),
    /// [`FakeServer::wait_for`] gave up.
    #[error("no matching client event within {0:?}")]
    Timeout(Duration),
}

/// `Result` with this crate's error.
pub type Result<T> = std::result::Result<T, TestkitError>;

/// Convenience: `127.0.0.1:0`.
pub fn loopback_any() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}
