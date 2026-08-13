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

use crate::element::{Element, PerElement};
use crate::fx::Fx;
use crate::hash::{Hashable, Hasher};

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
pub const RACES: PerElement<RaceAttrs> = PerElement([
    // ---------------------------------------------------------------- WOOD
    RaceAttrs {
        element: Element::Wood,
        lifespan: TICKS_PER_HOUR * 5 / 2, // 2.5 hours
        lifespan_variance: 180,
        speed: Fx::ratio(9, 100),
        radius: Fx::ratio(60, 100),
        deposit_unit: 38_000,
        deposit: RateBand::new(500, 1000, 2000, 6),
        // Grows where it stands and draws through its roots. Balanced between
        // simply existing and what it takes in.
        deposit_mix: ChannelMix::new(100, 200, 50, 250, 400),
        consume_unit: 22_000,
        consume: RateBand::new(300, 900, 1800, 6),
        consume_mix: ChannelMix::new(50, 50, 100, 500, 300),
        fantasy: "Grows in place. Slow, patient, and very hard to remove once rooted.",
    },
    // ---------------------------------------------------------------- FIRE
    RaceAttrs {
        element: Element::Fire,
        lifespan: TICKS_PER_MINUTE * 8, // 8 minutes
        lifespan_variance: 300,
        speed: Fx::ratio(46, 100),
        radius: Fx::ratio(40, 100),
        deposit_unit: 2_100,
        // Spikiest band in the table: can go quiet, then spike sixfold.
        deposit: RateBand::new(200, 1000, 6000, 20),
        // Terraforms almost entirely by dying. A fire creature's corpse is the
        // scorch mark.
        deposit_mix: ChannelMix::new(150, 700, 100, 50, 0),
        consume_unit: 4_400,
        consume: RateBand::new(150, 1100, 7000, 20),
        consume_mix: ChannelMix::new(0, 100, 400, 500, 0),
        fantasy: "Born from a lightning strike, burns hot, dies fast, and the ground remembers.",
    },
    // --------------------------------------------------------------- EARTH
    RaceAttrs {
        element: Element::Earth,
        lifespan: TICKS_PER_DAY * 14, // a fortnight
        lifespan_variance: 120,
        speed: Fx::ratio(4, 100),
        radius: Fx::ratio(150, 100),
        deposit_unit: 5_100_000,
        // Nearly a metronome. Barely bursts, barely rests.
        deposit: RateBand::new(800, 1000, 1400, 2),
        // Presence is the mechanism. An Earth body presses on the world simply
        // by continuing to be somewhere.
        deposit_mix: ChannelMix::new(50, 50, 50, 100, 750),
        consume_unit: 2_400_000,
        consume: RateBand::new(600, 850, 1200, 2),
        consume_mix: ChannelMix::new(0, 50, 50, 200, 700),
        fantasy: "Ancient. Lives for a fortnight and terraforms by refusing to move.",
    },
    // --------------------------------------------------------------- METAL
    RaceAttrs {
        element: Element::Metal,
        lifespan: TICKS_PER_HOUR * 12,
        lifespan_variance: 90,
        speed: Fx::ratio(21, 100),
        radius: Fx::ratio(55, 100),
        deposit_unit: 182_000,
        deposit: RateBand::new(250, 1000, 2500, 12),
        // Deposits at the moment of refining. Nothing from mere existence —
        // Metal that is not working leaves no trace at all.
        deposit_mix: ChannelMix::new(200, 150, 250, 400, 0),
        consume_unit: 96_000,
        consume: RateBand::new(200, 950, 2400, 12),
        consume_mix: ChannelMix::new(100, 50, 250, 600, 0),
        fantasy: "Precise and scheduled. Writes to the world only at the moment of forging.",
    },
    // --------------------------------------------------------------- WATER
    RaceAttrs {
        element: Element::Water,
        lifespan: TICKS_PER_MINUTE * 35,
        lifespan_variance: 220,
        speed: Fx::ratio(33, 100),
        radius: Fx::ratio(50, 100),
        deposit_unit: 8_900,
        deposit: RateBand::new(300, 1000, 3000, 10),
        // Erodes what it moves across. Action-dominant: standing still, Water
        // does almost nothing.
        deposit_mix: ChannelMix::new(50, 250, 500, 150, 50),
        consume_unit: 12_000,
        consume: RateBand::new(250, 1000, 3200, 10),
        consume_mix: ChannelMix::new(0, 100, 550, 300, 50),
        fantasy: "Rhythmic and tidal. Terraforms by flowing, and stops when it stops.",
    },
]);

/// The shipped table — the starting point every `World` is tuned away from.
#[inline]
pub fn attrs(e: Element) -> &'static RaceAttrs {
    &RACES.0[e.index()]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_race_is_internally_consistent() {
        for e in Element::ALL {
            let a = attrs(e);
            assert_eq!(a.element, e, "table row is filed under the wrong element");
            assert!(a.is_valid(), "{} has an invalid attribute set", e.name());
        }
    }

    #[test]
    fn every_mix_sums_to_one_thousand() {
        for e in Element::ALL {
            let a = attrs(e);
            assert_eq!(a.deposit_mix.total(), 1000, "{} deposit mix", e.name());
            assert_eq!(a.consume_mix.total(), 1000, "{} consume mix", e.name());
        }
    }

    #[test]
    fn every_band_has_a_nonzero_floor() {
        // The churn guarantee. A race with a zero floor can leave a region
        // frozen, which is how a dead server happens.
        for e in Element::ALL {
            let a = attrs(e);
            assert!(a.deposit.floor > 0, "{} deposit floor is zero", e.name());
            assert!(a.consume.floor > 0, "{} consume floor is zero", e.name());
        }
    }

    #[test]
    fn lifespans_span_the_intended_three_orders_of_magnitude() {
        let fire = attrs(Element::Fire).lifespan;
        let earth = attrs(Element::Earth).lifespan;
        assert!(earth / fire > 2000, "ratio is only {}", earth / fire);
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
        for w in order.windows(2) {
            assert!(
                attrs(w[0]).lifespan < attrs(w[1]).lifespan,
                "{} should be shorter-lived than {}",
                w[0].name(),
                w[1].name()
            );
        }
    }

    #[test]
    fn terraform_pressure_is_within_parity_band() {
        // §3.1: deposit_unit / lifespan roughly equal across races, so no race
        // reshapes the map faster than another. Allow a 2x spread.
        let ps: Vec<u64> = Element::ALL.iter().map(|e| attrs(*e).terraform_pressure()).collect();
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
    fn channel_dominance_matches_the_stated_fantasy() {
        let dominant = |e: Element| -> Channel {
            let a = attrs(e);
            *Channel::ALL
                .iter()
                .max_by_key(|c| a.deposit_mix.permille(**c))
                .unwrap()
        };
        // Short-lived races terraform by dying; long-lived ones by existing.
        assert_eq!(dominant(Element::Fire), Channel::OnDeath);
        assert_eq!(dominant(Element::Earth), Channel::OnExistence);
        assert_eq!(dominant(Element::Water), Channel::OnAction);
        assert_eq!(dominant(Element::Metal), Channel::OnConsume);
        assert_eq!(dominant(Element::Wood), Channel::OnExistence);
    }

    #[test]
    fn fire_leaves_nothing_by_merely_existing() {
        assert_eq!(attrs(Element::Fire).deposit_mix.permille(Channel::OnExistence), 0);
        assert_eq!(attrs(Element::Metal).deposit_mix.permille(Channel::OnExistence), 0);
    }

    #[test]
    fn a_rebalanced_mix_always_sums_to_one_thousand() {
        // Every reachable keystroke on the mix knobs, on every shipped row.
        for e in Element::ALL {
            for c in Channel::ALL {
                for v in [0u16, 1, 37, 250, 499, 500, 999, 1000, 5000] {
                    let mut m = attrs(e).deposit_mix;
                    m.set_rebalanced(c, v);
                    assert_eq!(m.total(), 1000, "{} {} → {}: {:?}", e.name(), c.name(), v, m);
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
        for e in Element::ALL {
            for edge in [Edge::Floor, Edge::Nominal, Edge::Ceiling] {
                for v in [0u32, 1, 500, 1000, 6000, 100_000] {
                    let mut b = attrs(e).deposit;
                    b.set_edge(edge, v);
                    assert!(b.is_valid(), "{} {:?}={} → {:?}", e.name(), edge, v, b);
                }
            }
        }
    }

    #[test]
    fn share_never_exceeds_the_unit() {
        for e in Element::ALL {
            let a = attrs(e);
            let total: u64 = Channel::ALL.iter().map(|c| a.deposit_mix.share(*c, 10_000)).sum();
            assert!(total <= 10_000, "{} over-distributes: {}", e.name(), total);
        }
    }

    // S3.0: `RaceAttrs` has never had a per-field hash coverage test, and
    // it's exactly the struct S3 adds a `kind` field to. `Hashable` here is
    // hand-rolled with no derive and no reflection (`src/hash.rs`), so a
    // forgotten field compiles clean and passes every other test in this
    // file silently — see `Entity`'s sibling test (`src/entity.rs`) and
    // `EcologyTuning`'s (`src/ecology.rs`). `fantasy` is deliberately
    // excluded: it's a human-readable label, not simulation state, and
    // `hash_into` never reads it.
    #[test]
    fn hash_notices_every_field() {
        let base = *attrs(Element::Fire);
        let hash_of = |a: &RaceAttrs| {
            let mut h = Hasher::new();
            a.hash_into(&mut h);
            h.finish()
        };
        let base_hash = hash_of(&base);

        let mut element = base;
        element.element = Element::Water;
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
