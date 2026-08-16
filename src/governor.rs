//! Bounded churn — the guarantee that makes terrain forecastable.
//!
//! Player behaviour decides *where* the world changes and *through which
//! channel*. It does not decide *how fast*. A bounded aggregate rate (e.g.
//! `TerrainTuning::ground_decay`) passes through a governor that enforces
//! two properties, per terrain tick, per territory:
//!
//!   1. **Never above the ceiling.** No amount of coordination, exploitation,
//!      or population spike moves more terrain in one tick than this.
//!   2. **Long-run average converges to nominal.** Bursting above nominal
//!      spends a bucket that only refills at nominal, so a sustained maximum
//!      effort and a sustained minimum effort differ in *timing*, not in total.
//!
//! Together these mean the terrain state at `T + k` is bounded before any
//! player has decided anything — which is what "legible, forecastable terrain"
//! actually requires.
//!
//! # Invariant VIII and the retired floor guarantee
//!
//! Before Invariant VIII (material conservation), this governor made a third
//! promise: **never below the floor**, emitted even at zero demand, even if
//! the race was extinct on this server — material conjured from nothing so a
//! lost biome could never become a permanently frozen absorbing state. That
//! guarantee is fundamentally incompatible with conservation (there is
//! nothing to conjure the floor's material *from*) and is retired here, on
//! purpose. [`Governor::settle`] now never grants more than `demand` allows,
//! full stop — an extinct race's own terrain genuinely stops changing from
//! that race's own activity. This is a real, deliberate trade-off, not an
//! oversight: other races' own conversions can still move an extinct race's
//! terrain layer (every layer stays readable/writable by every race's ring
//! relations), so "frozen" here means "no longer moved by a race that no
//! longer exists," not "frozen for the whole grid."
//!
//! `RateBand.floor` survives, repurposed rather than removed: it used to be
//! a source of free material; it is now a **reserve** — the portion of the
//! banked bucket a single tick may never spend down past (see `settle`
//! below). A race with a large floor keeps more of its own banked budget in
//! reserve at all times; a race with `floor == 0` can spend its whole bucket
//! in one tick, same as before Invariant VIII, just without ever creating
//! anything beyond what the bucket already holds.

use crate::hash::{Hashable, Hasher};
use crate::race::RateBand;

/// The outcome of one settlement. Every field is a metric worth surfacing in
/// the tuning harness.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Grant {
    /// What actually gets applied to the terrain field this tick. Always
    /// `<= demand` — see Invariant VIII's note on the retired floor above.
    pub granted: u64,
    /// Retained for shape/hash continuity but structurally always `0` now:
    /// nothing is ever forced into existence to honour a floor anymore (see
    /// the module doc's Invariant VIII note). Kept as a field rather than
    /// deleted so a future reintroduction of some other "forced" concept —
    /// or a soak harness still reading it — has somewhere to land.
    pub forced: u64,
    /// Demand refused. Non-zero means this race is rate-limited right now —
    /// the single most useful signal for spotting an exploit or a runaway.
    pub clipped: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Governor {
    band: RateBand,
    /// Banked budget, capped at `band.burst_cap()`.
    bucket: u64,
}

impl Governor {
    pub fn new(band: RateBand) -> Governor {
        debug_assert!(
            band.is_valid(),
            "rate band must satisfy floor <= nominal <= ceiling"
        );
        // Start full, so a freshly started territory can respond immediately
        // rather than spending its first burst window accumulating.
        Governor { band, bucket: band.burst_cap() }
    }

    #[inline]
    pub fn band(&self) -> RateBand {
        self.band
    }

    /// Re-band a running governor — the live view retuning a knob mid-flight.
    ///
    /// The banked bucket carries over but is clamped to the new burst cap, so
    /// lowering a band can never leave a governor holding a burst the new band
    /// does not permit.
    pub fn set_band(&mut self, band: RateBand) {
        self.band = band;
        self.bucket = self.bucket.min(band.burst_cap());
    }

    #[inline]
    pub fn bucket(&self) -> u64 {
        self.bucket
    }

    /// Settle one terrain tick's accumulated demand.
    ///
    /// Under Invariant VIII (material conservation) a grant can only ever be
    /// as large as what is actually payable — there is no floor-forced
    /// minimum anymore (see the module doc). `floor` is repurposed as a
    /// reserve: the bucket is never spent down past it in a single tick, so
    /// `floor` now bounds how much of the race's *own* banked budget one
    /// tick may draw on, not a guarantee of free material.
    pub fn settle(&mut self, demand: u64) -> Grant {
        self.bucket = self
            .bucket
            .saturating_add(self.band.nominal as u64)
            .min(self.band.burst_cap());

        // Demand can be paid only out of the spendable bucket (banked budget
        // above the reserve), and never beyond the per-tick ceiling.
        let spendable = self.bucket.saturating_sub(self.band.floor as u64);
        let payable = demand.min(spendable).min(self.band.ceiling as u64);
        self.bucket -= payable;

        let clipped = demand - payable;

        Grant { granted: payable, forced: 0, clipped }
    }
}

impl Hashable for Governor {
    fn hash_into(&self, h: &mut Hasher) {
        h.u32(self.band.floor)
            .u32(self.band.nominal)
            .u32(self.band.ceiling)
            .u32(self.band.burst_ticks)
            .u64(self.bucket);
    }
}

impl Hashable for Grant {
    fn hash_into(&self, h: &mut Hasher) {
        h.u64(self.granted).u64(self.forced).u64(self.clipped);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn band() -> RateBand {
        RateBand::new(100, 1000, 5000, 10)
    }

    #[test]
    fn zero_demand_grants_nothing() {
        // Invariant VIII: the old create-from-nothing floor is retired.
        // Zero demand now settles to zero granted, every tick, forever — an
        // extinct or idle race's terrain simply stops moving from its own
        // activity. `forced` is structurally always 0 now too.
        let mut g = Governor::new(band());
        for _ in 0..1000 {
            let grant = g.settle(0);
            assert_eq!(grant.granted, 0);
            assert_eq!(grant.forced, 0);
            assert_eq!(grant.clipped, 0);
        }
    }

    #[test]
    fn floor_reserves_a_slice_of_the_bucket_from_being_spent_in_one_tick() {
        // Repurposed meaning: `floor` no longer manufactures a minimum
        // grant — it caps how far a single tick may draw the bucket down.
        // burst_ticks=1 so the bucket starts at exactly `nominal`, isolating
        // the reserve's effect from the ceiling (which is high enough here
        // never to bind).
        let b = RateBand::new(200, 1000, 100_000, 1);
        let mut g = Governor::new(b);
        let grant = g.settle(100_000);
        assert_eq!(
            grant.granted, 800,
            "bucket starts at nominal (1000); floor (200) must stay unspent"
        );
        assert_eq!(g.bucket(), 200, "the reserve itself must remain banked");
    }

    #[test]
    fn ceiling_holds_against_unbounded_demand() {
        let mut g = Governor::new(band());
        for _ in 0..10_000 {
            let grant = g.settle(u64::MAX / 2);
            assert!(
                grant.granted <= 5000,
                "ceiling breached: {} > 5000",
                grant.granted
            );
        }
    }

    #[test]
    fn granted_never_exceeds_the_ceiling_or_the_demand_for_any_pattern() {
        // The load-bearing property under Invariant VIII: no behaviour,
        // adversarial or otherwise, moves the grant above the ceiling, and
        // it never exceeds demand either — nothing is ever manufactured.
        use crate::rand::{rand_below, Channel};
        let b = band();
        let mut g = Governor::new(b);
        for t in 0..50_000u64 {
            // A deliberately hostile demand pattern: long silences punctuated
            // by enormous coordinated spikes.
            let demand = match rand_below(0xABCD, t, 0, Channel::Governor, 10) {
                0..=5 => 0,
                6..=8 => rand_below(0xABCD, t, 1, Channel::Governor, 2000) as u64,
                _ => u64::from(u32::MAX),
            };
            let grant = g.settle(demand);
            assert!(
                grant.granted <= b.ceiling as u64 && grant.granted <= demand,
                "tick {}: granted {} outside [0, {}] or exceeded demand {}",
                t,
                grant.granted,
                b.ceiling,
                demand
            );
        }
    }

    #[test]
    fn sustained_maximum_effort_converges_to_nominal() {
        // Bursting is a timing advantage, never a total-throughput advantage.
        let b = band();
        let mut g = Governor::new(b);
        let ticks = 20_000u64;
        let mut total = 0u64;
        for _ in 0..ticks {
            total += g.settle(u64::MAX / 2).granted;
        }
        let avg = total / ticks;
        assert!(
            avg >= b.nominal as u64 && avg <= (b.nominal as u64) * 105 / 100,
            "average {} should converge to nominal {}",
            avg,
            b.nominal
        );
    }

    #[test]
    fn a_burst_is_available_then_spent() {
        let mut g = Governor::new(band());
        // Starts full: 10 ticks of nominal banked, so the first tick can pay
        // out at the ceiling.
        let first = g.settle(u64::MAX / 2);
        assert_eq!(first.granted, 5000);
        // Sustained maximum drains the bucket down to the refill rate.
        for _ in 0..50 {
            g.settle(u64::MAX / 2);
        }
        let settled = g.settle(u64::MAX / 2);
        assert_eq!(
            settled.granted, 1000,
            "after the bucket drains, throughput is nominal"
        );
    }

    #[test]
    fn clipped_reports_refused_demand() {
        let mut g = Governor::new(band());
        for _ in 0..100 {
            g.settle(u64::MAX / 2);
        }
        let grant = g.settle(10_000);
        assert!(grant.clipped > 0, "over-demand should register as clipped");
        assert_eq!(grant.granted + grant.clipped, 10_000);
    }

    #[test]
    fn forced_is_always_zero_now_that_the_floor_no_longer_forces_emission() {
        let mut g = Governor::new(band());
        for demand in [0u64, 500, 10_000, u64::from(u32::MAX)] {
            let grant = g.settle(demand);
            assert_eq!(grant.forced, 0, "demand {demand}");
        }
    }

    #[test]
    fn the_shipped_ground_decay_band_survives_the_hostile_pattern() {
        // Every per-race `RateBand` (`RaceAttrs.consume`) was retired along
        // with `race::Conversion` in the action-recipe migration; the one
        // shipped `RateBand` left in the crate is `TerrainTuning::
        // ground_decay`. Same hostile alternating-zero/max demand pattern
        // this test always used, now run against that band instead of
        // iterating every race's own.
        use crate::rand::{rand_below, Channel};
        let b = crate::terrain::TerrainTuning::default().ground_decay;
        let mut g = Governor::new(b);
        for t in 0..5_000u64 {
            let demand = if rand_below(1, t, 0, Channel::Governor, 3) == 0 { 0 } else { u64::from(u32::MAX) };
            let grant = g.settle(demand);
            assert!(
                grant.granted <= b.ceiling as u64 && grant.granted <= demand,
                "tick {}: {} outside [0, {}] or exceeded demand {}",
                t,
                grant.granted,
                b.ceiling,
                demand
            );
        }
    }

    #[test]
    fn forecast_bounds_hold_over_a_window() {
        // What forecastability actually means: before any player acts, the
        // total change over the next k ticks is already bounded above.
        // Invariant VIII retires the floor's lower bound (see the module
        // doc) — there is no longer a guaranteed *minimum* turnover, only a
        // guaranteed maximum.
        let b = band();
        let mut g = Governor::new(b);
        let k = 500u64;
        let mut total = 0u64;
        for t in 0..k {
            total += g.settle(if t % 7 == 0 { 99_999 } else { 0 }).granted;
        }
        assert!(total <= (b.ceiling as u64) * k);
    }
}
