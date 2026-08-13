# Chaotic Nature — simulation core (Stage 0)

The determinism skeleton. Everything else in the game is downstream of this
crate being correct, so it is small, has zero dependencies, and is tested
harder than its size suggests.

**Exit condition, met:** 10 000 ticks replay bit-identically from
`(seed, input_log)`, asserted at *every* tick rather than only at the end.

## The loop

```
chaos                  live view — every knob is on the screen
chaos watch            same, rebuilding whenever src/ changes
chaos edit             open the tuning table in $EDITOR
chaos verify           the determinism exit condition
chaos soak [ticks]     long headless run + per-race report
chaos test             full suite (185 tests, 3 gated behind --ignored)
```

`chaos` lives in `~/.local/bin`; set `CHAOS_ROOT` to point it at a different
checkout. Flags set starting conditions only — `chaos --speed 600 --pop 120
--size 128` — because every race attribute is editable while it runs.

**Everything tunable is in `src/race.rs`.** One table, five rows. The live view
puts that same table on screen: rows are attributes, columns are races, and the
cursor keys move between cells.

```
↑↓←→ / hjkl   move the cursor        - +   adjust        [ ]   adjust ×10
space         pause                  .     advance one simulated minute
< >           halve / double sim speed
tab           knob page (body & rates ⇄ channel mix ⇄ terrain & climate ⇄
              ecology ⇄ propagation ⇄ behavior)
x             toggle Plant / Animal on the current page (Race-scoped pages only)
m             show or hide the map   w     how much the view steers bodies
r / R         reset this knob / the whole table
z             restart at tick 0 with the current knobs
T             write the current table to src/race.tuned.rs
q             quit
```

## Reading the live view

**The race table** is the instrument that means something at Stage 0.

```
race    alive  population        deposit: floor╵ nominal┊ granted▓ ceiling→ │  granted  state
Fire      53   ▇▇▇▇▇▇▇▇▇▇▇▇▇▇▇▇  │╵▓▓▓┊░░░░░░░░░░░░░░░░░░░░░│  1000/6000   rate-limited — 23.8k refused
```

- **granted** — how much terrain this race actually moved on the last terrain
  tick (one per 100 sim ticks, i.e. one simulated minute). It is the governor's
  answer, not the race's request.
- **ceiling** — the hard cap from that race's rate band. `granted / ceiling` is
  therefore "what it got / the most it could ever get in one tick".
- The bar draws the whole band to scale from zero to the ceiling: filled to
  `granted`, with `╵` at the floor and `┊` at nominal. Where the fill stops
  relative to those two marks is the whole governor story.
- **state** is what the `Grant` fields add up to:
  - *rate-limited* — `clipped > 0`. Demand was refused; the race is pushing a
    rate limit. At soak populations this is the normal state, and the amount
    refused is the interesting number.
  - *idle — floor is doing the work* — `forced > 0`. Demand fell short of the
    floor, so the world emitted the floor anyway.
  - *extinct — still churning at its floor* — nobody of this race is alive here
    and the terrain still turns over, which is what stops a lost biome becoming
    an absorbing state.
  - *inside its band* — demand was met in full out of banked burst budget.

**The pressure line** under the knob grid is the §3.1 parity metric,
`deposit_unit / lifespan`, recomputed live. Lifespan and deposit-unit edits are
the ones that break parity, and this says so the moment they do.

> S1 update: the map now paints the terrain grid's dominant-element colour as
> a background behind every body, blended the same way the filmstrip renderer
> blends a PPM pixel — so a scorched Fire patch or a Wood-flooded region reads
> at a glance, with bodies drawn on top. There is still no feeding or
> attraction behaviour driving movement itself — bodies wander and bump
> regardless of what's under them — so the map is a much better *readout* at
> S1 than it was at Stage 0, but it becomes the main *instrument* only at S2
> (plants and animals), once bodies actually respond to what they're standing
> on.
>
> S2 update: this prediction was half right. Feeding landed — bodies within
> reach of prey on the ring edge they eat consume it outright — but movement
> still does not respond to the *terrain* at all; wandering and bumping are
> unchanged, and predation is judged purely by entity-to-entity distance, not
> by anything a body is standing on. "Bodies attracted to what the map shows"
> remains unbuilt; what landed is bodies attracted to *each other*.

### What the live view is allowed to do that nothing else is

Retuning a running world is a deliberate break with the replay story: no
recorded trace can reproduce a table that changed underneath it. So the tuning
table lives on `World` rather than in a global — `verify` and `soak` run the
shipped table start to finish and still reproduce bit-identically, and the live
view's edits reach only its own world. The header reads `✎ retuned` while a
world has been touched; `z` restarts at tick 0 so the run is reproducible again.

`restock to` and the `w` wander setting are not simulation rules either. Stage 0
has no reproduction and no goals, so without a hand on the tiller the map empties
out and the survivors travel in straight lines. Both are submitted as ordinary
input commands — the same path a player's input takes.

## The invariants

| | | Where it lives |
|---|---|---|
| I | Bounded propagation — nothing acts instantly at a distance | *S1: terrain diffusion cap* |
| II | No floating point in simulation state | `fx.rs` |
| III | Randomness is stateless | `rand.rs` |
| IV | Iteration order is defined | `world.rs`, `element::PerElement` |
| V | Inputs replicate; state does not | `input.rs` |
| VI | Every tick is reproducible | `replay.rs` |
| VII | Bounded churn — rates are floored and capped | `governor.rs` |

Invariant I has no home yet because there is no field to diffuse. It arrives
with the terrain grid at S1, and the band-width arithmetic that depends on it
arrives at S3.

### Three things that are easy to break later

**Overflow behaviour must match across profiles.** Rust panics on overflow in
debug and wraps in release — two different simulations from one source tree.
`Cargo.toml` sets `overflow-checks = true` in every profile. Do not remove it.

**The RNG has no state.** `rand(seed, tick, entity, channel)` is a pure hash.
Two territories simulating the same entity inside a shared band draw the same
number without exchanging a byte, which a sequential generator cannot do. The
`channel` argument keeps independent decisions independent, so adding a random
choice to foraging never shifts the stream feeding collision — and every replay
recorded before the change stays valid. Never renumber an existing channel.

**The tick phase order is a wire format.** `commands → aging → movement →
collisions → feeding → flora → settle → terrain → reap`. Reordering any two
phases changes results and invalidates every recorded trace.

## The rate model

`deposit_unit` is what one body writes to the terrain over its **entire life**.
Each channel's per-mille share is spread across however many times that channel
actually fires in a life — birth and death once, existence once per terrain
tick, actions and meals at their own cadence. That makes the §3.1 parity rule
something the code computes rather than something the table hopes for:

```
race    lifespan            pressure   dominant channel
Fire        800 ticks  (8m)     2625   death        — terraforms by dying
Water      3500 ticks  (35m)    2542   action       — terraforms by flowing
Wood      15000 ticks  (2.5h)   2533   existence
Metal     72000 ticks  (12h)    2527   consume      — only at the forge
Earth   2016000 ticks  (14d)    2529   existence    — terraforms by staying
```

Lifespans span 2520×. Terraforming pressure spans 1.04×. That is the tempo axis
working: wildly different rhythms, near-identical total effect on the map.

### The governor

Every race's deposition and consumption passes through a rate band, per terrain
tick, per territory. It guarantees three things regardless of player behaviour:

- **Never below the floor** — emitted even at zero demand, even if the race is
  extinct here. An emptied server keeps turning over, which is what stops a lost
  biome from becoming an absorbing state.
- **Never above the ceiling** — no amount of coordination or exploitation moves
  more terrain in one tick than this.
- **Long-run average converges to nominal** — bursting spends a bucket that only
  refills at nominal, so maximum effort and minimum effort differ in *timing*,
  not in total.

Together these bound the terrain state at `T + k` before any player has decided
anything, which is what forecastable terrain actually requires.

`Grant` reports `granted`, `forced` (emitted only to honour the floor — the race
is idle or absent) and `clipped` (demand refused — somebody is pushing a rate
limit). All three drive the gauge and the state column in the live view.

## Known and deliberate

- **Collision is O(n²).** Correct and fast enough here (~6 300 ticks/s at 700
  entities, ~3 800× real time). A uniform-grid broadphase is still unbuilt;
  `Terrain`'s row-major `(x, y) → index` scheme (S1) is exactly the bucket
  indexing it would want, and must iterate cells in index order to stay
  deterministic.
- **Demand vastly exceeds the bands at soak populations.** Earth with 294 bodies
  clips ~313 000 units per settlement against a ceiling of 1 400. The governor is
  doing its job, but it means that above a population threshold a race's marginal
  body changes the terrain by nothing at all. That caps the value of zerging a
  region — probably desirable — but it also decouples population from terrain
  influence, which is a real design question for S1, not a bug.
- **`speed` is an ecology knob, not a feel knob.** Mobility is the parameter
  that decides whether five biomes coexist in rotating spiral domains or
  collapse to a single survivor — there is a critical threshold and it is not
  where intuition puts it. Change it expecting the world to reorganise. It is
  the second row of the live view for that reason.
- **No combat, no artifacts.** S5. Feeding, starvation and reproduction
  landed at S2 (below); combat and artifacts still have no home.
- **Every race needs a "way of life," and every way of life needs a stable
  city.** Earth already clumps into something settlement-like under nothing
  but the existing subsystems (see "Demand vastly exceeds the bands," above)
  — that's a hint, not a feature, and it must not stay Earth-exclusive or
  wired through one subsystem by accident. Whatever "way of life" design
  eventually lands for each race, at least one naturally stable, city-like
  configuration has to be *possible* under it — not necessarily the only way
  to live, not necessarily what a player is steered toward, but reachable
  and, ideally, rewarded by whatever gameplay loops get built on top. A
  guiding constraint on the open S3/S4 gap below, not a design yet.

## S1 — the terrain field

Landed: five `u16` saturations per cell (`terrain.rs`), six operator slots in a
fixed order every terrain tick — deposit, consume, attrition, suppression,
climate, diffusion (see the S2 update note below: slots 3–4 shipped as
terrain's own `ring`/`star` originally, and moved to `ecology.rs` post-S2) —
gated at the terrain-tick boundary within `World::step`'s phase
order (`commands → aging → movement → collisions → feeding → flora → settle →
terrain → reap`; `feeding` is S2's addition and `flora` is S3.5's, both
below) — and a deterministic
climate influx map with a five-season, 30-simulated-day cycle (`climate.rs`).
Every design choice the README's one-sentence spec left open — the deposit/consume element
mapping, the diffusion boundary, the operator order's justification, the exit
condition made numeric — is written up in `docs/S1_TERRAIN_DESIGN.md`. The
exit condition ships as `tests/succession.rs`, run via
`cargo test --release -- --ignored`.

Also landed: headless filmstrip export (`src/bin/filmstrip.rs`, PPM frames +
`manifest.txt`, run via `cargo run --release --bin filmstrip -- [seed] [size]
[per_race] [ticks] [cadence] [outdir]`) and hot-reload through the `chaos`
live view — a third knob page (`terrain & climate`, diffuse rate/cap plus
climate's base range/floor/season peak/season length), pushed to a running
world every frame via `World::retune_terrain`/`retune_climate`, and `map()`
now paints the terrain grid's blended colour as a background behind bodies
instead of drawing bodies only.

Not yet landed: the uniform-grid collision broadphase noted above — `Terrain`'s
row-major indexing is ready for it, but nothing consumes it yet.

> **Post-S2 update: terrain isn't its own actor.** `ring` and `star` — operators 3
> and 4 above — ran every terrain tick with no entity involved at all:
> terrain converting a permille of its own stock into the next ring element,
> and terrain nullifying a permille of its own stock against its
> suppressor's, both purely a function of `TerrainTuning` against the grid.
> That's gone. Terrain should only change because of what bodies do, and
> deposit/consume already worked that way — ring/star didn't. The two
> *relations* survive, redirected onto whatever body is standing on the cell
> instead of onto the cell itself: see S2's own update note below for
> `attrition`/`suppression`. `chaos`'s "terrain & climate" knob page lost the
> `ring rate`/`star rate` knobs as a result — two knobs, not four, remain
> (`diffuse rate`, `diffuse cap`). One concrete, measured consequence:
> `tests/succession.rs`'s two 30-day exit-condition tests now fail (Wood
> saturates to the grid maximum within the first climate season and stays
> there, tripping the "no frozen extreme" check) — star was quietly
> load-bearing for that guarantee. Documented, not silently patched over, in
> `docs/S1_TERRAIN_DESIGN.md` §7; the fix is retuning `ClimateTuning` via the
> live view, not reintroducing star.

## S2 — feeding, starvation and reproduction

Landed: a body within reach of prey on the ring edge it eats
(`Element::eats`, `ecology.rs`, `World::phase_feeding`) consumes it outright.
The prey dies exactly as it would from old age; the predator's `hp` rises and
fires the `OnConsume` channel, which every race's deposit/consume mix has
carried a nonzero share for since Stage 0 with nothing to fire it until now.
A meal that carries a body's `hp` up across a threshold spawns one offspring
through the ordinary `World::spawn` path, so it charges `OnBirth` the same
way a command-spawned or seeded body always has. A body that goes too long
without a meal starves — `hp` drains after a grace period and death follows
the same path as old age. `EcologyTuning` (`src/ecology.rs`) holds the six
rate knobs — `forage_radius`, `satiation`, `feed_gain`, `starve_after`,
`starve_rate`, `repro_threshold` — retunable live the same way
`TerrainTuning` and `ClimateTuning` are, and exposed as a fourth `chaos`
knob page (`ecology (S2)`).

This reads the README's "plants and animals" as *the ecology layer* rather
than a literal two-tier taxonomy — nothing in the shipped table distinguishes
a "plant" race from an "animal" one, and the predation ring
(`Element::eats`) is a full five-way pentagon: every race eats exactly one
other and is eaten by exactly one other. That is this design's interpretive
call, flagged rather than silently assumed, the same way S1's deposit/consume
element mapping was.

**The shipped `EcologyTuning` defaults do not keep the ring populated
indefinitely.** A uniform five-race closed predation loop is a hard balance
problem — empirically, the shipped numbers collapse to zero within a few
thousand ticks starting from `seed_population`, the same way nothing ever
promised S1's shipped `TerrainTuning` produced good-looking succession
without a live tuning pass. `tests/ecology.rs` pins down the mechanism (a
meal fires `OnConsume`, kills the prey, can trigger a birth; starvation kills
after its grace period; a retuned ecology changes the run; none of it breaks
bit-identical replay) rather than asserting the shipped balance holds —
finding numbers that hold a population is exactly what `chaos`'s live view
exists for, not something this document can settle from a batch run.

Not yet landed: no per-race asymmetry in `EcologyTuning` itself — every field
ships as a single uniform value across all five races (`PerElement::filled`),
even though `RaceAttrs` varies wildly per race (Fire's 800-tick life against
Earth's 2 016 000). A short-lived race almost certainly needs a faster
satiation/reproduction cadence than a long-lived one to hold its own in the
ring; the live view is where that gets found.

> **Post-S2 update: attrition and suppression.** Two more `EcologyTuning`
> knobs, gated at the terrain-tick cadence like the terrain operators rather
> than every tick like feeding/starvation: `attrition_rate` deals hp damage
> to a body proportional to the terrain concentration of `element.eaten_by()`
> at its cell (the ring relation, read as environmental predation pressure —
> a Wood body standing in a Fire-soaked cell burns); `suppression_rate` adds
> directly to `hunger` proportional to the terrain concentration of
> `element.suppressed_by()` (the star relation, read as degraded foraging
> ability — a Fire body struggles to feed with enough Water around). Both
> read terrain, neither writes it — they're `ecology.rs` functions
> (`apply_attrition`/`apply_suppression`), not `terrain.rs` ones, precisely
> because they act on bodies, not on the ground. See the S1 section's update
> note above for what they replaced and why.

## S3 — ecology layers (the plant/animal Kind split)

Landed: a new `Kind` axis (`Kind::Plant | Kind::Animal`, `src/race.rs`) sits
orthogonal to `Element` — every element now splits into a Plant and an Animal
race variant, ten races total via `Race { element, kind }`, not a behavioural
relabelling of the old five and not a re-topology of the ring (`element.rs`'s
mod-5 arithmetic stays completely `Kind`-unaware, by design). A new
`PerRace<T>` type (mirroring `PerElement<T>` member for member) replaces
every per-race `World`/`Occupancy` table — `races`, the deposit/consume
governors and demand accumulators, `last_deposit`/`last_consume` — so a
Plant's and an Animal's demand settle through independent governors, never
clipping each other. `Entity` gained a `kind` field alongside its existing
`element`; predation/suppression/attrition relations still resolve off
`element` alone, race-attribute lookups resolve off the full `(element,
kind)` pair. `docs/S3_ECOLOGY_LAYERS_DESIGN.md` writes up every call this
required — the `PerRace<T>` type decision, the `terrain::Occupancy`
apportionment-salt fix a two-race-per-layer world needed (§3), the input log
v2 bump that lets `CmdKind::Spawn` carry a `Kind` (§9) — the same grounded,
decision-and-assumption method `docs/S1_TERRAIN_DESIGN.md` used for S1.

The ten-row table (`RACES` in `src/race.rs`) ships real, deliberately
designed numbers, not scaffold copies: every Plant row is rooted
(`speed: Fx::ZERO`), lives exactly 3x as long as its Animal twin, and is
existence-dominant in its deposit mix (it terraforms by merely persisting);
every Animal's `deposit_unit`/`consume_unit` is exactly half its pre-`Kind`
value and every Plant's is exactly 1.5x that same old value, so a Plant+Animal
pair's *combined* terraform pressure lands within about 1 part in 250 of the
single-race baseline that element carried before the split — the tempo
budget is split between the two kinds, not doubled. All ten rows still
cluster inside the pre-existing 2x parity band. `RaceAttrs::is_valid()`
enforces this shape directly: `Kind::Animal` rows must have positive speed,
`Kind::Plant` rows must have exactly zero speed, and every row (either kind)
must have a positive collision radius.

"Plant" means something mechanically, not just a label. `World::phase_movement`
skips `Kind::Plant` entirely, before step/jitter/reflect/clamp ever run — a
structural `continue`, not just a zero-speed number nothing reads, because the
per-tick jitter term alone would still random-walk a zero-speed body if
nothing else stopped it. `phase_collisions` is *not* skipped — a thicket
still occupies space and crowds neighbours through its radius exactly as
before. And `World::phase_feeding`'s predator/prey pairing now refuses a
Plant as predator (`entities[pred].kind != Kind::Animal` short-circuits the
match) — a Plant can be eaten, never eat. `apply_attrition`/`apply_suppression`
(`ecology.rs`) are deliberately unchanged: plants still take terrain-based
ring/star damage exactly like animals, on purpose.

`EcologyTuning::hunt_weight` (`PerRace<u16>`) now gates the Animal-vs-Animal
edge of `World::phase_feeding` too, via a new `Channel::Hunt` roll evaluated
once per predator per tick (not per prey pair, so the outcome agrees with
itself across every prey candidate a given predator is tested against in the
same tick). Grazing (Animal-vs-Plant) remains fully unconditional — the roll
only ever gates the Animal-prey edge. The shipped default is a uniform,
deliberately near-zero 150‰ across every Animal row (Plant rows are unread
and stay zero); real per-race differentiation is left to a future
live-tuning pass.

Every Animal body now has steering intelligence (`src/behavior.rs`, S3.4): a
`Drive` — `Graze`, `Hunt`, or `Flee` — is recomputed fresh every tick from
`(hunger, sensed neighbourhood, terrain)` rather than stored on `Entity`, in
fixed priority, always Flee > Hunt > Graze. Flee reuses
`ecology::apply_attrition`'s own danger signal — the terrain concentration of
`element.eaten_by()` at the body's cell — and steers away from whichever of
its four grid neighbours holds the most of it once `BehaviorTuning::
flee_threshold` is crossed. Hunt gates on the same `hunger >=
ecology.satiation[element]` test `World::phase_feeding` already uses, plus a
prey element sensed within a new, deliberately larger
`BehaviorTuning::sense_radius` (sensing at a distance and catching within
`EcologyTuning::forage_radius`'s bite range are different things); sensing
does not distinguish prey `Kind`, so whether a caught body ends up grazed or
hunt-weight-gated stays entirely `phase_feeding`'s downstream decision. Graze
is the default — no danger above threshold, and either not hungry or no prey
sensed — and steers nowhere, today's unmodified wander. Steering itself
(`behavior::steer`) is a bounded per-tick turn toward the desired heading via
`BehaviorTuning::turn_rate`, never a snap, the same bounded-propagation
discipline diffusion caps apply to terrain. `World::phase_movement` derives
every Animal's drive from an immutable snapshot of this tick's pre-movement
positions before any body in the same phase has moved, so steering never
depends on iteration order; three new `Stats` counters (`grazed`, `hunted`,
`fled`) track which drive fired.

Plants now reproduce (`World::phase_flora`, S3.5): a new tick phase, run
right after `phase_feeding` and before `phase_settle` — `commands → aging →
movement → collisions → feeding → flora → settle → terrain → reap` — not a
seventh `phase_terrain` slot, for two reasons: `phase_terrain` runs *after*
`phase_settle`, so a newborn's `OnBirth` demand would otherwise be deferred
to the next terrain tick, and folding it in would renumber
`docs/S1_TERRAIN_DESIGN.md`'s documented six-slot wire format. Gated at the
same terrain-tick boundary `phase_settle`/`phase_terrain` share, it rolls
each living Plant's chance to propagate (a new `PropagationTuning` table in
`ecology.rs`, `PerElement`-shaped since only Plant rows ever read it: period
between attempts, per-attempt chance, offspring size, the two rooting gates
below, and dispersal reach), scatters a candidate cell near the parent, and
roots a new offspring there if two gates both pass: `root_min` (the
candidate cell must already hold enough of the plant's own element in the
terrain) and `crowd_max` (the candidate cell must not already be crowded with
same-race bodies, checked via a new `Occupancy::count`). `root_min` alone is
a named runaway risk, not a silent gap: a plant's own deposits raise the
terrain concentration of its own element, which makes rooting easier, which
allows more plants — `crowd_max` is the shipped mitigation. Two new `Stats`
counters, `propagated` and `rooted_rejected`, track successful roots and
gate-failed attempts respectively, so a live view can watch whether
`crowd_max` is actually doing anything under the shipped table. A rooted
offspring starts at `Entity.size` less than full — the first new *stored*
field this design adds beyond `kind` — and grows into its mature collision
footprint over time; `size` is never accumulated, only recomputed from
scratch every tick in `phase_aging` as a pure function of `(birth_size, age,
lifespan)`, so a live retune of `offspring_size` or the maturity fraction
takes effect immediately, and it is read at exactly one place —
`phase_collisions`' radius calculation — so a seedling crowds less than a
mature body without touching deposit/consume demand at all.

Also landed: the `chaos` live view's two-axis (Element × Kind) support
(S3.6) — a `Kind` toggle (`x`) on the current knob page, driving a real
`view.kind` field rather than a hardcoded `Kind::Animal`, ten-wide
`PerRace`-shaped history and race rows, and new `propagation`/`behavior`
knob pages. Terrain/climate/propagation knobs stay Element-scoped — a
Wood-Plant and a Wood-Animal share one diffusion rate, one root-min stock —
while race/ecology/behavior knobs are Race-scoped; each page's own `Axis`
says which, so the Kind toggle doesn't imply two independent numbers where
there is really only one.

> **Windowed client update:** `chaos-ui` (`src/bin/chaos-ui.rs`, `eframe`/`egui`)
> is a second client alongside the terminal `chaos` live view, driving the exact
> same tuning table — now `src/tuning.rs`, in the library, so neither client
> copies it and drifts. It adds a real 2D map: every body drawn at its actual
> position, terrain coloured by what has soaked into each cell, with
> per-race/per-plant/per-terrain-element visibility toggles and a map-wide
> terrain concentration summary. It is a spectator/tuning surface only — no
> player-embodiment layer, nothing here steers or claims a body.

> **S3.7 exit condition.** `tests/layers.rs` closes S3 the way
> `tests/succession.rs` closed S1: under the shipped table, no S3 mechanism
> is dead code (Graze, Hunt, and Flee all fire, and at least one Plant roots
> — each proven in its own scenario, terrain pre-seeded where an ordinary
> run would not build up the right conditions in time; see the file's own
> header for why), a rooted Plant never self-propels across a long run
> (`phase_collisions` pushing it is not a violation — the design's own
> claim, distinguished from real drift), the named `crowd_max` runaway-risk
> mitigation actually bounds growth once growth has a real chance to happen
> (`#[ignore]`d — population climbs into the thousands, expensive under
> today's O(n²) collision check), and 10 000 ticks still replay bit-for-bit
> identically with every S3 phase wired into the tick. A real, unplanned
> finding surfaced along the way: `phase_aging`'s hunger/starvation
> mechanic (S2, predates the Kind split) has no `Kind` exemption, and a
> Plant can structurally never eat — so every Plant is *guaranteed* to
> starve roughly 2 000 ticks after birth under the shipped
> `EcologyTuning`, regardless of terrain or predation. Documented as a
> permanent regression (`every_plant_starves_under_the_shipped_table_
> because_it_can_never_eat`), not silently patched. Re-running
> `tests/succession.rs`'s two 30-day tests: **both still fail**, on the
> identical signature already recorded in the Post-S2 update above (Wood
> saturates to the grid's maximum within the first season and stays there)
> — S3 neither fixes nor worsens it; the failure predates the Kind split
> and is unrelated to it.

## Next

Nothing past S3 has a design yet. The "Known and deliberate" section above
names S5 (combat, artifacts) as the next *named* milestone; S4 is the open
gap now — including the Plant-starvation finding just above, which stands
in the way of any race's Plant variant being a real, livable way of life
rather than a guaranteed ~2 000-tick countdown.

One constraint on whatever fills that gap, from "Known and deliberate" above:
every race's eventual way of life must have a reachable, naturally stable
city-like structure. Earth already backs into one by accident; that has to
become a real, intentional possibility for every race, not an Earth-only
curiosity.

The concrete problem that motivates this, worked through in conversation but
not designed: a fast-breeding race (Fire, an 8-minute lifespan) has no
natural population ceiling once it starts splitting — nothing but
starvation, predation, and terrain (`attrition`/`suppression`, above) stands
between one Fire body and exponential burnout. A Fire "way of life" has to
lean on other races and the map to bound its own growth while keeping a
player playable, which raises real, unresolved questions rather than
answers: does a split hand control to an NPC, a new player, or something in
between? Is there a "right to continuity" a lineage competes for, where a
longer lineage is more stable and better rewarded? None of this is decided —
it's flagged here so it shapes whatever S4 design gets written, the same
way S1's, S2's, and S3's own interpretive calls were flagged before they
were resolved.
