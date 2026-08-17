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
//!   the body steers toward whichever of its 8 grid neighbours (fixed
//!   `tile::NEIGHBOURS_8` visiting order, so a tie breaks structurally rather
//!   than by iteration accident) holds the *least* of that same element.
//! - **Hunt** gates on the *same* `hunger >= ecology.satiation[element]` test
//!   `World::phase_feeding` already uses, plus a prey body sensed within
//!   `sense_radius[race]` tiles (Chebyshev) — deliberately larger than
//!   `EcologyTuning::forage_radius`, since sensing at a distance and catching
//!   within bite range are different things. A candidate counts as prey if
//!   it's a same-element `Kind::Plant` (`element.eats_plant()`, grazing) or a
//!   ring-adjacent `Kind::Animal` (`element.eats_animal()`, hunting) — the
//!   same Kind-aware split `World::phase_feeding` pairs on, so sensing and
//!   catching never disagree about who's valid prey. Nearest by Chebyshev
//!   distance wins; ties break by lowest id (Invariant IV). Which relation
//!   matched is not reported back — whether a caught body ends up grazed or
//!   hunt-weight-gated is entirely `World::phase_feeding`'s decision,
//!   downstream of and unaware of this module.
//! - **Graze** is the default: no danger above threshold, and either not
//!   hungry or no prey sensed. Steps toward `Entity.move_target` if one is
//!   committed (`CmdKind::SetTarget`); otherwise re-rolls a uniformly random
//!   one of the 8 neighbours (or "stay put") every eligible tick — the
//!   discrete equivalent of the old continuous wander, using the same
//!   `Channel::Wander` `entity::initial_facing` already draws from.
//!
//! `ai_decide_move()` returns a concrete destination `Tile` to step toward,
//! not a direction to blend — movement is discrete tile-hopping
//! (`World::phase_movement`), so there is no continuous heading to steer.
//!
//! `ai_decide_move`'s signature adds one parameter beyond the design doc's
//! own illustrative sketch (§6): an `&EcologyTuning` is required to read
//! `satiation`, the same gate `phase_feeding` already uses — the doc's
//! pseudocode elided this as `{ ... }`, not as a real omission.
//!
//! **Player/AI symmetry.** This function is one half of a pair —
//! `player_decide_move` (below) is the other. `World.player_id` names at
//! most one entity whose movement this tick comes from the player's own
//! `Entity.move_target` instead of this FSM; every other entity's movement
//! decision is this function, unchanged. Both halves return the identical
//! `Option<Tile>` shape `World::phase_movement`'s decide→resolve→apply
//! pipeline consumes — the asymmetry is entirely in *who decides*, never in
//! how the decision is resolved once made. See that function's own doc
//! comment for why it does not fall back to a random wander the way Graze
//! does: a player who has not committed a goal is standing still on
//! purpose, not idling.
//!
//! See `docs/S3_ECOLOGY_LAYERS_DESIGN.md` §6 for the full design account.

use crate::ecology::EcologyTuning;
use crate::entity::Entity;
use crate::hash::{Hashable, Hasher};
use crate::race::{Kind, PerRace, Race};
use crate::rand::{rand_below, Channel};
use crate::terrain::Terrain;
use crate::tile::{chebyshev_dist, step_toward, Tile, NEIGHBOURS_8};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Drive {
    Graze,
    Hunt,
    Flee,
}

/// First-guess defaults — "a starting point for the live tuning loop, not a
/// derived constant," the same spirit `race.rs`/`ecology.rs` state of their
/// own tables. `SENSE_RADIUS_DEFAULT` is deliberately larger than
/// `EcologyTuning`'s default `forage_radius` (4 tiles) — sensing at a
/// distance and catching within bite range are different things.
const FLEE_THRESHOLD_DEFAULT: u16 = 8_000;
const SENSE_RADIUS_DEFAULT: i32 = 8;

/// The rate/reach knobs `behavior::ai_decide_move` and `World::phase_movement`
/// read. `PerRace`-shaped like `EcologyTuning::hunt_weight`, and for the same
/// reason: Plant rows are unread (`World::phase_movement` never calls
/// `ai_decide_move` for a `Kind::Plant` body — it skips them structurally,
/// S3.2) — their entries exist only so `PerRace` stays uniformly 10-wide, not
/// because anything consults them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BehaviorTuning {
    /// Terrain concentration of `element.eaten_by()`, at the body's own
    /// cell, above which it flees. Same raw stock scale `EcologyTuning::
    /// attrition_rate` reads (`terrain.cell(x, y)[element]`, up to
    /// `u16::MAX`), not a per-mille fraction.
    pub flee_threshold: PerRace<u16>,
    /// How far a body can sense prey, in tiles (Chebyshev distance) —
    /// separate from and larger than `EcologyTuning::forage_radius` (the
    /// *catching* range).
    pub sense_radius: PerRace<i32>,
}

impl Default for BehaviorTuning {
    fn default() -> BehaviorTuning {
        let mut flee_threshold = PerRace::filled(0u16);
        let mut sense_radius = PerRace::filled(0i32);
        for r in Race::ALL {
            if r.kind == Kind::Animal {
                *flee_threshold.get_mut(r) = FLEE_THRESHOLD_DEFAULT;
                *sense_radius.get_mut(r) = SENSE_RADIUS_DEFAULT;
            }
        }
        BehaviorTuning { flee_threshold, sense_radius }
    }
}

impl Hashable for BehaviorTuning {
    fn hash_into(&self, h: &mut Hasher) {
        for (_, v) in self.flee_threshold.iter() {
            h.u16(*v);
        }
        for (_, v) in self.sense_radius.iter() {
            h.i32(*v);
        }
    }
}

/// Pure — testable against a bare `&[Entity]` + `&Terrain` (+ a `SpatialIndex`
/// built from that same pair) with no `World` involved, the same shape
/// `ecology::apply_attrition` already uses. Caller (`World::phase_movement`)
/// must only call this for an alive `Kind::Animal` entity at `i` that is
/// *not* `World.player_id` — that contract is asserted, not silently
/// assumed. `index` is a caller-owned broadphase, not built here, so
/// `phase_movement` can build it once and share it across every Animal's
/// Hunt scan this tick rather than paying an `O(n)` build per candidate —
/// see `SpatialIndex`'s own doc comment for why it can't just be cached
/// across ticks. `seed`/`tick` are needed only for Graze's random-wander
/// roll. The "AI" half of the player/AI symmetry this module's own doc
/// comment describes — see `player_decide_move` for the other half.
#[allow(clippy::too_many_arguments)]
pub fn ai_decide_move(
    entities: &[Entity],
    terrain: &Terrain,
    index: &crate::terrain::SpatialIndex,
    ecology: &EcologyTuning,
    tuning: &BehaviorTuning,
    seed: u64,
    tick: u64,
    i: usize,
) -> (Drive, Option<Tile>) {
    let e = &entities[i];
    debug_assert!(e.alive && e.kind == Kind::Animal, "ai_decide_move() called for a non-Animal or dead entity");
    let race = e.race();

    // Flee — highest priority. Reuses apply_attrition's own danger signal.
    let danger_el = e.element.eaten_by();
    let here = terrain.cell(e.pos.x, e.pos.y)[danger_el];
    if here > tuning.flee_threshold[race] {
        let mut best: Option<(Tile, u16)> = None;
        for (dx, dy) in NEIGHBOURS_8 {
            let n = e.pos.offset(dx, dy);
            let val = terrain.cell(n.x, n.y)[danger_el];
            let better = match best {
                None => true,
                Some((_, b)) => val < b,
            };
            if better {
                best = Some((n, val));
            }
        }
        if let Some((n, _)) = best {
            return (Drive::Flee, Some(n));
        }
    }

    // Hunt — the same satiation gate phase_feeding already uses, plus a
    // sensed prey body within reach. Sensing does care about the
    // candidate's Kind, in order to pick the right relation per Kind: a
    // same-element Plant (eats_plant, grazing) or a ring-adjacent Animal
    // (eats_animal, hunting).
    if e.hunger >= ecology.satiation[e.element] {
        let plant_prey_el = e.element.eats_plant();
        let animal_prey_el = e.element.eats_animal();
        let reach = tuning.sense_radius[race];
        let mut nearest: Option<(usize, i32)> = None;
        for j in index.query_ring(e.pos.x, e.pos.y, reach) {
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
            let d = chebyshev_dist(e.pos, other.pos);
            if d > reach {
                continue;
            }
            let take = match nearest {
                None => true,
                Some((bj, bd)) => d < bd || (d == bd && other.id < entities[bj].id),
            };
            if take {
                nearest = Some((j, d));
            }
        }
        if let Some((j, d)) = nearest {
            // Co-located (d == 0): no meaningful direction to step toward —
            // report the drive without a step target rather than stepping
            // toward the body's own tile for no directional reason.
            let target = if d == 0 { None } else { Some(step_toward(e.pos, entities[j].pos)) };
            return (Drive::Hunt, target);
        }
    }

    // Graze — the discrete equivalent of the old continuous wander. A
    // committed movement goal (`CmdKind::SetTarget`) takes priority: step
    // toward it. Otherwise re-roll a uniformly random one of the 8
    // neighbours, or stay put (index 8 of 9), every eligible tick.
    if let Some(goal) = e.move_target {
        if goal != e.pos {
            return (Drive::Graze, Some(step_toward(e.pos, goal)));
        }
    }
    let roll = rand_below(seed, tick, e.id.wrapping_add(0x9A2E), Channel::Wander, 9) as usize;
    let target = if roll == 8 {
        e.pos
    } else {
        let (dx, dy) = NEIGHBOURS_8[roll];
        e.pos.offset(dx, dy)
    };
    (Drive::Graze, Some(target))
}

/// The "player" half of the player/AI symmetry this module's own doc
/// comment describes. Derived purely from `Entity.move_target` — no danger
/// sensing, no hunt sensing, no random wander. Unlike Graze's own
/// `move_target` fallback (above), there is no "otherwise re-roll a random
/// neighbour" case: a player with no committed goal is standing still
/// because that *is* the player's decision, not because nothing else fired.
/// `World::phase_movement` calls this instead of `ai_decide_move` for
/// exactly the one entity named by `World.player_id`, if any — everyone
/// else's movement is unconditionally `ai_decide_move`, unchanged.
pub fn player_decide_move(e: &Entity) -> Option<Tile> {
    match e.move_target {
        Some(goal) if goal != e.pos => Some(step_toward(e.pos, goal)),
        _ => None,
    }
}
