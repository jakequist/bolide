//! `type` and `key` performed the way a person's hands perform them. **Pure**, like the
//! pointer half beside it, and drawing from the same session stream.
//!
//! A port of meteor's `HumanizedInteractionProvider.typeText` and `pressKey`
//! (`desktops/interaction/src/humanized.ts:141-243` at meteor commit `bba97bcb10c6`):
//!
//! - **type** — log-normal intervals between key-downs, at a pace that wanders per word;
//!   faster for common letter pairs, slower at word boundaries and for anything but a
//!   letter; now and then a longer pause; each key held a human while. A key may still be
//!   down when the next goes down (rollover), but only where meteor allows it: both keys
//!   type without a Shift of their own, and they differ. The keys that arrive are exactly
//!   direct mode's, one down and one up per character, so the text is exactly the text.
//! - **key** — the modifiers go down staggered, then a lead, the key is held a dwell, and
//!   the modifiers come up after it in reverse.
//!
//! **What differs from meteor, on purpose.** No typos: meteor's `typoRate` is off by
//! default there, and is left out here entirely. No settle after the last key (meteor's
//! `DEFAULT_INPUT_PACING.typeSettleMs` and `chordSettleMs`): bolide answers once the last
//! key has gone out, and a screenshot after input already lets the desktop settle. Tempo
//! is kept as an internal factor, fixed at 1, as the pointer's is.
//!
//! Keysyms are resolved before anything is drawn, so a refused act leaves the stream
//! where it was. Within one act every draw comes in meteor's order.

use super::{clamp_tempo, js_round, Draw, Range, SeededRandom, DEFAULT_TEMPO};
use crate::actions::{keysym_for_typed, ActionError, RfbOp};
use crate::wire::ComputerAction;

/// How bolide types: `--typing direct|humanized`.
///
/// The single statement of the vocabulary: the CLI parses through [`TypingMode::from_name`]
/// and offers [`TypingMode::NAMES`], so a value that parses is a mode that exists.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TypingMode {
    /// Today's machine input: every key down and up at once, with no wait between.
    #[default]
    Direct,
    /// A person's hands: see the [module docs](self).
    Humanized,
}

impl TypingMode {
    /// Every mode, in the order `--help` lists them.
    pub const ALL: [TypingMode; 2] = [TypingMode::Direct, TypingMode::Humanized];
    /// Their spellings, in the same order.
    pub const NAMES: [&'static str; 2] = ["direct", "humanized"];

    /// The spelling the flag accepts.
    pub fn as_str(self) -> &'static str {
        match self {
            TypingMode::Direct => Self::NAMES[0],
            TypingMode::Humanized => Self::NAMES[1],
        }
    }

    /// The mode of that name, if there is one.
    pub fn from_name(name: &str) -> Option<TypingMode> {
        Self::ALL.into_iter().find(|mode| mode.as_str() == name)
    }
}

// --- parameters -------------------------------------------------------------------------

/// Every number the plans use, at tempo 1, in ms — meteor's `HUMANIZED_DEFAULTS`
/// (`humanized.ts:299-316`), typing and key half, less `typoRate` and `noticeTypoMs`.
mod params {
    use super::Range;

    // Key intervals: log-normal, median and σ; the per-word pace's σ (humanized.ts:300-303).
    pub const KEY_MEDIAN_MS: f64 = 150.0;
    pub const KEY_SIGMA: f64 = 0.35;
    pub const WORD_PACE_SIGMA: f64 = 0.12;
    // What makes an interval shorter or longer (humanized.ts:304-306).
    pub const COMMON_PAIR_FACTOR: f64 = 0.75;
    pub const WORD_BOUNDARY_FACTOR: f64 = 1.25;
    pub const NON_LETTER_FACTOR: f64 = 1.2;
    // An occasional pause, and how long a key is held (humanized.ts:307-309).
    pub const PAUSE_CHANCE: f64 = 0.03;
    pub const PAUSE_MS: Range = (300.0, 900.0);
    pub const KEY_HOLD_MS: Range = (70.0, 130.0);
    // A chord (humanized.ts:313-316).
    pub const MODIFIER_STAGGER_MS: Range = (25.0, 70.0);
    pub const MODIFIER_LEAD_MS: Range = (40.0, 110.0);
    pub const CHORD_DWELL_MS: Range = (60.0, 110.0);
    pub const MODIFIER_RELEASE_MS: Range = (15.0, 60.0);
    // Below this, a key cannot be released in time for the next: the next waits
    // (humanized.ts:341-342).
    pub const MIN_HOLD_MS: f64 = 20.0;
    pub const MIN_UP_BEFORE_NEXT_MS: f64 = 8.0;
}

use params::*;

// --- the plan ---------------------------------------------------------------------------

/// The ops that type or press `action` humanly, or `None` for an act that is neither a
/// `type` nor a `key`. Every draw comes from `random`.
pub fn plan_typing(
    action: &ComputerAction,
    random: &mut SeededRandom,
) -> std::result::Result<Option<Vec<RfbOp>>, ActionError> {
    Ok(keystrokes(action, DEFAULT_TEMPO, random)?.map(|events| to_ops(&events)))
}

/// One KeyEvent of a typed act, `t` ms after it begins.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct KeyEvent {
    pub t: f64,
    pub keysym: u32,
    pub down: bool,
}

/// The timed key events of a `type` or `key`, or `None` for any other act. The keysyms
/// are resolved before anything is drawn.
pub(crate) fn keystrokes(
    action: &ComputerAction,
    tempo: f64,
    random: &mut SeededRandom,
) -> std::result::Result<Option<Vec<KeyEvent>>, ActionError> {
    let k = clamp_tempo(tempo);
    Ok(Some(match action {
        ComputerAction::Type { text } => {
            let wanted = text
                .chars()
                .map(keysym_for_typed)
                .collect::<std::result::Result<Vec<u32>, _>>()?;
            type_text(&mut Draw(random), &wanted, k)
        }
        ComputerAction::Key { text } => {
            let syms = bolide_rfb::keysym::parse_chord(text)?;
            press_key(&mut Draw(random), &syms, k)
        }
        _ => return Ok(None),
    }))
}

/// meteor's `typeText` (humanized.ts:141-218), without typos and without the settle.
fn type_text(d: &mut Draw, wanted: &[u32], k: f64) -> Vec<KeyEvent> {
    let n = wanted.len();
    if n == 0 {
        return Vec::new();
    }

    // Scalars first: every interval and hold, then place them.
    let mut word_pace = d.log_normal(1.0, WORD_PACE_SIGMA);
    let mut downs = Vec::with_capacity(n);
    let mut holds = Vec::with_capacity(n);
    let mut t = 0.0;
    for i in 0..n {
        holds.push(d.tri(KEY_HOLD_MS) * k);
        if i == 0 {
            downs.push(0.0);
            continue;
        }
        let (prev, ks) = (wanted[i - 1], wanted[i]);
        if prev == SPACE {
            word_pace = d.log_normal(1.0, WORD_PACE_SIGMA);
        }
        let mut gap = d.log_normal(KEY_MEDIAN_MS, KEY_SIGMA) * word_pace;
        if is_common_pair(prev, ks) {
            gap *= COMMON_PAIR_FACTOR;
        }
        if ks == SPACE || prev == SPACE {
            gap *= WORD_BOUNDARY_FACTOR;
        }
        if !is_letter(ks) {
            gap *= NON_LETTER_FACTOR;
        }
        if d.next() < PAUSE_CHANCE {
            gap += d.tri(PAUSE_MS);
        }
        t += gap * k;
        downs.push(t);
    }

    // Place the ups. A key may still be down when the next goes down (rollover) only when
    // both are keys the server types without a Shift of its own and they differ: a
    // neighbour pressed while an implied Shift is held would be typed shifted, and a key
    // pressed again while held is no keystroke at all.
    let mut ups: Vec<f64> = downs.iter().zip(&holds).map(|(t, h)| t + h).collect();
    let margin = MIN_UP_BEFORE_NEXT_MS * k;
    let may_overlap =
        |held: u32, next: u32| overlap_safe(held) && overlap_safe(next) && held != next;
    for next in 1..n {
        for j in 0..next {
            if ups[j] <= downs[next] - margin || may_overlap(wanted[j], wanted[next]) {
                continue;
            }
            // Still held and may not be: let it go just before, as early as a key can.
            ups[j] = (downs[j] + MIN_HOLD_MS * k).max(downs[next] - margin);
            let late = ups[j] + margin - downs[next];
            // The next key was due too soon to fit this release: push the rest along.
            if late > 0.0 {
                for m in next..n {
                    downs[m] += late;
                    ups[m] += late;
                }
            }
        }
    }
    let mut events = Vec::with_capacity(2 * n);
    for (i, &keysym) in wanted.iter().enumerate() {
        events.push(KeyEvent {
            t: downs[i],
            keysym,
            down: true,
        });
        events.push(KeyEvent {
            t: ups[i],
            keysym,
            down: false,
        });
    }
    // In time order; at one instant an up before a down, so nothing is ever held longer.
    events.sort_by(|a, b| a.t.total_cmp(&b.t).then(a.down.cmp(&b.down)));
    events
}

/// meteor's `pressKey` (humanized.ts:220-243), without the settle: the modifiers down,
/// staggered; a lead; the key held a dwell; the modifiers up in reverse.
fn press_key(d: &mut Draw, syms: &[u32], k: f64) -> Vec<KeyEvent> {
    let Some((&main, modifiers)) = syms.split_last() else {
        return Vec::new();
    };
    let mut events = Vec::with_capacity(2 * syms.len());
    let mut key = |t: f64, keysym: u32, down: bool| events.push(KeyEvent { t, keysym, down });
    let mut t = 0.0;
    for (i, &m) in modifiers.iter().enumerate() {
        if i > 0 {
            t += d.tri(MODIFIER_STAGGER_MS) * k;
        }
        key(t, m, true);
    }
    if !modifiers.is_empty() {
        t += d.tri(MODIFIER_LEAD_MS) * k;
    }
    key(t, main, true);
    t += d.tri(CHORD_DWELL_MS) * k;
    key(t, main, false);
    for &m in modifiers.iter().rev() {
        t += d.tri(MODIFIER_RELEASE_MS) * k;
        key(t, m, false);
    }
    events
}

/// The ops that play the events: each KeyEvent after a sleep to its time, rounded to the
/// millisecond. Nothing after the last: the act ends when its last key has gone out.
fn to_ops(events: &[KeyEvent]) -> Vec<RfbOp> {
    let mut ops = Vec::with_capacity(2 * events.len());
    let mut now = 0u64;
    for e in events {
        let t = js_round(e.t).max(0.0) as u64;
        if t > now {
            ops.push(RfbOp::Sleep { ms: t - now });
            now = t;
        }
        ops.push(RfbOp::Key {
            keysym: e.keysym,
            down: e.down,
        });
    }
    ops
}

// --- keys (humanized.ts:560-580) --------------------------------------------------------

const SPACE: u32 = 0x20;

fn is_letter(ks: u32) -> bool {
    (0x61..=0x7a).contains(&ks) || (0x41..=0x5a).contains(&ks)
}

/// A key the server types as itself, with no Shift of its own: lowercase letters, digits,
/// space and the unshifted punctuation of a US layout. Only these may overlap a neighbour.
const OVERLAP_SAFE: &str = "abcdefghijklmnopqrstuvwxyz0123456789 -=[]\\;',./`";

fn overlap_safe(ks: u32) -> bool {
    char::from_u32(ks).is_some_and(|c| OVERLAP_SAFE.contains(c))
}

/// The commonest English letter pairs: a typist's fingers have these by heart.
const COMMON_PAIRS: [&str; 42] = [
    "th", "he", "in", "er", "an", "re", "on", "at", "en", "nd", "ti", "es", "or", "te", "of", "ed",
    "is", "it", "al", "ar", "st", "to", "nt", "ng", "se", "ha", "as", "ou", "io", "le", "ve", "co",
    "me", "de", "hi", "ri", "ro", "ic", "ne", "ea", "ra", "ce",
];

fn is_common_pair(a: u32, b: u32) -> bool {
    if !is_letter(a) || !is_letter(b) {
        return false;
    }
    // Letters are ASCII, so `| 0x20` lowercases them, as meteor does.
    let pair = [(a | 0x20) as u8, (b | 0x20) as u8];
    COMMON_PAIRS.iter().any(|p| p.as_bytes() == pair)
}

#[cfg(test)]
mod tests;
