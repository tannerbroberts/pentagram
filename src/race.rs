//! Race attributes.
//!
//! Every race is mechanically bespoke, but bespoke *in service of* a small set
//! of shared base attributes: how long a body lives, how fast it moves, and
//! what it does through its `actions` table.
//!
//! **The action map.** A race writes to (or draws from) terrain/carried/item/
//! ground/body material exclusively through [`ActionRecipe`] rows in
//! `RaceAttrs.actions`, dispatched by the one generic `World::apply_action_recipe`
//! function — command-triggered (`Mine`, `Smelt`, `Pickup`) or auto-fired every
//! terrain tick (`Exist`, for whatever a race does by merely persisting). There
//! is no bespoke per-mechanism struct or dispatcher anymore, and no action that
//! runs regardless of what's in the table: if a race has no `Exist` recipe, it
//! does nothing on existence, exactly as an action-less race already does
//! nothing on `Mine`. This replaced an earlier `Conversion`/`Channel`-driven
//! pipeline that ran unconditionally outside the action system — see this
//! module's git history for the retired design and the holes that rip-out
//! opened up (terrain no longer passively replenished by existence,
//! population-wide apportionment lost, cross-race terraform-pressure parity
//! lost — deliberately left as follow-up work, not solved here).

use crate::element::Element;
use crate::fx::Fx;
use crate::hash::{Hashable, Hasher};

/// Rooted vs. mobile. Every element splits into a Plant and an Animal race
/// variant (`Race` below) — ten races total via this new axis, not a
/// behavioural relabelling of the five existing races and not a re-topology
/// of the ring (`element.rs`'s mod-5 arithmetic is completely `Kind`-unaware
/// by design). Never renumber — hash/iteration-visible, same discipline as
/// `rand::Channel` (rand.rs:16-17) and `Element`'s own ring order.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
#[repr(u8)]
pub enum Kind {
    Plant = 0,
    Animal = 1,
}

impl Kind {
    pub const COUNT: usize = 2;

    pub const fn name(self) -> &'static str {
        match self {
            Kind::Plant => "Plant",
            Kind::Animal => "Animal",
        }
    }
}

/// A race is `(element, kind)` — the axis a body actually spawns as.
/// Predation/suppression/attrition relations resolve off `element` alone,
/// unchanged from before S3; race-attribute lookups (the table below,
/// governors, demand) resolve off the full `(element, kind)` pair.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Race {
    pub element: Element,
    pub kind: Kind,
}

impl Race {
    pub const COUNT: usize = Element::COUNT * Kind::COUNT;

    /// Ring order primary, a race's two variants adjacent — mirrors why
    /// `Element::ALL`'s ring order matters (Invariant IV). Index order is
    /// hash/iteration-visible and therefore wire-format-grade: never
    /// renumber.
    pub const ALL: [Race; Race::COUNT] = [
        Race { element: Element::Wood, kind: Kind::Plant },
        Race { element: Element::Wood, kind: Kind::Animal },
        Race { element: Element::Fire, kind: Kind::Plant },
        Race { element: Element::Fire, kind: Kind::Animal },
        Race { element: Element::Earth, kind: Kind::Plant },
        Race { element: Element::Earth, kind: Kind::Animal },
        Race { element: Element::Metal, kind: Kind::Plant },
        Race { element: Element::Metal, kind: Kind::Animal },
        Race { element: Element::Water, kind: Kind::Plant },
        Race { element: Element::Water, kind: Kind::Animal },
    ];

    #[inline]
    pub const fn index(self) -> usize {
        self.element.index() * Kind::COUNT + self.kind as usize
    }
}

/// A value per race. Mirrors [`crate::element::PerElement`] member for
/// member — see that type's own doc comment. Kept as a distinct type (not `PerElement<[T;
/// 2]>`) so *per-element* things (terrain layers, `TerrainTuning`) stay
/// visibly distinct from *per-race* things (governors, demand, `RaceAttrs`
/// itself) — that distinction is the entire point of the `Kind` axis.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PerRace<T>(pub [T; Race::COUNT]);

impl<T> PerRace<T> {
    #[inline]
    pub fn get(&self, r: Race) -> &T {
        &self.0[r.index()]
    }

    #[inline]
    pub fn get_mut(&mut self, r: Race) -> &mut T {
        &mut self.0[r.index()]
    }

    /// Always yields races in `Race::ALL`'s fixed order.
    pub fn iter(&self) -> impl Iterator<Item = (Race, &T)> {
        Race::ALL.into_iter().zip(self.0.iter())
    }
}

impl<T: Copy> PerRace<T> {
    pub fn filled(v: T) -> PerRace<T> {
        PerRace([v; Race::COUNT])
    }
}

impl<T> core::ops::Index<Race> for PerRace<T> {
    type Output = T;
    #[inline]
    fn index(&self, r: Race) -> &T {
        &self.0[r.index()]
    }
}

impl<T> core::ops::IndexMut<Race> for PerRace<T> {
    #[inline]
    fn index_mut(&mut self, r: Race) -> &mut T {
        &mut self.0[r.index()]
    }
}

/// Sim ticks per second is 1 / 0.6, so a minute is 100 ticks.
pub const TICKS_PER_MINUTE: u64 = 100;
pub const TICKS_PER_HOUR: u64 = TICKS_PER_MINUTE * 60;
pub const TICKS_PER_DAY: u64 = TICKS_PER_HOUR * 24;

/// One terrain tick per 100 sim ticks — one simulated minute.
pub const TERRAIN_PERIOD: u64 = 100;

/// A hard-bounded rate, per terrain tick, per race, per territory. Still used
/// wherever a *population-aggregate*, governor-smoothed budget is wanted
/// (e.g. `TerrainTuning::ground_decay`) — no longer part of `RaceAttrs`
/// itself, since the per-race demand pipeline this served (`consume`/
/// `consume_mix`/`Channel`) was retired along with `Conversion`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RateBand {
    /// The portion of the banked bucket one terrain tick may never spend
    /// below (see [`Governor::settle`](crate::governor::Governor::settle)).
    pub floor: u32,
    /// The long-run average under sustained demand. Accrues into the burst bucket.
    pub nominal: u32,
    /// Never exceeded in a single terrain tick, under any behaviour.
    pub ceiling: u32,
    /// How many terrain ticks of `nominal` may be banked for a burst.
    pub burst_ticks: u32,
}

impl RateBand {
    pub const fn new(floor: u32, nominal: u32, ceiling: u32, burst_ticks: u32) -> RateBand {
        RateBand { floor, nominal, ceiling, burst_ticks }
    }

    #[inline]
    pub const fn burst_cap(&self) -> u64 {
        (self.nominal as u64) * (self.burst_ticks as u64)
    }

    pub fn is_valid(&self) -> bool {
        self.floor <= self.nominal && self.nominal <= self.ceiling
    }

    /// Move one edge of the band and drag the others along as needed, so
    /// `floor <= nominal <= ceiling` survives any single edit. A band that is
    /// briefly inverted would grant above its own ceiling, which is exactly the
    /// guarantee the governor exists to make.
    pub fn set_edge(&mut self, edge: Edge, v: u32) {
        match edge {
            Edge::Floor => {
                self.floor = v;
                self.nominal = self.nominal.max(self.floor);
                self.ceiling = self.ceiling.max(self.nominal);
            }
            Edge::Nominal => {
                self.nominal = v;
                self.floor = self.floor.min(self.nominal);
                self.ceiling = self.ceiling.max(self.nominal);
            }
            Edge::Ceiling => {
                self.ceiling = v;
                self.nominal = self.nominal.min(self.ceiling);
                self.floor = self.floor.min(self.nominal);
            }
        }
    }
}

/// Which edge of a [`RateBand`] an edit is moving.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Edge {
    Floor,
    Nominal,
    Ceiling,
}

/// Which of an entity's material pools an [`ActionRecipe`] hop reads from or
/// writes to. The element itself is a runtime parameter (the command's own
/// `element` field, or the acting race's own element for `Exist`), not baked
/// into the slot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RecipeSlot {
    /// The terrain cell this entity currently occupies.
    Terrain,
    /// This entity's own `Entity.carried` stock.
    Carried,
    /// A bundled `Item` in this entity's `Entity.items`, matched by element.
    Item,
    /// `World::ground_items`, filtered by element and by the recipe's own
    /// `reach` of the acting entity's position. Only meaningful as an input.
    Ground,
    /// This entity's own `Entity.material` (body mass).
    Body,
}

/// How a recipe's output element relates to its input element.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ElementTransform {
    /// Output element == input element (Mine, Pickup, Exist, a wear roll).
    Identity,
    /// Output element == input element.generates() (Smelt).
    Generates,
}

/// A hard cap, per firing, on how many input units a recipe may draw —
/// evaluated fresh every firing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RateLaw {
    /// A flat ceiling, independent of local conditions.
    Flat(u16),
    /// Scales with same-race crowding at the firing entity's cell and with
    /// the entity's own `size`: `base + per_neighbor * (crowd - 1) +
    /// per_size * size / 1000`. No shipped race uses this yet — the
    /// structural proof a "fast-replicating action" fits this table.
    NeighborScaled { base: u16, per_neighbor: u16, per_size: u16 },
}

/// Which action a race's recipe answers to — the table's own
/// self-description, so `RaceAttrs.actions` doesn't depend on a Vec-position
/// convention. Never renumber — hash-visible, same discipline as `Channel`
/// used to require.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
#[repr(u8)]
pub enum ActionSlot {
    /// Auto-fired once per terrain tick for every living body of this race,
    /// directly from `World::phase_terrain` — no command involved. The
    /// replacement for the old unconditional `Conversion` mechanism: a race
    /// with no `Exist` recipe simply does nothing on existence.
    Exist = 0,
    Mine = 1,
    Smelt = 2,
    Pickup = 3,
    /// A wear roll (`Item(X) -> Terrain(X)`, 1:1) — wired to no shipped race
    /// and no command yet, the structural proof durability fits this model.
    WearRoll = 4,
}

impl ActionSlot {
    pub const COUNT: usize = 5;
}

/// One input→output hop a race can perform. The single generalization every
/// command-triggered action (`Mine`, `Smelt`, `Pickup`) and the one
/// auto-fired action (`Exist`) are expressed through — see this module's own
/// doc comment.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ActionRecipe {
    pub slot: ActionSlot,
    pub input: RecipeSlot,
    pub output: RecipeSlot,
    pub transform: ElementTransform,
    /// Input units consumed per whole batch.
    pub ratio_in: u32,
    /// Output units produced per whole batch. Never exceeds `ratio_in` — a
    /// recipe cannot manufacture mass.
    pub ratio_out: u32,
    pub rate: RateLaw,
    /// Ticks after a firing before this entity may trigger this recipe
    /// again. `0` fires every tick/every command it is asked to.
    pub cooldown_ticks: u16,
    /// Reach for a `Ground`-sourced recipe (`Pickup`); ignored otherwise —
    /// conventionally `Fx::ZERO` on every other recipe.
    pub reach: Fx,
}

impl ActionRecipe {
    pub fn is_valid(&self) -> bool {
        self.ratio_in > 0 && self.ratio_out > 0 && self.ratio_out <= self.ratio_in
    }
}

impl Hashable for ActionRecipe {
    fn hash_into(&self, h: &mut Hasher) {
        h.u8(self.slot as u8)
            .u8(self.input as u8)
            .u8(self.output as u8)
            .u8(self.transform as u8)
            .u32(self.ratio_in)
            .u32(self.ratio_out);
        match self.rate {
            RateLaw::Flat(n) => {
                h.u8(0).u16(n);
            }
            RateLaw::NeighborScaled { base, per_neighbor, per_size } => {
                h.u8(1).u16(base).u16(per_neighbor).u16(per_size);
            }
        }
        h.u16(self.cooldown_ticks).i32(self.reach.raw());
    }
}

/// The full attribute set for one race.
#[derive(Clone, PartialEq, Debug)]
pub struct RaceAttrs {
    pub element: Element,
    pub kind: Kind,

    /// Natural lifespan in sim ticks, before variance.
    pub lifespan: u64,
    /// Per-mille variance applied to lifespan per individual, so a cohort born
    /// together does not die together.
    pub lifespan_variance: u16,

    /// Cells travelled per sim tick.
    ///
    /// This is an **ecology** knob, not a feel knob. Mobility is the parameter
    /// that decides whether five biomes coexist in rotating spiral domains or
    /// collapse into a single survivor (§4.1) — there is a critical threshold
    /// and it is not where intuition puts it. Change this expecting the whole
    /// world to reorganise.
    pub speed: Fx,
    /// Collision radius in cells.
    pub radius: Fx,

    /// This race's action map — every way it writes to, or draws from,
    /// terrain/carried/item/ground/body material. See this module's own doc
    /// comment. `Exist` recipes fire automatically every terrain tick;
    /// everything else is command-triggered.
    pub actions: Vec<ActionRecipe>,

    /// One-line statement of the fantasy the numbers above are serving.
    pub fantasy: &'static str,
}

impl RaceAttrs {
    /// One meal per this many ticks — still read by `EcologyTuning::default`
    /// for its satiation default, independent of the (now-retired) consume
    /// pipeline this constant originally spread.
    pub const FEED_PERIOD: u64 = 200;

    pub fn is_valid(&self) -> bool {
        self.lifespan > 0 && self.actions.iter().all(|a| a.is_valid()) && self.body_is_valid()
    }

    /// The kind-aware replacement for a universal "speed is always positive"
    /// assumption: an Animal must actually move, a Plant must actually not —
    /// `phase_movement`'s structural skip for `Kind::Plant` (`world.rs`) only
    /// makes sense if the table itself is honest about which rows are rooted.
    /// Both kinds still need a body that crowds neighbours, so `radius` is
    /// unconditional. See `docs/S3_ECOLOGY_LAYERS_DESIGN.md` §4.
    fn body_is_valid(&self) -> bool {
        let speed_ok = match self.kind {
            Kind::Animal => self.speed > Fx::ZERO,
            Kind::Plant => self.speed == Fx::ZERO,
        };
        // A Plant is rooted and can never mine or smelt (a deliberate act,
        // not passive existence) — an Animal row is free to ship neither too,
        // that's just "doesn't do this today," a valid tuning choice, so only
        // the Plant side is a hard structural rule.
        let mining_ok = self.kind == Kind::Animal || self.action(ActionSlot::Mine).is_none();
        let smelt_ok = self.kind == Kind::Animal || self.action(ActionSlot::Smelt).is_none();
        speed_ok && self.radius > Fx::ZERO && mining_ok && smelt_ok
    }

    /// The first (and, by convention, only) recipe this race has for `slot`,
    /// or `None` if it has none — the "no action, no effect" rule every
    /// dispatcher call goes through.
    pub fn action(&self, slot: ActionSlot) -> Option<&ActionRecipe> {
        self.actions.iter().find(|a| a.slot == slot)
    }

    /// Mutable counterpart to `action` — the live-tuning knob table's way of
    /// reaching into a specific recipe. `None` for a race with no recipe in
    /// this slot; a knob's setter is then correctly a no-op rather than
    /// inventing a new recipe row.
    pub fn action_mut(&mut self, slot: ActionSlot) -> Option<&mut ActionRecipe> {
        self.actions.iter_mut().find(|a| a.slot == slot)
    }
}

/// The table. Every number here is a knob, and every one is meant to be moved.
///
/// **Action-map migration.** Each row's old `conversion: Conversion::new(ratio_in,
/// ratio_out, deposit_share, body_share, waste_share)` — a population-wide,
/// unconditional-on-existence mechanism — becomes an `Exist` `ActionRecipe`:
/// `ratio_in`/`ratio_out` carry over unchanged (same batch math), but
/// `deposit_share`/`waste_share` do not — there is no longer a second
/// destination to split into, every batch's produced output credits
/// `Entity.material` in full. This is a deliberate, documented hole (terrain
/// is no longer passively replenished by existence — see the implementation
/// plan/PR description), not an oversight. Each row's `Exist.rate` is seeded
/// from its old `consume.nominal` (a population-aggregate number, now reread
/// as a per-entity, per-terrain-tick cap) — exactly as provisional a
/// first-guess as `mining_rate: 40` already was for every Animal row below.
pub const RACES: PerRace<RaceAttrs> = PerRace([
    // ---------------------------------------------------------------- WOOD
    RaceAttrs {
        element: Element::Wood,
        kind: Kind::Plant,
        lifespan: TICKS_PER_HOUR * 15 / 2, // 7.5 hours — 3x its Animal twin
        lifespan_variance: 180,
        speed: Fx::ZERO,
        radius: Fx::ratio(60, 100),
        actions: Vec::new(), // filled in by `seed_actions` below — see doc comment
        fantasy: "Grows where it stands and draws through its roots — the map remembers a thicket long after any single stem is gone.",
    },
    RaceAttrs {
        element: Element::Wood,
        kind: Kind::Animal,
        lifespan: TICKS_PER_HOUR * 5 / 2, // 2.5 hours
        lifespan_variance: 180,
        speed: Fx::ratio(9, 100),
        radius: Fx::ratio(60, 100),
        actions: Vec::new(),
        fantasy: "Ranges the thicket for a meal — what it takes in shapes the ground it leaves behind, no roots required to leave a mark.",
    },
    // ---------------------------------------------------------------- FIRE
    RaceAttrs {
        element: Element::Fire,
        kind: Kind::Plant,
        lifespan: TICKS_PER_MINUTE * 24, // 24 minutes — 3x its Animal twin
        lifespan_variance: 300,
        speed: Fx::ZERO,
        radius: Fx::ratio(40, 100),
        actions: Vec::new(),
        fantasy: "Ember-moss, smouldering low and constant — it never dies spectacularly, it just keeps glowing where it took root.",
    },
    RaceAttrs {
        element: Element::Fire,
        kind: Kind::Animal,
        lifespan: TICKS_PER_MINUTE * 8, // 8 minutes
        lifespan_variance: 300,
        speed: Fx::ratio(46, 100),
        radius: Fx::ratio(40, 100),
        actions: Vec::new(),
        fantasy: "Born from a lightning strike, burns hot, dies fast, and the ground remembers.",
    },
    // --------------------------------------------------------------- EARTH
    RaceAttrs {
        element: Element::Earth,
        kind: Kind::Plant,
        lifespan: TICKS_PER_DAY * 42, // 42 days — 3x its Animal twin
        lifespan_variance: 120,
        speed: Fx::ZERO,
        radius: Fx::ratio(150, 100),
        actions: Vec::new(),
        fantasy: "Older than the animal that shares its name — patience that outlasts even Earth's own long-lived fauna, terraforming by simply persisting the longest.",
    },
    RaceAttrs {
        element: Element::Earth,
        kind: Kind::Animal,
        lifespan: TICKS_PER_DAY * 14, // a fortnight
        lifespan_variance: 120,
        speed: Fx::ratio(4, 100),
        radius: Fx::ratio(150, 100),
        actions: Vec::new(),
        fantasy: "Ancient. Lives for a fortnight and terraforms by refusing to move.",
    },
    // --------------------------------------------------------------- METAL
    RaceAttrs {
        element: Element::Metal,
        kind: Kind::Plant,
        lifespan: TICKS_PER_HOUR * 36, // 36 hours — 3x its Animal twin
        lifespan_variance: 90,
        speed: Fx::ZERO,
        radius: Fx::ratio(55, 100),
        actions: Vec::new(),
        fantasy: "An ore-vein growth, embedded and slow — it forges nothing, it just sits in the seam and the seam changes around it.",
    },
    RaceAttrs {
        element: Element::Metal,
        kind: Kind::Animal,
        lifespan: TICKS_PER_HOUR * 12,
        lifespan_variance: 90,
        speed: Fx::ratio(21, 100),
        radius: Fx::ratio(55, 100),
        actions: Vec::new(),
        fantasy: "Precise and scheduled. Writes to the world only at the moment of forging.",
    },
    // --------------------------------------------------------------- WATER
    RaceAttrs {
        element: Element::Water,
        kind: Kind::Plant,
        lifespan: TICKS_PER_MINUTE * 105, // 105 minutes — 3x its Animal twin
        lifespan_variance: 220,
        speed: Fx::ZERO,
        radius: Fx::ratio(50, 100),
        actions: Vec::new(),
        fantasy: "A reed-bed, rooted in the shallows — where the animal current tears through, this just sits and slowly silts the bank.",
    },
    RaceAttrs {
        element: Element::Water,
        kind: Kind::Animal,
        lifespan: TICKS_PER_MINUTE * 35,
        lifespan_variance: 220,
        speed: Fx::ratio(33, 100),
        radius: Fx::ratio(50, 100),
        actions: Vec::new(),
        fantasy: "Rhythmic and tidal. Terraforms by flowing, and stops when it stops.",
    },
]);

/// `RACES`'s const-context `actions: Vec::new()` placeholders can't hold a
/// `Vec` literal in a `const` (allocation isn't const-evaluable in stable
/// Rust) — this seeds the real per-row action tables once, lazily, behind
/// [`SEEDED_RACES`]. `attrs`/`seeded_races` both go through that rather than
/// the raw `RACES` const directly.
fn seed_actions(mut a: RaceAttrs) -> RaceAttrs {
    // Exist ratio_in/ratio_out/rate per row — see `RACES`'s own doc comment
    // for how these were derived from the retired `Conversion`/`consume`
    // pipeline. `habitat = element.eats()`, drawn at the entity's own cell.
    let (ratio_in, ratio_out, nominal) = match (a.element, a.kind) {
        (Element::Wood, Kind::Plant) => (1, 1, 900),
        (Element::Wood, Kind::Animal) => (1, 1, 900),
        (Element::Fire, Kind::Plant) => (1000, 477, 1100),
        (Element::Fire, Kind::Animal) => (1000, 477, 1100),
        (Element::Earth, Kind::Plant) => (1, 1, 850),
        (Element::Earth, Kind::Animal) => (1, 1, 850),
        (Element::Metal, Kind::Plant) => (50, 1, 950),
        (Element::Metal, Kind::Animal) => (50, 1, 950),
        (Element::Water, Kind::Plant) => (1000, 742, 1000),
        (Element::Water, Kind::Animal) => (1000, 742, 1000),
    };
    a.actions.push(ActionRecipe {
        slot: ActionSlot::Exist,
        input: RecipeSlot::Terrain,
        output: RecipeSlot::Body,
        // Draws `element.habitat()` (passed in by the caller — see
        // `World::phase_terrain`) and produces the race's own element:
        // `habitat().generates() == self.element` (habitat is one ring-step
        // back, generates is one ring-step forward — they cancel).
        transform: ElementTransform::Generates,
        ratio_in,
        ratio_out,
        rate: RateLaw::Flat(nominal),
        cooldown_ticks: 0,
        reach: Fx::ZERO,
    });
    if a.kind == Kind::Animal {
        a.actions.push(ActionRecipe {
            slot: ActionSlot::Mine,
            input: RecipeSlot::Terrain,
            output: RecipeSlot::Carried,
            transform: ElementTransform::Identity,
            ratio_in: 1,
            ratio_out: 1,
            rate: RateLaw::Flat(40), // first-guess, uniform across every Animal row
            cooldown_ticks: 0,
            reach: Fx::ZERO,
        });
        a.actions.push(ActionRecipe {
            slot: ActionSlot::Smelt,
            input: RecipeSlot::Carried,
            output: RecipeSlot::Carried,
            transform: ElementTransform::Generates,
            ratio_in: 50,
            ratio_out: 1,
            rate: RateLaw::Flat(u16::MAX), // "convert everything held" — see caveat in RaceAttrs::action's callers
            cooldown_ticks: 0,
            reach: Fx::ZERO,
        });
    }
    a
}

/// `RACES` with every row's `actions` populated — computed once, lazily
/// (`Vec::push` isn't const-evaluable, so this can't happen inside `RACES`
/// itself). Kept behind a `LazyLock` rather than reseeding on every call so
/// `attrs` stays the zero-allocation, `&'static RaceAttrs`-returning
/// function every call site already expects.
static SEEDED_RACES: std::sync::LazyLock<PerRace<RaceAttrs>> =
    std::sync::LazyLock::new(|| PerRace(RACES.0.clone().map(seed_actions)));

/// The shipped table — the starting point every `World` is tuned away from.
pub fn attrs(r: Race) -> &'static RaceAttrs {
    &SEEDED_RACES.0[r.index()]
}

/// An owned copy of the full, action-seeded table — what `World::new` stores
/// so each `World` can retune its own copy independently of every other.
pub fn seeded_races() -> PerRace<RaceAttrs> {
    SEEDED_RACES.clone()
}

// The tuning table is part of simulation state the moment it becomes runtime
// data, so it has to be hashable: a world with a retuned knob must not compare
// equal to one without it.

impl Hashable for RateBand {
    fn hash_into(&self, h: &mut Hasher) {
        h.u32(self.floor)
            .u32(self.nominal)
            .u32(self.ceiling)
            .u32(self.burst_ticks);
    }
}

impl Hashable for RaceAttrs {
    fn hash_into(&self, h: &mut Hasher) {
        h.u8(self.element as u8)
            .u8(self.kind as u8)
            .u64(self.lifespan)
            .u16(self.lifespan_variance)
            .i32(self.speed.raw())
            .i32(self.radius.raw());
        h.u32(self.actions.len() as u32);
        for a in &self.actions {
            a.hash_into(h);
        }
    }
}

