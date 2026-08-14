//! S3's exit condition — the same role `tests/succession.rs` played for S1
//! and `tests/determinism.rs`'s `ten_thousand_ticks_replay_bit_identically`
//! played for S0: a runnable check, not a hope. See
//! `docs/S3_ECOLOGY_LAYERS_DESIGN.md` §12 (S3.7) and §13 for what "no
//! mechanism is dead code," "structural invariants hold," and the named
//! runaway-risk mitigation are asked to mean here.
//!
//! **A real finding, made while landing this file, not silently absorbed:**
//! under the literal shipped `EcologyTuning` — an ordinary ring of races
//! with an ordinary mixed population and no terrain pre-seeding — a Plant
//! is not merely at risk of dying before it gets to propagate; it is
//! *guaranteed* to. `phase_aging`'s hunger/starvation mechanic (S2, predates
//! the Kind split) applies to every entity with no Kind exemption, and a
//! Plant can structurally never eat (`phase_feeding` refuses a `Kind::Plant`
//! as predator, S3.2 §5) — so every Plant's hunger climbs unchecked from
//! birth and crosses the shipped `starve_after` (2 000 ticks) with total
//! certainty, `starve_rate` draining its starting `MAX_HP / 2` away roughly
//! 50 ticks later. See
//! `every_plant_starves_under_the_shipped_table_because_it_can_never_eat`
//! below. This is *why* several tests here pre-seed terrain (satisfying
//! `root_min`/`flee_threshold` immediately) rather than waiting for an
//! ordinary run to build up the same conditions organically — an ordinary
//! run's Plants starve to death first. It also means the runaway-growth
//! risk `crowd_max` exists to bound (§7, §13.4) never has a chance to
//! manifest under the literal shipped defaults: growth requires living long
//! enough to propagate more than once, and the shipped table gives a Plant
//! roughly 2 000 ticks (about six `PropagationTuning::period` cycles) to do
//! so before its guaranteed death. `plant_population_growth_is_bounded_by_
//! crowd_max` below constructs the scenario where growth *can* actually
//! happen (root_min pre-satisfied) to give the mitigation something real to
//! bound, since "under the shipped tables" would otherwise mean "untested,
//! because the precondition for the risk never arises." No `RaceAttrs`,
//! `EcologyTuning`, `BehaviorTuning`, or `PropagationTuning` value is
//! retuned anywhere in this file — every scenario below uses the shipped
//! numbers unmodified; only initial terrain stock and entity placement are
//! constructed, the same category of scenario-isolation
//! `tests/ecology.rs`'s `reproduction_sustains_fire_past_its_own_maximum_
//! lifespan` already establishes precedent for.
//!
//! `tests/succession.rs`'s two `#[ignore]`d 30-day tests were re-run as part
//! of landing this file: **both still fail**, on the identical signature
//! already documented in `README.md`'s "Post-S2 update" note and
//! `docs/S1_TERRAIN_DESIGN.md` §7 (Wood saturates to the grid's maximum
//! within the first climate season and stays there). S3 neither fixes nor
//! worsens it — the failure is a climate/terrain tuning gap, unrelated to
//! the Kind split — so nothing here re-tests that; it is recorded in
//! `README.md`'s S3 section instead.

use pentagram::element::Element;
use pentagram::fx::{Fx, V2};
use pentagram::input::InputLog;
use pentagram::race::{Kind, Race};
use pentagram::{attrs, World};

const SIZE: i32 = 64;
const PER_RACE: u32 = 15;

/// **No mechanism is dead code, part 1: Graze and Hunt.** Both fire within
/// the first few hundred ticks of an ordinary, unmodified mixed-population
/// run — well inside a Plant's ~2 000-tick guaranteed lifetime, so no
/// terrain pre-seeding is needed here the way it is below.
#[test]
fn grazing_and_hunting_occur_under_the_shipped_table() {
    let mut w = World::new(0x53A5, SIZE);
    w.seed_population(PER_RACE);
    let log = InputLog::new();
    for _ in 0..2_000 {
        w.step(&log);
    }
    assert!(w.stats.grazed > 0, "Graze never fired");
    assert!(w.stats.hunted > 0, "Hunt never fired");
}

/// **No mechanism is dead code, part 2: Flee.** The shipped
/// `BehaviorTuning::flee_threshold` and the wiring from `phase_movement`
/// into `behavior::drive` are proven live through a real `World::step`, not
/// just the pure function `behavior.rs`'s own unit tests already exercise.
/// Terrain is pre-seeded above the threshold so the check does not depend on
/// an ordinary run's terrain ever reaching it organically.
#[test]
fn flee_fires_when_danger_crosses_the_shipped_threshold() {
    let mut w = World::new(0x53A9, SIZE);
    let danger_el = Element::Wood.eaten_by();
    for x in 28..36 {
        for y in 28..36 {
            w.terrain.cell_mut(x, y)[danger_el] = u16::MAX;
        }
    }
    w.spawn(Race { element: Element::Wood, kind: Kind::Animal }, V2::new(Fx::from_int(32), Fx::from_int(32)));
    let log = InputLog::new();
    for _ in 0..20 {
        w.step(&log);
    }
    assert!(w.stats.fled > 0, "Flee never fired despite danger above the shipped flee_threshold");
}

/// **No mechanism is dead code, part 3: propagation, raced against
/// guaranteed starvation.** `root_min` is pre-satisfied grid-wide (an
/// ordinary run's terrain would not reach it in time — see this file's own
/// header note) so a real propagation attempt, at the shipped `chance` and
/// `period`, gets to run against a population large enough that at least one
/// success is overwhelming likely inside the ~2 000-tick window every Plant
/// actually has before `starve_after` kills it.
#[test]
fn at_least_one_plant_roots_before_the_shipped_table_starves_it() {
    let mut w = World::new(0x53AA, SIZE);
    for x in 0..SIZE {
        for y in 0..SIZE {
            w.terrain.cell_mut(x, y)[Element::Wood.habitat()] = 2_000;
        }
    }
    for k in 0..50u32 {
        let p = V2::new(Fx::from_int((k * 7 % 60) as i32), Fx::from_int((k * 13 % 60) as i32));
        w.spawn(Race { element: Element::Wood, kind: Kind::Plant }, p);
    }
    let log = InputLog::new();
    for _ in 0..2_000 {
        w.step(&log);
    }
    assert!(w.stats.propagated > 0, "no Plant ever successfully rooted before the shipped table's guaranteed starvation window closed");
}

/// **The interaction this file's header describes, pinned down as a
/// permanent regression rather than left as a one-off observation.** A
/// Plant can never eat (`phase_feeding` refuses it as predator), yet
/// `phase_aging`'s hunger/starvation mechanic has no Kind exemption — so
/// every Plant's hunger crosses the shipped `starve_after` with certainty,
/// and it dies of starvation roughly `starve_after + (MAX_HP / 2) /
/// starve_rate` ticks after birth (`Entity::spawn` starts every body at
/// half `MAX_HP`, not full), regardless of terrain, predation, or anything
/// else.
/// If a future retune ever exempts Plants from starvation (or gives them
/// some other way to reset `hunger`), this test is expected to start
/// failing — that would be the fix landing, not a regression.
#[test]
fn every_plant_starves_under_the_shipped_table_because_it_can_never_eat() {
    let mut w = World::new(0x53AB, SIZE);
    w.spawn(Race { element: Element::Wood, kind: Kind::Plant }, V2::new(Fx::from_int(32), Fx::from_int(32)));
    let log = InputLog::new();
    for _ in 0..2_500 {
        w.step(&log);
    }
    assert_eq!(w.alive_count(), 0, "an uneaten, unpropagated Plant should have starved by now");
    assert_eq!(w.stats.starved, 1, "the death should be attributed to starvation, not old age");
}

/// **The runaway-risk empirical check named in design-doc §7 and §13.4,**
/// given the one scenario (root_min pre-satisfied) where the risk actually
/// has a chance to manifest — see this file's header note for why an
/// ordinary run does not reach it. The ceiling is derived from the live
/// `PropagationTuning` table and the actual grid size, not hardcoded, so a
/// live retune of `crowd_max` keeps this test honest rather than silently
/// stale. Honestly scoped: `max_plants > initial_plants` proves propagation
/// is not dead code even under real crowding pressure, and `max_plants <=
/// ceiling` catches an actual runaway explosion — but the run (confirmed by
/// hand: ~3 000 plants at the full 20 000 ticks below, still climbing, well
/// under the ~12 000 ceiling) never gets close enough to that ceiling to be
/// a tight test of `crowd_max`'s exact per-cell arithmetic; a boundary bug
/// there (an off-by-one, say) would not necessarily be caught here.
///
/// `#[ignore]`d for the same reason `tests/succession.rs`'s two 30-day tests
/// are: this is the one test in this file where the population actually
/// grows into the thousands, and `phase_collisions`' broadphase-free O(n²)
/// pairwise check (`docs/S1_TERRAIN_DESIGN.md`'s "not yet landed" note)
/// makes that expensive — tens of seconds, against low hundreds of
/// milliseconds for everything else in this file. Run explicitly via
/// `cargo test --release -- --ignored`.
#[test]
#[ignore]
fn plant_population_growth_is_bounded_by_crowd_max() {
    let mut w = World::new(0x53AA, SIZE);
    for x in 0..SIZE {
        for y in 0..SIZE {
            w.terrain.cell_mut(x, y)[Element::Wood.habitat()] = 2_000;
        }
    }
    for k in 0..50u32 {
        let p = V2::new(Fx::from_int((k * 7 % 60) as i32), Fx::from_int((k * 13 % 60) as i32));
        w.spawn(Race { element: Element::Wood, kind: Kind::Plant }, p);
    }
    let log = InputLog::new();
    let initial_plants = w.entities.iter().filter(|e| e.kind == Kind::Plant).count();
    let cells = (w.terrain.side as usize) * (w.terrain.side as usize);
    let ceiling = w.propagation.crowd_max[Element::Wood] as usize * cells;

    let mut max_plants = initial_plants;
    for _ in 0..20_000 {
        w.step(&log);
        let n = w.entities.iter().filter(|e| e.alive && e.kind == Kind::Plant).count();
        max_plants = max_plants.max(n);
    }

    assert!(max_plants > initial_plants, "plant population never grew past its seeded start — propagation looks dead, not merely capped");
    assert!(
        max_plants <= ceiling,
        "plant population ({max_plants}) exceeded the crowd_max-derived ceiling ({ceiling}) \u{2014} the runaway-risk mitigation failed to bound growth"
    );
}

/// **A structural invariant, checked every tick, not just at the end.**
/// `a_plant_never_moves` (`world.rs`) already pins down "immobility is a
/// structural skip" (design doc §4) for a single isolated scenario;
/// `phase_collisions` is deliberately *not* skipped for a Plant (a thicket
/// still crowds neighbours, same design doc section), so a real
/// mixed-population run legitimately can displace one by collision. This
/// test proves the narrower, real claim continuously: whenever a Plant has
/// nothing within collision range to push it, it never moves on its own —
/// including a Plant born mid-run by `phase_flora`, which the narrower
/// scenario cannot see.
#[test]
fn a_rooted_plant_never_self_propels_across_a_long_run() {
    let mut w = World::new(0x53A6, SIZE);
    w.seed_population(PER_RACE);
    let log = InputLog::new();
    let mut any_plant_seen = false;
    let mut checked_isolated = false;
    for _ in 0..10_000 {
        // Snapshot *before* stepping: `phase_collisions` resolves overlaps
        // using each body's position entering the phase (this tick's
        // post-movement, pre-collision position -- unchanged from this
        // snapshot for a Plant, which skips movement entirely, and bounded
        // by `speed` for an Animal), and `phase_reap` removes anything that
        // died later in the very same tick -- a post-`step` snapshot would
        // silently lose a pusher that collided with a Plant and was then
        // eaten before the tick ended.
        let prev = w.entities.clone();
        w.step(&log);
        for p in &prev {
            if !p.alive || p.kind != Kind::Plant {
                continue;
            }
            any_plant_seen = true;
            let r_p = attrs(p.race()).radius;
            // Padded by `o`'s own speed (how far it can travel this tick
            // under `phase_movement`'s step, upper-bounding wherever it
            // actually was entering `phase_collisions`) plus a
            // `pentagram::world::JITTER`-scale margin for the per-tick
            // positional jitter every mover also picks up.
            let crowded = prev.iter().any(|o| {
                o.id != p.id && {
                    let r_o = attrs(o.race()).radius;
                    let reach = r_p + r_o + attrs(o.race()).speed + pentagram::world::JITTER + pentagram::world::JITTER;
                    (o.pos - p.pos).len_sq() < reach * reach
                }
            });
            if !crowded {
                checked_isolated = true;
                if let Some(now) = w.entities.iter().find(|e| e.id == p.id) {
                    assert_eq!(now.pos, p.pos, "isolated Plant {} moved from {:?} to {:?} with nothing nearby to push it", p.id, p.pos, now.pos);
                }
            }
        }
    }
    assert!(any_plant_seen, "no Plant was ever alive to check");
    assert!(checked_isolated, "no Plant was ever isolated enough to check the no-self-propulsion claim");
}

/// **The S0 exit condition, re-run now that every S3 phase is wired into
/// the tick.** 10 000 ticks from an ordinary mixed-population start still
/// replay bit-for-bit identically with `phase_flora` and the animal FSM
/// both live in the loop — not just with an empty world
/// (`tests/determinism.rs`'s `the_world_keeps_churning_with_no_players_at_
/// all`) or a pre-Kind-split population. This does not additionally assert
/// every `Stats` counter fired — this file's header note explains why an
/// ordinary run cannot promise that inside 10 000 ticks; that liveness
/// claim is what the scenario-isolated tests above are for.
#[test]
fn ten_thousand_ticks_replay_bit_identically_with_every_s3_phase_active() {
    let mut a = World::new(0x53A8, SIZE);
    a.seed_population(PER_RACE);
    let mut b = a.clone();
    let log = InputLog::new();
    for _ in 0..10_000 {
        a.step(&log);
        b.step(&log);
        assert_eq!(a.state_hash(), b.state_hash(), "two independent runs from the same seed diverged with every S3 phase active");
    }
}
