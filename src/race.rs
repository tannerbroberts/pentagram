//! Race attributes.
//!
//! Every race is mechanically bespoke, but bespoke *in service of* a small set
//! of shared base attributes. Three rate axes carry almost all of the identity:
//!
//! - **life duration** — how long a body persists
//! - **deposition** — how fast, how burstily, and through which channel a race
//!   writes itself into the terrain
//! - **consumption** — the same three questions for taking terrain away
//!
//! The *how* is the bespoke part and lives in [`ChannelMix`]: two races can
//! terraform at an identical rate and feel nothing alike, because one does it
//! by dying and the other by standing still.
//!
//! Tempo parity rule (§3.1 of the design doc): hold `deposit_unit / lifespan`
//! roughly equal across races and no race reshapes the map faster than
//! another, while the texture stays completely distinct.

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

/// The five ways a race can write to, or take from, the terrain. Which of
/// these a race leans on is the most legible part of its identity, because it
/// determines *when* the map changes: at a funeral, at a birth, or continuously.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
#[repr(u8)]
pub enum Channel {
    /// The moment of incarnation. Fire is born from a lightning strike and the
    /// ground remembers where.
    OnBirth = 0,
    /// The corpse. Dominant for short-lived races — they terraform by dying.
    OnDeath = 1,
    /// Movement and ability use. Water erodes what it flows across.
    OnAction = 2,
    /// Feeding. Metal deposits at the moment of refining what it ate.
    OnConsume = 3,
    /// Mere presence, per body per terrain tick. Dominant for long-lived
    /// races — Earth terraforms by being there for a fortnight.
    OnExistence = 4,
}

impl Channel {
    pub const COUNT: usize = 5;
    pub const ALL: [Channel; 5] = [
        Channel::OnBirth,
        Channel::OnDeath,
        Channel::OnAction,
        Channel::OnConsume,
        Channel::OnExistence,
    ];

    #[inline]
    pub const fn index(self) -> usize {
        self as usize
    }

    pub const fn name(self) -> &'static str {
        match self {
            Channel::OnBirth => "birth",
            Channel::OnDeath => "death",
            Channel::OnAction => "action",
            Channel::OnConsume => "consume",
            Channel::OnExistence => "existence",
        }
    }
}

/// How a race's deposition is distributed across the five channels, in
/// per-mille. Must sum to exactly 1000 — checked at load, not trusted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ChannelMix([u16; Channel::COUNT]);

impl ChannelMix {
    pub const fn new(birth: u16, death: u16, action: u16, consume: u16, existence: u16) -> ChannelMix {
        ChannelMix([birth, death, action, consume, existence])
    }

    #[inline]
    pub fn permille(&self, c: Channel) -> u16 {
        self.0[c.index()]
    }

    #[inline]
    pub fn total(&self) -> u32 {
        self.0.iter().map(|v| *v as u32).sum()
    }

    /// The share of one deposit unit that flows through this channel.
    #[inline]
    pub fn share(&self, c: Channel, unit: u64) -> u64 {
        unit.saturating_mul(self.permille(c) as u64) / 1000
    }

    pub fn is_valid(&self) -> bool {
        self.total() == 1000
    }

    /// Set one channel's share and rebalance the other four proportionally so
    /// the mix still sums to exactly 1000.
    ///
    /// Editing a mix is otherwise a two-step operation with an invalid state in
    /// the middle — and a live view would happily render that intermediate lie
    /// as if it were a world you could reason about. The invariant holds after
    /// every single keystroke instead.
    pub fn set_rebalanced(&mut self, c: Channel, v: u16) {
        let i = c.index();
        let v = v.min(1000);
        let rest_target = 1000u32 - v as u32;
        let rest_now: u32 = (0..Channel::COUNT)
            .filter(|k| *k != i)
            .map(|k| self.0[k] as u32)
            .sum();
        self.0[i] = v;

        // Not a `checked_div`: the zero case is a different rebalancing rule,
        // not a fallback for a division that failed.
        #[allow(clippy::manual_checked_ops)]
        if rest_now == 0 {
            // Nothing left to scale — spread the remainder evenly.
            let each = rest_target / (Channel::COUNT as u32 - 1);
            for k in 0..Channel::COUNT {
                if k != i {
                    self.0[k] = each as u16;
                }
            }
        } else {
            for k in 0..Channel::COUNT {
                if k != i {
                    self.0[k] = (self.0[k] as u32 * rest_target / rest_now) as u16;
                }
            }
        }

        // Integer division loses a few per-mille. Give them to the largest of
        // the channels we did not touch, so the sum lands on 1000 exactly.
        let drift = 1000i32 - self.total() as i32;
        if drift != 0 {
            let k = (0..Channel::COUNT)
                .filter(|k| *k != i)
                .max_by_key(|k| self.0[*k])
                .unwrap_or(0);
            self.0[k] = (self.0[k] as i32 + drift).clamp(0, 1000) as u16;
        }
    }
}

/// A hard-bounded rate, per terrain tick, per race, per territory.
///
/// The floor is a guarantee the world makes regardless of whether the race is
/// present, played well, or extinct. The ceiling is a guarantee the world
/// makes regardless of how many players coordinate to exceed it. Between them,
/// terrain change is forecastable: the state at `T + k` is bounded before any
/// player has decided anything.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RateBand {
    /// Always emitted, even at zero demand. Guarantees the world keeps churning.
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

/// The full attribute set for one race.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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

    /// Base quantity written per channel event, before the mix is applied.
    pub deposit_unit: u64,
    pub deposit: RateBand,
    pub deposit_mix: ChannelMix,

    /// Base quantity taken per channel event.
    pub consume_unit: u64,
    pub consume: RateBand,
    pub consume_mix: ChannelMix,

    /// One-line statement of the fantasy the numbers above are serving.
    pub fantasy: &'static str,
}

impl RaceAttrs {
    pub fn is_valid(&self) -> bool {
        self.deposit.is_valid()
            && self.consume.is_valid()
            && self.deposit_mix.is_valid()
            && self.consume_mix.is_valid()
            && self.lifespan > 0
            && self.body_is_valid()
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
        speed_ok && self.radius > Fx::ZERO
    }

    /// Tempo parity metric — deposit unit per 1000 ticks of life. Holding this
    /// roughly equal across races is what keeps a fast race from reshaping the
    /// map faster than a slow one. See §3.1.
    pub fn terraform_pressure(&self) -> u64 {
        self.deposit_unit.saturating_mul(1000) / self.lifespan.max(1)
    }

    // ------------------------------------------------------------------
    // Per-event demand.
    //
    // `deposit_unit` is the total a single body writes to the terrain over its
    // *entire life*. Each channel's per-mille share of that total is then
    // spread across however many times that channel actually fires in a life:
    // birth and death fire once, existence fires once per terrain tick,
    // actions and meals fire at their own cadence.
    //
    // This is what makes the parity rule real rather than aspirational — two
    // races with equal `deposit_unit / lifespan` write the same amount to the
    // map per unit time no matter how differently they spend it.
    //
    // All of these return **milli-units**, because a share divided across two
    // million Earth ticks underflows integer division otherwise.
    // ------------------------------------------------------------------

    /// One meal per this many ticks, used to spread the consume channel.
    pub const FEED_PERIOD: u64 = 200;

    #[inline]
    fn milli(&self, c: Channel, unit: u64, mix: &ChannelMix, divisor: u64) -> u64 {
        mix.share(c, unit).saturating_mul(MILLI) / divisor.max(1)
    }

    /// How many times a channel fires over one body's whole life. Birth and
    /// death fire once; the rest fire at their own cadence.
    #[inline]
    pub fn firings_per_life(&self, c: Channel) -> u64 {
        match c {
            Channel::OnBirth | Channel::OnDeath => 1,
            Channel::OnAction => self.lifespan,
            Channel::OnConsume => self.lifespan / Self::FEED_PERIOD,
            Channel::OnExistence => self.lifespan / TERRAIN_PERIOD,
        }
    }

    /// Milli-units of deposition contributed by one firing of `c`.
    #[inline]
    pub fn deposit_per(&self, c: Channel) -> u64 {
        self.milli(c, self.deposit_unit, &self.deposit_mix, self.firings_per_life(c))
    }

    /// Milli-units of consumption contributed by one firing of `c`.
    #[inline]
    pub fn consume_per(&self, c: Channel) -> u64 {
        self.milli(c, self.consume_unit, &self.consume_mix, self.firings_per_life(c))
    }
}

/// Fixed-point scale for demand accumulation. Demand is summed in milli-units
/// and divided down once, at settlement.
pub const MILLI: u64 = 1000;

/// The table. Every number here is a knob, and every one is meant to be moved.
///
/// **S3.2: real, distinct numbers.** Every Plant row below is now genuinely
/// its own row, not a copy of its Animal twin. Four deliberate design moves
/// produced them, all traceable back to `docs/S3_ECOLOGY_LAYERS_DESIGN.md`
/// §2:
///
/// - Every Plant's `speed` is `Fx::ZERO` — rooted, structurally enforced by
///   `phase_movement`'s `Kind::Plant` skip (`world.rs`), not just an unread
///   number on the row.
/// - Every Plant's `lifespan` is exactly 3x its Animal twin's (unchanged)
///   lifespan. A uniform linear scale preserves both the Fire<Water<Wood
///   <Metal<Earth ordering and the >2000x fire-to-earth spread *within* the
///   Plant kind for free.
/// - Every Animal's `deposit_unit`/`consume_unit` is exactly half its old
///   (pre-Kind-split) value; every Plant's is exactly 1.5x that same old
///   value. Combined with the 3x lifespan multiple, each row's
///   `terraform_pressure()` lands at almost exactly half the original
///   per-element figure, so a Plant+Animal pair's *combined* pressure comes
///   out within about 1 part in 250 of the old single-race baseline for that
///   element — the per-element tempo budget is *split* between the two
///   kinds, not carried twice over (see §3's consequence note on the
///   `apportion` floor doubling). All ten rows individually still cluster
///   comfortably inside the existing 2x parity band.
/// - Every Plant's `deposit_mix` is redesigned so `OnExistence` is strictly
///   its largest channel — rooted, terraforms by merely persisting, the
///   plant archetype. Every Animal's mixes are unchanged, with one deliberate
///   exception: Wood-Animal's `deposit_mix` moves off `OnExistence` onto
///   `OnConsume`, because Wood-Plant now owns the existence-dominant slot for
///   Wood and a mobile, speed-0.09 animal being "grows in place" no longer
///   made sense once it has a rooted sibling.
///
/// Radius and `lifespan_variance` are unchanged (shared by both kinds of an
/// element) — nothing in this design needs them to differ per kind. See
/// `docs/S3_ECOLOGY_LAYERS_DESIGN.md` §2, §12 for the full account.
pub const RACES: PerRace<RaceAttrs> = PerRace([
    // ---------------------------------------------------------------- WOOD
    RaceAttrs {
        element: Element::Wood,
        kind: Kind::Plant,
        lifespan: TICKS_PER_HOUR * 15 / 2, // 7.5 hours — 3x its Animal twin
        lifespan_variance: 180,
        speed: Fx::ZERO,
        radius: Fx::ratio(60, 100),
        deposit_unit: 57_000,
        deposit: RateBand::new(500, 1000, 2000, 6),
        deposit_mix: ChannelMix::new(80, 120, 40, 180, 580),
        consume_unit: 33_000,
        consume: RateBand::new(300, 900, 1800, 6),
        consume_mix: ChannelMix::new(30, 70, 30, 120, 750),
        fantasy: "Grows where it stands and draws through its roots — the map remembers a thicket long after any single stem is gone.",
    },
    RaceAttrs {
        element: Element::Wood,
        kind: Kind::Animal,
        lifespan: TICKS_PER_HOUR * 5 / 2, // 2.5 hours
        lifespan_variance: 180,
        speed: Fx::ratio(9, 100),
        radius: Fx::ratio(60, 100),
        deposit_unit: 19_000,
        deposit: RateBand::new(500, 1000, 2000, 6),
        // No longer existence-dominant now Wood-Plant owns that slot —
        // terraforms by what it takes in, not by standing around.
        deposit_mix: ChannelMix::new(100, 150, 100, 450, 200),
        consume_unit: 11_000,
        consume: RateBand::new(300, 900, 1800, 6),
        consume_mix: ChannelMix::new(50, 50, 100, 500, 300),
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
        deposit_unit: 3_150,
        deposit: RateBand::new(200, 1000, 6000, 20),
        deposit_mix: ChannelMix::new(50, 150, 50, 50, 700),
        consume_unit: 6_600,
        consume: RateBand::new(150, 1100, 7000, 20),
        consume_mix: ChannelMix::new(20, 60, 20, 100, 800),
        fantasy: "Ember-moss, smouldering low and constant — it never dies spectacularly, it just keeps glowing where it took root.",
    },
    RaceAttrs {
        element: Element::Fire,
        kind: Kind::Animal,
        lifespan: TICKS_PER_MINUTE * 8, // 8 minutes
        lifespan_variance: 300,
        speed: Fx::ratio(46, 100),
        radius: Fx::ratio(40, 100),
        deposit_unit: 1_050,
        // Spikiest band in the table: can go quiet, then spike sixfold.
        deposit: RateBand::new(200, 1000, 6000, 20),
        // Terraforms almost entirely by dying. A fire creature's corpse is the
        // scorch mark.
        deposit_mix: ChannelMix::new(150, 700, 100, 50, 0),
        consume_unit: 2_200,
        consume: RateBand::new(150, 1100, 7000, 20),
        consume_mix: ChannelMix::new(0, 100, 400, 500, 0),
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
        deposit_unit: 7_650_000,
        deposit: RateBand::new(800, 1000, 1400, 2),
        deposit_mix: ChannelMix::new(40, 40, 40, 80, 800),
        consume_unit: 3_600_000,
        consume: RateBand::new(600, 850, 1200, 2),
        consume_mix: ChannelMix::new(10, 20, 20, 50, 900),
        fantasy: "Older than the animal that shares its name — patience that outlasts even Earth's own long-lived fauna, terraforming by simply persisting the longest.",
    },
    RaceAttrs {
        element: Element::Earth,
        kind: Kind::Animal,
        lifespan: TICKS_PER_DAY * 14, // a fortnight
        lifespan_variance: 120,
        speed: Fx::ratio(4, 100),
        radius: Fx::ratio(150, 100),
        deposit_unit: 2_550_000,
        // Nearly a metronome. Barely bursts, barely rests.
        deposit: RateBand::new(800, 1000, 1400, 2),
        // Presence is the mechanism. An Earth body presses on the world simply
        // by continuing to be somewhere.
        deposit_mix: ChannelMix::new(50, 50, 50, 100, 750),
        consume_unit: 1_200_000,
        consume: RateBand::new(600, 850, 1200, 2),
        consume_mix: ChannelMix::new(0, 50, 50, 200, 700),
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
        deposit_unit: 273_000,
        deposit: RateBand::new(250, 1000, 2500, 12),
        deposit_mix: ChannelMix::new(100, 100, 150, 150, 500),
        consume_unit: 144_000,
        consume: RateBand::new(200, 950, 2400, 12),
        consume_mix: ChannelMix::new(60, 60, 100, 180, 600),
        fantasy: "An ore-vein growth, embedded and slow — it forges nothing, it just sits in the seam and the seam changes around it.",
    },
    RaceAttrs {
        element: Element::Metal,
        kind: Kind::Animal,
        lifespan: TICKS_PER_HOUR * 12,
        lifespan_variance: 90,
        speed: Fx::ratio(21, 100),
        radius: Fx::ratio(55, 100),
        deposit_unit: 91_000,
        deposit: RateBand::new(250, 1000, 2500, 12),
        // Deposits at the moment of refining. Nothing from mere existence —
        // Metal that is not working leaves no trace at all.
        deposit_mix: ChannelMix::new(200, 150, 250, 400, 0),
        consume_unit: 48_000,
        consume: RateBand::new(200, 950, 2400, 12),
        consume_mix: ChannelMix::new(100, 50, 250, 600, 0),
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
        deposit_unit: 13_350,
        deposit: RateBand::new(300, 1000, 3000, 10),
        deposit_mix: ChannelMix::new(50, 150, 150, 150, 500),
        consume_unit: 18_000,
        consume: RateBand::new(250, 1000, 3200, 10),
        consume_mix: ChannelMix::new(20, 80, 100, 150, 650),
        fantasy: "A reed-bed, rooted in the shallows — where the animal current tears through, this just sits and slowly silts the bank.",
    },
    RaceAttrs {
        element: Element::Water,
        kind: Kind::Animal,
        lifespan: TICKS_PER_MINUTE * 35,
        lifespan_variance: 220,
        speed: Fx::ratio(33, 100),
        radius: Fx::ratio(50, 100),
        deposit_unit: 4_450,
        deposit: RateBand::new(300, 1000, 3000, 10),
        // Erodes what it moves across. Action-dominant: standing still, Water
        // does almost nothing.
        deposit_mix: ChannelMix::new(50, 250, 500, 150, 50),
        consume_unit: 6_000,
        consume: RateBand::new(250, 1000, 3200, 10),
        consume_mix: ChannelMix::new(0, 100, 550, 300, 50),
        fantasy: "Rhythmic and tidal. Terraforms by flowing, and stops when it stops.",
    },
]);

/// The shipped table — the starting point every `World` is tuned away from.
#[inline]
pub fn attrs(r: Race) -> &'static RaceAttrs {
    &RACES.0[r.index()]
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

impl Hashable for ChannelMix {
    fn hash_into(&self, h: &mut Hasher) {
        for v in self.0 {
            h.u16(v);
        }
    }
}

impl Hashable for RaceAttrs {
    fn hash_into(&self, h: &mut Hasher) {
        h.u8(self.element as u8)
            .u8(self.kind as u8)
            .u64(self.lifespan)
            .u16(self.lifespan_variance)
            .i32(self.speed.raw())
            .i32(self.radius.raw())
            .u64(self.deposit_unit)
            .u64(self.consume_unit);
        self.deposit.hash_into(h);
        self.deposit_mix.hash_into(h);
        self.consume.hash_into(h);
        self.consume_mix.hash_into(h);
    }
}

/// The one specific race every narrative test below reaches for when it
/// needs "the" row for an element's Animal variant rather than every row.
/// Since S3.2, `Kind::Animal` is a genuine, deliberate choice of which row a
/// given test examines — Plant and Animal rows are numerically distinct now
/// (see `RACES`'s own doc comment) — not an "it doesn't matter which"
/// shortcut left over from the S3.1 scaffold.
#[cfg(test)]
fn animal(e: Element) -> Race {
    Race { element: e, kind: Kind::Animal }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_race_is_internally_consistent() {
        for race in Race::ALL {
            let a = attrs(race);
            assert_eq!(a.element, race.element, "table row is filed under the wrong element");
            assert_eq!(a.kind, race.kind, "table row is filed under the wrong kind");
            assert!(a.is_valid(), "{}-{} has an invalid attribute set", race.element.name(), race.kind.name());
        }
    }

    #[test]
    fn every_mix_sums_to_one_thousand() {
        for race in Race::ALL {
            let a = attrs(race);
            assert_eq!(a.deposit_mix.total(), 1000, "{}-{} deposit mix", race.element.name(), race.kind.name());
            assert_eq!(a.consume_mix.total(), 1000, "{}-{} consume mix", race.element.name(), race.kind.name());
        }
    }

    #[test]
    fn every_band_has_a_nonzero_floor() {
        // The churn guarantee. A race with a zero floor can leave a region
        // frozen, which is how a dead server happens.
        for race in Race::ALL {
            let a = attrs(race);
            assert!(a.deposit.floor > 0, "{}-{} deposit floor is zero", race.element.name(), race.kind.name());
            assert!(a.consume.floor > 0, "{}-{} consume floor is zero", race.element.name(), race.kind.name());
        }
    }

    #[test]
    fn lifespans_span_the_intended_three_orders_of_magnitude() {
        // Holds independently within each Kind: Plant lifespans are a
        // uniform 3x scale of Animal's, which preserves the ratio exactly.
        for kind in [Kind::Plant, Kind::Animal] {
            let fire = attrs(Race { element: Element::Fire, kind }).lifespan;
            let earth = attrs(Race { element: Element::Earth, kind }).lifespan;
            assert!(earth / fire > 2000, "{:?}: ratio is only {}", kind, earth / fire);
        }
    }

    #[test]
    fn lifespans_are_strictly_ordered_fire_to_earth() {
        let order = [
            Element::Fire,
            Element::Water,
            Element::Wood,
            Element::Metal,
            Element::Earth,
        ];
        // Holds independently within each Kind — Plant lifespans are a
        // uniform 3x scale of Animal's, which preserves ordering.
        for kind in [Kind::Plant, Kind::Animal] {
            for w in order.windows(2) {
                assert!(
                    attrs(Race { element: w[0], kind }).lifespan < attrs(Race { element: w[1], kind }).lifespan,
                    "{:?}: {} should be shorter-lived than {}",
                    kind,
                    w[0].name(),
                    w[1].name()
                );
            }
        }
    }

    #[test]
    fn plants_outlive_the_animal_of_their_own_element() {
        // Long-lived-and-rooted vs. short-lived-and-mobile is close to the
        // point of the split, so it should be a test, not a hope.
        for e in Element::ALL {
            let plant = attrs(Race { element: e, kind: Kind::Plant }).lifespan;
            let animal = attrs(Race { element: e, kind: Kind::Animal }).lifespan;
            assert!(plant > animal, "{}: plant lifespan {} should exceed animal lifespan {}", e.name(), plant, animal);
        }
    }

    #[test]
    fn terraform_pressure_is_within_parity_band() {
        // §3.1: deposit_unit / lifespan roughly equal across races, so no race
        // reshapes the map faster than another. Allow a 2x spread. This no
        // longer trivially holds now that Plant rows carry real, distinct
        // numbers (S3.2) — it holds because the ten rows were deliberately
        // designed to keep every one of them within a 2x band, per
        // `RACES`'s own doc comment.
        let ps: Vec<u64> = Race::ALL.iter().map(|r| attrs(*r).terraform_pressure()).collect();
        let lo = *ps.iter().min().unwrap();
        let hi = *ps.iter().max().unwrap();
        assert!(lo > 0, "a race has zero terraform pressure");
        assert!(
            hi <= lo * 2,
            "terraform pressure spread is {}..{} — outside parity",
            lo,
            hi
        );
    }

    #[test]
    fn combined_per_element_pressure_stays_near_the_s2_baseline() {
        // The S2/S3.1 single-race pressure each element carried before the
        // Kind split (deposit_unit * 1000 / lifespan, old single-race
        // table). S3.2's Plant+Animal rows are designed to *split* this
        // budget, not each independently carry it — see `RACES`'s doc
        // comment and §3's "consequence to track" note in the design doc.
        const WOOD_BASELINE: u64 = 2533;
        const FIRE_BASELINE: u64 = 2625;
        const EARTH_BASELINE: u64 = 2529;
        const METAL_BASELINE: u64 = 2527;
        const WATER_BASELINE: u64 = 2542;

        for (e, baseline) in [
            (Element::Wood, WOOD_BASELINE),
            (Element::Fire, FIRE_BASELINE),
            (Element::Earth, EARTH_BASELINE),
            (Element::Metal, METAL_BASELINE),
            (Element::Water, WATER_BASELINE),
        ] {
            let plant = attrs(Race { element: e, kind: Kind::Plant }).terraform_pressure();
            let animal = attrs(Race { element: e, kind: Kind::Animal }).terraform_pressure();
            let combined = plant + animal;
            // Within 10%: tight enough to catch a real regression, loose
            // enough not to be brittle to minor rebalancing.
            let lo = baseline * 9 / 10;
            let hi = baseline * 11 / 10;
            assert!(
                combined >= lo && combined <= hi,
                "{}: combined pressure {} outside [{}, {}] (baseline {})",
                e.name(),
                combined,
                lo,
                hi,
                baseline
            );
        }
    }

    fn dominant_deposit_channel(r: Race) -> Channel {
        let a = attrs(r);
        *Channel::ALL
            .iter()
            .max_by_key(|c| a.deposit_mix.permille(**c))
            .unwrap()
    }

    #[test]
    fn every_plant_is_existence_dominant() {
        // Rooted terraforming-by-being-there is close to the definition of
        // "plant" in this design.
        for e in Element::ALL {
            assert_eq!(
                dominant_deposit_channel(Race { element: e, kind: Kind::Plant }),
                Channel::OnExistence,
                "{}-Plant should be existence-dominant",
                e.name()
            );
        }
    }

    #[test]
    fn channel_dominance_matches_the_stated_fantasy() {
        // Short-lived animals terraform by dying; long-lived ones by
        // existing. Wood-Animal moved off `OnExistence` in S3.2 — Wood-Plant
        // now owns the existence-dominant slot for Wood.
        assert_eq!(dominant_deposit_channel(animal(Element::Fire)), Channel::OnDeath);
        assert_eq!(dominant_deposit_channel(animal(Element::Earth)), Channel::OnExistence);
        assert_eq!(dominant_deposit_channel(animal(Element::Water)), Channel::OnAction);
        assert_eq!(dominant_deposit_channel(animal(Element::Metal)), Channel::OnConsume);
        assert_eq!(dominant_deposit_channel(animal(Element::Wood)), Channel::OnConsume);
    }

    #[test]
    fn fire_and_metal_animals_leave_nothing_by_merely_existing() {
        assert_eq!(attrs(animal(Element::Fire)).deposit_mix.permille(Channel::OnExistence), 0);
        assert_eq!(attrs(animal(Element::Metal)).deposit_mix.permille(Channel::OnExistence), 0);
    }

    #[test]
    fn no_plant_has_a_zero_existence_share() {
        for e in Element::ALL {
            let a = attrs(Race { element: e, kind: Kind::Plant });
            assert!(
                a.deposit_mix.permille(Channel::OnExistence) > 0,
                "{}-Plant has a zero existence share",
                e.name()
            );
        }
    }

    #[test]
    fn a_rebalanced_mix_always_sums_to_one_thousand() {
        // Every reachable keystroke on the mix knobs, on every shipped row.
        for race in Race::ALL {
            for c in Channel::ALL {
                for v in [0u16, 1, 37, 250, 499, 500, 999, 1000, 5000] {
                    let mut m = attrs(race).deposit_mix;
                    m.set_rebalanced(c, v);
                    assert_eq!(m.total(), 1000, "{}-{} {} → {}: {:?}", race.element.name(), race.kind.name(), c.name(), v, m);
                    assert_eq!(m.permille(c), v.min(1000));
                }
            }
        }
    }

    #[test]
    fn rebalancing_survives_being_driven_into_a_corner() {
        // Zero everything, then set one channel — the degenerate path where
        // there is no remaining weight left to scale proportionally.
        let mut m = ChannelMix::new(1000, 0, 0, 0, 0);
        m.set_rebalanced(Channel::OnBirth, 0);
        assert_eq!(m.total(), 1000);
        m.set_rebalanced(Channel::OnDeath, 1000);
        assert_eq!(m.total(), 1000);
        assert_eq!(m.permille(Channel::OnDeath), 1000);
    }

    #[test]
    fn a_band_edge_edit_never_inverts_the_band() {
        for race in Race::ALL {
            for edge in [Edge::Floor, Edge::Nominal, Edge::Ceiling] {
                for v in [0u32, 1, 500, 1000, 6000, 100_000] {
                    let mut b = attrs(race).deposit;
                    b.set_edge(edge, v);
                    assert!(b.is_valid(), "{}-{} {:?}={} → {:?}", race.element.name(), race.kind.name(), edge, v, b);
                }
            }
        }
    }

    #[test]
    fn share_never_exceeds_the_unit() {
        for race in Race::ALL {
            let a = attrs(race);
            let total: u64 = Channel::ALL.iter().map(|c| a.deposit_mix.share(*c, 10_000)).sum();
            assert!(total <= 10_000, "{}-{} over-distributes: {}", race.element.name(), race.kind.name(), total);
        }
    }

    // S3.0: `RaceAttrs` has never had a per-field hash coverage test, and
    // it's exactly the struct S3 adds a `kind` field to. `Hashable` here is
    // hand-rolled with no derive and no reflection (`src/hash.rs`), so a
    // forgotten field compiles clean and passes every other test in this
    // file silently — see `Entity`'s sibling test (`src/entity.rs`) and
    // `EcologyTuning`'s (`src/ecology.rs`). `fantasy` is deliberately
    // excluded: it's a human-readable label, not simulation state, and
    // `hash_into` never reads it. S3.1 extends this with a `kind` variant
    // rather than writing a new test, per the established pattern.
    #[test]
    fn hash_notices_every_field() {
        let base = *attrs(animal(Element::Fire));
        let hash_of = |a: &RaceAttrs| {
            let mut h = Hasher::new();
            a.hash_into(&mut h);
            h.finish()
        };
        let base_hash = hash_of(&base);

        let mut element = base;
        element.element = Element::Water;
        let mut kind = base;
        kind.kind = Kind::Plant;
        let mut lifespan = base;
        lifespan.lifespan += 1;
        let mut lifespan_variance = base;
        lifespan_variance.lifespan_variance += 1;
        let mut speed = base;
        speed.speed = speed.speed + Fx::ONE;
        let mut radius = base;
        radius.radius = radius.radius + Fx::ONE;
        let mut deposit_unit = base;
        deposit_unit.deposit_unit += 1;
        let mut consume_unit = base;
        consume_unit.consume_unit += 1;
        let mut deposit = base;
        deposit.deposit.ceiling += 1;
        let mut deposit_mix = base;
        let v = deposit_mix.deposit_mix.permille(Channel::OnBirth);
        deposit_mix.deposit_mix.set_rebalanced(Channel::OnBirth, v + 1);
        let mut consume = base;
        consume.consume.ceiling += 1;
        let mut consume_mix = base;
        let v = consume_mix.consume_mix.permille(Channel::OnBirth);
        consume_mix.consume_mix.set_rebalanced(Channel::OnBirth, v + 1);

        for (name, variant) in [
            ("element", element),
            ("kind", kind),
            ("lifespan", lifespan),
            ("lifespan_variance", lifespan_variance),
            ("speed", speed),
            ("radius", radius),
            ("deposit_unit", deposit_unit),
            ("consume_unit", consume_unit),
            ("deposit", deposit),
            ("deposit_mix", deposit_mix),
            ("consume", consume),
            ("consume_mix", consume_mix),
        ] {
            assert_ne!(hash_of(&variant), base_hash, "{name} does not affect the hash");
        }
    }
}
