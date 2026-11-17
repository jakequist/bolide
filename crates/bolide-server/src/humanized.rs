//! Pointer acts performed the way a person's hand performs them. **Pure**, like
//! [`crate::plan`], and beside it: the same actions, other events.
//!
//! A port of meteor's `HumanizedInteractionProvider` (`desktops/interaction/src/humanized.ts`
//! at meteor commit `bba97bcb10c6`), its pointer half only:
//!
//! - **move** — a cubic Bézier bowed off the straight line, travelled on a minimum-jerk
//!   speed profile (fastest near the middle), for a duration from Fitts's law, with a small
//!   tremor, sampled at display rate; a long move overshoots and corrects. It starts from
//!   where bolide last left the pointer — never a teleport.
//! - **clicks** — a hover before the press and a press→release dwell; it lands within ±1 px
//!   of the point, and a pointer already resting that close does not move again. A double
//!   or triple click has human gaps between its clicks, all at one place.
//! - **drag** — a reach to the start, a press, a slower carried path to the end, a release
//!   there. No overshoot on either leg: a grab that overshoots grabs the wrong thing.
//! - **scroll** — the pointer goes to the point first, then the notches come in runs that
//!   speed up and slow down, with a pause between long runs.
//!
//! `type`, `key`, `wait`, `screenshot` and `cursor_position` are planned by [`crate::plan`]
//! exactly as in direct mode.
//!
//! All randomness is one [`SeededRandom`] stream, so the same seed and the same acts from
//! the same place give the same events. Within one plan the scalar draws (durations, bows,
//! landing) come before the per-sample draws (tremor, sample jitter), in meteor's order.
//!
//! **What differs from meteor, on purpose.** A scroll is not capped at meteor's 10 notches:
//! bolide honours the amount asked for. bolide has a `triple_click`, which meteor does not;
//! it is a double click with one more gap and press. Every pointer position is kept on the
//! screen when it becomes a PointerEvent (a landing, an overshoot or a bow near an edge
//! would otherwise leave it), and times are whole milliseconds on the wire, as
//! [`RfbOp::Sleep`]s between events. Meteor's tempo is kept as an internal factor, fixed
//! at 1; nothing sets it yet.

use bolide_rfb::proto::{
    BUTTON_LEFT, BUTTON_MIDDLE, BUTTON_RIGHT, BUTTON_WHEEL_DOWN, BUTTON_WHEEL_UP,
};

use crate::actions::{plan, ActionError, PlanContext, RfbOp};
use crate::wire::{ComputerAction, ScrollDirection};

/// How bolide moves the pointer: `--pointer direct|humanized`.
///
/// The single statement of the vocabulary: the CLI parses through [`PointerMode::from_name`]
/// and offers [`PointerMode::NAMES`], so a value that parses is a mode that exists.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PointerMode {
    /// Today's machine input: the pointer jumps to the point and presses at once.
    #[default]
    Direct,
    /// A person's hand: see the [module docs](self).
    Humanized,
}

impl PointerMode {
    /// Every mode, in the order `--help` lists them.
    pub const ALL: [PointerMode; 2] = [PointerMode::Direct, PointerMode::Humanized];
    /// Their spellings, in the same order.
    pub const NAMES: [&'static str; 2] = ["direct", "humanized"];

    /// The spelling the flag accepts.
    pub fn as_str(self) -> &'static str {
        match self {
            PointerMode::Direct => Self::NAMES[0],
            PointerMode::Humanized => Self::NAMES[1],
        }
    }

    /// The mode of that name, if there is one.
    pub fn from_name(name: &str) -> Option<PointerMode> {
        Self::ALL.into_iter().find(|mode| mode.as_str() == name)
    }
}

// --- the random stream ------------------------------------------------------------------

/// A seeded uniform `[0, 1)` stream: mulberry32 over a 32-bit seed, ported from meteor's
/// `desktops/interaction/src/random.ts:11-20`. Small, fast and well enough distributed for
/// jitter; nothing here is cryptographic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeededRandom {
    state: u32,
}

impl SeededRandom {
    /// The stream for `seed`.
    pub fn new(seed: u32) -> SeededRandom {
        SeededRandom { state: seed }
    }

    /// The next draw, in `[0, 1)`.
    pub fn next_f64(&mut self) -> f64 {
        self.state = self.state.wrapping_add(0x6d2b_79f5);
        let mut t = self.state;
        t = (t ^ (t >> 15)).wrapping_mul(t | 1);
        t ^= t.wrapping_add((t ^ (t >> 7)).wrapping_mul(t | 61));
        f64::from(t ^ (t >> 14)) / 4_294_967_296.0
    }
}

// --- parameters -------------------------------------------------------------------------

type Range = (f64, f64);

/// Every number the plans use, at tempo 1, in ms and px — meteor's `HUMANIZED_DEFAULTS`
/// (`humanized.ts:255-329`), pointer half. Round figures from the motor-control
/// literature, set so that meteor's bot-score fixture rates the result as a person's;
/// `humanized/score.rs` holds them to that here.
mod params {
    use super::Range;

    // Fitts's law `MT = a + b·log2(d/w + 1)`, and the bounds on a move (humanized.ts:256-263).
    pub const FITTS_A_MS: f64 = 110.0;
    pub const FITTS_B_MS: f64 = 120.0;
    pub const FITTS_WIDTH_PX: f64 = 24.0;
    pub const MOVE_MS: Range = (100.0, 1200.0);
    pub const MOVE_JITTER: Range = (0.85, 1.15);

    // The path's shape: bow, speed-profile skew, tremor, sampling (humanized.ts:264-275).
    pub const BOW: Range = (0.1, 0.25);
    pub const BOW_MAX_PX: f64 = 200.0;
    pub const PEAK_SKEW: Range = (0.85, 1.15);
    pub const TREMOR_PX: Range = (0.2, 0.6);
    pub const SAMPLE_HZ: Range = (60.0, 120.0);
    pub const MAX_STEP_PX: f64 = 25.0;

    // Overshoot and correction on a long move (humanized.ts:276-286).
    pub const OVERSHOOT_MIN_PX: f64 = 400.0;
    pub const OVERSHOOT_CHANCE: f64 = 0.6;
    pub const OVERSHOOT: Range = (0.03, 0.08);
    pub const OVERSHOOT_MAX_PX: f64 = 40.0;
    pub const OVERSHOOT_PAUSE_MS: Range = (20.0, 80.0);
    pub const CORRECTION_MS: Range = (120.0, 220.0);

    // Clicks: landing, hover, dwell, the double click's gap (humanized.ts:287-293).
    pub const LANDING_PX: f64 = 1.0;
    pub const HOVER_MS: Range = (40.0, 150.0);
    pub const DWELL_MS: Range = (60.0, 120.0);
    pub const DOUBLE_DWELL_MS: Range = (50.0, 100.0);
    pub const DOUBLE_GAP_MS: Range = (70.0, 160.0);

    // Drags (humanized.ts:294-298).
    pub const DRAG_HOVER_MS: Range = (60.0, 150.0);
    pub const DRAG_GRAB_MS: Range = (80.0, 180.0);
    pub const DRAG_HOLD_MS: Range = (60.0, 140.0);
    pub const DRAG_SLOWDOWN: f64 = 1.3;

    // Scrolls (humanized.ts:317-328).
    pub const SCROLL_HOVER_MS: Range = (50.0, 150.0);
    pub const NOTCH_MS: Range = (35.0, 70.0);
    pub const NOTCH_EASE: f64 = 1.5;
    pub const NOTCH_SIGMA: f64 = 0.25;
    pub const SCROLL_RUN: Range = (3.0, 6.0);
    pub const SCROLL_RUN_MAX: u32 = 6;
    pub const SCROLL_RUN_PAUSE_MS: Range = (250.0, 550.0);
    pub const NOTCH_RELEASE_MS: Range = (2.0, 8.0);

    // Tempo's bounds and nominal value: meteor's `shared/types/src/index.ts:113-115`.
    pub const MIN_TEMPO: f64 = 0.25;
    pub const MAX_TEMPO: f64 = 4.0;
    pub const DEFAULT_TEMPO: f64 = 1.0;
}

use params::*;

// --- the plan ---------------------------------------------------------------------------

/// Turn one action into the RFB operations that perform it, humanly.
///
/// `from` is where bolide last left the pointer, or `None` when it has sent no
/// PointerEvent yet, when the pointer is taken to be at the centre of the screen (meteor's
/// `CursorTracker.seedFromFrame`, `cursor.ts:26-30`). Every draw comes from `random`.
pub fn plan_humanized(
    action: &ComputerAction,
    ctx: PlanContext,
    from: Option<(u16, u16)>,
    random: &mut SeededRandom,
) -> std::result::Result<Vec<RfbOp>, ActionError> {
    let from = match from {
        Some((x, y)) => (i32::from(x), i32::from(y)),
        None => (i32::from(ctx.width / 2), i32::from(ctx.height / 2)),
    };
    match gesture(action, &ctx, from, DEFAULT_TEMPO, random)? {
        Some(gesture) => Ok(to_ops(&gesture, &ctx)),
        None => plan(action, ctx),
    }
}

/// One PointerEvent of a gesture, `t` ms after it begins.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Event {
    pub t: f64,
    pub x: i32,
    pub y: i32,
    pub buttons: u8,
}

/// A pointer act as timed events, before it becomes ops.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Gesture {
    pub events: Vec<Event>,
    pub ends_at: (i32, i32),
    pub duration_ms: f64,
}

/// The gesture of a pointer act from `from`, or `None` for an act that is not one
/// (humanized.ts:59-139, `plan`). Coordinates are checked before anything is drawn, so a
/// refused act leaves the stream where it was.
fn gesture(
    action: &ComputerAction,
    ctx: &PlanContext,
    from: (i32, i32),
    tempo: f64,
    random: &mut SeededRandom,
) -> std::result::Result<Option<Gesture>, ActionError> {
    let k = clamp_tempo(tempo);
    let d = &mut Draw(random);
    let here = Pt::from(from);
    let at = |coordinate: &[i32; 2]| ctx.check(*coordinate).map(Pt::from);
    Ok(Some(match action {
        ComputerAction::Screenshot
        | ComputerAction::CursorPosition
        | ComputerAction::Type { .. }
        | ComputerAction::Key { .. }
        | ComputerAction::Wait { .. } => return Ok(None),

        ComputerAction::MouseMove { coordinate } => {
            let target = at(coordinate)?;
            let path = plan_path(d, here, target, k, true, 0);
            let events = render_path(d, &path, 0.0);
            let duration_ms = last_t(&events);
            Gesture {
                events,
                ends_at: path.end.whole(),
                duration_ms,
            }
        }
        ComputerAction::LeftClick { coordinate } => click(d, here, at(coordinate)?, k, BUTTON_LEFT),
        ComputerAction::RightClick { coordinate } => {
            click(d, here, at(coordinate)?, k, BUTTON_RIGHT)
        }
        ComputerAction::MiddleClick { coordinate } => {
            click(d, here, at(coordinate)?, k, BUTTON_MIDDLE)
        }
        ComputerAction::DoubleClick { coordinate } => clicks(d, here, at(coordinate)?, k, 2),
        ComputerAction::TripleClick { coordinate } => clicks(d, here, at(coordinate)?, k, 3),

        ComputerAction::LeftClickDrag {
            start_coordinate,
            coordinate,
        } => {
            let (start, end) = (at(start_coordinate)?, at(coordinate)?);
            drag(d, here, start, end, k)
        }

        ComputerAction::Scroll {
            coordinate,
            scroll_direction,
            scroll_amount,
        } => {
            let target = at(coordinate)?;
            let wheel = match scroll_direction {
                ScrollDirection::Up => BUTTON_WHEEL_UP,
                ScrollDirection::Down => BUTTON_WHEEL_DOWN,
            };
            scroll(d, here, target, k, wheel, *scroll_amount)
        }
    }))
}

/// A single click (humanized.ts:71-80): hover, press, dwell, release.
fn click(d: &mut Draw, here: Pt, target: Pt, k: f64, mask: u8) -> Gesture {
    let hover = d.tri(HOVER_MS) * k;
    let dwell = d.tri(DWELL_MS) * k;
    let path = plan_path(d, here, target, k, true, 0);
    let mut events = render_path(d, &path, 0.0);
    let press = last_t(&events) + hover;
    events.push(ptr(press, path.end, mask));
    events.push(ptr(press + dwell, path.end, 0));
    Gesture {
        events,
        ends_at: path.end.whole(),
        duration_ms: press + dwell,
    }
}

/// `count` left clicks at one place (humanized.ts:81-92, the double click; a triple click
/// is one more gap and press, drawn in the same pattern).
fn clicks(d: &mut Draw, here: Pt, target: Pt, k: f64, count: usize) -> Gesture {
    let hover = d.tri(HOVER_MS) * k;
    let mut dwells = vec![d.tri(DOUBLE_DWELL_MS) * k];
    let mut gaps = Vec::with_capacity(count);
    for _ in 1..count {
        gaps.push(d.tri(DOUBLE_GAP_MS) * k);
        dwells.push(d.tri(DOUBLE_DWELL_MS) * k);
    }
    let path = plan_path(d, here, target, k, true, 0);
    let mut events = render_path(d, &path, 0.0);
    let mut t = last_t(&events) + hover;
    for (i, dwell) in dwells.into_iter().enumerate() {
        if i > 0 {
            t += gaps[i - 1];
        }
        events.push(ptr(t, path.end, BUTTON_LEFT));
        t += dwell;
        events.push(ptr(t, path.end, 0));
    }
    Gesture {
        events,
        ends_at: path.end.whole(),
        duration_ms: t,
    }
}

/// A drag (humanized.ts:93-115): reach, press, a slower carried path, hold, release. No
/// overshoot on either leg: a grab that overshoots grabs the wrong thing, and a drop that
/// overshoots drops there.
fn drag(d: &mut Draw, here: Pt, start: Pt, end: Pt, k: f64) -> Gesture {
    let hover = d.tri(DRAG_HOVER_MS) * k;
    let grab = d.tri(DRAG_GRAB_MS) * k;
    let hold = d.tri(DRAG_HOLD_MS) * k;
    let reach = plan_path(d, here, start, k, false, 0);
    let carry = plan_path(d, reach.end, end, k * DRAG_SLOWDOWN, false, BUTTON_LEFT);
    let mut events = render_path(d, &reach, 0.0);
    let press = last_t(&events) + hover;
    events.push(ptr(press, reach.end, BUTTON_LEFT));
    let carried = render_path(d, &carry, press + grab);
    events.extend(carried);
    let release = last_t(&events) + hold;
    events.push(ptr(release, carry.end, 0));
    Gesture {
        events,
        ends_at: carry.end.whole(),
        duration_ms: release,
    }
}

/// A scroll (humanized.ts:120-137): to the point, then the notches in runs. A scroll of
/// nothing does not even move, as in direct mode. Not capped: bolide sends every notch.
fn scroll(d: &mut Draw, here: Pt, target: Pt, k: f64, wheel: u8, ticks: u32) -> Gesture {
    if ticks < 1 {
        return Gesture {
            events: Vec::new(),
            ends_at: here.whole(),
            duration_ms: 0.0,
        };
    }
    let hover = d.tri(SCROLL_HOVER_MS) * k;
    let intervals: Vec<f64> = notch_intervals(d, ticks)
        .into_iter()
        .map(|ms| ms * k)
        .collect();
    let mut release_after: Vec<f64> = intervals
        .iter()
        .map(|_| d.tri(NOTCH_RELEASE_MS) * k)
        .collect();
    release_after.push(d.tri(NOTCH_RELEASE_MS) * k);
    let path = plan_path(d, here, target, k, false, 0);
    let mut events = render_path(d, &path, 0.0);
    let mut t = last_t(&events) + hover;
    for (i, release) in release_after.iter().enumerate() {
        if i > 0 {
            t += intervals[i - 1];
        }
        events.push(ptr(t, path.end, wheel));
        events.push(ptr(t + release, path.end, 0));
    }
    let duration_ms = last_t(&events);
    Gesture {
        events,
        ends_at: path.end.whole(),
        duration_ms,
    }
}

/// The ops that play a gesture: each PointerEvent after a sleep to its time, rounded to the
/// millisecond, then a sleep to the gesture's end. A position off the screen (a landing,
/// an overshoot or a bow near an edge) is kept on its edge: a desktop has no pixel there.
fn to_ops(gesture: &Gesture, ctx: &PlanContext) -> Vec<RfbOp> {
    let on_screen = |v: i32, side: u16| v.clamp(0, i32::from(side.max(1)) - 1) as u16;
    let mut ops = Vec::with_capacity(2 * gesture.events.len() + 1);
    let mut now = 0u64;
    let mut wait_until = |ops: &mut Vec<RfbOp>, t: f64| {
        let t = js_round(t).max(0.0) as u64;
        if t > now {
            ops.push(RfbOp::Sleep { ms: t - now });
            now = t;
        }
    };
    for e in &gesture.events {
        wait_until(&mut ops, e.t);
        ops.push(RfbOp::Pointer {
            x: on_screen(e.x, ctx.width),
            y: on_screen(e.y, ctx.height),
            buttons: e.buttons,
        });
    }
    wait_until(&mut ops, gesture.duration_ms);
    ops
}

/// meteor's `clampTempo` (`shared/types/src/index.ts:130-133`).
fn clamp_tempo(tempo: f64) -> f64 {
    if tempo.is_finite() {
        tempo.clamp(MIN_TEMPO, MAX_TEMPO)
    } else {
        DEFAULT_TEMPO
    }
}

// --- drawing ----------------------------------------------------------------------------

/// Every draw a plan makes, from the one stream (humanized.ts:347-367).
struct Draw<'a>(&'a mut SeededRandom);

impl Draw<'_> {
    fn next(&mut self) -> f64 {
        self.0.next_f64()
    }

    fn uniform(&mut self, (lo, hi): Range) -> f64 {
        lo + (hi - lo) * self.next()
    }

    /// Triangular on [lo, hi], peaked in the middle: people cluster, they are not uniform.
    fn tri(&mut self, (lo, hi): Range) -> f64 {
        let (a, b) = (self.next(), self.next());
        lo + (hi - lo) * (a + b) / 2.0
    }

    fn gauss(&mut self) -> f64 {
        let u = 1.0 - self.next(); // (0, 1]: never ln(0)
        let v = self.next();
        (-2.0 * u.ln()).sqrt() * (2.0 * std::f64::consts::PI * v).cos()
    }

    fn log_normal(&mut self, median: f64, sigma: f64) -> f64 {
        median * (sigma * self.gauss()).exp()
    }
}

// --- the pointer ------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
struct Pt {
    x: f64,
    y: f64,
}

impl From<(i32, i32)> for Pt {
    fn from((x, y): (i32, i32)) -> Pt {
        Pt {
            x: f64::from(x),
            y: f64::from(y),
        }
    }
}

impl From<(u16, u16)> for Pt {
    fn from((x, y): (u16, u16)) -> Pt {
        Pt {
            x: f64::from(x),
            y: f64::from(y),
        }
    }
}

impl Pt {
    /// A point that is a whole pixel already (a landing, a cursor, an aim).
    fn whole(self) -> (i32, i32) {
        (self.x as i32, self.y as i32)
    }
}

#[derive(Clone, Copy, Debug)]
struct Stroke {
    from: Pt,
    to: Pt,
    c1: Pt,
    c2: Pt,
    duration_ms: f64,
    skew: f64,
    tremor: f64,
}

struct PathPlan {
    /// Each stroke, and how long after the previous one it starts.
    strokes: Vec<(Stroke, f64)>,
    /// Where the pointer rests at the end: the landing point.
    end: Pt,
    buttons: u8,
    sample_ms: f64,
}

/// Every scalar of a movement from `from` to `target`, drawn before any sample is
/// (humanized.ts:400-450): the landing, the bow, the duration, the skew, the tremor, the
/// overshoot and its correction. A movement shorter than a few px is at most one event.
fn plan_path(
    d: &mut Draw,
    from: Pt,
    target: Pt,
    tempo: f64,
    overshoot: bool,
    buttons: u8,
) -> PathPlan {
    let landed = Pt {
        x: js_round(target.x + d.uniform((-LANDING_PX, LANDING_PX))),
        y: js_round(target.y + d.uniform((-LANDING_PX, LANDING_PX))),
    };
    // A hand already resting on the point (within the landing radius) stays put. The
    // landing is still drawn, so a seed's later draws keep their shape.
    let resting =
        (from.x - target.x).abs() <= LANDING_PX && (from.y - target.y).abs() <= LANDING_PX;
    let end = if resting { from } else { landed };
    let sample_ms = 1000.0 / d.uniform(SAMPLE_HZ);
    let dist = (end.x - from.x).hypot(end.y - from.y);
    if dist < 4.0 {
        let strokes = if end == from {
            Vec::new()
        } else {
            vec![(line(from, end, 0.0), 0.0)]
        };
        return PathPlan {
            strokes,
            end,
            buttons,
            sample_ms,
        };
    }

    let overshoots = overshoot && dist >= OVERSHOOT_MIN_PX && d.next() < OVERSHOOT_CHANCE;
    let mut aim = end;
    if overshoots {
        let by = (d.uniform(OVERSHOOT) * dist).min(OVERSHOOT_MAX_PX);
        let side = d.uniform((-0.3, 0.3)) * by;
        let ux = (end.x - from.x) / dist;
        let uy = (end.y - from.y) / dist;
        aim = Pt {
            x: js_round(end.x + ux * by - uy * side),
            y: js_round(end.y + uy * by + ux * side),
        };
    }
    let main = stroke(d, from, aim, tempo);
    let mut strokes = vec![(main, 0.0)];
    if overshoots {
        let pause = d.uniform(OVERSHOOT_PAUSE_MS) * tempo;
        let duration = d.uniform(CORRECTION_MS) * tempo;
        let correction = Stroke {
            skew: d.uniform(PEAK_SKEW),
            tremor: main.tremor / 2.0,
            ..line(aim, end, duration)
        };
        strokes.push((correction, pause));
    }
    PathPlan {
        strokes,
        end,
        buttons,
        sample_ms,
    }
}

/// One bowed, timed stroke (humanized.ts:453-478).
fn stroke(d: &mut Draw, from: Pt, to: Pt, tempo: f64) -> Stroke {
    let dx = to.x - from.x;
    let dy = to.y - from.y;
    let dist = dx.hypot(dy);
    let fitts = FITTS_A_MS + FITTS_B_MS * (dist / FITTS_WIDTH_PX + 1.0).log2();
    let duration_ms = (fitts * d.uniform(MOVE_JITTER)).clamp(MOVE_MS.0, MOVE_MS.1) * tempo;
    // Both control points bow to the same side (an arc, as a wrist pivots), by different
    // amounts and at different places along the line, so no two strokes share a shape.
    let side = if d.next() < 0.5 { -1.0 } else { 1.0 };
    let bow1 = side * (d.uniform(BOW) * dist).min(BOW_MAX_PX);
    let bow2 = bow1 * d.uniform((0.5, 1.2));
    let a1 = d.uniform((0.2, 0.4));
    let a2 = d.uniform((0.6, 0.8));
    let nx = -dy / dist;
    let ny = dx / dist;
    Stroke {
        from,
        to,
        c1: Pt {
            x: from.x + dx * a1 + nx * bow1,
            y: from.y + dy * a1 + ny * bow1,
        },
        c2: Pt {
            x: from.x + dx * a2 + nx * bow2,
            y: from.y + dy * a2 + ny * bow2,
        },
        duration_ms,
        skew: d.uniform(PEAK_SKEW),
        tremor: d.uniform(TREMOR_PX),
    }
}

/// A straight stroke, controls on the line: the tiny nudge, and the correction.
fn line(from: Pt, to: Pt, duration_ms: f64) -> Stroke {
    let at = |f: f64| Pt {
        x: from.x + (to.x - from.x) * f,
        y: from.y + (to.y - from.y) * f,
    };
    Stroke {
        from,
        to,
        c1: at(1.0 / 3.0),
        c2: at(2.0 / 3.0),
        duration_ms,
        skew: 1.0,
        tremor: 0.0,
    }
}

/// The minimum-jerk position profile: 0 → 1 with zero speed and acceleration at both ends.
fn min_jerk(tau: f64) -> f64 {
    tau * tau * tau * (10.0 - 15.0 * tau + 6.0 * tau * tau)
}

fn bezier(s: &Stroke, u: f64) -> Pt {
    let v = 1.0 - u;
    let (a, b, c, e) = (v * v * v, 3.0 * v * v * u, 3.0 * v * u * u, u * u * u);
    Pt {
        x: a * s.from.x + b * s.c1.x + c * s.c2.x + e * s.to.x,
        y: a * s.from.y + b * s.c1.y + c * s.c2.y + e * s.to.y,
    }
}

/// The pointer events of a planned path, the first stroke starting at `t0`
/// (humanized.ts:502-527).
fn render_path(d: &mut Draw, path: &PathPlan, t0: f64) -> Vec<Event> {
    let mut events = Vec::new();
    let mut t = t0;
    for (s, starts_after_ms) in &path.strokes {
        t += starts_after_ms;
        if s.duration_ms <= 0.0 {
            events.push(ptr(t, s.to, path.buttons));
            continue;
        }
        let dist = (s.to.x - s.from.x).hypot(s.to.y - s.from.y);
        let n = (s.duration_ms / path.sample_ms)
            .ceil()
            .max((dist / MAX_STEP_PX).ceil())
            .max(1.0) as u32;
        let dt = s.duration_ms / f64::from(n);
        for i in 1..=n {
            // Sample times wobble a little around the display's beat; the last is exact.
            let jitter = if i < n {
                d.uniform((-0.15, 0.15)) * dt
            } else {
                0.0
            };
            let tau = (f64::from(i) * dt + jitter) / s.duration_ms;
            let p = bezier(s, min_jerk(tau.powf(s.skew)));
            let fade = (std::f64::consts::PI * tau).sin();
            let x = if i == n {
                s.to.x
            } else {
                js_round(p.x + s.tremor * fade * d.gauss())
            };
            let y = if i == n {
                s.to.y
            } else {
                js_round(p.y + s.tremor * fade * d.gauss())
            };
            events.push(ptr(
                t + f64::from(i) * dt + jitter,
                Pt { x, y },
                path.buttons,
            ));
        }
        t += s.duration_ms;
    }
    events
}

// --- scrolling --------------------------------------------------------------------------

/// The gaps between `ticks` notches: runs that start slow, speed up and slow down again,
/// with the finger lifted between runs of a long scroll (humanized.ts:535-556).
fn notch_intervals(d: &mut Draw, ticks: u32) -> Vec<f64> {
    let mut runs = Vec::new();
    if ticks <= SCROLL_RUN_MAX {
        runs.push(ticks);
    } else {
        let mut left = ticks;
        while left > 0 {
            let n = left.min(js_round(d.uniform(SCROLL_RUN)) as u32);
            runs.push(n);
            left -= n;
        }
    }
    let mut out = Vec::with_capacity(ticks as usize);
    for (r, &n) in runs.iter().enumerate() {
        if r > 0 {
            out.push(d.tri(SCROLL_RUN_PAUSE_MS));
        }
        let beat = d.uniform(NOTCH_MS);
        for i in 1..n {
            let ease = 1.0
                + NOTCH_EASE * (1.0 - (std::f64::consts::PI * f64::from(i) / f64::from(n)).sin());
            out.push(beat * ease * d.log_normal(1.0, NOTCH_SIGMA));
        }
    }
    out
}

// --- small ------------------------------------------------------------------------------

fn ptr(t: f64, p: Pt, buttons: u8) -> Event {
    Event {
        t,
        x: p.x as i32,
        y: p.y as i32,
        buttons,
    }
}

fn last_t(events: &[Event]) -> f64 {
    events.last().map_or(0.0, |e| e.t)
}

/// JavaScript's `Math.round`: halves go up, toward +∞.
fn js_round(v: f64) -> f64 {
    (v + 0.5).floor()
}

#[cfg(test)]
mod score;
#[cfg(test)]
mod tests;
