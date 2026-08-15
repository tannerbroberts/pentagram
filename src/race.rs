//! Race attributes.
//!
//! Every race is mechanically bespoke, but bespoke *in service of* a small set
//! of shared base attributes. Two rate axes carry almost all of the identity:
//!
//! - **life duration** — how long a body persists
//! - **consumption** — how fast, how burstily, and through which channel a
//!   race draws its habitat element down from terrain
//!
//! Deposition is no longer a third, independent axis. Under Invariant VIII
//! (material conservation — see `governor.rs`'s module doc and
//! `terrain::apply_conversion`), a race cannot write more of its own element
//! into the world than it has actually drawn from its habitat and converted;
//! `deposit_unit`/`deposit`/`deposit_mix` — the pre-Invariant-VIII second,
//! independent rate axis — are retired. What a race *does* with the material
//! it converts (background terrain deposit vs. its own body vs. explicit
//! waste) is now [`Conversion`]'s three-way split, a fixed per-tick
//! consequence of the (still fully governed, still bespoke-timed) consume
//! draw rather than a second rate-limited process of its own.
//!
//! The *how* is still the bespoke part and lives in [`ChannelMix`]/
//! [`Conversion`]: two races can draw habitat at an identical rate and feel
//! nothing alike, because one banks the product in its own body until death
//! and the other writes it straight into the ground.
//!
//! Tempo parity rule (§3.1 of the design doc): hold (the produced-material
//! analogue of) `deposit_unit / lifespan` roughly equal across races so no
//! race reshapes the map faster than another, while the texture stays
//! completely distinct. See [`RaceAttrs::terraform_pressure`]'s doc comment
//! for how this is computed post-Invariant-VIII.

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
/// Under Invariant VIII (material conservation — see `governor.rs`'s module
/// doc for the full account), `floor` is a **reserve**, not a free-material
/// guarantee: it is the portion of the banked bucket a single tick may never
/// spend down past, not a minimum emitted regardless of demand. That older
/// guarantee — material conjured even at zero demand, even from an extinct
/// race — is retired; conservation has nothing to conjure it from. The
/// ceiling is unchanged: a guarantee the world makes regardless of how many
/// players coordinate to exceed it. Between reserve and ceiling, terrain
/// change stays forecastable in its *upper* bound: the state at `T + k` is
/// bounded above before any player has decided anything, though (post-
/// Invariant-VIII) no longer bounded below.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RateBand {
    /// The portion of the banked bucket one terrain tick may never spend
    /// below (see [`Governor::settle`](crate::governor::Governor::settle)).
    /// No longer a guaranteed minimum grant — see this struct's own doc.
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

/// The coupled deposit/consume conversion every race resolves through
/// (Invariant VIII: material conservation — see `governor.rs`'s module doc).
///
/// Before Invariant VIII, `deposit_unit` and `consume_unit` were two
/// independent knobs — nothing stopped a race from depositing more of its
/// own element than it ever drew from its habitat, i.e. conjuring matter.
/// Under conservation there is exactly one flow: a race's `consume`/
/// `consume_mix`/`consume_unit` (unchanged in role — see those fields' own
/// docs) govern *when* and *how much* of `element.habitat()` a body draws
/// down from terrain each tick; that granted draw then runs through this
/// `Conversion` to become units of `element` itself (the race's own
/// element), accounted across exactly three destinations that sum to the
/// produced amount, nothing left over and nothing conjured:
///
/// - `deposit_share` — background terrain deposit, same role `deposit_unit`
///   used to serve, now a consequence of the draw rather than a second
///   independent rate.
/// - `body_share` — added to the converting body's own `Entity.material`
///   (growth/reserves), returned to terrain in full only when that specific
///   body dies (`World::charge_death`) — this is how a race that used to be
///   "deposit-dominant at `OnDeath`" (a scorch-mark corpse) still behaves
///   that way under the new model: a high `body_share` banks material in
///   the body until death does the depositing, literally, not as a
///   channel-timing illusion.
/// - `waste_share` — an explicit byproduct, also returned to terrain (same
///   element, same cells) but accounted separately, the way a lossy
///   real-world process always sheds some of its own output as scrap.
///
/// # The ratio can only ever break even or lose, never gain
///
/// `ratio_out <= ratio_in` always (`is_valid` enforces it) — a conversion
/// cannot manufacture mass, so it can never produce more `element` units
/// than `habitat()` units it consumed. Of the granted draw `N`, only
/// `batches * ratio_out` (where `batches = N / ratio_in`, integer floor)
/// ever actually leaves the habitat layer net; the remainder implicit in
/// each batch (`ratio_in - ratio_out`) is tailings that return to the same
/// habitat layer at the same cells immediately, so `terrain::apply_conversion`
/// applies it as a single net subtraction rather than two offsetting writes
/// — see that function's own doc comment for the exact arithmetic. Any
/// leftover draw that doesn't fill a whole batch (`N % ratio_in`) is simply
/// never drawn from terrain in the first place.
///
/// # A load-bearing design decision this stage had to make
///
/// Several shipped races' old `deposit_unit` exceeded their old
/// `consume_unit` (e.g. Wood-Plant: 57 000 deposited vs. 33 000 consumed) —
/// impossible under a `ratio_out <= ratio_in` conversion, since that would
/// require producing *more* than was consumed. Those races are clamped to
/// the best conservation permits: a lossless `ratio_in == ratio_out`
/// conversion. This necessarily lowers their old terraform-pressure
/// magnitude; there is no conservative way to preserve it exactly. See
/// `RACES`'s own doc comment for the concrete per-race choices.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Conversion {
    /// Habitat-element units consumed per conversion batch.
    pub ratio_in: u32,
    /// Own-element units produced per conversion batch. Never exceeds
    /// `ratio_in` — see the struct doc.
    pub ratio_out: u32,
    /// Permille of the produced own-element amount written to terrain as
    /// background deposit.
    pub deposit_share: u16,
    /// Permille of the produced own-element amount added to the converting
    /// body's own `Entity.material`.
    pub body_share: u16,
    /// Permille of the produced own-element amount returned to terrain as an
    /// explicit waste byproduct — distinct bookkeeping from `deposit_share`,
    /// though it lands on the same terrain layer.
    pub waste_share: u16,
}

impl Conversion {
    pub const fn new(
        ratio_in: u32,
        ratio_out: u32,
        deposit_share: u16,
        body_share: u16,
        waste_share: u16,
    ) -> Conversion {
        Conversion { ratio_in, ratio_out, deposit_share, body_share, waste_share }
    }

    pub fn is_valid(&self) -> bool {
        self.ratio_in > 0
            && self.ratio_out > 0
            && self.ratio_out <= self.ratio_in
            && (self.deposit_share as u32 + self.body_share as u32 + self.waste_share as u32) == 1000
    }

    /// Set one share and rebalance the other two proportionally so all
    /// three still sum to exactly 1000 — the same discipline
    /// `ChannelMix::set_rebalanced` already uses for a race's five-channel
    /// deposit mix, applied here to `Conversion`'s three-way split. Bug 4
    /// (Invariant VIII audit): before this existed, `tuning.rs`'s three
    /// share knobs each wrote their own field directly and independently,
    /// with nothing to stop two adjacent live-tuning edits from pushing the
    /// sum past 1000 — `terrain::apply_conversion`'s `waste_amt` computation
    /// (`produced - deposit_amt - body_amt`) then underflows and panics
    /// under this crate's `overflow-checks = true`. Editing a conversion is
    /// otherwise a two-step operation with an invalid state in the middle;
    /// this keeps the invariant holding after every single keystroke
    /// instead. See `World::retune`'s own `Conversion::clamp_shares` call
    /// for the second, independent enforcement point (defense against a
    /// `Conversion` reaching a running world some other way than through
    /// this method).
    pub fn set_share_rebalanced(&mut self, which: Share, v: u16) {
        let i = which.index();
        let v = v.min(1000);
        let mut vals = [self.deposit_share, self.body_share, self.waste_share];
        let rest_target = 1000u32 - v as u32;
        let rest_now: u32 = (0..3).filter(|k| *k != i).map(|k| vals[k] as u32).sum();
        vals[i] = v;

        #[allow(clippy::manual_checked_ops)]
        if rest_now == 0 {
            let each = rest_target / 2;
            for k in 0..3 {
                if k != i {
                    vals[k] = each as u16;
                }
            }
        } else {
            for k in 0..3 {
                if k != i {
                    vals[k] = (vals[k] as u32 * rest_target / rest_now) as u16;
                }
            }
        }

        // Integer division loses a few per-mille. Give them to the largest
        // of the shares we did not touch, so the sum lands on 1000 exactly
        // — same drift-correction discipline `ChannelMix::set_rebalanced`
        // uses.
        let total: u32 = vals.iter().map(|v| *v as u32).sum();
        let drift = 1000i32 - total as i32;
        if drift != 0 {
            let k = (0..3).filter(|k| *k != i).max_by_key(|k| vals[*k]).unwrap_or(0);
            vals[k] = (vals[k] as i32 + drift).clamp(0, 1000) as u16;
        }

        self.deposit_share = vals[0];
        self.body_share = vals[1];
        self.waste_share = vals[2];
    }

    /// Force `deposit_share + body_share + waste_share == 1000` in place,
    /// preserving `deposit_share`/`body_share` as closely as possible and
    /// letting `waste_share` absorb the remainder — the second of bug 4's
    /// two enforcement points, called from `World::retune` so a
    /// `Conversion` that reaches a running world through some path other
    /// than `set_share_rebalanced` (a bare field replacement, a future
    /// call site, a test fixture) still cannot leave the shares able to
    /// underflow `terrain::apply_conversion`'s `waste_amt` computation. If
    /// `deposit_share + body_share` alone already exceeds 1000, both are
    /// scaled down proportionally so they fit and `waste_share` becomes 0;
    /// otherwise `waste_share` is simply recomputed as the remainder — a
    /// no-op whenever the input was already valid.
    pub fn clamp_shares(&mut self) {
        let d = self.deposit_share as u32;
        let b = self.body_share as u32;
        let sum_db = d + b;
        if sum_db > 1000 {
            self.deposit_share = (d * 1000 / sum_db) as u16;
            self.body_share = (1000 - self.deposit_share as u32) as u16;
            self.waste_share = 0;
        } else {
            self.waste_share = (1000 - sum_db) as u16;
        }
    }
}

/// Which of `Conversion`'s three permille shares an edit targets —
/// `Conversion::set_share_rebalanced`'s selector, the same role `Channel`
/// plays for `ChannelMix::set_rebalanced`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Share {
    Deposit,
    Body,
    Waste,
}

impl Share {
    #[inline]
    const fn index(self) -> usize {
        match self {
            Share::Deposit => 0,
            Share::Body => 1,
            Share::Waste => 2,
        }
    }
}

impl Hashable for Conversion {
    fn hash_into(&self, h: &mut Hasher) {
        h.u32(self.ratio_in)
            .u32(self.ratio_out)
            .u16(self.deposit_share)
            .u16(self.body_share)
            .u16(self.waste_share);
    }
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

    /// Base quantity of `element.habitat()` drawn per channel event, before
    /// the mix is applied — the total a single body draws from terrain over
    /// its **entire life**. The sole driven demand now (see `Conversion`'s
    /// doc comment for why deposition is no longer a second, independent
    /// one).
    pub consume_unit: u64,
    pub consume: RateBand,
    pub consume_mix: ChannelMix,

    /// Invariant VIII: the coupled conversion this race's granted draw
    /// resolves through every terrain tick. See [`Conversion`]'s own doc.
    pub conversion: Conversion,

    /// Items/inventory (Invariant VIII extension): terrain units of a chosen
    /// element a body draws into its own `Entity.carried` per `Mine`
    /// command, capped by whatever the cell actually holds — see
    /// `World::mine`. Structurally zero for every `Kind::Plant` row
    /// (enforced by `body_is_valid`, the same discipline `speed`'s
    /// Animal/Plant split already uses) — Plants are rooted and this is a
    /// deliberate act, not passive existence. A first-guess tunable, like
    /// every other rate in this table; real per-race differentiation is
    /// future live-tuning work (see `hunt_weight`'s own uniform-default
    /// precedent, `ecology.rs`).
    pub mining_rate: u32,

    /// One-line statement of the fantasy the numbers above are serving.
    pub fantasy: &'static str,
}

impl RaceAttrs {
    pub fn is_valid(&self) -> bool {
        self.consume.is_valid()
            && self.consume_mix.is_valid()
            && self.conversion.is_valid()
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
        // Items/inventory: a Plant can never mine (rooted, see
        // `mining_rate`'s own doc comment) — an Animal row is free to ship
        // `mining_rate == 0` too (that's just "doesn't mine today," a valid
        // tuning choice), so only the Plant side is a hard structural rule.
        let mining_ok = self.kind == Kind::Animal || self.mining_rate == 0;
        speed_ok && self.radius > Fx::ZERO && mining_ok
    }

    /// Tempo parity metric — produced-material unit per 1000 ticks of life.
    /// Holding this roughly equal across races is what keeps a fast race
    /// from reshaping the map faster than a slow one. See §3.1.
    ///
    /// Post-Invariant-VIII: there is no independent `deposit_unit` to read
    /// anymore (see `Conversion`'s doc comment), so this is now a proxy —
    /// `consume_unit` run once through the race's own conversion ratio,
    /// which is exactly what one whole life's worth of draw actually
    /// produces of the race's own element before the three-way split.
    pub fn terraform_pressure(&self) -> u64 {
        let produced_per_life = self
            .consume_unit
            .saturating_mul(self.conversion.ratio_out as u64)
            / (self.conversion.ratio_in as u64).max(1);
        produced_per_life.saturating_mul(1000) / self.lifespan.max(1)
    }

    // ------------------------------------------------------------------
    // Per-event demand.
    //
    // `consume_unit` is the total a single body draws from terrain over its
    // *entire life*. Each channel's per-mille share of that total is then
    // spread across however many times that channel actually fires in a life:
    // birth and death fire once, existence fires once per terrain tick,
    // actions and meals fire at their own cadence.
    //
    // This is what makes the parity rule real rather than aspirational — two
    // races with equal `consume_unit / lifespan` (once run through their own
    // conversion ratio, since Invariant VIII) write the same amount to the
    // map per unit time no matter how differently they spend it.
    //
    // Returns **milli-units**, because a share divided across two million
    // Earth ticks underflows integer division otherwise.
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

    /// Milli-units of habitat draw contributed by one firing of `c`. The
    /// sole driven demand post-Invariant-VIII — what used to also be a
    /// separate `deposit_per` is now `terrain::apply_conversion`'s derived
    /// consequence of this same draw, not a second accumulated demand.
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
///
/// **Invariant VIII migration.** `deposit_unit`/`deposit`/`deposit_mix` are
/// retired (see [`Conversion`]'s doc comment); `consume_unit`/`consume`/
/// `consume_mix` are carried over completely unchanged, so every row's
/// draw-timing character (when in its life it draws down its habitat) is
/// preserved exactly. Each row's new `conversion` was chosen to preserve the
/// *old* deposit-side character too, wherever conservation allows it:
///
/// - **Wood, Earth, Metal** all had `deposit_unit > consume_unit` — asking
///   to write back more than was ever drawn in, i.e. conjuring matter, which
///   `ratio_out <= ratio_in` forbids outright. These three are clamped to
///   the best a conservative conversion can do: lossless, `ratio_in ==
///   ratio_out == 1`. Their terraform pressure necessarily drops from the
///   old table (there is no conservative way to avoid that — see
///   `Conversion`'s doc comment) but their *relative* ordering and their
///   deposit-mix-implied character survive via the three-way split below:
///   an existence-dominant old `deposit_mix` becomes a high `deposit_share`
///   (steady background terraforming), and Metal-Animal's old
///   refining-only, zero-existence mix becomes a high `waste_share`
///   (forging sheds scrap) instead.
/// - **Fire, Water** both had `deposit_unit < consume_unit` — already a
///   lossy ratio in the old numbers — so their `conversion.ratio_out /
///   ratio_in` is set to reproduce that old ratio as closely as integer
///   division allows (Fire ≈ 0.477, Water ≈ 0.742).
/// - Fire-Animal's old character — "terraforms almost entirely by dying,
///   the corpse is the scorch mark" (`deposit_mix` was `OnDeath`-dominant at
///   700‰) — is now carried by a high `body_share` (800‰) instead of a
///   deposit-channel timing knob: material banks in the body as it lives
///   and returns to terrain in one lump only when `World::charge_death`
///   fires, which is a truer version of the old flavour text than the mix
///   knob ever was.
pub const RACES: PerRace<RaceAttrs> = PerRace([
    // ---------------------------------------------------------------- WOOD
    RaceAttrs {
        element: Element::Wood,
        kind: Kind::Plant,
        lifespan: TICKS_PER_HOUR * 15 / 2, // 7.5 hours — 3x its Animal twin
        lifespan_variance: 180,
        speed: Fx::ZERO,
        radius: Fx::ratio(60, 100),
        consume_unit: 33_000,
        consume: RateBand::new(300, 900, 1800, 6),
        consume_mix: ChannelMix::new(30, 70, 30, 120, 750),
        // Old deposit_unit (57 000) exceeded consume_unit (33 000) —
        // clamped to lossless (see the table's own doc comment). Old
        // deposit_mix was existence-dominant (580‰): background terraform
        // share stays highest here.
        conversion: Conversion::new(1, 1, 650, 300, 50),
        mining_rate: 0, // rooted — Plants never mine
        fantasy: "Grows where it stands and draws through its roots — the map remembers a thicket long after any single stem is gone.",
    },
    RaceAttrs {
        element: Element::Wood,
        kind: Kind::Animal,
        lifespan: TICKS_PER_HOUR * 5 / 2, // 2.5 hours
        lifespan_variance: 180,
        speed: Fx::ratio(9, 100),
        radius: Fx::ratio(60, 100),
        consume_unit: 11_000,
        consume: RateBand::new(300, 900, 1800, 6),
        consume_mix: ChannelMix::new(50, 50, 100, 500, 300),
        // Old deposit_mix moved off existence onto OnConsume (450‰) —
        // terraforms by what it takes in, not by standing around; the
        // deposit/body split stays balanced rather than existence-heavy.
        conversion: Conversion::new(1, 1, 500, 400, 100),
        mining_rate: 40, // first-guess, uniform across every Animal row
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
        consume_unit: 6_600,
        consume: RateBand::new(150, 1100, 7000, 20),
        consume_mix: ChannelMix::new(20, 60, 20, 100, 800),
        // Old deposit/consume ratio ≈ 3150/6600 ≈ 0.477.
        conversion: Conversion::new(1000, 477, 700, 250, 50),
        mining_rate: 0, // rooted — Plants never mine
        fantasy: "Ember-moss, smouldering low and constant — it never dies spectacularly, it just keeps glowing where it took root.",
    },
    RaceAttrs {
        element: Element::Fire,
        kind: Kind::Animal,
        lifespan: TICKS_PER_MINUTE * 8, // 8 minutes
        lifespan_variance: 300,
        speed: Fx::ratio(46, 100),
        radius: Fx::ratio(40, 100),
        // Spikiest band in the table: can go quiet, then spike sixfold.
        consume_unit: 2_200,
        consume: RateBand::new(150, 1100, 7000, 20),
        consume_mix: ChannelMix::new(0, 100, 400, 500, 0),
        // Old deposit/consume ratio ≈ 1050/2200 ≈ 0.477 — same as its Plant
        // twin. Terraforms almost entirely by dying: material banks in the
        // body (800‰) and returns as one lump at death, not spread by a
        // deposit-channel timing knob. A fire creature's corpse is the
        // scorch mark.
        conversion: Conversion::new(1000, 477, 100, 800, 100),
        mining_rate: 40, // first-guess, uniform across every Animal row
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
        consume_unit: 3_600_000,
        consume: RateBand::new(600, 850, 1200, 2),
        consume_mix: ChannelMix::new(10, 20, 20, 50, 900),
        // Old deposit_unit (7 650 000) exceeded consume_unit (3 600 000) —
        // clamped to lossless. Old deposit_mix was overwhelmingly
        // existence-dominant (800‰): background share stays highest.
        conversion: Conversion::new(1, 1, 750, 200, 50),
        mining_rate: 0, // rooted — Plants never mine
        fantasy: "Older than the animal that shares its name — patience that outlasts even Earth's own long-lived fauna, terraforming by simply persisting the longest.",
    },
    RaceAttrs {
        element: Element::Earth,
        kind: Kind::Animal,
        lifespan: TICKS_PER_DAY * 14, // a fortnight
        lifespan_variance: 120,
        speed: Fx::ratio(4, 100),
        radius: Fx::ratio(150, 100),
        // Nearly a metronome. Barely bursts, barely rests.
        consume_unit: 1_200_000,
        consume: RateBand::new(600, 850, 1200, 2),
        consume_mix: ChannelMix::new(0, 50, 50, 200, 700),
        // Old deposit_unit (2 550 000) exceeded consume_unit (1 200 000) —
        // clamped to lossless, same reasoning as Earth-Plant. Presence is
        // the mechanism: background share stays highest.
        conversion: Conversion::new(1, 1, 700, 250, 50),
        mining_rate: 40, // first-guess, uniform across every Animal row
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
        consume_unit: 144_000,
        consume: RateBand::new(200, 950, 2400, 12),
        consume_mix: ChannelMix::new(60, 60, 100, 180, 600),
        // Old deposit_unit (273 000) exceeded consume_unit (144 000) —
        // clamped to lossless.
        conversion: Conversion::new(1, 1, 550, 350, 100),
        mining_rate: 0, // rooted — Plants never mine
        fantasy: "An ore-vein growth, embedded and slow — it forges nothing, it just sits in the seam and the seam changes around it.",
    },
    RaceAttrs {
        element: Element::Metal,
        kind: Kind::Animal,
        lifespan: TICKS_PER_HOUR * 12,
        lifespan_variance: 90,
        speed: Fx::ratio(21, 100),
        radius: Fx::ratio(55, 100),
        consume_unit: 48_000,
        consume: RateBand::new(200, 950, 2400, 12),
        consume_mix: ChannelMix::new(100, 50, 250, 600, 0),
        // Old deposit_unit (91 000) exceeded consume_unit (48 000) —
        // clamped to lossless. Old deposit_mix was refining-only, zero
        // existence share ("writes to the world only at the moment of
        // forging"); that flavour now lives in a high waste_share — forging
        // sheds scrap — rather than a channel-timing knob.
        conversion: Conversion::new(1, 1, 400, 350, 250),
        mining_rate: 40, // first-guess, uniform across every Animal row
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
        consume_unit: 18_000,
        consume: RateBand::new(250, 1000, 3200, 10),
        consume_mix: ChannelMix::new(20, 80, 100, 150, 650),
        // Old deposit/consume ratio ≈ 13350/18000 ≈ 0.742.
        conversion: Conversion::new(1000, 742, 600, 300, 100),
        mining_rate: 0, // rooted — Plants never mine
        fantasy: "A reed-bed, rooted in the shallows — where the animal current tears through, this just sits and slowly silts the bank.",
    },
    RaceAttrs {
        element: Element::Water,
        kind: Kind::Animal,
        lifespan: TICKS_PER_MINUTE * 35,
        lifespan_variance: 220,
        speed: Fx::ratio(33, 100),
        radius: Fx::ratio(50, 100),
        consume_unit: 6_000,
        consume: RateBand::new(250, 1000, 3200, 10),
        consume_mix: ChannelMix::new(0, 100, 550, 300, 50),
        // Old deposit/consume ratio ≈ 4450/6000 ≈ 0.742 — same as its Plant
        // twin. Erodes what it moves across: deposit share stays highest,
        // direct and immediate rather than banked in the body.
        conversion: Conversion::new(1000, 742, 650, 250, 100),
        mining_rate: 40, // first-guess, uniform across every Animal row
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
            .u64(self.consume_unit);
        self.consume.hash_into(h);
        self.consume_mix.hash_into(h);
        self.conversion.hash_into(h);
        h.u32(self.mining_rate);
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
    fn every_mix_and_conversion_split_sums_to_one_thousand() {
        // Invariant VIII: `deposit_mix` is retired (see `Conversion`'s doc
        // comment) — the equivalent three-way split now lives on
        // `conversion` (deposit_share/body_share/waste_share), and must sum
        // to exactly 1000 the same way `consume_mix` always has.
        for race in Race::ALL {
            let a = attrs(race);
            assert_eq!(a.consume_mix.total(), 1000, "{}-{} consume mix", race.element.name(), race.kind.name());
            let c = a.conversion;
            assert_eq!(
                c.deposit_share as u32 + c.body_share as u32 + c.waste_share as u32,
                1000,
                "{}-{} conversion split",
                race.element.name(),
                race.kind.name()
            );
        }
    }

    #[test]
    fn every_consume_band_holds_back_a_nonzero_reserve() {
        // Invariant VIII repurposed `floor` from a churn guarantee into a
        // reserve (see `RateBand`'s own doc comment and `governor.rs`'s
        // module doc) — every shipped race still keeps a nonzero slice of
        // its own bucket banked at all times, a deliberate tuning choice,
        // not a structural requirement (`floor == 0` is perfectly valid now).
        for race in Race::ALL {
            let a = attrs(race);
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
        // §3.1: produced-material pressure roughly equal across races, so no
        // race reshapes the map faster than another. Invariant VIII changed
        // what `terraform_pressure()` reads (`consume_unit` run through the
        // race's own conversion ratio, not an independent `deposit_unit` —
        // see `RaceAttrs::terraform_pressure`'s own doc comment), and the
        // `ratio_out <= ratio_in` clamp (`Conversion`'s doc comment) forces
        // Wood/Earth/Metal down to a lossless 1:1 conversion while Fire/
        // Water keep their old, already-lossy ratio almost exactly — so the
        // ten rows no longer cluster inside the old 2x band on their own.
        // Widened to 2.5x, comfortably covering the ~2.2x spread the shipped
        // table actually produces (see
        // `combined_per_element_pressure_reflects_the_invariant_viii_
        // migration` below for the concrete per-element accounting), while
        // still catching a real regression toward one race dwarfing another.
        let ps: Vec<u64> = Race::ALL.iter().map(|r| attrs(*r).terraform_pressure()).collect();
        let lo = *ps.iter().min().unwrap();
        let hi = *ps.iter().max().unwrap();
        assert!(lo > 0, "a race has zero terraform pressure");
        assert!(
            hi * 2 <= lo * 5,
            "terraform pressure spread is {}..{} — outside the post-Invariant-VIII 2.5x band",
            lo,
            hi
        );
    }

    #[test]
    fn combined_per_element_pressure_reflects_the_invariant_viii_migration() {
        // The S2/S3.1 single-race pressure each element carried before the
        // Kind split (deposit_unit * 1000 / lifespan, old single-race
        // table, pre-Invariant-VIII).
        //
        // Fire and Water's old deposit/consume ratio was already lossy, and
        // `RACES`'s shipped `conversion.ratio_out/ratio_in` was deliberately
        // chosen to reproduce that old ratio (`RACES`'s own doc comment) —
        // so their combined pressure still lands within 10% of the old
        // baseline, and that closeness is still worth guarding as a
        // regression check, same as before Invariant VIII.
        //
        // Wood/Earth/Metal's old `deposit_unit` exceeded `consume_unit`,
        // which `ratio_out <= ratio_in` forbids outright — a conversion
        // cannot manufacture mass (`Conversion`'s doc comment). They are
        // clamped to lossless 1:1 conversion instead, which *necessarily*
        // lowers their combined pressure below the old baseline; there is
        // no conservative way to avoid that (same doc comment), so
        // asserting closeness to the old number for these three would be
        // asserting the old, retired create-from-nothing economy back into
        // existence. What is checked for them instead: pressure genuinely
        // dropped, in the expected direction, and did not collapse to
        // somewhere implausible.
        const WOOD_BASELINE: u64 = 2533;
        const FIRE_BASELINE: u64 = 2625;
        const EARTH_BASELINE: u64 = 2529;
        const METAL_BASELINE: u64 = 2527;
        const WATER_BASELINE: u64 = 2542;

        let combined_of = |e: Element| {
            attrs(Race { element: e, kind: Kind::Plant }).terraform_pressure()
                + attrs(Race { element: e, kind: Kind::Animal }).terraform_pressure()
        };

        for (e, baseline) in [(Element::Fire, FIRE_BASELINE), (Element::Water, WATER_BASELINE)] {
            let combined = combined_of(e);
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

        for (e, baseline) in [(Element::Wood, WOOD_BASELINE), (Element::Earth, EARTH_BASELINE), (Element::Metal, METAL_BASELINE)] {
            let combined = combined_of(e);
            assert!(
                combined < baseline,
                "{}: combined pressure {} should have dropped below the old baseline {} once clamped to a lossless conversion",
                e.name(),
                combined,
                baseline
            );
            assert!(
                combined * 5 > baseline,
                "{}: combined pressure {} collapsed below a fifth of the old baseline {} — likely a real miscalibration, not just the clamp",
                e.name(),
                combined,
                baseline
            );
        }
    }

    // Invariant VIII retired `deposit_mix` — deposition no longer has its
    // own per-channel timing at all, only a fixed three-way split of
    // whatever the (still per-channel-timed) `consume` draw produces (see
    // `Conversion`'s doc comment). So "dominant channel" below now reads
    // `consume_mix` — *when in its life a race draws its habitat down*,
    // which for most shipped rows still lines up with the old deposit-timing
    // character (`RACES`'s own doc comment walks through why, row by row).
    fn dominant_consume_channel(r: Race) -> Channel {
        let a = attrs(r);
        *Channel::ALL
            .iter()
            .max_by_key(|c| a.consume_mix.permille(**c))
            .unwrap()
    }

    #[test]
    fn every_plant_is_existence_dominant() {
        // Rooted terraforming-by-being-there is close to the definition of
        // "plant" in this design. Holds for every shipped Plant row's
        // `consume_mix` unchanged from before Invariant VIII — see `RACES`'s
        // doc comment.
        for e in Element::ALL {
            assert_eq!(
                dominant_consume_channel(Race { element: e, kind: Kind::Plant }),
                Channel::OnExistence,
                "{}-Plant should be existence-dominant",
                e.name()
            );
        }
    }

    #[test]
    fn channel_dominance_matches_the_stated_fantasy() {
        // Earth/Water/Metal/Wood-Animal's `consume_mix` dominance is
        // unchanged from before Invariant VIII (draw timing was never
        // retired, only deposit timing was). Fire-Animal's old
        // character — "terraforms almost entirely by dying" — no longer
        // shows up as a *consume* channel at all (it draws mostly through
        // OnConsume now); it lives in `conversion.body_share` instead,
        // asserted separately below, per `Conversion`'s own doc comment.
        assert_eq!(dominant_consume_channel(animal(Element::Earth)), Channel::OnExistence);
        assert_eq!(dominant_consume_channel(animal(Element::Water)), Channel::OnAction);
        assert_eq!(dominant_consume_channel(animal(Element::Metal)), Channel::OnConsume);
        assert_eq!(dominant_consume_channel(animal(Element::Wood)), Channel::OnConsume);

        let fire = attrs(animal(Element::Fire)).conversion;
        assert!(
            fire.body_share > fire.deposit_share && fire.body_share > fire.waste_share,
            "Fire-Animal should bank most of its produced material in the body until death: {:?}",
            fire
        );
    }

    #[test]
    fn fire_and_metal_animals_draw_nothing_by_merely_existing() {
        assert_eq!(attrs(animal(Element::Fire)).consume_mix.permille(Channel::OnExistence), 0);
        assert_eq!(attrs(animal(Element::Metal)).consume_mix.permille(Channel::OnExistence), 0);
    }

    #[test]
    fn no_plant_has_a_zero_existence_share() {
        for e in Element::ALL {
            let a = attrs(Race { element: e, kind: Kind::Plant });
            assert!(
                a.consume_mix.permille(Channel::OnExistence) > 0,
                "{}-Plant has a zero existence share",
                e.name()
            );
        }
    }

    #[test]
    fn a_rebalanced_mix_always_sums_to_one_thousand() {
        // Every reachable keystroke on the mix knobs, on every shipped row.
        // `deposit_mix` is retired (Invariant VIII) — `consume_mix` is the
        // one surviving per-channel-timing table this mechanism now edits.
        for race in Race::ALL {
            for c in Channel::ALL {
                for v in [0u16, 1, 37, 250, 499, 500, 999, 1000, 5000] {
                    let mut m = attrs(race).consume_mix;
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

    // Bug 4 regression: before `Conversion::set_share_rebalanced` existed,
    // `tuning.rs`'s three share knobs each wrote their own field
    // independently, so two adjacent keystrokes (e.g. push `dep share` to
    // 900, then `body share` to 900) could leave the sum at 1800 — which
    // `terrain::apply_conversion`'s `waste_amt = produced - deposit_amt -
    // body_amt` then underflows and panics on (`overflow-checks = true`).
    #[test]
    fn a_rebalanced_conversion_share_always_sums_to_one_thousand() {
        for race in Race::ALL {
            for which in [Share::Deposit, Share::Body, Share::Waste] {
                for v in [0u16, 1, 37, 250, 499, 500, 999, 1000, 5000] {
                    let mut c = attrs(race).conversion;
                    c.set_share_rebalanced(which, v);
                    assert_eq!(
                        c.deposit_share as u32 + c.body_share as u32 + c.waste_share as u32,
                        1000,
                        "{}-{} share {:?} → {}: {:?}",
                        race.element.name(),
                        race.kind.name(),
                        which,
                        v,
                        c
                    );
                    let got = match which {
                        Share::Deposit => c.deposit_share,
                        Share::Body => c.body_share,
                        Share::Waste => c.waste_share,
                    };
                    assert_eq!(got, v.min(1000));
                }
            }
        }
    }

    #[test]
    fn two_adjacent_share_edits_that_would_have_broken_the_sum_stay_valid() {
        // The exact scenario the bug report names: "two adjacent tuning-UI
        // keystrokes". Push deposit to 900, then body to 900 — the naive
        // independent-field-write version leaves 900+900+50=1850; the
        // rebalanced version keeps every edit's sum at exactly 1000.
        let mut c = Conversion::new(1, 1, 650, 300, 50);
        c.set_share_rebalanced(Share::Deposit, 900);
        assert_eq!(c.deposit_share as u32 + c.body_share as u32 + c.waste_share as u32, 1000);
        c.set_share_rebalanced(Share::Body, 900);
        assert_eq!(c.deposit_share as u32 + c.body_share as u32 + c.waste_share as u32, 1000);
        assert_eq!(c.body_share, 900);
        assert!(c.is_valid(), "{:?}", c);
    }

    #[test]
    fn conversion_share_rebalancing_survives_being_driven_into_a_corner() {
        let mut c = Conversion::new(1, 1, 1000, 0, 0);
        c.set_share_rebalanced(Share::Deposit, 0);
        assert_eq!(c.deposit_share as u32 + c.body_share as u32 + c.waste_share as u32, 1000);
        c.set_share_rebalanced(Share::Waste, 1000);
        assert_eq!(c.deposit_share as u32 + c.body_share as u32 + c.waste_share as u32, 1000);
        assert_eq!(c.waste_share, 1000);
    }

    #[test]
    fn clamp_shares_fixes_a_conversion_whose_sum_already_broke_one_thousand() {
        // The second enforcement point (`World::retune`): a `Conversion`
        // constructed some other way than through `set_share_rebalanced`
        // (a bare struct literal, exactly what `World::retune`'s old
        // straight field-replacement accepted unchecked) can arrive with an
        // invalid sum. `clamp_shares` must repair it rather than leave it
        // able to underflow `terrain::apply_conversion`'s `waste_amt`.
        let mut c = Conversion::new(1, 1, 900, 900, 50); // sums to 1850
        assert!(!c.is_valid());
        c.clamp_shares();
        assert!(c.is_valid(), "{:?}", c);
        assert_eq!(c.deposit_share as u32 + c.body_share as u32 + c.waste_share as u32, 1000);

        // Already-valid input is left untouched (idempotent).
        let mut valid = Conversion::new(1, 1, 650, 300, 50);
        valid.clamp_shares();
        assert_eq!(valid, Conversion::new(1, 1, 650, 300, 50));
    }

    #[test]
    fn a_band_edge_edit_never_inverts_the_band() {
        // `deposit`'s own `RateBand` is retired along with `deposit_mix`
        // (Invariant VIII) — `consume` is the one band left to edit.
        for race in Race::ALL {
            for edge in [Edge::Floor, Edge::Nominal, Edge::Ceiling] {
                for v in [0u32, 1, 500, 1000, 6000, 100_000] {
                    let mut b = attrs(race).consume;
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
            let total: u64 = Channel::ALL.iter().map(|c| a.consume_mix.share(*c, 10_000)).sum();
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
        let mut consume_unit = base;
        consume_unit.consume_unit += 1;
        let mut consume = base;
        consume.consume.ceiling += 1;
        let mut consume_mix = base;
        let v = consume_mix.consume_mix.permille(Channel::OnBirth);
        consume_mix.consume_mix.set_rebalanced(Channel::OnBirth, v + 1);
        // Invariant VIII: `deposit_unit`/`deposit`/`deposit_mix` are gone;
        // `conversion` and `mining_rate` are the fields that replaced them
        // in this struct's shape.
        let mut conversion_ratio = base;
        conversion_ratio.conversion.ratio_out = (conversion_ratio.conversion.ratio_out / 2).max(1);
        let mut conversion_split = base;
        conversion_split.conversion.deposit_share = conversion_split.conversion.deposit_share.saturating_add(1);
        conversion_split.conversion.body_share = conversion_split.conversion.body_share.saturating_sub(1);
        let mut mining_rate = base;
        mining_rate.mining_rate += 1;

        for (name, variant) in [
            ("element", element),
            ("kind", kind),
            ("lifespan", lifespan),
            ("lifespan_variance", lifespan_variance),
            ("speed", speed),
            ("radius", radius),
            ("consume_unit", consume_unit),
            ("consume", consume),
            ("consume_mix", consume_mix),
            ("conversion_ratio", conversion_ratio),
            ("conversion_split", conversion_split),
            ("mining_rate", mining_rate),
        ] {
            assert_ne!(hash_of(&variant), base_hash, "{name} does not affect the hash");
        }
    }
}
