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
