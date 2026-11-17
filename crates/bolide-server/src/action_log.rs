//! What the server tells an action log about each request it handles.
//!
//! The server knows what was asked and when; where that goes, and in what format, is the
//! log's business ([`ActionLog`]). Logged: every `/computer` action, `/screenshot`, and
//! `/clipboard` read and write. Not logged: `/status` and `/healthz`, which ask about
//! bolide rather than act on the desktop, and a request refused for its token, which never
//! reached it.
//!
//! The parameters are the request **as sent**, typed text in full: a log of a session is
//! a log of what was typed into it.

use std::time::Instant;

use serde_json::{Map, Value};

/// One request the server handled.
#[derive(Clone, Debug, PartialEq)]
pub struct LoggedRequest {
    /// The `/computer` action's name (`left_click`, `type`, …), or [`SCREENSHOT`],
    /// [`CLIPBOARD_GET`] or [`CLIPBOARD_SET`]. A `/computer` body with no action name
    /// to read is [`COMPUTER`].
    pub action: String,
    /// The parameters as sent: the `/computer` body less its `action`, the clipboard
    /// body, the screenshot's query. A body that is not a JSON object is
    /// `{"body": "<the text>"}`.
    pub params: Map<String, Value>,
    /// When the server began on it, on the server's [`crate::Clock`].
    pub start: Instant,
    /// When the server had its answer, on the same clock.
    pub end: Instant,
    /// `None` when it was answered ok; otherwise the message the caller was sent.
    pub error: Option<String>,
}

/// Where the server sends a [`LoggedRequest`] for each request it handles.
///
/// `record` returns nothing on purpose: a log that cannot be written must not fail the
/// request, so what to do about it is the log's own decision.
pub trait ActionLog: Send + Sync + std::fmt::Debug {
    /// One request, called once it has its answer and before the answer is sent.
    fn record(&self, request: LoggedRequest);
}

/// `GET /screenshot`'s action name, which is also the `/computer` action's.
pub const SCREENSHOT: &str = "screenshot";
/// `GET /clipboard`'s action name.
pub const CLIPBOARD_GET: &str = "clipboard_get";
/// `POST /clipboard`'s action name.
pub const CLIPBOARD_SET: &str = "clipboard_set";
/// A `/computer` request whose body names no action.
pub const COMPUTER: &str = "computer";

/// A body's parameters: its fields when it is a JSON object, else its text.
fn body_params(body: &[u8]) -> Map<String, Value> {
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(fields)) => fields,
        _ => Map::from_iter([(
            "body".to_string(),
            Value::String(String::from_utf8_lossy(body).into_owned()),
        )]),
    }
}

/// A `/computer` body's action name and parameters.
pub(crate) fn describe_computer(body: &[u8]) -> (String, Map<String, Value>) {
    let mut params = body_params(body);
    match params.get("action") {
        Some(Value::String(name)) => {
            let name = name.clone();
            params.remove("action");
            (name, params)
        }
        _ => (COMPUTER.to_string(), params),
    }
}

/// A `POST /clipboard` body's parameters.
pub(crate) fn describe_clipboard_set(body: &[u8]) -> (String, Map<String, Value>) {
    (CLIPBOARD_SET.to_string(), body_params(body))
}

/// A `GET /screenshot` query's parameters: each `key=value` pair, as given.
pub(crate) fn describe_screenshot(query: Option<&str>) -> (String, Map<String, Value>) {
    let params = query
        .unwrap_or_default()
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (key.to_string(), Value::String(value.to_string()))
        })
        .collect();
    (SCREENSHOT.to_string(), params)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn object(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(fields) => fields,
            other => panic!("not an object: {other}"),
        }
    }

    #[test]
    fn a_computer_body_is_its_action_and_the_rest() {
        assert_eq!(
            describe_computer(br#"{"action":"type","text":"a\nb"}"#),
            ("type".to_string(), object(json!({"text": "a\nb"})))
        );
        assert_eq!(
            describe_computer(br#"{"action":"screenshot"}"#),
            ("screenshot".to_string(), Map::new())
        );
    }

    #[test]
    fn a_computer_body_without_an_action_name_is_computer() {
        assert_eq!(
            describe_computer(br#"{"action":7,"x":1}"#),
            ("computer".to_string(), object(json!({"action": 7, "x": 1})))
        );
        assert_eq!(
            describe_computer(b"[1, 2]"),
            ("computer".to_string(), object(json!({"body": "[1, 2]"})))
        );
    }

    #[test]
    fn a_screenshot_query_is_its_pairs() {
        assert_eq!(describe_screenshot(None).1, Map::new());
        assert_eq!(describe_screenshot(Some("")).1, Map::new());
        assert_eq!(
            describe_screenshot(Some("format=json&x")).1,
            object(json!({"format": "json", "x": ""}))
        );
    }
}
