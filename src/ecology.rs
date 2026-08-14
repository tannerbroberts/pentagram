//! S2 — feeding, starvation and reproduction.
//!
//! Stage 0 shipped a five-race predation ring (`Element::eats`), a fully
//! tuned `OnConsume` channel in every race's deposit/consume mix, and an
//! `hp` field on every body — but nothing ever fired an `OnConsume` event,
//! nothing ever moved `hp`, and the `Forage` random channel sat reserved and
//! unread. S2 is that wiring, not a new subsystem: a body within reach of
//! prey on the ring it eats consumes it, which feeds the existing channel,
//! moves the existing `hp` field, and — when a meal fills a body up — spawns
//! an offspring through the existing `World::spawn` (so it charges `OnBirth`
//! the same way a command-spawned or seeded body always has).
//!
//! **Update, S3.1** — the paragraph below is superseded, kept for history the
//! same way S1's ring/star → attrition/suppression note is in `README.md`:
//! ~~No plant/animal type split. The ring is a full pentagon — every race
//! eats exactly one other and is eaten by exactly one other (`element.rs`'s
//! own arithmetic) — so nothing in the shipped data distinguishes a "plant"
//! tier from an "animal" tier.~~ `src/race.rs`'s `Kind` axis now does exactly
//! that: every element splits into a `Kind::Plant` and a `Kind::Animal` race
//! (`Race`, ten rows total), a real structural distinction rather than an
//! interpretive reading. **S3.1 is a scaffold stage** — the mechanisms
//! *this* file owns (`phase_feeding`'s predation pairing, `apply_attrition`/
//! `apply_suppression`) are still purely `Element`-keyed and completely
//! `Kind`-unaware, so a Plant is, today, still exactly as mobile and exactly
//! as eligible to be predator or prey as its Animal twin — the shipped Plant
//! rows in `race.rs` are literal copies of their Animal counterpart for
//! exactly this reason. Kind-gated predation (a Plant is never a predator;
//! hunting Animal prey needs a tunable hunt-weight roll) and plant rooting
//! (`phase_movement` skipping `Kind::Plant`) are S3.2/S3.3's job — see
//! `docs/S3_ECOLOGY_LAYERS_DESIGN.md` §4, §5, §12.
//!
//! **Update, S3.2** — the paragraph above is itself now partly superseded:
//! ~~a Plant is, today, still exactly as mobile and exactly as eligible to
//! be predator or prey as its Animal twin~~. Two of the three gaps it named
//! are closed. `World::phase_feeding`'s pairing derivation now refuses a
//! Plant as predator (kind-gated on `entities[pred].kind == Kind::Animal`,
//! `world.rs`); `World::phase_movement` now skips `Kind::Plant` entirely,
//! before step/jitter/reflect/clamp run — rooted is a structural guarantee,
//! not just a `speed: Fx::ZERO` number nothing reads. What is **not** closed:
//! `apply_attrition`/`apply_suppression` below remain deliberately
//! unconditional on `Kind` — plants still take terrain-based ring/star
//! damage exactly like animals, on purpose, so a future reader should not
//! "helpfully" add a kind exemption here, it was never intended. And the
//! hunt-weight gate on Animal-vs-Animal predation (a satiated, in-reach
//! Animal predator still always eats Animal prey today, same as before) is
//! still S3.3's job, not shipped yet.
//!
//! **Update, S3.3** — that gap is now closed too. `EcologyTuning::hunt_weight`
//! (`PerRace<u16>`) now gates the Animal-vs-Animal edge of
//! `World::phase_feeding` via a per-predator-per-tick roll on the new
//! `Channel::Hunt` (rolled once per predator per tick, on `(seed, tick,
//! predator id)` only, so it agrees with itself across every prey candidate
//! tested against that predator this tick — never rolled per prey pair);
//! grazing (Animal-vs-Plant) remains fully unconditional, exactly as before.
//! The shipped default is a uniform 150‰ across every Animal row —
//! deliberately "near zero" per the design brief, not a derived constant —
//! with Plant rows left at zero since they are never read. Real per-race
//! differentiation ("this animal is more carnivorous than that one") is
//! future live-tuning work, not decided here.
//!
//! Every number below is a first guess, in the same spirit `race.rs` and
//! `terrain.rs` state of their own tables: a starting point for the live
//! tuning loop, not a derived constant.

use crate::element::PerElement;
use crate::entity::Entity;
use crate::fx::Fx;
use crate::hash::{Hashable, Hasher};
use crate::race::{Kind, PerRace, Race};
use crate::terrain::Terrain;

/// First-guess default for `EcologyTuning::hunt_weight`'s Animal rows — "near
/// zero" per the design brief (docs/S3_ECOLOGY_LAYERS_DESIGN.md's forks list,
/// item 3), not a derived constant. Which animal ships "more carnivorous"
/// than another is an open live-tuning question (§13.6), not decided here —
/// every Animal row ships this same uniform value until that tuning pass.
const HUNT_WEIGHT_DEFAULT: u16 = 150;

/// The rate knobs `World::phase_feeding` and the starvation half of
/// `World::phase_aging` read. `PerElement`-shaped throughout, so it drops
/// into the `chaos` knob/page machinery the same way `TerrainTuning` and
/// `ClimateTuning` already do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EcologyTuning {
    /// How far a body can reach to eat prey on its ring edge, in cells.
    pub forage_radius: PerElement<Fx>,
    /// Minimum ticks between one body's successful meals — read off `hunger`,
    /// so a body that just ate ignores prey in reach until this many ticks
    /// have passed. Without a cooldown, `phase_feeding` runs every tick and
    /// every predator within reach eats every single tick it can: population
    /// collapse of every prey species inside a few hundred ticks, observed
    /// empirically before this knob existed. `RaceAttrs::FEED_PERIOD` (200 —
    /// "one meal per this many ticks") is the cadence the `OnConsume`
    /// channel's own demand accounting has assumed since Stage 0; this reuses
    /// it as the actual gameplay pace rather than leaving it a pure
    /// accounting fiction.
    pub satiation: PerElement<u32>,
    /// `hp` restored by one successful meal, out of [`crate::entity::MAX_HP`].
    pub feed_gain: PerElement<i32>,
    /// Ticks without a meal before starvation drain begins. A grace period,
    /// not an alarm clock — most bodies die of old age well inside it.
    pub starve_after: PerElement<u32>,
    /// `hp` lost per tick once starvation has begun.
    pub starve_rate: PerElement<i32>,
    /// `hp` a meal must reach (from below) to trigger reproduction. Set to
    /// `MAX_HP` by default: a body reproduces on the meal that fills it up,
    /// not on every meal.
    pub repro_threshold: PerElement<i32>,
    /// Permille of `element.eaten_by()`'s terrain concentration converted to
    /// hp damage each terrain tick (`apply_attrition`). Terrain's ring
    /// relation, redirected: standing in what eats you is what burns you
    /// now, not an ambient terrain-to-terrain conversion. Terrain isn't its
    /// own actor — this is why the old `TerrainTuning::ring_rate` is gone.
    pub attrition_rate: PerElement<u16>,
    /// Permille of `element.suppressed_by()`'s terrain concentration added
    /// directly to `hunger` each terrain tick (`apply_suppression`).
    /// Terrain's star relation, redirected: suppressed ability shows up as
    /// degraded foraging, felt through the starvation machinery this file
    /// already owns rather than as direct damage.
    pub suppression_rate: PerElement<u16>,
    /// Permille chance, per satiated in-reach Animal predator per tick, that
    /// an Animal prey candidate is actually hunted rather than passed over —
    /// the per-race dial that makes "carnivorous" a tunable spectrum instead
    /// of a hardcoded herbivore/carnivore topology (`Element::eats()` is
    /// reused as the edge; this only gates how often an Animal predator takes
    /// that edge against Animal, not Plant, prey — grazing stays
    /// unconditional). Rolled once per predator per tick, not per prey pair —
    /// the roll depends only on (seed, tick, predator id), so it agrees with
    /// itself across every prey candidate a predator is tested against in the
    /// same tick. Plant rows are unread: a Plant is never a predator
    /// (`World::phase_feeding`'s `Kind::Animal` gate), so their entries exist
    /// only so `PerRace` stays uniformly 10-wide, not because anything
    /// consults them. See `docs/S3_ECOLOGY_LAYERS_DESIGN.md` §5, §13.6.
    pub hunt_weight: PerRace<u16>,
}

impl Default for EcologyTuning {
    fn default() -> EcologyTuning {
        let mut hunt_weight = PerRace::filled(0u16);
        for r in Race::ALL {
            if r.kind == Kind::Animal {
                *hunt_weight.get_mut(r) = HUNT_WEIGHT_DEFAULT;
            }
        }
        EcologyTuning {
            forage_radius: PerElement::filled(Fx::ratio(400, 100)),
            satiation: PerElement::filled(crate::race::RaceAttrs::FEED_PERIOD as u32),
            feed_gain: PerElement::filled(40),
            // Generous relative to `RaceAttrs::FEED_PERIOD` (200 ticks — the
            // "one meal per this many ticks" cadence the OnConsume channel's
            // own demand accounting has assumed since Stage 0): ten missed
            // windows, not three, before drain begins. A slow, long-lived
            // race (Earth: 4/100 cells/tick, a 14-day life) needs real
            // opportunity to find prey at all before starvation is a fair
            // pressure rather than a death sentence for being slow.
            starve_after: PerElement::filled(2000),
            starve_rate: PerElement::filled(1),
            // Below `MAX_HP`, not at it: a newborn spawns at half strength
            // (`Entity::spawn`), so requiring the *full* cap would mean two
            // meals — one to reach maturity, a second to actually reproduce
            // — before any generation can replace itself. One solid meal
            // (`feed_gain` past this mark) is "well fed enough," not
            // "topped out."
            repro_threshold: PerElement::filled(3 * crate::entity::MAX_HP / 4),
            // 1, not the old `ring_rate`'s 5: hp's range (MAX_HP = 100) is
            // ~650x smaller than terrain stock's range (u16::MAX). At rate
            // 1, a fully saturated single-element cell still cannot one-shot
            // a full-health body in a single terrain tick; rate 2 already
            // can once the `.min(hp)` cap engages.
            attrition_rate: PerElement::filled(1),
            // 14, the literal old `star_rate` default, redirected onto
            // `hunger` instead of a terrain stock — same first-guess
            // intensity, new target.
            suppression_rate: PerElement::filled(14),
            hunt_weight,
        }
    }
}

impl Hashable for EcologyTuning {
    fn hash_into(&self, h: &mut Hasher) {
        for (_, v) in self.forage_radius.iter() {
            h.i32(v.raw());
        }
        for (_, v) in self.satiation.iter() {
            h.u32(*v);
        }
        for (_, v) in self.feed_gain.iter() {
            h.i32(*v);
        }
        for (_, v) in self.starve_after.iter() {
            h.u32(*v);
        }
        for (_, v) in self.starve_rate.iter() {
            h.i32(*v);
        }
        for (_, v) in self.repro_threshold.iter() {
            h.i32(*v);
        }
        for (_, v) in self.attrition_rate.iter() {
            h.u16(*v);
        }
        for (_, v) in self.suppression_rate.iter() {
            h.u16(*v);
        }
        for (_, v) in self.hunt_weight.iter() {
            h.u16(*v);
        }
    }
}

/// Operator 3 of `phase_terrain`, in `ecology.rs` not `terrain.rs`: terrain
/// is read, never written, and only living bodies are affected. A body of
/// element `e` takes hp damage proportional to the terrain concentration of
/// `e.eaten_by()` at the cell it currently occupies — the ring relation,
/// redirected onto the thing standing in it instead of onto the terrain
/// itself. Mirrors the old `apply_ring`'s permille-of-stock math; the
/// `.min(hp)` cap plays the role `apply_star`'s `.min(snapshot[e])` cap
/// used to (a loss can never exceed what the target actually holds).
///
/// No inline death/reap handling: `World::phase_aging` runs every tick,
/// unconditionally, and already catches `hp <= 0` the very next tick — the
/// same one-tick lag starvation's own drain already has.
pub fn apply_attrition(entities: &mut [Entity], terrain: &Terrain, tuning: &EcologyTuning) {
    for e in entities.iter_mut() {
        // Deliberately unconditional on `e.kind` — plants take terrain-based
        // ring damage exactly like animals. See the module doc's S3.2 note.
        if !e.alive {
            continue;
        }
        let (x, y) = terrain.cell_of(e.pos);
        let stock = terrain.cell(x, y)[e.element.eaten_by()] as u64;
        let rate = tuning.attrition_rate[e.element] as u64;
        let dmg = ((stock * rate) / 1000).min(e.hp.max(0) as u64) as i32;
        e.hp = e.hp.saturating_sub(dmg);
    }
}

/// Operator 4 of `phase_terrain`, in `ecology.rs` not `terrain.rs`: same
/// terrain-read/body-write shape as `apply_attrition`. A body of element `e`
/// has `hunger` pushed forward proportional to the terrain concentration of
/// `e.suppressed_by()` at its cell — the star relation, redirected: ability
/// suppressed by what's around it shows up as degraded foraging, felt
/// through the starvation machinery `phase_aging`/`starve_after`/
/// `starve_rate` already implement, not as a new kind of damage.
pub fn apply_suppression(entities: &mut [Entity], terrain: &Terrain, tuning: &EcologyTuning) {
    for e in entities.iter_mut() {
        // Deliberately unconditional on `e.kind` — plants take terrain-based
        // star damage exactly like animals. See the module doc's S3.2 note.
        if !e.alive {
            continue;
        }
        let (x, y) = terrain.cell_of(e.pos);
        let stock = terrain.cell(x, y)[e.element.suppressed_by()] as u64;
        let rate = tuning.suppression_rate[e.element] as u64;
        let extra = ((stock * rate) / 1000) as u32;
        e.hunger = e.hunger.saturating_add(extra);
    }
}

/// S3.5's plant-reproduction knob table for `World::phase_flora`.
/// `PerElement`-shaped, not `PerRace`-shaped, because only `Kind::Plant` rows
/// ever read it -- an Animal never calls `phase_flora`'s logic, so shipping
/// five permanently dead `PerRace` rows would misstate what actually varies.
/// Every number below is a first guess, in the same spirit `race.rs` and
/// this file's own `EcologyTuning` state of their own tables: a starting
/// point for the live tuning loop, not a derived constant. See
/// `docs/S3_ECOLOGY_LAYERS_DESIGN.md` section 7.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PropagationTuning {
    /// Terrain ticks between propagation attempts. 0 means a Plant of that
    /// element never attempts.
    pub period: PerElement<u64>,
    /// Permille chance, per eligible Plant per attempt, that propagation
    /// actually fires.
    pub chance: PerElement<u16>,
    /// Permille of full size a new offspring is born at (`Entity.size`).
    pub offspring_size: PerElement<u16>,
    /// Minimum terrain stock of the plant's *habitat* element (`Element::
    /// habitat`, the ingredient it draws down from terrain to sustain
    /// itself -- not its own element) required at the candidate cell for
    /// rooting to succeed -- the same raw stock scale `EcologyTuning::
    /// attrition_rate` reads (up to `u16::MAX`), not a per-mille fraction.
    pub root_min: PerElement<u16>,
    /// Max scatter offset from the parent, same shape as `world::BIRTH_SCATTER`.
    pub dispersal: PerElement<Fx>,
    /// Max same-race bodies already occupying the candidate cell before
    /// rooting is refused -- the mitigation for the positive-feedback
    /// runaway risk named in section 7 of the design doc: more plants
    /// deposit more of their element, which makes rooting easier, which
    /// allows more plants.
    pub crowd_max: PerElement<u16>,
    /// Own-element terrain stock at which a growing Plant's size ceiling
    /// reaches full (1000 permille) -- the "made of" mechanic: a Plant's
    /// growth potential scales with how much of its own element the local
    /// terrain already holds (self-reinforcing, since Plants are
    /// existence-dominant depositors of that same element). 0 disables
    /// scaling (ceiling is always 1000, matching an Animal). Same raw stock
    /// scale `root_min` uses, not a per-mille fraction. First-guess
    /// default, like every other number in this table -- a starting point
    /// for live tuning, not a derived constant.
    pub growth_ref: PerElement<u16>,
}

impl Default for PropagationTuning {
    fn default() -> PropagationTuning {
        PropagationTuning {
            period: PerElement::filled(3),
            chance: PerElement::filled(200),
            offspring_size: PerElement::filled(200),
            root_min: PerElement::filled(300),
            dispersal: PerElement::filled(Fx::ratio(300, 100)),
            crowd_max: PerElement::filled(3),
            growth_ref: PerElement::filled(1000),
        }
    }
}

impl Hashable for PropagationTuning {
    fn hash_into(&self, h: &mut Hasher) {
        for (_, v) in self.period.iter() {
            h.u64(*v);
        }
        for (_, v) in self.chance.iter() {
            h.u16(*v);
        }
        for (_, v) in self.offspring_size.iter() {
            h.u16(*v);
        }
        for (_, v) in self.root_min.iter() {
            h.u16(*v);
        }
        for (_, v) in self.dispersal.iter() {
            h.i32(v.raw());
        }
        for (_, v) in self.crowd_max.iter() {
            h.u16(*v);
        }
        for (_, v) in self.growth_ref.iter() {
            h.u16(*v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::element::Element;

    #[test]
    fn default_is_internally_sane() {
        for e in Element::ALL {
            let t = EcologyTuning::default();
            assert!(t.forage_radius[e] > Fx::ZERO, "{} has no reach", e.name());
            assert!(t.feed_gain[e] > 0, "{} gains nothing from a meal", e.name());
            assert!(t.starve_rate[e] > 0, "{} never starves", e.name());
            assert!(
                t.repro_threshold[e] <= crate::entity::MAX_HP,
                "{} can never reach its own reproduction threshold",
                e.name()
            );
        }
    }

    #[test]
    fn hunt_weight_defaults_to_near_zero_for_animals_and_zero_for_plants() {
        let t = EcologyTuning::default();
        for r in Race::ALL {
            let w = t.hunt_weight[r];
            match r.kind {
                Kind::Animal => assert!(w > 0 && w < 1000, "{}-Animal hunt_weight should be a real, sub-certain dial, got {}", r.element.name(), w),
                Kind::Plant => assert_eq!(w, 0, "{}-Plant hunt_weight is unread and should stay zero", r.element.name()),
            }
        }
    }

    #[test]
    fn hash_notices_every_field() {
        let a = EcologyTuning::default();
        let base = {
            let mut h = Hasher::new();
            a.hash_into(&mut h);
            h.finish()
        };

        let mut forage = a;
        forage.forage_radius[Element::Fire] += Fx::ONE;
        let mut satiation = a;
        satiation.satiation[Element::Fire] += 1;
        let mut feed_gain = a;
        feed_gain.feed_gain[Element::Fire] += 1;
        let mut starve_after = a;
        starve_after.starve_after[Element::Fire] += 1;
        let mut starve_rate = a;
        starve_rate.starve_rate[Element::Fire] += 1;
        let mut repro_threshold = a;
        repro_threshold.repro_threshold[Element::Fire] += 1;
        let mut attrition_rate = a;
        attrition_rate.attrition_rate[Element::Fire] += 1;
        let mut suppression_rate = a;
        suppression_rate.suppression_rate[Element::Fire] += 1;
        let mut hunt_weight = a;
        hunt_weight.hunt_weight[Race { element: Element::Fire, kind: Kind::Animal }] += 1;

        for (name, tuning) in [
            ("forage_radius", forage),
            ("satiation", satiation),
            ("feed_gain", feed_gain),
            ("starve_after", starve_after),
            ("starve_rate", starve_rate),
            ("repro_threshold", repro_threshold),
            ("attrition_rate", attrition_rate),
            ("suppression_rate", suppression_rate),
            ("hunt_weight", hunt_weight),
        ] {
            let mut h = Hasher::new();
            tuning.hash_into(&mut h);
            assert_ne!(h.finish(), base, "{name} does not affect the hash");
        }
    }

    fn spawn_at(element: Element, pos: crate::fx::V2) -> Entity {
        let race = crate::race::Race { element, kind: crate::race::Kind::Animal };
        let a = crate::race::attrs(race);
        let mut e = Entity::spawn(1, element, pos, 5, 0, a);
        e.hp = crate::entity::MAX_HP;
        e
    }

    #[test]
    fn attrition_never_drives_hp_below_zero_in_one_call() {
        let mut terrain = Terrain::new(2);
        // Wood.eaten_by() == Fire — saturate Fire so a Wood body standing
        // here takes the largest possible single-tick hit.
        terrain.cell_mut(0, 0)[Element::Fire] = u16::MAX;
        let tuning = EcologyTuning { attrition_rate: PerElement::filled(1000), ..EcologyTuning::default() };
        let mut entities = [spawn_at(Element::Wood, crate::fx::V2::ZERO)];
        apply_attrition(&mut entities, &terrain, &tuning);
        assert_eq!(entities[0].hp, 0, "capped at what the body actually held");
    }

    #[test]
    fn attrition_does_nothing_at_zero_concentration() {
        let terrain = Terrain::new(2);
        let tuning = EcologyTuning { attrition_rate: PerElement::filled(1000), ..EcologyTuning::default() };
        let mut entities = [spawn_at(Element::Wood, crate::fx::V2::ZERO)];
        apply_attrition(&mut entities, &terrain, &tuning);
        assert_eq!(entities[0].hp, crate::entity::MAX_HP, "no eater stock, no damage");
    }

    #[test]
    fn a_fully_saturated_cell_at_the_shipped_rate_cannot_one_shot_a_full_health_body() {
        let mut terrain = Terrain::new(2);
        terrain.cell_mut(0, 0)[Element::Fire] = u16::MAX;
        let tuning = EcologyTuning::default();
        let mut entities = [spawn_at(Element::Wood, crate::fx::V2::ZERO)];
        apply_attrition(&mut entities, &terrain, &tuning);
        assert!(entities[0].hp > 0, "shipped attrition_rate one-shot a full-health body");
    }

    #[test]
    fn suppression_adds_hunger_proportional_to_suppressor_concentration() {
        let mut terrain = Terrain::new(2);
        // Fire.suppressed_by() == Water.
        terrain.cell_mut(0, 0)[Element::Water] = 1000;
        let tuning = EcologyTuning { suppression_rate: PerElement::filled(500), ..EcologyTuning::default() };
        let mut entities = [spawn_at(Element::Fire, crate::fx::V2::ZERO)];
        apply_suppression(&mut entities, &terrain, &tuning);
        assert_eq!(entities[0].hunger, 500, "1000 * 500‰ = 500");
    }

    #[test]
    fn suppression_does_nothing_at_zero_concentration() {
        let terrain = Terrain::new(2);
        let tuning = EcologyTuning { suppression_rate: PerElement::filled(1000), ..EcologyTuning::default() };
        let mut entities = [spawn_at(Element::Fire, crate::fx::V2::ZERO)];
        apply_suppression(&mut entities, &terrain, &tuning);
        assert_eq!(entities[0].hunger, 0, "no suppressor stock, no extra hunger");
    }

    #[test]
    fn dead_entities_are_skipped_by_both_operators() {
        let mut terrain = Terrain::new(2);
        terrain.cell_mut(0, 0)[Element::Fire] = u16::MAX;
        terrain.cell_mut(0, 0)[Element::Water] = u16::MAX;
        let tuning = EcologyTuning {
            attrition_rate: PerElement::filled(1000),
            suppression_rate: PerElement::filled(1000),
            ..EcologyTuning::default()
        };
        let mut e = spawn_at(Element::Wood, crate::fx::V2::ZERO);
        e.alive = false;
        let mut entities = [e];
        apply_attrition(&mut entities, &terrain, &tuning);
        apply_suppression(&mut entities, &terrain, &tuning);
        assert_eq!(entities[0].hp, crate::entity::MAX_HP);
        assert_eq!(entities[0].hunger, 0);
    }

    #[test]
    fn propagation_default_is_internally_sane() {
        let t = PropagationTuning::default();
        for e in Element::ALL {
            assert!(t.period[e] > 0, "{} never attempts propagation", e.name());
            assert!(t.chance[e] > 0 && t.chance[e] <= 1000, "{} has an out-of-range chance", e.name());
            assert!(t.offspring_size[e] > 0 && t.offspring_size[e] < 1000, "{} seedlings do not start smaller than mature", e.name());
            assert!(t.crowd_max[e] > 0, "{} can never root anywhere", e.name());
        }
    }

    #[test]
    fn propagation_hash_notices_every_field() {
        let a = PropagationTuning::default();
        let base = {
            let mut h = Hasher::new();
            a.hash_into(&mut h);
            h.finish()
        };

        let mut period = a;
        period.period[Element::Fire] += 1;
        let mut chance = a;
        chance.chance[Element::Fire] += 1;
        let mut offspring_size = a;
        offspring_size.offspring_size[Element::Fire] += 1;
        let mut root_min = a;
        root_min.root_min[Element::Fire] += 1;
        let mut dispersal = a;
        dispersal.dispersal[Element::Fire] += Fx::ONE;
        let mut crowd_max = a;
        crowd_max.crowd_max[Element::Fire] += 1;
        let mut growth_ref = a;
        growth_ref.growth_ref[Element::Fire] += 1;

        for (name, tuning) in [
            ("period", period),
            ("chance", chance),
            ("offspring_size", offspring_size),
            ("root_min", root_min),
            ("dispersal", dispersal),
            ("crowd_max", crowd_max),
            ("growth_ref", growth_ref),
        ] {
            let mut h = Hasher::new();
            tuning.hash_into(&mut h);
            assert_ne!(h.finish(), base, "{name} does not affect the hash");
        }
    }
}
