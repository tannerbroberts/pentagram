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

