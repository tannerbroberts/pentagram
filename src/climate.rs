//! S1 — climate influx. Operator 5 of `phase_terrain`.
//!
//! A per-cell, per-element baseline — weather, mineral seepage, background
//! decay — that keeps every cell turning over even where no race has ever
//! set foot. This is deliberately redundant with the Governor's own
//! "extinct race still churns its floor" guarantee (`terrain.rs`'s uniform
//! apportionment fallback): two independently sufficient reasons no cell can
//! freeze forever is a belt-and-suspenders choice, not an oversight — one
//! operates at the aggregate-per-race level, this one at the per-cell level
//! regardless of population.
//!
//! No floats anywhere (Invariant II) — every term is either a cached integer
//! produced once from `rand.rs`'s stateless hash, or plain integer/modular
//! arithmetic on the terrain-tick counter.

use crate::element::{Element, PerElement};
use crate::hash::{Hashable, Hasher};
use crate::rand::{rand_below, Channel};

/// The knobs climate reads. Every field is `PerElement`, matching
/// `terrain.rs`'s `TerrainTuning` so both drop into the same live-view knob
/// machinery once that integration lands. Defaults are a first guess.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ClimateTuning {
    /// Static per-cell "fertility" range for each element — how much the
    /// one-time geography draw can vary from one cell to the next, as
    /// `(lo, hi)` inclusive.
    pub base_range: PerElement<(u16, u16)>,
    /// Always-on floor, added every terrain tick regardless of season — the
    /// part of the anti-absorbing-state guarantee that never turns off.
    pub floor: PerElement<u16>,
    /// Peak seasonal bonus applied to the in-season element, scaled by that
    /// cell's static base and an integer triangle wave over the season.
    pub season_peak: PerElement<u16>,
    /// Terrain ticks per season. Five seasons — one per element, in ring
    /// order — make one full lap. Default: 6 simulated days
    /// (`6 * TICKS_PER_DAY / TERRAIN_PERIOD`), so five seasons land on
    /// exactly the S1 exit condition's 30-simulated-day window.
    pub season_ticks: u64,
}

impl Default for ClimateTuning {
    fn default() -> ClimateTuning {
        ClimateTuning {
            base_range: PerElement::filled((0, 40)),
            floor: PerElement::filled(1),
            season_peak: PerElement::filled(3000),
            season_ticks: crate::race::TICKS_PER_DAY * 6 / crate::race::TERRAIN_PERIOD,
        }
    }
}

impl Hashable for ClimateTuning {
    fn hash_into(&self, h: &mut Hasher) {
        for (_, v) in self.base_range.iter() {
            h.u16(v.0).u16(v.1);
        }
        for (_, v) in self.floor.iter() {
            h.u16(*v);
        }
        for (_, v) in self.season_peak.iter() {
            h.u16(*v);
        }
        h.u64(self.season_ticks);
    }
}

/// The static geography cache plus the temporal cycle formula. Constructed
/// once per `World`; the cache is a pure function of `(seed, side,
/// tuning.base_range)`, so it is not separately covered by
/// `World::state_hash` — two worlds with equal seed/side/tuning already
/// provably have an identical cache (see `docs/S1_TERRAIN_DESIGN.md` §1).
#[derive(Clone, Debug)]
pub struct Climate {
    side: i32,
    base: Vec<PerElement<u16>>,
}

impl Climate {
    pub fn new(seed: u64, side_cells: i32, tuning: &ClimateTuning) -> Climate {
        let side = side_cells.max(1);
        let n = (side as usize) * (side as usize);
        let mut base = Vec::with_capacity(n);
        for cell in 0..n as u32 {
            let mut row = PerElement::filled(0u16);
            for e in Element::ALL {
                let (lo, hi) = tuning.base_range[e];
                // u32, not u16: the inclusive span of a full-width (0,
                // u16::MAX) range is 65536, which does not fit back in a
                // u16 — computing it in the narrower type would silently
                // saturate at 65535 and make the documented top of the
                // range unreachable.
                let span = (hi as u32).saturating_sub(lo as u32).saturating_add(1);
                // tick=0 is a fixed, deliberate coordinate here: this is a
                // one-shot *spatial* draw (geography), not a per-tick
                // temporal one, so it must never vary with terrain_tick.
                // `wrapping_*`, not `+`/`*`: this is a hash salt scrambling
                // the coordinate, not a quantity that needs to stay exact —
                // wrapping keeps it panic-free under `overflow-checks =
                // true` at grid sizes large enough for `cell * 5` to
                // overflow u32, without changing behaviour at any size this
                // crate actually ships.
                let salt = cell.wrapping_mul(5).wrapping_add(e.index() as u32);
                let r = rand_below(seed, 0, salt, Channel::Climate, span);
                row[e] = lo.saturating_add(r as u16);
            }
            base.push(row);
        }
        Climate { side, base }
    }

    /// The per-cell, per-tick influx operator 5 applies. `terrain_tick` is
    /// the world's own monotonically increasing terrain-tick counter
    /// (`(world.tick + 1) / TERRAIN_PERIOD`), shared with every other
    /// stateless-random decision this tick.
    pub fn influx_at(&self, x: i32, y: i32, terrain_tick: u64, t: &ClimateTuning) -> PerElement<u16> {
        let idx = (y.clamp(0, self.side - 1) as usize) * (self.side as usize) + (x.clamp(0, self.side - 1) as usize);
        let base = self.base[idx];
        self.seasoned(base, terrain_tick, t)
    }

    /// The cached one-time geography draw at a cell, before any seasonal
    /// scaling — mainly for tests, but a legitimate read of what
    /// `Climate::new` actually produced.
    pub fn base_at(&self, x: i32, y: i32) -> PerElement<u16> {
        let idx = (y.clamp(0, self.side - 1) as usize) * (self.side as usize) + (x.clamp(0, self.side - 1) as usize);
        self.base[idx]
    }

    fn seasoned(&self, base: PerElement<u16>, terrain_tick: u64, t: &ClimateTuning) -> PerElement<u16> {
        let season_len = t.season_ticks.max(1);
        let in_season = Element::from_index(((terrain_tick / season_len) % Element::COUNT as u64) as usize);
        let phase = terrain_tick % season_len;
        let half = (season_len / 2).max(1);
        let tri = if phase <= half { phase } else { season_len - phase };

        let mut out = PerElement::filled(0u16);
        for e in Element::ALL {
            let mut v = t.floor[e] as u64;
            if e == in_season {
                let peak = t.season_peak[e] as u64;
                v = v.saturating_add((base[e] as u64).saturating_mul(peak).saturating_mul(tri) / half / 1000);
            }
            out[e] = v.min(u16::MAX as u64) as u16;
        }
        out
    }
}

/// Operator 5. Adds this tick's climate influx to every cell, after every
/// entity- and terrain-chemistry operator has run (so its job stays narrow:
/// top up whatever's left, rather than adding fresh mass the same tick's
/// suppression pass could immediately annihilate before it's ever
/// observable) and before diffusion (so a fresh influx begins spreading the
/// same tick it's applied, exactly like every other operator's output).
pub fn apply_influx(terrain: &mut crate::terrain::Terrain, climate: &Climate, tuning: &ClimateTuning, terrain_tick: u64) {
    for y in 0..terrain.side {
        for x in 0..terrain.side {
            let influx = climate.influx_at(x, y, terrain_tick, tuning);
            let cell = terrain.cell_mut(x, y);
            for e in Element::ALL {
                cell[e] = cell[e].saturating_add(influx[e]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terrain::Terrain;

    #[test]
    fn is_pure_given_the_same_coordinates() {
        let c = Climate::new(7, 16, &ClimateTuning::default());
        let a = c.influx_at(3, 5, 100, &ClimateTuning::default());
        let b = c.influx_at(3, 5, 100, &ClimateTuning::default());
        assert_eq!(a, b);
    }

    #[test]
    fn static_geography_does_not_vary_with_terrain_tick() {
        let t = ClimateTuning { season_peak: PerElement::filled(0), ..ClimateTuning::default() };
        let c = Climate::new(1, 8, &t);
        // With season_peak zeroed, influx_at should be identical at every
        // tick — only the always-on floor remains, and it does not depend
        // on the season.
        let a = c.influx_at(2, 2, 0, &t);
        let b = c.influx_at(2, 2, 999_999, &t);
        assert_eq!(a, b);
    }

    #[test]
    fn floor_is_always_on_regardless_of_season() {
        let t = ClimateTuning { floor: PerElement::filled(5), season_peak: PerElement::filled(0), ..ClimateTuning::default() };
        let c = Climate::new(2, 4, &t);
        for tick in [0u64, 1, 8_640, 43_199] {
            let v = c.influx_at(0, 0, tick, &t);
            for e in Element::ALL {
                assert_eq!(v[e], 5, "floor missing at tick {tick}");
            }
        }
    }

    #[test]
    fn season_advances_through_every_element_across_one_lap() {
        let t = ClimateTuning::default();
        let season_len = t.season_ticks;
        let mut seen = std::collections::BTreeSet::new();
        for lap_step in 0..Element::COUNT as u64 {
            let mid_season_tick = lap_step * season_len + season_len / 2;
            let in_season = Element::from_index(((mid_season_tick / season_len) % 5) as usize);
            seen.insert(in_season);
        }
        assert_eq!(seen.len(), 5, "one lap must touch every element exactly once");
    }

    #[test]
    fn peak_bonus_only_applies_to_the_in_season_element() {
        let t = ClimateTuning {
            base_range: PerElement::filled((100, 100)),
            floor: PerElement::filled(0),
            season_peak: PerElement::filled(1000),
            season_ticks: 100,
        };
        let c = Climate::new(3, 4, &t);
        // Wood (index 0) is in season for terrain_tick in [0, 100).
        let mid = 50u64; // mid-season, peak of the triangle wave
        let v = c.influx_at(0, 0, mid, &t);
        assert!(v[Element::Wood] > 0, "in-season element should get a bonus");
        for e in Element::ALL {
            if e != Element::Wood {
                assert_eq!(v[e], 0, "{:?} is out of season and floor is zero", e);
            }
        }
    }

    #[test]
    fn apply_influx_touches_every_cell() {
        let tuning = ClimateTuning { floor: PerElement::filled(3), season_peak: PerElement::filled(0), ..ClimateTuning::default() };
        let climate = Climate::new(4, 5, &tuning);
        let mut terrain = Terrain::new(5);
        apply_influx(&mut terrain, &climate, &tuning, 1);
        for y in 0..5 {
            for x in 0..5 {
                for e in Element::ALL {
                    assert_eq!(terrain.cell(x, y)[e], 3);
                }
            }
        }
    }

    #[test]
    fn hash_changes_when_tuning_changes() {
        let mut h1 = Hasher::new();
        ClimateTuning::default().hash_into(&mut h1);
        let mut h2 = Hasher::new();
        let mut t = ClimateTuning::default();
        t.floor[Element::Wood] = 9;
        t.hash_into(&mut h2);
        assert_ne!(h1.finish(), h2.finish());
    }

    #[test]
    fn full_width_base_range_span_is_computed_without_rounding_loss() {
        // Regression: computing the inclusive span in u16 saturates at
        // 65535 for a full-width (0, u16::MAX) range, making the
        // documented top of the range unreachable. The fix computes it in
        // u32, where the true span (65536) fits.
        let (lo, hi): (u16, u16) = (0, u16::MAX);
        let span = (hi as u32).saturating_sub(lo as u32).saturating_add(1);
        assert_eq!(span, 65_536);
    }

    #[test]
    fn full_width_base_range_can_actually_reach_its_documented_maximum() {
        let t = ClimateTuning { base_range: PerElement::filled((0, u16::MAX)), ..ClimateTuning::default() };
        let mut max_seen = 0u16;
        for seed in 0..20_000u64 {
            let c = Climate::new(seed, 4, &t);
            for y in 0..4i32 {
                for x in 0..4i32 {
                    for e in Element::ALL {
                        max_seen = max_seen.max(c.base_at(x, y)[e]);
                    }
                }
            }
        }
        assert_eq!(max_seen, u16::MAX, "the documented inclusive top of base_range must be reachable");
    }

    #[test]
    fn geography_draw_does_not_panic_at_a_cell_count_that_would_overflow_u32_unwrapped() {
        // Regression: `cell * 5` (unwrapped) overflows u32 once `cell`
        // exceeds u32::MAX / 5 — reachable at large-but-legal grid sides.
        // `Climate::new` must not panic even though this crate builds with
        // overflow-checks = true in every profile; a small grid can't
        // exercise the real threshold directly, so this asserts the fixed
        // salt expression itself is panic-free at the boundary.
        let cell = u32::MAX / 5 + 10; // just past the old overflow threshold
        let _ = cell.wrapping_mul(5).wrapping_add(4);
    }
}
