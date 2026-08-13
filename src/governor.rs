//! Bounded churn — the guarantee that makes terrain forecastable.
//!
//! Player behaviour decides *where* the world changes and *through which
//! channel*. It does not decide *how fast*. Every race's deposition and
//! consumption passes through a governor that enforces three properties, per
//! terrain tick, per race, per territory:
//!
//!   1. **Never below the floor.** Emitted even at zero demand, even if the
//!      race is extinct on this server. The world keeps turning over whether
//!      or not anyone is playing, which is also what stops a lost biome from
//!      becoming an absorbing state.
//!   2. **Never above the ceiling.** No amount of coordination, exploitation,
//!      or population spike moves more terrain in one tick than this.
//!   3. **Long-run average converges to nominal.** Bursting above nominal
//!      spends a bucket that only refills at nominal, so a sustained maximum
//!      effort and a sustained minimum effort differ in *timing*, not in total.
//!
//! Together these mean the terrain state at `T + k` is bounded before any
//! player has decided anything — which is what "legible, forecastable terrain"
//! actually requires.

use crate::hash::{Hashable, Hasher};
use crate::race::RateBand;

/// The outcome of one settlement. Every field is a metric worth surfacing in
/// the tuning harness.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Grant {
    /// What actually gets applied to the terrain field this tick.
    pub granted: u64,
    /// The part of `granted` emitted purely to honour the floor, because
    /// demand fell short. High values mean a race is absent or idle here.
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
    /// The floor is emitted for free and does **not** drain the bucket: it is
    /// the world's own churn, not the race's effort. That distinction is what
    /// lets an extinct race still turn its terrain over.
    pub fn settle(&mut self, demand: u64) -> Grant {
        self.bucket = self
            .bucket
            .saturating_add(self.band.nominal as u64)
            .min(self.band.burst_cap());

        // Demand can be paid only out of banked budget, and never beyond the
        // per-tick ceiling.
        let payable = demand.min(self.bucket).min(self.band.ceiling as u64);
        self.bucket -= payable;

        let clipped = demand - payable;
        let granted = payable.max(self.band.floor as u64);
        let forced = granted - payable;

        Grant { granted, forced, clipped }
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
    use crate::element::Element;
    use crate::race::attrs;

    fn band() -> RateBand {
        RateBand::new(100, 1000, 5000, 10)
    }

    #[test]
    fn zero_demand_still_emits_the_floor() {
        let mut g = Governor::new(band());
        for _ in 0..1000 {
            let grant = g.settle(0);
            assert_eq!(grant.granted, 100);
            assert_eq!(grant.forced, 100);
            assert_eq!(grant.clipped, 0);
        }
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
    fn granted_is_always_inside_the_band_for_any_demand_pattern() {
        // The load-bearing property: no behaviour, adversarial or otherwise,
        // moves the rate outside [floor, ceiling].
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
                grant.granted >= b.floor as u64 && grant.granted <= b.ceiling as u64,
                "tick {}: granted {} outside [{}, {}]",
                t,
                grant.granted,
                b.floor,
                b.ceiling
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
    fn forced_is_zero_when_demand_meets_the_floor() {
        let mut g = Governor::new(band());
        let grant = g.settle(500);
        assert_eq!(grant.granted, 500);
        assert_eq!(grant.forced, 0);
    }

    #[test]
    fn every_shipped_race_band_survives_the_hostile_pattern() {
        use crate::rand::{rand_below, Channel};
        for e in Element::ALL {
            let a = attrs(e);
            for (label, b) in [("deposit", a.deposit), ("consume", a.consume)] {
                let mut g = Governor::new(b);
                for t in 0..5_000u64 {
                    let demand = if rand_below(1, t, e.index() as u32, Channel::Governor, 3) == 0 {
                        0
                    } else {
                        u64::from(u32::MAX)
                    };
                    let grant = g.settle(demand);
                    assert!(
                        grant.granted >= b.floor as u64 && grant.granted <= b.ceiling as u64,
                        "{} {} tick {}: {} outside [{}, {}]",
                        e.name(),
                        label,
                        t,
                        grant.granted,
                        b.floor,
                        b.ceiling
                    );
                }
            }
        }
    }

    #[test]
    fn forecast_bounds_hold_over_a_window() {
        // What forecastability actually means: before any player acts, the
        // total change over the next k ticks is already bounded.
        let b = band();
        let mut g = Governor::new(b);
        let k = 500u64;
        let mut total = 0u64;
        for t in 0..k {
            total += g.settle(if t % 7 == 0 { 99_999 } else { 0 }).granted;
        }
        assert!(total >= (b.floor as u64) * k);
        assert!(total <= (b.ceiling as u64) * k);
    }
}
