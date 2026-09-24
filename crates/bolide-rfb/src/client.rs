//! The handshake and the read loop.
//!
//! CONTRACT — this file is `bolide-rfb`'s to finish.
//!
//! ## Handshake, in order (RFC 6143 §7.1)
//!
//! 1. Read 12 bytes of ProtocolVersion; [`crate::proto::parse_version`] it. Reply with
//!    the *lower* of the server's version and 3.8. A banner that is not one is
//!    `Error::NotRfb`; a major other than 3 is `Error::UnsupportedVersion`.
//! 2. **3.7+:** read `u8` count, then that many security-type bytes. Choose
//!    `SECURITY_NONE` when offered and no password is set, else `SECURITY_VNC_AUTH`;
//!    write the chosen byte. A count of 0 means failure — read the `u32`-prefixed
//!    reason string and return `Error::Refused`.
//!    **3.3:** the server sends a `u32` security type instead and the client chooses
//!    nothing.
//! 3. **VNC auth:** read the 16-byte challenge, write
//!    [`crate::proto::vnc_auth_response`]. No password configured → `Error::PasswordRequired`
//!    *before* sending anything.
//! 4. SecurityResult (`u32`). Non-zero → read the reason string (3.8 only) and return
//!    `Error::AuthFailed`.
//! 5. ClientInit: one byte, the shared flag.
//! 6. ServerInit: `u16` width, `u16` height, 16-byte PIXEL_FORMAT, `u32` name length,
//!    name bytes.
//! 7. Send SetPixelFormat([`crate::proto::PixelFormat::RGBX_32`]) and
//!    SetEncodings([`crate::proto::ADVERTISED_ENCODINGS`]).
//!
//! Every read in this phase is under `config.timeout` → `Error::Timeout`.
//!
//! ## The read loop
//!
//! One server message at a time: FramebufferUpdate (0), SetColourMapEntries (1, read
//! and discard — bolide always negotiates true colour), Bell (2), ServerCutText (3).
//! Anything else is `Error::Protocol`.
//!
//! A FramebufferUpdate is `u8 pad, u16 count`, then `count` rects of
//! `u16 x, y, w, h, i32 encoding` and the encoding's payload:
//!
//! - **Raw** — `w * h * bytes_per_pixel` bytes, blitted through the pixel format.
//! - **CopyRect** — `u16 src_x, u16 src_y`: copy from elsewhere in *this* framebuffer.
//!   Overlapping regions must copy correctly (copy the source rows out first, or walk
//!   in the safe direction) — a scroll is exactly the overlapping case.
//! - **ZRLE** — see `zrle.rs`.
//! - **DesktopSize (-223)** — the rect's `w`/`h` are the new size: re-allocate black,
//!   mark unpainted, emit `ServerEvent::Resized`, and request a full update.
//!
//! After the last rect of an update: emit `ServerEvent::Painted`.
//!
//! ## Two tasks, not one
//!
//! Reading a message is a sequence of `read_exact`s and so is not cancel-safe; writing
//! has to happen while that sequence is mid-flight. Trying to do both in one
//! `select!` means either a partially-read message or a blocked writer. So the stream
//! is split: a reader task and a writer task, both watching one shutdown channel, and
//! whichever notices death first announces it.
//!
//! ## Which pixel format the decoders use
//!
//! bolide asks for `RGBX_32` and then decodes with it, because RFB gives a client no way
//! to learn that a server ignored SetPixelFormat — there is no acknowledgement and Raw
//! carries no length. What the decoders do *not* do is assume it: every pixel goes
//! through [`crate::proto::PixelFormat::to_rgba`], so the format is one value to change
//! rather than a shape baked into three decoders.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc, watch};

use crate::proto::{self, PixelFormat};
use crate::session::{Command, Guard, Inner, State};
use crate::zrle::ZrleDecoder;
use crate::{Config, Error, Framebuffer, Result, ServerEvent, SessionHandle};

/// Broadcast depth. A subscriber that falls this far behind lags rather than stalling
/// the read loop — see `RecvError::Lagged` handling in `session.rs`.
const EVENT_CAPACITY: usize = 256;

/// Nothing a well-behaved server sends is this big; the cap is there so a malformed
/// length cannot ask bolide to allocate a gigabyte.
const MAX_STRING: u32 = 64 * 1024;
const MAX_CUT_TEXT: u32 = 8 * 1024 * 1024;
const MAX_ZRLE_PAYLOAD: u32 = 64 * 1024 * 1024;

/// Dial `addr` (`"host:port"`) and complete the handshake.
pub(crate) async fn connect_tcp(addr: &str, config: Config) -> Result<SessionHandle> {
    let stream =
        match tokio::time::timeout(config.timeout, tokio::net::TcpStream::connect(addr)).await {
            Err(_) => return Err(Error::Timeout(config.timeout)),
            Ok(Ok(s)) => s,
            Ok(Err(e)) => return Err(Error::Io(e)),
        };
    // Every message bolide sends is tiny and latency-sensitive; Nagle would batch a
    // keystroke behind the next one.
    let _ = stream.set_nodelay(true);
    connect_stream(stream, config).await
}

/// Complete the handshake over an already-open stream, and spawn the session task.
pub(crate) async fn connect_stream<S>(stream: S, config: Config) -> Result<SessionHandle>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let mut conn = Conn::new(stream, config.timeout);
    let init = handshake(&mut conn, &config).await?;

    let (events, _) = broadcast::channel(EVENT_CAPACITY);
    let (tx, rx) = mpsc::unbounded_channel();
    let (shutdown, shutdown_rx) = watch::channel(false);
    let inner = Arc::new(Inner {
        events,
        commands: tx,
        shutdown,
        state: Mutex::new(State {
            fb: Framebuffer::black(init.width, init.height),
            painted: false,
            last_cut_text: None,
        }),
        desktop_name: init.name,
        connected: AtomicBool::new(true),
        generation: AtomicU64::new(0),
        outstanding: AtomicUsize::new(0),
        // Nothing has been asked for yet, so there is no baseline for "incremental".
        needs_full: AtomicBool::new(true),
    });

    let (read_half, write_half) = tokio::io::split(conn.into_inner());
    tokio::spawn(writer_task(
        write_half,
        rx,
        Arc::clone(&inner),
        shutdown_rx.clone(),
    ));
    tokio::spawn(reader_task(
        Conn::new(read_half, config.timeout),
        Arc::clone(&inner),
        config.continuous,
        shutdown_rx,
    ));

    let handle = SessionHandle {
        _guard: Arc::new(Guard(Arc::downgrade(&inner))),
        inner,
    };
    if config.continuous {
        // `needs_full` makes this a whole-screen request whatever we ask for.
        handle.inner.send(Command::Request { incremental: true })?;
    }
    Ok(handle)
}

// ---------------------------------------------------------------------------
// Handshake
// ---------------------------------------------------------------------------

/// What ServerInit told us.
struct ServerInit {
    width: u16,
    height: u16,
    name: String,
}

async fn handshake<S>(conn: &mut Conn<S>, config: &Config) -> Result<ServerInit>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut banner = [0u8; 12];
    conn.fill(&mut banner).await?;
    let (major, minor) = proto::parse_version(&banner).ok_or(Error::NotRfb)?;
    if major != 3 || minor < 3 {
        return Err(Error::UnsupportedVersion { major, minor });
    }
    // Negotiate *down*: answer with the lower of the server's version and 3.8. 3.4-3.6
    // never existed as deployed dialects; RFB says treat anything under 3.7 as 3.3.
    let (reply, minor) = match minor {
        3..=6 => (proto::VERSION_3_3, 3u32),
        7 => (proto::VERSION_3_7, 7),
        _ => (proto::VERSION_3_8, 8),
    };
    conn.write(reply).await?;

    let chosen = if minor >= 7 {
        choose_security(conn, config).await?
    } else {
        // 3.3: the server decides and says so in a u32. Zero is failure.
        let ty = conn.u32().await?;
        if ty == 0 {
            return Err(Error::Refused(conn.reason().await?));
        }
        let ty = ty as u8;
        if ty != proto::SECURITY_NONE && ty != proto::SECURITY_VNC_AUTH {
            return Err(Error::NoSupportedSecurity { offered: vec![ty] });
        }
        ty
    };

    let mut expect_security_result = true;
    if chosen == proto::SECURITY_VNC_AUTH {
        // Fail before the response, not after: sending the DES of an empty password
        // is a login attempt, and a server that counts them will lock the account.
        let Some(password) = config.password.as_deref() else {
            return Err(Error::PasswordRequired);
        };
        let mut challenge = [0u8; 16];
        conn.fill(&mut challenge).await?;
        conn.write(&proto::vnc_auth_response(password, &challenge))
            .await?;
    } else if minor < 7 {
        // 3.3 with security None goes straight to initialisation — no SecurityResult.
        expect_security_result = false;
    }

    if expect_security_result {
        let result = conn.u32().await?;
        if result != proto::SECURITY_RESULT_OK {
            // 3.8 explains itself; 3.7 and 3.3 just hang up.
            let reason = if minor >= 8 {
                conn.reason().await?
            } else {
                "the server rejected the credentials".to_string()
            };
            return Err(Error::AuthFailed(reason));
        }
    }

    conn.write(&[u8::from(config.shared)]).await?;

    let width = conn.u16().await?;
    let height = conn.u16().await?;
    let mut format = [0u8; 16];
    conn.fill(&mut format).await?;
    let name = conn.string(MAX_STRING).await?;

    conn.write(&proto::set_pixel_format(&PixelFormat::RGBX_32))
        .await?;
    conn.write(&proto::set_encodings(proto::ADVERTISED_ENCODINGS))
        .await?;

    Ok(ServerInit {
        width,
        height,
        name,
    })
}

/// The 3.7+ security list: read it, pick one, write the choice.
async fn choose_security<S>(conn: &mut Conn<S>, config: &Config) -> Result<u8>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let count = conn.u8().await?;
    if count == 0 {
        return Err(Error::Refused(conn.reason().await?));
    }
    let offered = conn.bytes(count as usize).await?;
    let has = |t: u8| offered.contains(&t);
    // A password does not *require* VNC auth — a server that wants none should still
    // connect — but when both are on offer the one that authenticates wins.
    let chosen = if config.password.is_some() && has(proto::SECURITY_VNC_AUTH) {
        proto::SECURITY_VNC_AUTH
    } else if has(proto::SECURITY_NONE) {
        proto::SECURITY_NONE
    } else if has(proto::SECURITY_VNC_AUTH) {
        proto::SECURITY_VNC_AUTH
    } else {
        return Err(Error::NoSupportedSecurity { offered });
    };
    conn.write(&[chosen]).await?;
    Ok(chosen)
}

// ---------------------------------------------------------------------------
// The writer task
// ---------------------------------------------------------------------------

async fn writer_task<W>(
    mut w: W,
    mut rx: mpsc::UnboundedReceiver<Command>,
    inner: Arc<Inner>,
    mut shutdown: watch::Receiver<bool>,
) where
    W: AsyncWrite + Unpin,
{
    loop {
        let command = tokio::select! {
            got = rx.recv() => match got {
                Some(c) => c,
                None => break,
            },
            _ = shutdown.changed() => break,
        };
        let bytes = match command {
            Command::Write(bytes) => bytes,
            Command::Request { incremental } => inner.build_request(incremental),
        };
        if let Err(e) = w.write_all(&bytes).await {
            inner.mark_dead(Some(e.to_string()));
            break;
        }
    }
    // Half-closing tells a real server we are finished, and tells a test's duplex the
    // client has hung up.
    let _ = w.shutdown().await;
}

// ---------------------------------------------------------------------------
// The read loop
// ---------------------------------------------------------------------------

/// Everything the read loop owns and nobody else may touch.
struct Decoders {
    zrle: ZrleDecoder,
    format: PixelFormat,
}

async fn reader_task<R>(
    mut conn: Conn<R>,
    inner: Arc<Inner>,
    continuous: bool,
    mut shutdown: watch::Receiver<bool>,
) where
    R: AsyncRead + Unpin,
{
    let mut decoders = Decoders {
        zrle: ZrleDecoder::new(),
        format: PixelFormat::RGBX_32,
    };
    loop {
        let outcome = tokio::select! {
            res = read_message(&mut conn, &inner, &mut decoders, continuous) => res,
            // `changed()` also errors when the last handle drops `Inner`, which is the
            // other way this loop is meant to end.
            _ = shutdown.changed() => break,
        };
        if let Err(e) = outcome {
            inner.mark_dead(Some(e.to_string()));
            break;
        }
    }
}

async fn read_message<R>(
    conn: &mut Conn<R>,
    inner: &Arc<Inner>,
    decoders: &mut Decoders,
    continuous: bool,
) -> Result<()>
where
    R: AsyncRead + Unpin,
{
    // Silence on an idle screen is not an error, so the *first* byte of a message
    // waits forever. Everything after it is owed to us and is under the timeout.
    conn.disarm();
    let message = conn.u8().await?;
    conn.arm();
    match message {
        proto::server_msg::FRAMEBUFFER_UPDATE => {
            read_framebuffer_update(conn, inner, decoders, continuous).await
        }
        proto::server_msg::SET_COLOUR_MAP_ENTRIES => {
            // bolide negotiated true colour, so this is noise — but it is *sized* noise,
            // and getting its length wrong desynchronises everything after it.
            let _pad = conn.u8().await?;
            let _first = conn.u16().await?;
            let count = conn.u16().await?;
            conn.skip(count as usize * 6).await
        }
        proto::server_msg::BELL => {
            inner.emit(ServerEvent::Bell);
            Ok(())
        }
        proto::server_msg::SERVER_CUT_TEXT => {
            conn.skip(3).await?;
            let text = conn.string(MAX_CUT_TEXT).await?;
            inner.lock().last_cut_text = Some(text.clone());
            inner.emit(ServerEvent::CutText(text));
            Ok(())
        }
        other => Err(Error::Protocol(format!(
            "server message type {other} is not one of 0..=3"
        ))),
    }
}

async fn read_framebuffer_update<R>(
    conn: &mut Conn<R>,
    inner: &Arc<Inner>,
    decoders: &mut Decoders,
    continuous: bool,
) -> Result<()>
where
    R: AsyncRead + Unpin,
{
    let _pad = conn.u8().await?;
    let count = conn.u16().await?;
    let mut painted_any = false;
    let mut resized = false;
    for _ in 0..count {
        let x = conn.u16().await?;
        let y = conn.u16().await?;
        let w = conn.u16().await?;
        let h = conn.u16().await?;
        let encoding = conn.i32().await?;
        match encoding {
            proto::ENC_RAW => {
                let bpp = decoders.format.bytes_per_pixel();
                let len = w as usize * h as usize * bpp;
                let data = conn.bytes(len).await?;
                // The lock is taken *after* the read, never across it: a std MutexGuard
                // held over an await would not compile, which is the point.
                let mut st = inner.lock();
                blit_raw(&mut st.fb, x, y, w, h, &decoders.format, &data)?;
                painted_any = true;
            }
            proto::ENC_COPY_RECT => {
                let src_x = conn.u16().await?;
                let src_y = conn.u16().await?;
                let mut st = inner.lock();
                copy_rect(&mut st.fb, x, y, w, h, src_x, src_y)?;
                painted_any = true;
            }
            proto::ENC_ZRLE => {
                let len = conn.u32().await?;
                if len > MAX_ZRLE_PAYLOAD {
                    return Err(Error::Protocol(format!("ZRLE rect claims {len} bytes")));
                }
                let payload = conn.bytes(len as usize).await?;
                // Inflate before locking: decompression is the expensive part and it
                // does not need the framebuffer.
                let rgba = decoders
                    .zrle
                    .decode_rect(&payload, &decoders.format, w, h)?;
                let mut st = inner.lock();
                blit_rgba(&mut st.fb, x, y, w, h, &rgba)?;
                painted_any = true;
            }
            proto::ENC_DESKTOP_SIZE => {
                {
                    let mut st = inner.lock();
                    st.fb = Framebuffer::black(w, h);
                    st.painted = false;
                }
                // A resize invalidates the baseline, so the next request must be whole.
                inner.needs_full.store(true, Ordering::SeqCst);
                inner.emit(ServerEvent::Resized {
                    width: w,
                    height: h,
                });
                resized = true;
            }
            other => return Err(Error::UnsupportedEncoding(other)),
        }
    }

    inner
        .outstanding
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
            Some(v.saturating_sub(1))
        })
        .ok();

    // A resize always earns a request — the screen we hold is black and wrong. Otherwise
    // continuous mode tops up, but only when nothing is already in flight.
    //
    // This happens *before* `Painted` goes out, so that everything an observer of the
    // event might ask about the session is already true when it arrives.
    if resized || (continuous && inner.outstanding.load(Ordering::SeqCst) == 0) {
        let _ = inner.send(Command::Request { incremental: true });
    }
    if painted_any {
        let generation = inner.generation.fetch_add(1, Ordering::SeqCst) + 1;
        inner.lock().painted = true;
        inner.emit(ServerEvent::Painted { generation });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Decoders (pure — the tests at the bottom of this file drive them directly)
// ---------------------------------------------------------------------------

/// Every rect has to land inside the framebuffer. A server that says otherwise is
/// broken, and writing where it asked would be an out-of-bounds write in a process an
/// agent is driving somebody's machine from.
fn check_bounds(fb: &Framebuffer, x: u16, y: u16, w: u16, h: u16) -> Result<()> {
    if x as u32 + w as u32 > fb.width as u32 || y as u32 + h as u32 > fb.height as u32 {
        return Err(Error::Protocol(format!(
            "a {w}x{h} rect at ({x},{y}) does not fit a {}x{} framebuffer",
            fb.width, fb.height
        )));
    }
    Ok(())
}

/// Raw: `w * h` pixels in the negotiated format, row-major.
fn blit_raw(
    fb: &mut Framebuffer,
    x: u16,
    y: u16,
    w: u16,
    h: u16,
    format: &PixelFormat,
    data: &[u8],
) -> Result<()> {
    check_bounds(fb, x, y, w, h)?;
    let bpp = format.bytes_per_pixel();
    let (rw, rh) = (w as usize, h as usize);
    if data.len() < rw * rh * bpp {
        return Err(Error::Protocol(format!(
            "a raw {w}x{h} rect needs {} bytes, got {}",
            rw * rh * bpp,
            data.len()
        )));
    }
    let stride = fb.width as usize * 4;
    for row in 0..rh {
        for col in 0..rw {
            let src = (row * rw + col) * bpp;
            let rgba = format.to_rgba(format.read_pixel(&data[src..src + bpp]));
            let dst = (y as usize + row) * stride + (x as usize + col) * 4;
            fb.rgba[dst..dst + 4].copy_from_slice(&rgba);
        }
    }
    Ok(())
}

/// A rect that is already RGBA (what ZRLE hands back).
fn blit_rgba(fb: &mut Framebuffer, x: u16, y: u16, w: u16, h: u16, rgba: &[u8]) -> Result<()> {
    check_bounds(fb, x, y, w, h)?;
    let (rw, rh) = (w as usize, h as usize);
    if rgba.len() < rw * rh * 4 {
        return Err(Error::Protocol("a decoded rect came up short".to_string()));
    }
    let stride = fb.width as usize * 4;
    for row in 0..rh {
        let dst = (y as usize + row) * stride + x as usize * 4;
        let src = row * rw * 4;
        fb.rgba[dst..dst + rw * 4].copy_from_slice(&rgba[src..src + rw * 4]);
    }
    Ok(())
}

/// CopyRect: move a region of the framebuffer onto another part of itself.
///
/// The source is copied out **whole** before anything is written back. A scroll is a
/// copy of a region onto itself shifted by a few rows, so the naive row-by-row version
/// smears the first row down the screen — and it looks right in a test that copies two
/// disjoint rectangles.
fn copy_rect(
    fb: &mut Framebuffer,
    x: u16,
    y: u16,
    w: u16,
    h: u16,
    src_x: u16,
    src_y: u16,
) -> Result<()> {
    check_bounds(fb, x, y, w, h)?;
    check_bounds(fb, src_x, src_y, w, h)?;
    let (rw, rh) = (w as usize, h as usize);
    let stride = fb.width as usize * 4;
    let mut scratch = vec![0u8; rw * rh * 4];
    for row in 0..rh {
        let src = (src_y as usize + row) * stride + src_x as usize * 4;
        scratch[row * rw * 4..(row + 1) * rw * 4].copy_from_slice(&fb.rgba[src..src + rw * 4]);
    }
    for row in 0..rh {
        let dst = (y as usize + row) * stride + x as usize * 4;
        fb.rgba[dst..dst + rw * 4].copy_from_slice(&scratch[row * rw * 4..(row + 1) * rw * 4]);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// A read head with a deadline
// ---------------------------------------------------------------------------

/// Reads with a timeout that can be switched off for the one read that is allowed to
/// block forever: the first byte of the next server message.
struct Conn<S> {
    stream: S,
    timeout: Duration,
    armed: bool,
}

impl<S> Conn<S> {
    fn new(stream: S, timeout: Duration) -> Conn<S> {
        Conn {
            stream,
            timeout,
            armed: true,
        }
    }

    fn into_inner(self) -> S {
        self.stream
    }

    fn arm(&mut self) {
        self.armed = true;
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl<S: AsyncRead + Unpin> Conn<S> {
    async fn fill(&mut self, buf: &mut [u8]) -> Result<()> {
        if !self.armed {
            self.stream.read_exact(buf).await?;
            return Ok(());
        }
        match tokio::time::timeout(self.timeout, self.stream.read_exact(buf)).await {
            Err(_) => Err(Error::Timeout(self.timeout)),
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(Error::Io(e)),
        }
    }

    async fn u8(&mut self) -> Result<u8> {
        let mut b = [0u8; 1];
        self.fill(&mut b).await?;
        Ok(b[0])
    }

    async fn u16(&mut self) -> Result<u16> {
        let mut b = [0u8; 2];
        self.fill(&mut b).await?;
        Ok(u16::from_be_bytes(b))
    }

    async fn u32(&mut self) -> Result<u32> {
        let mut b = [0u8; 4];
        self.fill(&mut b).await?;
        Ok(u32::from_be_bytes(b))
    }

    async fn i32(&mut self) -> Result<i32> {
        Ok(self.u32().await? as i32)
    }

    async fn bytes(&mut self, n: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; n];
        self.fill(&mut buf).await?;
        Ok(buf)
    }

    /// Read and throw away, without allocating what we are throwing away.
    async fn skip(&mut self, mut n: usize) -> Result<()> {
        let mut sink = [0u8; 1024];
        while n > 0 {
            let take = n.min(sink.len());
            self.fill(&mut sink[..take]).await?;
            n -= take;
        }
        Ok(())
    }

    /// A `u32`-prefixed latin-1 string, capped so a bad length cannot be an allocation.
    async fn string(&mut self, cap: u32) -> Result<String> {
        let len = self.u32().await?;
        if len > cap {
            return Err(Error::Protocol(format!(
                "a string of {len} bytes is past the {cap}-byte limit"
            )));
        }
        Ok(proto::from_latin1(&self.bytes(len as usize).await?))
    }

    /// The `u32`-prefixed reason a 3.8 server gives for hanging up.
    async fn reason(&mut self) -> Result<String> {
        self.string(MAX_STRING).await
    }
}

impl<S: AsyncWrite + Unpin> Conn<S> {
    async fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.stream.write_all(bytes).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill(fb: &mut Framebuffer, colour: [u8; 4]) {
        for px in fb.rgba.chunks_exact_mut(4) {
            px.copy_from_slice(&colour);
        }
    }

    const RED: [u8; 4] = [255, 0, 0, 255];
    const GREEN: [u8; 4] = [0, 255, 0, 255];
    const BLUE: [u8; 4] = [0, 0, 255, 255];

    #[test]
    fn raw_lands_at_the_rects_offset() {
        let mut fb = Framebuffer::black(4, 4);
        let f = PixelFormat::RGBX_32;
        let mut data = Vec::new();
        for c in [RED, GREEN, BLUE, [1, 2, 3, 255]] {
            f.write_pixel(f.from_rgba(c), &mut data);
        }
        blit_raw(&mut fb, 1, 2, 2, 2, &f, &data).unwrap();
        assert_eq!(fb.pixel(1, 2), Some(RED));
        assert_eq!(fb.pixel(2, 2), Some(GREEN));
        assert_eq!(fb.pixel(1, 3), Some(BLUE));
        assert_eq!(fb.pixel(2, 3), Some([1, 2, 3, 255]));
        // Nothing outside the rect moved.
        assert_eq!(fb.pixel(0, 0), Some([0, 0, 0, 255]));
        assert_eq!(fb.pixel(3, 3), Some([0, 0, 0, 255]));
    }

    /// The decoder must go through `PixelFormat::to_rgba`, not assume four bytes per
    /// pixel with the colour already in the right order.
    #[test]
    fn raw_decodes_a_16bpp_565_rect() {
        let f = PixelFormat {
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
        let mut fb = Framebuffer::black(2, 1);
        let mut data = Vec::new();
        for c in [RED, BLUE] {
            f.write_pixel(f.from_rgba(c), &mut data);
        }
        assert_eq!(data.len(), 4, "two pixels at two bytes each");
        blit_raw(&mut fb, 0, 0, 2, 1, &f, &data).unwrap();
        assert_eq!(fb.pixel(0, 0), Some(RED));
        assert_eq!(fb.pixel(1, 0), Some(BLUE));
    }

    #[test]
    fn raw_decodes_a_big_endian_rect() {
        let f = PixelFormat {
            big_endian: true,
            red_shift: 16,
            green_shift: 8,
            blue_shift: 0,
            ..PixelFormat::RGBX_32
        };
        let mut fb = Framebuffer::black(1, 1);
        let mut data = Vec::new();
        f.write_pixel(f.from_rgba(GREEN), &mut data);
        assert_eq!(data, vec![0x00, 0x00, 0xff, 0x00]);
        blit_raw(&mut fb, 0, 0, 1, 1, &f, &data).unwrap();
        assert_eq!(fb.pixel(0, 0), Some(GREEN));
    }

    #[test]
    fn a_rect_that_does_not_fit_is_a_protocol_error() {
        let mut fb = Framebuffer::black(4, 4);
        let f = PixelFormat::RGBX_32;
        let data = vec![0u8; 4 * 4 * 4];
        let err = blit_raw(&mut fb, 2, 2, 4, 4, &f, &data).unwrap_err();
        assert!(matches!(err, Error::Protocol(_)), "{err:?}");
        let err = copy_rect(&mut fb, 0, 0, 2, 2, 3, 3).unwrap_err();
        assert!(matches!(err, Error::Protocol(_)), "{err:?}");
    }

    #[test]
    fn a_short_raw_rect_is_a_protocol_error() {
        let mut fb = Framebuffer::black(4, 4);
        let err = blit_raw(&mut fb, 0, 0, 4, 4, &PixelFormat::RGBX_32, &[0u8; 8]).unwrap_err();
        assert!(matches!(err, Error::Protocol(_)), "{err:?}");
    }

    #[test]
    fn copy_rect_moves_a_disjoint_region() {
        let mut fb = Framebuffer::black(4, 2);
        fill(&mut fb, BLUE);
        for x in 0..2 {
            let i = x * 4;
            fb.rgba[i..i + 4].copy_from_slice(&RED);
        }
        copy_rect(&mut fb, 2, 1, 2, 1, 0, 0).unwrap();
        assert_eq!(fb.pixel(2, 1), Some(RED));
        assert_eq!(fb.pixel(3, 1), Some(RED));
        assert_eq!(fb.pixel(0, 0), Some(RED), "the source is left alone");
    }

    /// A scroll **is** the overlapping case: copy rows 0..3 onto rows 1..4 of the same
    /// framebuffer. Row-by-row in the wrong direction smears row 0 over everything.
    #[test]
    fn copy_rect_handles_a_scroll_that_overlaps_itself() {
        let mut fb = Framebuffer::black(1, 4);
        let rows = [RED, GREEN, BLUE, [9, 9, 9, 255]];
        for (y, c) in rows.iter().enumerate() {
            fb.rgba[y * 4..y * 4 + 4].copy_from_slice(c);
        }
        // Move rows 0,1,2 down one, so the screen becomes RED, RED, GREEN, BLUE.
        copy_rect(&mut fb, 0, 1, 1, 3, 0, 0).unwrap();
        assert_eq!(fb.pixel(0, 0), Some(RED));
        assert_eq!(fb.pixel(0, 1), Some(RED));
        assert_eq!(fb.pixel(0, 2), Some(GREEN));
        assert_eq!(fb.pixel(0, 3), Some(BLUE));
    }

    /// The other direction of the same bug: scrolling *up* is safe row-by-row forwards
    /// and broken row-by-row backwards.
    #[test]
    fn copy_rect_handles_a_scroll_up() {
        let mut fb = Framebuffer::black(1, 4);
        let rows = [RED, GREEN, BLUE, [9, 9, 9, 255]];
        for (y, c) in rows.iter().enumerate() {
            fb.rgba[y * 4..y * 4 + 4].copy_from_slice(c);
        }
        copy_rect(&mut fb, 0, 0, 1, 3, 0, 1).unwrap();
        assert_eq!(fb.pixel(0, 0), Some(GREEN));
        assert_eq!(fb.pixel(0, 1), Some(BLUE));
        assert_eq!(fb.pixel(0, 2), Some([9, 9, 9, 255]));
        assert_eq!(fb.pixel(0, 3), Some([9, 9, 9, 255]));
    }
}
