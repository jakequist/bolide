//! Computer-use action → RFB operations. **Pure.** See the crate docs for why.
//!
//! CONTRACT — `bolide-server`'s to finish. The rules [`plan`] must implement, each of
//! which is a test:
//!
//! - **Every click is `move → press → release`**, three PointerEvents, in that order.
//!   RFB has no "click at (x, y)": a press carries whatever position it carries, so a
//!   press sent before the pointer has arrived clicks the *previous* location. This is
//!   the single most common way a VNC automation misfires.
//! - **`double_click` / `triple_click`** are one move followed by two/three
//!   press+release pairs — not two/three independent clicks, because re-sending the
//!   move between them is harmless but re-deriving the position is not.
//! - **`left_click_drag`** is `move(start) → press(start) → move(end) → release(end)`.
//!   The release must carry the *end* coordinate; releasing at the start cancels the
//!   drag on most toolkits.
//! - **`scroll`** is a move to the coordinate, then `amount` × (press wheel-button,
//!   release wheel-button) — `bolide_rfb::proto::BUTTON_WHEEL_UP` / `_DOWN`. The leading
//!   move is load-bearing for the same reason a click's is: a wheel notch scrolls what
//!   is under the cursor. `scroll_amount` of 0 plans **nothing at all**, not even the
//!   move: relocating the pointer for a scroll of nothing changes what the next wheel
//!   event would land on.
//! - **`type`** is, per character, `key(sym, down) → key(sym, up)` via
//!   `bolide_rfb::keysym::keysym_for_char`. A `\n` types `Return`. No modifier is
//!   synthesised for uppercase: an X keysym for `A` is a distinct keysym from `a`, and
//!   servers handle the shift themselves. Text is typed in full even when long; any
//!   inter-key delay belongs in [`RfbOp::Sleep`], not in the caller.
//! - **`key`** is `bolide_rfb::keysym::parse_chord` pressed in order and released in
//!   **reverse** order, so modifiers surround the key. An unknown name is
//!   [`ActionError::UnknownKey`], never a silently dropped keypress.
//! - **`wait`** is a single [`RfbOp::Sleep`], clamped to `wire::MAX_WAIT_SECONDS`.
//! - **`screenshot`** and **`cursor_position`** plan *no* ops — they are answered from
//!   state, and returning an empty plan (rather than being special-cased upstream) is
//!   what keeps the HTTP layer from growing branches.
//! - **Bounds.** A coordinate outside `0..width` / `0..height`, or negative, is
//!   [`ActionError::OutOfBounds`]. bolide does not clamp: a model that thinks the screen
//!   is bigger than it is needs to be told, not quietly redirected to the edge.

use bolide_rfb::keysym::UnknownKey;
use bolide_rfb::proto::{
    BUTTON_LEFT, BUTTON_MIDDLE, BUTTON_RIGHT, BUTTON_WHEEL_DOWN, BUTTON_WHEEL_UP,
};
use bolide_rfb::Session;

use crate::wire::{ComputerAction, ScrollDirection, MAX_SCROLL_AMOUNT, MAX_WAIT_SECONDS};

/// One thing to do to the RFB session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RfbOp {
    /// PointerEvent: position plus the button mask held *after* this event.
    Pointer {
        /// X in framebuffer pixels.
        x: u16,
        /// Y in framebuffer pixels.
        y: u16,
        /// `bolide_rfb::proto::BUTTON_*` mask.
        buttons: u8,
    },
    /// KeyEvent.
    Key {
        /// X11 keysym.
        keysym: u32,
        /// Down or up.
        down: bool,
    },
    /// Wait. Used by `wait`, and available for inter-key pacing.
    Sleep {
        /// Milliseconds.
        ms: u64,
    },
}

/// What [`plan`] needs to know about the session.
#[derive(Clone, Copy, Debug)]
pub struct PlanContext {
    /// Framebuffer width.
    pub width: u16,
    /// Framebuffer height.
    pub height: u16,
}

/// Why an action could not be planned.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ActionError {
    /// A coordinate is off-screen.
    #[error("coordinate ({x}, {y}) is outside the {width}x{height} screen")]
    OutOfBounds {
        /// The x that was out of range.
        x: i32,
        /// The y that was out of range.
        y: i32,
        /// Screen width.
        width: u16,
        /// Screen height.
        height: u16,
    },
    /// A key name bolide does not know.
    #[error("{0}")]
    UnknownKey(#[from] UnknownKey),
}

impl PlanContext {
    /// Check one coordinate and return it as pixel indices.
    ///
    /// No clamping, on purpose: `0..width` and `0..height` are the only coordinates a
    /// desktop has, and a caller that thinks the screen is bigger has a bug that a
    /// silent redirect to the edge would hide.
    fn check(&self, coordinate: [i32; 2]) -> std::result::Result<(u16, u16), ActionError> {
        let [x, y] = coordinate;
        if x < 0 || y < 0 || x >= i32::from(self.width) || y >= i32::from(self.height) {
            return Err(ActionError::OutOfBounds {
                x,
                y,
                width: self.width,
                height: self.height,
            });
        }
        Ok((x as u16, y as u16))
    }
}

/// `move → press → release` at one point, `count` times over a single move.
fn clicks_at(x: u16, y: u16, buttons: u8, count: usize) -> Vec<RfbOp> {
    let mut ops = Vec::with_capacity(1 + 2 * count);
    ops.push(RfbOp::Pointer { x, y, buttons: 0 });
    for _ in 0..count {
        ops.push(RfbOp::Pointer { x, y, buttons });
        ops.push(RfbOp::Pointer { x, y, buttons: 0 });
    }
    ops
}

/// One character's keysym, with the control characters a `type` action really carries
/// routed to their named keys.
///
/// `keysym_for_char` maps a Latin-1 code point to itself, and `\n` (0x0a) is not a key
/// — `Return` (0xff0d) is. The three names come from `bolide_rfb::keysym`; this does not
/// hold a table of its own.
fn keysym_for_typed(c: char) -> std::result::Result<u32, UnknownKey> {
    let named = match c {
        '\n' | '\r' => "Return",
        '\t' => "Tab",
        _ => return Ok(bolide_rfb::keysym::keysym_for_char(c)),
    };
    bolide_rfb::keysym::keysym_for(named).ok_or_else(|| UnknownKey(named.to_string()))
}

/// Turn one action into the RFB operations that perform it.
pub fn plan(
    action: &ComputerAction,
    ctx: PlanContext,
) -> std::result::Result<Vec<RfbOp>, ActionError> {
    Ok(match action {
        // Answered from state, not from the wire.
        ComputerAction::Screenshot | ComputerAction::CursorPosition => Vec::new(),

        ComputerAction::LeftClick { coordinate } => {
            let (x, y) = ctx.check(*coordinate)?;
            clicks_at(x, y, BUTTON_LEFT, 1)
        }
        ComputerAction::RightClick { coordinate } => {
            let (x, y) = ctx.check(*coordinate)?;
            clicks_at(x, y, BUTTON_RIGHT, 1)
        }
        ComputerAction::MiddleClick { coordinate } => {
            let (x, y) = ctx.check(*coordinate)?;
            clicks_at(x, y, BUTTON_MIDDLE, 1)
        }
        ComputerAction::DoubleClick { coordinate } => {
            let (x, y) = ctx.check(*coordinate)?;
            clicks_at(x, y, BUTTON_LEFT, 2)
        }
        ComputerAction::TripleClick { coordinate } => {
            let (x, y) = ctx.check(*coordinate)?;
            clicks_at(x, y, BUTTON_LEFT, 3)
        }

        ComputerAction::LeftClickDrag {
            start_coordinate,
            coordinate,
        } => {
            let (sx, sy) = ctx.check(*start_coordinate)?;
            let (ex, ey) = ctx.check(*coordinate)?;
            vec![
                RfbOp::Pointer {
                    x: sx,
                    y: sy,
                    buttons: 0,
                },
                RfbOp::Pointer {
                    x: sx,
                    y: sy,
                    buttons: BUTTON_LEFT,
                },
                RfbOp::Pointer {
                    x: ex,
                    y: ey,
                    buttons: BUTTON_LEFT,
                },
                RfbOp::Pointer {
                    x: ex,
                    y: ey,
                    buttons: 0,
                },
            ]
        }

        ComputerAction::MouseMove { coordinate } => {
            let (x, y) = ctx.check(*coordinate)?;
            vec![RfbOp::Pointer { x, y, buttons: 0 }]
        }

        ComputerAction::Scroll {
            coordinate,
            scroll_direction,
            scroll_amount,
        } => {
            // The coordinate is checked even for a scroll of nothing: a caller that
            // named an off-screen point has a bug either way.
            let (x, y) = ctx.check(*coordinate)?;
            let notches = (*scroll_amount).min(MAX_SCROLL_AMOUNT);
            if notches == 0 {
                // Not even the move. Relocating the pointer for a scroll of nothing
                // changes what the *next* wheel event would land on.
                Vec::new()
            } else {
                let button = match scroll_direction {
                    ScrollDirection::Up => BUTTON_WHEEL_UP,
                    ScrollDirection::Down => BUTTON_WHEEL_DOWN,
                };
                clicks_at(x, y, button, notches as usize)
            }
        }

        ComputerAction::Type { text } => {
            let mut ops = Vec::with_capacity(text.chars().count() * 2);
            for c in text.chars() {
                let keysym = keysym_for_typed(c)?;
                ops.push(RfbOp::Key { keysym, down: true });
                ops.push(RfbOp::Key {
                    keysym,
                    down: false,
                });
            }
            ops
        }

        ComputerAction::Key { text } => {
            let syms = bolide_rfb::keysym::parse_chord(text)?;
            let mut ops = Vec::with_capacity(syms.len() * 2);
            for &keysym in &syms {
                ops.push(RfbOp::Key { keysym, down: true });
            }
            // Reverse, so the modifiers surround the key they modify.
            for &keysym in syms.iter().rev() {
                ops.push(RfbOp::Key {
                    keysym,
                    down: false,
                });
            }
            ops
        }

        ComputerAction::Wait { duration } => {
            // NaN first: `clamp` returns it unchanged, and `as u64` would then
            // silently mean "no wait" without anyone having decided that.
            let seconds = if duration.is_nan() {
                0.0
            } else {
                duration.clamp(0.0, MAX_WAIT_SECONDS)
            };
            vec![RfbOp::Sleep {
                ms: (seconds * 1000.0) as u64,
            }]
        }
    })
}

/// Hand a plan to a session, op by op.
///
/// `cursor` is updated with every PointerEvent actually sent — including the ones sent
/// before a failure, because the pointer really did move — which is what
/// `cursor_position` reports. RFB has no message that asks a server where the cursor
/// is, so bolide's own record is the only answer there is.
pub async fn execute(
    session: &dyn Session,
    ops: &[RfbOp],
    cursor: &mut [i32; 2],
) -> bolide_rfb::Result<()> {
    for op in ops {
        match *op {
            RfbOp::Pointer { x, y, buttons } => {
                session.pointer(x, y, buttons).await?;
                *cursor = [i32::from(x), i32::from(y)];
            }
            RfbOp::Key { keysym, down } => session.key(keysym, down).await?,
            RfbOp::Sleep { ms } => {
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{ScrollDirection, MAX_SCROLL_AMOUNT, MAX_WAIT_SECONDS};
    use bolide_rfb::proto::{
        BUTTON_LEFT, BUTTON_MIDDLE, BUTTON_RIGHT, BUTTON_WHEEL_DOWN, BUTTON_WHEEL_UP,
    };

    const CTX: PlanContext = PlanContext {
        width: 800,
        height: 600,
    };

    fn ops(action: ComputerAction) -> Vec<RfbOp> {
        plan(&action, CTX).expect("planned")
    }

    fn pointer(x: u16, y: u16, buttons: u8) -> RfbOp {
        RfbOp::Pointer { x, y, buttons }
    }

    #[test]
    fn a_click_moves_before_it_presses() {
        assert_eq!(
            ops(ComputerAction::LeftClick {
                coordinate: [42, 17]
            }),
            vec![
                pointer(42, 17, 0),
                pointer(42, 17, BUTTON_LEFT),
                pointer(42, 17, 0),
            ]
        );
    }

    #[test]
    fn each_button_has_its_own_mask() {
        let cases = [
            (
                ComputerAction::LeftClick { coordinate: [1, 2] },
                BUTTON_LEFT,
            ),
            (
                ComputerAction::MiddleClick { coordinate: [1, 2] },
                BUTTON_MIDDLE,
            ),
            (
                ComputerAction::RightClick { coordinate: [1, 2] },
                BUTTON_RIGHT,
            ),
        ];
        for (action, mask) in cases {
            assert_eq!(
                ops(action.clone()),
                vec![pointer(1, 2, 0), pointer(1, 2, mask), pointer(1, 2, 0)],
                "planning {action:?}"
            );
        }
    }

    #[test]
    fn a_double_click_is_one_move_and_two_pairs() {
        assert_eq!(
            ops(ComputerAction::DoubleClick { coordinate: [5, 6] }),
            vec![
                pointer(5, 6, 0),
                pointer(5, 6, BUTTON_LEFT),
                pointer(5, 6, 0),
                pointer(5, 6, BUTTON_LEFT),
                pointer(5, 6, 0),
            ]
        );
    }

    #[test]
    fn a_triple_click_is_one_move_and_three_pairs() {
        let planned = ops(ComputerAction::TripleClick { coordinate: [5, 6] });
        assert_eq!(planned.len(), 7, "one move plus three press/release pairs");
        assert_eq!(planned[0], pointer(5, 6, 0));
        assert_eq!(
            planned[1..],
            [
                pointer(5, 6, BUTTON_LEFT),
                pointer(5, 6, 0),
                pointer(5, 6, BUTTON_LEFT),
                pointer(5, 6, 0),
                pointer(5, 6, BUTTON_LEFT),
                pointer(5, 6, 0),
            ]
        );
    }

    /// Releasing at the *start* cancels the drag on most toolkits; this is the whole
    /// reason the drag is not two clicks.
    #[test]
    fn a_drag_releases_at_the_end_coordinate() {
        assert_eq!(
            ops(ComputerAction::LeftClickDrag {
                start_coordinate: [10, 20],
                coordinate: [30, 40],
            }),
            vec![
                pointer(10, 20, 0),
                pointer(10, 20, BUTTON_LEFT),
                pointer(30, 40, BUTTON_LEFT),
                pointer(30, 40, 0),
            ]
        );
    }

    #[test]
    fn a_mouse_move_presses_nothing() {
        assert_eq!(
            ops(ComputerAction::MouseMove { coordinate: [7, 8] }),
            vec![pointer(7, 8, 0)]
        );
    }

    #[test]
    fn a_scroll_moves_first_then_sends_notches() {
        for (direction, mask) in [
            (ScrollDirection::Up, BUTTON_WHEEL_UP),
            (ScrollDirection::Down, BUTTON_WHEEL_DOWN),
        ] {
            assert_eq!(
                ops(ComputerAction::Scroll {
                    coordinate: [3, 4],
                    scroll_direction: direction,
                    scroll_amount: 2,
                }),
                vec![
                    pointer(3, 4, 0),
                    pointer(3, 4, mask),
                    pointer(3, 4, 0),
                    pointer(3, 4, mask),
                    pointer(3, 4, 0),
                ],
                "scrolling {direction:?}"
            );
        }
    }

    /// Relocating the pointer for a scroll of nothing changes what the *next* wheel
    /// event would land on, so a zero scroll plans nothing at all.
    #[test]
    fn a_scroll_of_zero_notches_plans_nothing_at_all() {
        assert_eq!(
            ops(ComputerAction::Scroll {
                coordinate: [3, 4],
                scroll_direction: ScrollDirection::Up,
                scroll_amount: 0,
            }),
            vec![]
        );
    }

    #[test]
    fn an_over_large_scroll_is_clamped_not_refused() {
        let planned = ops(ComputerAction::Scroll {
            coordinate: [3, 4],
            scroll_direction: ScrollDirection::Down,
            scroll_amount: 10_000,
        });
        assert_eq!(planned.len(), 1 + 2 * MAX_SCROLL_AMOUNT as usize);
    }

    #[test]
    fn wait_is_one_sleep_clamped_to_the_maximum() {
        assert_eq!(
            ops(ComputerAction::Wait { duration: 1.5 }),
            vec![RfbOp::Sleep { ms: 1500 }]
        );
        assert_eq!(
            ops(ComputerAction::Wait { duration: 9_000.0 }),
            vec![RfbOp::Sleep {
                ms: (MAX_WAIT_SECONDS * 1000.0) as u64
            }]
        );
        assert_eq!(
            ops(ComputerAction::Wait { duration: -3.0 }),
            vec![RfbOp::Sleep { ms: 0 }],
            "a negative wait is zero, not a panic"
        );
    }

    /// Answered from state. Returning an empty plan rather than being special-cased
    /// upstream is what keeps the HTTP layer from growing branches.
    #[test]
    fn screenshot_and_cursor_position_plan_nothing() {
        assert_eq!(ops(ComputerAction::Screenshot), vec![]);
        assert_eq!(ops(ComputerAction::CursorPosition), vec![]);
    }

    #[test]
    fn an_off_screen_coordinate_is_an_error_not_a_clamp() {
        for coordinate in [[800, 10], [10, 600], [-1, 10], [10, -1], [9999, 9999]] {
            let err = plan(&ComputerAction::LeftClick { coordinate }, CTX)
                .expect_err("should be out of bounds");
            assert_eq!(
                err,
                ActionError::OutOfBounds {
                    x: coordinate[0],
                    y: coordinate[1],
                    width: 800,
                    height: 600,
                },
                "planning a click at {coordinate:?}"
            );
        }
    }

    #[test]
    fn the_last_pixel_is_in_bounds_and_the_next_one_is_not() {
        assert!(plan(
            &ComputerAction::MouseMove {
                coordinate: [799, 599]
            },
            CTX
        )
        .is_ok());
        assert!(plan(
            &ComputerAction::MouseMove {
                coordinate: [800, 599]
            },
            CTX
        )
        .is_err());
        assert!(plan(
            &ComputerAction::MouseMove {
                coordinate: [799, 600]
            },
            CTX
        )
        .is_err());
    }

    /// Both ends of a drag are checked.
    #[test]
    fn a_drag_checks_both_of_its_coordinates() {
        assert!(plan(
            &ComputerAction::LeftClickDrag {
                start_coordinate: [10, 20],
                coordinate: [900, 20],
            },
            CTX
        )
        .is_err());
        assert!(plan(
            &ComputerAction::LeftClickDrag {
                start_coordinate: [-1, 20],
                coordinate: [30, 40],
            },
            CTX
        )
        .is_err());
    }

    #[test]
    fn a_scroll_checks_its_coordinate() {
        assert!(plan(
            &ComputerAction::Scroll {
                coordinate: [800, 0],
                scroll_direction: ScrollDirection::Up,
                scroll_amount: 1,
            },
            CTX
        )
        .is_err());
    }

    #[test]
    fn typing_is_one_down_and_one_up_per_character() {
        let a = bolide_rfb::keysym::keysym_for_char('a');
        let b = bolide_rfb::keysym::keysym_for_char('b');
        assert_eq!(
            ops(ComputerAction::Type { text: "ab".into() }),
            vec![
                RfbOp::Key {
                    keysym: a,
                    down: true
                },
                RfbOp::Key {
                    keysym: a,
                    down: false
                },
                RfbOp::Key {
                    keysym: b,
                    down: true
                },
                RfbOp::Key {
                    keysym: b,
                    down: false
                },
            ]
        );
    }

    /// No shift is synthesised for an uppercase letter: the X keysym for `A` is a
    /// distinct keysym from `a`, and the server handles the shift itself.
    #[test]
    fn typing_an_uppercase_letter_synthesises_no_modifier() {
        assert_eq!(
            ops(ComputerAction::Type { text: "A".into() }).len(),
            2,
            "one down and one up, with no shift around them"
        );
    }

    #[test]
    fn typing_a_newline_types_return() {
        let ret = bolide_rfb::keysym::parse_chord("Return").expect("Return is a key")[0];
        assert_eq!(
            ops(ComputerAction::Type { text: "\n".into() }),
            vec![
                RfbOp::Key {
                    keysym: ret,
                    down: true
                },
                RfbOp::Key {
                    keysym: ret,
                    down: false
                },
            ]
        );
    }

    /// Modifiers must surround the key: press in order, release in reverse.
    #[test]
    fn a_chord_releases_in_reverse_order() {
        let syms = bolide_rfb::keysym::parse_chord("ctrl+shift+t").expect("a known chord");
        assert_eq!(syms.len(), 3, "the chord parser owes three keysyms");
        let mut expected: Vec<RfbOp> = syms
            .iter()
            .map(|&keysym| RfbOp::Key { keysym, down: true })
            .collect();
        expected.extend(syms.iter().rev().map(|&keysym| RfbOp::Key {
            keysym,
            down: false,
        }));
        assert_eq!(
            ops(ComputerAction::Key {
                text: "ctrl+shift+t".into()
            }),
            expected
        );
    }

    #[test]
    fn an_unknown_key_name_is_an_error_not_a_dropped_keypress() {
        let err = plan(
            &ComputerAction::Key {
                text: "nosuchkeyatall".into(),
            },
            CTX,
        )
        .expect_err("an unknown name must not plan a keypress");
        assert!(
            matches!(err, ActionError::UnknownKey(_)),
            "got {err:?} instead"
        );
    }
}
