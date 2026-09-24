//! The HTTP wire, as types.
//!
//! This module is the **one statement** of bolide's computer-use protocol: the server
//! answers with these types and the CLI (which depends on this crate) parses the same
//! ones, so a field cannot be renamed on one side only. `docs/protocol.md` describes
//! the same wire in prose for people writing a client in another language; if the two
//! ever disagree, this file is right and the prose is stale.
//!
//! # The action vocabulary
//!
//! Names are **1:1 with Anthropic's `computer` tool**, so an agent already holding that
//! tool's schema can point it at `http://127.0.0.1:<port>/computer` and send the tool
//! input unchanged. That is the whole design goal, and it is why there is no tidier
//! spelling here: `left_click` and not `click(button)`, `coordinate: [x, y]` and not
//! `{x, y}`, `duration` in *seconds* for `wait` while everything else is milliseconds.
//!
//! The vocabulary bolide implements:
//!
//! `screenshot`, `left_click`, `right_click`, `middle_click`, `double_click`,
//! `triple_click`, `left_click_drag`, `mouse_move`, `type`, `key`, `scroll`,
//! `cursor_position`, `wait`.

use serde::{Deserialize, Serialize};

/// One computer-use action, exactly as the `computer` tool spells it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ComputerAction {
    /// Capture the current screen. Answered with [`ComputerResponse::image`].
    Screenshot,
    /// Press and release button 1 at `coordinate`.
    LeftClick {
        /// `[x, y]` in framebuffer pixels.
        coordinate: [i32; 2],
    },
    /// Press and release button 3 at `coordinate`.
    RightClick {
        /// `[x, y]` in framebuffer pixels.
        coordinate: [i32; 2],
    },
    /// Press and release button 2 at `coordinate`.
    MiddleClick {
        /// `[x, y]` in framebuffer pixels.
        coordinate: [i32; 2],
    },
    /// Two button-1 clicks at `coordinate`.
    DoubleClick {
        /// `[x, y]` in framebuffer pixels.
        coordinate: [i32; 2],
    },
    /// Three button-1 clicks at `coordinate`.
    TripleClick {
        /// `[x, y]` in framebuffer pixels.
        coordinate: [i32; 2],
    },
    /// Press button 1 at `start_coordinate`, move to `coordinate`, release.
    LeftClickDrag {
        /// Where the drag starts.
        start_coordinate: [i32; 2],
        /// Where the drag ends.
        coordinate: [i32; 2],
    },
    /// Move the pointer without pressing anything.
    MouseMove {
        /// `[x, y]` in framebuffer pixels.
        coordinate: [i32; 2],
    },
    /// Type literal text.
    #[serde(rename = "type")]
    Type {
        /// The text to type, character by character.
        text: String,
    },
    /// Press a key or chord, e.g. `"Return"`, `"ctrl+c"`, `"cmd+space"`.
    Key {
        /// The chord, in `xdotool` spelling. See `bolide_rfb::keysym`.
        text: String,
    },
    /// Scroll under the pointer.
    Scroll {
        /// Where to put the pointer first — an RFB wheel notch scrolls whatever is
        /// under the cursor, so the move is part of the action, not a nicety.
        coordinate: [i32; 2],
        /// `up` or `down`. RFB has no horizontal wheel, so nothing else is accepted.
        scroll_direction: ScrollDirection,
        /// Notches. Every one is sent; there is no cap.
        scroll_amount: u32,
    },
    /// Where bolide last put the pointer.
    ///
    /// RFB has no message that asks a server where the cursor is — the protocol is
    /// write-only in that direction. So this reports bolide's own last PointerEvent, and
    /// is `[0, 0]` until something has moved the pointer. Honest, and documented as
    /// such, rather than a guess dressed as a reading.
    CursorPosition,
    /// Do nothing for `duration` **seconds** (the `computer` tool's unit).
    Wait {
        /// Seconds. Honoured in full; negative or NaN is zero.
        duration: f64,
    },
}

/// Which way a wheel notch goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScrollDirection {
    /// Wheel up (RFB button 4).
    Up,
    /// Wheel down (RFB button 5).
    Down,
}

/// A base64 PNG, as `/computer` returns it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Image {
    /// Always `"png"` in v0.
    pub format: String,
    /// Standard base64 (with padding) of the PNG bytes.
    pub data: String,
    /// Image width in pixels.
    pub width: u16,
    /// Image height in pixels.
    pub height: u16,
}

/// The answer to `POST /computer`.
///
/// One shape for every action, with the action-specific parts optional, so a client can
/// deserialize without knowing which action it sent.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ComputerResponse {
    /// True when the action was performed.
    pub ok: bool,
    /// Present for `screenshot`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub image: Option<Image>,
    /// Present for `cursor_position`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub coordinate: Option<[i32; 2]>,
}

/// Every error body bolide returns, at every endpoint.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ErrorResponse {
    /// Always false.
    pub ok: bool,
    /// A stable machine-readable code — see [`ErrorCode`].
    pub error: ErrorCode,
    /// A sentence a CLI can print unmodified.
    pub message: String,
}

impl ErrorResponse {
    /// Build one.
    pub fn new(error: ErrorCode, message: impl Into<String>) -> ErrorResponse {
        ErrorResponse {
            ok: false,
            error,
            message: message.into(),
        }
    }
}

/// Why a request failed. Stable strings: a client branches on these, not on the prose.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The body did not parse, or a field was out of range.
    BadRequest,
    /// A coordinate was outside the framebuffer.
    OutOfBounds,
    /// A key or chord bolide does not know.
    UnknownKey,
    /// `--token` is set and the request did not present it.
    Unauthorized,
    /// The RFB session is gone.
    Disconnected,
    /// The desktop did not produce a frame in time.
    Timeout,
    /// Anything else, including an RFB-level failure.
    Internal,
}

/// `GET /status`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StatusResponse {
    /// Whether the RFB session is live.
    pub connected: bool,
    /// The desktop bolide is attached to, `host:port`. Never carries credentials.
    pub remote: String,
    /// ServerInit's desktop name.
    pub desktop_name: String,
    /// Framebuffer width.
    pub width: u16,
    /// Framebuffer height.
    pub height: u16,
    /// False until the first FramebufferUpdate has been applied.
    pub painted: bool,
    /// bolide's version.
    pub version: String,
}

/// `GET /clipboard`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClipboardResponse {
    /// The most recent ServerCutText, or `null` if the desktop has never sent one.
    ///
    /// RFB clipboards are push-only — there is no message that asks a server for its
    /// selection — so this is what the desktop volunteered, not a live read.
    pub text: Option<String>,
}

/// `POST /clipboard`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClipboardRequest {
    /// Text to put on the remote clipboard. Lowered to latin-1 on the RFB wire.
    pub text: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire names are ABI: an agent sends the `computer` tool's own input object,
    /// so a rename here silently breaks every caller. Asserting the JSON, not the
    /// round-trip, is what makes that a visible diff.
    #[test]
    fn actions_serialize_with_the_computer_tool_spelling() {
        let cases: Vec<(ComputerAction, serde_json::Value)> = vec![
            (
                ComputerAction::Screenshot,
                serde_json::json!({"action": "screenshot"}),
            ),
            (
                ComputerAction::LeftClick {
                    coordinate: [10, 20],
                },
                serde_json::json!({"action": "left_click", "coordinate": [10, 20]}),
            ),
            (
                ComputerAction::TripleClick { coordinate: [1, 2] },
                serde_json::json!({"action": "triple_click", "coordinate": [1, 2]}),
            ),
            (
                ComputerAction::LeftClickDrag {
                    start_coordinate: [1, 2],
                    coordinate: [3, 4],
                },
                serde_json::json!({
                    "action": "left_click_drag",
                    "start_coordinate": [1, 2],
                    "coordinate": [3, 4]
                }),
            ),
            (
                ComputerAction::Type { text: "hi".into() },
                serde_json::json!({"action": "type", "text": "hi"}),
            ),
            (
                ComputerAction::Key {
                    text: "ctrl+c".into(),
                },
                serde_json::json!({"action": "key", "text": "ctrl+c"}),
            ),
            (
                ComputerAction::Scroll {
                    coordinate: [5, 6],
                    scroll_direction: ScrollDirection::Down,
                    scroll_amount: 3,
                },
                serde_json::json!({
                    "action": "scroll",
                    "coordinate": [5, 6],
                    "scroll_direction": "down",
                    "scroll_amount": 3
                }),
            ),
            (
                ComputerAction::CursorPosition,
                serde_json::json!({"action": "cursor_position"}),
            ),
            (
                ComputerAction::Wait { duration: 1.5 },
                serde_json::json!({"action": "wait", "duration": 1.5}),
            ),
            (
                ComputerAction::MouseMove { coordinate: [0, 0] },
                serde_json::json!({"action": "mouse_move", "coordinate": [0, 0]}),
            ),
            (
                ComputerAction::MiddleClick { coordinate: [7, 8] },
                serde_json::json!({"action": "middle_click", "coordinate": [7, 8]}),
            ),
            (
                ComputerAction::RightClick { coordinate: [7, 8] },
                serde_json::json!({"action": "right_click", "coordinate": [7, 8]}),
            ),
            (
                ComputerAction::DoubleClick { coordinate: [7, 8] },
                serde_json::json!({"action": "double_click", "coordinate": [7, 8]}),
            ),
        ];
        for (action, json) in cases {
            assert_eq!(
                serde_json::to_value(&action).unwrap(),
                json,
                "serializing {action:?}"
            );
            assert_eq!(
                serde_json::from_value::<ComputerAction>(json.clone()).unwrap(),
                action,
                "deserializing {json}"
            );
        }
    }

    #[test]
    fn an_ok_response_omits_the_optional_halves() {
        let body = serde_json::to_value(ComputerResponse {
            ok: true,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(body, serde_json::json!({"ok": true}));
    }

    #[test]
    fn error_codes_are_stable_strings() {
        assert_eq!(
            serde_json::to_value(ErrorResponse::new(ErrorCode::OutOfBounds, "nope")).unwrap(),
            serde_json::json!({"ok": false, "error": "out_of_bounds", "message": "nope"})
        );
    }

    #[test]
    fn an_unknown_action_does_not_deserialize() {
        let r: std::result::Result<ComputerAction, _> =
            serde_json::from_value(serde_json::json!({"action": "hold_key", "text": "a"}));
        assert!(r.is_err());
    }
}
