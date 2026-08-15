//! Direct, `World`-free proof that diffusion never moves mass further than
//! one cell per terrain tick (Invariant I) lives in `terrain.rs`'s own unit
//! tests (`diffusion_never_moves_further_than_one_cell_per_call`,
//! `diffusion_never_exceeds_the_flat_cap_per_edge`), not duplicated here.
//!
//! This file used to also carry S1's exit condition as two `#[ignore]`d
//! 30-day tests ("succession visibly cycling with no absorbing state"),
//! including a strong claim that terrain alone, with zero population, must
//! keep moving forever. That claim depended on an always-on, per-cell,
//! population-independent terrain influx mechanism that has since been torn
//! out entirely — there is no longer any mechanism that moves terrain
//! independent of what living bodies do, and there does not need to be one:
//! population never actually goes to zero and stays there (every race has
//! its own spawn/reproduction path back into the ecosystem), so an
//! absorbing-state guarantee for a permanently empty world was never a real
//! requirement. Both tests, and the helpers that existed only to serve them,
//! are deleted along with that mechanism rather than patched to keep
//! compiling.

use pentagram::element::Element;
use pentagram::replay::scripted_log;
use pentagram::{World, TERRAIN_PERIOD};

const SEED: u64 = 0xC0FFEE;

/// Fast, non-ignored smoke test kept in the default `cargo test` path:
/// two independent runs from the same seed must still agree bit-for-bit
/// once terrain is in the loop, and the grid must actually have moved.
/// A regression in operator order, a missed saturating op, or a
/// non-deterministic apportionment tie-break would show up here in well
/// under a second, long before a 30-day `--ignored` run would catch it.
#[test]
fn terrain_replays_bit_identically_and_the_grid_moves() {
    let ticks = TERRAIN_PERIOD * 40; // 40 terrain ticks — enough for every operator to fire repeatedly
    let log = scripted_log(0x5EED, ticks, 20);

    let mut a = World::new(SEED, 24);
    // Terrain starts all-zero and there is no longer any population-
    // independent influx, so a race's habitat draw needs something to
    // actually draw from before conversion can move anything -- same
    // reasoning as `tests/conservation.rs`'s own terrain seeding.
    for y in 0..24i32 {
        for x in 0..24i32 {
            for e in Element::ALL {
                a.terrain.cell_mut(x, y)[e] = 20_000;
            }
        }
    }
    a.seed_population(4);
    let fresh_hash = a.terrain.state_hash();
    let mut b = a.clone();

    for _ in 0..ticks {
        a.step(&log);
        b.step(&log);
    }

    assert_eq!(
        a.state_hash(),
        b.state_hash(),
        "two independent runs from the same seed must agree once terrain is in the loop"
    );
    assert_ne!(
        a.terrain.state_hash(),
        fresh_hash,
        "the terrain grid must have changed over twelve simulated hours"
    );
}
