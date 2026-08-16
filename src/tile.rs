//! Integer tile coordinates — the sole position type for every body and
//! ground item in the simulation (the OSRS-style tile-navigation rewrite).
//! Movement is discrete: a body occupies exactly one `Tile` between ticks,
//! never a fractional position, and every "how far / which way" computation
//! in the crate now runs in plain `i32`, not `Fx`.
//!
//! Deliberately not folded into `fx.rs` — that file's entire purpose is
//! being the *only* numeric type simulation state may hold (Invariant II),
//! and a `Tile` is not a scalar. Deliberately not folded into `terrain.rs`
//! either — a `Tile` has to exist independent of any particular `Terrain`
//! instance: `Entity`, `input::CmdKind`'s payloads, and `GroundItem` all need
//! one with no grid necessarily in scope to clamp against.

use crate::hash::{Hashable, Hasher};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Hash)]
pub struct Tile {
    pub x: i32,
    pub y: i32,
}

impl Tile {
    pub const ZERO: Tile = Tile { x: 0, y: 0 };

    #[inline]
    pub const fn new(x: i32, y: i32) -> Tile {
        Tile { x, y }
    }

    /// Clamp to `[0, side) × [0, side)` — the same bound `Terrain::index`
    /// already enforces internally, exposed here for positions that need
    /// clamping before (or without) ever reaching a `Terrain` (spawn
    /// placement, a proposed movement destination).
    #[inline]
    pub fn clamp(self, side: i32) -> Tile {
        let hi = (side - 1).max(0);
        Tile { x: self.x.clamp(0, hi), y: self.y.clamp(0, hi) }
    }

    #[inline]
    pub fn offset(self, dx: i32, dy: i32) -> Tile {
        Tile { x: self.x.saturating_add(dx), y: self.y.saturating_add(dy) }
    }
}

/// Chebyshev (king-move) distance — the natural reach metric on an
/// 8-directional grid: exactly 1 for any of the 8 immediate neighbours, no
/// sqrt/squaring needed anywhere a "how many tiles away" comparison used to
/// run on continuous `Fx` positions.
#[inline]
pub fn chebyshev_dist(a: Tile, b: Tile) -> i32 {
    (a.x - b.x).unsigned_abs().max((a.y - b.y).unsigned_abs()) as i32
}

/// Fixed visiting order for a 4-neighbour scan — N, E, S, W. Ties in a
/// "which neighbour is best" comparison break structurally (first in this
/// order to qualify wins), never by iteration accident — the same discipline
/// `Element::ALL`/`Race::ALL`'s own fixed orders already document.
pub const NEIGHBOURS_4: [(i32, i32); 4] = [(0, -1), (1, 0), (0, 1), (-1, 0)];

/// Fixed visiting order for an 8-neighbour scan — the 4 cardinal directions
/// above, then the 4 diagonals, in a stable order. Never reorder —
/// hash/iteration-visible the moment anything ties on it, same discipline as
/// `NEIGHBOURS_4`.
pub const NEIGHBOURS_8: [(i32, i32); 8] =
    [(0, -1), (1, -1), (1, 0), (1, 1), (0, 1), (-1, 1), (-1, 0), (-1, -1)];

/// One greedy step from `from` toward `to`, clamped to `{-1, 0, 1}` per axis
/// — the natural single-tile step on a king-move grid, no trig or
/// normalisation needed. Returns `from` unchanged if `to == from`.
#[inline]
pub fn step_toward(from: Tile, to: Tile) -> Tile {
    Tile {
        x: from.x + (to.x - from.x).signum(),
        y: from.y + (to.y - from.y).signum(),
    }
}

impl Hashable for Tile {
    fn hash_into(&self, h: &mut Hasher) {
        h.i32(self.x).i32(self.y);
    }
}
