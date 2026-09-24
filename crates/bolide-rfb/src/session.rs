//! The session actor and its handle.
//!
//! CONTRACT — this file is `bolide-rfb`'s to finish; the public shape below is fixed.
//!
//! `SessionHandle` is a cheap `Clone` that talks to one background task over an mpsc
//! channel. The task owns the socket, the framebuffer and the decoder state; nothing
//! else may touch them. That is what lets `bolide-server` answer an HTTP request while
//! the screen keeps updating, and it is why every mutating method is `async` even
//! though none of them waits for the server to reply.
//!
//! Required behaviour beyond the trait docs in `lib.rs`:
//!
//! - **Ordering.** Commands are applied in the order they were sent. A `type` that
//!   turns into forty KeyEvents must not interleave with a concurrent `pointer`.
//! - **`refresh(incremental=false)`** sends a non-incremental FramebufferUpdateRequest
//!   and resolves on the *next* applied update — not on one that was already in flight.
//! - **The first request is never incremental**, whatever the caller asked for:
//!   "incremental" means "changes since the update you last sent me", and before the
//!   first there is no baseline.
//! - **Continuous mode** re-issues an incremental request after each applied update, so
//!   an idle handle still holds a current screen. It must not spin: one request
//!   outstanding at a time.
//! - **Death is an event.** When the socket dies the task emits
//!   `ServerEvent::Disconnected`, marks `is_connected()` false, and every subsequent
//!   command returns `Error::Closed` rather than hanging.
//!
//! ## How the ordering guarantee is actually delivered
//!
//! There is exactly one queue. Every method turns its arguments into a [`Command`] and
//! pushes it onto an unbounded mpsc; one writer task pops and writes. So a message is
//! never split by another message's bytes, and two commands issued in sequence reach
//! the wire in that sequence.
//!
//! An *atomic burst* — "these forty KeyEvents, with nothing between them" — is one
//! command, not forty, because the trait's `key()` cannot express it: two tasks calling
//! `key()` in a loop legitimately interleave. [`SessionHandle::key_burst`] is the
//! inherent method that says "this is one thing", and it is what a `type` action should
//! use.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::{broadcast, mpsc, watch};

use crate::proto;
use crate::{Error, Framebuffer, Result, ServerEvent, Session};

/// One unit of work for the writer task. A burst is one `Command`, which is what makes
/// it un-interleavable.
pub(crate) enum Command {
    /// Bytes already built by the caller; written as one `write_all`.
    Write(Vec<u8>),
    /// A FramebufferUpdateRequest, built *at write time* so it uses the current size
    /// and the current "have we ever asked for a full screen" flag.
    Request {
        /// What the caller asked for. Downgraded to non-incremental when there is no
        /// baseline to be incremental against.
        incremental: bool,
    },
}

/// A cheap, cloneable handle to a live RFB session. See the module docs.
#[derive(Clone)]
pub struct SessionHandle {
    pub(crate) inner: std::sync::Arc<Inner>,
    /// Dropping the *last* handle closes the session. Without this the two tasks hold
    /// the socket open forever after their last caller has gone — a daemon that leaks
    /// a connection per failed request is the failure a long-lived server cannot have.
    pub(crate) _guard: std::sync::Arc<Guard>,
}

/// The last-handle-dropped hook. Holds a `Weak` so it is not itself part of the cycle.
pub(crate) struct Guard(pub(crate) std::sync::Weak<Inner>);

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(inner) = self.0.upgrade() {
            inner.mark_dead(None);
        }
    }
}

/// The parts of the session two tasks and every handle share.
pub(crate) struct Inner {
    pub(crate) events: broadcast::Sender<ServerEvent>,
    pub(crate) commands: mpsc::UnboundedSender<Command>,
    /// Flipped once, by whoever notices death first; both tasks watch it.
    pub(crate) shutdown: watch::Sender<bool>,
    pub(crate) state: Mutex<State>,
    pub(crate) desktop_name: String,
    pub(crate) connected: AtomicBool,
    pub(crate) generation: AtomicU64,
    /// FramebufferUpdateRequests sent but not yet answered. Continuous mode only adds
    /// one when this is zero, which is what stops it spinning.
    pub(crate) outstanding: AtomicUsize,
    /// True until a non-incremental request has gone out, and true again after a
    /// resize: there is nothing for "incremental" to be relative to.
    pub(crate) needs_full: AtomicBool,
}

/// Everything a caller can read synchronously.
pub(crate) struct State {
    pub(crate) fb: Framebuffer,
    pub(crate) painted: bool,
    pub(crate) last_cut_text: Option<String>,
}

impl Inner {
    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // A panic in a decoder must not turn every later read into a panic as well;
        // the framebuffer is data, not an invariant that poisoning protects.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn emit(&self, event: ServerEvent) {
        // `send` fails only when nobody is subscribed, which is the normal case for a
        // handle nobody has called `events()` on.
        let _ = self.events.send(event);
    }

    /// Build the next FramebufferUpdateRequest, downgrading "incremental" when there is
    /// no baseline. Called on the writer task so the size is the one in force now.
    pub(crate) fn build_request(&self, incremental: bool) -> Vec<u8> {
        let first = self.needs_full.swap(false, Ordering::SeqCst);
        let (w, h) = {
            let st = self.lock();
            (st.fb.width, st.fb.height)
        };
        proto::framebuffer_update_request(incremental && !first, 0, 0, w, h)
    }

    pub(crate) fn send(&self, command: Command) -> Result<()> {
        if !self.connected.load(Ordering::SeqCst) {
            return Err(Error::Closed);
        }
        if let Command::Request { .. } = command {
            self.outstanding.fetch_add(1, Ordering::SeqCst);
        }
        self.commands.send(command).map_err(|_| Error::Closed)
    }

    /// Announce death exactly once, whichever task notices it first.
    pub(crate) fn mark_dead(&self, reason: Option<String>) {
        if self.connected.swap(false, Ordering::SeqCst) {
            self.emit(ServerEvent::Disconnected { reason });
            let _ = self.shutdown.send(true);
        }
    }
}

impl SessionHandle {
    /// Send several KeyEvents as **one** command, so nothing can be written between
    /// them.
    ///
    /// The trait cannot express this: forty separate `key()` calls from two tasks
    /// legitimately interleave, and a `type` action that lost a modifier halfway
    /// through would be a bug nobody could reproduce. A caller that has a burst —
    /// typing a string, pressing and releasing a chord — should send it through here.
    pub async fn key_burst(&self, events: &[(u32, bool)]) -> Result<()> {
        let mut bytes = Vec::with_capacity(events.len() * 8);
        for (keysym, down) in events {
            bytes.extend_from_slice(&proto::key_event(*keysym, *down));
        }
        self.inner.send(Command::Write(bytes))
    }

    /// The count of FramebufferUpdateRequests sent but not yet answered. Exposed for
    /// the test that continuous mode never has more than one in flight.
    #[doc(hidden)]
    pub fn outstanding_requests(&self) -> usize {
        self.inner.outstanding.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl Session for SessionHandle {
    fn size(&self) -> (u16, u16) {
        let st = self.inner.lock();
        (st.fb.width, st.fb.height)
    }

    fn desktop_name(&self) -> String {
        self.inner.desktop_name.clone()
    }

    fn painted(&self) -> bool {
        self.inner.lock().painted
    }

    async fn framebuffer(&self) -> Result<Framebuffer> {
        // Deliberately still answers after a disconnect: the last frame is the last
        // true thing we know about that desktop, and `is_connected()` is how a caller
        // learns it is stale.
        Ok(self.inner.lock().fb.clone())
    }

    async fn refresh(&self, incremental: bool, timeout: Duration) -> Result<Framebuffer> {
        // Subscribe *before* asking, or an update that arrives while the request is
        // still being written is missed and the wait runs to the timeout.
        let mut events = self.inner.events.subscribe();
        self.inner.send(Command::Request { incremental })?;
        let outcome = tokio::time::timeout(timeout, async {
            loop {
                match events.recv().await {
                    Ok(ServerEvent::Painted { .. }) => return Ok(()),
                    Ok(ServerEvent::Disconnected { .. }) => return Err(Error::Closed),
                    Ok(_) => continue,
                    // A caller that fell behind still wants the *next* frame, not an
                    // error about the ones it missed.
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return Err(Error::Closed),
                }
            }
        })
        .await;
        match outcome {
            Err(_) => Err(Error::Timeout(timeout)),
            Ok(Err(e)) => Err(e),
            Ok(Ok(())) => self.framebuffer().await,
        }
    }

    async fn pointer(&self, x: u16, y: u16, buttons: u8) -> Result<()> {
        self.inner
            .send(Command::Write(proto::pointer_event(x, y, buttons)))
    }

    async fn key(&self, keysym: u32, down: bool) -> Result<()> {
        self.inner
            .send(Command::Write(proto::key_event(keysym, down)))
    }

    async fn cut_text(&self, text: &str) -> Result<()> {
        self.inner
            .send(Command::Write(proto::client_cut_text(text)))
    }

    fn last_cut_text(&self) -> Option<String> {
        self.inner.lock().last_cut_text.clone()
    }

    fn events(&self) -> broadcast::Receiver<ServerEvent> {
        self.inner.events.subscribe()
    }

    fn is_connected(&self) -> bool {
        self.inner.connected.load(Ordering::SeqCst)
    }

    async fn close(&self) -> Result<()> {
        // Idempotent: `mark_dead` is a one-shot, so a second close is a no-op rather
        // than a second `Disconnected` event.
        self.inner.mark_dead(None);
        Ok(())
    }
}
