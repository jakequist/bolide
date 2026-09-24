//! The axum layer.
//!
//! The routes, and what each answers with:
//!
//! | route | body | answer |
//! |---|---|---|
//! | `POST /computer` | [`crate::wire::ComputerAction`] | [`crate::wire::ComputerResponse`] |
//! | `GET /screenshot` | — | `image/png` bytes |
//! | `GET /screenshot?format=json` | — | [`crate::wire::Image`] |
//! | `GET /clipboard` | — | [`crate::wire::ClipboardResponse`] |
//! | `POST /clipboard` | [`crate::wire::ClipboardRequest`] | `{"ok": true}` |
//! | `GET /status` | — | [`crate::wire::StatusResponse`] |
//! | `GET /healthz` | — | `200 ok`, no auth |
//!
//! Rules:
//!
//! - **Screenshots are fresh.** Before encoding, if the session is unpainted, ask for a
//!   non-incremental update and wait up to `frame_timeout`; otherwise a short
//!   incremental refresh is enough. An unpainted framebuffer is a *black screen at
//!   exactly screen dimensions* — indistinguishable from a real black screen, and a
//!   model shown one reports the app as blank. A timeout on an already-painted frame
//!   answers with what we have; a timeout on an unpainted one is
//!   [`crate::wire::ErrorCode::Timeout`].
//! - **Errors map, they do not leak.** Every failure becomes
//!   [`crate::wire::ErrorResponse`] with the right code and an HTTP status: bad body →
//!   400, out of bounds / unknown key → 400, missing or wrong token → 401, session gone
//!   → 503, frame timeout → 504, anything else → 500. No `Debug` of an internal error
//!   reaches the body.
//! - **The token is compared in full**, and `/healthz` is exempt so a supervisor can
//!   probe without holding it.
//! - **Nothing logs a password**, because nothing here ever sees one: the RFB password
//!   lives and dies in the CLI's `connect`.
//!
//! Every decision worth testing is either in [`crate::plan`] or in one of the small
//! pure helpers below (`wants_json`, `token_matches`, `bearer`); the handlers
//! themselves are argument shuffling, and `tests/routes.rs` drives all of them against
//! a recording fake `Session`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{RawQuery, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use bolide_rfb::{Error as RfbError, Framebuffer, Session};
use serde::de::DeserializeOwned;

use crate::actions::{execute, plan, ActionError, PlanContext};
use crate::image::encode_png;
use crate::wire::{
    ClipboardRequest, ClipboardResponse, ComputerAction, ComputerResponse, ErrorCode,
    ErrorResponse, Image, StatusResponse,
};
use crate::{Running, ServerConfig};

/// How long an *already painted* screen waits for an incremental update before
/// answering with the frame it has.
///
/// Short on purpose, and capped by `frame_timeout`: on a still desktop nothing changes,
/// so the full frame budget would be spent on every screenshot for no new pixels. The
/// long wait is reserved for the case where waiting is the only honest answer — an
/// unpainted framebuffer.
const INCREMENTAL_WAIT: Duration = Duration::from_millis(250);

/// What a handler gets to work with.
struct AppState {
    session: Arc<dyn Session>,
    config: ServerConfig,
    /// **The desktop's one pair of hands**, and bolide's own last PointerEvent behind it.
    ///
    /// Held for the whole length of an action, which is the only thing that makes an
    /// action mean anything. A desktop has one pointer and one keyboard; two `/computer`
    /// requests running concurrently reach it braided together — a `type "aaaa"` racing a
    /// `type "bbbb"` types `abbabab a` into whatever had focus, and a press from one
    /// action with a release from another is a drag that never ends. Nothing about the
    /// HTTP layer prevents that on its own: axum runs handlers concurrently, and the
    /// session actor's queue preserves the order commands arrive in, not the order a
    /// caller meant them in.
    ///
    /// A `tokio` mutex rather than a `std` one because it is deliberately held across
    /// awaits. The cursor lives inside it rather than beside it so there is one lock,
    /// taken once: RFB has no message that asks a server where the pointer is, so this
    /// record is the only answer `cursor_position` can give, and a record updated
    /// outside the lock would be a third caller's position.
    hands: tokio::sync::Mutex<[i32; 2]>,
}

// ------------------------------------------------------------------------- errors

/// A failure on its way out as [`ErrorResponse`].
struct ApiError {
    code: ErrorCode,
    message: String,
}

impl ApiError {
    fn new(code: ErrorCode, message: impl Into<String>) -> ApiError {
        ApiError {
            code,
            message: message.into(),
        }
    }

    /// Anything bolide cannot explain to a caller.
    ///
    /// The detail is logged, never sent: an internal error's text is bolide's innards,
    /// and a caller can do nothing with it but paste it somewhere it does not belong.
    fn internal(context: &'static str, detail: impl std::fmt::Display) -> ApiError {
        tracing::error!(%detail, "{context}");
        ApiError::new(
            ErrorCode::Internal,
            "bolide could not complete the request; see the server log",
        )
    }
}

/// The HTTP status each code answers with. The table is the protocol's.
fn status_for(code: ErrorCode) -> StatusCode {
    match code {
        ErrorCode::BadRequest | ErrorCode::OutOfBounds | ErrorCode::UnknownKey => {
            StatusCode::BAD_REQUEST
        }
        ErrorCode::Unauthorized => StatusCode::UNAUTHORIZED,
        ErrorCode::Disconnected => StatusCode::SERVICE_UNAVAILABLE,
        ErrorCode::Timeout => StatusCode::GATEWAY_TIMEOUT,
        ErrorCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            status_for(self.code),
            Json(ErrorResponse::new(self.code, self.message)),
        )
            .into_response()
    }
}

impl From<ActionError> for ApiError {
    fn from(err: ActionError) -> ApiError {
        // Both variants' `Display` is bolide's own sentence about the caller's own
        // request — the one case where the detail belongs in the body.
        let code = match err {
            ActionError::OutOfBounds { .. } => ErrorCode::OutOfBounds,
            ActionError::UnknownKey(_) => ErrorCode::UnknownKey,
        };
        ApiError::new(code, err.to_string())
    }
}

/// An RFB failure, classified for a caller who has to decide what to do next.
fn from_rfb(err: RfbError) -> ApiError {
    match err {
        RfbError::Closed => ApiError::new(
            ErrorCode::Disconnected,
            "the RFB session is gone; reconnect with `bolide connect`",
        ),
        RfbError::Timeout(after) => ApiError::new(
            ErrorCode::Timeout,
            format!("the desktop sent no frame within {after:?}"),
        ),
        other => ApiError::internal("the RFB session failed", other),
    }
}

// ------------------------------------------------------------------- pure helpers

/// Whether `?format=json` was asked for.
///
/// Hand-parsed because axum's `query` feature is off: bolide has exactly one query
/// parameter, and pulling in a form decoder to read it would be the larger surface.
fn wants_json(query: Option<&str>) -> bool {
    query.is_some_and(|q| {
        q.split('&')
            .any(|pair| pair.eq_ignore_ascii_case("format=json"))
    })
}

/// The token out of an `Authorization: Bearer …` header, scheme matched
/// case-insensitively because clients disagree about its spelling.
fn bearer(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim_start())
}

/// Compare a presented token with the configured one, in full.
///
/// No early exit on the first differing byte: how long the comparison takes must not
/// say how much of the token was right.
fn token_matches(presented: &str, expected: &str) -> bool {
    if presented.len() != expected.len() {
        return false;
    }
    presented
        .bytes()
        .zip(expected.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

fn parse_body<T: DeserializeOwned>(body: &Bytes) -> Result<T, ApiError> {
    serde_json::from_slice(body).map_err(|err| {
        ApiError::new(
            ErrorCode::BadRequest,
            format!("the request body did not parse: {err}"),
        )
    })
}

// ------------------------------------------------------------------------ freshness

/// The current screen, as fresh as this session can honestly make it.
async fn fresh_frame(state: &AppState) -> Result<Framebuffer, ApiError> {
    if !state.session.painted() {
        // Nothing to fall back on: the framebuffer is black at exactly screen
        // dimensions, which is a lie a model cannot detect. Ask for the whole screen,
        // wait the full budget, and say `timeout` rather than answer with the black.
        return state
            .session
            .refresh(false, state.config.frame_timeout)
            .await
            .map_err(from_rfb);
    }

    let wait = state.config.frame_timeout.min(INCREMENTAL_WAIT);
    match state.session.refresh(true, wait).await {
        Ok(frame) => Ok(frame),
        // A still screen produces no update. That is not a failure — the frame we
        // already have is current by definition.
        Err(RfbError::Timeout(_)) => state.session.framebuffer().await.map_err(from_rfb),
        Err(other) => Err(from_rfb(other)),
    }
}

fn to_png(frame: &Framebuffer) -> Result<Vec<u8>, ApiError> {
    encode_png(frame).map_err(|err| ApiError::internal("the framebuffer would not encode", err))
}

fn to_image(frame: &Framebuffer) -> Result<Image, ApiError> {
    Ok(Image {
        format: "png".to_string(),
        data: base64::engine::general_purpose::STANDARD.encode(to_png(frame)?),
        width: frame.width,
        height: frame.height,
    })
}

// ------------------------------------------------------------------------- handlers

async fn computer(State(state): State<Arc<AppState>>, body: Bytes) -> Result<Response, ApiError> {
    let action: ComputerAction = parse_body(&body)?;
    let (width, height) = state.session.size();
    let ops = plan(&action, PlanContext { width, height })?;

    // One action at a time, start to finish — see `AppState::hands`. The guard also
    // carries the cursor, and `execute` writes through it as the pointer moves, so a
    // run that fails part way still records where the pointer actually got to.
    let mut hands = state.hands.lock().await;
    let outcome = execute(state.session.as_ref(), &ops, &mut hands).await;
    let cursor = *hands;
    drop(hands);
    outcome.map_err(from_rfb)?;

    let mut response = ComputerResponse {
        ok: true,
        ..Default::default()
    };
    match action {
        ComputerAction::Screenshot => {
            response.image = Some(to_image(&fresh_frame(&state).await?)?);
        }
        ComputerAction::CursorPosition => response.coordinate = Some(cursor),
        _ => {}
    }
    Ok(Json(response).into_response())
}

async fn screenshot(
    State(state): State<Arc<AppState>>,
    RawQuery(query): RawQuery,
) -> Result<Response, ApiError> {
    let frame = fresh_frame(&state).await?;
    if wants_json(query.as_deref()) {
        return Ok(Json(to_image(&frame)?).into_response());
    }
    Ok(([(header::CONTENT_TYPE, "image/png")], to_png(&frame)?).into_response())
}

async fn clipboard_get(State(state): State<Arc<AppState>>) -> Response {
    Json(ClipboardResponse {
        text: state.session.last_cut_text(),
    })
    .into_response()
}

async fn clipboard_post(
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let request: ClipboardRequest = parse_body(&body)?;
    state
        .session
        .cut_text(&request.text)
        .await
        .map_err(from_rfb)?;
    Ok(Json(ComputerResponse {
        ok: true,
        ..Default::default()
    })
    .into_response())
}

/// Answers even when the desktop is gone — that is the question it exists to answer.
async fn status(State(state): State<Arc<AppState>>) -> Response {
    let (width, height) = state.session.size();
    Json(StatusResponse {
        connected: state.session.is_connected(),
        remote: state.config.remote.clone(),
        desktop_name: state.session.desktop_name(),
        width,
        height,
        painted: state.session.painted(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
    .into_response()
}

async fn healthz() -> &'static str {
    "ok"
}

async fn require_token(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let Some(expected) = state.config.token.as_deref() else {
        return next.run(request).await;
    };
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(bearer);
    match presented {
        Some(token) if token_matches(token, expected) => next.run(request).await,
        _ => ApiError::new(
            ErrorCode::Unauthorized,
            "this bolide requires `Authorization: Bearer <token>`",
        )
        .into_response(),
    }
}

/// Build the router. See the module docs for the routes.
pub(crate) fn router(session: Arc<dyn Session>, config: ServerConfig) -> Router {
    let state = Arc::new(AppState {
        session,
        config,
        hands: tokio::sync::Mutex::new([0, 0]),
    });

    Router::new()
        .route("/computer", post(computer))
        .route("/screenshot", get(screenshot))
        .route("/clipboard", get(clipboard_get).post(clipboard_post))
        .route("/status", get(status))
        // Applies to the routes registered *above* it only, which is how `/healthz`
        // stays exempt without the gate having to know a path by name.
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_token,
        ))
        .route("/healthz", get(healthz))
        // No body cap: axum's default 2 MiB would refuse a large clipboard paste, and
        // how much to paste is the caller's decision.
        .layer(axum::extract::DefaultBodyLimit::disable())
        .with_state(state)
}

/// Bind and spawn.
pub(crate) async fn serve(
    session: Arc<dyn Session>,
    config: ServerConfig,
) -> std::io::Result<Running> {
    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    let addr: SocketAddr = listener.local_addr()?;
    let app = router(session, config);
    let (tx, rx) = tokio::sync::oneshot::channel();
    let joined = tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await;
    });
    Ok(Running::new(addr, tx, joined))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_table_matches_the_protocol() {
        for (code, status) in [
            (ErrorCode::BadRequest, 400),
            (ErrorCode::OutOfBounds, 400),
            (ErrorCode::UnknownKey, 400),
            (ErrorCode::Unauthorized, 401),
            (ErrorCode::Disconnected, 503),
            (ErrorCode::Timeout, 504),
            (ErrorCode::Internal, 500),
        ] {
            assert_eq!(status_for(code).as_u16(), status, "status for {code:?}");
        }
    }

    #[test]
    fn only_format_json_asks_for_json() {
        assert!(wants_json(Some("format=json")));
        assert!(wants_json(Some("cache=0&format=json")));
        assert!(wants_json(Some("format=JSON")));
        assert!(!wants_json(None));
        assert!(!wants_json(Some("")));
        assert!(!wants_json(Some("format=png")));
        assert!(!wants_json(Some("formatted=json")));
    }

    #[test]
    fn a_bearer_header_yields_its_token() {
        assert_eq!(bearer("Bearer abc"), Some("abc"));
        assert_eq!(bearer("bearer abc"), Some("abc"));
        assert_eq!(bearer("BEARER  abc"), Some("abc"));
        assert_eq!(bearer("Basic abc"), None);
        assert_eq!(bearer("abc"), None);
    }

    /// In full: a prefix of the token is not the token, and neither is a suffix.
    #[test]
    fn a_token_is_compared_in_full() {
        assert!(token_matches("hunter2", "hunter2"));
        assert!(!token_matches("hunter", "hunter2"));
        assert!(!token_matches("hunter22", "hunter2"));
        assert!(!token_matches("hunter3", "hunter2"));
        assert!(!token_matches("", "hunter2"));
    }

    /// The one rule about internal errors: the detail is logged, never sent.
    #[test]
    fn an_internal_error_carries_none_of_its_detail() {
        let err = ApiError::internal("test", "zlib stream desynchronised at tile 27");
        assert_eq!(err.code, ErrorCode::Internal);
        assert!(!err.message.contains("zlib"), "leaked: {}", err.message);
        assert!(!err.message.contains("27"), "leaked: {}", err.message);
    }
}
