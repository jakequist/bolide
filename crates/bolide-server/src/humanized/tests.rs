//! The humanized plan as data. Ported from meteor's `desktops/interaction/tests/
//! humanized.test.ts` and `random.test.ts` (commit `bba97bcb10c6`), pointer half: what a
//! person's hand looks like is asserted as properties of the gesture — the path, the
//! pauses — under a seeded stream, so every property is checked on the same bytes each run.
//! Then bolide's own: the ops on the wire, the screen's edges, and direct's acts untouched.

use super::*;
use crate::actions::{DragPacing, PlanContext};

const W: u16 = 1920;
const H: u16 = 1080;

const CTX: PlanContext = PlanContext {
    width: W,
    height: H,
    drag: DragPacing::DEFAULT,
};

const SEEDS: std::ops::RangeInclusive<u32> = 1..=40;

fn plan_at(action: ComputerAction, from: (i32, i32), tempo: f64, seed: u32) -> Gesture {
    gesture(&action, &CTX, from, tempo, &mut SeededRandom::new(seed))
        .expect("planned")
        .expect("a pointer act")
}

/// meteor's `ctx()`: the cursor at (100, 100), tempo 1, seed 42.
fn plan(action: ComputerAction) -> Gesture {
    plan_at(action, (100, 100), 1.0, 42)
}

fn click(x: i32, y: i32) -> ComputerAction {
    ComputerAction::LeftClick { coordinate: [x, y] }
}

fn mv(x: i32, y: i32) -> ComputerAction {
    ComputerAction::MouseMove { coordinate: [x, y] }
}

fn scroll(x: i32, y: i32, dir: ScrollDirection, amount: u32) -> ComputerAction {
    ComputerAction::Scroll {
        coordinate: [x, y],
        scroll_direction: dir,
        scroll_amount: amount,
    }
}

fn press_index(g: &Gesture) -> usize {
    g.events
        .iter()
        .position(|e| e.buttons != 0)
        .expect("a press")
}

/// Every step from the cursor through each pointer event: the largest jump.
fn max_step(from: (i32, i32), g: &Gesture) -> f64 {
    let mut prev = from;
    let mut max: f64 = 0.0;
    for e in &g.events {
        max = max.max(f64::from(e.x - prev.0).hypot(f64::from(e.y - prev.1)));
        prev = (e.x, e.y);
    }
    max
}

fn non_decreasing(g: &Gesture) -> bool {
    g.events.windows(2).all(|w| w[1].t >= w[0].t)
        && g.duration_ms >= g.events.last().map_or(0.0, |e| e.t)
}

// --- the random stream (random.test.ts) -------------------------------------------------

/// The values meteor's `seededRandom` gives, computed by running `random.ts:11-20` under
/// node: the port is the same generator, not one like it.
#[test]
fn the_stream_is_meteors_mulberry32_draw_for_draw() {
    for (seed, expected) in [
        (
            0,
            [
                0.266_429_208_684_712_65,
                0.000_329_745_700_582_861_9,
                0.223_272_027_447_819_7,
                0.146_202_147_938_311_1,
            ],
        ),
        (
            42,
            [
                0.601_103_751_920_163_6,
                0.448_290_558_997_541_67,
                0.852_465_793_490_409_9,
                0.669_734_041_439_369_3,
            ],
        ),
        (
            u32::MAX,
            [
                0.896_422_614_110_633_7,
                0.189_478_256_739_676,
                0.715_652_678_161_859_5,
                0.944_059_909_321_367_7,
            ],
        ),
    ] {
        let mut r = SeededRandom::new(seed);
        let got: Vec<f64> = (0..4).map(|_| r.next_f64()).collect();
        assert_eq!(got, expected, "seed {seed}");
    }
}

#[test]
fn the_same_seed_is_the_same_stream_and_another_is_not() {
    let take = |seed| {
        let mut r = SeededRandom::new(seed);
        (0..5).map(|_| r.next_f64()).collect::<Vec<_>>()
    };
    assert_eq!(take(9), take(9));
    assert_ne!(take(1), take(2));
}

#[test]
fn the_stream_draws_uniformly_from_zero_to_one() {
    let mut r = SeededRandom::new(123);
    let xs: Vec<f64> = (0..10_000).map(|_| r.next_f64()).collect();
    assert!(xs.iter().all(|x| (0.0..1.0).contains(x)));
    let mean = xs.iter().sum::<f64>() / xs.len() as f64;
    assert!((0.48..0.52).contains(&mean), "mean {mean}");
}

// --- the pointer (humanized.test.ts) ----------------------------------------------------

#[test]
fn is_deterministic_under_a_seed_and_different_under_another() {
    let a = plan_at(click(700, 400), (100, 100), 1.0, 7);
    let b = plan_at(click(700, 400), (100, 100), 1.0, 7);
    let c = plan_at(click(700, 400), (100, 100), 1.0, 8);
    assert_eq!(a, b);
    assert_ne!(a, c);
}

#[test]
fn a_click_lands_within_two_px_of_the_point_and_the_gesture_ends_there() {
    for seed in SEEDS {
        let g = plan_at(click(913, 57), (100, 100), 1.0, seed);
        let press = &g.events[press_index(&g)];
        assert!((press.x - 913).abs() <= 2 && (press.y - 57).abs() <= 2);
        assert!((g.ends_at.0 - 913).abs() <= 2 && (g.ends_at.1 - 57).abs() <= 2);
        assert!(non_decreasing(&g), "seed {seed}");
    }
}

#[test]
fn starts_from_the_cursor_and_never_teleports_every_step_is_under_80_px() {
    for seed in SEEDS {
        for tempo in [0.25, 1.0, 4.0] {
            let g = plan_at(click(1900, 1060), (5, 5), tempo, seed);
            let step = max_step((5, 5), &g);
            assert!(step < 80.0, "seed {seed} tempo {tempo}: {step}");
        }
    }
}

#[test]
fn curves_the_path_is_longer_than_the_straight_line() {
    for seed in SEEDS {
        let g = plan_at(mv(700, 500), (100, 100), 1.0, seed);
        let mut len = 0.0;
        let mut prev = (100, 100);
        for e in &g.events {
            len += f64::from(e.x - prev.0).hypot(f64::from(e.y - prev.1));
            prev = (e.x, e.y);
        }
        assert!(len / 600f64.hypot(400.0) > 1.005, "seed {seed}");
    }
}

#[test]
fn peaks_in_speed_in_the_middle_of_the_movement() {
    let mut peaks = Vec::new();
    for seed in SEEDS {
        let g = plan_at(mv(400, 120), (100, 100), 1.0, seed);
        let ps = &g.events;
        let (mut best_v, mut best_t) = (-1.0, 0.0);
        for i in 1..ps.len() {
            let dt = ps[i].t - ps[i - 1].t;
            if dt <= 0.0 {
                continue;
            }
            let v = f64::from(ps[i].x - ps[i - 1].x).hypot(f64::from(ps[i].y - ps[i - 1].y)) / dt;
            if v > best_v {
                best_v = v;
                best_t = (ps[i].t + ps[i - 1].t) / 2.0;
            }
        }
        let span = ps[ps.len() - 1].t - ps[0].t;
        peaks.push((best_t - ps[0].t) / span);
    }
    peaks.sort_by(f64::total_cmp);
    let median = peaks[peaks.len() >> 1];
    assert!((0.35..0.65).contains(&median), "median {median}");
}

#[test]
fn samples_at_display_rate_60_to_120_hz() {
    let g = plan(mv(900, 700));
    let gaps: Vec<f64> = g.events.windows(2).map(|w| w[1].t - w[0].t).collect();
    let mean = gaps.iter().sum::<f64>() / gaps.len() as f64;
    assert!(
        (1000.0 / 125.0..=1000.0 / 55.0).contains(&mean),
        "mean {mean}"
    );
}

#[test]
fn takes_longer_the_further_it_goes() {
    let near = plan_at(mv(140, 100), (100, 100), 1.0, 3);
    let far = plan_at(mv(1500, 900), (100, 100), 1.0, 3);
    assert!(far.duration_ms > near.duration_ms);
}

#[test]
fn overshoots_and_corrects_on_a_long_move() {
    let overshot = SEEDS
        .filter(|&seed| {
            plan_at(mv(1300, 100), (100, 100), 1.0, seed)
                .events
                .iter()
                .any(|e| e.x > 1303)
        })
        .count();
    assert!(overshot > SEEDS.count() / 3, "{overshot} overshot");
}

#[test]
fn a_click_hovers_40_to_150_ms_before_the_press_and_holds_it_60_to_120_ms() {
    for seed in SEEDS {
        let g = plan_at(click(500, 300), (100, 100), 1.0, seed);
        let i = press_index(&g);
        let hover = g.events[i].t - g.events[i - 1].t;
        let dwell = g.events[i + 1].t - g.events[i].t;
        assert!(
            (40.0..=150.0).contains(&hover),
            "seed {seed}: hover {hover}"
        );
        assert!(
            (60.0..=120.0).contains(&dwell),
            "seed {seed}: dwell {dwell}"
        );
        assert_eq!(g.events[i + 1].buttons, 0);
    }
}

#[test]
fn presses_the_button_it_was_asked_for() {
    for (action, mask) in [
        (
            ComputerAction::RightClick {
                coordinate: [500, 300],
            },
            BUTTON_RIGHT,
        ),
        (
            ComputerAction::MiddleClick {
                coordinate: [500, 300],
            },
            BUTTON_MIDDLE,
        ),
        (click(500, 300), BUTTON_LEFT),
    ] {
        let g = plan(action);
        assert_eq!(g.events[press_index(&g)].buttons, mask);
    }
}

#[test]
fn a_click_already_at_the_point_does_not_wander() {
    let g = plan(click(100, 100));
    assert!(max_step((100, 100), &g) <= 3.0);
}

#[test]
fn a_pointer_already_resting_within_the_landing_radius_does_not_move_again() {
    for seed in SEEDS {
        for action in [
            click(1000, 650),
            ComputerAction::DoubleClick {
                coordinate: [1000, 650],
            },
            mv(1000, 650),
            scroll(1000, 650, ScrollDirection::Down, 2),
        ] {
            for at in [(1000, 650), (1001, 649), (999, 651)] {
                let g = plan_at(action.clone(), at, 1.0, seed);
                assert_eq!(g.ends_at, at, "{action:?} from {at:?} seed {seed}");
                for e in &g.events {
                    assert_eq!((e.x, e.y), at, "{action:?} from {at:?} seed {seed}");
                }
            }
        }
    }
}

#[test]
fn aimed_first_a_click_at_the_same_point_leaves_the_pointer_where_the_aim_left_it() {
    for seed in SEEDS {
        let mut random = SeededRandom::new(seed);
        let aim = gesture(&mv(1000, 650), &CTX, (200, 300), 1.0, &mut random)
            .expect("planned")
            .expect("a pointer act");
        let click = gesture(&click(1000, 650), &CTX, aim.ends_at, 1.0, &mut random)
            .expect("planned")
            .expect("a pointer act");
        assert_eq!(click.ends_at, aim.ends_at, "seed {seed}");
    }
}

/// The presses of a gesture: each event that sets the left button from none.
fn presses(g: &Gesture) -> Vec<&Event> {
    g.events
        .iter()
        .enumerate()
        .filter(|(i, e)| e.buttons == BUTTON_LEFT && (*i == 0 || g.events[i - 1].buttons == 0))
        .map(|(_, e)| e)
        .collect()
}

#[test]
fn a_double_click_is_two_clicks_the_second_inside_a_double_click_interval() {
    let g = plan(ComputerAction::DoubleClick {
        coordinate: [500, 300],
    });
    let downs = presses(&g);
    assert_eq!(downs.len(), 2);
    let gap = downs[1].t - downs[0].t;
    assert!(gap > 100.0 && gap < 400.0, "gap {gap}");
    // The two presses are at the same place: a double click that moved is two clicks.
    assert_eq!((downs[1].x, downs[1].y), (downs[0].x, downs[0].y));
}

#[test]
fn a_drag_presses_at_the_origin_carries_along_a_path_and_releases_at_the_target() {
    for seed in 1..=10 {
        let g = plan_at(
            ComputerAction::LeftClickDrag {
                start_coordinate: [200, 200],
                coordinate: [800, 600],
            },
            (50, 50),
            1.0,
            seed,
        );
        let ps = &g.events;
        let first_down = ps
            .iter()
            .position(|e| e.buttons == BUTTON_LEFT)
            .expect("a press");
        let last_up = ps.len() - 1;
        assert!((ps[first_down].x - 200).abs() <= 2);
        assert_eq!(ps[last_up].buttons, 0);
        assert!((ps[last_up].x - 800).abs() <= 2 && (ps[last_up].y - 600).abs() <= 2);
        // The carry is a path, with the button held throughout.
        let carried = &ps[first_down..last_up];
        assert!(carried.len() > 10);
        assert!(carried.iter().all(|e| e.buttons == BUTTON_LEFT));
        assert!(max_step((50, 50), &g) < 80.0);
    }
}

// --- scrolling (humanized.test.ts) ------------------------------------------------------

fn wheel_downs(g: &Gesture) -> Vec<&Event> {
    g.events
        .iter()
        .filter(|e| e.buttons & (BUTTON_WHEEL_UP | BUTTON_WHEEL_DOWN) != 0)
        .collect()
}

#[test]
fn a_scroll_moves_to_the_point_first_humanly_then_notches_the_exact_count() {
    let g = plan_at(
        scroll(600, 400, ScrollDirection::Down, 7),
        (50, 50),
        1.0,
        42,
    );
    let wheels = wheel_downs(&g);
    assert_eq!(wheels.len(), 7);
    assert!(wheels.iter().all(|e| e.buttons == BUTTON_WHEEL_DOWN));
    assert!(max_step((50, 50), &g) < 80.0);
    assert!((wheels[0].x - 600).abs() <= 2);
}

#[test]
fn notches_accelerate_and_decelerate_rather_than_arrive_as_a_burst() {
    // One run of six: five intervals. On average over many seeds the ends are slower than
    // the middle — per seed, the notch jitter may reorder a pair, as a hand's does.
    let mut sum = [0.0; 5];
    for seed in SEEDS {
        let g = plan_at(
            scroll(100, 100, ScrollDirection::Up, 6),
            (100, 100),
            1.0,
            seed,
        );
        let ts: Vec<f64> = wheel_downs(&g).iter().map(|e| e.t).collect();
        let iv: Vec<f64> = ts.windows(2).map(|w| w[1] - w[0]).collect();
        assert_eq!(iv.len(), 5);
        assert!(iv.iter().all(|&v| v > 5.0), "seed {seed}: {iv:?}");
        for (s, v) in sum.iter_mut().zip(iv) {
            *s += v;
        }
    }
    assert!(sum[2] < sum[0] * 0.8, "{sum:?}");
    assert!(sum[2] < sum[4] * 0.8, "{sum:?}");
}

#[test]
fn pauses_between_long_scroll_runs() {
    let g = plan(scroll(100, 100, ScrollDirection::Down, 10));
    let ts: Vec<f64> = wheel_downs(&g).iter().map(|e| e.t).collect();
    let longest = ts.windows(2).map(|w| w[1] - w[0]).fold(0.0, f64::max);
    assert!(longest > 200.0, "longest gap {longest}");
}

#[test]
fn a_scroll_of_nothing_does_not_even_move() {
    let g = plan(scroll(900, 900, ScrollDirection::Down, 0));
    assert_eq!(g.events, vec![]);
    assert_eq!(g.ends_at, (100, 100));
    let ops = plan_humanized(
        &scroll(900, 900, ScrollDirection::Down, 0),
        CTX,
        Some((100, 100)),
        &mut SeededRandom::new(1),
    )
    .expect("planned");
    assert_eq!(ops, vec![]);
}

// --- tempo (humanized.test.ts) ----------------------------------------------------------

#[test]
fn a_clicks_hover_and_dwell_scale_with_tempo_and_where_it_lands_does_not() {
    let at = |tempo| {
        let g = plan_at(click(640, 360), (100, 100), tempo, 11);
        let i = press_index(&g);
        (
            g.events[i].t - g.events[i - 1].t,
            g.events[i + 1].t - g.events[i].t,
            g.ends_at,
            g.duration_ms,
        )
    };
    let fast = at(0.5);
    let nominal = at(1.0);
    assert!((fast.0 - nominal.0 / 2.0).abs() < 0.5, "hover");
    assert!((fast.1 - nominal.1 / 2.0).abs() < 0.5, "dwell");
    assert_eq!(fast.2, nominal.2);
    assert!(fast.3 < nominal.3 * 0.6);
}

/// meteor's test plans a `type_text`; typing is not humanized here, so a click.
#[test]
fn clamps_a_tempo_outside_a_quarter_to_four_rather_than_trusting_it() {
    assert_eq!(
        plan_at(click(640, 360), (100, 100), 100.0, 1),
        plan_at(click(640, 360), (100, 100), 4.0, 1)
    );
    assert_eq!(
        plan_at(click(640, 360), (100, 100), f64::NAN, 1),
        plan_at(click(640, 360), (100, 100), 1.0, 1)
    );
}

// --- bolide's own -----------------------------------------------------------------------

/// What bolide sends: one stream for a whole session, so the plan of a sequence of acts
/// is the same under the same seed and differs under another.
#[test]
fn the_same_seed_plans_the_same_ops_for_the_same_acts() {
    let session = |seed| {
        let mut random = SeededRandom::new(seed);
        let mut from = None;
        let mut out = Vec::new();
        for act in [
            click(300, 200),
            ComputerAction::LeftClickDrag {
                start_coordinate: [300, 200],
                coordinate: [900, 700],
            },
            scroll(800, 500, ScrollDirection::Down, 3),
            mv(10, 10),
        ] {
            let ops = plan_humanized(&act, CTX, from, &mut random).expect("planned");
            from = last_pointer(&ops).or(from);
            out.push(ops);
        }
        out
    };
    assert_eq!(session(1), session(1));
    assert_ne!(session(1), session(2));
}

fn last_pointer(ops: &[RfbOp]) -> Option<(u16, u16)> {
    ops.iter().rev().find_map(|op| match *op {
        RfbOp::Pointer { x, y, .. } => Some((x, y)),
        _ => None,
    })
}

fn pointers(ops: &[RfbOp]) -> Vec<(u16, u16, u8)> {
    ops.iter()
        .filter_map(|op| match *op {
            RfbOp::Pointer { x, y, buttons } => Some((x, y, buttons)),
            _ => None,
        })
        .collect()
}

#[test]
fn the_pointer_starts_from_where_bolide_last_left_it() {
    for seed in SEEDS {
        let ops = plan_humanized(
            &click(1500, 900),
            CTX,
            Some((321, 123)),
            &mut SeededRandom::new(seed),
        )
        .expect("planned");
        let (x, y, _) = pointers(&ops)[0];
        let first = f64::from(x as i32 - 321).hypot(f64::from(y as i32 - 123));
        assert!(
            first <= MAX_STEP_PX + 3.0,
            "seed {seed}: first step {first}"
        );
    }
}

/// Before bolide has sent a PointerEvent it does not know where the pointer is, and takes
/// it to be at the centre of the screen, as meteor does.
#[test]
fn with_no_pointer_event_yet_the_pointer_starts_from_the_centre() {
    let ops =
        plan_humanized(&click(100, 100), CTX, None, &mut SeededRandom::new(5)).expect("planned");
    let (x, y, _) = pointers(&ops)[0];
    let first = f64::from(x as i32 - 960).hypot(f64::from(y as i32 - 540));
    assert!(
        first <= MAX_STEP_PX + 3.0,
        "first step {first} from the centre"
    );
}

#[test]
fn a_triple_click_is_three_presses_at_one_place() {
    for seed in SEEDS {
        let g = plan_at(
            ComputerAction::TripleClick {
                coordinate: [500, 300],
            },
            (100, 100),
            1.0,
            seed,
        );
        let downs = presses(&g);
        assert_eq!(downs.len(), 3);
        assert!(downs.iter().all(|d| (d.x, d.y) == (downs[0].x, downs[0].y)));
        assert_eq!(g.events.last().map(|e| e.buttons), Some(0));
        for w in downs.windows(2) {
            let gap = w[1].t - w[0].t;
            assert!(gap > 100.0 && gap < 400.0, "seed {seed}: gap {gap}");
        }
    }
}

/// A landing, an overshoot or a bow near an edge would leave the screen; a PointerEvent
/// cannot, so every one is kept on it.
#[test]
fn every_pointer_event_stays_on_the_screen() {
    let small = PlanContext {
        width: 640,
        height: 480,
        ..CTX
    };
    for seed in SEEDS {
        let mut random = SeededRandom::new(seed);
        let mut from = None;
        for act in [
            click(0, 0),
            click(639, 479),
            mv(0, 479),
            ComputerAction::LeftClickDrag {
                start_coordinate: [639, 0],
                coordinate: [0, 0],
            },
            scroll(639, 479, ScrollDirection::Up, 2),
        ] {
            let ops = plan_humanized(&act, small, from, &mut random).expect("planned");
            for (x, y, _) in pointers(&ops) {
                assert!(x < 640 && y < 480, "seed {seed}: {act:?} sent ({x}, {y})");
            }
            from = last_pointer(&ops).or(from);
        }
    }
}

/// The ops keep the gesture's timing: the sleeps before each event add up to its time,
/// rounded to the millisecond, and the trailing ones to the gesture's length.
#[test]
fn the_ops_wait_out_the_gestures_timing() {
    for seed in SEEDS {
        let mut random = SeededRandom::new(seed);
        let act = ComputerAction::DoubleClick {
            coordinate: [700, 400],
        };
        let g = gesture(&act, &CTX, (100, 100), 1.0, &mut random.clone())
            .expect("planned")
            .expect("a pointer act");
        let ops = plan_humanized(&act, CTX, Some((100, 100)), &mut random).expect("planned");
        let mut now = 0u64;
        let mut seen = 0;
        for op in &ops {
            match *op {
                RfbOp::Sleep { ms } => now += ms,
                RfbOp::Pointer { buttons, .. } => {
                    let e = &g.events[seen];
                    assert_eq!(buttons, e.buttons);
                    assert_eq!(now, js_round(e.t) as u64, "seed {seed}: event {seen}");
                    seen += 1;
                }
                RfbOp::Key { .. } => panic!("a click typed"),
            }
        }
        assert_eq!(seen, g.events.len());
        assert_eq!(now, js_round(g.duration_ms) as u64);
    }
}

/// A click takes a person's time, as the README says: about half a second for a short
/// reach, about a second across half a screen (Fitts's law, and the overshoot a long move
/// makes), and never more than 2 s even corner to corner.
#[test]
fn a_humanized_click_takes_half_a_second_to_two_seconds() {
    let took = |act: ComputerAction, from, seed| -> u64 {
        plan_humanized(&act, CTX, Some(from), &mut SeededRandom::new(seed))
            .expect("planned")
            .iter()
            .map(|op| match op {
                RfbOp::Sleep { ms } => *ms,
                _ => 0,
            })
            .sum()
    };
    let median = |act: ComputerAction| {
        let mut all: Vec<u64> = SEEDS.map(|s| took(act.clone(), (100, 100), s)).collect();
        all.sort_unstable();
        (all[0], all[all.len() / 2])
    };
    let (fastest, short) = median(click(250, 180));
    assert!(
        (350..=700).contains(&short),
        "a short reach's median {short} ms"
    );
    assert!(fastest >= 250, "fastest {fastest} ms");
    let (_, half) = median(click(1000, 600));
    assert!(
        (800..=1400).contains(&half),
        "half a screen's median {half} ms"
    );
    for seed in SEEDS {
        let ms = took(click(1900, 1060), (5, 5), seed);
        assert!(ms <= 2000, "seed {seed}: corner to corner took {ms} ms");
    }
}

/// bolide honours the amount asked for; meteor's ten-notch cap is not ported.
#[test]
fn a_large_scroll_is_honoured_in_full() {
    let g = plan(scroll(500, 500, ScrollDirection::Down, 500));
    assert_eq!(wheel_downs(&g).len(), 500);
}

/// Typing and keys are not humanized: they plan exactly what direct mode plans.
#[test]
fn typing_keys_waits_and_reads_plan_exactly_as_direct_mode_does() {
    for act in [
        ComputerAction::Type {
            text: "Hello, world!\n".into(),
        },
        ComputerAction::Key {
            text: "ctrl+shift+t".into(),
        },
        ComputerAction::Wait { duration: 1.5 },
        ComputerAction::Screenshot,
        ComputerAction::CursorPosition,
    ] {
        let mut random = SeededRandom::new(1);
        assert_eq!(
            plan_humanized(&act, CTX, Some((5, 5)), &mut random),
            crate::actions::plan(&act, CTX),
            "{act:?}"
        );
        assert_eq!(random, SeededRandom::new(1), "{act:?} drew from the stream");
    }
}

#[test]
fn an_off_screen_coordinate_is_still_an_error_not_a_clamp() {
    for act in [
        click(1920, 5),
        ComputerAction::LeftClickDrag {
            start_coordinate: [5, 5],
            coordinate: [5, 1080],
        },
        scroll(-1, 5, ScrollDirection::Up, 1),
        mv(5, -1),
    ] {
        assert!(matches!(
            plan_humanized(&act, CTX, None, &mut SeededRandom::new(1)),
            Err(ActionError::OutOfBounds { .. })
        ));
    }
}

#[test]
fn the_pointer_modes_are_spelled_once() {
    for mode in PointerMode::ALL {
        assert_eq!(PointerMode::from_name(mode.as_str()), Some(mode));
    }
    assert_eq!(PointerMode::from_name("human"), None);
    assert_eq!(PointerMode::default(), PointerMode::Direct);
}
