# Pentagram S1 — the terrain field

Final design for Stage 1, synthesized from three independent drafts (literal-minimalism,
succession-dynamics, determinism/performance) against the actual Stage 0 tree
(`src/*.rs`, `src/bin/chaos/*.rs`, `tests/determinism.rs`). Every place this document
resolves a disagreement between the source drafts, or fills a gap the README leaves
open, is called out explicitly — as a **decision** (with the rejected alternative and
why) or an **assumption** (flagged, not asserted as given).

The only spec for S1 is one sentence in `README.md`'s "Next" section:

> S1 — the terrain field: five `u16` saturations per cell, six operators in fixed
> order, filmstrip output, hot reload. Exit condition is
> succession visibly cycling with no absorbing state over 30 simulated days.

Everything below is this document's answer to that sentence, written to be handed to
an engineer and implemented directly.

---

## 1. Grid data structure

```rust
// src/terrain.rs
use crate::element::{Element, PerElement};
use crate::hash::{Hashable, Hasher};

#[derive(Clone, Debug)]
pub struct Terrain {
    /// Cells per side. Always equal to `World::size.floor_int()` — see the
    /// 1:1 resolution decision below.
    pub side: i32,
    /// Row-major, cell-major: `index = y * side + x`, and all five
    /// saturations of one cell are contiguous. Invariant IV — this is the
    /// one order every operator, the hash, and both renderers iterate in.
    cells: Vec<PerElement<u16>>,
}
```

**Layout: array-of-structs, not struct-of-arrays.** All three drafts converge here
independently, and for a good reason: two of the six operators (ring, star) read and
write all five channels of *one* cell and touch no neighbor; deposit/consume touch one
channel of one cell at a time but are population-bound, not grid-bound; only diffusion
is naturally per-element-plane-shaped, and even there the neighbor stride is dominated
by `side` regardless of AoS/SoA. Keeping a cell's five values contiguous (`PerElement<u16>`,
which already carries the ring-order iteration contract Invariant IV requires) lets
ring+star process a cell in one touch instead of five strided ones. This also matches
Stage 0's own bias — `Entity` is one struct per body, not a struct-of-arrays.

**Resolution: 1:1 with `World::size`, square.** **Decision, all three drafts agree,
flagged as an assumption in every one of them (see §9 item 1):** the README never says
whether terrain resolution should be coarser than the entity coordinate space. 1:1 is
adopted because `World::size` already *is* "the simulated square, in cells"
(`world.rs`'s own doc comment), the live view already discretizes entity positions to
a `size`-cell grid this way, and a second independent "terrain resolution" parameter
would be a second source of truth for what a cell is. `Terrain::cell_of(pos: V2) ->
(i32, i32)` is `(pos.x.floor_int().clamp(0, side-1), pos.y.floor_int().clamp(0,
side-1))` — the same floor/clamp `World::phase_movement` already performs.

**On `World`:**

```rust
pub struct World {
    // ...existing fields unchanged...
    pub terrain: crate::terrain::Terrain,
    pub terrain_tuning: crate::terrain::TerrainTuning,   // §2, §6
}
```

`World::new(seed, size_cells)` constructs `Terrain::new(size_cells)` from the size
already passed in.

**Hashing.** `Terrain` gets a straightforward `Hashable` impl (row-major cells,
ring-order per cell):

```rust
impl Hashable for Terrain {
    fn hash_into(&self, h: &mut Hasher) {
        h.i32(self.side);
        for c in &self.cells {
            for (_, v) in c.iter() { h.u16(*v); }
        }
    }
}
```

`World::state_hash` gains this call placed **next to `entities`**, not next to
`races`: terrain cells are moving world state ("what's physically in the world right
now"), the same category as entity positions, while `terrain_tuning`
is configuration and belongs next to `races` — mirroring the existing comment there,
*"a retuned world must not hash the same as an untuned one,"* which now has to hold for
terrain tuning too.

**Memory cost, at the sizes the live view already offers:**

| size | cells | grid (5×u16/cell) | + diffusion double-buffer |
|---|---|---|---|
| 96×96 | 9 216 | 92 160 B (~90 KiB) | ~180 KiB |
| 256×256 | 65 536 | 655 360 B (~640 KiB) | ~1.28 MiB |

Trivial at either size — this section exists to show the arithmetic, not because
memory is a real constraint at S1. `World` already derives `Clone` (used by tests and
by `verify`'s two-run comparison); a clone now costs up to ~2 MB at 256×256 instead of
a few hundred bytes, which is worth noting for anyone adding a hot clone-heavy loop
later, but is not itself a problem.

---

## 2. The six operators, in fixed order

This is the centerpiece, and the one place all three drafts had to be checked hardest
against each other, because — per `world.rs`'s own doctrine, "the tick phase order is
a wire format, not an implementation detail" — this order ships once and then every
recorded replay depends on it.

All six run inside one new phase, `phase_terrain`, gated by the identical boundary
`phase_settle` already uses, and placed **immediately after `phase_settle` and before
`phase_reap`**:

```
commands → aging → movement → collisions → settle → terrain → reap
```

It runs right after `settle` so every operator sees this tick's freshly computed
`last_deposit`/`last_consume` `Grant`s rather than last tick's, and it runs before
`reap` only by the same "the tick still ends by removing the dead" convention the
existing six-phase list already follows — terrain touches the grid, `reap` touches
`entities`, so the two commute, but keeping one fixed closing phase is easier to hold
in your head as a wire format than a conditional one.

**Fixed order:**

1. **Deposit** — apply
2. **Consume** — apply
3. **Attrition** — ring's relation (`eaten_by`), applied to bodies
4. **Suppression** — star's relation (`suppressed_by`), applied to bodies
6. **Diffusion** — bounded spread

> **Update, post-S2.** Slots 3 and 4 originally shipped as `terrain.rs`
> operators `ring`/`star` — terrain converting a permille of its own stock
> into the next ring element, and terrain nullifying a permille of its own
> stock against its suppressor's, both running every terrain tick with **no
> entity involved at all**. That's a real design flaw: terrain should never
> be an actor unto itself, only a surface that changes because of what
> bodies do. Deposit/consume already satisfy that (governor-gated, per-race
> demand); ring/star didn't. The fix keeps the ring/star *relations* but
> redirects them onto whatever body is standing on the cell instead of onto
> the cell itself — `ecology::apply_attrition` (`eaten_by()`, hp damage) and
> `ecology::apply_suppression` (`suppressed_by()`, added `hunger`). Slots 3
> and 4 are no longer `terrain.rs` functions; they're `ecology.rs` functions
> called from the same position in `World::phase_terrain`. The rest of this
> section is left as originally written, describing the operators that
> *used* to occupy these slots, because the ordering reasoning below (why 3
> comes after 2, why 5 comes after 4) mostly still applies to their
> replacements — see the callouts inline.

### Decision: what element layer deposit and consume touch

**All three drafts had to guess this — the README never says.** Two of the three
(independently) converged on the same answer, which this document adopts:

- **Deposit writes `race.element`** (self). A Fire body's deposit is a scorch mark —
  Fire-colored terrain.
- **Consume removes `race.element.eats()`** (the ring, read backward). A Fire body's
  consumption burns down the *Wood* layer near it, not the Fire layer — `eats()`'s own
  doc comment already says *"Fire eats Wood... Energy transfers along this edge."*

This is chosen over the simpler self/self alternative (the third draft's guess)
because it gives `race.rs`'s otherwise-curious design a reason to exist: every race
already has a **separate** `consume_unit`/`consume_mix`/`consume` band, fully distinct
from `deposit`'s — if consume just meant "remove your own element," that whole second
band would be mechanically redundant with deposit's negative. Reading consume as
"metabolize what you eat" gives it real, distinct meaning, and lines up with
`element.rs`'s own framing that the five-cycle relation set is *"one relation set,
read three ways (combat, ecology, terrain)"* — under this reading, `beats()` is a
future combat reading, consume-via-`eats()` is this stage's ecology reading, and the
ring/star **operators** below (3 and 4) are the terrain reading, a clean three-way
split rather than two overlapping ideas fighting over one relation. It is still a
guess, not a README fact — flip is a one-line change if the project owner disagrees
(§9, §10).

### Justification, operator by operator

**1 — Deposit.** Turns each race's already-settled `Grant.granted` (from
`last_deposit`, produced by the unmodified `phase_settle`) into per-cell increments at
the occupied cells of that race's living bodies (§3 has the distribution algorithm).
Runs first because it is the tick's freshest fact — what bodies actually earned this
tick — and every downstream operator should react to a grid that already includes it.

**2 — Consume.** Turns each race's `last_consume` `Grant` into per-cell decrements on
`race.element.eats()`, at the same occupied cells (§3, same algorithm, same
occupancy table — built once, reused). Runs immediately after deposit so a race that
both produces and consumes in the same terrain tick can draw on material deposited
*this same tick* rather than being perpetually one terrain tick behind for no
ecological reason.

**3 — Ring succession (`generates`).** *(Superseded — see the update box above. As
originally shipped:)* the terrain's own ambient metabolism, independent of any entity:
each tick, a bounded permille of every cell's stock of element `e` converts to
`e.generates()` — Wood's decay feeds Fire's fuel, Fire's ash becomes Earth, Earth's
compaction becomes Metal, Metal's oxide dissolves into Water, Water's evaporation feeds
Wood. This is the literal terrain reading `element.rs` promises for the ring, and it
was meant to give "succession" meaning even where no race has ever set foot — a cell's
dominant element drifting around the ring on its own, forever. That "on its own" is
exactly what's wrong with it. `apply_attrition` still runs after entity effects (1, 2)
so it reads this tick's actual terrain, not a stale one — that part of the reasoning
survives the redirect intact.

**4 — Star suppression (`suppressed_by`).** *(Superseded — see the update box above. As
originally shipped:)* the balancing check: each cell's element `e` loses a bounded
permille of `e.suppressed_by()`'s current stock, in the same cell — a pure
nullification, no transfer, exactly `element.rs`'s own text: *"No energy transfers
along this edge"* (contrast operator 3's `eats()` doc: *"Energy transfers along this
edge"*). This was meant to stop deposit- and ring-driven growth from running away
inside a single cell; with ring gone and `apply_suppression` no longer writing terrain
at all, that balancing job has no owner — §7's "no frozen extreme" check is no longer
mechanically guaranteed the way it was, only empirically true or false of whatever
`EcologyTuning` is shipped (see the exit-condition update in §7).
`apply_suppression` still runs immediately after `apply_attrition` (4 after 3), since
nothing forces a different order and keeping the slot stable minimizes the diff.

**6 — Bounded diffusion.** Runs last, and this is **Invariant I's literal home** (see
below for the concrete cap). Running it last means every one of this tick's other
operators gets exactly one diffusion pass before the tick ends — deposit, consume,
ring, and star's output all spread by the same fixed, bounded amount this
tick, with no exceptions and no operator's contribution waiting an extra tick to begin
propagating. That uniformity is what makes the diffusion cap's claim — "influence `N`
cells away needs at least `N` terrain ticks" — exact for *every* source term, not just
some of them.

### Adjacent-swap failure table — what breaks, concretely

| swap | what breaks |
|---|---|
| **Deposit ↔ Consume** | Consumption would always act on *last* tick's terrain rather than this tick's. A race that both produces and consumes in one window would show a spurious one-terrain-tick lag between production and feeding with no ecological cause — visible in the filmstrip as production and consumption never quite lining up spatially, even when the entities are co-located. |
| **Consume ↔ Attrition/Suppression** *(was Consume ↔ Ring)* | Attrition/suppression must run after consume so a body's damage/hunger this tick reflects terrain that already includes this tick's deposit and consume — the same "react to fresh, not stale, terrain" reasoning the original Ring justification stated, still true of its replacement. Unlike the original Ring, there's no longer a *competing-demand* concern here (attrition/suppression don't touch `Grant`/the governor at all), so this row is weaker than it was: it's about data freshness, not about two mechanisms colliding over the same budget. |
| **Attrition ↔ Suppression** *(was Ring ↔ Star)* | The original row's reasoning was about a conserving transfer (ring) vs. a destructive nullification (star) racing over the *same terrain stock* — order mattered because one drained what the other might annihilate first. Neither `apply_attrition` nor `apply_suppression` writes to terrain anymore; each independently reads terrain and writes to a different `Entity` field (`hp` vs. `hunger`). They no longer interact with each other at all, so this swap is now inert — the two could run in either order with an identical result. The slot order is kept as-is (attrition before suppression) purely to minimize the diff from the original six-slot layout, not because anything breaks if reversed. |
| **Suppression ↔ Diffusion** | Diffusion must be last. Consume (slot 2) is the only operator that destroys terrain mass outright rather than moving or adding it, and it already runs well before diffusion; nothing between consume and diffusion touches a cell's stock, so this swap is inert in the same way the row above is — kept in order purely to minimize the diff. |

### Snapshot semantics — the correctness trap that used to span three of the six operators

**Diffusion must read a full pre-operator snapshot before writing anything.** Ring and
star used to share this requirement; they don't anymore, because neither still exists
as a terrain-to-terrain operator (see the update box in §2).

- **`apply_attrition`/`apply_suppression`** (`ecology.rs`) iterate *entities*, not
  cells, and each entity reads terrain but never writes it — there is no
  read/write-order hazard to guard against inside a single call the way ring/star used
  to have with each other's outputs. (Historical note, since the original reasoning is
  still instructive for why the *old* ring/star needed it: they were per-cell local,
  each cell's outcome depending only on that cell's own five values, so they needed a
  **per-cell** snapshot — read all five values into a local array, compute every
  outflow/loss from that snapshot, then write all five results back. Processing
  `Element::ALL` in index order and writing back as you go would have let Wood's
  transfer into Fire become visible when Fire's *own* transfer into Earth was computed
  later in the same pass — silently letting material hop two ring-steps in one
  operator invocation, an artifact of iteration order rather than a designed rule.)
- **Diffusion** is not per-cell — it reads and writes neighbors — so it still needs a
  **whole-grid** snapshot: a double buffer, `dst` starting as a full copy of the
  pre-operator grid, written to, then swapped in. An in-place row-major pass would let
  column 0's outflow contaminate column 1's inputs within the same operator, letting
  mass travel two cells in one terrain tick — which is precisely the bound Invariant I
  exists to prevent.

### The diffusion cap, concretely — Invariant I's home

Gradient-based (Fickian) diffusion, not a flat outflow-regardless-of-destination push:
material moves down the concentration difference between two cells, proportionally,
with a **flat per-edge ceiling** on top. This is chosen over a simpler "always push a
fixed fraction of the source out" scheme because a source-only push can move mass
*uphill*, from a poor cell into an already-richer neighbor, which is physically
backwards and would make the map's texture harder to read; a gradient formula
naturally equilibrates and never does that, at the same integer-arithmetic cost.

```rust
// Operator 6. Reads the pre-tick snapshot (dst starts as a copy of cells),
// visits each cell's East and South edges only (so every edge is processed
// exactly once), writes into dst, then swaps dst into cells.
for y in 0..side {
    for x in 0..side {
        let here = terrain.cell(x, y); // pre-tick snapshot value
        for (nx, ny) in [(x + 1, y), (x, y + 1)] {
            if nx >= side || ny >= side { continue; } // closed, no-flux boundary
            let there = terrain.cell(nx, ny);
            for e in Element::ALL {
                let diff = here[e] as i32 - there[e] as i32;
                let rate = tuning.diffuse_rate[e] as i32;   // permille
                let cap = tuning.diffuse_cap[e] as i32;     // flat units/edge/tick
                let flow = (diff * rate / 1000).clamp(-cap, cap);
                // flow > 0 moves `here -> there`; flow < 0 moves the reverse.
                dst[idx(x, y)][e]   = dst[idx(x, y)][e].saturating_sub_signed(flow);
                dst[idx(nx, ny)][e] = dst[idx(nx, ny)][e].saturating_add_signed(flow);
            }
        }
    }
}
std::mem::swap(&mut terrain.cells, &mut dst);
```

`diffuse_cap` (a `PerElement<u16>`, e.g. default 50 units/edge/terrain-tick, tunable —
§6) is the literal, concrete content of "nothing acts instantly at a distance": no
matter how saturated a source cell is, a single edge can carry at most `diffuse_cap`
units across in one terrain tick, full stop. Without a flat cap, a purely proportional
scheme still bounds the *fraction* that can move but not the *absolute amount* — as a
cell's saturation grows toward `u16::MAX`, a proportional-only scheme moves an
unbounded absolute quantity per tick, which is a weaker and less testable claim than
"a source at cell `C` cannot influence a cell `k` cells away within fewer than `k`
terrain ticks, for any `k`, regardless of magnitude." The cap is what makes that claim
literally, numerically true, and §7 tests it directly.

**Boundary condition: closed / no-flux, no wraparound.** Edge and corner cells simply
have fewer edges to process (2 instead of 4) — nothing is ever silently discarded and
nothing is ever silently invented at a boundary; total grid mass added by diffusion
across the whole grid in one tick is exactly zero, by construction, since every `flow`
subtracted from one cell is added to exactly one neighbor. **Flagged as an assumption**
(toroidal wrap is an equally defensible alternative the README does not rule out) —
see §9.

**Saturating arithmetic is mandatory, not optional.** `overflow-checks = true` in every
profile means `u16 ± u16` panics on overflow/underflow. Every one of the six operators
must use `saturating_add`/`saturating_sub` (or the signed variants shown above)
exclusively on cell values — the same discipline `Fx` already documents for itself.
Any `u64`/`u128` intermediate (a distribution share, a governor grant, a diffusion
flow) cast down to `u16` must clamp first; a bare `as u16` truncates/wraps instead of
saturating, which is the "silently wrong" failure mode `fx.rs`'s own doc comment
explicitly calls out as the thing to avoid.

---

## 3. Spatializing the Governor

**`Governor` is untouched.** It still runs exactly once per race (== once per element —
`race.rs`'s table is indexed by `Element` directly, there is no separate race
dimension) per terrain tick, still consumes one aggregate `u64` demand and produces one
`Grant { granted, forced, clipped }`, and all 9 property tests in `governor.rs` plus
the two in `world.rs` (`governors_always_grant_inside_their_band`,
`an_extinct_race_still_churns_its_terrain`) keep passing completely unmodified — none
of them observe anything spatial, because `Grant.granted` stays exactly what it is
today. All new work is a pure, deterministic **distribution** step downstream of
`settle()`, inside the new Deposit/Consume operators.

### Algorithm: weighted, largest-remainder apportionment

Shared by operators 1 and 2, once per element per channel (deposit, consume):

1. **Build one occupancy table per terrain tick**, shared by both operators: walk
   `self.entities` once, in the existing ascending-`id` order Invariant IV already
   guarantees, and for each `alive` entity, map its position to a cell
   (`Terrain::cell_of`) and increment `weight[cell]` for that entity's `element`. This
   is `O(population)`, not `O(grid)`, and is a pure function of already-agreed state —
   both territories already have bit-identical entity positions (Stage 0's own replay
   guarantee), so both compute bit-identical occupancy without exchanging a byte.
2. **If total weight is zero** (race extinct here, or off-grid — shouldn't happen given
   clamping, but handled): spread `total = Grant.granted` **uniformly across every
   cell in the grid**, `base = total / cells`, with the `total % cells` remainder units
   handed out starting from a **rotating offset**, not a fixed one:
   ```rust
   let start = rand_below(seed, terrain_tick, 0, Channel::Terrain, cells as u32);
   ```
   then wrapping in row-major order from `start`. **Decision:** one of the three source
   drafts uses a *fixed* row-major starting point (first `N` cells, every tick). That is
   rejected here: in the uniform-fallback case every cell ties on weight, so with a
   fixed starting point the *same* handful of low-index cells would win the leftover
   remainder unit **every single terrain tick for the life of the world** — a
   permanent, visible artifact (a fixed blob or line) in the corner of an otherwise
   ambient floor, which is exactly the kind of un-designed asymmetry this document
   otherwise goes out of its way to avoid (see the ring/star ordering discussion in
   §2). A stateless-hash-rotated starting offset costs one extra `rand_below` call per
   race per channel per terrain tick and removes the artifact entirely — this is the
   spatial form of "an emptied server keeps turning over," and it should read as
   ambient hum, not as a permanently painted corner.
3. **Otherwise**, with `W = Σ weight[cell]` over occupied cells (`u64`, safely bounded —
   population is at most a few hundred per race even at soak scale):
   - `base[cell] = (total as u128 * weight[cell] as u128 / W as u128) as u64` — the
     `u128` intermediate makes the multiply overflow-proof regardless of how large
     `total`/`weight` get, with no `checked_mul` needed; the result is provably `≤
     total`, so the final cast is safe.
   - `remainder = total - Σ base[cell]` (always `< occupied.len()`, since each cell's
     flooring loss is `< 1`).
   - Sort occupied cells by `(fractional remainder desc, tie_key asc)`, where
     `tie_key = rand_below(seed, terrain_tick, cell_index as u32, Channel::Terrain,
     u32::MAX)` — a **stateless, per-tick-rotating** tie-break, not a fixed
     ascending-cell-index one. **Decision, same reasoning as step 2:** a static
     index-order tie-break would let the lowest-index occupied cells win every tie
     forever whenever two cells' weights happen to coincide (common at low
     populations, where several cells often hold exactly one body each) — a slow,
     invisible bias toward one part of the map over a 43 200-terrain-tick run. The
     rotating hash tie-break costs nothing extra architecturally (the same channel and
     coordinate scheme already in use everywhere else in the crate) and removes the
     bias.
   - Give `+1` to the first `remainder` cells in that sorted order.
4. **Write**: deposit does `cell[race.element] = v.saturating_add(base[cell] as u16)`;
   consume does `cell[race.element.eats()] = v.saturating_sub(base[cell] as u16)`
   (clamped at `u16::MAX` / `0` respectively). Both share step 1's occupancy table —
   built once per terrain tick, not twice.

### Consume can legitimately outrun what a cell holds

A consume `Grant.granted` is an upper bound on *rate*, not a promise that the terrain
physically holds that much at the weighted cells. Excess demand at a specific cell is
simply not applied — `saturating_sub` floors at zero, the shortfall is not carried
forward or redistributed elsewhere. This does not violate any Governor guarantee
(those describe the aggregate `granted` number, which is exactly as before); it means
"granted" is a capacity bound on consumption, not a guarantee of physical availability.
All three source drafts converge on this and flag it explicitly — kept here as the
chosen behavior, flagged again in §9.

### Determinism story

Everything above is a pure function of `(self.entities, self.terrain, self.races,
last_deposit, last_consume, seed, terrain_tick)`, all of which are themselves already
pure functions of `(seed, input_log)` up to this point in the tick. Two territories
simulating the same entities: both already compute an identical `Grant` (Governor
unchanged, already proven deterministic); both build an identical occupancy table
(entities iterated in the same canonical ascending-id order on both sides — already
guaranteed); both compute identical `base`/`remainder` splits (pure integer arithmetic,
no platform-dependent rounding); both resolve ties and rotate the fallback offset
identically (the same `(seed, tick, id, channel)` hash primitive every other
stateless-random decision in the crate already uses). Nothing here reads wall-clock
time or iterates a `HashMap` — so it composes with `replay.rs` without any new
machinery there, extending Stage 0's existing story by exactly one more deterministic
transformation, the same way the RNG's own doc comment already promises for any new
decision.

**Cost:** `O(P log P)` per element per channel, where `P` is that race's live
population (the `log` is from sorting occupied cells for the tie-break) — independent
of grid size, which is the point: apportionment cost tracks population, not map area,
so it stays cheap even at 256×256.

---

## 5. Filmstrip output

**New standalone bin, `src/bin/filmstrip.rs`**, following the exact pattern of
`src/bin/soak.rs`/`src/bin/verify.rs` — headless, deterministic CLI args, no
ANSI/keyboard machinery. **Decision, unanimous across all three drafts:** not a `chaos`
live-view mode. The live view is inherently interactive and explicitly, deliberately
non-reproducible the moment a knob is touched (`main.rs`'s own retuning story); a
filmstrip is exactly the kind of artifact that should be regenerable byte-for-byte from
`(seed, ticks, cadence)` alone, matching `verify`'s whole reason for existing.

```
cargo run --release --bin filmstrip -- [seed] [size] [per_race] [ticks] [cadence] [outdir]
  seed     world seed                          (default 0xBEEF)
  size     grid side in cells                  (default 96)
  per_race starting population per race         (default 20)
  ticks    total sim ticks to run               (default 4_320_000  = 30 days)
  cadence  sim ticks between captured frames     (default TICKS_PER_HOUR = 6_000)
  outdir   directory for frame_NNNNNN.ppm        (default ./filmstrip)
```

Default cadence gives 720 frames over the full 30-day window — fine-grained enough to
show the seasonal cycle as smooth motion between frames, coarse enough to stay a
reasonable file count; CLI-overridable.

**What each frame captures:** the terrain grid only, one pixel per cell, in **exact
integer arithmetic** — `pixel = Σ(RGB[e] * sat[e]) / Σ(sat[e])`, all `u32`, falling
back to a fixed background color when every saturation is zero. **Decision:** one of
the three source drafts allows itself a `#![allow(clippy::float_arithmetic)]` for this
blend, citing the real precedent that `chaos/main.rs`, `chaos/view.rs`, and `soak.rs`
already carry that exact attribute (for elapsed-time/throughput math, not rendering).
This document rejects using it here: the blend is small-integer arithmetic (`u16`
saturations, `u8` RGB channels) with no precision need floats would actually buy, so
paying zero dependency-risk and needing no escape hatch at all is strictly better than
the alternative at equal implementation cost. It is *stricter* than `Fx::to_f32_render`'s
own "render-only, escape-hatch-permitted" discipline, not merely consistent with it.
The RGB palette itself is the same five constants `chaos/view.rs::RGB` already defines
— move that array to a small shared location (e.g. `pub const RGB` in `terrain.rs`) so
both the live view and the filmstrip bin read one definition instead of duplicating it.

**Format:** binary PPM (`P6`) — the smallest format hand-encodable with `std::fs`
alone, honoring the zero-dependency rule (no PNG encoder crate). One file per frame,
`frame_000000.ppm`, `frame_000001.ppm`, … , written as produced rather than buffered in
memory, plus a plain-text `manifest.txt` recording `seed`, `size`, `cadence`, and the
sim-tick number for every frame, so a frame can always be traced back to an exact point
in a reproducible run. Stitching frames into a video/GIF is explicitly **out of the
crate**, left to an external tool (`ffmpeg`) at tuning time — that conversion is
presentation tooling, not simulation, and keeps `Cargo.toml` empty.

**Production path and render-only guarantee, enforced by construction:** the bin builds
a `World` via the existing `replay::build(seed, size, per_race)`, steps it with
`replay::scripted_log` (both already exist and are exercised by `tests/determinism.rs`
today) so a filmstrip run is itself a replayable artifact, and the frame-writing
function takes `&World` (immutable), reading only `world.terrain` — never called from
`World::step` or any of the six operators, only from the bin's own loop around
`w.step(&log)`. This mirrors the discipline `Fx::to_f32_render`'s doc comment states
for itself: *"if you see it called anywhere outside a renderer, that is the bug."*

---

## 6. Hot reload

**Applies to one new tuning table**, analogous in shape and role to `race.rs`'s
`RaceAttrs`/`RACES`: `TerrainTuning` (ring rate, star rate, diffuse rate + cap — all
`PerElement`, §2). **Decision, all three drafts agree and for the same reason:** every
field is `PerElement<_>` rather than one global scalar per operator. This is not strictly
required by "six operators in fixed order," but it drops straight into the *existing*
`chaos/knobs.rs` machinery unmodified — that file's `Knob { get: fn(&Tuning, Element)
-> i64, set: fn(&mut Tuning, Element, i64) }` abstraction is already built entirely
around "one value per race/element," and a global scalar would need a second, parallel
knob abstraction just for this page. Per-element terrain tuning means "adding a
knob means adding a row to a table," exactly the promise the existing file already
makes for race attributes.

```rust
// src/bin/chaos/knobs.rs — additions
pub struct Tuning {
    pub races: PerElement<RaceAttrs>,   // existing
    pub restock: PerElement<u32>,       // existing
    pub terrain: TerrainTuning,         // new
}

static TERRAIN: [Knob; N] = [ /* ring rate, star rate, diffuse rate, diffuse cap */ ];

pub static PAGES: &[Page] = &[
    Page { title: "body & rates", knobs: &BODY },
    Page { title: "channel mix ‰", knobs: &MIX },
    Page { title: "terrain", knobs: &TERRAIN }, // new
];
```

**What "hot" means here.** Stage 0 already has two tiers of "hot," and this document
extends the existing runtime tier rather than inventing a third:

1. **File-level (`chaos watch`)** rebuilds the whole binary when `src/` changes — the
   coarse tier, needed for anything not yet exposed as a knob (a new operator, a new
   formula term). Out of scope for this document (`chaos` lives outside this repo, in
   `~/.local/bin`).
2. **Runtime (this design's tier).** `World` gains `retune_terrain(&mut self,
   t: TerrainTuning)`, mirroring the
   existing `World::retune` exactly — straight field replacement, since the table
   carries no governor-style internal state (no burst bucket to reconcile). `chaos/main.rs`'s
   per-frame line `w.retune(t.races)` gains a sibling,
   `w.retune_terrain(t.terrain)`, pushed every frame
   unconditionally — turning a terrain knob is visible on the very next frame's map
   render, on a running world, with no rebuild and no restart. `T` (write table) gains
   a matching block for the new table, in the same `*.tuned.rs` style.

**Why it matters:** the ring/star/diffuse rates are exactly the numbers nobody
can pick correctly on paper — per the exit condition (§7), they are what decides
whether succession actually cycles or collapses to monoculture, which is precisely the
category of number this project's whole live-view apparatus exists to let someone feel
by turning a knob and watching the map, rather than compute in advance. `race.rs`'s own
header already says it about itself: *"every number here is a knob, and every one is
meant to be moved"* — S1's terrain numbers are the same kind of thing.

---

## 7. Exit condition as a runnable check

> **Update, post-S2 (ring/star removal).** Confirmed by actually running both
> `#[ignore]`d tests below after `ring`/`star` were removed (§2's update box):
> **both now fail.** Both hit the same failure — Wood saturates to
> `cells × u16::MAX` and stays there past day 8, tripping check (c). This is
> mechanical, not surprising in hindsight: check (c) was guaranteed by
> star's balancing pass (§2's original justification for operator 4 said so
> explicitly — "what stops deposit- and ring-driven growth from running
> away inside a single cell"). With star gone and nothing else in
> `phase_terrain` capable of *destroying* terrain mass except entity-driven
> consume, an element that outpaces consume has nothing left to check its
> growth once diffusion can only spread the surplus, never remove it.
>
> Per the project's standing decision that terrain must not be its own actor,
> the fix is **not** reintroducing star; "no frozen extreme" became a
> live-tuning target rather than a property this stage guarantees by
> construction, exactly like S2's shipped `EcologyTuning` defaults not
> holding a population indefinitely (`docs/S1_TERRAIN_DESIGN.md` originally,
> now see `ecology.rs`'s own module doc and the README's S2 section).

**30 simulated days, in ticks:** `TICKS_PER_DAY = 144 000` (confirmed from `race.rs`:
`TICKS_PER_MINUTE(100) × 60 × 24`). 30 days = **4 320 000 sim ticks** = **43 200
terrain ticks** (`TERRAIN_PERIOD = 100`).

### "Absorbing state," made numeric for a `u16` grid

Four checks, all cheap to compute from a full-grid scan, synthesized from the
strongest, most concrete version of each across the three drafts:

- **(a) Frozen.** `Terrain`'s own hash (a standalone `terrain.state_hash()`, meaningful
  even at zero population where whole-world hash could still move from entity-only
  churn) must not repeat between two samples one simulated day apart, for any sampled
  day.
- **(b) Starved.** Total grid mass for any element, `Σ_cells sat[e]`, must not sit at
  exactly `0` for more than one full season (`8 640` terrain ticks) — a temporary
  seasonal trough is expected and legitimate; a permanent wipeout is the failure.
- **(c) Saturated.** Symmetric to (b): no element's total may sit at `cells × u16::MAX`
  for more than one season.
- **(d) Monoculture.** No single element's share of total grid mass may stay `≥ 90%`
  for the entire 30-day window.

### "Cycling," made numeric

Define the **dominant element** at any sampled point as `argmax_e Σ_cells sat[e]`,
ties broken by ascending ring index. Sample once per simulated day (30 samples over the
window) and require:

- **In a populated run** (a starting population per race, run headless with no
  restocking — S1 has no reproduction, so the back half of a long run is realistically
  testing terrain mechanics more than population dynamics; see §9 item 8): the
  defensible claim — **at least 3 of the 5 elements** appear as dominant
  across the 30 samples, and no element's dominant run is longer than 10 consecutive
  daily samples. Population dynamics add enough noise that requiring all five here
  would be asserting something about *entity* behavior this document has no basis to
  promise.

### Test sketch, in the style of `tests/determinism.rs`

```rust
// tests/succession.rs

const DAYS: u64 = 30;
const TICKS: u64 = DAYS * TICKS_PER_DAY;          // 4_320_000
const SIZE: i32 = 64;                              // kept small — see cost note
const SEASON_TICKS_SIM: u64 = TICKS_PER_DAY * 6;

#[test]
#[ignore]
fn thirty_days_of_succession_visibly_cycles_with_a_population() {
    let mut w = World::new(SEED, SIZE);
    w.seed_population(20);
    let log = InputLog::new();
    let mut dominant = Vec::with_capacity(DAYS as usize);
    for _day in 0..DAYS {
        for _ in 0..TICKS_PER_DAY { w.step(&log); }
        assert_no_absorbing_totals(&w.terrain, SEASON_TICKS_SIM);
        dominant.push(dominant_element(&w.terrain));
    }
    let distinct: std::collections::BTreeSet<_> = dominant.iter().collect();
    assert!(distinct.len() >= 3, "only cycled through {:?}", dominant);
    assert_no_run_longer_than(&dominant, 10);
}

#[test]
fn diffusion_never_exceeds_one_cell_per_terrain_tick() {
    // Fast, unit-scale, no World needed: seed one cell in an otherwise-empty
    // Terrain, run one operator call, assert only its direct edge-neighbors
    // changed; run k calls, assert nothing beyond k cells changed. Direct
    // proof of Invariant I, independent of population dynamics.
}

#[test]
fn thirty_day_terrain_state_replays_bit_identically() {
    // Extends Stage 0's existing replay discipline to the new terrain state —
    // cheap, and exactly in the spirit of tests/determinism.rs.
    let log = pentagram::replay::scripted_log(0x5EED, TICKS, 100);
    let mut a = World::new(SEED, SIZE);
    a.seed_population(20);
    let mut b = a.clone();
    for _ in 0..TICKS { a.step(&log); b.step(&log); }
    assert_eq!(a.state_hash(), b.state_hash());
}
```

**Cost note, stated rather than discovered in CI:** a 30-day run is ~430× longer than
Stage 0's existing 10 000-tick exit condition. Both 30-day tests above are `#[ignore]`d
out of the default `cargo test` path, run explicitly (mirroring how `soak` is a
separate invocation, not part of `chaos test`'s normal suite) or via a dedicated
headless bin later if that becomes the preferred shape; `diffusion_never_exceeds...`
and a short 2-day smoke-test variant of the succession check are fast enough to run on
every `cargo test` and catch an operator-order regression long before a 30-day run
would be needed to notice it.

---

## 8. File / module layout

**New files:**

| file | contents |
|---|---|
| `src/terrain.rs` | `Terrain`, `TerrainTuning`, indexing, `Hashable` impl, operators 1, 2 and 6 (`apply_deposit`, `apply_consume`, `apply_diffusion`), the shared RGB palette constant, `render_ppm`, unit + property tests. Operators 3 and 4 (`apply_ring`, `apply_star`) shipped here originally; post-S2 they're gone — see `src/ecology.rs`. |
| `src/ecology.rs` *(post-S2)* | `EcologyTuning`, S2's feeding/starvation/reproduction rates, plus operators 3 and 4 (`apply_attrition`, `apply_suppression`) — terrain's old ring/star relations, redirected onto bodies. Not a "new file" for S1 — noted here because these two operators now live in this file instead of `terrain.rs`. |
| `src/bin/filmstrip.rs` | headless PPM-frame export (§5) |
| `tests/succession.rs` | the exit-condition tests (§7) |

**Existing files that change:**

- `src/lib.rs` — `pub mod terrain;`; re-export `Terrain`,
  `TerrainTuning`; update the Invariant I table row from
  *"S1: terrain diffusion cap"* (placeholder) to point at `terrain::apply_diffusion`.
- `src/world.rs` — new fields `terrain`, `terrain_tuning`;
  `World::new` constructs them from `size_cells`; new `phase_terrain` inserted between
  `phase_settle` and `phase_reap`, calling the six operators in the fixed order from
  §2; `state_hash` gains the terrain block (next to `entities`) and the tuning
  block (next to `races`); new `retune_terrain` mirroring `retune`;
  the file's own "tick phase order is a wire format" doc comment updated to list all
  seven phases.
- `src/rand.rs` — append `Channel::Terrain = 8` after
  `Governor = 7`, never renumbering an existing discriminant.
- `src/bin/chaos/knobs.rs` — `Tuning` gains `terrain: TerrainTuning`; new knob array
  and a third `Page` entry (§6).
- `src/bin/chaos/main.rs` — `w.retune_terrain(t.terrain)`
  alongside the existing `w.retune(t.races)`; `write_table` extended for the new
  table; `z` (restart) reconstructs terrain fresh along with the world.
- `src/bin/chaos/view.rs` — `map()` currently draws bodies only, with an explicit
  caveat comment that the map is not yet meaningful; this becomes false at S1 and the
  function is replaced: terrain's dominant-element color as background (reusing the
  integer blend from `render_ppm`), entity glyphs drawn on top as today. This is the
  single most user-visible change in the whole design and worth prototyping early.
- `README.md` — Invariant I's "where it lives" cell filled in; the "no terrain" clause
  in "Known and deliberate" updated; "Next" moves from S1 to S2.

**Unchanged, deliberately:** `src/governor.rs`, `src/entity.rs`, `src/fx.rs`,
`src/hash.rs`, `src/input.rs`, `src/replay.rs`, `src/race.rs` — none of these need to
know terrain exists. `src/bin/soak.rs`/`src/bin/verify.rs` need no structural change to
keep working; a terrain summary line in `soak`'s report is a plausible, optional
follow-up, not required.

**Explicitly out of scope, flagged rather than silently ignored:** the README's
"Known and deliberate" section separately states that a uniform-grid collision
broadphase "arrives with the terrain field at S1" — but that sentence is not part of
the "Next" contract this document answers, and none of the nine sections above depend
on it. This design does not implement it, but `Terrain`'s row-major `(x, y) → index`
scheme is exactly the bucket indexing a broadphase would want, and `Terrain::cell_of`
is already the function it would call per entity — whoever builds it should reuse this
indexing rather than invent a second one.

---

## 9. Risks and open questions (assumptions, not given facts)

1. **Terrain resolution is 1:1 with `World::size`, and the grid is square.** Not stated
   by the README; chosen for the simplest possible coordinate story (§1).
2. **Deposit writes `race.element`; consume removes `race.element.eats()`.** The single
   highest-leverage guess in this document (§2) — two of three source drafts converged
   on it independently and it is well-motivated by `element.rs`'s and `race.rs`'s own
   text, but it is still an interpretation, not an assertion. Flip is a one-line change.
3. **The six operators' formulas and every rate constant are first guesses**, in the
   same spirit `race.rs`'s own header states of itself: *"every number here is a knob,
   and every one is meant to be moved."* Ring/star/diffuse rates and caps are
   starting points that need the
   live-view tuning pass §6 exists to enable — whether the shipped defaults actually
   produce visible ring-cycling rather than (a) a boring near-uniform grid or (b) an
   early-locked monoculture is an empirical question this document cannot answer
   without running the simulation.
4. **Boundary condition for diffusion is closed/no-flux, not toroidal wrap.** Materially
   changes edge-cell behavior and hash values; the README does not say either way (§2).
5. **Extinct-race spatial fallback is uniform-across-the-grid with a rotating offset,
   with no memory of where the race last had living bodies.** Simpler, no new hashed
   state, but loses "this used to be Fire territory" fidelity. A memory-based
   alternative is plausible future work, not designed here.
6. **Consumption below zero is silently lost, not borrowed, not redistributed, and not
   fed back into the Governor's next-tick demand signal.** A cell running out mid-tick
   does not currently make the Governor "aware" that demand physically could not be met
   at a location — whether a scarcity → `clipped` coupling belongs at S1 or later
   (S2/S3) is not resolved here.
7. **S1 has no reproduction** (per README's "Known and deliberate": that's S2). A
   30-day headless run with a fixed starting population and no restocking will age
   every race out well inside the window — even Earth's 14-day lifespan means the
   population is gone partway through 30 days — leaving the back half of any such run
   testing terrain mechanics essentially alone, not ecological succession in
   any populated sense. §7 runs the exit-condition check on a fixed cohort that ages
   out mid-window specifically because this ambiguity cannot be resolved
   from the repo alone; only S2's arrival fully answers what "succession" is meant to
   include.
8. **Filmstrip cadence, format, and manifest shape are proposals**, not specified
   anywhere in the README. PPM was chosen only because it is the smallest
   zero-dependency-compatible format; hourly cadence was chosen to balance smoothness
   against file count. Either is easy to change.
9. **The out-of-repo `chaos` wrapper script** (`~/.local/bin`) is not visible or
    editable from this checkout — whether it should grow a `chaos filmstrip` alias is
    unaddressed here, by necessity.
10. **The collision broadphase** mentioned in "Known and deliberate" is treated as a
    separate, out-of-scope performance item (§8) rather than folded into this design,
    since it is not part of the literal "Next" sentence this document answers.

---

## Assumptions/open questions to confirm with the project owner before implementing

The items above are exhaustive; these are the ones most worth a explicit go/no-go
before an engineer starts, ranked by how much they'd reshape the implementation if the
answer is "no":

1. **Deposit → self, consume → `eats()` (ring-backward).** This is the single most
   consequential interpretive call in the document (§2, §9.2) — it decides what the
   grid actually *means* mechanically. Confirm, or specify the intended alternative
   (self/self, or something else) before any code is written against it.
2. **Terrain grid is 1:1 with `World::size`.** Confirm this is intended, versus a
   coarser terrain resolution for large worlds (§9.1).
3. **Diffusion boundary is closed/no-flux, not toroidal.** A small but hash-affecting
   choice (§9.4) — confirm before it becomes load-bearing in recorded replays.
4. **Filmstrip is a standalone headless bin, not a `chaos` mode**, and produces PPM
   frames rather than any packaged video/GIF format (§5, §9.8). Confirm this matches
   how the team actually intends to *watch* succession day to day.
5. **Hot reload means the existing runtime-knob tier (§6), not `chaos watch`'s
   rebuild-on-save.** Confirm this reading of "hot" against what the project owner had
   in mind — the README's only precedent for "hot" is file-watch-and-rebuild, and this
   document deliberately reads S1's "hot reload" as the *other*, faster tier instead.
6. **No reproduction exists yet (S2), so the 30-day exit-condition run is necessarily
   a fixed cohort that ages out mid-window (§9.7).** Confirm
   the test design in §7 (a fixed starting population, no restocking) is an acceptable
   reading of "succession visibly cycling," given S1 cannot yet
   populate the whole window with living entities.
