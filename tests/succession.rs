//! S1's exit condition, as a runnable check: "succession visibly cycling
//! with no absorbing state over 30 simulated days." See
//! `docs/S1_TERRAIN_DESIGN.md` §7 for how "cycling" and "absorbing state"
//! are translated into the concrete, numeric properties asserted below.
//!
//! The two 30-day tests are `#[ignore]`d out of the default `cargo test`
//! path — a 30-day run is roughly 430× longer than Stage 0's own 10 000-tick
//! exit condition — and run explicitly via `cargo test --release -- --ignored`,
//! mirroring how `soak` is a separate invocation rather than part of the
//! normal suite. The smoke test below stays in the default path and is fast
//! enough to catch an operator-order regression long before a 30-day run
//! would be needed to notice it.
//!
//! Direct, `World`-free proof that diffusion never moves mass further than
//! one cell per terrain tick (Invariant I) lives in `terrain.rs`'s own unit
//! tests (`diffusion_never_moves_further_than_one_cell_per_call`,
//! `diffusion_never_exceeds_the_flat_cap_per_edge`), not duplicated here.

use std::collections::BTreeSet;

use pentagram::element::Element;
use pentagram::input::InputLog;
use pentagram::race::TICKS_PER_DAY;
use pentagram::replay::scripted_log;
use pentagram::{World, TERRAIN_PERIOD};

const SEED: u64 = 0xC0FFEE;
const DAYS: u64 = 30;
const SIZE: i32 = 64;

fn dominant_element(w: &World) -> Element {
    Element::ALL.into_iter().max_by_key(|e| w.terrain.total(*e)).unwrap()
}

/// How many simulated days one climate season spans, derived from the
/// world's own tuning rather than hardcoded — the default ships five
/// seasons across exactly the 30-day exit-condition window (see
/// `climate.rs`'s `ClimateTuning::default`).
fn season_days(w: &World) -> u64 {
    (w.climate_tuning.season_ticks * TERRAIN_PERIOD) / TICKS_PER_DAY
}

/// (b)/(c) starved/saturated: no element's grid-wide total may sit at
/// exactly zero or at the grid's maximum for more than one full season. A
/// temporary seasonal trough or peak is expected and legitimate; a
/// permanent one is the absorbing-state failure this guards against.
fn check_no_frozen_extreme(streaks: &mut [u64; Element::COUNT], w: &World, day: u64) {
    let cells = (w.terrain.side as u64) * (w.terrain.side as u64);
    let max_total = cells * u16::MAX as u64;
    let limit = season_days(w);
    for e in Element::ALL {
        let total = w.terrain.total(e);
        let i = e.index();
        if total == 0 || total == max_total {
            streaks[i] += 1;
        } else {
            streaks[i] = 0;
        }
        assert!(
            streaks[i] <= limit,
            "{} sat at an absorbing extreme ({}) for more than one season ({} days), through day {}",
            e.name(),
            total,
            limit,
            day
        );
    }
}

/// (d) monoculture: no single element's share of total grid mass may stay
/// at or above 90% for the entire window.
fn assert_no_permanent_monoculture(monoculture_days: u64, total_days: u64) {
    assert!(
        monoculture_days < total_days,
        "one element held \u{2265}90% of grid mass on every sampled day \u{2014} monoculture, not succession"
    );
}

fn assert_no_run_longer_than(dominant: &[Element], max_run: u64) {
    let mut run = 1u64;
    for w in dominant.windows(2) {
        if w[0] == w[1] {
            run += 1;
            assert!(
                run <= max_run,
                "dominant element stuck at {:?} for {} consecutive days",
                w[0],
                run
            );
        } else {
            run = 1;
        }
    }
}

/// **The strong claim**: with zero population, climate and terrain
/// mechanics alone (deposit/consume never fire — there is nobody to fire
/// them) must cycle the dominant element through the entire ring at least
/// once across the 30-day window, matching the five-season climate cycle
/// exactly. This isolates the exit condition from ecological noise, the
/// same spirit as `tests/determinism.rs`'s
/// `the_world_keeps_churning_with_no_players_at_all`.
#[test]
#[ignore]
fn thirty_days_of_climate_alone_cycles_through_every_element() {
    let mut w = World::new(SEED, SIZE);
    let log = InputLog::new();
    let mut dominant = Vec::with_capacity(DAYS as usize);
    let mut last_hash = None;
    let mut streaks = [0u64; Element::COUNT];
    let mut monoculture_days = 0u64;

    for day in 0..DAYS {
        for _ in 0..TICKS_PER_DAY {
            w.step(&log);
        }

        // (a) frozen: the terrain must keep moving, day over day.
        let h = w.terrain.state_hash();
        assert_ne!(Some(h), last_hash, "terrain frozen at day {day}");
        last_hash = Some(h);

        check_no_frozen_extreme(&mut streaks, &w, day);

        let totals: Vec<u64> = Element::ALL.iter().map(|e| w.terrain.total(*e)).collect();
        let grand_total: u64 = totals.iter().sum();
        let peak = *totals.iter().max().unwrap();
        if grand_total > 0 && peak * 10 >= grand_total * 9 {
            monoculture_days += 1;
        }

        dominant.push(dominant_element(&w));
    }

    assert_no_permanent_monoculture(monoculture_days, DAYS);

    let distinct: BTreeSet<_> = dominant.iter().collect();
    assert_eq!(
        distinct.len(),
        Element::COUNT,
        "climate alone did not cycle through every element: {:?}",
        dominant
    );
}

/// **The weaker, more defensible claim**: with a starting population and no
/// restocking (S1 has no reproduction — see `docs/S1_TERRAIN_DESIGN.md`
/// §9.8), at least 3 of the 5 elements must appear as the dominant element
/// across the 30 daily samples, and no element may hold that position for
/// more than 10 consecutive days.
#[test]
#[ignore]
fn thirty_days_of_succession_visibly_cycles_with_a_population() {
    let mut w = World::new(SEED, SIZE);
    w.seed_population(20);
    let log = InputLog::new();
    let mut dominant = Vec::with_capacity(DAYS as usize);
    let mut streaks = [0u64; Element::COUNT];
    let mut monoculture_days = 0u64;

    for day in 0..DAYS {
        for _ in 0..TICKS_PER_DAY {
            w.step(&log);
        }
        check_no_frozen_extreme(&mut streaks, &w, day);

        let totals: Vec<u64> = Element::ALL.iter().map(|e| w.terrain.total(*e)).collect();
        let grand_total: u64 = totals.iter().sum();
        let peak = *totals.iter().max().unwrap();
        if grand_total > 0 && peak * 10 >= grand_total * 9 {
            monoculture_days += 1;
        }

        dominant.push(dominant_element(&w));
    }

    assert_no_permanent_monoculture(monoculture_days, DAYS);

    let distinct: BTreeSet<_> = dominant.iter().collect();
    assert!(distinct.len() >= 3, "only cycled through {:?}", dominant);
    assert_no_run_longer_than(&dominant, 10);
}

/// Fast, non-ignored smoke test kept in the default `cargo test` path:
/// two independent runs from the same seed must still agree bit-for-bit
/// once terrain is in the loop, and the grid must actually have moved.
/// A regression in operator order, a missed saturating op, or a
/// non-deterministic apportionment tie-break would show up here in well
/// under a second, long before a 30-day `--ignored` run would catch it.
#[test]
fn terrain_replays_bit_identically_and_the_grid_moves() {
    let ticks = TERRAIN_PERIOD * 40; // 40 terrain ticks — enough for every operator to fire repeatedly
    let log = scripted_log(0x5EED, ticks, 20);

    let mut a = World::new(SEED, 24);
    a.seed_population(4);
    let fresh_hash = a.terrain.state_hash();
    let mut b = a.clone();

    for _ in 0..ticks {
        a.step(&log);
        b.step(&log);
    }

    assert_eq!(
        a.state_hash(),
        b.state_hash(),
        "two independent runs from the same seed must agree once terrain is in the loop"
    );
    assert_ne!(
        a.terrain.state_hash(),
        fresh_hash,
        "the terrain grid must have changed over twelve simulated hours"
    );
}
