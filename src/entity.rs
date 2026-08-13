//! The simulated body.
//!
//! Stage 0 kept this deliberately thin — enough state to exercise movement,
//! collision, ageing and every deposition channel, and nothing more. S1 added
//! the terrain field; S2 adds feeding, starvation and reproduction
//! (`World::phase_feeding`, `ecology.rs`). Combat has no home yet; that
//! arrives at S5.

use crate::element::Element;
use crate::fx::{Fx, V2};
use crate::hash::{Hashable, Hasher};
use crate::rand::{rand_range, rand_signed, Channel};
use crate::race::{Kind, Race, RaceAttrs};
#[cfg(test)]
use crate::race::attrs;

/// Displacement below which a tick does not count as an action for the
/// `OnAction` deposition channel. Standing still must not terraform, or Water
/// stops being action-dominant in practice.
pub const ACTION_THRESHOLD: Fx = Fx::ratio(1, 100);

/// The top of `hp`'s range — a fixed 0..=100 scale regardless of race, the
/// same way every race's channel mix lives on a 0..1000 per-mille scale
/// regardless of its `deposit_unit`. S2's feeding and starvation both read
/// and write within this range.
pub const MAX_HP: i32 = 100;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Entity {
    pub id: u32,
    pub element: Element,
    pub kind: Kind,
    pub pos: V2,
    /// Unit vector. Movement is `heading * SPEED[element]`.
    pub heading: V2,
    pub age: u64,
    /// Rolled at birth from the race's base lifespan plus its variance, so a
    /// cohort born together does not die together.
    pub lifespan: u64,
    pub hp: i32,
    pub alive: bool,
    /// Set by movement, read by demand accumulation, cleared each tick.
    pub acted: bool,
    /// S2: ticks since this body last fed. Drives starvation in
    /// `World::phase_aging`; reset to zero by a successful meal in
    /// `World::phase_feeding`.
    pub hunger: u32,
}

impl Entity {
    /// `a` is the spawning world's *live* row for this element, not the shipped
    /// table — the live view can retune between one birth and the next.
    pub fn spawn(id: u32, element: Element, pos: V2, seed: u64, tick: u64, a: &RaceAttrs) -> Entity {
        Entity {
            id,
            element,
            kind: a.kind,
            pos,
            heading: initial_heading(seed, tick, id),
            age: 0,
            lifespan: roll_lifespan(a, seed, tick, id),
            // Born at half strength, not full: `EcologyTuning::repro_threshold`
            // defaults to `MAX_HP` itself, so a body spawned already at the
            // cap could never cross it from below and would need a full
            // starvation-and-recovery cycle before its first possible birth —
            // an avoidable multi-thousand-tick stall on every population's
            // very first generation. Starting below the cap means the meal
            // that raises a newborn to maturity is also the one that can
            // trigger its own reproduction.
            hp: MAX_HP / 2,
            alive: true,
            acted: false,
            hunger: 0,
        }
    }

    #[inline]
    pub fn is_expired(&self) -> bool {
        self.age >= self.lifespan
    }

    /// This body's race — `(element, kind)`, the axis race-attribute lookups
    /// resolve off. Recovers it from the two fields rather than storing it
    /// redundantly, so every call site that needs it does so through one
    /// accessor instead of hand-building the struct literal.
    #[inline]
    pub fn race(&self) -> Race {
        Race { element: self.element, kind: self.kind }
    }
}

impl Hashable for Entity {
    fn hash_into(&self, h: &mut Hasher) {
        h.u32(self.id)
            .u8(self.element as u8)
            .u8(self.kind as u8)
            .i32(self.pos.x.raw())
            .i32(self.pos.y.raw())
            .i32(self.heading.x.raw())
            .i32(self.heading.y.raw())
            .u64(self.age)
            .u64(self.lifespan)
            .i32(self.hp)
            .bool(self.alive)
            .bool(self.acted)
            .u32(self.hunger);
    }
}

/// Deterministic per-individual lifespan. Variance is per-mille around the
/// race's base value.
pub fn roll_lifespan(a: &RaceAttrs, seed: u64, tick: u64, id: u32) -> u64 {
    let v = a.lifespan_variance as i32;
    if v == 0 {
        return a.lifespan.max(1);
    }
    let delta = rand_range(seed, tick, id, Channel::LifespanVariance, -v, v + 1);
    let scaled = (a.lifespan as i128) * (1000 + delta as i128) / 1000;
    scaled.max(1) as u64
}

/// A deterministic unit heading from the entity's birth coordinates.
pub fn initial_heading(seed: u64, tick: u64, id: u32) -> V2 {
    let x = rand_signed(seed, tick, id, Channel::Wander);
    let y = rand_signed(seed, tick, id.wrapping_add(0x5EED), Channel::Wander);
    let v = V2::new(x, y);
    if v.len_sq().is_zero() {
        V2::new(Fx::ONE, Fx::ZERO)
    } else {
        v.normalized()
    }
}

/// The one specific race the tests below reach for when they need "the" row
/// for an element's Animal variant rather than every row. Since S3.2,
/// `Kind::Animal` is a genuine, deliberate choice of which row a given test
/// examines — Plant and Animal rows are numerically distinct now (see
/// `race.rs`'s `RACES` doc comment) — not an "it doesn't matter which"
/// shortcut left over from the S3.1 scaffold.
#[cfg(test)]
fn animal(e: Element) -> Race {
    Race { element: e, kind: Kind::Animal }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_is_reproducible() {
        let f = attrs(animal(Element::Fire));
        let a = Entity::spawn(7, Element::Fire, V2::ZERO, 99, 12, f);
        let b = Entity::spawn(7, Element::Fire, V2::ZERO, 99, 12, f);
        assert_eq!(a, b);
    }

    #[test]
    fn different_ids_get_different_headings() {
        let w = attrs(animal(Element::Water));
        let a = Entity::spawn(1, Element::Water, V2::ZERO, 5, 0, w);
        let b = Entity::spawn(2, Element::Water, V2::ZERO, 5, 0, w);
        assert_ne!(a.heading, b.heading);
    }

    #[test]
    fn a_retuned_lifespan_reaches_the_bodies_born_after_it() {
        // The live view's whole premise: turn a knob, and the next thing born
        // is built to the new number.
        let mut a = *attrs(animal(Element::Fire));
        a.lifespan_variance = 0;
        a.lifespan = 4242;
        let e = Entity::spawn(1, Element::Fire, V2::ZERO, 5, 0, &a);
        assert_eq!(e.lifespan, 4242);
    }

    #[test]
    fn initial_heading_is_unit_length() {
        for id in 0..500u32 {
            let h = initial_heading(3, 0, id);
            let l = h.len();
            assert!(
                (l - Fx::ONE).abs().raw() <= 512,
                "id {} heading length {:?}",
                id,
                l
            );
        }
    }

    #[test]
    fn lifespan_stays_inside_the_variance_band() {
        for race in Race::ALL {
            let a = attrs(race);
            let v = a.lifespan_variance as u128;
            let lo = (a.lifespan as u128) * (1000 - v) / 1000;
            let hi = (a.lifespan as u128) * (1000 + v) / 1000;
            for id in 0..400u32 {
                let l = roll_lifespan(a, 11, 0, id) as u128;
                assert!(l >= lo && l <= hi, "{}-{} id {} → {}", race.element.name(), race.kind.name(), id, l);
            }
        }
    }

    #[test]
    fn a_cohort_does_not_die_together() {
        let a = attrs(animal(Element::Fire));
        let spans: std::collections::BTreeSet<u64> =
            (0..200u32).map(|id| roll_lifespan(a, 1, 0, id)).collect();
        assert!(spans.len() > 100, "only {} distinct lifespans", spans.len());
    }

    #[test]
    fn speed_ordering_matches_the_fantasy() {
        let order = [
            Element::Fire,
            Element::Water,
            Element::Metal,
            Element::Wood,
            Element::Earth,
        ];
        for w in order.windows(2) {
            assert!(
                attrs(animal(w[0])).speed > attrs(animal(w[1])).speed,
                "{} should be faster than {}",
                w[0].name(),
                w[1].name()
            );
        }
    }

    #[test]
    fn every_race_has_a_kind_appropriate_body() {
        // S3.2's kind-aware `is_valid` rule, exercised directly: an Animal
        // must actually move, a Plant must actually not — and every row,
        // both kinds, still needs a body that crowds neighbours.
        for race in Race::ALL {
            let a = attrs(race);
            match race.kind {
                Kind::Animal => assert!(a.speed > Fx::ZERO, "{}-{} is immobile", race.element.name(), race.kind.name()),
                Kind::Plant => assert!(a.speed == Fx::ZERO, "{}-{} should be rooted", race.element.name(), race.kind.name()),
            }
            assert!(a.radius > Fx::ZERO, "{}-{} has no body", race.element.name(), race.kind.name());
        }
    }

    // S3.0: `Entity` has never had a per-field hash coverage test. `Hashable`
    // is hand-rolled with no derive and no reflection (`src/hash.rs`), so a
    // field added to the struct but forgotten in `hash_into` compiles clean
    // and passes every other test in this file silently. This test exists so
    // S3's `kind`/`size` additions extend an established pattern instead of
    // being the first-ever instance of it — see `EcologyTuning`'s
    // `hash_notices_every_field` (`src/ecology.rs`) for the sibling case.
    // S3.1 extends this with a `kind` variant rather than writing a new test.
    #[test]
    fn hash_notices_every_field() {
        let f = attrs(animal(Element::Fire));
        let base = Entity::spawn(1, Element::Fire, V2::ZERO, 5, 0, f);
        let hash_of = |e: &Entity| {
            let mut h = Hasher::new();
            e.hash_into(&mut h);
            h.finish()
        };
        let base_hash = hash_of(&base);

        let mut id = base;
        id.id += 1;
        let mut element = base;
        element.element = Element::Water;
        let mut kind = base;
        kind.kind = Kind::Plant;
        let mut pos_x = base;
        pos_x.pos.x = pos_x.pos.x + Fx::ONE;
        let mut pos_y = base;
        pos_y.pos.y = pos_y.pos.y + Fx::ONE;
        let mut heading_x = base;
        heading_x.heading.x = heading_x.heading.x + Fx::ONE;
        let mut heading_y = base;
        heading_y.heading.y = heading_y.heading.y + Fx::ONE;
        let mut age = base;
        age.age += 1;
        let mut lifespan = base;
        lifespan.lifespan += 1;
        let mut hp = base;
        hp.hp += 1;
        let mut alive = base;
        alive.alive = !alive.alive;
        let mut acted = base;
        acted.acted = !acted.acted;
        let mut hunger = base;
        hunger.hunger += 1;

        for (name, variant) in [
            ("id", id),
            ("element", element),
            ("kind", kind),
            ("pos.x", pos_x),
            ("pos.y", pos_y),
            ("heading.x", heading_x),
            ("heading.y", heading_y),
            ("age", age),
            ("lifespan", lifespan),
            ("hp", hp),
            ("alive", alive),
            ("acted", acted),
            ("hunger", hunger),
        ] {
            assert_ne!(hash_of(&variant), base_hash, "{name} does not affect the hash");
        }
    }
}
