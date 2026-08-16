//! Stateless randomness — Invariant III.
//!
//! Two territories simulating the same entity inside a shared band must draw
//! the same number without having exchanged a byte. A sequential generator
//! cannot do that, because it would require both sides to have consumed the
//! same number of prior draws in the same order. So there is no generator
//! state anywhere: every draw is a pure hash of where and when it happened.
//!
//! The `Channel` argument is what makes the simulation tunable. Each logical
//! decision draws from its own channel, so adding a new random decision to
//! foraging never shifts the stream feeding collision resolution — and every
//! replay recorded before the change stays valid.

use crate::fx::Fx;

/// One channel per logical decision. Never reuse a discriminant; never
/// renumber an existing one, or you invalidate every recorded replay.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
#[repr(u8)]
pub enum Channel {
    /// Retired: continuous-movement positional jitter, gone along with the
    /// old physics-based `phase_movement`/`phase_collisions`. Kept, unread,
    /// per this enum's own "never reuse a discriminant" rule.
    MoveJitter = 0,
    Wander = 1,
    Collision = 2,
    LifespanVariance = 3,
    /// S2: offspring birth-placement scatter in `World::phase_feeding`. The
    /// catch itself is deterministic (in reach or not) — this is only the
    /// jitter so a cohort born from one parent does not stack exactly on it.
    Forage = 4,
    Crit = 5,
    SpawnPlacement = 6,
    Governor = 7,
    /// S1: terrain apportionment — the extinct-race uniform-fallback rotating
    /// offset and the largest-remainder tie-break (see `terrain.rs`).
    Terrain = 8,
    /// S3.3: per-predator-per-tick roll gating Animal-vs-Animal predation
    /// (`World::phase_feeding`) against a race's `hunt_weight`. Grazing Plant
    /// prey never rolls this channel — only the Animal-prey edge is a dial.
    /// See `docs/S3_ECOLOGY_LAYERS_DESIGN.md` §5.
    Hunt = 10,
    /// S3.5: `World::phase_flora`'s per-eligible-plant-per-attempt propagation
    /// roll, keyed by terrain tick. See docs/S3_ECOLOGY_LAYERS_DESIGN.md section 7.
    Propagate = 11,
    /// S3.5: `World::phase_flora`'s x/y scatter draw for where a new
    /// offspring roots, relative to its parent.
    Disperse = 12,
    /// Reserved for ad-hoc debugging. Never read by shipped simulation code.
    Debug = 255,
}

/// The primitive draw. SplitMix64 finalisation over a mixed coordinate tuple.
#[inline]
pub fn rand(seed: u64, tick: u64, id: u32, ch: Channel) -> u32 {
    let mut h = seed
        ^ tick.wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ ((id as u64) << 17)
        ^ ((ch as u64) << 3);
    h ^= h >> 30;
    h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94D0_49BB_1331_11EB);
    (h ^ (h >> 31)) as u32
}

/// Uniform in `[0, n)`. Uses the multiply-high reduction rather than modulo,
/// which keeps the distribution flat without a rejection loop — and a
/// rejection loop would make the cost of a draw data-dependent, which is a
/// timing side channel and an inconsistency risk under a tick budget.
#[inline]
pub fn rand_below(seed: u64, tick: u64, id: u32, ch: Channel, n: u32) -> u32 {
    if n == 0 {
        return 0;
    }
    (((rand(seed, tick, id, ch) as u64) * (n as u64)) >> 32) as u32
}

/// Uniform in `[lo, hi)`. Returns `lo` if the range is empty.
#[inline]
pub fn rand_range(seed: u64, tick: u64, id: u32, ch: Channel, lo: i32, hi: i32) -> i32 {
    if hi <= lo {
        return lo;
    }
    let span = (hi as i64 - lo as i64) as u32;
    lo + rand_below(seed, tick, id, ch, span) as i32
}

/// Uniform in `[0, 1)` as a fixed-point value.
#[inline]
pub fn rand_unit(seed: u64, tick: u64, id: u32, ch: Channel) -> Fx {
    Fx::from_raw((rand(seed, tick, id, ch) >> 16) as i32)
}

/// Uniform in `[-1, 1)` as a fixed-point value.
#[inline]
pub fn rand_signed(seed: u64, tick: u64, id: u32, ch: Channel) -> Fx {
    Fx::from_raw(((rand(seed, tick, id, ch) >> 15) as i32) - crate::fx::ONE_RAW)
}

/// True with probability `num / den`.
#[inline]
pub fn rand_chance(seed: u64, tick: u64, id: u32, ch: Channel, num: u32, den: u32) -> bool {
    if den == 0 {
        return false;
    }
    rand_below(seed, tick, id, ch, den) < num
}
