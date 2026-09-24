//! bolide's loopback HTTP server: the computer-use protocol, spoken to an RFB desktop.
//!
//! ```text
//!   agent ──HTTP/JSON──▶ bolide-server ──RFB──▶ the desktop
//!                          (this crate)
//! ```
//!
//! # The seam that makes this testable
//!
//! An action is turned into RFB operations by [`plan`], which is **pure**: it takes the
//! action and the session's current geometry and returns a `Vec<RfbOp>`. No socket, no
//! async, no server. Every question worth asking — does a double click send four
//! pointer events or two? does a scroll move the pointer first? does a drag release
//! where it started or where it ended? — is a table test over that function.
//!
//! What is left over ([`execute`]) is a loop that hands each op to a
//! [`bolide_rfb::Session`], and the HTTP layer is argument parsing. That is deliberate:
//! the untested part should be too boring to hide a bug in.
//!
//! # Binding
//!
//! Default `127.0.0.1:0` — loopback, kernel-assigned port. bolide hands an agent full
//! control of somebody's desktop; it must not be reachable from the network by
//! accident, and a fixed default port is how two bolides on one box collide. The bound
//! address is returned from [`serve`] and printed by the CLI. A bearer token
//! ([`ServerConfig::token`]) is optional and, when set, required on every route except
//! `/healthz`.

#![deny(missing_docs)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bolide_rfb::Session;

pub mod wire;

mod actions;
mod http;
mod image;

pub use actions::{execute, plan, ActionError, PlanContext, RfbOp};
pub use image::encode_png;

/// How to run the server.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// Where to listen. Default `127.0.0.1:0`.
    pub bind: SocketAddr,
    /// Optional bearer token. When `Some`, every route but `/healthz` requires
    /// `Authorization: Bearer <token>`.
    pub token: Option<String>,
    /// What `/status` reports as the remote, e.g. `"mac01.local:5900"`. Must never
    /// carry a password.
    pub remote: String,
    /// How long `screenshot` waits for a fresh frame before answering with what it has.
    pub frame_timeout: Duration,
}

impl Default for ServerConfig {
    fn default() -> ServerConfig {
        ServerConfig {
            bind: SocketAddr::from(([127, 0, 0, 1], 0)),
            token: None,
            remote: String::new(),
            frame_timeout: Duration::from_secs(5),
        }
    }
}

/// A server that is listening.
pub struct Running {
    /// The address actually bound — with a `:0` port, the one the kernel chose.
    pub addr: SocketAddr,
    shutdown: tokio::sync::oneshot::Sender<()>,
    joined: tokio::task::JoinHandle<()>,
}

impl Running {
    /// Stop listening and wait for in-flight requests to finish.
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(());
        let _ = self.joined.await;
    }
}

/// Bind and serve. Returns once the listener is up, with the address it got.
pub async fn serve(session: Arc<dyn Session>, config: ServerConfig) -> std::io::Result<Running> {
    http::serve(session, config).await
}

/// The axum router, for tests that would rather call it than bind a port.
pub fn router(session: Arc<dyn Session>, config: ServerConfig) -> axum::Router {
    http::router(session, config)
}

#[doc(hidden)]
pub mod internal {
    //! Plumbing `serve` needs and nobody else should.
    pub use super::{Running, ServerConfig};
}

impl Running {
    #[doc(hidden)]
    pub fn new(
        addr: SocketAddr,
        shutdown: tokio::sync::oneshot::Sender<()>,
        joined: tokio::task::JoinHandle<()>,
    ) -> Running {
        Running {
            addr,
            shutdown,
            joined,
        }
    }
}
