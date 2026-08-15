//! S1 — the terrain field.
//!
//! Five `u16` saturations per cell. `phase_terrain` in `world.rs` (run right
//! after `phase_settle`) now runs a fixed four-slot sequence every terrain
//! tick, but only two of those slots are terrain operators now — this file
//! owns:
//!
//! ```text
//!   1 conversion    6 diffusion
//! ```
//!
//! **Invariant VIII update.** Slots 1 and 2 used to be independent `deposit`
//! and `consume` operators, each spending its own separately governed
//! budget — the deposit side able to write more of a race's own element than
//! it had ever drawn from anywhere, i.e. conjuring matter. Under material
//! conservation there is one coupled operator instead:
//! [`apply_conversion`] reads a race's already-settled habitat draw and
//! converts it, at the race's own fixed ratio, into its own element, split
//! across background terrain deposit / the drawing bodies' own held
//! material / explicit waste — see that function's own doc comment for the
//! exact accounting. The slot count this file owns drops from four to
//! three; `world.rs`'s `phase_terrain` doc comment carries the equivalent
//! note for the *other* two (ecology's) slots.
//!
//! Slots that used to be `ring` and `star` (terrain converting a permille of
//! a cell's own stock into the next ring element, and terrain nullifying a
//! permille of a cell's stock against its suppressor's — both ran every
//! terrain tick with no entity involved at all, terrain acting on itself)
//! are gone too, predating this change. The ring and star *relations* still
//! matter, but they now read terrain and act on bodies instead of the other
//! way around — `ecology.rs`'s `apply_attrition` (ring's relation,
//! `eaten_by()`, redirected to body hp) and `apply_suppression` (star's
//! relation, `suppressed_by()`, redirected to body `hunger`) fill two slots
//! in `phase_terrain`, gated at the same terrain-tick cadence. Terrain only
//! changes because of what bodies do; it does not act on its own.
//!
//! The slot order is still a wire format exactly the way `World::step`'s
//! phase order already is — reordering any two of these changes every
//! recorded replay. See `docs/S1_TERRAIN_DESIGN.md` for the full design and
//! the reasoning behind the order, the deposit/consume element mapping, and
//! every other choice this file makes that the README's one-line S1 spec
//! left open (predates Invariant VIII; read `apply_conversion`'s doc comment
//! for what changed since).
//!
//! **Conversion writes `race.element` (self, the produced side) and removes
//! `race.element.habitat()`** (same ring-backward math as `eats()`/
//! `eats_animal()`, read here as terrain consumption rather than predation)
//! — a Fire body's deposit is a scorch mark on the Fire layer, its draw
//! burns down the Wood layer under it. This is this design's interpretive
//! call, not a README fact (see the design doc §9.2).
//!
//! Diffusion is not per-cell — it reads neighbours — so it snapshots the
//! *whole grid* into a scratch double-buffer before writing anything back.
//! Every arithmetic op on a cell value is saturating: `overflow-checks =
//! true` in every profile means a bare `u16 + u16` panics on overflow, and
//! Invariant II's own discipline (saturate rather than wrap or panic)
//! applies here exactly as it does in `fx.rs`.

use std::collections::BTreeMap;

use crate::element::{Element, PerElement};
use crate::entity::Entity;
use crate::fx::V2;
use crate::governor::Grant;
use crate::hash::{Hashable, Hasher};
use crate::race::{PerRace, Race, RaceAttrs};
use crate::rand::{rand_below, Channel};

/// Element colours. The single definition — `tuning::RGB` re-exports this
/// rather than keeping its own copy, so the filmstrip renderer (`blend_rgb`,
/// below) and every live-view client read one table instead of two that can
/// drift apart.
pub const RGB: PerElement<(u8, u8, u8)> = PerElement([
    (127, 176, 105), // Wood
    (226, 105, 74),  // Fire
    (211, 164, 69),  // Earth
    (173, 164, 206), // Metal
    (89, 160, 198),  // Water
]);

/// The rate knobs the six operators read. Every field is `PerElement`, so it
/// drops straight into the existing `Knob`/`Page` live-view machinery
/// (`chaos/knobs.rs`) unmodified whenever that integration lands.
///
/// Every number here is a first guess, in the same spirit `race.rs`'s own
/// header states of itself: a starting point for the tuning loop, not a
/// derived constant.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TerrainTuning {
    /// Permille of the concentration difference between two neighbouring
    /// cells that moves across that edge each terrain tick, before the flat
    /// cap below is applied. Operator 6.
    pub diffuse_rate: PerElement<u16>,
    /// Flat units per edge per terrain tick — the literal, concrete content
    /// of Invariant I at S1: no matter how saturated a source cell is, one
    /// edge can never carry more than this in one terrain tick.
    pub diffuse_cap: PerElement<u16>,
}

impl Default for TerrainTuning {
    fn default() -> TerrainTuning {
        TerrainTuning {
            diffuse_rate: PerElement::filled(200),
            diffuse_cap: PerElement::filled(40),
        }
    }
}

impl Hashable for TerrainTuning {
    fn hash_into(&self, h: &mut Hasher) {
        for (_, v) in self.diffuse_rate.iter() {
            h.u16(*v);
        }
        for (_, v) in self.diffuse_cap.iter() {
            h.u16(*v);
        }
    }
}

/// The grid. Row-major, cell-major: `index = y * side + x`, and all five
/// saturations of one cell are contiguous — Invariant IV, and the one order
/// every operator, the hash, and any future renderer iterate in.
#[derive(Clone, Debug)]
pub struct Terrain {
    /// Cells per side. Always equal to the `World::size` this terrain was
    /// built with (1:1 resolution with the entity coordinate space — see
    /// the design doc §1/§9.1).
    pub side: i32,
    cells: Vec<PerElement<u16>>,
    /// Reused scratch buffer for diffusion's whole-grid snapshot (operator
    /// 6). Allocated once at construction and never resized, so a terrain
    /// tick after the first does zero allocation for diffusion.
    scratch: Vec<PerElement<u16>>,
    /// Invariant VIII (bug 2 fix, later widened for the `deposit_at` gap):
    /// `apply_conversion`'s deposit/waste side writes to a race's occupied
    /// cells via `apportion`, which saturates a cell rather than exceed
    /// `u16::MAX` (Invariant II) — a cell already near the ceiling can
    /// silently accept less than requested. Rather than let that shortfall
    /// simply vanish, `apply_conversion` banks it here, per race and per
    /// element, and retries it (added on top of that race's own next
    /// deposit) every subsequent terrain tick until it fully lands.
    /// Diffusion (operator 6, same tick, right after conversion) keeps
    /// draining saturated cells toward their neighbours in the meantime, so
    /// a banked shortfall is delayed, never stuck forever.
    ///
    /// Per-element, not just per-race, because `apply_conversion` is not
    /// this ledger's only writer: `deposit_at` (a single-cell material
    /// return — corpse decomposition, smelting tailings, a broken item's
    /// quantity) can saturate too, and unlike `apply_conversion` — which
    /// only ever deposits a race into its own element's layer — a
    /// `deposit_at` shortfall can be for *any* element a body happened to
    /// be carrying or holding as an item, attributed to that body's own
    /// race so it retries the same way, spread across that race's currently
    /// occupied cells (or the extinct-race uniform fallback if none are
    /// left alive), right alongside `apply_conversion`'s own retry. Indexed
    /// `[race][element]`; the race's own element channel is what
    /// `apply_conversion` reads/writes, every other channel is
    /// `deposit_at`-only. Hashed like every other piece of terrain state
    /// (Invariant VI) — two worlds that diverge only in banked overflow are
    /// not actually in the same state.
    overflow: PerRace<PerElement<u64>>,
}

impl Terrain {
    pub fn new(side_cells: i32) -> Terrain {
        let side = side_cells.max(1);
        let n = (side as usize) * (side as usize);
        Terrain {
            side,
            cells: vec![PerElement::filled(0u16); n],
            scratch: vec![PerElement::filled(0u16); n],
            overflow: PerRace::filled(PerElement::filled(0)),
        }
    }

    #[inline]
    fn usize_side(&self) -> usize {
        self.side as usize
    }

    #[inline]
    pub fn index(&self, x: i32, y: i32) -> usize {
        let side = self.usize_side();
        (y.clamp(0, self.side - 1) as usize) * side + (x.clamp(0, self.side - 1) as usize)
    }

    #[inline]
    fn xy_of(&self, index: u32) -> (i32, i32) {
        let side = self.side.max(1);
        ((index as i32) % side, (index as i32) / side)
    }

    /// Which cell a world position falls in — the same floor/clamp
    /// `World::phase_movement` already performs on entity positions.
    #[inline]
    pub fn cell_of(&self, pos: V2) -> (i32, i32) {
        (
            pos.x.floor_int().clamp(0, self.side - 1),
            pos.y.floor_int().clamp(0, self.side - 1),
        )
    }

    #[inline]
    pub fn cell(&self, x: i32, y: i32) -> &PerElement<u16> {
        &self.cells[self.index(x, y)]
    }

    #[inline]
    pub fn cell_mut(&mut self, x: i32, y: i32) -> &mut PerElement<u16> {
        let i = self.index(x, y);
        &mut self.cells[i]
    }

    /// Total saturation of one element across the whole grid. Used by the
    /// exit-condition checks (dominant element, starved/saturated/monoculture).
    pub fn total(&self, e: Element) -> u64 {
        self.cells.iter().map(|c| c[e] as u64).sum()
    }

    /// Set every cell's `element` to exactly `amount`, leaving every other
    /// element untouched. A one-time genesis choice for `World::new` to call
    /// -- not an ongoing population-independent per-tick influx (Invariant
    /// VIII closed off that whole category of source): this fixes the
    /// *starting* state once, the same category of choice as any other
    /// initial condition (`RACES`' shipped defaults, an entity's starting
    /// `hp`), not a create-from-nothing operator that runs during the
    /// simulation.
    pub fn seed_uniform(&mut self, element: Element, amount: u16) {
        for c in &mut self.cells {
            c[element] = amount;
        }
    }

    pub fn state_hash(&self) -> u64 {
        let mut h = Hasher::new();
        self.hash_into(&mut h);
        h.finish()
    }

    /// A race's currently-banked shortfall on its own element's channel
    /// (bug 2 fix) -- see `overflow`'s own doc comment. Exposed read-only,
    /// mainly for tests; `apply_conversion` is this channel's writer.
    pub fn overflow(&self, r: Race) -> u64 {
        self.overflow[r][r.element]
    }

    /// A race's currently-banked shortfall on an arbitrary element channel
    /// -- `overflow` above is a convenience shorthand for this restricted
    /// to the race's own element. This general form is what covers
    /// `deposit_at`'s banked shortfalls (corpse decomposition, smelting
    /// tailings, broken items) on elements other than the race's own.
    /// Exposed read-only, mainly for tests.
    pub fn overflow_of(&self, r: Race, e: Element) -> u64 {
        self.overflow[r][e]
    }
}

impl Hashable for Terrain {
    fn hash_into(&self, h: &mut Hasher) {
        h.i32(self.side);
        for c in &self.cells {
            for (_, v) in c.iter() {
                h.u16(*v);
            }
        }
        for (_, per_element) in self.overflow.iter() {
            for (_, v) in per_element.iter() {
                h.u64(*v);
            }
        }
    }
}

// ------------------------------------------------------------------
// Spatializing the Governor — §3 of the design doc. `Governor` itself is
// untouched: it still runs once per race per terrain tick and still
// produces one aggregate `Grant`. Everything below is a pure, deterministic
// distribution of that one number across the cells a race's living bodies
// currently occupy.
// ------------------------------------------------------------------

/// One terrain tick's occupancy: for each race, which cells its living
/// bodies are in and how many. Built once per terrain tick and shared by
/// both the deposit and the consume operator. Race-shaped, not
/// element-shaped: a Wood-Plant and a Wood-Animal both write the Wood
/// terrain layer, but each accrues its own governor demand and must be
/// apportioned separately (see `apply_conversion` below).
///
/// `BTreeMap` rather than a hash map — same reason `entity.rs`'s own tests
/// reach for `BTreeSet`: Invariant IV requires a defined iteration order,
/// and a sorted-by-cell-index map gives one for free.
pub struct Occupancy {
    weight: PerRace<BTreeMap<u32, u32>>,
}

impl Occupancy {
    pub fn build(entities: &[Entity], terrain: &Terrain) -> Occupancy {
        let mut weight: PerRace<BTreeMap<u32, u32>> = PerRace::default();
        for e in entities {
            if !e.alive {
                continue;
            }
            let (x, y) = terrain.cell_of(e.pos);
            let idx = terrain.index(x, y) as u32;
            let race = e.race();
            *weight.get_mut(race).entry(idx).or_insert(0) += 1;
        }
        Occupancy { weight }
    }

    /// How many bodies of `race` currently occupy the terrain cell at
    /// `cell_index` (`Terrain::index`'s output). `phase_flora`'s `crowd_max`
    /// gate needs this (S3.5); `apply_conversion` doesn't -- it
    /// iterate the whole weight map by other means already.
    pub fn count(&self, race: Race, cell_index: u32) -> u32 {
        self.weight.get(race).get(&cell_index).copied().unwrap_or(0)
    }
}

/// A uniform-grid broadphase over currently-alive entities, bucketed by the
/// same 1:1 cell grid `Terrain` already uses. `World::phase_movement`
/// (behavior sensing), `phase_collisions`, and `phase_feeding` used to each
/// scan every other entity per candidate -- three separate O(n²) passes per
/// tick, "correct and fast enough for Stage 0" per this file's own long-
/// standing note, with a uniform-grid broadphase always the intended fix
/// once something needed it (see `docs/S1_TERRAIN_DESIGN.md`'s file-layout
/// table: *"Terrain's row-major indexing is ready for it, but nothing
/// consumes it yet"*). This is that broadphase.
///
/// Not a cache: entity positions move between `phase_movement`,
/// `phase_collisions`, and `phase_feeding`, so a single index built once per
/// tick would already be stale by the second phase that wanted it. Cheap to
/// rebuild (one `O(n)` pass) each time it's needed -- what it replaces is
/// the `O(n)` *inner* scan per candidate, not the `O(n)` build itself.
///
/// `query_ring` always returns candidates in ascending original-index order,
/// the same order a brute-force `for j in 0..n` scan would visit them in
/// (`self.entities` is already sorted ascending by id -- Invariant IV) --
/// so a call site that switches from brute force to this index sees
/// *exactly* the same candidates in *exactly* the same order, an
/// accelerated enumeration, not an approximation. Callers whose result
/// doesn't depend on visiting order (a pure min/max reduction, like nearest-
/// prey-by-distance) don't need this guarantee, but nothing is lost by
/// providing it uniformly rather than maintaining two variants.
/// CSR (compressed-sparse-row) layout, the same shape a sparse matrix's row
/// index uses -- not `Occupancy`'s `BTreeMap` and not a dense
/// `Vec<Vec<u32>>` per cell. Both of those were tried first and both lost to
/// brute force at real scale: a `BTreeMap` pays real pointer-chasing and
/// per-lookup overhead on every one of the handful of cells a query touches,
/// and a `Vec<Vec<u32>>` means one small heap allocation per bucket even
/// when empty. CSR is two flat, contiguous allocations total —
/// `offsets[c]..offsets[c+1]` is cell `c`'s slice into the single `packed`
/// array -- so build is two cache-friendly linear passes (no per-entity
/// allocation) and a query is direct slice indexing (no tree, no hashing).
/// Measured, not assumed: swapping this in over the `BTreeMap` version took
/// a 2 000-population soak run from roughly 3x *slower* than brute force to
/// a real win.
pub struct SpatialIndex {
    side: i32,
    /// Length `side*side + 1`. `offsets[side*side]` is the total alive count.
    offsets: Vec<u32>,
    /// Length `offsets[side*side]`. Ascending original-index order within
    /// each cell's slice — entities are scattered into it in ascending
    /// index order, one cursor per cell, so each slice comes out sorted for
    /// free (Invariant IV: `entities` is already ascending by id).
    packed: Vec<u32>,
}

impl SpatialIndex {
    /// Build from every currently alive entity's current position. `O(n)`,
    /// two linear passes (count, then scatter) plus one prefix sum over the
    /// cell count -- no per-entity or per-bucket heap allocation.
    pub fn build(entities: &[Entity], terrain: &Terrain) -> SpatialIndex {
        let side = terrain.side;
        let n_cells = (side as usize) * (side as usize);
        let mut offsets = vec![0u32; n_cells + 1];
        for e in entities {
            if !e.alive {
                continue;
            }
            let (x, y) = terrain.cell_of(e.pos);
            offsets[terrain.index(x, y) + 1] += 1;
        }
        for c in 1..offsets.len() {
            offsets[c] += offsets[c - 1];
        }
        let mut cursor = offsets.clone();
        let mut packed = vec![0u32; offsets[n_cells] as usize];
        for (i, e) in entities.iter().enumerate() {
            if !e.alive {
                continue;
            }
            let (x, y) = terrain.cell_of(e.pos);
            let c = terrain.index(x, y);
            packed[cursor[c] as usize] = i as u32;
            cursor[c] += 1;
        }
        SpatialIndex { side, offsets, packed }
    }

    /// A conservative (rounded up, with a one-cell safety margin) cell-radius
    /// bound for a world-unit reach — how far `query_ring` must search to be
    /// guaranteed not to miss a candidate a brute-force scan would have
    /// found within `reach` of a point anywhere inside the query cell.
    /// `floor_int() + 2` dominates `ceil(reach) + 1`, the actual tight bound,
    /// for every non-negative `reach` -- a `Fx` without floats has no cheap
    /// exact `ceil`, and a slightly wider search costs a little extra
    /// candidate-filtering, not correctness, so there is no reason to chase
    /// the tight bound here.
    pub fn radius_cells(reach: crate::fx::Fx) -> i32 {
        reach.max(crate::fx::Fx::ZERO).floor_int() + 2
    }

    /// Every alive entity's original index whose cell lies within
    /// `radius_cells` (inclusive) of cell `(cx, cy)`, in ascending index
    /// order. The caller still owns the exact distance check -- this only
    /// narrows which cells are worth looking in, exactly the same
    /// broadphase-then-narrowphase split `phase_collisions`'s own bounding
    /// check already performs per pair, just applied before the pair is
    /// ever formed instead of after.
    pub fn query_ring(&self, cx: i32, cy: i32, radius_cells: i32) -> Vec<u32> {
        let r = radius_cells.max(0);
        let mut out = Vec::new();
        for dy in -r..=r {
            let y = cy + dy;
            if y < 0 || y >= self.side {
                continue;
            }
            for dx in -r..=r {
                let x = cx + dx;
                if x < 0 || x >= self.side {
                    continue;
                }
                let idx = (y as usize) * (self.side as usize) + (x as usize);
                let (start, end) = (self.offsets[idx] as usize, self.offsets[idx + 1] as usize);
                out.extend_from_slice(&self.packed[start..end]);
            }
        }
        out.sort_unstable();
        out
    }
}

/// Apportion `total` across `weight`'s cells in proportion to each cell's
/// weight, using largest-remainder rounding so the per-cell amounts sum to
/// exactly `total`. Ties in the remainder — and the fallback spread when
/// nothing is occupied — are broken with a stateless, per-terrain-tick
/// rotating hash rather than a fixed cell-index order, so no single cell
/// permanently wins every leftover unit for the life of the world.
///
/// Returns the amount actually applied, summed across every touched cell —
/// **not necessarily `total`**. Every per-cell write below saturates
/// (`saturating_add`/`saturating_sub`, Invariant II's usual discipline), so
/// a cell that is already near `u16::MAX` (add) or already low on `target`
/// (subtract) can silently accept less than its computed share. Invariant
/// VIII: `apply_conversion` depends on this return value being honest on
/// *both* sides it calls this with — the habitat-drawdown (subtract) side,
/// where crediting a race's produced output must track what was *really*
/// removed from its habitat rather than what was merely requested (or a
/// habitat layer running low would let that race manufacture output from
/// stock it never actually had), and the deposit/waste (add) side, where a
/// shortfall against an already-near-saturated target cell must be banked
/// (`Terrain::overflow`) rather than silently dropped.
fn apportion(
    terrain: &mut Terrain,
    target: Element,
    total: u64,
    weight: &BTreeMap<u32, u32>,
    seed: u64,
    terrain_tick: u64,
    add: bool,
    race: Race,
) -> u64 {
    if total == 0 {
        return 0;
    }
    let total_weight: u64 = weight.values().map(|w| *w as u64).sum();
    let mut amounts: BTreeMap<u32, u64> = BTreeMap::new();
    // Distinguishes which race/channel this apportionment call is for, so
    // two independent calls in the same terrain tick — e.g. two different
    // extinct races both hitting the uniform fallback below — don't draw the
    // exact same rotation offset or tie-break order and pile their floor
    // emission onto the same handful of cells together.
    //
    // **Race-unique, not target-unique.** Salted on `race`, not `target`
    // (the terrain layer being written) — deliberately, and required for
    // correctness. Two different races can target the very same element
    // layer with the same `add` flag: a Wood-Plant and a Wood-Animal both
    // deposit (`add == true`) into the Wood layer. `Race::index()` is
    // unique per race regardless of what element layer it maps to, so
    // salting on it keeps every (race, add) pair's fallback rotation and
    // tie-break order independent — see
    // `apportion_decorrelates_two_races_sharing_the_same_element_layer`
    // below and `docs/S3_ECOLOGY_LAYERS_DESIGN.md` §3.
    let salt = (race.index() as u32) * 2 + u32::from(!add);

    if total_weight == 0 {
        // Extinct-race (or off-grid) fallback: spread uniformly across the
        // whole grid, so an emptied server's floor still churns everywhere
        // rather than nowhere. A rotating start avoids the same low-index
        // cells winning the leftover remainder every single tick forever.
        let cells = (terrain.side as u64) * (terrain.side as u64);
        if cells == 0 {
            return 0;
        }
        let base = total / cells;
        let remainder = total % cells;
        let start = rand_below(seed, terrain_tick, salt, Channel::Terrain, cells as u32) as u64;
        for i in 0..cells {
            let cell = ((start + i) % cells) as u32;
            let extra = u64::from(i < remainder);
            let amt = base + extra;
            if amt > 0 {
                amounts.insert(cell, amt);
            }
        }
    } else {
        let mut assigned: u128 = 0;
        let mut order: Vec<(u32, u128, u32)> = Vec::with_capacity(weight.len());
        for (&cell, &w) in weight.iter() {
            let num = (total as u128) * (w as u128);
            let base = num / (total_weight as u128);
            assigned += base;
            amounts.insert(cell, base as u64);
            let frac = num % (total_weight as u128);
            let tie = rand_below(
                seed,
                terrain_tick,
                cell.wrapping_mul(0x9E37_79B1).wrapping_add(salt),
                Channel::Terrain,
                u32::MAX,
            );
            order.push((cell, frac, tie));
        }
        // Largest fractional remainder first; ties broken by the rotating
        // hash rather than by cell index, so no fixed cell wins forever.
        order.sort_by(|a, b| b.1.cmp(&a.1).then(a.2.cmp(&b.2)));
        let remainder = (total as u128 - assigned) as usize;
        for &(cell, _, _) in order.iter().take(remainder) {
            *amounts.get_mut(&cell).unwrap() += 1;
        }
    }

    let mut applied: u64 = 0;
    for (cell, amt) in amounts {
        let (x, y) = terrain.xy_of(cell);
        let v = amt.min(u16::MAX as u64) as u16;
        if v == 0 {
            continue;
        }
        let c = terrain.cell_mut(x, y);
        let before = c[target];
        c[target] = if add { before.saturating_add(v) } else { before.saturating_sub(v) };
        applied += c[target].abs_diff(before) as u64;
    }
    applied
}

/// Operator 1 (Invariant VIII). Each race's already-settled
/// `last_consume[r].granted` — the governed draw of `r.element.habitat()` —
/// resolves through `r`'s [`crate::race::Conversion`] into units of
/// `r.element` itself, apportioned across the same cells that race's living
/// bodies currently occupy (same race-unique salt discipline `apportion`
/// itself documents).
///
/// # The arithmetic, exactly
///
/// Let `N = last_consume[r].granted` (habitat units), `conv = races[r].conversion`.
///
/// - `batches = N / conv.ratio_in` (integer floor) — any remainder
///   (`N % ratio_in`) does not fill a whole batch and is simply never drawn
///   from terrain in the first place.
/// - `produced = batches * conv.ratio_out` — units of `r.element` the
///   governed demand *asks* the conversion to create.
/// - Net habitat removed from terrain is `produced`, **not** `N` or
///   `batches * ratio_in`: of every batch's `ratio_in` habitat units, only
///   `ratio_out` ever leave net — the remaining `ratio_in - ratio_out` per
///   batch are tailings that return to the very same habitat layer, at the
///   very same cells, in the very same tick (Conversion's own doc comment
///   explains why `ratio_out <= ratio_in` always). Writing that as "subtract
///   `batches * ratio_in`, then add back `batches * (ratio_in - ratio_out)`"
///   would apportion across cells identically in total but through two
///   separate largest-remainder roundings; folding it into the single net
///   subtraction below is simpler and reaches the same conserved total, so
///   that is what this function does.
/// - **What actually leaves the habitat layer can be less than `produced`.**
///   `apportion` saturates per cell (Invariant II) and returns the amount it
///   actually managed to remove, which is capped by whatever `r`'s occupied
///   cells actually hold — a governed grant is a demand ceiling, not a
///   promise that the physical stock is there to back it (the same
///   "can't spend what isn't in the bucket" principle `World::mine` already
///   applies to a single cell, here applied to a race's whole apportioned
///   territory). This actual amount, not the requested `produced`, is what
///   the rest of this function calls `produced` and splits three ways below
///   — crediting the full governed request regardless of physical
///   availability would manufacture `r.element` out of habitat the race
///   never actually had, which is exactly the create-from-nothing failure
///   Invariant VIII exists to close off.
/// - `produced` (now the actual, stock-backed amount) then splits three ways
///   per `conv`'s permille shares, remainder-corrected so the three parts
///   sum to exactly `produced` (same drift-correction discipline
///   `ChannelMix::set_rebalanced` uses): `deposit_amt`, `body_amt`,
///   `waste_amt`.
/// - `deposit_amt + waste_amt` (both `r.element`) apply to terrain via
///   `apportion` — two different bookkeeping reasons landing on the same
///   layer, at the same cells. This add-side `apportion` call saturates
///   exactly like the habitat-drawdown call above, and its actual-applied
///   return value is captured the same way: any shortfall (a race's
///   occupied cells already near `u16::MAX`) is banked in
///   `Terrain`'s per-race `overflow` and retried on top of that race's own
///   deposit every subsequent tick, rather than silently discarded — see
///   `Terrain::overflow`'s own doc comment.
/// - `body_amt` cannot be applied here: `terrain.rs` does not own `Entity`
///   state. It comes back out through this function's `PerRace<u64>`
///   return value for `World` to credit onto each living body's own
///   `Entity.material` (see `World::phase_terrain`/`credit_body_material`).
///   **Exception:** if `r` currently has zero living bodies (occupancy is
///   empty — e.g. its last body died earlier this same tick, still
///   charging an `OnDeath` draw), there is nothing to credit; that share is
///   folded into the terrain deposit instead, via the same
///   extinct-race uniform-fallback `apportion` already provides, so it is
///   never simply lost.
pub fn apply_conversion(
    terrain: &mut Terrain,
    occ: &Occupancy,
    races: &PerRace<RaceAttrs>,
    last_consume: &PerRace<Grant>,
    seed: u64,
    terrain_tick: u64,
) -> PerRace<u64> {
    let mut body_share: PerRace<u64> = PerRace::filled(0);
    for r in Race::ALL {
        let n = last_consume[r].granted;
        let conv = races[r].conversion;

        // This tick's own production -- all zero if there is nothing to
        // convert (`n == 0`, no whole batch fits, or the habitat side
        // couldn't actually deliver any stock). Deliberately *not* an early
        // `continue` on any of these anymore (bug 2 fix): a race with zero
        // production this tick can still be sitting on a banked overflow
        // shortfall from a previous tick, and that shortfall must get a
        // chance to be retried below regardless of today's demand.
        let (mut deposit_amt, mut body_amt, mut waste_amt) = (0u64, 0u64, 0u64);
        if n > 0 && conv.ratio_in > 0 {
            let batches = n / conv.ratio_in as u64;
            if batches > 0 {
                let requested = batches * conv.ratio_out as u64;
                // Net habitat drawdown -- see the doc comment above for why
                // this is `requested`, not `n` or `batches * ratio_in`.
                // `produced` is whatever `apportion` actually managed to
                // remove -- capped by the real stock at this race's
                // occupied cells, never the requested amount unconditionally
                // (see the doc comment's note on why this matters for
                // conservation).
                let produced =
                    apportion(terrain, r.element.habitat(), requested, &occ.weight[r], seed, terrain_tick, false, r);
                if produced > 0 {
                    deposit_amt = produced * conv.deposit_share as u64 / 1000;
                    body_amt = produced * conv.body_share as u64 / 1000;
                    // Remainder, not a third independent multiply --
                    // guarantees the three shares sum to exactly `produced`
                    // regardless of permille rounding, same discipline
                    // `ChannelMix::set_rebalanced` uses.
                    waste_amt = produced - deposit_amt - body_amt;
                }
            }
        }

        let alive: u32 = occ.weight[r].values().sum();
        let terrain_bound = if alive == 0 {
            // No living body of this race exists right now to grow -- fold
            // its share into the terrain deposit rather than lose it.
            deposit_amt + waste_amt + body_amt
        } else {
            *body_share.get_mut(r) = body_amt;
            deposit_amt + waste_amt
        };

        // Bug 2 fix: fold in whatever this race's deposit/waste side failed
        // to land on a previous tick (banked in `terrain.overflow`) before
        // asking `apportion` for this tick's amount, and capture what
        // `apportion` actually managed to apply this time -- it saturates
        // per cell (Invariant II) and can silently write less than
        // requested when a race's occupied cells are already near
        // `u16::MAX`. Whatever is still short after this attempt is
        // re-banked for next tick rather than discarded.
        let requested_terrain = terrain_bound + terrain.overflow[r][r.element];
        if requested_terrain > 0 {
            let applied = apportion(terrain, r.element, requested_terrain, &occ.weight[r], seed, terrain_tick, true, r);
            *terrain.overflow.get_mut(r).get_mut(r.element) = requested_terrain - applied;
        }

        // `deposit_at` gap fix: this race may also be sitting on banked
        // shortfalls from single-cell deposits (`World::charge_death`'s
        // carried/items return, `World::smelt`'s tailings, `World::
        // break_item`'s returned quantity) on element channels other than
        // its own -- `apportion`'s own doc comment/this struct's `overflow`
        // doc comment. Retry every such channel here too, spread across
        // this race's currently occupied cells exactly like its own-element
        // retry just above (or the extinct-race uniform fallback if none of
        // its bodies are alive right now), rather than only ever retrying
        // through `deposit_at` itself -- which would need occupancy it
        // doesn't have access to.
        for e in Element::ALL {
            if e == r.element {
                continue; // handled above, together with this tick's own production
            }
            let banked = terrain.overflow[r][e];
            if banked == 0 {
                continue;
            }
            let applied = apportion(terrain, e, banked, &occ.weight[r], seed, terrain_tick, true, r);
            *terrain.overflow.get_mut(r).get_mut(e) = banked - applied;
        }
    }
    body_share
}

/// A direct, single-cell material return — unlike [`apportion`], which
/// spreads a race's aggregate grant across every cell its living bodies
/// occupy, this writes to exactly the one cell a specific body's material
/// actually returns to. Used by `World::charge_death` (a corpse decomposes
/// where it fell, not smeared across a race's entire territory),
/// `World::smelt` (tailings) and `World::break_item` (a broken item's
/// quantity).
///
/// `race` is the body whose action produced this deposit -- the dying
/// entity in `charge_death`, the smelter in `smelt`, the item's owner in
/// `break_item` -- and need not match `e`: a body's `carried`/items can
/// hold elements quite different from its own race's. It exists purely for
/// attribution, not to gate or scale the deposit.
///
/// Saturates like every other terrain write (Invariant II): `amount` can
/// exceed both `u16::MAX` outright and this cell's actual remaining
/// headroom below it. Either way the shortfall is not silently discarded --
/// it is banked in `Terrain::overflow[race][e]` (see that field's own doc
/// comment) and retried by `apply_conversion` every subsequent terrain
/// tick, spread across `race`'s currently occupied cells, until it fully
/// lands. This is the same discipline `apportion` already uses for
/// `apply_conversion`'s own deposit/waste side (the bug 2 fix) — a
/// near-saturated single target cell is exactly the shape that discipline
/// exists for.
pub fn deposit_at(terrain: &mut Terrain, race: Race, e: Element, amount: u64, pos: V2) {
    if amount == 0 {
        return;
    }
    let (x, y) = terrain.cell_of(pos);
    let c = terrain.cell_mut(x, y);
    let headroom = u64::from(u16::MAX - c[e]);
    let applied = amount.min(headroom);
    c[e] += applied as u16;
    let shortfall = amount - applied;
    if shortfall > 0 {
        let bucket = terrain.overflow.get_mut(race).get_mut(e);
        *bucket = bucket.saturating_add(shortfall);
    }
}

/// Operator 6 — bounded diffusion, Invariant I's literal home. Gradient
/// (Fickian) flow: material moves down the concentration difference between
/// two neighbouring cells, proportionally, with a flat per-edge ceiling on
/// top — the ceiling is the concrete content of "nothing acts instantly at
/// a distance": no matter how saturated a source cell is, one edge can
/// never carry more than `diffuse_cap` units in one terrain tick.
///
/// The grid wraps toroidally — the east edge is adjacent to the west edge,
/// the south to the north — so every cell has exactly four neighbours and
/// nothing is discarded or invented at a boundary. Every cell's East and
/// South edges are visited exactly once, covering every edge of the torus
/// exactly once (at `side == 2` the wrap would otherwise revisit the same
/// physical edge from both directions, so that case is explicitly skipped —
/// see the `dup` guard below; `side == 1`'s wrap lands on itself, where
/// `diff` is trivially zero).
///
/// **A cell can be the source of up to four edges in the same tick** — its
/// own East and South edges when it holds more than that neighbour, and its
/// West and North neighbours' East/South edges when *they* hold less than
/// this cell. Bounding each edge's flow to that single edge's own gradient
/// and cap is not enough on its own: at the shipped default that never
/// matters (four edges at 20% each tops out at 80% of the cell), but at
/// higher `diffuse_rate` values each of the (up to) four edges could
/// independently claim up to the cell's *entire* stock, and the total sent
/// out would exceed what the cell ever held — `saturating_sub` floors the
/// source's own loss at zero while every destination still received its
/// full, uncapped share, fabricating mass. So the source side runs in two
/// passes: pass 1 totals each cell's raw outbound demand across all four of
/// its edges; a later pass recomputes the identical per-edge flows and,
/// wherever a cell's total demand would exceed its actual stock, scales
/// *all* of that cell's outflows down together, proportionally, before
/// applying them.
///
/// **A cell can equally be the destination of up to four edges in the same
/// tick** — several already-source-scaled neighbours can each independently
/// compute a valid outgoing amount toward this one cell, and their sum can
/// exceed this cell's remaining headroom to `u16::MAX` even though no single
/// edge does. Unlike the source side, `saturating_add` clipping here does
/// not just redistribute the shortfall — the matching `saturating_sub` on
/// each contributing source side already happened in full, so a clipped add
/// is a genuine mass loss, not a mass move. This mirrors the source-side fix
/// one level later: an extra pass tallies each destination's total incoming
/// (already source-scaled) demand across all four of its edges, and the
/// final apply pass scales that down again, proportionally, wherever it
/// would exceed the destination's actual headroom — the same floor-division,
/// leave-a-few-units-behind discipline the source side already uses. Total
/// mass moved by this operator across the whole grid is exactly zero by
/// construction, for every reachable grid size and tuning value, since the
/// amount subtracted from a source and the amount added to its destination
/// are now always computed as the identical, already-doubly-scaled number.
///
/// Whole-grid snapshot rule: this reads and writes neighbours, so unlike
/// ring/star it needs a full double-buffer, not just a per-cell one — an
/// in-place pass would let column 0's outflow contaminate column 1's inputs
/// within the same tick, letting mass travel two cells in one terrain tick,
/// which is exactly the bound this operator exists to prevent.
pub fn apply_diffusion(terrain: &mut Terrain, tuning: &TerrainTuning) {
    let side = terrain.usize_side();
    terrain.scratch.clone_from(&terrain.cells);

    // i64: `diff` and `rate` are each individually in-range for i32 (±65535
    // and 0..=65535), but their product is not — 65535 × 65535 overflows
    // i32, and this crate builds with `overflow-checks = true` in every
    // profile, so a plain i32 multiply would be a live panic once a tuning
    // knob pushes `diffuse_rate` high enough.
    let flow = |cells: &[PerElement<u16>], here_idx: usize, there_idx: usize, e: Element| -> i64 {
        let diff = cells[here_idx][e] as i64 - cells[there_idx][e] as i64;
        let rate = tuning.diffuse_rate[e] as i64;
        let cap = tuning.diffuse_cap[e] as i64;
        (diff * rate / 1000).clamp(-cap, cap)
    };
    let edges = |side: usize, x: usize, y: usize| {
        let east_dup = side == 2 && x == 1;
        let south_dup = side == 2 && y == 1;
        [((x + 1) % side, y, east_dup), (x, (y + 1) % side, south_dup)]
    };

    let n = side * side;
    let mut out_total: Vec<PerElement<i64>> = vec![PerElement::filled(0i64); n];
    for y in 0..side {
        for x in 0..side {
            let here_idx = y * side + x;
            for (nx, ny, dup) in edges(side, x, y) {
                if dup {
                    continue;
                }
                let there_idx = ny * side + nx;
                for e in Element::ALL {
                    let f = flow(&terrain.cells, here_idx, there_idx, e);
                    if f > 0 {
                        out_total[here_idx][e] += f;
                    } else if f < 0 {
                        out_total[there_idx][e] += -f;
                    }
                }
            }
        }
    }

    // The source-side-scaled amount one edge would move, before any
    // destination-side cap (bug 3, below) — shared by the destination-tally
    // pass and the final apply pass so the two can never disagree about the
    // same edge's number. Takes `cells`/`out_total` as explicit parameters
    // rather than capturing `terrain`/`out_total` directly, the same reason
    // `flow` above does: it is called from inside a loop that also needs to
    // mutate `terrain.scratch`, and each borrow here must end with the call
    // rather than live across that mutation.
    let source_scaled = |cells: &[PerElement<u16>],
                          out_total: &[PerElement<i64>],
                          here_idx: usize,
                          there_idx: usize,
                          e: Element|
     -> (usize, usize, i64) {
        let f = flow(cells, here_idx, there_idx, e);
        if f == 0 {
            return (here_idx, there_idx, 0);
        }
        let (source_idx, dest_idx, magnitude) =
            if f > 0 { (here_idx, there_idx, f) } else { (there_idx, here_idx, -f) };
        let stock = cells[source_idx][e] as i64;
        let demand = out_total[source_idx][e];
        // Floor division: strictly conservative relative to the per-edge
        // cap (a few units of "should have moved" are simply left behind,
        // rather than risk moving one more than the source actually holds
        // via rounding up).
        let scaled = if demand > stock { magnitude * stock / demand } else { magnitude };
        (source_idx, dest_idx, scaled)
    };

    // Bug 3 fix: tally each destination cell's total incoming (already
    // source-scaled) demand across all four of its edges — the mirror of
    // `out_total` above, one level later in the pipeline.
    let mut in_total: Vec<PerElement<i64>> = vec![PerElement::filled(0i64); n];
    for y in 0..side {
        for x in 0..side {
            let here_idx = y * side + x;
            for (nx, ny, dup) in edges(side, x, y) {
                if dup {
                    continue;
                }
                let there_idx = ny * side + nx;
                for e in Element::ALL {
                    let (_, dest_idx, scaled) = source_scaled(&terrain.cells, &out_total, here_idx, there_idx, e);
                    if scaled > 0 {
                        in_total[dest_idx][e] += scaled;
                    }
                }
            }
        }
    }

    for y in 0..side {
        for x in 0..side {
            let here_idx = y * side + x;
            for (nx, ny, dup) in edges(side, x, y) {
                if dup {
                    continue;
                }
                let there_idx = ny * side + nx;
                for e in Element::ALL {
                    let (source_idx, dest_idx, scaled) =
                        source_scaled(&terrain.cells, &out_total, here_idx, there_idx, e);
                    if scaled <= 0 {
                        continue;
                    }
                    // Bug 3 fix: cap this edge's contribution so the
                    // destination's total incoming across all its edges
                    // never exceeds its actual headroom to `u16::MAX` — the
                    // same proportional, floor-division discipline as the
                    // source-side cap above, one level later.
                    let dest_stock = terrain.cells[dest_idx][e] as i64;
                    let headroom = u16::MAX as i64 - dest_stock;
                    let demand = in_total[dest_idx][e];
                    let amt2 = if demand > headroom { scaled * headroom / demand } else { scaled };
                    if amt2 <= 0 {
                        continue;
                    }
                    let amt = amt2 as u16; // safe: amt2 <= headroom <= u16::MAX
                    terrain.scratch[source_idx][e] = terrain.scratch[source_idx][e].saturating_sub(amt);
                    terrain.scratch[dest_idx][e] = terrain.scratch[dest_idx][e].saturating_add(amt);
                }
            }
        }
    }

    std::mem::swap(&mut terrain.cells, &mut terrain.scratch);
}

/// Integer alpha-blend of a cell's five saturations into one RGB pixel:
/// `pixel = Σ(RGB[e] * sat[e]) / Σ(sat[e])`, all `u64`. Falls back to a fixed
/// background colour when every saturation is zero, rather than dividing by
/// zero. Render-only — see `render_ppm`'s own doc comment for the discipline
/// this function exists to serve.
pub fn blend_rgb(sat: &PerElement<u64>) -> (u8, u8, u8) {
    const BACKGROUND: (u8, u8, u8) = (18, 18, 24);

    let mut total: u64 = 0;
    let mut r: u64 = 0;
    let mut g: u64 = 0;
    let mut b: u64 = 0;
    for (e, &s) in sat.iter() {
        let (cr, cg, cb) = RGB[e];
        r += cr as u64 * s;
        g += cg as u64 * s;
        b += cb as u64 * s;
        total += s;
    }
    if total == 0 {
        return BACKGROUND;
    }
    (
        (r / total) as u8,
        (g / total) as u8,
        (b / total) as u8,
    )
}

/// Render one binary PPM (P6) frame of `terrain`, one pixel per cell,
/// row-major (`y` outer, `x` inner — matching `Terrain::index`'s own
/// `index = y * side + x` convention). Exact integer arithmetic throughout
/// (`blend_rgb`); no floats, no external image crate.
///
/// **Render-only.** This must only ever be called from a bin's own loop
/// around `w.step(&log)` — never from inside `World::step` or any terrain
/// operator. If you see it called anywhere else, that is the bug (the same
/// discipline `Fx::to_f32_render`'s doc comment states for itself).
pub fn render_ppm(terrain: &Terrain) -> Vec<u8> {
    let side = terrain.side;
    let mut out = format!("P6\n{side} {side}\n255\n").into_bytes();
    out.reserve((side as usize) * (side as usize) * 3);
    for y in 0..side {
        for x in 0..side {
            let mut sat: PerElement<u64> = PerElement::filled(0u64);
            for e in Element::ALL {
                sat[e] = terrain.cell(x, y)[e] as u64;
            }
            let (r, g, b) = blend_rgb(&sat);
            out.push(r);
            out.push(g);
            out.push(b);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::race::Kind;

    fn grant(granted: u64) -> Grant {
        Grant { granted, forced: 0, clipped: 0 }
    }

    fn animal(e: Element) -> Race {
        Race { element: e, kind: Kind::Animal }
    }

    #[test]
    fn new_terrain_is_all_zero() {
        let t = Terrain::new(8);
        for e in Element::ALL {
            assert_eq!(t.total(e), 0);
        }
    }

    #[test]
    fn seed_uniform_sets_every_cell_of_one_element_and_leaves_the_rest_alone() {
        let mut t = Terrain::new(6);
        t.seed_uniform(Element::Earth, 1000);
        for y in 0..6i32 {
            for x in 0..6i32 {
                assert_eq!(t.cell(x, y)[Element::Earth], 1000, "every cell must hold exactly the seeded amount");
                for e in Element::ALL {
                    if e != Element::Earth {
                        assert_eq!(t.cell(x, y)[e], 0, "{} must be untouched by seeding a different element", e.name());
                    }
                }
            }
        }
    }

    #[test]
    fn hash_changes_when_a_cell_changes() {
        let mut t = Terrain::new(8);
        let a = t.state_hash();
        t.cell_mut(3, 3)[Element::Fire] = 5;
        let b = t.state_hash();
        assert_ne!(a, b);
    }

    #[test]
    fn apportion_weighted_sums_to_exactly_total() {
        let t = Terrain::new(8);
        let mut weight = BTreeMap::new();
        weight.insert(t.index(0, 0) as u32, 3);
        weight.insert(t.index(1, 0) as u32, 1);
        weight.insert(t.index(2, 0) as u32, 1);
        for total in [0u64, 1, 7, 100, 9_999] {
            let mut tt = Terrain::new(8);
            apportion(&mut tt, Element::Fire, total, &weight, 0xABC, 1, true, animal(Element::Fire));
            assert_eq!(tt.total(Element::Fire), total, "total={total}");
        }
    }

    #[test]
    fn spatial_index_query_ring_finds_exactly_the_brute_force_candidates() {
        // Random entities, random query point/radius, many trials: the set
        // `query_ring` returns (after the caller's own exact distance
        // filter, exactly like every real call site applies) must equal a
        // brute-force `for j in 0..n` scan's set, every time -- this is the
        // whole correctness claim the broadphase rewire depends on.
        use crate::fx::Fx;
        use crate::race::{attrs, Race};
        let terrain = Terrain::new(20);
        let race = Race { element: Element::Wood, kind: Kind::Animal };
        let a = attrs(race);
        let mut entities = Vec::new();
        for seed in 0..200u64 {
            let x = Fx::from_int(((seed * 7919) % 20) as i32);
            let y = Fx::from_int(((seed * 104729) % 20) as i32);
            let mut e = Entity::spawn(seed as u32 + 1, Element::Wood, V2::new(x, y), seed, 0, &a);
            e.alive = seed % 11 != 0; // a handful of dead bodies mixed in
            entities.push(e);
        }
        let index = SpatialIndex::build(&entities, &terrain);

        for trial in 0..50i32 {
            let cx = trial % 20;
            let cy = (trial * 3) % 20;
            let reach = Fx::ratio(300 + trial * 17, 100); // varying reach, up to ~11.6 cells
            let reach_sq = reach * reach;
            let center = V2::new(Fx::from_int(cx), Fx::from_int(cy));

            let mut brute: Vec<u32> = (0..entities.len())
                .filter(|&j| entities[j].alive && (entities[j].pos - center).len_sq() <= reach_sq)
                .map(|j| j as u32)
                .collect();
            brute.sort_unstable();

            let r = SpatialIndex::radius_cells(reach);
            let mut indexed: Vec<u32> = index
                .query_ring(cx, cy, r)
                .into_iter()
                .filter(|&j| {
                    let e = &entities[j as usize];
                    e.alive && (e.pos - center).len_sq() <= reach_sq
                })
                .collect();
            indexed.sort_unstable();

            assert_eq!(indexed, brute, "trial {trial}: cx={cx} cy={cy} reach={reach:?}");
        }
    }

    #[test]
    fn apportion_uniform_fallback_sums_to_exactly_total() {
        let empty = BTreeMap::new();
        for total in [0u64, 1, 5, 1_000, 50_000] {
            let mut tt = Terrain::new(6);
            apportion(&mut tt, Element::Water, total, &empty, 0xDEF, 42, true, animal(Element::Water));
            assert_eq!(tt.total(Element::Water), total, "total={total}");
        }
    }

    #[test]
    fn uniform_fallback_rotates_which_cell_wins_the_remainder() {
        // The whole reason for the rotating offset: the same low-index cell
        // must not win the leftover remainder unit on every tick.
        let empty = BTreeMap::new();
        let mut winners = std::collections::BTreeSet::new();
        for tick in 0..20u64 {
            let mut t = Terrain::new(5); // 25 cells, so total=26 leaves a remainder of 1
            apportion(&mut t, Element::Earth, 26, &empty, 0x1234, tick, true, animal(Element::Earth));
            for y in 0..5 {
                for x in 0..5 {
                    if t.cell(x, y)[Element::Earth] == 2 {
                        winners.insert((x, y));
                    }
                }
            }
        }
        assert!(winners.len() > 1, "the remainder cell never rotated: {:?}", winners);
    }

    #[test]
    fn apportion_decorrelates_two_races_sharing_the_same_element_layer() {
        // Regression, §3 of the design doc: before the salt widened from
        // `target.index()*2 + !add` to `race.index()*2 + !add`, two
        // different races both depositing (`add == true`) into the very
        // same element layer — Wood-Plant and Wood-Animal both write Wood —
        // would draw the identical fallback rotation offset off the old
        // target-only salt, silently correlating their apportionment noise
        // instead of decorrelating it. This case did not exist before S3.1
        // (`Occupancy` was Element-shaped, one race per element); now that
        // two races can share a target, the salt must be race-unique, not
        // just target-unique.
        let empty = BTreeMap::new();
        let mut winners = std::collections::BTreeSet::new();
        for kind in [Kind::Plant, Kind::Animal] {
            let race = Race { element: Element::Wood, kind };
            let mut t = Terrain::new(5); // 25 cells; total=26 leaves a remainder of 1
            apportion(&mut t, Element::Wood, 26, &empty, 0x1234, 7, true, race);
            for y in 0..5 {
                for x in 0..5 {
                    if t.cell(x, y)[Element::Wood] == 2 {
                        winners.insert((x, y));
                    }
                }
            }
        }
        assert!(
            winners.len() > 1,
            "two races sharing the same element layer drew the identical fallback pattern: {:?}",
            winners
        );
    }

    #[test]
    fn apply_conversion_draws_habitat_and_splits_produced_material_into_deposit_body_and_waste() {
        // Wood-Plant: ratio_in=1, ratio_out=1 (a lossless conversion — see
        // `RACES`'s own doc comment for why this row is clamped there), split
        // 650/300/50 deposit/body/waste. habitat() = Wood.eats() = Water, so
        // this body draws down Water and deposits/wastes onto Wood.
        let race = Race { element: Element::Wood, kind: Kind::Plant };
        let mut t = Terrain::new(4);
        t.cell_mut(1, 1)[Element::Water] = 10_000;
        let entities = vec![Entity::spawn(
            1,
            Element::Wood,
            V2::new(crate::fx::Fx::from_int(1), crate::fx::Fx::from_int(1)),
            0,
            0,
            crate::race::attrs(race),
        )];
        let occ = Occupancy::build(&entities, &t);
        let mut granted = PerRace::filled(grant(0));
        granted[race] = grant(100);

        let body_share = apply_conversion(&mut t, &occ, &crate::race::RACES, &granted, 1, 1);

        assert_eq!(
            t.cell(1, 1)[Element::Water],
            10_000 - 100,
            "habitat drawdown is the produced amount, 100 at this 1:1 ratio"
        );
        assert_eq!(t.total(Element::Wood), 65 + 5, "deposit_share (650‰) + waste_share (50‰) of the 100 produced");
        assert_eq!(*body_share.get(race), 30, "body_share (300‰) of the 100 produced, returned for World to credit");
        for e in Element::ALL {
            if e != Element::Water && e != Element::Wood {
                assert_eq!(t.total(e), 0);
            }
        }
    }

    #[test]
    fn apply_conversion_folds_an_extinct_races_body_share_into_terrain_deposit() {
        // No living body of this race exists this tick (occupancy is empty)
        // -- the whole produced amount, body_share included, must land on
        // terrain rather than vanish. See `apply_conversion`'s own doc
        // comment for why.
        //
        // Habitat stock is seeded across *every* cell, not just one --
        // extinct-race apportionment spreads uniformly across the whole
        // grid (`apportion`'s own fallback), and since Invariant VIII's
        // production cap (`apply_conversion_caps_production_at_the_actual_
        // available_habitat_stock`) now honours actual per-cell stock, a
        // single seeded cell would starve every other cell's share of the
        // draw and this test would be proving the cap, not the fold.
        let race = Race { element: Element::Wood, kind: Kind::Plant };
        let mut t = Terrain::new(4);
        for y in 0..4 {
            for x in 0..4 {
                t.cell_mut(x, y)[Element::Water] = 1000;
            }
        }
        let occ = Occupancy::build(&[], &t);
        let mut granted = PerRace::filled(grant(0));
        granted[race] = grant(100);

        let body_share = apply_conversion(&mut t, &occ, &crate::race::RACES, &granted, 1, 1);

        assert_eq!(*body_share.get(race), 0, "nothing alive to credit");
        assert_eq!(t.total(Element::Wood), 100, "the whole produced amount folds into terrain deposit instead of being lost");
    }

    #[test]
    fn apply_conversion_caps_production_at_the_actual_available_habitat_stock() {
        // Regression: `apportion`'s per-cell subtraction saturates
        // (Invariant II) and used to silently clip below what a race's
        // governed grant requested, while the produced side was credited in
        // full regardless of whether the habitat drawdown actually
        // succeeded -- manufacturing `r.element` out of habitat the race
        // never actually had. Granted draw (100) exceeds the occupied
        // cell's actual Water stock (40); production must be capped to what
        // was actually removed (40), not the requested 100.
        let race = Race { element: Element::Wood, kind: Kind::Plant };
        let mut t = Terrain::new(4);
        t.cell_mut(1, 1)[Element::Water] = 40; // less than the granted draw below
        let entities = vec![Entity::spawn(
            1,
            Element::Wood,
            V2::new(crate::fx::Fx::from_int(1), crate::fx::Fx::from_int(1)),
            0,
            0,
            crate::race::attrs(race),
        )];
        let occ = Occupancy::build(&entities, &t);
        let mut granted = PerRace::filled(grant(0));
        granted[race] = grant(100);

        let body_share = apply_conversion(&mut t, &occ, &crate::race::RACES, &granted, 1, 1);

        assert_eq!(t.cell(1, 1)[Element::Water], 0, "the cell's whole stock, and no more, is drawn down");
        assert_eq!(
            t.total(Element::Wood),
            26 + 2,
            "deposit_share (650‰) + waste_share (50‰) of the capped 40 actually produced, not the requested 100"
        );
        assert_eq!(*body_share.get(race), 12, "body_share (300‰) of the capped 40, not the requested 100");
        // Conservation, directly: every unit that left the Water layer is
        // traceable to the Wood layer (deposit + waste) or the returned
        // body_share -- nothing more, regardless of what was requested.
        assert_eq!(
            40 - t.total(Element::Water),
            t.total(Element::Wood) + *body_share.get(race),
            "habitat lost must equal element produced across terrain deposit+waste and body_share"
        );
    }

    #[test]
    fn apply_conversion_banks_a_saturated_deposit_shortfall_instead_of_losing_it() {
        // Bug 2 regression: the deposit/waste (add) side of `apply_conversion`
        // used to discard `apportion`'s actual-applied return value, so a
        // near-saturated target cell would silently clip the deposit with no
        // accounting for the shortfall. Set the one occupied cell's Wood
        // stock to within 5 of `u16::MAX`, so a much larger deposit+waste
        // request (70, at this row's 650/300/50 split of a 100 produced) can
        // only actually land 5 of it.
        let race = Race { element: Element::Wood, kind: Kind::Plant };
        let mut t = Terrain::new(4);
        t.cell_mut(1, 1)[Element::Wood] = u16::MAX - 5;
        t.cell_mut(1, 1)[Element::Water] = 10_000; // ample habitat stock to draw down
        let entities = vec![Entity::spawn(
            1,
            Element::Wood,
            V2::new(crate::fx::Fx::from_int(1), crate::fx::Fx::from_int(1)),
            0,
            0,
            crate::race::attrs(race),
        )];
        let occ = Occupancy::build(&entities, &t);
        let mut granted = PerRace::filled(grant(0));
        granted[race] = grant(100); // produces 100 at this row's lossless 1:1 ratio

        let body_share = apply_conversion(&mut t, &occ, &crate::race::RACES, &granted, 1, 1);

        assert_eq!(t.cell(1, 1)[Element::Wood], u16::MAX, "the cell fills to exactly its ceiling, no more");
        assert_eq!(*body_share.get(race), 30, "body_share is unaffected by the terrain-side saturation");
        // The 65 (deposit) + 5 (waste) = 70 requested, only 5 of which could
        // actually land (the cell's headroom) -- the missing 65 must be
        // banked, not silently gone.
        assert_eq!(t.overflow(race), 65, "the shortfall the saturated cell couldn't accept must be banked, not destroyed");

        // A second terrain tick, with no *new* production (`granted` back to
        // zero) and headroom reopened (diffusion or consumption elsewhere
        // would ordinarily do this; simulated here directly) must still
        // retry and land the banked shortfall -- proving it is genuinely
        // "delayed", not merely recorded and forgotten.
        t.cell_mut(1, 1)[Element::Wood] = 0;
        let granted_zero = PerRace::filled(grant(0));
        let occ2 = Occupancy::build(&entities, &t);
        let body_share2 = apply_conversion(&mut t, &occ2, &crate::race::RACES, &granted_zero, 1, 2);

        assert_eq!(t.cell(1, 1)[Element::Wood], 65, "the previously-banked shortfall lands once headroom exists");
        assert_eq!(t.overflow(race), 0, "fully retried, nothing left banked");
        assert_eq!(*body_share2.get(race), 0, "no new production this tick, so no new body_share");
    }

    #[test]
    fn deposit_at_banks_a_saturated_cells_shortfall_instead_of_losing_it() {
        // Round-2 regression: `deposit_at` (the single-cell material return
        // `World::charge_death`/`smelt`/`break_item` all call) used to clip
        // its incoming amount to the cell's remaining `u16` headroom via a
        // bare `saturating_add` and return nothing -- any shortfall above
        // what the cell could absorb simply vanished, with no accounting at
        // all. Same shape as the `apply_conversion` bug 2 fix above, proved
        // the same way: set a cell within 5 of `u16::MAX`, deposit far more
        // than that headroom, and show the shortfall is banked rather than
        // destroyed.
        let race = Race { element: Element::Fire, kind: Kind::Animal };
        let mut t = Terrain::new(4);
        t.cell_mut(2, 2)[Element::Water] = u16::MAX - 5; // only 5 headroom
        let pos = V2::new(crate::fx::Fx::from_int(2), crate::fx::Fx::from_int(2));

        // A corpse's carried Water (unrelated to its own Fire element) is
        // exactly the newly-reachable shape bug 1's `charge_death` fix
        // created: large lump sums with nothing capping them beforehand.
        deposit_at(&mut t, race, Element::Water, 68_000, pos);

        assert_eq!(t.cell(2, 2)[Element::Water], u16::MAX, "the cell fills to exactly its ceiling, no more");
        assert_eq!(
            t.overflow_of(race, Element::Water),
            68_000 - 5,
            "everything the cell couldn't absorb must be banked under (race, element), not destroyed"
        );

        // Retried and landed exactly like `apply_conversion`'s own banked
        // shortfall: a subsequent terrain tick, with headroom reopened and
        // zero new production, must fully apply the banked amount.
        t.cell_mut(2, 2)[Element::Water] = 0;
        let occ = Occupancy::build(&[], &t); // no living bodies of `race` -- exercises the extinct-race uniform fallback
        let granted_zero = PerRace::filled(grant(0));
        apply_conversion(&mut t, &occ, &crate::race::RACES, &granted_zero, 9, 1);

        assert_eq!(t.total(Element::Water), 68_000 - 5, "the previously-banked shortfall lands once headroom exists");
        assert_eq!(t.overflow_of(race, Element::Water), 0, "fully retried, nothing left banked");
    }

    #[test]
    fn deposit_at_shortfall_retries_spread_across_the_races_living_bodies_not_just_the_original_cell() {
        // Same banking mechanism, but with a living body of `race` present
        // elsewhere on the grid at retry time -- proving the retry actually
        // goes through `apportion`'s occupancy-weighted spread (the same
        // path `apply_conversion`'s own retry uses), not merely a hardcoded
        // replay at the original deposit position.
        let race = Race { element: Element::Metal, kind: Kind::Plant };
        let mut t = Terrain::new(4);
        t.cell_mut(0, 0)[Element::Earth] = u16::MAX; // fully saturated, zero headroom
        let pos = V2::new(crate::fx::Fx::from_int(0), crate::fx::Fx::from_int(0));

        deposit_at(&mut t, race, Element::Earth, 500, pos);
        assert_eq!(t.overflow_of(race, Element::Earth), 500, "zero headroom banks the whole amount");

        // A living body of `race`, at a different, empty cell.
        let entities = vec![Entity::spawn(
            1,
            Element::Metal,
            V2::new(crate::fx::Fx::from_int(3), crate::fx::Fx::from_int(3)),
            0,
            0,
            crate::race::attrs(race),
        )];
        let occ = Occupancy::build(&entities, &t);
        let granted_zero = PerRace::filled(grant(0));
        apply_conversion(&mut t, &occ, &crate::race::RACES, &granted_zero, 3, 1);

        assert_eq!(t.overflow_of(race, Element::Earth), 0, "fully retried once a living body gives it somewhere to land");
        assert_eq!(t.cell(3, 3)[Element::Earth], 500, "lands at the living body's own cell, not the original saturated one");
        assert_eq!(t.cell(0, 0)[Element::Earth], u16::MAX, "the original cell is untouched by the retry");
    }

    #[test]
    fn reviewer3_overflow_survives_many_saturated_retries_then_fully_drains_once_reopened() {
        // Round-3 reviewer 3's own angle: read the retry path end to end
        // and confirm it actually *drains* banked overflow over subsequent
        // ticks rather than only ever recording it once. Unlike the other
        // overflow tests above (which retry exactly once), this keeps the
        // one occupied cell pinned at saturation across MANY separate
        // `apply_conversion` calls -- each one an independent "terrain
        // tick" attempting the retry and failing again -- accumulating more
        // banked shortfall on top each time, before finally opening the
        // cell up and confirming the *entire* accumulated bank, across all
        // those failed attempts, lands exactly once headroom exists.
        let race = Race { element: Element::Metal, kind: Kind::Animal };
        let mut t = Terrain::new(4);
        let pos = V2::new(crate::fx::Fx::from_int(2), crate::fx::Fx::from_int(2));
        let entities = vec![Entity::spawn(1, Element::Metal, pos, 0, 0, crate::race::attrs(race))];
        let occ = Occupancy::build(&entities, &t);
        let granted_zero = PerRace::filled(grant(0));

        let mut total_banked_expected = 0u64;
        const RETRY_ATTEMPTS: u64 = 40;
        for tick in 1..=RETRY_ATTEMPTS {
            // Re-pin the cell to fully saturated before every attempt, as
            // if some unrelated sustained producer kept refilling it the
            // whole time -- and deposit a fresh shortfall on top of
            // whatever is already banked, exactly like a steady trickle of
            // deaths onto the same busy cell would.
            t.cell_mut(2, 2)[Element::Earth] = u16::MAX;
            deposit_at(&mut t, race, Element::Earth, 300, pos);
            total_banked_expected += 300;

            // A retry attempt against a cell that is still fully saturated
            // must change nothing: not shrink the bank (material must not
            // vanish) and not grow it beyond what was actually deposited
            // (material must not be double-banked).
            apply_conversion(&mut t, &occ, &crate::race::RACES, &granted_zero, tick, tick);
            assert_eq!(
                t.overflow_of(race, Element::Earth),
                total_banked_expected,
                "a retry against a still-saturated cell must neither lose nor double-bank the shortfall (attempt {tick})"
            );
            assert_eq!(t.cell(2, 2)[Element::Earth], u16::MAX, "the saturated cell itself is untouched by a failed retry");
        }

        // Now actually open the cell up and let the retry mechanism land
        // the whole accumulated bank for real.
        t.cell_mut(2, 2)[Element::Earth] = 0;
        apply_conversion(&mut t, &occ, &crate::race::RACES, &granted_zero, RETRY_ATTEMPTS + 1, RETRY_ATTEMPTS + 1);

        assert_eq!(t.overflow_of(race, Element::Earth), 0, "the entire bank accumulated over many failed retries fully drains the instant headroom reopens");
        assert_eq!(t.cell(2, 2)[Element::Earth], total_banked_expected as u16, "every unit banked across all those attempts actually lands, exactly, none created or destroyed");
    }

    #[test]
    fn diffusion_conserves_total_mass_across_the_grid() {
        let mut t = Terrain::new(6);
        t.cell_mut(2, 2)[Element::Metal] = 60_000;
        t.cell_mut(4, 1)[Element::Metal] = 500;
        let before = t.total(Element::Metal);
        let tuning = TerrainTuning::default();
        for _ in 0..5 {
            apply_diffusion(&mut t, &tuning);
        }
        assert_eq!(t.total(Element::Metal), before, "diffusion must not create or destroy mass");
    }

    #[test]
    fn diffusion_never_moves_further_than_one_cell_per_call() {
        let mut t = Terrain::new(9);
        t.cell_mut(4, 4)[Element::Water] = 60_000;
        let tuning = TerrainTuning { diffuse_rate: PerElement::filled(1000), diffuse_cap: PerElement::filled(60_000) };
        apply_diffusion(&mut t, &tuning);
        for y in 0..9i32 {
            for x in 0..9i32 {
                let manhattan = (x - 4).abs() + (y - 4).abs();
                if manhattan > 1 {
                    assert_eq!(
                        t.cell(x, y)[Element::Water],
                        0,
                        "cell ({x},{y}) is more than one edge away from the source and must be untouched"
                    );
                }
            }
        }
    }

    #[test]
    fn diffusion_never_exceeds_the_flat_cap_per_edge() {
        let mut t = Terrain::new(4);
        t.cell_mut(0, 0)[Element::Wood] = 65_000;
        let tuning = TerrainTuning { diffuse_rate: PerElement::filled(1000), diffuse_cap: PerElement::filled(10) };
        apply_diffusion(&mut t, &tuning);
        // Every neighbour can gain at most `diffuse_cap` from this one source cell.
        assert!(t.cell(1, 0)[Element::Wood] <= 10);
        assert!(t.cell(0, 1)[Element::Wood] <= 10);
        assert!(t.cell(3, 0)[Element::Wood] <= 10, "west neighbour via wraparound");
        assert!(t.cell(0, 3)[Element::Wood] <= 10, "north neighbour via wraparound");
    }

    #[test]
    fn diffusion_does_not_panic_at_extreme_tuning_values() {
        // Regression: `diff * rate` as a bare i32 multiply overflows once
        // `diffuse_rate` exceeds roughly 32766, and this crate builds with
        // overflow-checks = true in every profile — this used to panic.
        let mut t = Terrain::new(4);
        t.cell_mut(0, 0)[Element::Wood] = 65_535;
        let tuning = TerrainTuning {
            diffuse_rate: PerElement::filled(u16::MAX),
            diffuse_cap: PerElement::filled(u16::MAX),
        };
        apply_diffusion(&mut t, &tuning); // must not panic
    }

    #[test]
    fn diffusion_never_fabricates_mass_even_when_rate_exceeds_one_hundred_percent() {
        // Regression: a `diffuse_rate` above 1000‰ (100%) used to let `flow`
        // exceed the source cell's actual stock — `saturating_sub` floored
        // the source's loss at zero while the neighbour still received the
        // full, uncapped amount, creating mass from nothing.
        let mut t = Terrain::new(4);
        t.cell_mut(0, 0)[Element::Wood] = 40_000;
        let before = t.total(Element::Wood);
        let tuning = TerrainTuning {
            diffuse_rate: PerElement::filled(1500),
            diffuse_cap: PerElement::filled(60_000),
        };
        apply_diffusion(&mut t, &tuning);
        assert_eq!(t.total(Element::Wood), before, "diffusion must not create mass even at rate > 1000‰");
    }

    #[test]
    fn diffusion_never_fabricates_mass_when_a_cell_has_four_simultaneous_outgoing_edges() {
        // Regression: even at rate == 1000‰ (100%), a cell with four poorer
        // neighbours (its own East/South, plus its West/North neighbours'
        // edges into it) used to be able to send up to 100% of its stock
        // down *each* of the four independently, since every edge's cap was
        // only checked against that one edge's own gradient — moving up to
        // 4x the cell's actual contents in a single tick.
        let mut t = Terrain::new(5); // large enough that the source's 4 neighbours are distinct cells
        t.cell_mut(2, 2)[Element::Water] = 40_000;
        let before = t.total(Element::Water);
        let tuning = TerrainTuning {
            diffuse_rate: PerElement::filled(1000),
            diffuse_cap: PerElement::filled(u16::MAX),
        };
        apply_diffusion(&mut t, &tuning);
        assert_eq!(t.total(Element::Water), before, "diffusion must not create mass across a cell's combined edges");
        assert_eq!(t.cell(2, 2)[Element::Water], 0, "the source should be fully (not over-) drained");
    }

    #[test]
    fn diffusion_respects_the_cap_on_a_side_two_torus() {
        // Regression: at side == 2, East/South wrap onto a neighbour
        // already reachable from the opposite direction, so the same
        // physical edge was visited (and its flow applied) twice.
        let mut t = Terrain::new(2);
        t.cell_mut(0, 0)[Element::Metal] = 65_000;
        let before = t.total(Element::Metal);
        let tuning = TerrainTuning {
            diffuse_rate: PerElement::filled(1000),
            diffuse_cap: PerElement::filled(40),
        };
        apply_diffusion(&mut t, &tuning);
        assert!(t.cell(1, 0)[Element::Metal] <= 40, "east neighbour exceeded the cap: {}", t.cell(1, 0)[Element::Metal]);
        assert!(t.cell(0, 1)[Element::Metal] <= 40, "south neighbour exceeded the cap: {}", t.cell(0, 1)[Element::Metal]);
        assert_eq!(t.total(Element::Metal), before, "diffusion must not create mass on a side-2 torus either");
    }

    #[test]
    fn diffusion_never_fabricates_mass_when_several_sources_overpack_one_destination() {
        // Bug 3 regression: `out_total`/source-side scaling protects a
        // SOURCE cell's total outgoing flow from exceeding its own stock,
        // but had no equivalent protection on the DESTINATION side -- four
        // independent source cells, each individually well within its own
        // per-edge cap and its own source-side scaling, can still together
        // aim more at one shared destination than that destination has
        // headroom for. The destination is deliberately set close to
        // `u16::MAX` (headroom 10) while all four of its neighbours sit at
        // the true ceiling, so the natural per-edge gradient (10, well
        // under the flat cap) from each of the four still sums to 40 into a
        // cell with only 10 headroom -- exactly the adversarial "several
        // near-saturated sources, one already-full destination" setup.
        let mut t = Terrain::new(5); // large enough that the centre's 4 neighbours are distinct cells
        t.cell_mut(2, 2)[Element::Fire] = u16::MAX - 10; // the shared destination, 10 units of headroom
        t.cell_mut(1, 2)[Element::Fire] = u16::MAX;
        t.cell_mut(3, 2)[Element::Fire] = u16::MAX;
        t.cell_mut(2, 1)[Element::Fire] = u16::MAX;
        t.cell_mut(2, 3)[Element::Fire] = u16::MAX;
        let before = t.total(Element::Fire);
        let tuning = TerrainTuning { diffuse_rate: PerElement::filled(1000), diffuse_cap: PerElement::filled(1000) };

        apply_diffusion(&mut t, &tuning);

        // Floor-division rounding (the same conservative discipline the
        // source side already uses) can leave a handful of units short of
        // the destination's exact headroom, but must never exceed
        // `u16::MAX` and must land *something* close to the full 10 units
        // of headroom -- not silently clip away nearly all of it the way
        // the pre-fix `saturating_add` did.
        let gained = t.cell(2, 2)[Element::Fire] - (u16::MAX - 10);
        assert!(gained >= 8, "destination should absorb nearly all of its 10 units of headroom, got {gained}");
        assert!(t.cell(2, 2)[Element::Fire] <= u16::MAX);
        assert_eq!(
            t.total(Element::Fire),
            before,
            "diffusion must not create or destroy mass even when several sources overpack one destination"
        );
    }

    #[test]
    fn apportion_fallback_decorrelates_across_elements_and_channels() {
        // Regression: every element's fallback draw used the same
        // (seed, tick, id=0) coordinate regardless of which element or
        // direction (deposit/consume) was being apportioned, so an
        // all-extinct tick piled every element's floor onto the identical
        // cell. The salt (race, add) must break that correlation.
        let empty = BTreeMap::new();
        let mut winners = std::collections::BTreeSet::new();
        for e in Element::ALL {
            let mut t = Terrain::new(5); // 25 cells; total=26 leaves a remainder of 1
            apportion(&mut t, e, 26, &empty, 0x1234, 7, true, animal(e));
            for y in 0..5 {
                for x in 0..5 {
                    if t.cell(x, y)[e] == 2 {
                        winners.insert((x, y));
                    }
                }
            }
        }
        assert!(
            winners.len() > 1,
            "every element's fallback remainder landed on the same cell: {:?}",
            winners
        );
    }

    #[test]
    fn apportion_decorrelates_deposit_from_consume_for_the_same_element() {
        let empty = BTreeMap::new();
        let mut t_add = Terrain::new(5);
        apportion(&mut t_add, Element::Fire, 26, &empty, 0x1234, 7, true, animal(Element::Fire));
        let mut t_sub = Terrain::new(5);
        for y in 0..5 {
            for x in 0..5 {
                t_sub.cell_mut(x, y)[Element::Fire] = u16::MAX; // headroom so saturating_sub doesn't mask the result
            }
        }
        apportion(&mut t_sub, Element::Fire, 26, &empty, 0x1234, 7, false, animal(Element::Fire));

        let winner = |t: &Terrain, target_amount: u16| {
            let mut found = None;
            for y in 0..5 {
                for x in 0..5 {
                    if t.cell(x, y)[Element::Fire] == target_amount {
                        found = Some((x, y));
                    }
                }
            }
            found
        };
        let add_winner = winner(&t_add, 2);
        let sub_winner = winner(&t_sub, u16::MAX - 2);
        assert!(add_winner.is_some() && sub_winner.is_some());
        assert_ne!(
            add_winner, sub_winner,
            "deposit and consume for the same element must not draw the identical fallback pattern"
        );
    }
}
