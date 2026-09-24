//! What each subcommand becomes on the wire, and where it is sent.
//!
//! The types are `bolide_server::wire`'s, deliberately: asserting on those values is what
//! makes a rename in the server a compile error here rather than a 400 at runtime.

use bolide_cli::api::{self, url_for};
use bolide_cli::cli::{Button, Dir};
use bolide_cli::error::{EXIT_FAILURE, EXIT_NOT_CONNECTED, EXIT_USAGE};
use bolide_server::wire::{ComputerAction, ErrorCode, ErrorResponse, ScrollDirection};

#[test]
fn click_maps_each_button_to_its_own_action() {
    assert_eq!(
        api::action_for_click(420, 180, "left"),
        ComputerAction::LeftClick {
            coordinate: [420, 180]
        }
    );
    assert_eq!(
        api::action_for_click(1, 2, "right"),
        ComputerAction::RightClick { coordinate: [1, 2] }
    );
    assert_eq!(
        api::action_for_click(1, 2, "middle"),
        ComputerAction::MiddleClick { coordinate: [1, 2] }
    );
}

#[test]
fn the_button_names_the_parser_accepts_are_the_names_the_mapping_knows() {
    // One statement: clap's value_enum and `action_for_click` read the same enum, so a
    // button that parses cannot fall through to a left click.
    for button in [Button::Left, Button::Right, Button::Middle] {
        assert_eq!(
            api::action_for_click(7, 8, button.as_str()),
            button.click_at(7, 8),
            "{button:?}"
        );
    }
    assert_eq!(Button::from_name("sideways"), None);
}

#[test]
fn move_type_and_key_map_straight_through() {
    assert_eq!(
        api::action_for_move(3, 4),
        ComputerAction::MouseMove { coordinate: [3, 4] }
    );
    assert_eq!(
        api::action_for_type("hello"),
        ComputerAction::Type {
            text: "hello".into()
        }
    );
    assert_eq!(
        api::action_for_key("cmd+space"),
        ComputerAction::Key {
            text: "cmd+space".into()
        }
    );
}

#[test]
fn scroll_carries_the_coordinate_the_direction_and_the_amount() {
    assert_eq!(
        api::action_for_scroll(5, 6, Dir::Down, 3),
        ComputerAction::Scroll {
            coordinate: [5, 6],
            scroll_direction: ScrollDirection::Down,
            scroll_amount: 3,
        }
    );
    assert_eq!(
        api::action_for_scroll(0, 0, Dir::Up, 1),
        ComputerAction::Scroll {
            coordinate: [0, 0],
            scroll_direction: ScrollDirection::Up,
            scroll_amount: 1,
        }
    );
}

#[test]
fn urls_join_without_doubling_or_dropping_a_slash() {
    assert_eq!(
        url_for("http://127.0.0.1:53211", "/computer"),
        "http://127.0.0.1:53211/computer"
    );
    assert_eq!(
        url_for("http://127.0.0.1:53211/", "/screenshot"),
        "http://127.0.0.1:53211/screenshot"
    );
    assert_eq!(
        url_for("http://127.0.0.1:53211", "clipboard"),
        "http://127.0.0.1:53211/clipboard"
    );
    assert_eq!(
        url_for("http://127.0.0.1:53211", "/status"),
        "http://127.0.0.1:53211/status"
    );
}

#[test]
fn a_server_error_body_becomes_the_message_the_user_sees() {
    let body = serde_json::to_vec(&ErrorResponse::new(
        ErrorCode::OutOfBounds,
        "coordinate (9999, 10) is outside the 1728x1117 screen",
    ))
    .unwrap();
    let err = api::error_from_response(400, &body);
    assert_eq!(err.code, EXIT_FAILURE);
    assert!(
        err.message.contains("outside the 1728x1117 screen"),
        "{}",
        err.message
    );
}

#[test]
fn a_disconnected_session_is_exit_3_not_a_generic_failure() {
    let body = serde_json::to_vec(&ErrorResponse::new(
        ErrorCode::Disconnected,
        "the RFB session is gone",
    ))
    .unwrap();
    assert_eq!(
        api::error_from_response(503, &body).code,
        EXIT_NOT_CONNECTED
    );
}

#[test]
fn a_rejected_token_is_a_usage_error() {
    let body =
        serde_json::to_vec(&ErrorResponse::new(ErrorCode::Unauthorized, "no token")).unwrap();
    assert_eq!(api::error_from_response(401, &body).code, EXIT_USAGE);
}

#[test]
fn an_unparseable_error_body_still_says_something_useful() {
    let err = api::error_from_response(500, b"<html>oh no</html>");
    assert_eq!(err.code, EXIT_FAILURE);
    assert!(err.message.contains("500"), "{}", err.message);
}
