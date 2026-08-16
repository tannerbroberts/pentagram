//! The simulated body.
//!
//! Stage 0 kept this deliberately thin — enough state to exercise movement,
//! collision, ageing and every deposition channel, and nothing more. S1 added
//! the terrain field; S2 adds feeding, starvation and reproduction
//! (`World::phase_feeding`, `ecology.rs`). Combat has no home yet; that
//! arrives at S5.

use crate::element::{Element, PerElement};
use crate::fx::{Fx, V2};
use crate::hash::{Hashable, Hasher};
use crate::rand::{rand_range, rand_signed, Channel};
use crate::race::{ActionSlot, Kind, Race, RaceAttrs};
#[cfg(test)]
use crate::race::attrs;

/// Displacement below which a tick does not count as an action for the
/// `OnAction` deposition channel. Standing still must not terraform, or Water
/// stops being action-dominant in practice.
pub const ACTION_THRESHOLD: Fx = Fx::ratio(1, 100);

/// The top of `hp`'s range — a fixed 0..=100 scale regardless of race, the
/// same way every race's channel mix lives on a 0..1000 per-mille scale
/// regardless of its `consume_unit`. S2's feeding and starvation both read
/// and write within this range.
pub const MAX_HP: i32 = 100;

/// Items and inventory: a lightweight, single-element material bundle a body
/// carries in `Entity.items` (below), distinct from `Entity.carried` (loose,
/// unbundled stock of other elements) and `Entity.material` (the body's own
/// element, its own mass). Created by `World`'s `MakeItem` command from
/// carried stock, destroyed by `BreakItem`, which returns `quantity` units of
/// `element` to terrain at the breaking body's position — Invariant VIII: a
/// pure transfer, nothing created or destroyed, in either direction. No
/// composites/alloys and no durability this pass (deferred, see
/// `docs/S3_ECOLOGY_LAYERS_DESIGN.md`'s successor design notes) — an `Item`
/// is a quantity of exactly one element, nothing more.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Item {
    pub element: Element,
    pub quantity: u64,
}

impl Hashable for Item {
    fn hash_into(&self, h: &mut Hasher) {
        h.u8(self.element as u8).u64(self.quantity);
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
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
    /// S3.5: current structural size, per-mille of full size. 1000 for
    /// everything except an in-progress Plant seedling -- see grown_size.
    /// Read at exactly one place: World::phase_collisions' radius
    /// calculation, so a seedling crowds less than a mature plant. Does NOT
    /// scale deposit/consume demand -- a deliberate choice, see
    /// docs/S3_ECOLOGY_LAYERS_DESIGN.md section 7.
    pub size: u16,
    /// Invariant VIII (material conservation): how many units of its own
    /// element (`self.element`) this specific body currently holds/embodies
    /// -- distinct from `size` (a structural/collision-radius fraction) and
    /// from `hp`/`hunger` (the separate vitality system, untouched by this
    /// field). Grown by the `Body`-output arm of `World::apply_action_recipe`
    /// (this body's own race's `Exist` recipe, fired once per terrain tick —
    /// see `race.rs`'s module doc) and, for Animals, by predation -- killing
    /// prey transfers the prey's entire `material` to the predator's, in
    /// full (`World::phase_feeding`; "you are what you eat"). Lost entirely
    /// on death: `World::charge_death` returns it to terrain as
    /// `self.element`, at `self.pos`.
    pub material: u64,
    /// Items/inventory (post-Invariant-VIII): loose, unbundled material of
    /// *other* elements this body is physically carrying — distinct from
    /// `material` above, which is only ever this body's own element (what it
    /// is made of). Gained 1:1 from `World`'s `Mine` command (terrain →
    /// carried, gated by this race's `Mine` `ActionRecipe`, `Kind::Animal`
    /// only — Plants are rooted and never mine) and reshaped by `Smelt`
    /// (carried element X → carried `X.generates()`, at a fixed lossy ratio,
    /// tailings returned to terrain). Spent by `MakeItem`, which bundles a
    /// quantity of one element out of here into a portable `Item` (below).
    /// Always zero for a `Kind::Plant` body — nothing ever credits it, since
    /// mining is the only source and Plants cannot mine — so `MakeItem`/
    /// `Smelt` are naturally no-ops for a Plant without needing their own
    /// separate `Kind` gate.
    pub carried: PerElement<u64>,
    /// Items/inventory: portable, single-element material bundles this body
    /// holds, each created by `MakeItem` out of `carried` and destroyed by
    /// `BreakItem` (removed from here, its full `quantity` returned to
    /// terrain at this body's position, as its own `element` — Invariant
    /// VIII, a pure transfer). Ground-dropped items lying on terrain,
    /// independent of any entity, live in `World::ground_items` instead —
    /// populated by `World::charge_death`'s item split, reachable by a
    /// `Pickup` command.
    pub items: Vec<Item>,
    /// The action map's cooldown state: tick at or after which this body may
    /// next fire the recipe in each `ActionSlot`, indexed by
    /// `ActionSlot as usize`. See `World::apply_action_recipe`.
    pub action_ready_at: [u64; ActionSlot::COUNT],
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
            size: 1000,
            // Invariant VIII: a newborn starts holding none of its own
            // element -- the simplest conservative choice, and the one that
            // needs no parent-material bookkeeping at every one of the
            // several call sites that spawn a body with no particular
            // parent in hand (`seed_population`, command spawns). A body
            // grows its own material entirely through its own subsequent
            // consumption-conversion (`World::credit_body_material`), never
            // by inheriting a slice of a parent's. A richer birth-endowment
            // model (transferring some of a parent's material at birth) is
            // a plausible future refinement, not built here.
            material: 0,
            // Items/inventory: a newborn starts with nothing carried and no
            // items, for the same reason `material` starts at zero — mining,
            // smelting and item-making are this body's own subsequent
            // actions, never inherited from a parent.
            carried: PerElement::filled(0),
            items: Vec::new(),
            // A newborn may fire any action immediately -- cooldowns are
            // "ticks since last fired," not a startup delay.
            action_ready_at: [0; ActionSlot::COUNT],
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
            .u32(self.hunger)
            .u16(self.size)
            .u64(self.material);
        // Items/inventory: `carried` in fixed ring order (PerElement::iter's
        // contract), then `items` length-prefixed (so two inventories that
        // differ only in count still diverge) and each item in insertion
        // order — deterministic because `items` is only ever mutated by
        // canonically-ordered commands (Invariant V/VI).
        for (_, v) in self.carried.iter() {
            h.u64(*v);
        }
        h.u32(self.items.len() as u32);
        for item in &self.items {
            item.hash_into(h);
        }
        for v in self.action_ready_at {
            h.u64(v);
        }
    }
}

/// S3.5: the per-mille of lifespan at which a growing Plant reaches full
/// size. Derived, not an independent knob -- a live-tuning session cannot
/// put it out of sync with lifespan, since it is always read as a fraction
/// of whatever `lifespan` currently is rather than stored as its own tick
/// count.
pub const MATURITY_PERMILLE: u64 = 250;

/// S3.5: `Entity.size`'s pure growth function, purely derived from
/// `(birth_size, age, lifespan, ceiling)`, never accumulated -- so it can
/// be, and is (`World::phase_aging`), recomputed from scratch every tick.
/// Linear growth from `birth_size` at age 0 to `ceiling` at
/// `MATURITY_PERMILLE` of lifespan, then held at `ceiling`. `birth_size ==
/// ceiling` collapses this to a constant at every age with no
/// special-casing needed -- the shape every Animal (ceiling always 1000)
/// actually takes.
///
/// S3.8: `ceiling` is no longer hardcoded to 1000 -- callers pass a
/// per-tick value (`growth_ceiling`, for a Plant) so growth can be capped
/// below full size when the local terrain doesn't yet support it. If
/// `ceiling <= birth_size` there's no growth room this tick (e.g. poor
/// local stock), so this returns `birth_size` unchanged rather than
/// shrinking below it -- since size is recomputed fresh every tick, an
/// improving ceiling later still resumes growth from here.
pub fn grown_size(birth_size: u16, age: u64, lifespan: u64, ceiling: u16) -> u16 {
    if ceiling <= birth_size {
        return birth_size;
    }
    let maturity_age = lifespan.saturating_mul(MATURITY_PERMILLE) / 1000;
    if maturity_age == 0 || age >= maturity_age {
        return ceiling;
    }
    let birth = birth_size as u64;
    let target = ceiling as u64;
    (birth + (target - birth) * age / maturity_age).min(target) as u16
}

/// The size ceiling a growing Plant's local own-element terrain stock
/// currently supports, in permille (capped at 1000, matching `grown_size`'s
/// scale). `growth_ref == 0` disables scaling entirely (always full 1000,
/// same as an Animal). Pure integer math -- Invariant II.
#[inline]
pub fn growth_ceiling(own_element_stock: u16, growth_ref: u16) -> u16 {
    if growth_ref == 0 {
        return 1000;
    }
    ((own_element_stock as u32 * 1000) / growth_ref as u32).min(1000) as u16
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
        let mut a = attrs(animal(Element::Fire)).clone();
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

        // S3.9 note: `Entity` is no longer `Copy` (the new `items: Vec<Item>`
        // field can't be) — every variant below now explicitly `.clone()`s
        // `base` rather than relying on an implicit copy, the same discipline
        // `world.rs`'s own `state_hash_notices_*` tests already use for
        // `World` (which was never `Copy` to begin with).
        let mut id = base.clone();
        id.id += 1;
        let mut element = base.clone();
        element.element = Element::Water;
        let mut kind = base.clone();
        kind.kind = Kind::Plant;
        let mut pos_x = base.clone();
        pos_x.pos.x = pos_x.pos.x + Fx::ONE;
        let mut pos_y = base.clone();
        pos_y.pos.y = pos_y.pos.y + Fx::ONE;
        let mut heading_x = base.clone();
        heading_x.heading.x = heading_x.heading.x + Fx::ONE;
        let mut heading_y = base.clone();
        heading_y.heading.y = heading_y.heading.y + Fx::ONE;
        let mut age = base.clone();
        age.age += 1;
        let mut lifespan = base.clone();
        lifespan.lifespan += 1;
        let mut hp = base.clone();
        hp.hp += 1;
        let mut alive = base.clone();
        alive.alive = !alive.alive;
        let mut acted = base.clone();
        acted.acted = !acted.acted;
        let mut hunger = base.clone();
        hunger.hunger += 1;
        let mut size = base.clone();
        size.size += 1;
        let mut material = base.clone();
        material.material += 1;
        let mut carried = base.clone();
        carried.carried[Element::Wood] += 1;
        let mut items = base.clone();
        items.items.push(Item { element: Element::Wood, quantity: 1 });
        let mut action_ready_at = base.clone();
        action_ready_at.action_ready_at[0] += 1;

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
            ("size", size),
            ("material", material),
            ("carried", carried),
            ("items", items),
            ("action_ready_at", action_ready_at),
        ] {
            assert_ne!(hash_of(&variant), base_hash, "{name} does not affect the hash");
        }
    }

    // S3.9: `items`'s length must affect the hash even when every item it
    // does hold is identical between the two sides — a naive "hash whatever
    // `items` contains" implementation that folded items together without a
    // length prefix could let a 2-item and a 3-item inventory of the same
    // repeated item collide. Regression-shaped, not just coverage.
    #[test]
    fn item_count_affects_the_hash_even_with_identical_items() {
        let f = attrs(animal(Element::Fire));
        let mut two = Entity::spawn(1, Element::Fire, V2::ZERO, 5, 0, f);
        two.items.push(Item { element: Element::Wood, quantity: 7 });
        two.items.push(Item { element: Element::Wood, quantity: 7 });
        let mut three = two.clone();
        three.items.push(Item { element: Element::Wood, quantity: 7 });

        let hash_of = |e: &Entity| {
            let mut h = Hasher::new();
            e.hash_into(&mut h);
            h.finish()
        };
        assert_ne!(hash_of(&two), hash_of(&three));
    }

    // S3.5: `grown_size` is the pure function `Entity.size` is recomputed
    // from every tick (`World::phase_aging`) -- covered here in isolation,
    // separately from the mechanism test in `world.rs` that proves
    // `phase_aging` actually calls it.
    #[test]
    fn grown_size_starts_at_birth_size() {
        assert_eq!(grown_size(200, 0, 4000, 1000), 200);
    }

    #[test]
    fn grown_size_reaches_and_holds_full_size_at_maturity() {
        // maturity_age = 4000 * 250 / 1000 = 1000.
        assert_eq!(grown_size(200, 1000, 4000, 1000), 1000);
        assert_eq!(grown_size(200, 1000000, 4000, 1000), 1000, "held at 1000 past maturity");
    }

    #[test]
    fn grown_size_is_linear_at_the_midpoint() {
        // maturity_age = 4000 * 250 / 1000 = 1000; age 500 is halfway there.
        let birth = 200u64;
        let expected = (birth + (1000 - birth) * 500 / 1000) as u16;
        assert_eq!(grown_size(200, 500, 4000, 1000), expected);
    }

    #[test]
    fn grown_size_at_full_birth_size_is_constant_at_every_age() {
        for age in [0, 1, 500, 1000, 5000] {
            assert_eq!(grown_size(1000, age, 4000, 1000), 1000, "age {age}");
        }
    }

    #[test]
    fn grown_size_does_not_panic_on_zero_lifespan() {
        assert_eq!(grown_size(200, 0, 0, 1000), 1000);
        assert_eq!(grown_size(200, 5, 0, 1000), 1000);
    }

    // S3.8: growth toward a ceiling other than a hardcoded 1000.
    #[test]
    fn grown_size_stops_at_a_ceiling_below_1000() {
        // maturity_age = 4000 * 250 / 1000 = 1000.
        assert_eq!(grown_size(200, 1000, 4000, 500), 500);
        assert_eq!(grown_size(200, 1000000, 4000, 500), 500, "held at ceiling past maturity");
    }

    #[test]
    fn grown_size_never_drops_below_birth_size_when_ceiling_is_below_it() {
        assert_eq!(grown_size(600, 1000, 4000, 500), 600, "no growth room -- stays at birth size");
        assert_eq!(grown_size(600, 0, 4000, 500), 600);
    }

    #[test]
    fn growth_ceiling_scales_linearly_and_caps_at_1000() {
        assert_eq!(growth_ceiling(1000, 1000), 1000, "stock == growth_ref reaches full");
        assert_eq!(growth_ceiling(500, 1000), 500, "stock == growth_ref/2 is about half");
        assert_eq!(growth_ceiling(2000, 1000), 1000, "stock > growth_ref still caps at 1000");
        assert_eq!(growth_ceiling(0, 1000), 0, "no stock, no ceiling room");
        assert_eq!(growth_ceiling(12345, 0), 1000, "growth_ref == 0 always gives full 1000");
        assert_eq!(growth_ceiling(0, 0), 1000, "growth_ref == 0 always gives full 1000");
    }
}
