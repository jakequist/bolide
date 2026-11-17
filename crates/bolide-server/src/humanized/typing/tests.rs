//! Humanized typing as data. Ported from meteor's `desktops/interaction/tests/
//! humanized.test.ts` (commit `bba97bcb10c6`), its typing, key and chord tests and the
//! tempo test that types: what a person's typing looks like is asserted as properties of
//! the key events under a seeded stream. Not ported: the typo tests (`typoRate` is not
//! ported) and the settle tests (bolide plans no settle after the last key). Then bolide's
//! own: the ops on the wire, exact text, rollover, the stream, and the modes.

use super::*;
use crate::actions::{plan, DragPacing, PlanContext};
use crate::humanized::{plan_humanized, plan_input, PointerMode};
use crate::wire::ScrollDirection;

const SEEDS: std::ops::RangeInclusive<u32> = 1..=40;

const CTX: PlanContext = PlanContext {
    width: 1920,
    height: 1080,
    drag: DragPacing::DEFAULT,
};

fn typing(text: &str) -> ComputerAction {
    ComputerAction::Type { text: text.into() }
}

fn key(text: &str) -> ComputerAction {
    ComputerAction::Key { text: text.into() }
}

fn plan_at(action: ComputerAction, tempo: f64, seed: u32) -> Vec<KeyEvent> {
    keystrokes(&action, tempo, &mut SeededRandom::new(seed))
        .expect("planned")
        .expect("a typed act")
}

/// meteor's `ctx()`: tempo 1, seed 42.
fn strokes(action: ComputerAction) -> Vec<KeyEvent> {
    plan_at(action, 1.0, 42)
}

/// The keysyms a `type` of `text` must send, one per character: direct mode's.
fn text_keysyms(text: &str) -> Vec<u32> {
    text.chars()
        .map(|c| keysym_for_typed(c).expect("typeable"))
        .collect()
}

/// The text a key stream types: its key-downs, in order.
fn typed(events: &[KeyEvent]) -> Vec<u32> {
    events.iter().filter(|e| e.down).map(|e| e.keysym).collect()
}

fn non_decreasing(events: &[KeyEvent]) -> bool {
    events.windows(2).all(|w| w[1].t >= w[0].t)
}

fn keysym(name: &str) -> u32 {
    bolide_rfb::keysym::keysym_for(name).expect("a known key")
}

// --- typing (humanized.test.ts:240-358) -------------------------------------------------

const TEXT: &str = "Hello, the quick brown fox — jumps over 12 lazy dogs.\nOK!";

#[test]
fn types_exactly_the_text_asked_for() {
    for seed in SEEDS {
        let events = plan_at(typing(TEXT), 1.0, seed);
        assert_eq!(typed(&events), text_keysyms(TEXT), "seed {seed}");
        assert!(non_decreasing(&events), "seed {seed}");
    }
}

#[test]
fn every_key_that_goes_down_comes_up_and_a_key_is_never_pressed_twice_while_held() {
    let events = strokes(typing("bookkeeper aaa"));
    assert_eq!(events.len(), 28);
    let mut held = std::collections::HashSet::new();
    for e in &events {
        if e.down {
            assert!(held.insert(e.keysym), "{:#x} pressed while held", e.keysym);
        } else {
            assert!(held.remove(&e.keysym), "{:#x} released unheld", e.keysym);
        }
    }
    assert!(held.is_empty());
}

#[test]
fn intervals_vary_like_a_persons_a_mean_of_150_to_220_ms() {
    let text = "the rain in spain falls mainly on the plain and nobody minds it at all";
    let mut means = Vec::new();
    for seed in SEEDS {
        let downs: Vec<f64> = plan_at(typing(text), 1.0, seed)
            .iter()
            .filter(|e| e.down)
            .map(|e| e.t)
            .collect();
        let iv: Vec<f64> = downs.windows(2).map(|w| w[1] - w[0]).collect();
        let mu = iv.iter().sum::<f64>() / iv.len() as f64;
        let sd = (iv.iter().map(|x| (x - mu).powi(2)).sum::<f64>() / iv.len() as f64).sqrt();
        assert!(sd / mu > 0.25, "seed {seed}: cv {}", sd / mu);
        means.push(mu);
    }
    let mean = means.iter().sum::<f64>() / means.len() as f64;
    assert!((150.0..=220.0).contains(&mean), "mean interval {mean} ms");
}

#[test]
fn rolls_over_some_keys_go_down_before_the_previous_one_is_up() {
    let events = strokes(typing("there is another thing in the attic"));
    let (mut held, mut rolled) = (0, 0);
    for e in &events {
        if e.down {
            if held > 0 {
                rolled += 1;
            }
            held += 1;
        } else {
            held -= 1;
        }
    }
    assert!(rolled > 0);
}

/// A key the server types as itself, with no Shift of its own (meteor's test's `plain`).
fn plain(ks: u32) -> bool {
    char::from_u32(ks).is_some_and(|c| {
        c.is_ascii_lowercase() || c.is_ascii_digit() || " -=[]\\;',./`".contains(c)
    })
}

/// An uppercase letter or a shifted symbol makes the server press Shift for it; a
/// neighbour going down while that Shift is held would type shifted too.
#[test]
fn never_overlaps_a_key_the_server_must_shift_for_its_neighbours_are_up_first() {
    for seed in SEEDS {
        let events = plan_at(typing("aBc dEf G!h ?i"), 1.0, seed);
        let mut held = std::collections::HashSet::new();
        for e in &events {
            if e.down {
                if !held.is_empty() {
                    assert!(plain(e.keysym), "seed {seed}: {:#x} overlapped", e.keysym);
                    for &h in &held {
                        assert!(plain(h), "seed {seed}: {h:#x} held under another");
                    }
                }
                held.insert(e.keysym);
            } else {
                held.remove(&e.keysym);
            }
        }
    }
}

#[test]
fn types_nothing_for_no_text_and_draws_nothing() {
    let mut random = SeededRandom::new(42);
    let events = keystrokes(&typing(""), 1.0, &mut random).expect("planned");
    assert_eq!(events, Some(Vec::new()));
    assert_eq!(random, SeededRandom::new(42));
}

// --- keys and chords (humanized.test.ts:360-394) ----------------------------------------

/// meteor's chord test expects Shift to make the `t` a `T`; bolide's chord parser sends
/// `t`, as its direct mode always has, and humanized keys send the same keysyms.
#[test]
fn a_chord_modifiers_down_first_the_key_held_released_before_the_modifiers() {
    let ks = strokes(key("ctrl+shift+t"));
    let (ctrl, shift, t) = (keysym("ctrl"), keysym("shift"), keysym("t"));
    assert_eq!(
        ks.iter().map(|e| (e.keysym, e.down)).collect::<Vec<_>>(),
        vec![
            (ctrl, true),
            (shift, true),
            (t, true),
            (t, false),
            (shift, false),
            (ctrl, false),
        ]
    );
    let dwell = ks[3].t - ks[2].t;
    assert!((50.0..=130.0).contains(&dwell), "dwell {dwell}");
    assert!(ks[2].t > ks[1].t);
}

#[test]
fn a_single_key_is_held_for_a_human_dwell() {
    let ks = strokes(key("Return"));
    assert_eq!(ks.len(), 2);
    assert!(ks[1].t - ks[0].t >= 50.0);
}

#[test]
fn refuses_a_key_no_table_can_spell_as_direct_does_and_draws_nothing() {
    let mut random = SeededRandom::new(42);
    assert!(matches!(
        keystrokes(&key("ctrl+NoSuchKey"), 1.0, &mut random),
        Err(ActionError::UnknownKey(_))
    ));
    assert_eq!(random, SeededRandom::new(42));
}

/// Each step of a chord in meteor's ranges: modifiers 25–70 ms apart, a 40–110 ms lead to
/// the key, a 60–110 ms dwell, then each modifier up 15–60 ms after the last release.
#[test]
fn a_chords_gaps_fall_in_meteors_ranges() {
    for seed in SEEDS {
        let ks = plan_at(key("ctrl+alt+shift+t"), 1.0, seed);
        let gap = |i: usize| ks[i].t - ks[i - 1].t;
        for (i, (lo, hi)) in [
            (1, MODIFIER_STAGGER_MS),
            (2, MODIFIER_STAGGER_MS),
            (3, MODIFIER_LEAD_MS),
            (4, CHORD_DWELL_MS),
            (5, MODIFIER_RELEASE_MS),
            (6, MODIFIER_RELEASE_MS),
            (7, MODIFIER_RELEASE_MS),
        ] {
            assert!(
                (lo..=hi).contains(&gap(i)),
                "seed {seed}: gap {i} is {}",
                gap(i)
            );
        }
        assert_eq!(ks[0].t, 0.0);
    }
}

// --- tempo (humanized.test.ts:445-478) --------------------------------------------------

#[test]
fn scales_durations_and_intervals_never_keys() {
    let slow = plan_at(typing("scaled but shaped"), 2.0, 5);
    let nominal = plan_at(typing("scaled but shaped"), 1.0, 5);
    let keys = |e: &[KeyEvent]| e.iter().map(|e| (e.keysym, e.down)).collect::<Vec<_>>();
    assert_eq!(keys(&slow), keys(&nominal));
    for (s, n) in slow.iter().zip(&nominal) {
        assert!((s.t - 2.0 * n.t).abs() < 0.5, "{} vs 2×{}", s.t, n.t);
    }
}

#[test]
fn clamps_a_tempo_outside_a_quarter_to_four_rather_than_trusting_it() {
    assert_eq!(
        plan_at(typing("abc"), 100.0, 1),
        plan_at(typing("abc"), 4.0, 1)
    );
}

// --- bolide's own -----------------------------------------------------------------------

/// Shifted characters, repeated letters, punctuation, a newline and a tab: the key-downs
/// are exactly direct mode's, in order, under every seed, and so are the key-ups.
#[test]
fn the_text_typed_is_exact_shifted_characters_and_repeated_letters_included() {
    let text = "AAA aaa Bookkeeper! ~!@#$%^&*()_+{}|:\"<>? 1122 Mississippi.\n\tZz";
    for seed in SEEDS {
        let ops = plan_typing(&typing(text), &mut SeededRandom::new(seed))
            .expect("planned")
            .expect("a typed act");
        let keys: Vec<(u32, bool)> = ops
            .iter()
            .filter_map(|op| match *op {
                RfbOp::Key { keysym, down } => Some((keysym, down)),
                _ => None,
            })
            .collect();
        let downs: Vec<u32> = keys.iter().filter(|k| k.1).map(|k| k.0).collect();
        assert_eq!(downs, text_keysyms(text), "seed {seed}");
        // The same multiset of keys as direct's ops, only some of them reordered.
        let mut direct: Vec<(u32, bool)> = crate::actions::plan(&typing(text), CTX)
            .expect("planned")
            .into_iter()
            .filter_map(|op| match op {
                RfbOp::Key { keysym, down } => Some((keysym, down)),
                _ => None,
            })
            .collect();
        let mut ours = keys.clone();
        direct.sort_unstable();
        ours.sort_unstable();
        assert_eq!(ours, direct, "seed {seed}");
    }
}

/// Rollover never holds an implied Shift and never repeats a key, across every seed and
/// a text built to tempt it: doubled letters, capitals beside lowercase, shifted symbols.
#[test]
fn rollover_never_holds_shift_implicitly_and_never_repeats_a_key() {
    let text = "aAbBaa the THE tthhee !1!1 ??// bookkeeper Mississippi";
    for seed in SEEDS {
        let events = plan_at(typing(text), 1.0, seed);
        let mut held: Vec<u32> = Vec::new();
        for e in &events {
            if e.down {
                assert!(
                    !held.contains(&e.keysym),
                    "seed {seed}: {:#x} twice",
                    e.keysym
                );
                if !held.is_empty() {
                    assert!(plain(e.keysym), "seed {seed}: {:#x} over a key", e.keysym);
                    assert!(held.iter().all(|&h| plain(h)), "seed {seed}: {held:x?}");
                }
                held.push(e.keysym);
            } else {
                held.retain(|&h| h != e.keysym);
            }
        }
        assert!(held.is_empty(), "seed {seed}: left held {held:x?}");
    }
}

/// Every hold is meteor's 70–130 ms, unless a release had to come early to let the next
/// key go down first; never shorter than the 20 ms a key can be released in.
#[test]
fn holds_are_70_to_130_ms_unless_cut_short_for_the_next_key() {
    for seed in SEEDS {
        let events = plan_at(typing("Hello, World! 42"), 1.0, seed);
        for (i, down) in events.iter().enumerate().filter(|(_, e)| e.down) {
            let up = events[i + 1..]
                .iter()
                .find(|e| !e.down && e.keysym == down.keysym)
                .expect("released");
            let hold = up.t - down.t;
            assert!(
                (MIN_HOLD_MS..=KEY_HOLD_MS.1).contains(&hold),
                "seed {seed}: held {:#x} {hold} ms",
                down.keysym
            );
        }
    }
}

/// The ops wait out the plan's timing to the millisecond and end on the last key: no
/// sleep after it, so the act is answered when its last key has gone out.
#[test]
fn the_ops_wait_out_the_timing_and_end_on_the_last_key() {
    for act in [typing("Hi, Bob!\n"), key("ctrl+shift+t")] {
        let events = plan_at(act.clone(), 1.0, 9);
        let ops = plan_typing(&act, &mut SeededRandom::new(9))
            .expect("planned")
            .expect("a typed act");
        assert!(matches!(ops.last(), Some(RfbOp::Key { .. })), "{ops:?}");
        let mut now = 0u64;
        let mut at = Vec::new();
        for op in &ops {
            match *op {
                RfbOp::Sleep { ms } => now += ms,
                RfbOp::Key { .. } => at.push(now),
                RfbOp::Pointer { .. } => panic!("typing moved the pointer"),
            }
        }
        let want: Vec<u64> = events.iter().map(|e| js_round(e.t) as u64).collect();
        assert_eq!(at, want, "{act:?}");
    }
}

/// The same seed and the same acts give the same ops; another seed does not.
#[test]
fn the_same_seed_types_the_same_events_and_another_does_not() {
    let session = |seed| {
        let mut random = SeededRandom::new(seed);
        [typing("Hello, World!"), key("ctrl+a"), typing("again")]
            .iter()
            .map(|act| plan_typing(act, &mut random).expect("planned"))
            .collect::<Vec<_>>()
    };
    assert_eq!(session(1), session(1));
    assert_ne!(session(1), session(2));
}

/// The cost of humanized typing, as the README states it: about 5 characters a second,
/// so a 100-character line of prose takes about 19.5 s (19.50 s, the mean over these 200
/// seeds).
#[test]
fn a_hundred_character_line_takes_about_nineteen_and_a_half_seconds() {
    let line = "The quick brown fox jumps over the lazy dog, then naps; at 4 PM it wakes, \
                eats, and runs home again.";
    assert_eq!(line.chars().count(), 100);
    let mut total = 0.0;
    for seed in 1..=200 {
        let events = plan_at(typing(line), 1.0, seed);
        total += events.last().expect("typed").t;
    }
    let mean_s = total / 200.0 / 1000.0;
    assert!(
        (19.0..=20.0).contains(&mean_s),
        "a 100-character line took {mean_s} s"
    );
}

// --- the modes --------------------------------------------------------------------------

#[test]
fn the_typing_modes_are_spelled_once() {
    for mode in TypingMode::ALL {
        assert_eq!(TypingMode::from_name(mode.as_str()), Some(mode));
    }
    assert_eq!(TypingMode::from_name("human"), None);
    assert_eq!(TypingMode::default(), TypingMode::Direct);
}

/// Direct typing plans exactly what direct mode plans, whatever the pointer, and draws
/// nothing from the stream.
#[test]
fn direct_typing_types_as_direct_mode_does_and_draws_nothing() {
    for pointer in PointerMode::ALL {
        for act in [typing("Hi, Bob!\n"), key("ctrl+shift+t")] {
            let mut random = SeededRandom::new(1);
            assert_eq!(
                plan_input(
                    &act,
                    CTX,
                    pointer,
                    TypingMode::Direct,
                    Some((5, 5)),
                    &mut random
                ),
                plan(&act, CTX)
            );
            assert_eq!(random, SeededRandom::new(1), "{pointer:?} {act:?} drew");
        }
    }
}

/// Humanized typing covers `type` and `key` only: pointer acts follow the pointer mode,
/// exactly as they would with direct typing.
#[test]
fn humanized_typing_leaves_the_pointer_acts_to_the_pointer_mode() {
    let acts = [
        ComputerAction::LeftClick {
            coordinate: [300, 200],
        },
        ComputerAction::Scroll {
            coordinate: [800, 500],
            scroll_direction: ScrollDirection::Down,
            scroll_amount: 3,
        },
        ComputerAction::Wait { duration: 0.5 },
    ];
    for act in acts {
        assert_eq!(
            plan_input(
                &act,
                CTX,
                PointerMode::Direct,
                TypingMode::Humanized,
                None,
                &mut SeededRandom::new(3)
            ),
            plan(&act, CTX)
        );
        assert_eq!(
            plan_input(
                &act,
                CTX,
                PointerMode::Humanized,
                TypingMode::Humanized,
                None,
                &mut SeededRandom::new(3)
            ),
            plan_humanized(&act, CTX, None, &mut SeededRandom::new(3))
        );
    }
}

/// Humanized typing is humanized whatever the pointer mode: the same timed keys.
#[test]
fn humanized_typing_types_humanly_under_either_pointer() {
    for pointer in PointerMode::ALL {
        let ops = plan_input(
            &typing("hello"),
            CTX,
            pointer,
            TypingMode::Humanized,
            None,
            &mut SeededRandom::new(4),
        )
        .expect("planned");
        assert_eq!(
            Some(ops.clone()),
            plan_typing(&typing("hello"), &mut SeededRandom::new(4)).expect("planned")
        );
        assert!(
            ops.iter().any(|op| matches!(op, RfbOp::Sleep { .. })),
            "{pointer:?}"
        );
    }
}
