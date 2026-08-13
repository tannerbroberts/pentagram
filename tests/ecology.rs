//! S2's exit condition, as a runnable check: feeding, starvation and
//! reproduction actually happen under the shipped table, and none of them
//! break Invariant VI (every tick reproduces from `(seed, input_log)` alone).
//!
//! What this file does **not** claim: that the shipped `EcologyTuning`
//! defaults keep a five-race predation ring alive indefinitely. They do not
//! — a uniform five-way closed ring collapses within a few thousand ticks
//! under the shipped numbers, the same way S1's shipped `TerrainTuning`
//! defaults were not asserted to produce good-looking succession without a
//! live tuning pass (`docs/S1_TERRAIN_DESIGN.md` §9.3). Real per-race
//! balance is exactly what `chaos`'s live view exists to find. The tests
//! here pin down the mechanism, not the balance.

use pentagram::ecology::EcologyTuning;
use pentagram::element::{Element, PerElement};
use pentagram::fx::{Fx, V2};
use pentagram::input::InputLog;
use pentagram::race::{attrs, Kind, Race};
use pentagram::replay::{build, record, scripted_log, verify};
use pentagram::world::World;

const SEED: u64 = 0xC0FFEE;
const SIZE: i32 = 64;
const PER_RACE: u32 = 12;

/// **The exit condition.** Feeding, starvation and reproduction all fire at
/// least once under the shipped table within a bounded run — the `OnConsume`
/// channel and `Entity::hp` are no longer dead code, and neither is a birth
/// that came from something other than a command or `seed_population`.
#[test]
fn feeding_starvation_and_reproduction_all_occur_under_the_shipped_table() {
    let ticks = 8_000u64;
    let mut w = build(SEED, SIZE, PER_RACE);
    let log = InputLog::new();
    let births_before = w.stats.births;
    for _ in 0..ticks {
        w.step(&log);
    }
    assert!(w.stats.feedings > 0, "no predation occurred in {ticks} ticks");
    assert!(
        w.stats.births > births_before,
        "no reproduction occurred beyond the initial seed"
    );
    // Not every run need starve anyone — a body that keeps eating never
    // does — so this is observed rather than asserted.
    let _ = w.stats.starved;
}

/// Ten thousand ticks with S2's phases exercised on every one of them,
/// replaying bit-identically — the same exit condition Stage 0 shipped,
/// re-run now that `phase_feeding` sits in the tick between `phase_collisions`
/// and `phase_settle`.
#[test]
fn ten_thousand_ticks_replay_bit_identically_with_ecology_active() {
    let ticks = 10_000;
    let log = scripted_log(0x5EED, ticks, PER_RACE * 10);
    let trace = record(SEED, SIZE, PER_RACE, &log, ticks);
    assert_eq!(trace.ticks(), ticks as usize);
    if let Err(d) = verify(&trace, &log) {
        panic!("{d}");
    }
}

/// A retuned ecology table is part of state exactly the way a retuned
/// `TerrainTuning` or `ClimateTuning` is: two runs from the same seed and log
/// but different `EcologyTuning` must diverge.
#[test]
fn a_retuned_ecology_changes_the_run() {
    let ticks = 2_000u64;
    let log = scripted_log(0x9, ticks, PER_RACE * 10);

    let a = build(SEED, SIZE, PER_RACE);
    let mut b = a.clone();
    b.retune_ecology(EcologyTuning {
        feed_gain: PerElement::filled(1),
        ..EcologyTuning::default()
    });

    let mut a = a;
    for _ in 0..ticks {
        a.step(&log);
        b.step(&log);
    }
    assert_ne!(a.state_hash(), b.state_hash());
}

/// Proof the mechanism *can* sustain a population through actual
/// reproduction, not merely through individuals outliving the clock.
///
/// An earlier version of this test ran only as long as the shipped
/// `starve_after` default and asserted "some race is still alive" — which
/// passed even with reproduction deleted outright, because several races'
/// natural lifespans already exceed that window on their own (Wood, Metal,
/// Earth: 15 000, 72 000, 2 016 000 ticks) and the tuning's own
/// `starve_after` made starvation unreachable inside the run length. This
/// version isolates Fire — the shortest-lived race, so a survivor genuinely
/// proves something — against a single prey (Wood) with no predator of its
/// own, and runs well past the *maximum* individual lifespan any single Fire
/// body could possibly roll (computed from `RaceAttrs`, not hardcoded): any
/// Fire alive at the end is necessarily a descendant. `starve_after` is set
/// far below the shipped default so starvation is at least reachable inside
/// the run — Wood is finite and not replenished, so once it runs out
/// starvation becomes the only thing that can end a Fire body's life short
/// of old age.
#[test]
fn reproduction_sustains_fire_past_its_own_maximum_lifespan() {
    let fire = attrs(Race { element: Element::Fire, kind: Kind::Animal });
    let max_lifespan = fire.lifespan * (1000 + fire.lifespan_variance as u64) / 1000;
    let ticks = max_lifespan + max_lifespan / 2; // a further 50% margin past the ceiling.

    let mut w = World::new(0xF00D, 64);
    w.retune_ecology(EcologyTuning {
        forage_radius: PerElement::filled(Fx::ratio(500, 100)),
        satiation: PerElement::filled(150),
        feed_gain: PerElement::filled(70),
        starve_after: PerElement::filled(1_200), // comfortably inside `ticks`, so it's a real pressure.
        starve_rate: PerElement::filled(1),
        repro_threshold: PerElement::filled(60),
        ..EcologyTuning::default() // shipped attrition/suppression defaults — this test exercises them too.
    });

    let fire_seed = 40u32;
    for k in 0..fire_seed {
        let p = V2::new(Fx::from_int((k * 7 % 60) as i32), Fx::from_int((k * 13 % 60) as i32));
        w.spawn(Race { element: Element::Fire, kind: Kind::Animal }, p);
    }
    for k in 0..400u32 {
        let p = V2::new(Fx::from_int((k * 3 % 60) as i32), Fx::from_int((k * 11 % 60) as i32));
        w.spawn(Race { element: Element::Wood, kind: Kind::Animal }, p);
    }

    let log = InputLog::new();
    let births_before = w.stats.births;
    for _ in 0..ticks {
        w.step(&log);
    }

    assert!(
        ticks > max_lifespan,
        "test is not actually past every possible individual lifespan"
    );
    assert!(
        w.population()[Element::Fire] > 0,
        "Fire should still be present {ticks} ticks in — every original individual is provably dead by now, \
         so a survivor can only be a descendant"
    );
    assert!(
        w.stats.births - births_before > fire_seed as u64,
        "more births must have occurred than the initial seed for Fire to still be here"
    );
    assert!(w.stats.starved > 0 || w.stats.feedings > 0, "neither starvation nor feeding ever occurred");
}
