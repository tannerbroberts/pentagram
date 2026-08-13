# Pentagram S3 — ecology layers (the plant/animal `Kind` split)

Final design for Stage 3, following the same method `docs/S1_TERRAIN_DESIGN.md` used:
grounded against the actual tree (`src/*.rs`, `src/bin/chaos/*.rs`, `tests/*.rs`) as it
stands after S2, with every place this document makes a call flagged explicitly as a
**decision** (with the rejected alternative and why) or an **assumption** (named, not
asserted as given).

The spec for S3 is not a README sentence — S1 and S2 shipped from one, but S3/S4 were an
explicit, undesigned gap ("nothing in this repo describes what they are"). S3's brief was
worked out in conversation instead: give animals hardcoded (no learning, no neural
nets) predator/prey instincts — aggro, panic, grazing — and give plants their own
reproduction mechanics (propagation frequency, offspring size, rooting requirements,
lifespan, production rate, resource consumption, terraforming behavior) as visible,
tunable knobs, matching the project's existing live-tuning philosophy (`RateBand`,
`ChannelMix`, `EcologyTuning`). Three forks got settled before this document was written,
and are treated as given below, not re-litigated:

1. Every element splits into a **Plant** and an **Animal** race variant — 10 races total
   via a new `Kind` axis, not a behavioral relabeling of the existing 5 and not a
   re-topology of the ring. `Element`'s mod-5 ring arithmetic is untouched.
2. Plants reproduce on their own decoupled clock, not animals' hp-threshold-on-meal
   trigger.
3. Animal-on-animal predation is a per-race tunable **hunt weight** (defaulting near
   zero), not a hardcoded herbivore/carnivore topology — reusing the same
   `Element::eats()` edge a second time.

Everything below is this document's answer to what that means mechanically, written to
be handed to an engineer and implemented directly, in the numbered stages §12 lays out.

---

## 1. `Kind`, `Race`, and `PerRace<T>`

```rust
// src/race.rs

/// Never renumber — hash/iteration-visible, same discipline as `rand::Channel`
/// (rand.rs:16-17) and `element.rs`'s ring order.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
#[repr(u8)]
pub enum Kind {
    Plant = 0,
    Animal = 1,
}

/// A race is (element, kind) — the axis `Entity` actually spawns as.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Race {
    pub element: Element,
    pub kind: Kind,
}

impl Race {
    pub const COUNT: usize = 10;
    /// Ring order primary, a race's two variants adjacent — mirrors why
    /// `element.rs`'s ring order matters (Invariant IV).
    pub const ALL: [Race; 10] = [ /* Wood-Plant, Wood-Animal, Fire-Plant, ... */ ];

    #[inline]
    pub const fn index(self) -> usize {
        self.element.index() * 2 + self.kind as usize
    }
}

pub struct PerRace<T>(pub [T; Race::COUNT]);
```

**Decision: `PerRace<T>`, a new type, not `PerElement<[T; 2]>`.** `PerElement<T>::iter()`
yields `(Element, &T)`; reusing `[T; 2]` for the Kind axis turns every one of the ~20
existing per-race iteration sites into a nested loop instead of a type rename, and the
same shape would mean two different things depending on which field it sits on.
`PerRace<T>` mirrors `PerElement<T>` member for member (`get`/`get_mut`/`iter`/`filled`/
`Index`/`IndexMut`) so callers migrate mechanically, and keeps *per-element* things
(terrain layers, `TerrainTuning`, `Terrain`'s own `PerElement<u16>` cells — these stay
5-wide) visibly distinct from *per-race* things (governors, demand, `RaceAttrs` itself —
these become 10-wide). That distinction is the entire point of S3, so the type system
should say it, not just a comment.

**Decision: `Kind`/`Race` live in `race.rs`, not `element.rs`.** The ring
(`generates`/`eats`/`suppresses`/`suppressed_by`) stays completely `Kind`-unaware — that
is the whole premise of treating Kind as orthogonal. `race.rs` is already "the design's
rate model" per `lib.rs`'s reading-order note; `Race` belongs there.

`Entity.element: Element` (`entity.rs:31`) is today the only race-identity field on a
body. It gains a sibling: `Entity.kind: Kind`. Predation/suppression/attrition relations
resolve off `element` alone, unchanged; race-attribute lookups resolve off `(element,
kind)`.

---

## 2. The ten-race table

`RACES: PerElement<RaceAttrs>` (race.rs:329-417) becomes `RACES: PerRace<RaceAttrs>`,
ten rows. `RaceAttrs` gains a `kind: Kind` field, hashed the same cast-to-`u8` way
`element` already is. `attrs(e: Element) -> &'static RaceAttrs` (race.rs:420-423)
becomes `attrs(r: Race) -> &'static RaceAttrs`.

**Blast radius, confirmed by direct read of every call site:** exactly one production
call site exists for the free function — `world.rs:119-120`, inside `World::new`,
building the governors. Everywhere else in production code already goes through the live
per-world `self.races[...]` table (seeded from `RACES`, swappable via `World::retune`).
The ~15 test-only call sites (`entity.rs`, `terrain.rs`, `ecology.rs`, `governor.rs`,
`tests/determinism.rs`, `tests/ecology.rs`) each become mechanical `attrs(Race{..})`
edits. `Entity::spawn` already takes `&RaceAttrs` by parameter rather than looking it up
internally (entity.rs:52), so plumbing the second axis through spawning needs no change
to `Entity::spawn` itself — only to what its caller passes.

**Assumption, to confirm before the table's real numbers are written (S3.2):** every
plant row is `speed: Fx::ZERO` and every animal row keeps a positive speed, following
directly from D3/§4 below. This document does not attempt final lifespan/deposit/consume
numbers for all ten rows — that is exactly the "first guess for the live tuning loop"
category `race.rs`'s own header already claims for the shipped table, and S3.2 is where
those numbers get chosen and defended against the re-decided narrative tests (§10).

---

## 3. Rekeying `World`: ten governors, not five

`RaceAttrs` carries its own `deposit`/`consume` `RateBand` per row. Merging a plant's and
an animal's demand into one governor would let one clip the other's grant — exactly the
per-race guarantee `governor.rs` exists to make. So `World`'s six `PerElement`-shaped
sibling tables all become `PerRace`-shaped:

```rust
pub races: PerRace<RaceAttrs>,
deposit_gov: PerRace<Governor>,
consume_gov: PerRace<Governor>,
deposit_demand: PerRace<u64>,
consume_demand: PerRace<u64>,
pub last_deposit: PerRace<Grant>,
pub last_consume: PerRace<Grant>,
```

**Confirmed by direct read of `governor.rs`: zero changes needed there.** `Governor` and
`Grant` are fully race-agnostic value types — `new`/`set_band`/`settle` take and return
plain `RateBand`/`Grant` values with no `Element` (or now `Race`) awareness at all. The
per-race keying has always lived entirely in `World`'s wrapper tables, so widening it to
ten is purely a `World`-side (and `terrain.rs`-side, below) concern.

`terrain::Occupancy` (terrain.rs:212-224) becomes `PerRace`-shaped. `apply_deposit`/
`apply_consume` (terrain.rs:322-364) map **race → element layer**: a race's deposit
target is `race.element`; its consume target is `race.element.eats()`. Terrain itself
stays 5-wide — a Wood-Plant and a Wood-Animal both write the Wood layer.

**Decision, a real bug caught in review, not a hypothetical:** `apportion`'s rotation
salt (terrain.rs:255) is currently `target.index() * 2 + !add as usize` — a 2-way salt
that assumes at most one race ever targets a given `(element, add)` pair. The moment two
races (Wood-Plant and Wood-Animal) can both deposit into the Wood layer in the same
terrain tick, this collides — both would rotate through the extinct-race fallback offset
identically, silently correlating their apportionment noise instead of decorrelating it
(exactly the property `apportion_fallback_decorrelates_across_elements_and_channels`,
terrain.rs, already tests for the *old* 5-wide case). The salt must widen to something
race-unique, e.g. `race.index() * 2 + !add as usize`. This is a required fix, not an
optional hardening — landing S3.1 without it reintroduces a correlated-noise bug the
existing test suite cannot catch because it doesn't yet have two races sharing a layer.

**Consequence to track, not silently absorb:** the extinct-race floor guarantee
(`RateBand::floor`, "always emitted, even at zero demand") now emits *two* floors into
each element layer per terrain tick instead of one. Baseline world churn doubles unless
the shipped plant+animal floors are chosen to split, not each independently carry,
today's per-element budget. Same reasoning applies to §3.1's terraform-pressure parity
metric — see §10.

---

## 4. Plants root: `phase_movement` and table validity

**Decision: immobility is a structural skip, not `speed = 0` alone.**
`phase_movement`'s per-tick jitter (`world.rs:314-317`, two `rand_signed(...,
Channel::MoveJitter)` draws) is added to `delta` regardless of `speed` — a zero-speed
body would still random-walk under jitter alone, defeating "rooted" as a claim about
position, not just about self-propelled motion. The mechanism is an early `continue` for
`Kind::Plant` in the per-entity loop (`world.rs:309`), before `step`/jitter/reflect/clamp
run at all.

Table honesty: plant rows still *ship* `speed: Fx::ZERO` (so the number on the row is
truthful, not just unread), and `RaceAttrs::is_valid()` (race.rs:259-265) gains a
kind-aware rule replacing today's universal `speed > Fx::ZERO`:

```rust
match self.kind {
    Kind::Animal => self.speed > Fx::ZERO,
    Kind::Plant  => self.speed == Fx::ZERO,
} && self.radius > Fx::ZERO   // both kinds still have a body that crowds
```

`Entity::spawn` stays kind-agnostic — it still rolls `initial_heading` for plants too
(entity.rs:57). One enforcement point (the `phase_movement` skip) beats two: a
`SetHeading` command aimed at a plant then writes a field nothing reads, which is honest
and hash-visible rather than a second silent special case layered on top.

`phase_collisions` (world.rs:356-393) is **not** skipped for plants — a thicket still
occupies space and crowds neighbours, using `races[race].radius` exactly as today.

---

## 5. `phase_feeding`: the predation surgery

The entire change is at `world.rs:442-450`, today's pairing derivation:

```rust
// today
let (pred, prey) = if a.eats() == b { (i, j) } else if b.eats() == a { (j, i) } else { continue };
```

**Decision — new derivation, same ring edge read two ways:**

```rust
// pred must be Kind::Animal; a Plant is never a predator
let (pred, prey, pred_kind_ok) = /* derive (i, j) or (j, i) exactly as today, then: */;
if entities[pred].kind != Kind::Animal { continue; }
// ring edge unchanged: pred.element.eats() == prey.element
match entities[prey].kind {
    Kind::Plant => { /* graze: always eligible, existing satiation + forage_radius gates */ }
    Kind::Animal => {
        // hunt: same gates, PLUS a hunt-weight roll
        if !rand_chance(seed, tick, entities[pred].id, Channel::Hunt, hunt_weight[pred_race], 1000) {
            continue;
        }
    }
}
```

**Decision: the hunt roll is per-predator-per-tick, not per-pair.** This is not a
stylistic simplification — it is required for §6's FSM and this phase to *agree*.
`phase_movement`'s Hunt state must steer toward the same prey class `phase_feeding` will
actually accept, and both run within the same `self.tick`. Invariant III (stateless
randomness) makes the two phases agree for free only if the roll depends solely on
`(seed, tick, id)` — a per-*pair* roll would let a body steer toward animal prey all tick
and then have `phase_feeding` refuse to eat it, which reads as a bug even though nothing
is technically broken.

**Decision: no prey-preference ranking.** First match in ascending-`j` order still wins,
exactly as today (world.rs:419-421's existing doc claim: the result depends on the fixed
order, never on how many candidates exist). A predator does not prefer Plant over Animal
prey or vice versa when both are in reach — whichever is found first in the scan wins.

**Decision: grazing kills the plant outright**, the same `charge_death` path S2 predation
already uses (world.rs:504-511) — one code path for "something died to feed something
else," regardless of kind. **Rejected alternative, named not silently dropped:** grazing
could instead do proportional hp damage and let the plant regrow, which would need a new
partial-damage-to-plants mechanism and a regrowth model; deferred, because lethal grazing
is already fully tunable through `satiation`/`forage_radius` without it, and one code
path is worth more than the extra realism right now.

`apply_attrition`/`apply_suppression` (`ecology.rs:152-183`) are **unchanged, on
purpose** — they already apply unconditionally to every alive entity with no kind check,
and this design keeps it that way: plants keep taking terrain-based ring/star damage
exactly like animals. A one-line comment at each site should say so explicitly, so a
future reader doesn't "helpfully" add a kind exemption that was never intended.

---

## 6. The animal FSM

**Decision: the drive is derived every tick, not stored.** New module `src/behavior.rs`:

```rust
pub enum Drive { Graze, Hunt, Flee }

/// Pure — same shape as `ecology::apply_attrition`, testable against a bare
/// `&[Entity]` + `&Terrain` with no `World` involved.
pub fn drive(entities: &[Entity], terrain: &Terrain, tuning: &BehaviorTuning, i: usize)
    -> (Drive, Option<V2>) { ... }
```

No new persistent `Entity` field for FSM state — recomputing from `(hunger, sensed
neighbourhood, terrain)` each tick keeps the hash surface minimal and avoids a coherence
problem between a stored state and a world that has since changed under it.

**Fixed priority: Flee > Hunt > Graze**, always.

- **Flee** reuses the *existing* attrition relation as a danger signal rather than only
  passive per-tick damage: read the body's own cell's concentration of
  `element.eaten_by()` (`Terrain::cell_of`/`cell`, the same helpers
  `apply_attrition`/`apply_suppression` already call). Above a `flee_threshold[race]`,
  steer *away* from the highest-concentration neighbour among a fixed visiting order
  (N, E, S, W), so ties break structurally rather than by iteration accident.
- **Hunt** gates on `hunger >= ecology.satiation[race]` — the *same* gate
  `phase_feeding` already uses — plus a prey body sensed within `sense_radius[race]`
  (a new, larger radius than `forage_radius`; sensing at a distance and catching within
  bite range are different things). Target selection: nearest by `len_sq()`, ties broken
  by lowest id — Invariant IV's ascending-id discipline, reused rather than reinvented.
- **Graze** is today's unmodified wander — no steering at all.

**Decision: steering is a bounded per-tick turn, not a snap to the target.**
`heading = (heading.scale(1000 - turn) + desired.scale(turn)).normalized()` via an
`Fx::ratio(_, 1000)` turn-rate knob, all existing `V2`/`Fx` operations. This is Invariant
I ("bounded propagation") applied to steering the same way diffusion caps apply it to
terrain. The exactly-opposite-heading degenerate case (turn = 500, `heading + desired`
can length-zero) needs an explicit branch, the same shape `initial_heading`'s own
zero-length guard already uses (entity.rs:115-119).

**Decision: snapshot-then-apply, mandatory, same reasoning `phase_feeding` already
documents.** Compute desired headings from an immutable pass over `entities`, apply in a
second mutable pass. The borrow checker forces this anyway, but the real reason is
determinism: without it, a body could steer toward a target that already moved earlier
in the same tick's mutable pass, making the result depend on iteration order rather than
only on `(seed, tick, ids)`.

Insertion point: the desired-heading nudge lands in `phase_movement`, *before* the
existing `step = e.heading.scale(races[race].speed)` computation (`world.rs:313`) — then
the existing step/jitter/reflect/clamp logic runs completely unchanged downstream of it.

New `Stats` counters: `grazed`, `hunted`, `fled` — the FSM's exit condition (§13, S3.7)
needs to observe that every drive actually fires under the shipped tables, not just that
the code compiles.

---

## 7. Plant propagation: `phase_flora`

**Decision: a new tick phase, not a seventh `phase_terrain` slot.** New order:

```
phase_commands → phase_aging → phase_movement → phase_collisions → phase_feeding
  → phase_flora → phase_settle → phase_terrain → phase_reap
```

gated on the same `(tick + 1) % TERRAIN_PERIOD == 0` boundary `phase_settle`/
`phase_terrain` already share. **Rejected alternative:** folding propagation into
`phase_terrain`'s existing six-slot sequence. Two problems, either one sufficient to
reject it: `phase_terrain` runs *after* `phase_settle`, so a newborn's `OnBirth` demand
would be deferred to the *next* terrain tick, contradicting the phase-order doc's own
stated rationale that a tick's events charge demand within that same tick; and it would
renumber a documented six-slot wire format (`docs/S1_TERRAIN_DESIGN.md` §2). A new phase
costs one line in `step` and avoids both.

New `PropagationTuning` in `ecology.rs` (reproduction is already ecology's concern via
`repro_threshold`), **`PerElement`-shaped, not `PerRace`-shaped** — only plant rows ever
read it, and the doc comment says so explicitly rather than shipping five permanently
dead `PerRace` rows:

| knob | type | meaning |
|---|---|---|
| `period` | `PerElement<u64>` | terrain ticks between propagation attempts (0 = never) |
| `chance` | `PerElement<u16>` | per-mille, per eligible plant, per attempt |
| `offspring_size` | `PerElement<u16>` | per-mille of full size at birth |
| `root_min` | `PerElement<u16>` | min terrain stock of the plant's **own** element required at the candidate cell |
| `dispersal` | `PerElement<Fx>` | max scatter offset from the parent (same shape as existing `BIRTH_SCATTER`) |
| `crowd_max` | `PerElement<u16>` | max same-race bodies already occupying the candidate cell |

`period` + `chance` together give coarse cadence and fine rate control, the same
floor/nominal-style two-knob shape `RateBand` already uses elsewhere in this codebase.

Algorithm, ascending-id over the (already id-sorted) `entities`, snapshot-then-apply
exactly like `phase_feeding`'s `births` vec (world.rs:485, 517-524) so a newborn cannot
propagate in the tick it was born and the scan never observes a growing vector:

1. Roll `chance` on a new `Channel::Propagate`, keyed by `self.tick` — the same
   per-tick keying convention `Channel::Hunt`/`Forage`/`MoveJitter` already use elsewhere
   in this codebase (none of them key by `terrain_tick`). Since `phase_flora` only ever
   runs when `(tick + 1)` is a multiple of `TERRAIN_PERIOD`, `self.tick` is already a
   bijective function of `terrain_tick` at this call site, so this is just a naming
   choice, not a determinism or behavioral difference.
2. Draw x/y scatter offsets on a new `Channel::Disperse`, `id.wrapping_add(K)` salt for
   the second axis — the same paired-draw convention `Channel::Forage`'s birth-scatter
   already establishes (world.rs:519-522).
3. Clamp the candidate position to bounds.
4. Check `root_min` via `Terrain::cell_of`/`cell` — the same helpers `apply_attrition`/
   `apply_suppression` already call, reused rather than reinvented.
5. Check `crowd_max` via a new `Occupancy::count(race, cell_index) -> u32` (a small public
   accessor on `Occupancy`'s currently-private per-race weight map).
6. Queue on success; spawn all queued offspring after the scan completes.

**Named runaway risk, with a shipped mitigation, not a silent gap.** `root_min` reads
the terrain concentration of the plant's *own* element — which the plant's own deposits
*increase*. That is a positive feedback loop: more plants → more of their element in the
ground → easier for more plants to root. `crowd_max` is the mitigation. This is the
direct S3 instance of the risk the README already names in its own words for a different
race ("a fast-breeding race has no natural population ceiling once splitting starts") —
S3.7's exit condition (§13) must actually observe this bounded under the shipped table,
not merely assume `crowd_max` is sufficient.

**New `Entity.size: u16`** (per-mille of full size), the first new *stored* field this
design introduces beyond `kind`. Set at birth: `offspring_size[element]` for a plant
offspring, `1000` for everything else (animals are born at full structural size in this
design — only plants grow into their footprint). Grown in `phase_aging` as a pure
function of `(age, lifespan, birth_size)` reaching `1000` at a `MATURITY_PERMILLE`
fraction of life — derived, not an independent knob, so a live-tuning session can't put
it out of sync with lifespan. **Read at exactly one place**: `phase_collisions`' radius
calculation (world.rs:371), so a seedling crowds less than a mature plant.

**Decision: size does not scale deposit/consume demand.** `terraform_pressure()`
(race.rs:270-272, `deposit_unit / lifespan`) is a per-race constant the live view reports
every frame; letting `size` scale it would silently move a race's §3.1 parity number as
its population's age structure shifts, with no knob visibly responsible for the drift.
Named here so a future contributor sees this was considered and rejected, not missed.

---

## 8. The hashing checklist

`src/hash.rs` is a 120-line hand-rolled FNV-1a hasher with **no derive macro and no
reflection anywhere in this codebase**. `World` itself does not implement `Hashable` —
`state_hash()` (world.rs:612-662) is a hand-curated, explicitly-ordered list of
`.hash_into()`/primitive-hash calls. Every line below is a manual edit that **compiles
clean and passes every existing test silently if skipped**:

| # | Site | Edit |
|---|---|---|
| 1 | `Entity::hash_into` (entity.rs:81-96) | `+ h.u8(self.kind as u8)`, `+ h.u16(self.size)` |
| 2 | `RaceAttrs::hash_into` (race.rs:446-460) | `+ h.u8(self.kind as u8)` |
| 3 | `EcologyTuning::hash_into` (ecology.rs:111-138) | `+ hunt_weight` loop |
| 4 | new `impl Hashable for PropagationTuning` | pattern-copy ecology.rs:111 |
| 5 | new `impl Hashable for BehaviorTuning` | pattern-copy ecology.rs:111 |
| 6 | `World::state_hash` (world.rs:629-652) | six `PerElement` loops widen to `PerRace` (mechanical) |
| 7 | `World::state_hash`, after world.rs:634 | **new** explicit calls: `self.propagation.hash_into(&mut h)`, `self.behavior.hash_into(&mut h)` |
| 8 | `World::state_hash` (world.rs:653-660) | `Stats` is hashed field-by-field by hand; every new counter (`grazed`/`hunted`/`fled`/`propagated`/`rooted_rejected`) needs its own line |
| 9 | `Terrain::hash_into` (terrain.rs:181) | unchanged — terrain stays 5-wide; leave a comment so nobody widens it by reflex |
| 10 | `Governor`/`Grant` (governor.rs:101-113) | unchanged — value types |

**This gap is documented, not fixed, because it cannot be mechanically fixed** in a
120-line hand-rolled hasher with no reflection — the same category of accepted,
documented limitation as S1's succession-test gap. The mitigation is targeted regression
tests, following the pattern S3.0 already established for `Entity` and `RaceAttrs`
(`hash_notices_every_field`, `entity.rs`/`race.rs`) before this document was written:
every struct in the table above needs its own such test, and `World` needs a
`state_hash_notices_a_retuned_X` test per new tuning struct (mirroring
`state_hash_notices_a_retuned_ecology`/`_races`, also already landed in S3.0).

---

## 9. Input log v2

`CmdKind::Spawn { element, at }` (input.rs:22) must carry a `Kind`. **Decision:** bump
`InputLog::VERSION` (input.rs:16) to 2, add a kind byte to the Spawn tag's payload
(input.rs:121-125, 150-159), add `LogError::BadKind(u8)` beside the existing
`BadElement`, and keep a v1 read path that decodes an old Spawn as `Kind::Animal` (every
race in the pre-S3 world was, in effect, mobile and predatory — the closest honest
default). State plainly, in the code comment and here: the compat path keeps old v1 logs
*readable*, not hash-reproducing. Replaying a v1 log against an S3 world will not
reproduce its originally recorded hashes, because the simulation itself changed — the
compat path is about not throwing away old recordings, not about pretending nothing
changed. `CmdKind::tag()` (input.rs:28-34) and its existing tag values must not be
renumbered, same discipline as `rand::Channel`.

---

## 10. The race table's narrative tests, re-decided

`race.rs`'s 12-test module breaks into two categories.

**Mechanical (loop `Race::ALL` instead of `Element::ALL`), no design decision required:**
`every_mix_sums_to_one_thousand`, `every_band_has_a_nonzero_floor`,
`a_rebalanced_mix_always_sums_to_one_thousand`, `a_band_edge_edit_never_inverts_the_band`,
`share_never_exceeds_the_unit`. `rebalancing_survives_being_driven_into_a_corner` needs no
change at all — it's table-independent. `every_race_is_internally_consistent` gains a
`race.element == r.element && race.kind == r.kind` check.

**Real per-Kind decisions, not copy-paste:**

- `lifespans_are_strictly_ordered_fire_to_earth` / `lifespans_span_the_intended_three_
  orders_of_magnitude` → the existing Fire→Water→Wood→Metal→Earth ordering and
  three-orders-of-magnitude spread are asserted **within each Kind independently**. New
  companion claim: `plants_outlive_the_animal_of_their_own_element` — long-lived-and-
  rooted vs. short-lived-and-mobile is close to the point of the split, so it should be a
  test, not a hope.
- `terraform_pressure_is_within_parity_band` → **stays one 2× band across all ten races**
  (Kind is not a license to break §3.1's tempo-parity rule). Per §3's consequence note,
  this means the shipped plant+animal rows must *split* today's per-element pressure
  budget, not each independently carry it. New companion test:
  `combined_per_element_pressure_stays_near_the_s2_baseline`. **If a 2× band across ten
  rows proves untunable once real numbers are chosen, the documented fallback is parity
  within each Kind plus a stated, deliberate inter-Kind ratio — written down as a
  decision at that point, not silently widened.**
- `channel_dominance_matches_the_stated_fantasy` → splits into
  `every_plant_is_existence_dominant` (rooted terraforming-by-being-there is close to the
  definition of "plant" in this design) and the existing per-element animal claims,
  **except Wood-Animal must move off `OnExistence`** (its current dominant channel,
  race.rs:551) onto `OnConsume` or `OnAction`, since Wood-Plant now owns the existence
  slot for that element.
- `fire_leaves_nothing_by_merely_existing` → narrows to
  `fire_and_metal_animals_leave_nothing_by_merely_existing`, plus a new complementary
  `no_plant_has_a_zero_existence_share`.

`seed_population` (world.rs:168-177) loops the full race set, so `seed_population(12)`
seeds 120 bodies once `Race::ALL` has 10 entries, not 60 — every `PER_RACE * 5` site
(`tests/determinism.rs`, `tests/ecology.rs`, `src/bin/{verify,soak,filmstrip}.rs`) needs
its multiplier updated to match.

---

## 11. The chaos TUI (`src/bin/chaos/`) — deferred, but must compile immediately

`cargo test` builds every target, so `src/bin/chaos/` must at minimum **compile** the
moment `Race`/`PerRace` land (S3.1) — a minimal mechanical fix (Kind hardcoded to
`Animal`, `Knob.get`/`set` retyped `Element → Race`, going through the existing
`knob!`/`edge_knob!`/`mix_knobs!`/`burst_knob!` macros) ships in S3.1 so the design work
below can be deferred to its own stage without blocking every other stage's
`cargo test`.

The design work itself, deferred to S3.6:

- `View.col` is currently an `Element` index (view.rs:70,100) driving the knob-page
  column cursor. **Decision: a Kind *toggle*, not ten columns.** `view.rs:362` already
  clamps per-column cell width to a readable 8-14 chars at 5 columns; ten columns on an
  80-column terminal would give 4-7 chars, unreadable. Keep 5 columns, add a Kind
  selector in the page header bound to a new key.
- Terrain/climate/propagation knobs are **Element**-scoped (a Wood-Plant and a
  Wood-Animal share one diffusion rate), while race/ecology/behavior knobs are
  **Race**-scoped. A `Page { title, knobs, axis: Axis::Race | Axis::Element }` field is
  needed so Element-scoped pages can say so in their header and ignore the Kind toggle,
  rather than silently implying two independent numbers that are actually the same cell.
- `Tuning` (knobs.rs:28-34) gains `propagation`/`behavior` fields, `Tuning::new`'s
  defaults, the `R`-key full reset, two new `PAGES` entries, and two new
  `w.retune_propagation(...)`/`w.retune_behavior(...)` lines in the per-frame push
  (main.rs:172-178) — all mechanical, following `retune_ecology`'s existing pattern.
- Hardcoded `5`s needing a second look: view.rs:123 (fixed layout height), view.rs:61 and
  main.rs:341 (5-element history vectors), view.rs:293-325 (`races()` row rendering),
  view.rs:373/386/403/418 (`Element::ALL` loops), main.rs:265-266 (Left/Right wrap),
  main.rs:309/331-337 (`R`/`z` handlers), main.rs:365-380 (`inputs()` restock, per-element
  → per-race). `write_table` (main.rs:409-530), the literal-source emitter, needs to
  produce ten `RaceAttrs` rows plus the two new tuning constants — mechanical but long.

**The core simulation stages (S3.1-S3.5, S3.7) are fully verifiable without any of this**
— `cargo test`, `cargo run --release --bin verify`, `--bin soak`, `--bin filmstrip` (which
renders terrain only, unaffected — `render_ppm` stays 5-element) all exercise the
simulation layer directly. The interactive TUI catching up is a separate, later
milestone, not a blocker.

---

## 12. Staged rollout

```
S3.0  hash net + this document                              ← lands standalone, no behavior change
  └─ S3.1  Kind axis, behavior-neutral; input log v2; chaos compile fix
       └─ S3.2  plants root & stop hunting; README "## S3" section lands
            ├─ S3.3  hunt weight ──> S3.4  animal FSM
            └─ S3.5  plant propagation; tick-order change        [parallel to S3.3/S3.4]
                 └─ S3.6  chaos two-axis TUI
                      └─ S3.7  exit condition + closing docs
```

- **S3.0** (landed alongside this document): `hash_notices_every_field` tests for
  `Entity` and `RaceAttrs` (neither had one before), a
  `state_hash_notices_a_retuned_races` test (races was the one `PerElement`-shaped
  `World` field with no "retuned" coverage — exactly the field S3.1 rekeys), and a
  rename of the misleadingly-named `state_hash_notices_every_field_it_covers` to
  `state_hash_changes_as_the_world_steps` (it never checked exhaustiveness; now it
  doesn't claim to). No simulation code changes; `cargo test` green, `state_hash`
  unchanged, `verify` still bit-identical over 10 000 ticks.
- **S3.1**: `Kind`/`Race`/`PerRace`; 10-row `RACES`; `Entity.kind`; `World`/`Occupancy`
  rekeyed to `PerRace`; the `apportion` salt fix (§3); input log v2 (§9); chaos-TUI
  compile fix (§11). Plant rows ship as literal copies of animal rows — an explicitly
  named scaffold state, still mobile and predatory, replaced next stage. Supersede
  `ecology.rs:13-19`'s "no plant/animal split" doc comment here, in the same
  flag-then-update style S1's ring/star note already models. Exit: `cargo test` green
  with updated population counts; `verify`/`soak` reproduce bit-identically.
- **S3.2**: real plant table numbers; `phase_feeding` pairing surgery (§5);
  `phase_movement` plant skip (§4); kind-aware `is_valid()`; the re-decided narrative
  tests (§10). New structural-invariant tests: a plant never appears as predator, a plant
  never moves. README `## S3` section lands here, in the changelog-blockquote style §2's
  S1/S2 sections already use.
- **S3.3**: `hunt_weight` knob, `Channel::Hunt`, the roll in `phase_feeding`.
- **S3.4**: `src/behavior.rs`, `BehaviorTuning`, snapshot-then-apply steering in
  `phase_movement`, `grazed`/`hunted`/`fled` counters. Tested standalone against bare
  entity slices, the `apply_attrition` pattern.
- **S3.5**: `PropagationTuning`, `Entity.size`, `phase_flora`, the tick-order doc updates
  this requires (`world.rs`, `lib.rs`, README, `docs/S1_TERRAIN_DESIGN.md` §2 noting the
  six terrain slots are unchanged and why propagation is a separate phase, not a
  seventh).
- **S3.6**: the chaos TUI's Kind toggle and everything §11 names.
- **S3.7**: `tests/layers.rs` — no mechanism is dead code (every Drive fires, at least
  one plant roots, under the shipped tables over a bounded run); the structural
  invariants hold every tick; 10 000 ticks replay bit-identically with every S3 phase
  active (the S0 exit condition, re-run). Re-run `tests/succession.rs`'s two `#[ignore]`d
  30-day tests and record the honest outcome, whichever direction it goes, in the same
  documented-not-patched spirit as their current failing status. Final doc sweep:
  `lib.rs`'s reading-order paragraph gains `behavior`; README's "Next" section shrinks to
  name only S4/S5.

---

## 13. Risks and open questions (assumptions, not given facts)

1. **Grazing kills the plant outright rather than damaging and regrowing it** (§5). The
   simpler of two real options; regrowth is deferred, not ruled out.
2. **The terraform-pressure parity band stays one 2× band across all ten races** (§10),
   requiring the shipped plant+animal rows to split rather than each carry today's
   per-element budget. Whether real numbers can actually satisfy this once chosen is an
   empirical S3.2 question this document cannot answer in advance.
3. **The `apportion` salt fix (§3) is required for correctness**, not an optional
   hardening pass — S3.1 must not ship without it, or two races sharing an element layer
   silently correlate their apportionment noise.
4. **Plant propagation's positive-feedback runaway risk** (§7) is named and given a
   shipped mitigation (`crowd_max`), but whether the shipped default actually bounds
   growth under the real ten-row table is an empirical S3.7 question, the same category
   as S2's own "the shipped `EcologyTuning` defaults do not keep the ring populated
   indefinitely" finding.
5. **`Entity.size` is read only at `phase_collisions`.** It deliberately does not scale
   deposit/consume demand (§7) or forage/sense radii — whether a mature plant should be
   easier to detect or harder to graze than a seedling is unaddressed here, flagged as
   plausible future work rather than designed.
6. **Animal-on-animal hunting reuses the herbivore edge (`Y = X.eats()`) rather than a
   second, independent relation.** This was a settled design fork going into this
   document (not reopened here), but its consequence for the shipped `hunt_weight`
   defaults — how "carnivorous" any given animal row actually ships — is not decided by
   this document and belongs to S3.3.
7. **v1 input logs decode as `Kind::Animal`** (§9) — a judgment call about the closest
   honest default for a world that predates the Kind axis, not a claim about what those
   bodies "really were."
8. **The chaos TUI's Kind-toggle design (§11)** is this document's own proposal, not
   validated against actually using it — it may need revision once S3.6 is underway and
   the two-axis interaction is felt at the keyboard rather than reasoned about on paper.

---

## Assumptions/open questions to confirm with the project owner before implementing

Ranked by how much they'd reshape later stages if the answer turns out to be "no":

1. **`PerRace<T>` as a new type, indexed `element.index()*2 + kind as usize`** (§1) is
   this document's single most consequential structural call — every later section
   depends on it. Confirm before S3.1, or specify the intended alternative.
2. **Grazing is lethal, one code path shared with S2 predation** (§5, §13.1). If partial
   damage + regrowth is actually wanted for plants, §5 and §7's propagation model need to
   be redesigned together, not patched independently.
3. **The terraform-pressure band stays unified across Kind** (§10, §13.2) rather than
   splitting into an explicit inter-Kind ratio from the start. If the ten-row table
   proves this untunable in practice, that fallback needs a real decision at that point.
