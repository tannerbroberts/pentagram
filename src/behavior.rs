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

/// Pure — testable against a bare `&[Entity]` + `&Terrain` with no `World`
/// involved, the same shape `ecology::apply_attrition` already uses. Caller
/// (`World::phase_movement`) must only call this for an alive
/// `Kind::Animal` entity at `i`; that contract is asserted, not silently
/// assumed.
pub fn drive(entities: &[Entity], terrain: &Terrain, ecology: &EcologyTuning, tuning: &BehaviorTuning, i: usize) -> (Drive, Option<V2>) {
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
        let mut nearest: Option<(usize, Fx)> = None;
        for (j, other) in entities.iter().enumerate() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::element::Element;

    fn spawn_at(element: Element, pos: V2) -> Entity {
        let race = Race { element, kind: Kind::Animal };
        let a = crate::race::attrs(race);
        Entity::spawn(1, element, pos, 5, 0, a)
    }

    fn spawn_id_at(id: u32, element: Element, pos: V2) -> Entity {
        let race = Race { element, kind: Kind::Animal };
        let a = crate::race::attrs(race);
        Entity::spawn(id, element, pos, 5, 0, a)
    }

    #[test]
    fn flee_wins_over_hunt_even_with_hungry_predator_and_sensed_prey() {
        // Wood.eaten_by() == Fire, Wood.eats() == Water (ring order).
        let mut terrain = Terrain::new(8);
        terrain.cell_mut(2, 2)[Element::Fire] = u16::MAX; // way above flee_threshold
        let mut wood = spawn_at(Element::Wood, V2::new(Fx::from_int(2), Fx::from_int(2)));
        wood.hunger = u32::MAX; // definitely satiation-gated hungry
        let water = spawn_id_at(2, Element::Water, V2::new(Fx::from_int(2), Fx::from_int(2)) + V2::new(Fx::from_int(1), Fx::ZERO));
        let entities = vec![wood, water];
        let ecology = EcologyTuning::default();
        let tuning = BehaviorTuning::default();
        let (d, _) = drive(&entities, &terrain, &ecology, &tuning, 0);
        assert_eq!(d, Drive::Flee, "danger above threshold must win over an available hunt");
    }

    #[test]
    fn flee_neighbour_tie_break_picks_the_first_in_n_e_s_w_order() {
        // All four neighbours tied at the same (max) concentration: N should
        // win structurally, and the body should steer directly away from it
        // (i.e. south).
        let mut terrain = Terrain::new(8);
        let danger_el = Element::Wood.eaten_by(); // Fire
        let cx = 4;
        let cy = 4;
        terrain.cell_mut(cx, cy)[danger_el] = 9_000; // above default threshold
        for (dx, dy) in NEIGHBOUR_OFFSETS {
            terrain.cell_mut(cx + dx, cy + dy)[danger_el] = 5_000;
        }
        let wood = spawn_at(Element::Wood, V2::new(Fx::from_int(cx), Fx::from_int(cy)));
        let entities = vec![wood];
        let ecology = EcologyTuning::default();
        let tuning = BehaviorTuning::default();
        let (d, dir) = drive(&entities, &terrain, &ecology, &tuning, 0);
        assert_eq!(d, Drive::Flee);
        let dir = dir.expect("flee should always produce a direction here");
        // N is (0,-1); fleeing away from N means steering toward (0, 1) = south.
        assert_eq!(dir, V2::new(Fx::ZERO, Fx::ONE), "should flee directly away from the N neighbour, got {:?}", dir);
    }

    #[test]
    fn hunt_requires_the_satiation_gate_hungry_with_no_prey_sensed_grazes() {
        let terrain = Terrain::new(8);
        let mut wood = spawn_at(Element::Wood, V2::new(Fx::from_int(4), Fx::from_int(4)));
        wood.hunger = u32::MAX;
        let entities = vec![wood];
        let ecology = EcologyTuning::default();
        let tuning = BehaviorTuning::default();
        let (d, target) = drive(&entities, &terrain, &ecology, &tuning, 0);
        assert_eq!(d, Drive::Graze);
        assert_eq!(target, None);
    }

    #[test]
    fn hunt_requires_the_satiation_gate_prey_sensed_but_not_hungry_grazes() {
        let terrain = Terrain::new(8);
        let wood = spawn_at(Element::Wood, V2::new(Fx::from_int(4), Fx::from_int(4))); // hunger = 0
        let water = spawn_id_at(2, Element::Water, V2::new(Fx::from_int(5), Fx::from_int(4)));
        let entities = vec![wood, water];
        let ecology = EcologyTuning::default();
        let tuning = BehaviorTuning::default();
        let (d, target) = drive(&entities, &terrain, &ecology, &tuning, 0);
        assert_eq!(d, Drive::Graze);
        assert_eq!(target, None);
    }

    #[test]
    fn hunt_picks_nearest_prey_by_len_sq_with_id_tie_break() {
        let terrain = Terrain::new(16);
        let mut wood = spawn_at(Element::Wood, V2::new(Fx::from_int(8), Fx::from_int(8)));
        wood.hunger = u32::MAX;
        // Two equidistant Water prey (Wood eats Water), tie broken by lowest id.
        let far_water = spawn_id_at(5, Element::Water, V2::new(Fx::from_int(8), Fx::from_int(2)));
        let near_low_id = spawn_id_at(2, Element::Water, V2::new(Fx::from_int(8), Fx::from_int(6)));
        let near_high_id = spawn_id_at(3, Element::Water, V2::new(Fx::from_int(6), Fx::from_int(8)));
        let entities = vec![wood, far_water, near_low_id, near_high_id];
        let ecology = EcologyTuning::default();
        let tuning = BehaviorTuning::default();
        let (d, target) = drive(&entities, &terrain, &ecology, &tuning, 0);
        assert_eq!(d, Drive::Hunt);
        // Steering toward (8,6) from (8,8) is straight up: (0, -1).
        let target = target.expect("nearer prey should produce a direction");
        assert_eq!(target, V2::new(Fx::ZERO, -Fx::ONE), "should steer toward the lower-id equidistant prey, got {:?}", target);
    }

    #[test]
    fn hunt_against_co_located_prey_returns_hunt_with_no_direction() {
        let terrain = Terrain::new(8);
        let mut wood = spawn_at(Element::Wood, V2::new(Fx::from_int(4), Fx::from_int(4)));
        wood.hunger = u32::MAX;
        let water = spawn_id_at(2, Element::Water, V2::new(Fx::from_int(4), Fx::from_int(4)));
        let entities = vec![wood, water];
        let ecology = EcologyTuning::default();
        let tuning = BehaviorTuning::default();
        let (d, target) = drive(&entities, &terrain, &ecology, &tuning, 0);
        assert_eq!(d, Drive::Hunt);
        assert_eq!(target, None, "co-located prey has no meaningful direction");
    }

    #[test]
    fn graze_is_the_default_with_nothing_sensed_and_no_danger() {
        let terrain = Terrain::new(8);
        let wood = spawn_at(Element::Wood, V2::new(Fx::from_int(4), Fx::from_int(4)));
        let entities = vec![wood];
        let ecology = EcologyTuning::default();
        let tuning = BehaviorTuning::default();
        let (d, target) = drive(&entities, &terrain, &ecology, &tuning, 0);
        assert_eq!(d, Drive::Graze);
        assert_eq!(target, None);
    }

    #[test]
    fn steer_bounds_the_turn_rather_than_snapping() {
        let heading = V2::new(Fx::ONE, Fx::ZERO);
        let desired = V2::new(Fx::ZERO, Fx::ONE);
        let turn = Fx::ratio(1, 4); // 25%
        let out = steer(heading, desired, turn);
        assert_ne!(out, desired, "a bounded turn must not snap straight to the desired heading");
        assert_ne!(out, heading, "a nonzero turn must move away from the original heading");
        // Both components should be strictly between 0 and 1 for a partial turn.
        assert!(out.x > Fx::ZERO && out.x < Fx::ONE, "{:?}", out);
        assert!(out.y > Fx::ZERO && out.y < Fx::ONE, "{:?}", out);
    }

    #[test]
    fn steer_falls_back_to_heading_on_the_exactly_opposite_degenerate_case() {
        let heading = V2::new(Fx::ONE, Fx::ZERO);
        let desired = V2::new(-Fx::ONE, Fx::ZERO);
        let out = steer(heading, desired, Fx::HALF);
        assert_eq!(out, heading, "the exactly-opposite + turn=0.5 case must fall back to the unchanged heading");
    }

    #[test]
    fn hash_notices_every_field() {
        let a = BehaviorTuning::default();
        let base = {
            let mut h = Hasher::new();
            a.hash_into(&mut h);
            h.finish()
        };

        let animal_wood = Race { element: Element::Wood, kind: Kind::Animal };

        let mut flee_threshold = a;
        flee_threshold.flee_threshold[animal_wood] += 1;
        let mut sense_radius = a;
        sense_radius.sense_radius[animal_wood] = sense_radius.sense_radius[animal_wood] + Fx::ONE;
        let mut turn_rate = a;
        turn_rate.turn_rate[animal_wood] = turn_rate.turn_rate[animal_wood] + Fx::ONE;

        for (name, tuning) in [
            ("flee_threshold", flee_threshold),
            ("sense_radius", sense_radius),
            ("turn_rate", turn_rate),
        ] {
            let mut h = Hasher::new();
            tuning.hash_into(&mut h);
            assert_ne!(h.finish(), base, "{name} does not affect the hash");
        }
    }

    #[test]
    fn defaults_are_nonzero_for_animals_and_zero_for_plants() {
        let t = BehaviorTuning::default();
        for r in Race::ALL {
            match r.kind {
                Kind::Animal => {
                    assert!(t.flee_threshold[r] > 0, "{}-Animal flee_threshold is zero", r.element.name());
                    assert!(t.sense_radius[r] > Fx::ZERO, "{}-Animal sense_radius is zero", r.element.name());
                    assert!(t.turn_rate[r] > Fx::ZERO, "{}-Animal turn_rate is zero", r.element.name());
                }
                Kind::Plant => {
                    assert_eq!(t.flee_threshold[r], 0, "{}-Plant flee_threshold should stay zero", r.element.name());
                    assert_eq!(t.sense_radius[r], Fx::ZERO, "{}-Plant sense_radius should stay zero", r.element.name());
                    assert_eq!(t.turn_rate[r], Fx::ZERO, "{}-Plant turn_rate should stay zero", r.element.name());
                }
            }
        }
    }
}
