//! The CLI's HTTP client for its own server.
//!
//! Types come from [`bolide_server::wire`]; this module must not define its own copies
//! of the request or response bodies. That is the whole reason `bolide-cli` depends on
//! `bolide-server` at all — the wire is stated once, in one crate, and both ends of it
//! are compiled against that statement.
//!
//! URL building is a pure function ([`url_for`]) so the routes can be asserted without
//! a server: a typo'd path is a 404 at runtime and a diff at test time. So is the
//! mapping from a subcommand to an action, and so is the mapping from a server error
//! body to an exit code.

use std::time::Duration;

use bolide_server::wire::{
    ClipboardRequest, ClipboardResponse, ComputerAction, ComputerResponse, ErrorCode,
    ErrorResponse, ScrollDirection, StatusResponse,
};

use crate::cli::{Button, Dir};
use crate::error::CliError;
use crate::state::SessionState;

/// `POST /computer`.
pub const COMPUTER: &str = "/computer";
/// `GET /screenshot`.
pub const SCREENSHOT: &str = "/screenshot";
/// `GET`/`POST /clipboard`.
pub const CLIPBOARD: &str = "/clipboard";
/// `GET /status`.
pub const STATUS: &str = "/status";

/// Join a base URL and a route, without doubling or dropping the slash between them.
pub fn url_for(base: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

/// What `bolide click X Y --button B` becomes.
///
/// Resolves through [`Button`], which is also what clap validates `--button` against, so
/// there is one vocabulary rather than two that must agree. An unknown name cannot
/// arrive from the CLI; if one does, it is a left click and the caller has a bug.
pub fn action_for_click(x: i32, y: i32, button: &str) -> ComputerAction {
    Button::from_name(button)
        .unwrap_or(Button::Left)
        .click_at(x, y)
}

/// What `bolide move X Y` becomes.
pub fn action_for_move(x: i32, y: i32) -> ComputerAction {
    ComputerAction::MouseMove { coordinate: [x, y] }
}

/// What `bolide type TEXT` becomes.
pub fn action_for_type(text: &str) -> ComputerAction {
    ComputerAction::Type {
        text: text.to_string(),
    }
}

/// What `bolide key CHORD` becomes.
pub fn action_for_key(chord: &str) -> ComputerAction {
    ComputerAction::Key {
        text: chord.to_string(),
    }
}

/// What `bolide scroll X Y --dir D --amount N` becomes.
pub fn action_for_scroll(x: i32, y: i32, dir: Dir, amount: u32) -> ComputerAction {
    ComputerAction::Scroll {
        coordinate: [x, y],
        scroll_direction: ScrollDirection::from(dir),
        scroll_amount: amount,
    }
}

/// Turn a non-2xx answer into the error the user sees, and the code the shell sees.
///
/// The mapping that matters is `disconnected` → exit 3: the state file said there was a
/// session and the server says there is not, which is the same fact as "no state file"
/// from a script's point of view.
pub fn error_from_response(status: u16, body: &[u8]) -> CliError {
    match serde_json::from_slice::<ErrorResponse>(body) {
        Ok(err) => match err.error {
            ErrorCode::Disconnected => CliError::not_connected(err.message),
            ErrorCode::Unauthorized => CliError::usage(err.message),
            _ => CliError::failure(err.message),
        },
        Err(_) => {
            let text = String::from_utf8_lossy(body);
            let text = text.trim();
            if text.is_empty() {
                CliError::failure(format!("the bolide server answered {status}"))
            } else {
                CliError::failure(format!("the bolide server answered {status}: {text}"))
            }
        }
    }
}

/// An HTTP client pointed at one bolide server.
pub struct Client {
    base: String,
    token: Option<String>,
    http: reqwest::Client,
}

impl Client {
    /// A client for the session in the state file.
    pub fn for_session(state: &SessionState) -> Result<Client, CliError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| CliError::failure(format!("could not build an HTTP client: {e}")))?;
        Ok(Client {
            base: state.endpoint.clone(),
            token: state.token.clone(),
            http,
        })
    }

    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.authed(self.http.get(url_for(&self.base, path)))
    }

    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.authed(self.http.post(url_for(&self.base, path)))
    }

    fn authed(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(token) => req.bearer_auth(token),
            None => req,
        }
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<Vec<u8>, CliError> {
        let response = req.send().await.map_err(|e| self.unreachable(e))?;
        let status = response.status();
        let body = response.bytes().await.map_err(|e| {
            CliError::failure(format!("the bolide server cut the answer short: {e}"))
        })?;
        if status.is_success() {
            Ok(body.to_vec())
        } else {
            Err(error_from_response(status.as_u16(), &body))
        }
    }

    /// A server we cannot reach at all is the same situation as no server: the state
    /// file is a promise the daemon is no longer keeping.
    fn unreachable(&self, e: reqwest::Error) -> CliError {
        if e.is_connect() {
            CliError::not_connected(format!(
                "nothing is listening at {}; run `bolide connect vnc://HOST` (or `bolide disconnect` to clear the stale session)",
                self.base
            ))
        } else if e.is_timeout() {
            CliError::failure(format!(
                "the bolide server at {} did not answer in time",
                self.base
            ))
        } else {
            CliError::failure(format!(
                "could not reach the bolide server at {}: {e}",
                self.base
            ))
        }
    }

    fn json<T: serde::de::DeserializeOwned>(&self, body: &[u8]) -> Result<T, CliError> {
        serde_json::from_slice(body).map_err(|e| {
            CliError::failure(format!(
                "the bolide server answered something unexpected: {e}"
            ))
        })
    }

    /// `POST /computer`.
    pub async fn computer(&self, action: &ComputerAction) -> Result<ComputerResponse, CliError> {
        let body = self.send(self.post(COMPUTER).json(action)).await?;
        self.json(&body)
    }

    /// `GET /status`.
    pub async fn status(&self) -> Result<StatusResponse, CliError> {
        let body = self.send(self.get(STATUS)).await?;
        self.json(&body)
    }

    /// `GET /screenshot` — the raw PNG bytes.
    pub async fn screenshot(&self) -> Result<Vec<u8>, CliError> {
        self.send(self.get(SCREENSHOT)).await
    }

    /// `GET /clipboard`.
    pub async fn clipboard(&self) -> Result<ClipboardResponse, CliError> {
        let body = self.send(self.get(CLIPBOARD)).await?;
        self.json(&body)
    }

    /// `POST /clipboard`.
    pub async fn set_clipboard(&self, text: &str) -> Result<(), CliError> {
        let request = ClipboardRequest {
            text: text.to_string(),
        };
        self.send(self.post(CLIPBOARD).json(&request)).await?;
        Ok(())
    }
}
