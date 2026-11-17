//! "Looks human" is a number. Ported from meteor's `desktops/interaction/tests/
//! humanized-score.test.ts`, with the scorer it imports (`testapps/rimmer/src/score.ts`)
//! and the bridge from a gesture to a page's samples (`tests/gesture-recording.ts`), all at
//! commit `bba97bcb10c6`.
//!
//! The same short task is planned by the direct and the humanized planner, played the way
//! bolide plays it — the ops, with their sleeps — and scored. Only the pointer and wheel
//! halves of the scorer are ported: typing is not humanized here, so the task has no typing
//! in it, and a score with no keys measured weighs only the scores it measured.

use super::*;
use crate::actions::{plan, DragPacing, PlanContext};

// --- the scorer (score.ts) --------------------------------------------------------------

/// score.ts:17-26.
const TELEPORT_PX: f64 = 80.0;
const MIN_SEGMENT_PX: f64 = 30.0;
const MOVE_GAP_MS: f64 = 250.0;
const WHEEL_BURST_GAP_MS: f64 = 300.0;
const MIN_PROFILE_SPEEDS: usize = 4;

/// score.ts:35-44, the pointer and wheel weights.
const W_STRAIGHTNESS: f64 = 1.0;
const W_TELEPORTS: f64 = 1.5;
const W_SPEED_PROFILE: f64 = 0.5;
const W_HOVER: f64 = 1.0;
const W_DWELL: f64 = 1.0;
const W_WHEEL: f64 = 1.0;

/// What a page's recorder sees (rimmer's `recording.ts`), pointer and wheel.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Sample {
    Move { t: f64, x: f64, y: f64 },
    Down { t: f64, x: f64, y: f64 },
    Up { t: f64, x: f64, y: f64 },
    Wheel { t: f64 },
}

#[derive(Clone, Copy, Debug)]
struct Pt {
    t: f64,
    x: f64,
    y: f64,
    kind: Kind,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Move,
    Down,
    Up,
}

#[derive(Debug, Default)]
struct Scores {
    straightness: Option<f64>,
    teleports: Option<f64>,
    speed_profile: Option<f64>,
    hover: Option<f64>,
    dwell: Option<f64>,
    wheel: Option<f64>,
}

#[derive(Debug)]
struct ScoreResult {
    humanness: Option<f64>,
    scores: Scores,
    teleports: usize,
}

fn clamp01(v: f64) -> f64 {
    v.clamp(0.0, 1.0)
}

fn dist(a: &Pt, b: &Pt) -> f64 {
    (a.x - b.x).hypot(a.y - b.y)
}

fn mean(xs: &[f64]) -> Option<f64> {
    (!xs.is_empty()).then(|| xs.iter().sum::<f64>() / xs.len() as f64)
}

fn median(xs: &[f64]) -> Option<f64> {
    if xs.is_empty() {
        return None;
    }
    let mut s = xs.to_vec();
    s.sort_by(f64::total_cmp);
    let m = s.len() >> 1;
    Some(if s.len() % 2 == 1 {
        s[m]
    } else {
        (s[m - 1] + s[m]) / 2.0
    })
}

fn cv(xs: &[f64]) -> Option<f64> {
    let mu = mean(xs)?;
    if xs.len() < 2 {
        return None;
    }
    if mu == 0.0 {
        return Some(0.0);
    }
    let variance = xs.iter().map(|x| (x - mu).powi(2)).sum::<f64>() / xs.len() as f64;
    Some(variance.sqrt() / mu)
}

/// score.ts:153-173: for each press, the movement that brought the pointer there.
fn approaches(pointer: &[Pt]) -> Vec<(Pt, Vec<Pt>)> {
    let mut out = Vec::new();
    let mut since = 0;
    for (i, s) in pointer.iter().enumerate() {
        if s.kind == Kind::Move {
            continue;
        }
        if s.kind == Kind::Down {
            let moves = &pointer[since..i];
            let mut first = 0;
            for j in 0..moves.len() {
                let prev_t = if j == 0 {
                    if since > 0 {
                        pointer[since - 1].t
                    } else {
                        f64::NEG_INFINITY
                    }
                } else {
                    moves[j - 1].t
                };
                if moves[j].t - prev_t > MOVE_GAP_MS {
                    first = j;
                }
            }
            let start = if since + first >= 1 {
                pointer[since + first - 1]
            } else {
                moves.get(first).copied().unwrap_or(*s)
            };
            let mut points = moves[first..].to_vec();
            points.push(*s);
            out.push((start, points));
        }
        since = i + 1;
    }
    out
}

/// score.ts:186-220: where the speed peaks, as a fraction of the movement.
fn peak(points: &[Pt]) -> Option<f64> {
    let moves: Vec<&Pt> = points.iter().filter(|p| p.kind == Kind::Move).collect();
    let mut sp = Vec::new();
    for w in moves.windows(2) {
        let dt = w[1].t - w[0].t;
        if dt > 0.0 {
            sp.push(((w[0].t + w[1].t) / 2.0, dist(w[0], w[1]) / dt * 1000.0));
        }
    }
    if sp.len() < MIN_PROFILE_SPEEDS {
        return None;
    }
    let t0 = moves[0].t;
    let span = moves[moves.len() - 1].t - t0;
    if span <= 0.0 {
        return None;
    }
    let mut peak_at = sp[0];
    for &s in &sp {
        if s.1 > peak_at.1 {
            peak_at = s;
        }
    }
    Some((peak_at.0 - t0) / span)
}

fn band(v: f64, lo: f64, hi: f64, ramp: f64) -> f64 {
    if v < lo {
        clamp01(1.0 - (lo - v) / ramp)
    } else if v > hi {
        clamp01(1.0 - (v - hi) / ramp)
    } else {
        1.0
    }
}

/// score.ts:384-405, pointer and wheel.
fn score(samples: &[Sample]) -> ScoreResult {
    let pointer: Vec<Pt> = samples
        .iter()
        .filter_map(|s| match *s {
            Sample::Move { t, x, y } => Some(Pt {
                t,
                x,
                y,
                kind: Kind::Move,
            }),
            Sample::Down { t, x, y } => Some(Pt {
                t,
                x,
                y,
                kind: Kind::Down,
            }),
            Sample::Up { t, x, y } => Some(Pt {
                t,
                x,
                y,
                kind: Kind::Up,
            }),
            Sample::Wheel { .. } => None,
        })
        .collect();
    let presses = pointer.iter().filter(|p| p.kind == Kind::Down).count();

    let teleports = pointer
        .windows(2)
        .filter(|w| dist(&w[0], &w[1]) >= TELEPORT_PX)
        .count();
    let mut ratios = Vec::new();
    let mut peaks = Vec::new();
    for (start, points) in approaches(&pointer) {
        let press = points[points.len() - 1];
        let direct = dist(&start, &press);
        if direct < MIN_SEGMENT_PX {
            continue;
        }
        let mut len = 0.0;
        let mut prev = start;
        for p in &points {
            len += dist(&prev, p);
            prev = *p;
        }
        ratios.push(len / direct);
        if let Some(p) = peak(&points) {
            peaks.push(p);
        }
    }
    let mut hovers = Vec::new();
    let mut dwells = Vec::new();
    for (i, s) in pointer.iter().enumerate() {
        if s.kind != Kind::Down {
            continue;
        }
        if i > 0 {
            hovers.push(s.t - pointer[i - 1].t);
        }
        if let Some(up) = pointer[i + 1..].iter().find(|p| p.kind == Kind::Up) {
            dwells.push(up.t - s.t);
        }
    }

    // score.ts:299-323: wheel bursts of three or more, their interval mean and CV.
    let wheels: Vec<f64> = samples
        .iter()
        .filter_map(|s| match *s {
            Sample::Wheel { t } => Some(t),
            _ => None,
        })
        .collect();
    let mut bursts: Vec<Vec<f64>> = Vec::new();
    for t in wheels {
        match bursts.last_mut() {
            Some(last) if t - last[last.len() - 1] <= WHEEL_BURST_GAP_MS => last.push(t),
            _ => bursts.push(vec![t]),
        }
    }
    let mut interval_means = Vec::new();
    let mut interval_cvs = Vec::new();
    for b in bursts.iter().filter(|b| b.len() >= 3) {
        let iv: Vec<f64> = b.windows(2).map(|w| w[1] - w[0]).collect();
        interval_means.push(mean(&iv).unwrap_or(0.0));
        interval_cvs.push(cv(&iv).unwrap_or(0.0));
    }

    // score.ts:347-370.
    let wheel = match (mean(&interval_cvs), mean(&interval_means)) {
        (Some(c), Some(m)) => Some(clamp01(c / 0.2).min(clamp01((m - 4.0) / 8.0))),
        _ => None,
    };
    let scores = Scores {
        straightness: median(&ratios).map(|s| clamp01((s - 1.002) / 0.018)),
        teleports: (presses > 0).then(|| clamp01(1.0 - teleports as f64 / presses as f64)),
        speed_profile: median(&peaks).map(|p| band(p, 0.25, 0.65, 0.2)),
        hover: median(&hovers).map(|h| clamp01(h / 40.0)),
        dwell: median(&dwells).map(|d| clamp01((d - 10.0) / 40.0)),
        wheel,
    };
    // score.ts:372-382.
    let (mut num, mut den) = (0.0, 0.0);
    for (v, w) in [
        (scores.straightness, W_STRAIGHTNESS),
        (scores.teleports, W_TELEPORTS),
        (scores.speed_profile, W_SPEED_PROFILE),
        (scores.hover, W_HOVER),
        (scores.dwell, W_DWELL),
        (scores.wheel, W_WHEEL),
    ] {
        if let Some(v) = v {
            num += v * w;
            den += w;
        }
    }
    ScoreResult {
        humanness: (den > 0.0).then(|| num / den),
        scores,
        teleports,
    }
}

// --- the recording (gesture-recording.ts) ----------------------------------------------

const BUTTONS: u8 = BUTTON_LEFT | BUTTON_MIDDLE | BUTTON_RIGHT;
const WHEEL: u8 = BUTTON_WHEEL_UP | BUTTON_WHEEL_DOWN;

/// gesture-recording.ts:24-46, over the ops bolide sends: a pointer event that does not
/// move the pointer is no `move`, one that sets a button bit is a `down`, clears one an
/// `up`, and a wheel bit going down is one notch.
fn samples_of(ops: &[RfbOp], start: f64, from: &mut (u16, u16, u8), out: &mut Vec<Sample>) -> f64 {
    let mut t = start;
    for op in ops {
        match *op {
            RfbOp::Sleep { ms } => t += ms as f64,
            RfbOp::Key { .. } => {}
            RfbOp::Pointer { x, y, buttons } => {
                let (fx, fy) = (f64::from(x), f64::from(y));
                if (x, y) != (from.0, from.1) {
                    out.push(Sample::Move { t, x: fx, y: fy });
                }
                let was = from.2 & BUTTONS;
                let now = buttons & BUTTONS;
                if now & !was != 0 {
                    out.push(Sample::Down { t, x: fx, y: fy });
                }
                if was & !now != 0 {
                    out.push(Sample::Up { t, x: fx, y: fy });
                }
                if buttons & WHEEL & !(from.2 & WHEEL) != 0 {
                    out.push(Sample::Wheel { t });
                }
                *from = (x, y, buttons);
            }
        }
    }
    t - start
}

#[derive(Clone, Copy)]
enum Planner {
    Direct,
    Humanized { tempo: f64 },
}

/// gesture-recording.ts:52-70: plan and play each act from where the last ended, with
/// `think()` ms between acts (the agent's turn).
fn record(
    planner: Planner,
    task: &[ComputerAction],
    seed: u32,
    mut think: impl FnMut() -> f64,
) -> Vec<Sample> {
    let ctx = PlanContext {
        width: 1280,
        height: 800,
        drag: DragPacing::DEFAULT,
    };
    let mut random = SeededRandom::new(seed);
    let mut pointer = (640u16, 400u16, 0u8);
    let mut t = 1000.0;
    let mut out = Vec::new();
    for act in task {
        let ops = match planner {
            Planner::Direct => plan(act, ctx).expect("planned"),
            Planner::Humanized { tempo } => {
                let from = (i32::from(pointer.0), i32::from(pointer.1));
                match gesture(act, &ctx, from, tempo, &mut random).expect("planned") {
                    Some(g) => to_ops(&g, &ctx),
                    None => plan(act, ctx).expect("planned"),
                }
            }
        };
        t += samples_of(&ops, t, &mut pointer, &mut out) + think();
    }
    out
}

/// humanized-score.test.ts:13-23, without its typing (`type_text` twice and a Tab).
fn task() -> Vec<ComputerAction> {
    vec![
        ComputerAction::LeftClick {
            coordinate: [420, 180],
        },
        ComputerAction::LeftClick {
            coordinate: [430, 260],
        },
        ComputerAction::Scroll {
            coordinate: [640, 500],
            scroll_direction: ScrollDirection::Down,
            scroll_amount: 6,
        },
        ComputerAction::DoubleClick {
            coordinate: [300, 420],
        },
        ComputerAction::LeftClickDrag {
            start_coordinate: [200, 600],
            coordinate: [700, 640],
        },
        ComputerAction::LeftClick {
            coordinate: [980, 690],
        },
    ]
}

fn score_of(planner: Planner, seed: u32) -> ScoreResult {
    let mut think = SeededRandom::new(seed + 1000);
    score(&record(planner, &task(), seed, || {
        1500.0 + 1500.0 * think.next_f64()
    }))
}

#[test]
fn direct_scores_like_a_bot() {
    let r = score_of(Planner::Direct, 1);
    let humanness = r.humanness.expect("measured");
    assert!(humanness < 0.4, "{humanness}: {:?}", r.scores);
}

#[test]
fn humanized_scores_like_a_person_on_every_seed() {
    for seed in 1..=20 {
        let r = score_of(Planner::Humanized { tempo: 1.0 }, seed);
        assert_eq!(r.teleports, 0, "seed {seed}");
        let humanness = r.humanness.expect("measured");
        assert!(humanness >= 0.7, "seed {seed}: {humanness} {:?}", r.scores);
    }
}

/// Tempo 0.25 quarters every pause, so hover and dwell fall below the human bands on
/// purpose. What must survive is the shape: no teleport, a curved path peaking mid-way.
/// (meteor's test also holds the key intervals' variation; there is no typing here.)
#[test]
fn humanized_at_its_fastest_tempo_is_fast_but_still_shaped() {
    let r = score(&record(
        Planner::Humanized { tempo: 0.25 },
        &task(),
        3,
        || 2000.0,
    ));
    assert_eq!(r.teleports, 0);
    assert_eq!(r.scores.straightness, Some(1.0));
    assert_eq!(r.scores.speed_profile, Some(1.0));
    let direct = score_of(Planner::Direct, 3).humanness.expect("measured");
    assert!(r.humanness.expect("measured") > direct);
}
