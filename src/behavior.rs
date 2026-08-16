//! S3.4 — the animal FSM: Flee, Hunt, Graze.
//!
//! Every Animal body's steering intent is *derived* fresh each tick from
//! `(hunger, sensed neighbourhood, terrain)` — never stored on `Entity` — so
//! there is no persistent FSM-state field to keep in sync with a world that
//! has moved on since it was last written.
//!
//! Fixed priority, always: **Flee > Hunt > Graze**.
//!
//! - **Flee** reuses `ecology::apply_attrition`'s own danger signal — the
//!   terrain concentration of `element.eaten_by()` at the body's current
//!   cell — rather than inventing a second one. Above `flee_threshold[race]`,
//!   the body steers away from whichever of its four grid neighbours (fixed
//!   N/E/S/W visiting order, so a tie breaks structurally rather than by
//!   iteration accident) holds the most of that same element.
//! - **Hunt** gates on the *same* `hunger >= ecology.satiation[element]` test
//!   `World::phase_feeding` already uses, plus a prey body sensed within
//!   `sense_radius[race]` — deliberately larger than `EcologyTuning::
//!   forage_radius`, since sensing at a distance and catching within bite
//!   range are different things. A candidate counts as prey if it's a
//!   same-element `Kind::Plant` (`element.eats_plant()`, grazing) or a
//!   ring-adjacent `Kind::Animal` (`element.eats_animal()`, hunting) — the
//!   same Kind-aware split `World::phase_feeding` pairs on, so sensing and
//!   catching never disagree about who's valid prey. Nearest by `len_sq()`
//!   wins; ties break by lowest id (Invariant IV). Which relation matched is
//!   not reported back — whether a caught body ends up grazed or
//!   hunt-weight-gated is entirely `World::phase_feeding`'s decision,
//!   downstream of and unaware of this module.
//! - **Graze** is the default: no danger above threshold, and either not
//!   hungry or no prey sensed. No steering — today's unmodified wander.
//!
//! Steering itself (`steer`) is a bounded per-tick turn toward the desired
//! heading, never a snap — Invariant I's bounded-propagation discipline,
//! applied to headings the way diffusion caps apply it to terrain.
//!
//! `drive`'s signature adds one parameter beyond the design doc's own
//! illustrative sketch (§6): an `&EcologyTuning` is required to read
//! `satiation`, the same gate `phase_feeding` already uses — the doc's
//! pseudocode elided this as `{ ... }`, not as a real omission.
//!
//! See `docs/S3_ECOLOGY_LAYERS_DESIGN.md` §6 for the full design account.

use crate::ecology::EcologyTuning;
use crate::entity::Entity;
use crate::fx::{Fx, V2};
use crate::hash::{Hashable, Hasher};
use crate::race::{Kind, PerRace, Race};
use crate::terrain::Terrain;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Drive {
    Graze,
    Hunt,
    Flee,
}

/// Fixed visiting order for Flee's neighbour scan — ties break structurally
/// (first in this order to hold the max wins) rather than by iteration
/// accident. The labels are a stable, documented order, not a claim about
/// real compass geometry.
const NEIGHBOUR_OFFSETS: [(i32, i32); 4] = [(0, -1), (1, 0), (0, 1), (-1, 0)]; // N, E, S, W

/// First-guess defaults — "a starting point for the live tuning loop, not a
/// derived constant," the same spirit `race.rs`/`ecology.rs` state of their
/// own tables. `SENSE_RADIUS_DEFAULT` is deliberately larger than
/// `EcologyTuning`'s default `forage_radius` (4.0 cells) — sensing at a
/// distance and catching within bite range are different things.
const FLEE_THRESHOLD_DEFAULT: u16 = 8_000;
const SENSE_RADIUS_DEFAULT: Fx = Fx::ratio(800, 100); // 8.0 cells
const TURN_RATE_DEFAULT: Fx = Fx::ratio(150, 1000); // 15% blend toward the desired heading per tick

/// The rate/reach knobs `behavior::drive` and `World::phase_movement` read.
/// `PerRace`-shaped like `EcologyTuning::hunt_weight`, and for the same
/// reason: Plant rows are unread (`World::phase_movement` never calls
/// `drive` for a `Kind::Plant` body — it skips them structurally, S3.2) —
/// their entries exist only so `PerRace` stays uniformly 10-wide, not
/// because anything consults them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BehaviorTuning {
    /// Terrain concentration of `element.eaten_by()`, at the body's own
    /// cell, above which it flees. Same raw stock scale `EcologyTuning::
    /// attrition_rate` reads (`terrain.cell(x, y)[element]`, up to
    /// `u16::MAX`), not a per-mille fraction.
    pub flee_threshold: PerRace<u16>,
    /// How far a body can sense prey, in cells — separate from and larger
    /// than `EcologyTuning::forage_radius` (the *catching* range).
    pub sense_radius: PerRace<Fx>,
    /// Per-tick steering blend toward the desired heading, in `Fx`'s [0, 1]
    /// range (an `Fx::ratio(_, 1000)`-style fraction) — 0 never turns, 1
    /// snaps instantly. Bounded turning, not a snap, is the whole point.
    pub turn_rate: PerRace<Fx>,
}

impl Default for BehaviorTuning {
    fn default() -> BehaviorTuning {
        let mut flee_threshold = PerRace::filled(0u16);
        let mut sense_radius = PerRace::filled(Fx::ZERO);
        let mut turn_rate = PerRace::filled(Fx::ZERO);
        for r in Race::ALL {
            if r.kind == Kind::Animal {
                *flee_threshold.get_mut(r) = FLEE_THRESHOLD_DEFAULT;
                *sense_radius.get_mut(r) = SENSE_RADIUS_DEFAULT;
                *turn_rate.get_mut(r) = TURN_RATE_DEFAULT;
            }
        }
        BehaviorTuning { flee_threshold, sense_radius, turn_rate }
    }
}

impl Hashable for BehaviorTuning {
    fn hash_into(&self, h: &mut Hasher) {
        for (_, v) in self.flee_threshold.iter() {
            h.u16(*v);
        }
        for (_, v) in self.sense_radius.iter() {
            h.i32(v.raw());
        }
        for (_, v) in self.turn_rate.iter() {
            h.i32(v.raw());
        }
    }
}

/// Pure — testable against a bare `&[Entity]` + `&Terrain` (+ a `SpatialIndex`
/// built from that same pair) with no `World` involved, the same shape
/// `ecology::apply_attrition` already uses. Caller (`World::phase_movement`)
/// must only call this for an alive `Kind::Animal` entity at `i`; that
/// contract is asserted, not silently assumed. `index` is a caller-owned
/// broadphase, not built here, so `phase_movement` can build it once and
/// share it across every Animal's Hunt scan this tick rather than paying an
/// `O(n)` build per candidate — see `SpatialIndex`'s own doc comment for why
/// it can't just be cached across ticks.
pub fn drive(
    entities: &[Entity],
    terrain: &Terrain,
    index: &crate::terrain::SpatialIndex,
    ecology: &EcologyTuning,
    tuning: &BehaviorTuning,
    i: usize,
) -> (Drive, Option<V2>) {
    let e = &entities[i];
    debug_assert!(e.alive && e.kind == Kind::Animal, "drive() called for a non-Animal or dead entity");
    let race = e.race();

    // Flee — highest priority. Reuses apply_attrition's own danger signal.
    let (x, y) = terrain.cell_of(e.pos);
    let danger_el = e.element.eaten_by();
    let here = terrain.cell(x, y)[danger_el];
    if here > tuning.flee_threshold[race] {
        let mut best: Option<(V2, u16)> = None;
        for (dx, dy) in NEIGHBOUR_OFFSETS {
            let val = terrain.cell(x + dx, y + dy)[danger_el];
            let better = match best {
                None => true,
                Some((_, b)) => val > b,
            };
            if better {
                best = Some((V2::new(Fx::from_int(dx), Fx::from_int(dy)), val));
            }
        }
        if let Some((dir, _)) = best {
            // `dir` is always a unit-length grid offset (one axis is ±1, the
            // other 0) — never the zero vector — so this never hits V2::
            // normalized's own zero-length guard.
            return (Drive::Flee, Some((-dir).normalized()));
        }
    }

    // Hunt — the same satiation gate phase_feeding already uses, plus a
    // sensed prey body within reach. Sensing now does care about the
    // candidate's Kind, in order to pick the right relation per Kind: a
    // same-element Plant (eats_plant, grazing) or a ring-adjacent Animal
    // (eats_animal, hunting).
    if e.hunger >= ecology.satiation[e.element] {
        let plant_prey_el = e.element.eats_plant();
        let animal_prey_el = e.element.eats_animal();
        let reach = tuning.sense_radius[race];
        let reach_sq = reach * reach;
        let (cx, cy) = terrain.cell_of(e.pos);
        let search_r = crate::terrain::SpatialIndex::radius_cells(reach);
        let mut nearest: Option<(usize, Fx)> = None;
        for j in index.query_ring(cx, cy, search_r) {
            let j = j as usize;
            let other = &entities[j];
            if j == i || !other.alive {
                continue;
            }
            let is_prey = (other.kind == Kind::Plant && other.element == plant_prey_el)
                || (other.kind == Kind::Animal && other.element == animal_prey_el);
            if !is_prey {
                continue;
            }
            let d = other.pos - e.pos;
            let dsq = d.len_sq();
            if dsq > reach_sq {
                continue;
            }
            let take = match nearest {
                None => true,
                Some((bj, bd)) => dsq < bd || (dsq == bd && other.id < entities[bj].id),
            };
            if take {
                nearest = Some((j, dsq));
            }
        }
        if let Some((j, dsq)) = nearest {
            // Co-located (dsq == 0): no meaningful direction to steer in —
            // report the drive without a steering target rather than
            // returning V2::ZERO through normalized() and perturbing the
            // heading's fixed-point value for no directional reason.
            let target = if dsq.is_zero() { None } else { Some((entities[j].pos - e.pos).normalized()) };
            return (Drive::Hunt, target);
        }
    }

    (Drive::Graze, None)
}

/// Bounded per-tick turn toward `desired`, never a snap — Invariant I's
/// bounded-propagation discipline applied to steering the way diffusion caps
/// apply it to terrain. `turn` is clamped to [0, 1] defensively (a live-tuned
/// knob could otherwise push it out of range). The exactly-opposite-heading
/// degenerate case (desired ≈ -heading at turn ≈ 0.5, which can blend to the
/// zero vector) falls back to the unchanged `heading` — the same shape
/// `entity::initial_heading`'s own zero-length guard already uses.
pub fn steer(heading: V2, desired: V2, turn: Fx) -> V2 {
    let turn = turn.clamp(Fx::ZERO, Fx::ONE);
    let blended = heading.scale(Fx::ONE - turn) + desired.scale(turn);
    if blended.len_sq().is_zero() {
        heading
    } else {
        blended.normalized()
    }
}
