//! The simulation.
//!
//! The tick order below is part of the specification, not an implementation
//! detail. Reordering any two phases changes results and therefore invalidates
//! every recorded replay — treat it the way you would treat a wire format.
//!
//! `commands → aging → movement → collisions → feeding → flora → settle →
//! terrain → reap` — `feeding` (S2, `phase_feeding`) runs after collisions so
//! predator and prey are compared at this tick's settled positions, and
//! before settle so a kill's `OnDeath` demand and a meal's `OnConsume`
//! demand both land in the same terrain tick's settlement as everything else
//! that happened this tick. `flora` (S3.5, `phase_flora`) runs right after
//! feeding for the same reason — a rooted seedling's `OnBirth` demand must
//! land in this same terrain tick's settlement, not the next one — and
//! before settle/terrain so it never grows `phase_terrain`'s own fixed
//! six-slot sequence into a seventh. `terrain` (S1) runs the six fixed-order
//! operators described in `docs/S1_TERRAIN_DESIGN.md` and `terrain.rs`'s own
//! doc comment.

use crate::behavior::{BehaviorTuning, Drive};
use crate::ecology::{EcologyTuning, PropagationTuning};
use crate::element::{Element, PerElement};
use crate::entity::{Entity, Item, ACTION_THRESHOLD, MAX_HP};
use crate::fx::{Fx, V2};
use crate::governor::{Governor, Grant};
use crate::hash::{Hashable, Hasher};
use crate::input::{CmdKind, Command, InputLog};
use crate::race::{attrs, Channel as DepChannel, Kind, PerRace, Race, RaceAttrs, MILLI, RACES, TERRAIN_PERIOD};
use crate::rand::{rand_chance, rand_signed, Channel};
use crate::terrain::{Occupancy, Terrain, TerrainTuning};

/// Per-tick positional noise, so entities do not travel on perfect rails.
pub const JITTER: Fx = Fx::ratio(1, 400);

/// Positional spread for an offspring spawned by `phase_feeding`, so a
/// cohort born from the same parent does not stack exactly on top of it.
pub const BIRTH_SCATTER: Fx = Fx::ratio(150, 100);

/// Items/inventory (Invariant VIII extension): `Smelt`'s fixed conversion
/// ratio, `X` → `X.generates()` — the project owner's own worked example,
/// shipped as-is rather than derived: 50 carried units of `X` in, 1 unit of
/// `X.generates()` out, the remaining 49 returned to terrain at the smelting
/// body's position as tailings (`World::smelt`). Unlike `race::Conversion`
/// (one ratio per race, live-tunable), this is one ratio for every race and
/// every element — smelting is the same physical process no matter who's
/// doing it — so it is a plain constant, not a `RaceAttrs` field or a
/// `tuning.rs` knob.
pub const SMELT_RATIO_IN: u64 = 50;
pub const SMELT_RATIO_OUT: u64 = 1;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Stats {
    pub births: u64,
    pub deaths: u64,
    pub collisions: u64,
    pub actions: u64,
    /// Total demand refused by the (sole, post-Invariant-VIII) conversion
    /// governor. A rising value means somebody is pushing on a rate limit.
    /// Named `deposit_clipped` for historical continuity with the
    /// pre-Invariant-VIII two-governor model; it now tracks the one
    /// governor that gates the coupled conversion's habitat draw.
    pub deposit_clipped: u64,
    /// S2: successful predation events, counted at the predator.
    pub feedings: u64,
    /// S2: deaths where hunger had already crossed `starve_after` — a subset
    /// of `deaths`, not an addition to it.
    pub starved: u64,
    /// S3.4: an Animal's FSM drive this tick was Graze — no danger sensed
    /// above `flee_threshold`, and either not hungry or no prey sensed
    /// within `sense_radius`.
    pub grazed: u64,
    /// S3.4: an Animal's FSM drive this tick was Hunt — hungry, with prey
    /// (either Kind) sensed within `sense_radius`, steering toward it.
    /// Whether the catch itself succeeds is `World::phase_feeding`'s
    /// separate satiation/reach/hunt-weight gating.
    pub hunted: u64,
    /// S3.4: an Animal's FSM drive this tick was Flee — the body's own cell
    /// held enough of what eats it (`element.eaten_by()`) to cross
    /// `flee_threshold`, steering away from the worst neighbouring cell.
    pub fled: u64,
    /// S3.5: successful plant propagation events (a new offspring actually
    /// rooted) -- a subset of `births`, not an addition to it.
    pub propagated: u64,
    /// S3.5: propagation attempts that passed the chance roll but then
    /// failed either the `root_min` or `crowd_max` gate. A rising value
    /// confirms `crowd_max` is actually doing something under the shipped
    /// table, not merely assumed to -- see section 7's named runaway-risk
    /// note (docs/S3_ECOLOGY_LAYERS_DESIGN.md).
    pub rooted_rejected: u64,
}

#[derive(Clone, Debug)]
pub struct World {
    pub seed: u64,
    pub tick: u64,
    /// Always sorted by ascending `id` — Invariant IV. Every phase that
    /// touches this vector must preserve that ordering.
    pub entities: Vec<Entity>,
    pub next_id: u32,
    /// The simulated square is `[0, size] × [0, size]` in cells.
    pub size: Fx,

    /// The tuning table this world is running, seeded from [`RACES`] and
    /// changeable at runtime through [`World::retune`]. It lives here rather
    /// than in a global so that the live view can turn a knob without any
    /// other world — a soak, a verification replay — seeing it.
    ///
    /// It is covered by [`World::state_hash`], so a retuned world never
    /// compares equal to an untuned one.
    pub races: PerRace<RaceAttrs>,

    /// S1: the terrain field and the tuning tables its operators read.
    /// Covered by [`World::state_hash`] the same way `races` is — a
    /// retuned world must not hash the same as an untuned one.
    pub terrain: Terrain,
    pub terrain_tuning: TerrainTuning,

    /// S2: feeding/starvation/reproduction rates. Covered by
    /// [`World::state_hash`] the same way `terrain_tuning` is.
    pub ecology: EcologyTuning,

    /// S3.4: the animal FSM's rate/reach knobs. Covered by
    /// [`World::state_hash`] the same way `ecology` is.
    pub behavior: BehaviorTuning,

    /// S3.5: plant-reproduction rate/reach knobs (`World::phase_flora`).
    /// Covered by [`World::state_hash`] the same way `ecology`/`behavior` are.
    pub propagation: PropagationTuning,

    /// Invariant VIII: the sole governor left. Pre-Invariant-VIII this
    /// gated one of two independent flows (deposit); it now gates the one
    /// coupled conversion's habitat draw — see `race::Conversion` and
    /// `terrain::apply_conversion`. Deposit's own governor/RateBand/demand
    /// are retired entirely: deposition is now a fully derived,
    /// same-tick consequence of this draw, not a second rate-limited
    /// process (its bound is inherited transitively, since the produced
    /// amount can never exceed the draw by construction).
    consume_gov: PerRace<Governor>,
    /// Accumulated in milli-units between terrain ticks.
    consume_demand: PerRace<u64>,

    pub last_consume: PerRace<Grant>,
    pub stats: Stats,
}

impl World {
    pub fn new(seed: u64, size_cells: i32) -> World {
        // `size` is an `Fx`, which saturates past `i32::MAX >> Fx::SHIFT`
        // cells — clamping here, once, before it reaches `Fx::from_int` or
        // `Terrain::new`, keeps both in agreement. Leaving `Terrain` free to
        // construct at a raw, unclamped size that `Fx` would have silently
        // shrunk would desync the terrain grid from the entity coordinate
        // space the design's 1:1 resolution decision depends on.
        let size_cells = size_cells.clamp(1, i32::MAX >> crate::fx::SHIFT);
        World {
            seed,
            tick: 0,
            entities: Vec::new(),
            next_id: 1,
            size: Fx::from_int(size_cells),
            races: RACES,
            terrain: Terrain::new(size_cells),
            terrain_tuning: TerrainTuning::default(),
            ecology: EcologyTuning::default(),
            behavior: BehaviorTuning::default(),
            propagation: PropagationTuning::default(),
            consume_gov: PerRace(Race::ALL.map(|r| Governor::new(attrs(r).consume))),
            consume_demand: PerRace::filled(0),
            last_consume: PerRace::default(),
            stats: Stats::default(),
        }
    }

    /// Swap the tuning table on a running world.
    ///
    /// Rate bands reach their governors immediately; banked burst budget
    /// carries over, clamped to whatever the new band allows. Lifespan is the
    /// one knob that does *not* reach back — every body already alive keeps the
    /// span it rolled at birth, so lowering it thins the population by
    /// attrition rather than by mass execution.
    pub fn retune(&mut self, mut races: PerRace<RaceAttrs>) {
        // Bug 4 (Invariant VIII audit): a bare field replacement, unlike the
        // tuning-knob path (`tuning.rs`'s `Conversion::set_share_rebalanced`
        // knobs), does nothing on its own to keep `deposit_share +
        // body_share + waste_share == 1000` — and a `Conversion` that
        // breaks that sum makes `terrain::apply_conversion`'s `waste_amt`
        // remainder subtraction underflow and panic (`overflow-checks =
        // true` in every profile). This is the second of the two
        // enforcement points the fix adds — real, not a `debug_assert` that
        // compiles out in release — so a live retune that somehow carries
        // an invalid `Conversion` (through a path other than the knob
        // table) is corrected here rather than accepted as-is.
        for r in Race::ALL {
            races[r].conversion.clamp_shares();
        }
        self.races = races;
        for r in Race::ALL {
            self.consume_gov.get_mut(r).set_band(races[r].consume);
        }
    }

    /// Swap the terrain operators' tuning table on a running world. A
    /// straight field replacement — unlike the race governors, none of the
    /// six operators carry internal state that needs reconciling.
    pub fn retune_terrain(&mut self, terrain_tuning: TerrainTuning) {
        self.terrain_tuning = terrain_tuning;
    }

    /// Swap the ecology tuning table. A straight field replacement — feeding
    /// and starvation carry no governor-style internal state to reconcile.
    pub fn retune_ecology(&mut self, ecology: EcologyTuning) {
        self.ecology = ecology;
    }

    /// Swap the behavior tuning table. A straight field replacement, same as
    /// `retune_ecology` — the FSM carries no internal state to reconcile,
    /// only read fresh every tick.
    pub fn retune_behavior(&mut self, behavior: BehaviorTuning) {
        self.behavior = behavior;
    }

    /// Swap the propagation tuning table. A straight field replacement, same
    /// as `retune_ecology`/`retune_behavior`.
    pub fn retune_propagation(&mut self, propagation: PropagationTuning) {
        self.propagation = propagation;
    }

    /// A deterministic starting population, spread evenly across the ten
    /// races and placed by hash rather than by sequence.
    pub fn seed_population(&mut self, per_race: u32) {
        for r in Race::ALL {
            for k in 0..per_race {
                let salt = (r.index() as u32) * 7919 + k;
                let x = self.scatter(salt, 0);
                let y = self.scatter(salt, 1);
                self.spawn(r, V2::new(x, y));
            }
        }
    }

    fn scatter(&self, salt: u32, axis: u32) -> Fx {
        let r = crate::rand::rand_below(
            self.seed,
            u64::from(axis),
            salt,
            Channel::SpawnPlacement,
            (self.size.floor_int().max(1)) as u32,
        );
        Fx::from_int(r as i32)
    }

    pub fn spawn(&mut self, race: Race, at: V2) -> u32 {
        self.spawn_sized(race, at, 1000)
    }

    /// Same as `World::spawn`, except the newborn's `size` (per-mille of
    /// full size) is set explicitly rather than defaulting to fully grown;
    /// only `phase_flora`'s plant offspring (S3.5) ever pass anything but
    /// 1000 -- see `Entity.size`'s own doc comment.
    pub fn spawn_sized(&mut self, race: Race, at: V2, size: u16) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        let a = self.races[race];
        let mut e = Entity::spawn(id, race.element, self.clamp_to_bounds(at), self.seed, self.tick, &a);
        e.size = size;
        // Ids are handed out ascending, so pushing preserves the sort.
        self.entities.push(e);
        self.stats.births += 1;
        *self.consume_demand.get_mut(race) += a.consume_per(DepChannel::OnBirth);
        id
    }

    fn clamp_to_bounds(&self, p: V2) -> V2 {
        V2::new(p.x.clamp(Fx::ZERO, self.size), p.y.clamp(Fx::ZERO, self.size))
    }

    fn find(&mut self, id: u32) -> Option<usize> {
        self.entities.binary_search_by_key(&id, |e| e.id).ok()
    }

    // ------------------------------------------------------------------
    // The tick.
    // ------------------------------------------------------------------

    pub fn step(&mut self, log: &InputLog) {
        self.phase_commands(log);
        self.phase_aging();
        self.phase_movement();
        self.phase_collisions();
        self.phase_feeding();
        self.phase_flora();
        self.phase_settle();
        self.phase_terrain();
        self.phase_reap();
        self.tick += 1;
    }

    /// 1 — apply every command stamped for this tick, in canonical order.
    fn phase_commands(&mut self, log: &InputLog) {
        for c in log.at(self.tick) {
            self.apply(*c);
        }
    }

    fn apply(&mut self, c: Command) {
        match c.kind {
            CmdKind::Spawn { element, kind, at } => {
                self.spawn(Race { element, kind }, at);
            }
            CmdKind::SetHeading { dir } => {
                if let Some(i) = self.find(c.entity) {
                    let n = dir.normalized();
                    if !n.len_sq().is_zero() {
                        self.entities[i].heading = n;
                    }
                }
            }
            CmdKind::Kill => {
                if let Some(i) = self.find(c.entity) {
                    self.entities[i].hp = 0;
                }
            }
            CmdKind::Mine { element } => self.mine(c.entity, element),
            CmdKind::Smelt { element } => self.smelt(c.entity, element),
            CmdKind::MakeItem { element, quantity } => self.make_item(c.entity, element, quantity),
            CmdKind::BreakItem { index } => self.break_item(c.entity, index),
        }
    }

    /// Items/inventory (Invariant VIII extension): mine up to
    /// `RaceAttrs::mining_rate[race]` units of `element` out of the terrain
    /// cell this body currently occupies, into its own `Entity.carried` —
    /// a pure 1:1 transfer, never more than the cell actually holds. A
    /// no-op for a dead entity, an unknown id, a `Kind::Plant` (rooted —
    /// mining is a deliberate act, never passive existence), a zero-rate
    /// race, or an already-empty cell.
    fn mine(&mut self, id: u32, element: Element) {
        let Some(i) = self.find(id) else { return };
        if !self.entities[i].alive || self.entities[i].kind != Kind::Animal {
            return;
        }
        let race = self.entities[i].race();
        let rate = self.races[race].mining_rate as u64;
        if rate == 0 {
            return;
        }
        let pos = self.entities[i].pos;
        let (x, y) = self.terrain.cell_of(pos);
        let stock = self.terrain.cell(x, y)[element] as u64;
        let amount = rate.min(stock);
        if amount == 0 {
            return;
        }
        let amt16 = amount as u16; // amount <= stock <= u16::MAX
        let c = self.terrain.cell_mut(x, y);
        c[element] = c[element].saturating_sub(amt16);
        self.entities[i].carried[element] = self.entities[i].carried[element].saturating_add(amount);
    }

    /// Items/inventory (Invariant VIII extension): convert as many whole
    /// `SMELT_RATIO_IN`-unit batches of this body's own carried `element` as
    /// it currently holds into `SMELT_RATIO_OUT` units apiece of carried
    /// `element.generates()` — the fixed, race-independent ratio
    /// `SMELT_RATIO_IN`/`SMELT_RATIO_OUT` document. The per-batch difference
    /// (tailings) is not discarded: it returns to terrain at this body's
    /// current position, as `element`, in one net deposit for the whole
    /// command (same "single net write, not per-batch" discipline
    /// `terrain::apply_conversion`'s own doc comment explains for its
    /// tailings). A no-op for a dead entity, an unknown id, a `Kind::Plant`
    /// (structurally never carries anything to smelt — see
    /// `Entity.carried`'s own doc comment), or fewer than `SMELT_RATIO_IN`
    /// units on hand.
    fn smelt(&mut self, id: u32, element: Element) {
        let Some(i) = self.find(id) else { return };
        if !self.entities[i].alive || self.entities[i].kind != Kind::Animal {
            return;
        }
        let have = self.entities[i].carried[element];
        let batches = have / SMELT_RATIO_IN;
        if batches == 0 {
            return;
        }
        let consumed = batches * SMELT_RATIO_IN;
        let produced = batches * SMELT_RATIO_OUT;
        let tailings = consumed - produced;
        let next = element.generates();
        self.entities[i].carried[element] -= consumed;
        self.entities[i].carried[next] = self.entities[i].carried[next].saturating_add(produced);
        let race = self.entities[i].race();
        let pos = self.entities[i].pos;
        crate::terrain::deposit_at(&mut self.terrain, race, element, tailings, pos);
    }

    /// Items/inventory: bundle `quantity` units of this body's own carried
    /// `element` into a new `Item` pushed onto `Entity.items` — a no-op
    /// (nothing created, nothing spent) if the entity is dead, unknown,
    /// `quantity` is zero, or fewer than `quantity` units are actually
    /// carried. No `Kind` gate here (unlike `mine`/`smelt`): a `Kind::Plant`
    /// body's `carried` is structurally always zero (nothing ever credits
    /// it — mining is the only source and Plants cannot mine), so this is
    /// already a no-op for one without needing its own redundant check.
    fn make_item(&mut self, id: u32, element: Element, quantity: u64) {
        let Some(i) = self.find(id) else { return };
        if !self.entities[i].alive || quantity == 0 {
            return;
        }
        if self.entities[i].carried[element] < quantity {
            return;
        }
        self.entities[i].carried[element] -= quantity;
        self.entities[i].items.push(Item { element, quantity });
    }

    /// Items/inventory: destroy the item at `index` in this body's
    /// `Entity.items`, returning its full quantity to terrain at this
    /// body's current position, as the item's own element — Invariant VIII,
    /// a pure transfer. A no-op for a dead entity, an unknown id, or an
    /// out-of-range index.
    fn break_item(&mut self, id: u32, index: u32) {
        let Some(i) = self.find(id) else { return };
        if !self.entities[i].alive {
            return;
        }
        let idx = index as usize;
        if idx >= self.entities[i].items.len() {
            return;
        }
        let item = self.entities[i].items.remove(idx);
        let race = self.entities[i].race();
        let pos = self.entities[i].pos;
        crate::terrain::deposit_at(&mut self.terrain, race, item.element, item.quantity, pos);
    }

    /// 2 — age every body, drain `hp` for anyone past their starvation grace
    /// period (S2), and mark the expired ones. Death demand is charged here
    /// so a body that dies this tick still contributes its corpse.
    fn phase_aging(&mut self) {
        let ecology = self.ecology;
        let propagation = self.propagation;
        for i in 0..self.entities.len() {
            // Scoped so the mutable borrow of `self.entities` ends before
            // `charge_death` below needs `&mut self` itself.
            let dead = {
                let e = &mut self.entities[i];
                if !e.alive {
                    None
                } else {
                    e.age += 1;
                    e.acted = false;
                    e.hunger = e.hunger.saturating_add(1);
                    let starving = e.hunger > ecology.starve_after[e.element];
                    if starving {
                        e.hp = e.hp.saturating_sub(ecology.starve_rate[e.element]);
                    }
                    // S3.5: size is derived fresh every tick from
                    // (age, lifespan, birth_size), never accumulated -- a
                    // retuned offspring_size/MATURITY_PERMILLE takes effect
                    // immediately. Animals collapse to a constant 1000 since
                    // birth_size equals full size, so the formula never
                    // moves them.
                    // S3.8: a growing Plant's ceiling is no longer a flat
                    // 1000 -- it's capped by how much of the Plant's own
                    // element the local terrain currently holds (the "made
                    // of" mechanic, `growth_ceiling`/`growth_ref`). An
                    // Animal's ceiling stays a flat 1000, unchanged.
                    let birth_size = if e.kind == Kind::Plant { propagation.offspring_size[e.element] } else { 1000 };
                    let ceiling = if e.kind == Kind::Plant {
                        let (x, y) = self.terrain.cell_of(e.pos);
                        let stock = self.terrain.cell(x, y)[e.element];
                        crate::entity::growth_ceiling(stock, propagation.growth_ref[e.element])
                    } else {
                        1000
                    };
                    e.size = crate::entity::grown_size(birth_size, e.age, e.lifespan, ceiling);
                    if e.is_expired() || e.hp <= 0 {
                        e.alive = false;
                        // Invariant VIII (bug 1): a corpse's carried stock
                        // and bundled items are material too -- taken out of
                        // the dying `Entity` here (carried is `Copy`, items
                        // is moved out via `mem::take` since `Entity` isn't)
                        // so `charge_death` below can deposit them at the
                        // death position along with `material`.
                        let carried = e.carried;
                        let items = std::mem::take(&mut e.items);
                        Some((e.race(), starving, e.pos, e.material, carried, items))
                    } else {
                        None
                    }
                }
            };
            if let Some((race, starving, pos, material, carried, items)) = dead {
                if starving {
                    self.stats.starved += 1;
                }
                self.charge_death(race, pos, material, &carried, &items);
            }
        }
    }

    /// Charge the `OnDeath` habitat-draw demand for one body's race, count
    /// the death, and return the body's own held material, carried stock and
    /// items to terrain. Shared by natural/starvation death (`phase_aging`)
    /// and predation (`phase_feeding`) — a corpse decomposes the same way
    /// regardless of what ended the body.
    ///
    /// Invariant VIII: `material` is the dying body's own `Entity.material`
    /// at the moment of death, deposited back to terrain as `race.element`
    /// at `pos` — this literally *is* the death deposit now, replacing the
    /// old abstract `OnDeath` deposit-demand charge (there is no
    /// `deposit_per`/`deposit_demand` anymore — see `race::Conversion`'s
    /// doc comment). Unlike background deposit (spread across a race's
    /// occupied territory via `apportion`), this lands at exactly one cell:
    /// a corpse decomposes where it fell, not smeared across the map.
    /// `phase_feeding` transfers a killed body's material to its predator
    /// *before* calling this, so `material` is already `0` for prey eaten
    /// this tick — the material moved to the predator, it did not also fall
    /// to the ground, so there is no double count.
    ///
    /// `carried` (loose stock of other elements) and `items` (bundled
    /// `Item`s) are a second, previously-unaccounted pool every death path
    /// used to just drop on the floor of `phase_reap`'s `retain` — bug 1 of
    /// the Invariant VIII conservation audit. **Design decision:** unlike
    /// `material`, a predator never inherits its prey's `carried`/`items` —
    /// both always fall to the ground at the death position, regardless of
    /// cause of death (old age, starvation, or predation). Chosen over
    /// "the predator loots the corpse" for two reasons: it is the simpler,
    /// more uniform rule (one code path, `charge_death`, handles every death
    /// the same way, rather than predation needing a second transfer
    /// mechanism on top of the one `phase_feeding` already has for
    /// `material`); and thematically it is the more defensible reading —
    /// predation ("you are what you eat") plausibly transfers the prey's
    /// own flesh, but there is no reason a predator's stomach also
    /// inherits a stack of ore the prey happened to be carrying or a
    /// bundled item in its pouch. Both pools are deposited at `pos`, each at
    /// its own element, exactly like an ordinary `BreakItem`/mined-stock
    /// return would.
    fn charge_death(&mut self, race: Race, pos: V2, material: u64, carried: &PerElement<u64>, items: &[Item]) {
        let a = self.races[race];
        self.stats.deaths += 1;
        *self.consume_demand.get_mut(race) += a.consume_per(DepChannel::OnDeath);
        crate::terrain::deposit_at(&mut self.terrain, race, race.element, material, pos);
        for (e, &amt) in carried.iter() {
            crate::terrain::deposit_at(&mut self.terrain, race, e, amt, pos);
        }
        for item in items {
            crate::terrain::deposit_at(&mut self.terrain, race, item.element, item.quantity, pos);
        }
    }

    /// 3 — move, jitter, and reflect off the bounds.
    fn phase_movement(&mut self) {
        let (seed, tick, size) = (self.seed, self.tick, self.size);
        let races = self.races;
        let ecology = self.ecology;
        let behavior = self.behavior;
        let mut acted: PerRace<u64> = PerRace::filled(0);
        let (mut grazed, mut hunted, mut fled) = (0u64, 0u64, 0u64);

        // S3.4: snapshot-then-apply. Every Animal's desired heading is
        // derived from an immutable read of this tick's pre-movement
        // positions, before any body in this same phase has moved —
        // otherwise steering would depend on iteration order rather than
        // only on (seed, tick, ids), the same reasoning phase_feeding's own
        // snapshot-then-apply already documents. See
        // `docs/S3_ECOLOGY_LAYERS_DESIGN.md` §6.
        let n = self.entities.len();
        let mut drives: Vec<Option<(Drive, Option<V2>)>> = vec![None; n];
        for i in 0..n {
            if self.entities[i].alive && self.entities[i].kind == Kind::Animal {
                drives[i] = Some(crate::behavior::drive(&self.entities, &self.terrain, &ecology, &behavior, i));
            }
        }

        for i in 0..n {
            if !self.entities[i].alive {
                continue;
            }
            if self.entities[i].kind == Kind::Plant {
                // Rooted: a structural skip, not `speed == 0` alone — the
                // jitter term below would still random-walk a zero-speed
                // body if this ran. See `docs/S3_ECOLOGY_LAYERS_DESIGN.md` §4.
                continue;
            }
            let race = self.entities[i].race();

            if let Some((d, desired)) = drives[i] {
                match d {
                    Drive::Graze => grazed += 1,
                    Drive::Hunt => hunted += 1,
                    Drive::Flee => fled += 1,
                }
                if let Some(target) = desired {
                    let heading = self.entities[i].heading;
                    self.entities[i].heading = crate::behavior::steer(heading, target, behavior.turn_rate[race]);
                }
            }

            let e = &mut self.entities[i];
            let step = e.heading.scale(races[race].speed);
            let jitter = V2::new(
                rand_signed(seed, tick, e.id, Channel::MoveJitter) * JITTER,
                rand_signed(seed, tick, e.id.wrapping_add(0x9E37), Channel::MoveJitter) * JITTER,
            );
            let delta = step + jitter;
            let mut p = e.pos + delta;

            // Reflect rather than clamp, so a body never sticks to an edge.
            if p.x < Fx::ZERO {
                p.x = -p.x;
                e.heading.x = -e.heading.x;
            } else if p.x > size {
                p.x = size + size - p.x;
                e.heading.x = -e.heading.x;
            }
            if p.y < Fx::ZERO {
                p.y = -p.y;
                e.heading.y = -e.heading.y;
            } else if p.y > size {
                p.y = size + size - p.y;
                e.heading.y = -e.heading.y;
            }
            e.pos = V2::new(p.x.clamp(Fx::ZERO, size), p.y.clamp(Fx::ZERO, size));

            if delta.len_sq() > ACTION_THRESHOLD * ACTION_THRESHOLD {
                e.acted = true;
                *acted.get_mut(race) += 1;
            }
        }

        self.stats.grazed += grazed;
        self.stats.hunted += hunted;
        self.stats.fled += fled;

        for (race, n) in acted.iter() {
            if *n > 0 {
                self.stats.actions += *n;
                *self.consume_demand.get_mut(race) += races[race].consume_per(DepChannel::OnAction) * *n;
            }
        }
    }

    /// 4 — pairwise separation. O(n²) is correct and fast enough for Stage 0;
    /// a uniform-grid broadphase arrives with the terrain field at S1, and it
    /// must iterate cells in index order to stay deterministic.
    fn phase_collisions(&mut self) {
        let n = self.entities.len();
        let mut fix = vec![V2::ZERO; n];

        for i in 0..n {
            if !self.entities[i].alive {
                continue;
            }
            for j in (i + 1)..n {
                if !self.entities[j].alive {
                    continue;
                }
                let a = &self.entities[i];
                let b = &self.entities[j];
                let d = b.pos - a.pos;
                let a_race = a.race();
                let b_race = b.race();
                // S3.5: a seedling's collision footprint scales with its
                // current growth (Entity.size, per-mille of full size) --
                // the one place size is read. Animals and mature/unrooted
                // Plants are always at size 1000, so this is a no-op for
                // them (radius times 1.0 equals radius).
                let a_radius = self.races[a_race].radius * Fx::ratio(a.size as i32, 1000);
                let b_radius = self.races[b_race].radius * Fx::ratio(b.size as i32, 1000);
                let min = a_radius + b_radius;
                let dist_sq = d.len_sq();
                if dist_sq >= min * min || dist_sq.is_zero() {
                    continue;
                }
                let dist = d.len();
                let overlap = (min - dist) * Fx::HALF;
                let push = d.normalized().scale(overlap);
                fix[i] = fix[i] - push;
                fix[j] = fix[j] + push;
                self.stats.collisions += 1;
            }
        }

        let size = self.size;
        for (i, e) in self.entities.iter_mut().enumerate() {
            if !e.alive || fix[i] == V2::ZERO {
                continue;
            }
            let p = e.pos + fix[i];
            e.pos = V2::new(p.x.clamp(Fx::ZERO, size), p.y.clamp(Fx::ZERO, size));
        }
    }

    /// 5 — feeding (S2). A body whose `hunger` has reached `ecology.satiation`
    /// and which is within `ecology.forage_radius` of prey it can pair
    /// against consumes it outright: the prey dies exactly as it would from
    /// age or starvation (`charge_death`), and the predator's `hp` rises and
    /// fires `OnConsume` — the channel every race's deposit/consume mix has
    /// carried a nonzero share for since Stage 0, with nothing to fire it
    /// until now. Pairing is now split by prey Kind (S3.8): an Animal grazes
    /// a same-element Plant (`Element::eats_plant`, the Kind-sibling's
    /// product) or hunts a ring-adjacent Animal (`Element::eats_animal`, the
    /// original ring relation) — a Plant is never a predator either way. A
    /// meal that carries a body's `hp` up across `repro_threshold` spawns one
    /// offspring through the ordinary `World::spawn` path, so it charges
    /// `OnBirth` the same way a command-spawned or seeded body always has.
    /// Without the `satiation` gate every predator in reach eats every
    /// single tick it can, which empirically collapses every prey
    /// population within a few hundred ticks. The shipped `EcologyTuning`
    /// defaults are, like `TerrainTuning`'s before it, a first guess for the
    /// live tuning loop — a uniform five-way
    /// predation ring is a hard balance problem, and nothing here promises
    /// the shipped numbers converge to a stable population on their own.
    ///
    /// Pairwise, same O(n²) shape as `phase_collisions` and for the same
    /// reason — correct and fast enough here, and a future uniform-grid
    /// broadphase would want to serve both passes at once. At most one
    /// direction of any pair can be a predation match: for a same-element
    /// Animal/Plant pair only the Animal side is ever eligible to be
    /// predator (the Kind check below), and for an Animal/Animal pair
    /// `element.rs`'s ring arithmetic guarantees at most one direction can
    /// match (no element eats its own eater — see
    /// `nothing_beats_itself_and_the_two_edges_never_coincide`). So every
    /// ordered pair is checked once, in ascending-id order (Invariant IV),
    /// for a result that depends on that fixed order and never on how many
    /// candidates a body could have fed on.
    fn phase_feeding(&mut self) {
        let n = self.entities.len();
        let ecology = self.ecology;
        let races = self.races;

        // Two scratch passes, the same shape `phase_collisions` uses for its
        // `fix` buffer: decide every outcome first, against a fixed snapshot
        // of who is alive, then apply — so which pairs are found never
        // depends on the order mutations happened to land in.
        let mut eaten = vec![false; n];
        let mut fed = vec![false; n];
        // Invariant VIII: which prey index (if any) each predator index
        // ate this tick, so the material transfer below knows the pairing
        // — `eaten`/`fed` alone only record booleans, not who-ate-whom.
        // Each predator eats at most one prey per tick (`fed[pred]` blocks
        // further matches within the same scan), so this is a genuine 1:1
        // mapping.
        let mut ate: Vec<Option<usize>> = vec![None; n];

        for i in 0..n {
            if !self.entities[i].alive || eaten[i] {
                continue;
            }
            for j in (i + 1)..n {
                if !self.entities[j].alive || eaten[j] {
                    continue;
                }
                // Pairing derivation is Kind-aware and relation-correct: an
                // Animal vs. a same-element Plant pairs on `eats_plant`
                // (grazing), an Animal vs. a ring-adjacent Animal pairs on
                // `eats_animal` (hunting), and every other Kind combination
                // (Plant-vs-Plant, or the non-Animal side of a cross-Kind
                // pair) never pairs. A Plant is never a predator. Animal-vs-
                // Animal predation additionally rolls a per-race hunt-weight
                // gate below (S3.3) — grazing Plant prey stays fully
                // unconditional. See `docs/S3_ECOLOGY_LAYERS_DESIGN.md` §5.
                let a = self.entities[i].element;
                let b = self.entities[j].element;
                let ka = self.entities[i].kind;
                let kb = self.entities[j].kind;
                let pair = match (ka, kb) {
                    (Kind::Animal, Kind::Plant) if a.eats_plant() == b => Some((i, j)),
                    (Kind::Plant, Kind::Animal) if b.eats_plant() == a => Some((j, i)),
                    (Kind::Animal, Kind::Animal) if a.eats_animal() == b => Some((i, j)),
                    (Kind::Animal, Kind::Animal) if b.eats_animal() == a => Some((j, i)),
                    _ => None,
                };
                let (pred, prey) = match pair {
                    Some(p) => p,
                    None => continue,
                };
                // `eaten[pred]` matters even though the outer loop's
                // `eaten[i]` guard looks like it should already cover this:
                // that guard runs once, before this inner loop starts, but
                // `i` can still become prey *during* this same inner loop
                // (whenever `j` turns out to eat `i` first) and then, on a
                // later `j` in the same pass, be picked as a predator again.
                // Without this check a body already killed as prey earlier
                // in this exact scan could still hunt and "eat" something
                // else before the eaten-pass below ever reaps it.
                if eaten[pred] || eaten[prey] || fed[pred] {
                    continue;
                }
                // Satiation, not an `hp`-below-max gate: a predator that just
                // ate ignores further prey in reach until `hunger` has built
                // back up. Gating on `hp < MAX_HP` instead would deadlock the
                // whole ring — every body spawns below the cap on purpose
                // (`Entity::spawn`), but would still only need one meal to
                // top out, after which it stops hunting even though
                // `hunger` (ticks since last meal) has reset to zero and
                // prey is still plentiful.
                let pred_el = self.entities[pred].element;
                if self.entities[pred].hunger < ecology.satiation[pred_el] {
                    continue;
                }
                let d = self.entities[prey].pos - self.entities[pred].pos;
                let reach = ecology.forage_radius[pred_el];
                if d.len_sq() > reach * reach {
                    continue;
                }
                // S3.3: the final, kind-specific gate. Grazing (Plant prey) is
                // unconditional; hunting (Animal prey) additionally needs a
                // per-race hunt-weight roll to succeed. Rolled on
                // (seed, tick, predator id) only — never on prey id — so the
                // result is the same no matter which prey candidate this
                // predator is being tested against this tick (required so a
                // future Hunt-drive steering pass, S3.4, can agree with this
                // phase about which prey class a predator will actually take).
                if self.entities[prey].kind == Kind::Animal {
                    let pred_race = self.entities[pred].race();
                    let weight = ecology.hunt_weight[pred_race] as u32;
                    if !rand_chance(self.seed, self.tick, self.entities[pred].id, Channel::Hunt, weight, 1000) {
                        continue;
                    }
                }
                eaten[prey] = true;
                fed[pred] = true;
                ate[pred] = Some(prey);
            }
        }

        // Invariant VIII, Animal predation only ("you are what you eat"):
        // each predator's material transfer must see its own prey's *final*
        // material — including whatever that prey itself gained as a
        // predator earlier in this very tick — not the prey's raw
        // pre-transfer value. `ate` only ever has fed[pred] set once per
        // predator and eaten[prey] set once per prey (bug 5's own analysis:
        // each entity ate at most one prey and was eaten by at most one
        // predator this tick), so the pairings found this tick form simple
        // chains, never branching. Resolving `i`'s transfer-in by first
        // recursively resolving `ate[i]` (its prey) — memoized by
        // `resolved`, so a chain is only ever walked once no matter which
        // end of it this loop reaches first — makes the result a pure
        // function of this tick's `ate` pairing data, not of ascending
        // array-index iteration order (Invariant IV/VI): a 3-body chain
        // X-eats-Y-eats-Z now always credits X with X+Y+Z's material,
        // regardless of whether X or Y happens to sit at the lower array
        // index. Without this, whichever of X/Y resolved first under plain
        // ascending-index iteration could read the other's material before
        // its own chain-gain had been folded in, silently misrouting mass
        // typed as the wrong element once `charge_death` deposited the
        // orphaned remainder under the intermediate body's own race.
        fn resolve_material(i: usize, entities: &mut [Entity], ate: &[Option<usize>], resolved: &mut [bool]) {
            if resolved[i] {
                return;
            }
            resolved[i] = true;
            if let Some(prey) = ate[i] {
                resolve_material(prey, entities, ate, resolved);
                let gained = entities[prey].material;
                entities[i].material = entities[i].material.saturating_add(gained);
                entities[prey].material = 0;
            }
        }
        let mut resolved = vec![false; n];
        for (i, &was_fed) in fed.iter().enumerate() {
            if was_fed {
                resolve_material(i, &mut self.entities, &ate, &mut resolved);
            }
        }

        let mut births: Vec<(Race, V2, u32)> = Vec::new();
        for (i, &was_fed) in fed.iter().enumerate() {
            if !was_fed {
                continue;
            }
            let el = self.entities[i].element;
            let race = self.entities[i].race();
            let before = self.entities[i].hp;
            let after = before.saturating_add(ecology.feed_gain[el]).min(MAX_HP);
            self.entities[i].hp = after;
            self.entities[i].hunger = 0;
            self.stats.feedings += 1;
            *self.consume_demand.get_mut(race) += races[race].consume_per(DepChannel::OnConsume);

            if before < ecology.repro_threshold[el] && after >= ecology.repro_threshold[el] {
                births.push((race, self.entities[i].pos, self.entities[i].id));
            }
        }

        for (i, &was_eaten) in eaten.iter().enumerate() {
            if was_eaten {
                let race = self.entities[i].race();
                let pos = self.entities[i].pos;
                let material = self.entities[i].material;
                // Invariant VIII (bug 1): carried/items always fall to the
                // ground on death, including predation — see
                // `charge_death`'s doc comment for why a predator does not
                // inherit them. `material` above already reflects
                // `resolve_material`'s chain resolution: for a prey that was
                // itself a predator earlier in the chain, its material was
                // already transferred out (zeroed) before its own predator's
                // transfer ran, so this is correctly `0` for every eaten
                // body whose material moved on, and nonzero only for a
                // grazed/hunted body that never got its own transfer-in.
                let carried = self.entities[i].carried;
                let items = std::mem::take(&mut self.entities[i].items);
                self.entities[i].alive = false;
                self.entities[i].hp = 0;
                self.charge_death(race, pos, material, &carried, &items);
            }
        }

        // Newborns join by the normal `spawn` path (ascending id, so the
        // sort invariant holds) after every kill and every meal from this
        // tick is already resolved — a body born from feeding does not
        // itself get to move, collide or eat again until its own next tick.
        // An offspring is the same race as its parent — same element and
        // same kind.
        let (seed, tick) = (self.seed, self.tick);
        for (race, pos, parent_id) in births {
            let jitter = V2::new(
                rand_signed(seed, tick, parent_id, Channel::Forage) * BIRTH_SCATTER,
                rand_signed(seed, tick, parent_id.wrapping_add(0x0F0D), Channel::Forage) * BIRTH_SCATTER,
            );
            self.spawn(race, pos + jitter);
        }
    }

    /// 6 — plant reproduction (S3.5). Gated at the same terrain-tick
    /// boundary `phase_settle`/`phase_terrain` share. A new phase rather
    /// than folding into `phase_terrain`'s own operator sequence,
    /// because `phase_terrain` runs *after* `phase_settle` — a newborn's
    /// `OnBirth` demand would be deferred to the next terrain tick — and
    /// folding in would renumber `docs/S1_TERRAIN_DESIGN.md`'s documented
    /// slot wire format (five slots as of Invariant VIII — see
    /// `terrain.rs`'s own module doc). Snapshot-then-apply, the same shape
    /// `phase_feeding`'s own `births` vec already uses: every candidate is
    /// decided against a fixed snapshot of who's alive and where (including
    /// `Occupancy`, built once up front), and every successful offspring is
    /// spawned only after the whole scan completes — so a newborn cannot
    /// itself propagate in the tick it was born, and the scan never observes
    /// a growing entity vector. See `docs/S3_ECOLOGY_LAYERS_DESIGN.md`
    /// section 7.
    fn phase_flora(&mut self) {
        if !(self.tick + 1).is_multiple_of(TERRAIN_PERIOD) {
            return;
        }
        let terrain_tick = (self.tick + 1) / TERRAIN_PERIOD;
        let propagation = self.propagation;
        let occ = Occupancy::build(&self.entities, &self.terrain);

        let mut offspring: Vec<(Race, V2)> = Vec::new();
        for e in &self.entities {
            if !e.alive || e.kind != Kind::Plant {
                continue;
            }
            let el = e.element;
            let period = propagation.period[el];
            if period == 0 || !terrain_tick.is_multiple_of(period) {
                continue;
            }
            if !rand_chance(self.seed, self.tick, e.id, Channel::Propagate, propagation.chance[el] as u32, 1000) {
                continue;
            }

            let dx = rand_signed(self.seed, self.tick, e.id, Channel::Disperse) * propagation.dispersal[el];
            let dy = rand_signed(self.seed, self.tick, e.id.wrapping_add(0x0D15), Channel::Disperse) * propagation.dispersal[el];
            let candidate = self.clamp_to_bounds(e.pos + V2::new(dx, dy));

            let (cx, cy) = self.terrain.cell_of(candidate);
            // S3.8: root_min gates on the candidate cell's stock of the
            // Plant's *habitat* element (what it draws down from terrain to
            // sustain itself), not its own element -- terrain rich in what
            // this plant consumes is good habitat for it.
            let stock = self.terrain.cell(cx, cy)[el.habitat()];
            if (stock as u32) < propagation.root_min[el] as u32 {
                self.stats.rooted_rejected += 1;
                continue;
            }
            let idx = self.terrain.index(cx, cy) as u32;
            let race = e.race();
            if occ.count(race, idx) >= propagation.crowd_max[el] as u32 {
                self.stats.rooted_rejected += 1;
                continue;
            }

            offspring.push((race, candidate));
        }

        for (race, pos) in offspring {
            let size = self.propagation.offspring_size[race.element];
            self.spawn_sized(race, pos, size);
            self.stats.propagated += 1;
        }
    }

    /// 7 — at a terrain-tick boundary, charge existence and settle the
    /// (sole, post-Invariant-VIII) conversion governor. This is the only
    /// place demand becomes a granted, governed habitat draw; turning that
    /// draw into terrain change is `phase_terrain`'s job.
    fn phase_settle(&mut self) {
        if !(self.tick + 1).is_multiple_of(TERRAIN_PERIOD) {
            return;
        }

        let mut alive: PerRace<u64> = PerRace::filled(0);
        for e in &self.entities {
            if e.alive {
                *alive.get_mut(e.race()) += 1;
            }
        }
        let races = self.races;
        for (race, n) in alive.iter() {
            if *n > 0 {
                *self.consume_demand.get_mut(race) +=
                    races[race].consume_per(DepChannel::OnExistence) * *n;
            }
        }

        for race in Race::ALL {
            let c = self.consume_demand[race] / MILLI;
            let grant = self.consume_gov.get_mut(race).settle(c);
            self.stats.deposit_clipped += grant.clipped;
            self.last_consume[race] = grant;
            self.consume_demand[race] = 0;
        }
    }

    /// 8 — the four fixed-order operator slots gated at the same terrain-tick
    /// boundary `phase_settle` uses so `apply_conversion` sees this tick's
    /// freshly computed `last_consume` grant. See `docs/S1_TERRAIN_DESIGN.md`
    /// and `terrain.rs`'s own doc comment for why this exact order —
    /// conversion, attrition, suppression, diffusion — is a wire
    /// format, not a stylistic choice.
    ///
    /// **Invariant VIII.** The old, independent deposit and consume operators
    /// (slots 1 and 2) are now one coupled `apply_conversion` (slot 1) — see
    /// that function's doc comment for the exact accounting. It cannot
    /// credit a living body's own `Entity.material` itself (`terrain.rs`
    /// doesn't own `Entity` state), so it returns each race's produced
    /// body-material share for `credit_body_material` to apply immediately
    /// after, still within this same phase/terrain-tick boundary. Slots
    /// that used to be terrain's own `ring`/`star` operators, converting and
    /// nullifying stock with no entity involved at all, are gone too
    /// (predates Invariant VIII); terrain isn't its own actor, so those
    /// slots are `ecology::apply_attrition`/`apply_suppression` — the same
    /// ring/star *relations*, now read from terrain and applied to the
    /// bodies standing in it.
    fn phase_terrain(&mut self) {
        if !(self.tick + 1).is_multiple_of(TERRAIN_PERIOD) {
            return;
        }
        let terrain_tick = (self.tick + 1) / TERRAIN_PERIOD;
        let occ = Occupancy::build(&self.entities, &self.terrain);
        let body_share =
            crate::terrain::apply_conversion(&mut self.terrain, &occ, &self.races, &self.last_consume, self.seed, terrain_tick);
        self.credit_body_material(&body_share);
        crate::ecology::apply_attrition(&mut self.entities, &self.terrain, &self.ecology);
        crate::ecology::apply_suppression(&mut self.entities, &self.terrain, &self.ecology);
        crate::terrain::apply_diffusion(&mut self.terrain, &self.terrain_tuning);
    }

    /// Invariant VIII: distribute each race's produced body-material share
    /// (`apply_conversion`'s return value) evenly across every one of that
    /// race's currently-living bodies, in ascending id order (Invariant
    /// IV — `self.entities` is already sorted this way), with the
    /// indivisible remainder going to the lowest-id bodies first so the
    /// total credited is always exactly `share`, nothing left over.
    ///
    /// `apply_conversion` already guards the zero-living-bodies case (that
    /// share folds into terrain deposit instead, before this ever runs), so
    /// a nonzero `share` reaching here is always backed by at least one
    /// living body of that race.
    fn credit_body_material(&mut self, body_share: &PerRace<u64>) {
        for r in Race::ALL {
            let share = body_share[r];
            if share == 0 {
                continue;
            }
            let indices: Vec<usize> =
                (0..self.entities.len()).filter(|&i| self.entities[i].alive && self.entities[i].race() == r).collect();
            let count = indices.len() as u64;
            if count == 0 {
                continue;
            }
            let base = share / count;
            let remainder = share % count;
            for (k, &i) in indices.iter().enumerate() {
                let extra = u64::from((k as u64) < remainder);
                self.entities[i].material = self.entities[i].material.saturating_add(base + extra);
            }
        }
    }

    /// 9 — remove the dead. `retain` is order-preserving, so the id sort holds.
    fn phase_reap(&mut self) {
        self.entities.retain(|e| e.alive);
    }

    // ------------------------------------------------------------------

    pub fn alive_count(&self) -> usize {
        self.entities.iter().filter(|e| e.alive).count()
    }

    pub fn population(&self) -> PerElement<u32> {
        let mut p = PerElement::filled(0);
        for e in &self.entities {
            if e.alive {
                *p.get_mut(e.element) += 1;
            }
        }
        p
    }

    /// The canonical state hash. Everything that can affect a future tick must
    /// be in here — anything left out is a divergence this instrument cannot see.
    pub fn state_hash(&self) -> u64 {
        let mut h = Hasher::new();
        h.u64(self.seed)
            .u64(self.tick)
            .u32(self.next_id)
            .i32(self.size.raw())
            .u32(self.entities.len() as u32);

        for e in &self.entities {
            e.hash_into(&mut h);
        }
        // Terrain cells are moving world state — "what's physically in the
        // world right now" — the same category as entity positions.
        self.terrain.hash_into(&mut h);
        // The tuning tables are state now, so a retuned world must not hash
        // the same as an untuned one — otherwise `retune`/`retune_terrain`
        // would be a silent divergence.
        for (_, a) in self.races.iter() {
            a.hash_into(&mut h);
        }
        self.terrain_tuning.hash_into(&mut h);
        self.ecology.hash_into(&mut h);
        self.behavior.hash_into(&mut h);
        self.propagation.hash_into(&mut h);
        for (_, g) in self.consume_gov.iter() {
            g.hash_into(&mut h);
        }
        for (_, d) in self.consume_demand.iter() {
            h.u64(*d);
        }
        for (_, g) in self.last_consume.iter() {
            g.hash_into(&mut h);
        }
        h.u64(self.stats.births)
            .u64(self.stats.deaths)
            .u64(self.stats.collisions)
            .u64(self.stats.actions)
            .u64(self.stats.deposit_clipped)
            .u64(self.stats.feedings)
            .u64(self.stats.starved)
            .u64(self.stats.grazed)
            .u64(self.stats.hunted)
            .u64(self.stats.fled)
            .u64(self.stats.propagated)
            .u64(self.stats.rooted_rejected);
        h.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world() -> World {
        let mut w = World::new(0xC0FFEE, 64);
        w.seed_population(8);
        w
    }

    #[test]
    fn entities_stay_sorted_by_id() {
        let mut w = world();
        let log = InputLog::new();
        for _ in 0..2000 {
            w.step(&log);
            let ids: Vec<u32> = w.entities.iter().map(|e| e.id).collect();
            let mut sorted = ids.clone();
            sorted.sort_unstable();
            assert_eq!(ids, sorted, "id ordering broken at tick {}", w.tick);
        }
    }

    // Bug 4 regression: `World::retune` used to be a bare field replacement
    // with no validation at all — a `PerRace<RaceAttrs>` whose `Conversion`
    // shares summed to something other than 1000 (exactly the shape a live
    // tuning session could produce before `tuning.rs`'s knobs were switched
    // to `Conversion::set_share_rebalanced`, or that any other future
    // caller of `retune` could still hand in directly) was accepted as-is,
    // and `terrain::apply_conversion`'s `waste_amt = produced - deposit_amt
    // - body_amt` then underflowed and panicked (`overflow-checks = true`
    // in every profile) the moment that race actually converted anything.
    #[test]
    fn retune_corrects_a_conversion_whose_shares_would_have_broken_the_sum_and_panicked() {
        let mut w = world();
        let race = Race { element: Element::Wood, kind: Kind::Plant };
        let mut races = w.races;
        // 900 + 900 + 50 = 1850, not 1000 -- exactly the "two adjacent
        // keystrokes" shape the bug report describes, simulated here as a
        // single bad `retune` call rather than two separate knob edits.
        races[race].conversion = crate::race::Conversion::new(1, 1, 900, 900, 50);
        assert!(!races[race].conversion.is_valid(), "test setup should start invalid");

        w.retune(races);

        assert!(
            w.races[race].conversion.is_valid(),
            "retune must reject/correct an invalid conversion, not accept it: {:?}",
            w.races[race].conversion
        );
        assert_eq!(
            w.races[race].conversion.deposit_share as u32
                + w.races[race].conversion.body_share as u32
                + w.races[race].conversion.waste_share as u32,
            1000
        );

        // The real regression: this must not panic. Run long enough that
        // Wood-Plant bodies actually draw down habitat and convert through
        // `terrain::apply_conversion` at least once.
        let log = InputLog::new();
        for _ in 0..500 {
            w.step(&log);
        }
    }

    #[test]
    fn everything_stays_inside_the_bounds() {
        let mut w = world();
        let log = InputLog::new();
        for _ in 0..3000 {
            w.step(&log);
            for e in &w.entities {
                assert!(e.pos.x >= Fx::ZERO && e.pos.x <= w.size, "{:?}", e.pos);
                assert!(e.pos.y >= Fx::ZERO && e.pos.y <= w.size, "{:?}", e.pos);
            }
        }
    }

    #[test]
    fn fire_turns_over_many_times_before_earth_dies_once() {
        // The tempo axis, observed rather than asserted from the table —
        // this predates S2 and is specifically about the *lifespan* spread,
        // not about feeding. Ecology is neutralised (zero forage reach,
        // starvation grace beyond the run length) so predation and
        // starvation cannot end a life here: only `is_expired()` can, which
        // is the property this test exists to observe.
        let mut w = World::new(7, 48);
        w.retune_ecology(EcologyTuning {
            forage_radius: PerElement::filled(Fx::ZERO),
            starve_after: PerElement::filled(u32::MAX),
            ..EcologyTuning::default()
        });
        w.seed_population(6);
        let log = InputLog::new();
        for _ in 0..5000 {
            w.step(&log);
        }
        let pop = w.population();
        // `population()` aggregates by element, across both kinds — both
        // Earth-Plant (6,048,000-tick lifespan) and Earth-Animal
        // (2,016,000-tick lifespan) far outlive this 5000-tick run (12 = 6
        // per_race × 2 kinds), and both Fire variants (2400- and 800-tick
        // lifespans, well under 5000 even at max variance) burn out
        // identically.
        assert_eq!(pop[Element::Earth], 12, "Earth should not have died at all");
        assert_eq!(pop[Element::Fire], 0, "Fire should have burned out entirely");
        assert!(w.stats.deaths >= 12, "expected turnover, saw {}", w.stats.deaths);
    }

    #[test]
    fn governors_always_grant_inside_their_ceiling() {
        // Invariant VIII retires the lower bound (see `governor.rs`'s
        // module doc) — a grant can be anywhere from 0 up to the ceiling
        // now, never a guaranteed floor. `>= b.floor` is gone on purpose,
        // not an oversight.
        let mut w = world();
        let log = InputLog::new();
        for _ in 0..4000 {
            w.step(&log);
            for race in Race::ALL {
                let b = attrs(race).consume;
                let g = w.last_consume[race];
                assert!(
                    g.granted <= b.ceiling as u64,
                    "{}-{} granted {} above ceiling {}",
                    race.element.name(),
                    race.kind.name(),
                    g.granted,
                    b.ceiling
                );
            }
        }
    }

    #[test]
    fn an_extinct_races_terrain_stops_changing_from_its_own_activity() {
        // Invariant VIII retires the old create-from-nothing floor
        // (`governor.rs`'s module doc): only Earth-Animal exists, so every
        // other race has zero consumption demand all run and must be
        // granted exactly zero, not a floor -- conservation forbids
        // emitting material nothing was ever drawn from. This is the
        // deliberate trade-off the old
        // `an_extinct_race_still_churns_its_terrain` test used to pin down
        // the opposite of; there is no conservative equivalent of "still
        // churns," so this asserts the new behaviour in the same scenario
        // instead of merely deleting the old assertion.
        let mut w = World::new(3, 32);
        let earth_animal = Race { element: Element::Earth, kind: Kind::Animal };
        for k in 0..4 {
            w.spawn(earth_animal, V2::new(Fx::from_int(k * 3), Fx::from_int(k * 3)));
        }
        let log = InputLog::new();
        for _ in 0..(TERRAIN_PERIOD * 3) {
            w.step(&log);
        }
        for race in Race::ALL {
            if race == earth_animal {
                continue;
            }
            assert_eq!(
                w.last_consume[race].granted,
                0,
                "{}-{} extinct race should draw and be granted nothing",
                race.element.name(),
                race.kind.name()
            );
            assert_eq!(w.last_consume[race].forced, 0, "forced is structurally always 0 now");
        }
    }

    #[test]
    fn commands_are_applied_at_their_stamped_tick() {
        let mut w = World::new(1, 32);
        let id = w.spawn(Race { element: Element::Metal, kind: Kind::Animal }, V2::new(Fx::from_int(16), Fx::from_int(16)));
        let mut log = InputLog::new();
        log.push(Command {
            tick: 10,
            entity: id,
            kind: CmdKind::Kill,
        });
        log.finalize();
        for _ in 0..10 {
            w.step(&log);
        }
        assert_eq!(w.alive_count(), 1, "still alive just before the command tick");
        w.step(&log);
        assert_eq!(w.alive_count(), 0, "killed on the command tick");
    }

    // S3.0: renamed from `state_hash_notices_every_field_it_covers` — that
    // name claimed an exhaustiveness guarantee this test never checked. It
    // only asserts the hash isn't a constant across one step; `state_hash`
    // (above) is a hand-curated, non-reflective field list, and there is no
    // mechanical way to verify every `World` field is actually folded in.
    // The real mitigation is the targeted per-struct/per-field tests next to
    // this one (`state_hash_notices_a_retuned_ecology`,
    // `state_hash_notices_a_retuned_races` below, and the `hash_notices_
    // every_field` tests in `entity.rs`/`race.rs`/`ecology.rs`) — this test
    // is just a smoke test that the hash moves at all.
    #[test]
    fn state_hash_changes_as_the_world_steps() {
        let mut a = world();
        let b = a.clone();
        assert_eq!(a.state_hash(), b.state_hash());
        let log = InputLog::new();
        a.step(&log);
        assert_ne!(a.state_hash(), b.state_hash());
    }

    #[test]
    fn state_hash_notices_a_retuned_ecology() {
        let a = world();
        let mut b = a.clone();
        b.retune_ecology(EcologyTuning {
            feed_gain: PerElement::filled(1),
            ..EcologyTuning::default()
        });
        assert_ne!(a.state_hash(), b.state_hash());
    }

    #[test]
    fn state_hash_notices_a_retuned_behavior() {
        let a = world();
        let mut b = a.clone();
        b.retune_behavior(BehaviorTuning { flee_threshold: PerRace::filled(1), ..BehaviorTuning::default() });
        assert_ne!(a.state_hash(), b.state_hash());
    }

    // S3.0: `retune_ecology`/`retune_terrain` each already
    // had a "retuned" coverage test; `retune` (the race table itself) did
    // not, despite `races` being exactly the field S3.1 rekeys to `PerRace`.
    // Closing this gap now, before that change lands, is the point of S3.0.
    // S3.4: `phase_movement` increments `stats.grazed`/`hunted`/`fled` every
    // tick an Animal's FSM drive resolves to that branch, but those counters
    // live outside every hashed sub-struct (`races`, `ecology`, `behavior`,
    // ...) — they are only reachable through the hand-curated `stats.u64(..)`
    // chain at the tail of `state_hash`. Guard each one directly so a future
    // edit that drops one of the three lines fails loudly instead of
    // compiling clean and passing every other test silently.
    #[test]
    fn state_hash_notices_grazed_hunted_and_fled() {
        let a = world();

        let mut b = a.clone();
        b.stats.grazed += 1;
        assert_ne!(a.state_hash(), b.state_hash(), "grazed not hashed");

        let mut c = a.clone();
        c.stats.hunted += 1;
        assert_ne!(a.state_hash(), c.state_hash(), "hunted not hashed");

        let mut d = a.clone();
        d.stats.fled += 1;
        assert_ne!(a.state_hash(), d.state_hash(), "fled not hashed");
    }

    #[test]
    fn state_hash_notices_a_retuned_propagation() {
        let a = world();
        let mut b = a.clone();
        b.retune_propagation(crate::ecology::PropagationTuning {
            crowd_max: PerElement::filled(1),
            ..crate::ecology::PropagationTuning::default()
        });
        assert_ne!(a.state_hash(), b.state_hash());
    }

    // S3.5: `stats.propagated`/`stats.rooted_rejected` live outside every
    // hashed sub-struct, only reachable through the hand-curated
    // `stats.u64(..)` chain at the tail of `state_hash` — same reasoning as
    // `state_hash_notices_grazed_hunted_and_fled` above.
    #[test]
    fn state_hash_notices_propagated_and_rooted_rejected() {
        let a = world();

        let mut b = a.clone();
        b.stats.propagated += 1;
        assert_ne!(a.state_hash(), b.state_hash(), "propagated not hashed");

        let mut c = a.clone();
        c.stats.rooted_rejected += 1;
        assert_ne!(a.state_hash(), c.state_hash(), "rooted_rejected not hashed");
    }

    #[test]
    fn state_hash_notices_a_retuned_races() {
        let a = world();
        let mut b = a.clone();
        let mut races = a.races;
        races[Race { element: Element::Fire, kind: Kind::Animal }].lifespan += 1;
        b.retune(races);
        assert_ne!(a.state_hash(), b.state_hash());
    }

    // ------------------------------------------------------------------
    // S2 — feeding, starvation and reproduction. Hand-built scenarios that
    // pin the mechanism down exactly, rather than assertions about whether
    // the shipped `EcologyTuning` defaults happen to produce a thriving
    // population — that is a live-tuning question, the same way whether
    // `TerrainTuning`'s defaults produce visible ring-cycling was one at S1.
    // ------------------------------------------------------------------

    #[test]
    fn a_body_eaten_this_tick_cannot_also_feed_before_it_is_reaped() {
        // Regression: the scratch-buffer guard used to check `eaten[prey]`
        // and `fed[pred]` but not `eaten[pred]`, so a body killed earlier in
        // this exact O(n^2) scan (as someone else's prey) could still be
        // resolved as a predator later in the same scan, registering a
        // posthumous meal before the eaten-pass below ever reaped it.
        // Chain: Earth eats Fire eats Wood, all three in ascending-id order
        // so Fire is resolved as prey (by Earth) before it is later
        // considered as predator (of Wood) in the same inner-loop pass.
        let mut w = World::new(9, 32);
        w.retune_ecology(EcologyTuning { hunt_weight: PerRace::filled(1000), ..EcologyTuning::default() });
        let center = V2::new(Fx::from_int(16), Fx::from_int(16));
        let fire_id = w.spawn(Race { element: Element::Fire, kind: Kind::Animal }, center); // Fire eats Wood.
        let earth_id = w.spawn(Race { element: Element::Earth, kind: Kind::Animal }, center); // Earth eats Fire.
        let wood_id = w.spawn(Race { element: Element::Wood, kind: Kind::Animal }, center);
        for id in [fire_id, earth_id] {
            let i = w.entities.iter().position(|e| e.id == id).unwrap();
            let el = w.entities[i].element;
            w.entities[i].hunger = w.ecology.satiation[el];
        }

        let log = InputLog::new();
        w.step(&log);

        assert!(!w.entities.iter().any(|e| e.id == fire_id), "Fire should be Earth's meal");
        assert!(
            w.entities.iter().any(|e| e.id == wood_id),
            "Wood must survive — its only predator (Fire) was already dead this tick"
        );
        assert_eq!(w.stats.feedings, 1, "only Earth's meal should register, not a posthumous one from Fire");
    }

    #[test]
    fn feeding_kills_prey_feeds_the_predator_and_fires_on_consume() {
        let mut w = World::new(1, 32);
        w.retune_ecology(EcologyTuning { hunt_weight: PerRace::filled(1000), ..EcologyTuning::default() });
        let center = V2::new(Fx::from_int(16), Fx::from_int(16));
        let pred_id = w.spawn(Race { element: Element::Fire, kind: Kind::Animal }, center); // Fire eats Wood.
        let prey_id = w.spawn(Race { element: Element::Wood, kind: Kind::Animal }, center); // Same cell: certainly in reach.
        // Skip the satiation wait so this tick's feeding pass fires.
        let pi = w.entities.iter().position(|e| e.id == pred_id).unwrap();
        w.entities[pi].hunger = w.ecology.satiation[Element::Fire];

        let log = InputLog::new();
        w.step(&log);

        assert!(
            !w.entities.iter().any(|e| e.id == prey_id),
            "eaten prey should be dead and reaped in the same tick"
        );
        let pred = w.entities.iter().find(|e| e.id == pred_id).expect("predator survives");
        assert_eq!(pred.hp, 50 + w.ecology.feed_gain[Element::Fire], "hp should rise by feed_gain");
        assert_eq!(pred.hunger, 0, "a meal resets the hunger clock");
        assert_eq!(w.stats.feedings, 1);
    }

    #[test]
    fn a_freshly_fed_predator_does_not_eat_again_until_satiated() {
        let mut w = World::new(4, 32);
        w.retune_ecology(EcologyTuning { hunt_weight: PerRace::filled(1000), ..EcologyTuning::default() });
        let center = V2::new(Fx::from_int(16), Fx::from_int(16));
        let pred_id = w.spawn(Race { element: Element::Fire, kind: Kind::Animal }, center);
        w.spawn(Race { element: Element::Wood, kind: Kind::Animal }, center);
        w.spawn(Race { element: Element::Wood, kind: Kind::Animal }, center);
        let pi = w.entities.iter().position(|e| e.id == pred_id).unwrap();
        w.entities[pi].hunger = w.ecology.satiation[Element::Fire];

        let log = InputLog::new();
        w.step(&log);

        assert_eq!(w.stats.feedings, 1, "only one meal despite two reachable prey");
        let alive_wood = w.entities.iter().filter(|e| e.element == Element::Wood && e.alive).count();
        assert_eq!(alive_wood, 1, "the second prey survives this tick");
    }

    #[test]
    fn a_predator_below_satiation_does_not_eat_even_with_prey_in_reach() {
        // Isolates the satiation gate itself, as distinct from the same-tick
        // `fed[pred]` scratch-buffer dedup that `..._until_satiated` above
        // actually exercises: a freshly spawned predator (hunger = 0) must
        // not eat on the very first tick just because prey is reachable.
        let mut w = World::new(11, 32);
        let center = V2::new(Fx::from_int(16), Fx::from_int(16));
        w.spawn(Race { element: Element::Fire, kind: Kind::Animal }, center); // hunger starts at 0, well below satiation.
        w.spawn(Race { element: Element::Wood, kind: Kind::Animal }, center);

        let log = InputLog::new();
        w.step(&log);

        assert_eq!(w.stats.feedings, 0, "a predator below satiation must not eat");
        assert!(
            w.entities.iter().any(|e| e.element == Element::Wood && e.alive),
            "prey should survive an unsatiated predator"
        );
    }

    #[test]
    fn a_satiated_predator_starts_eating_again_on_a_later_tick() {
        // Cross-tick persistence of the satiation gate, as distinct from the
        // same-tick dedup: after one meal resets hunger to zero, the same
        // predator must go quiet until hunger climbs back to satiation, then
        // resume — this cannot be observed from a single `w.step()` call.
        let mut w = World::new(12, 32);
        w.retune_ecology(EcologyTuning {
            satiation: PerElement::filled(5),
            // Isolate the satiation property under test: the default
            // repro_threshold is well within one meal's reach from a fresh
            // spawn's hp, which would spawn a second predator mid-test and
            // confound the feedings count this test is pinning down.
            repro_threshold: PerElement::filled(i32::MAX),
            hunt_weight: PerRace::filled(1000),
            ..EcologyTuning::default()
        });
        let center = V2::new(Fx::from_int(16), Fx::from_int(16));
        let pred_id = w.spawn(Race { element: Element::Fire, kind: Kind::Animal }, center);
        let pi = w.entities.iter().position(|e| e.id == pred_id).unwrap();
        w.entities[pi].hunger = 5;
        w.spawn(Race { element: Element::Wood, kind: Kind::Animal }, center);

        let log = InputLog::new();
        w.step(&log); // first meal
        assert_eq!(w.stats.feedings, 1);

        for _ in 0..4 {
            w.spawn(Race { element: Element::Wood, kind: Kind::Animal }, center); // keep prey in reach
            w.step(&log);
        }
        assert_eq!(w.stats.feedings, 1, "still under satiation — no second meal yet");

        w.spawn(Race { element: Element::Wood, kind: Kind::Animal }, center);
        w.step(&log); // hunger has now climbed back to 5
        assert_eq!(w.stats.feedings, 2, "satiation cleared again — the predator resumes eating");
    }

    #[test]
    fn a_non_predation_pair_never_triggers_feeding() {
        // Negative case for phase_feeding's own (pred, prey) derivation,
        // independent of element.rs's own ring-arithmetic unit tests: two
        // elements that do not eat each other, co-located and both hungry,
        // must never register a meal.
        let mut w = World::new(13, 32);
        let center = V2::new(Fx::from_int(16), Fx::from_int(16));
        let a_id = w.spawn(Race { element: Element::Fire, kind: Kind::Animal }, center);
        let b_id = w.spawn(Race { element: Element::Metal, kind: Kind::Animal }, center); // Fire suppresses Metal; neither eats the other.
        for id in [a_id, b_id] {
            let i = w.entities.iter().position(|e| e.id == id).unwrap();
            let el = w.entities[i].element;
            w.entities[i].hunger = w.ecology.satiation[el];
        }

        let log = InputLog::new();
        for _ in 0..5 {
            w.step(&log);
        }

        assert_eq!(w.stats.feedings, 0, "Fire and Metal do not eat each other");
        assert_eq!(w.alive_count(), 2);
    }

    #[test]
    fn a_plant_is_never_a_predator() {
        // Same setup `feeding_kills_prey_feeds_the_predator_and_fires_on_
        // consume` uses to prove a hungry predator in reach *does* eat — but
        // the "predator" here is a Plant, so it must not, regardless of
        // satiation or reach. See `docs/S3_ECOLOGY_LAYERS_DESIGN.md` §5.
        let mut w = World::new(21, 32);
        let center = V2::new(Fx::from_int(16), Fx::from_int(16));
        let pred_id = w.spawn(Race { element: Element::Fire, kind: Kind::Plant }, center); // Fire eats Wood.
        let prey_id = w.spawn(Race { element: Element::Wood, kind: Kind::Animal }, center); // Same cell: certainly in reach.
        let pi = w.entities.iter().position(|e| e.id == pred_id).unwrap();
        w.entities[pi].hunger = w.ecology.satiation[Element::Fire];

        let log = InputLog::new();
        for _ in 0..10 {
            w.step(&log);
        }

        assert!(
            w.entities.iter().any(|e| e.id == prey_id && e.alive),
            "prey must survive a Plant that would otherwise be a satiated, in-reach predator"
        );
        assert_eq!(w.stats.feedings, 0, "a Plant must never register a feeding as predator");
    }

    #[test]
    fn hunt_weight_zero_blocks_animal_predation_but_never_grazing() {
        // Fire.eats_animal() == Wood (ring): Fire::Animal hunts Wood::Animal.
        // Fire.eats_plant() == Fire (same element): Fire::Animal grazes
        // Fire::Plant. Spawn a satiated, in-reach Fire::Animal predator
        // against BOTH prey, with hunt_weight zeroed for every Animal race —
        // the Animal prey must survive every tick (the roll can never
        // succeed at weight 0), while the Plant prey, ungated, still gets
        // grazed.
        let mut w = World::new(20, 32);
        w.retune_ecology(EcologyTuning { hunt_weight: PerRace::filled(0), ..EcologyTuning::default() });
        let center = V2::new(Fx::from_int(16), Fx::from_int(16));
        let pred_id = w.spawn(Race { element: Element::Fire, kind: Kind::Animal }, center);
        let prey_animal_id = w.spawn(Race { element: Element::Wood, kind: Kind::Animal }, center);
        w.spawn(Race { element: Element::Fire, kind: Kind::Plant }, center);
        let pi = w.entities.iter().position(|e| e.id == pred_id).unwrap();
        w.entities[pi].hunger = w.ecology.satiation[Element::Fire];

        let log = InputLog::new();
        for _ in 0..20 {
            w.step(&log);
            let pi = w.entities.iter().position(|e| e.id == pred_id).unwrap();
            w.entities[pi].hunger = w.ecology.satiation[Element::Fire]; // stay eligible every tick
        }

        assert!(
            w.entities.iter().any(|e| e.id == prey_animal_id),
            "Animal prey must never be eaten when hunt_weight is zero"
        );
        assert!(
            !w.entities.iter().any(|e| e.element == Element::Fire && e.kind == Kind::Plant),
            "Plant prey should still be grazed — grazing is unconditional on hunt_weight"
        );
    }

    #[test]
    fn a_plant_never_moves() {
        // Structural, not "speed happens to be zero": jitter alone would
        // perturb an unrooted body even at speed zero, so this pins down
        // `phase_movement`'s early skip for `Kind::Plant`.
        let mut w = World::new(22, 32);
        let start = V2::new(Fx::from_int(16), Fx::from_int(16));
        let id = w.spawn(Race { element: Element::Earth, kind: Kind::Plant }, start);

        let log = InputLog::new();
        for _ in 0..50 {
            w.step(&log);
        }

        let e = w.entities.iter().find(|e| e.id == id).expect("plant should still be alive");
        assert_eq!(e.pos, start, "a rooted plant must never move, bit-for-bit");
    }

    #[test]
    fn the_fsm_actually_drives_something_over_a_run() {
        // Not S3.7's full exit condition (that needs every drive to fire
        // under the shipped table over a longer, more careful run) — just
        // proof phase_movement's wiring is live, not dead code: with a
        // mixed population running long enough, at least Graze fires (it's
        // the default), and the mechanism doesn't panic or desync.
        let mut w = World::new(77, 64);
        w.seed_population(10);
        let log = InputLog::new();
        for _ in 0..3000 {
            w.step(&log);
        }
        assert!(w.stats.grazed > 0, "grazed should have fired at least once");
    }

    #[test]
    fn feed_gain_is_clamped_at_max_hp() {
        let mut w = World::new(14, 32);
        w.retune_ecology(EcologyTuning {
            satiation: PerElement::filled(0),
            feed_gain: PerElement::filled(MAX_HP), // one meal alone would already overshoot
            hunt_weight: PerRace::filled(1000),
            ..EcologyTuning::default()
        });
        let center = V2::new(Fx::from_int(16), Fx::from_int(16));
        let pred_id = w.spawn(Race { element: Element::Fire, kind: Kind::Animal }, center);
        w.spawn(Race { element: Element::Wood, kind: Kind::Animal }, center);

        let log = InputLog::new();
        w.step(&log);

        let pred = w.entities.iter().find(|e| e.id == pred_id).unwrap();
        assert_eq!(pred.hp, MAX_HP, "hp must clamp at MAX_HP rather than overshoot");
    }

    #[test]
    fn a_meal_that_crosses_repro_threshold_spawns_an_offspring() {
        let mut w = World::new(3, 32);
        // Isolate the repro-threshold property under test from S3.3's
        // hunt-weight roll, same as the satiation/dedup tests above.
        w.retune_ecology(EcologyTuning { hunt_weight: PerRace::filled(1000), ..EcologyTuning::default() });
        let center = V2::new(Fx::from_int(16), Fx::from_int(16));
        let pred_id = w.spawn(Race { element: Element::Fire, kind: Kind::Animal }, center);
        w.spawn(Race { element: Element::Wood, kind: Kind::Animal }, center);
        let pi = w.entities.iter().position(|e| e.id == pred_id).unwrap();
        w.entities[pi].hunger = w.ecology.satiation[Element::Fire];
        let births_before = w.stats.births;

        let log = InputLog::new();
        w.step(&log);

        assert!(
            w.stats.births > births_before,
            "a meal crossing repro_threshold should spawn offspring through the ordinary spawn path"
        );
    }

    #[test]
    fn starvation_kills_after_the_grace_period_with_no_meal_available() {
        let mut w = World::new(2, 32);
        w.retune_ecology(EcologyTuning {
            forage_radius: PerElement::filled(Fx::ZERO), // nothing is ever in reach
            starve_after: PerElement::filled(50),
            starve_rate: PerElement::filled(1000), // one drain tick is fatal
            ..EcologyTuning::default()
        });
        let id = w.spawn(Race { element: Element::Fire, kind: Kind::Animal }, V2::new(Fx::from_int(16), Fx::from_int(16)));

        let log = InputLog::new();
        for _ in 0..51 {
            w.step(&log);
        }

        assert!(!w.entities.iter().any(|e| e.id == id), "should have starved to death");
        assert_eq!(w.stats.starved, 1);
    }

    #[test]
    fn a_fed_body_never_starves() {
        let mut w = World::new(6, 32);
        // `starve_after` must clear the satiation gate, or a body starves
        // before it is ever eligible to eat at all — the grace period has to
        // be a window to find food, not a race against reaching the gate.
        // The margin is a tight 3 ticks (not a generous one) specifically so
        // the loop below can run right up against `starve_after` — proof the
        // meal's `hunger` reset actually took effect, not just that the body
        // survived somewhere comfortably inside a wide window.
        let margin = 3;
        w.retune_ecology(EcologyTuning {
            starve_after: PerElement::filled(w.ecology.satiation[Element::Fire] + margin),
            starve_rate: PerElement::filled(1000),
            hunt_weight: PerRace::filled(1000),
            ..EcologyTuning::default()
        });
        let center = V2::new(Fx::from_int(16), Fx::from_int(16));
        let pred_id = w.spawn(Race { element: Element::Fire, kind: Kind::Animal }, center);
        let pi = w.entities.iter().position(|e| e.id == pred_id).unwrap();
        w.entities[pi].hunger = w.ecology.satiation[Element::Fire];
        w.spawn(Race { element: Element::Wood, kind: Kind::Animal }, center);

        let log = InputLog::new();
        w.step(&log); // feeds immediately, resetting hunger to 0
        assert_eq!(w.stats.feedings, 1, "setup failed to feed the body");

        for _ in 0..(margin - 1) {
            w.step(&log);
        }
        assert!(
            w.entities.iter().any(|e| e.id == pred_id),
            "a body fed just before the grace period should not have starved"
        );
    }

    // ------------------------------------------------------------------
    // S3.5 — plant propagation (`phase_flora`, `Entity.size`,
    // `PropagationTuning`). Hand-built scenarios that force success or
    // failure at each gate individually, the same discipline the S2 feeding
    // tests above use, rather than leaning on the shipped defaults'
    // emergent behaviour.

    #[test]
    fn phase_flora_roots_a_seedling_under_favourable_conditions() {
        // NOTE: size alone cannot distinguish "a genuine new offspring" from
        // "the original parent, still young relative to its own maturity
        // window" -- `phase_aging` reads `birth_size` live off the current
        // `offspring_size` table for *every* alive Plant each tick,
        // regardless of how that Plant actually came to exist (see
        // `Entity.size`'s doc comment / the S3.5 design clarification). So
        // this test identifies the new offspring by id (anything other than
        // `parent_id`), not merely by scanning for `size < 1000`.
        let mut w = World::new(31, 32);
        w.retune_propagation(PropagationTuning {
            period: PerElement::filled(1),
            chance: PerElement::filled(1000),
            root_min: PerElement::filled(0),
            crowd_max: PerElement::filled(100),
            dispersal: PerElement::filled(Fx::ZERO),
            ..PropagationTuning::default()
        });
        let center = V2::new(Fx::from_int(16), Fx::from_int(16));
        let parent_id = w.spawn(Race { element: Element::Wood, kind: Kind::Plant }, center);

        let log = InputLog::new();
        for _ in 0..TERRAIN_PERIOD {
            w.step(&log);
        }

        assert!(w.stats.propagated > 0, "a favourable roll should have produced at least one offspring");
        assert!(
            w.entities.iter().any(|e| e.id != parent_id
                && e.element == Element::Wood
                && e.kind == Kind::Plant
                && e.size < 1000),
            "a smaller-than-full-size Wood seedling, distinct from the parent, should have rooted"
        );
    }

    #[test]
    fn root_min_actually_gates_propagation() {
        // Same id-based reasoning as the favourable test above: the
        // assertion is "no entity besides the original parent exists", not
        // "no small-sized entity exists" -- the parent itself is small-sized
        // too, early in its own life, under this design.
        let mut w = World::new(32, 32);
        w.retune_propagation(PropagationTuning {
            period: PerElement::filled(1),
            chance: PerElement::filled(1000),
            root_min: PerElement::filled(u16::MAX),
            crowd_max: PerElement::filled(100),
            dispersal: PerElement::filled(Fx::ZERO),
            ..PropagationTuning::default()
        });
        let center = V2::new(Fx::from_int(16), Fx::from_int(16));
        let parent_id = w.spawn(Race { element: Element::Wood, kind: Kind::Plant }, center);

        let log = InputLog::new();
        for _ in 0..TERRAIN_PERIOD {
            w.step(&log);
        }

        assert!(w.stats.rooted_rejected > 0, "an unreachable root_min should reject every attempt");
        assert!(
            !w.entities.iter().any(|e| e.id != parent_id),
            "no seedling should have rooted against an unreachable root_min"
        );
    }

    #[test]
    fn crowd_max_actually_gates_propagation() {
        let mut w = World::new(33, 32);
        w.retune_propagation(PropagationTuning {
            period: PerElement::filled(1),
            chance: PerElement::filled(1000),
            root_min: PerElement::filled(0),
            crowd_max: PerElement::filled(0),
            dispersal: PerElement::filled(Fx::ZERO),
            ..PropagationTuning::default()
        });
        let center = V2::new(Fx::from_int(16), Fx::from_int(16));
        let parent_id = w.spawn(Race { element: Element::Wood, kind: Kind::Plant }, center);

        let log = InputLog::new();
        for _ in 0..TERRAIN_PERIOD {
            w.step(&log);
        }

        assert!(
            w.stats.rooted_rejected > 0,
            "a zero crowd_max should reject every attempt even though root_min alone would pass"
        );
        assert!(
            !w.entities.iter().any(|e| e.id != parent_id),
            "no seedling should have rooted against a zero crowd_max"
        );
    }

    #[test]
    fn entity_size_actually_grows_via_phase_aging() {
        // Fire-Plant, not Wood-Plant: a much shorter lifespan keeps this
        // test's loop small while still exercising the real `phase_aging`
        // call site, not just the pure `grown_size` function in isolation
        // (`entity.rs` covers that in isolation already).
        //
        // S3.8: growth is now additionally capped by `growth_ceiling`, a
        // function of the local *own*-element (Fire) terrain stock — seed
        // it at (at least) `growth_ref` so the ceiling is a full 1000 and
        // this test still proves the age/lifespan growth curve, not the new
        // stock-scaling behavior (`entity.rs` covers that in isolation).
        //
        // Pre-existing, S3.8-era test defect fixed here (unrelated to
        // Invariant VIII, flagged by the Stage 2 report): a one-time terrain
        // seed at a single cell does not stay put -- `apply_diffusion`
        // spreads that peak out to its zero-stock neighbours every terrain
        // tick, so by several hundred ticks in the ceiling had actually
        // collapsed back below `birth_size`, freezing growth at the
        // seedling's starting size instead of proving it reaches full
        // growth. Diffusion is not what this test is about, so it is
        // disabled here the same way other tests in this file neutralise a
        // mechanism that isn't the one under test (e.g.
        // `fire_turns_over_many_times_before_earth_dies_once` zeroing
        // `forage_radius`).
        let mut w = World::new(34, 32);
        w.retune_terrain(TerrainTuning { diffuse_rate: PerElement::filled(0), diffuse_cap: PerElement::filled(0) });
        let race = Race { element: Element::Fire, kind: Kind::Plant };
        let center = V2::new(Fx::from_int(16), Fx::from_int(16));
        let (cx, cy) = w.terrain.cell_of(center);
        w.terrain.cell_mut(cx, cy)[Element::Fire] = w.propagation.growth_ref[Element::Fire];
        let id = w.spawn_sized(race, center, 100);

        let log = InputLog::new();
        for _ in 0..1000 {
            w.step(&log);
        }

        let e = w.entities.iter().find(|e| e.id == id);
        assert!(e.is_some(), "the seedling should not have died of old age or starvation mid-test");
        assert_eq!(e.unwrap().size, 1000, "size should have reached full growth well past maturity");
    }

    // ------------------------------------------------------------------
    // Invariant VIII, items/inventory extension: Mine, Smelt, MakeItem,
    // BreakItem. Hand-built scenarios that pin the exact conservation
    // arithmetic down, the same way S2's feeding tests pin `phase_feeding`
    // down rather than asserting on the shipped tuning table's behaviour.
    // ------------------------------------------------------------------

    #[test]
    fn mine_transfers_terrain_into_carried_exactly() {
        let mut w = World::new(1, 8);
        let race = Race { element: Element::Fire, kind: Kind::Animal };
        let pos = V2::new(Fx::from_int(2), Fx::from_int(2));
        let id = w.spawn(race, pos);
        let (x, y) = w.terrain.cell_of(pos);
        w.terrain.cell_mut(x, y)[Element::Wood] = 1000;
        let before_terrain = w.terrain.total(Element::Wood);

        w.mine(id, Element::Wood);

        let rate = w.races[race].mining_rate as u64;
        assert!(rate > 0, "test assumes the shipped table gives Fire-Animal a nonzero mining_rate");
        let e = w.entities.iter().find(|e| e.id == id).unwrap();
        assert_eq!(e.carried[Element::Wood], rate, "carried should gain exactly the mining rate");
        assert_eq!(
            w.terrain.total(Element::Wood),
            before_terrain - rate,
            "terrain should lose exactly what carried gained -- a pure transfer"
        );
    }

    #[test]
    fn mine_never_draws_more_than_the_cell_actually_holds() {
        let mut w = World::new(2, 8);
        let race = Race { element: Element::Fire, kind: Kind::Animal };
        let pos = V2::new(Fx::from_int(2), Fx::from_int(2));
        let id = w.spawn(race, pos);
        let (x, y) = w.terrain.cell_of(pos);
        w.terrain.cell_mut(x, y)[Element::Wood] = 3; // less than any sane mining_rate

        w.mine(id, Element::Wood);

        let e = w.entities.iter().find(|e| e.id == id).unwrap();
        assert_eq!(e.carried[Element::Wood], 3, "must not mine more than the cell held");
        assert_eq!(w.terrain.cell(x, y)[Element::Wood], 0, "cell should be fully, not over-, drained");
    }

    #[test]
    fn a_plant_can_never_mine() {
        let mut w = World::new(3, 8);
        let race = Race { element: Element::Fire, kind: Kind::Plant };
        let pos = V2::new(Fx::from_int(2), Fx::from_int(2));
        let id = w.spawn(race, pos);
        let (x, y) = w.terrain.cell_of(pos);
        w.terrain.cell_mut(x, y)[Element::Wood] = 1000;

        w.mine(id, Element::Wood);

        let e = w.entities.iter().find(|e| e.id == id).unwrap();
        assert_eq!(e.carried[Element::Wood], 0, "a rooted Plant must never mine");
        assert_eq!(w.terrain.cell(x, y)[Element::Wood], 1000, "terrain must be untouched");
    }

    #[test]
    fn smelt_converts_whole_batches_and_returns_tailings_conserving_total() {
        let mut w = World::new(4, 8);
        let race = Race { element: Element::Metal, kind: Kind::Animal };
        let pos = V2::new(Fx::from_int(3), Fx::from_int(3));
        let id = w.spawn(race, pos);
        {
            let i = w.find(id).unwrap();
            let e = &mut w.entities[i];
            e.carried[Element::Wood] = 125; // 2 whole batches of 50, 25 left over
        }

        w.smelt(id, Element::Wood);

        let (x, y) = w.terrain.cell_of(pos);
        let e = w.entities.iter().find(|e| e.id == id).unwrap();
        assert_eq!(e.carried[Element::Wood], 25, "leftover under one batch stays untouched");
        assert_eq!(e.carried[Element::Fire], 2, "Wood.generates() == Fire, 2 batches -> 2 units out");
        assert_eq!(
            w.terrain.cell(x, y)[Element::Wood],
            2 * (SMELT_RATIO_IN - SMELT_RATIO_OUT) as u16,
            "tailings (49 per batch) return to terrain at the smelter's position"
        );
        // Conservation: 125 in carried before == 25 remaining + 2*50 accounted
        // for (2 produced + 98 tailings), nothing created or destroyed.
        assert_eq!(25 + 2 * SMELT_RATIO_IN as u64, 125);
    }

    #[test]
    fn smelt_is_a_noop_below_one_whole_batch() {
        let mut w = World::new(5, 8);
        let race = Race { element: Element::Metal, kind: Kind::Animal };
        let pos = V2::new(Fx::from_int(3), Fx::from_int(3));
        let id = w.spawn(race, pos);
        {
            let i = w.find(id).unwrap();
            let e = &mut w.entities[i];
            e.carried[Element::Wood] = SMELT_RATIO_IN as u64 - 1;
        }

        w.smelt(id, Element::Wood);

        let e = w.entities.iter().find(|e| e.id == id).unwrap();
        assert_eq!(e.carried[Element::Wood], SMELT_RATIO_IN as u64 - 1, "nothing consumed below one batch");
        assert_eq!(e.carried[Element::Fire], 0);
    }

    #[test]
    fn make_item_then_break_item_round_trips_exactly() {
        let mut w = World::new(6, 8);
        let race = Race { element: Element::Metal, kind: Kind::Animal };
        let pos = V2::new(Fx::from_int(5), Fx::from_int(5));
        let id = w.spawn(race, pos);
        {
            let i = w.find(id).unwrap();
            let e = &mut w.entities[i];
            e.carried[Element::Wood] = 500;
        }

        w.make_item(id, Element::Wood, 300);
        {
            let e = w.entities.iter().find(|e| e.id == id).unwrap();
            assert_eq!(e.carried[Element::Wood], 200, "spent from carried");
            assert_eq!(e.items.len(), 1);
            assert_eq!(e.items[0], crate::entity::Item { element: Element::Wood, quantity: 300 });
        }

        let (x, y) = w.terrain.cell_of(pos);
        let before = w.terrain.cell(x, y)[Element::Wood];
        w.break_item(id, 0);
        let e = w.entities.iter().find(|e| e.id == id).unwrap();
        assert!(e.items.is_empty(), "the item should be gone");
        assert_eq!(
            w.terrain.cell(x, y)[Element::Wood],
            before + 300,
            "breaking must return the item's full quantity to terrain at the breaker's position"
        );
    }

    #[test]
    fn make_item_is_a_noop_without_enough_carried_material() {
        let mut w = World::new(7, 8);
        let race = Race { element: Element::Metal, kind: Kind::Animal };
        let id = w.spawn(race, V2::new(Fx::from_int(1), Fx::from_int(1)));
        w.make_item(id, Element::Wood, 50); // nothing carried at all
        let e = w.entities.iter().find(|e| e.id == id).unwrap();
        assert!(e.items.is_empty(), "must not fabricate an item from nothing");
    }

    #[test]
    fn break_item_out_of_range_index_is_a_noop() {
        let mut w = World::new(8, 8);
        let race = Race { element: Element::Metal, kind: Kind::Animal };
        let id = w.spawn(race, V2::new(Fx::from_int(1), Fx::from_int(1)));
        w.break_item(id, 0); // no items at all yet
        let e = w.entities.iter().find(|e| e.id == id).unwrap();
        assert!(e.items.is_empty());
    }

    // ------------------------------------------------------------------
    // Invariant VIII conservation audit: bug 1 (carried/items dropped on
    // death) and bug 5 (predation chain resolution order). Same discipline
    // as the Mine/Smelt/MakeItem/BreakItem section above -- hand-built
    // scenarios that pin the exact conservation arithmetic down.
    // ------------------------------------------------------------------

    #[test]
    fn charge_death_deposits_material_carried_and_items_at_the_death_position() {
        // Bug 1 regression: every death path used to capture and conserve
        // only `Entity.material` via `charge_death` -- `carried`
        // (mined-but-unsmelted stock of other elements) and `items`
        // (bundled `Item`s) were never inspected, then silently dropped by
        // `phase_reap`'s `retain(|e| e.alive)`. Exercises `charge_death`
        // directly, pinning its own accounting down independent of which
        // death path calls it.
        let mut w = World::new(1, 16);
        let race = Race { element: Element::Fire, kind: Kind::Animal };
        let pos = V2::new(Fx::from_int(3), Fx::from_int(3));
        let mut carried = PerElement::filled(0u64);
        carried[Element::Wood] = 40;
        carried[Element::Water] = 7;
        let items = vec![
            Item { element: Element::Metal, quantity: 12 },
            Item { element: Element::Earth, quantity: 3 },
        ];

        w.charge_death(race, pos, 25, &carried, &items);

        let (x, y) = w.terrain.cell_of(pos);
        assert_eq!(w.terrain.cell(x, y)[Element::Fire], 25, "material, as before this fix");
        assert_eq!(w.terrain.cell(x, y)[Element::Wood], 40, "carried Wood reaches terrain");
        assert_eq!(w.terrain.cell(x, y)[Element::Water], 7, "carried Water reaches terrain");
        assert_eq!(w.terrain.cell(x, y)[Element::Metal], 12, "bundled item (Metal) reaches terrain");
        assert_eq!(w.terrain.cell(x, y)[Element::Earth], 3, "bundled item (Earth) reaches terrain");
    }

    #[test]
    fn charge_death_banks_a_saturated_deposits_shortfall_instead_of_losing_it() {
        // Round-2 regression: `deposit_at` -- reached here through
        // `charge_death` for a dying body's `material`, every element of its
        // `carried` stock, and every `Item`'s quantity -- used to clip
        // silently to the target cell's remaining `u16` headroom and report
        // nothing, so any shortfall simply vanished. This is the exact
        // scenario the bug report itself gives: a body accumulates a large
        // lump sum of one carried element over its lifetime (nothing caps
        // `Entity.carried` in `mine()`), then dies onto a cell that is
        // already near saturated on that element.
        let mut w = World::new(11, 16);
        let race = Race { element: Element::Fire, kind: Kind::Animal };
        let pos = V2::new(Fx::from_int(6), Fx::from_int(6));
        let (x, y) = w.terrain.cell_of(pos);
        w.terrain.cell_mut(x, y)[Element::Water] = u16::MAX - 100; // only 100 headroom
        let mut carried = PerElement::filled(0u64);
        carried[Element::Water] = 68_000; // a whole lifetime's mined lump sum, per the bug report

        w.charge_death(race, pos, 0, &carried, &[]);

        assert_eq!(w.terrain.cell(x, y)[Element::Water], u16::MAX, "the cell fills to exactly its ceiling, no more");
        assert_eq!(
            w.terrain.overflow_of(race, Element::Water),
            68_000 - 100,
            "the shortfall a saturated cell couldn't absorb on death must be banked, not destroyed"
        );

        // Run the world for real (not a direct second `charge_death` call)
        // so the banked shortfall's retry goes through the ordinary
        // `phase_terrain` -> `apply_conversion` path exactly like any other
        // player would see it, with headroom reopened by draining the cell.
        w.terrain.cell_mut(x, y)[Element::Water] = 0;
        let before_total: u64 = (0..w.terrain.side)
            .flat_map(|yy| (0..w.terrain.side).map(move |xx| (xx, yy)))
            .map(|(xx, yy)| w.terrain.cell(xx, yy)[Element::Water] as u64)
            .sum::<u64>()
            + w.terrain.overflow_of(race, Element::Water);
        assert_eq!(before_total, 68_000 - 100, "nothing lost or gained before the retry runs");

        let log = InputLog::new();
        for _ in 0..TERRAIN_PERIOD {
            w.step(&log);
        }

        assert_eq!(w.terrain.overflow_of(race, Element::Water), 0, "fully retried once headroom reopens, nothing left banked");
        let after_total: u64 = (0..w.terrain.side)
            .flat_map(|yy| (0..w.terrain.side).map(move |xx| (xx, yy)))
            .map(|(xx, yy)| w.terrain.cell(xx, yy)[Element::Water] as u64)
            .sum();
        assert_eq!(after_total, 68_000 - 100, "the banked shortfall actually lands on terrain -- nothing created or destroyed");
    }

    #[test]
    fn smelt_banks_a_saturated_tailings_shortfall_and_it_lands_on_retry() {
        // Round-3 independent re-verification, targeting `smelt` specifically
        // (the fix stage's own end-to-end test only exercised `charge_death`;
        // `deposit_at`'s own unit tests call it directly, bypassing this call
        // site entirely). Same shape: saturate the tailings cell first.
        let mut w = World::new(21, 16);
        let race = Race { element: Element::Metal, kind: Kind::Animal };
        let pos = V2::new(Fx::from_int(6), Fx::from_int(6));
        let id = w.spawn(race, pos);
        {
            let i = w.find(id).unwrap();
            w.entities[i].carried[Element::Wood] = 2000 * SMELT_RATIO_IN; // 2000 whole batches
        }
        let (x, y) = w.terrain.cell_of(pos);
        w.terrain.cell_mut(x, y)[Element::Wood] = u16::MAX - 100; // only 100 headroom for tailings

        w.smelt(id, Element::Wood);

        let tailings = 2000 * (SMELT_RATIO_IN - SMELT_RATIO_OUT);
        assert_eq!(w.terrain.cell(x, y)[Element::Wood], u16::MAX, "cell fills to exactly its ceiling");
        assert_eq!(
            w.terrain.overflow_of(race, Element::Wood),
            tailings - 100,
            "smelt's tailings shortfall on a saturated cell must be banked, not destroyed"
        );

        w.terrain.cell_mut(x, y)[Element::Wood] = 0;
        let log = InputLog::new();
        for _ in 0..(2000 * TERRAIN_PERIOD) {
            w.step(&log);
        }

        assert_eq!(w.terrain.overflow_of(race, Element::Wood), 0, "fully retried once headroom reopens");
        let after_total: u64 = (0..w.terrain.side)
            .flat_map(|yy| (0..w.terrain.side).map(move |xx| (xx, yy)))
            .map(|(xx, yy)| w.terrain.cell(xx, yy)[Element::Wood] as u64)
            .sum();
        assert_eq!(after_total, tailings - 100, "the banked tailings shortfall actually lands, nothing created or destroyed");
    }

    #[test]
    fn reviewer8_probe_extreme_carried_and_item_quantities_do_not_panic_or_lose_material() {
        // Round-3 reviewer 8's own angle: is there any downstream consumer
        // of `Entity.carried`/`Item.quantity` that breaks (panics, wraps,
        // silently truncates) at a magnitude far beyond anything reachable
        // through ordinary mining, independent of `deposit_at`'s own fix?
        // Push both pools to values orders of magnitude past the 68,000
        // "whole lifetime" example the bug report itself used, and drive
        // them through every consumer (`smelt`, `charge_death` via
        // `carried` and via `items`) with debug assertions enabled (this is
        // a debug test build, so integer overflow panics rather than
        // silently wraps -- the exact failure mode this probe is checking
        // for).
        let mut w = World::new(31, 16);
        let race = Race { element: Element::Metal, kind: Kind::Animal };
        let pos = V2::new(Fx::from_int(4), Fx::from_int(4));
        let id = w.spawn(race, pos);

        // Absurdly large carried stock -- u64::MAX / 4, far past anything a
        // real lifetime of mining could accumulate but still a legal value
        // of the field's actual type.
        let huge: u64 = u64::MAX / 4;
        {
            let i = w.find(id).unwrap();
            w.entities[i].carried[Element::Wood] = huge;
        }

        // smelt() batches/consumed/produced/tailings arithmetic on a huge
        // `have` -- must not panic (overflow) and must conserve exactly.
        w.smelt(id, Element::Wood);
        let batches = huge / SMELT_RATIO_IN;
        let consumed = batches * SMELT_RATIO_IN;
        let produced = batches * SMELT_RATIO_OUT;
        let tailings = consumed - produced;
        {
            let i = w.find(id).unwrap();
            assert_eq!(w.entities[i].carried[Element::Wood], huge - consumed, "leftover under one batch stays untouched even at this scale");
            assert_eq!(w.entities[i].carried[Element::Fire], produced, "Wood.generates() == Fire, exact batch conversion");
        }
        // Tailings deposit_at's own headroom/overflow arithmetic on a huge
        // amount -- must bank the full shortfall, not wrap or clip silently
        // past what it reports.
        let (x, y) = w.terrain.cell_of(pos);
        let landed = w.terrain.cell(x, y)[Element::Wood] as u64;
        let banked = w.terrain.overflow_of(race, Element::Wood);
        assert_eq!(landed + banked, tailings, "every tailings unit is either on terrain or banked -- none vanished");

        // charge_death with a second huge carried element plus a huge item
        // quantity, on top of the world already carrying the smelt banked
        // overflow above -- must not panic and must conserve.
        let mut carried = PerElement::filled(0u64);
        carried[Element::Water] = huge;
        let items = vec![Item { element: Element::Earth, quantity: huge }];
        w.charge_death(race, pos, 0, &carried, &items);
        // Prevent the still-alive test body from starving/dying again mid
        // retry-loop and dumping a second, unaccounted-for lump at the same
        // position -- this probe is about the arithmetic, not about
        // exercising a second death.
        {
            let i = w.find(id).unwrap();
            w.entities[i].alive = false;
        }
        // Reviewer-1 round-3 fix: `charge_death` above also added an
        // `OnDeath` consume-demand charge for `race` (Metal), independent of
        // `deposit_at`. Once granted by the governor, `apply_conversion`'s
        // own (pre-existing, unrelated-to-this-fix) production line spends
        // that demand by drawing down `race.element.habitat()` -- Metal's
        // habitat is Earth (`Element::habitat` is one ring-step back from
        // `generates`) -- and depositing the converted result as Metal. That
        // is a real, correct, and *already-accounted-for* Earth-to-Metal
        // transfer (this same test's Earth cell has real stock sitting in
        // it after the deposit above, so the draw has something to actually
        // consume), but it is not what this probe is about: this probe
        // tracks each element's grand total in isolation
        // (`terrain(e) + overflow_of(race, e)`), which is only a valid
        // invariant for a channel nothing else is concurrently draining.
        // Zeroing the banked demand here removes that unrelated confound so
        // the assertions below isolate exactly what they claim to check --
        // `deposit_at`'s own shortfall-banking/retry arithmetic -- without
        // also asserting a stronger, unrelated claim (that ordinary
        // Conversion habitat consumption never touches a channel this probe
        // happens to be watching), which was never bug 6's scope and is
        // proven separately by `tests/conservation.rs`'s whole-system
        // invariant instead.
        w.consume_demand = PerRace::filled(0);
        w.last_consume = PerRace::default();

        let water_landed = w.terrain.cell(x, y)[Element::Water] as u64;
        let water_banked = w.terrain.overflow_of(race, Element::Water);
        assert_eq!(water_landed + water_banked, huge, "carried Water: every unit is either on terrain or banked");

        let earth_landed = w.terrain.cell(x, y)[Element::Earth] as u64;
        let earth_banked = w.terrain.overflow_of(race, Element::Earth);
        assert_eq!(earth_landed + earth_banked, huge, "item Earth quantity: every unit is either on terrain or banked");

        // Run the retry loop for real and check exact conservation, not
        // full drain: at this magnitude (~4.6e18) the shortfall vastly
        // exceeds this world's entire grid capacity (16*16 cells *
        // u16::MAX each ~= 1.7e7), so `apply_conversion`'s per-tick retry
        // can only ever land a sliver per tick and the rest is correctly
        // re-banked (`terrain.rs`'s "other channels" retry loop:
        // `*terrain.overflow.get_mut(r).get_mut(e) = banked - applied`) --
        // this is expected, not a bug: nothing here promises delivery
        // within any bounded number of ticks, only that nothing is ever
        // destroyed. `deposit_at`'s doc comment's "until it fully lands" is
        // a liveness property for realistic magnitudes (grid capacity is
        // ~250x a whole lifetime's worth of mining at this world's own
        // tuning), not a safety one -- conservation itself must hold at any
        // magnitude, which is what this asserts.
        let log = InputLog::new();
        for _ in 0..TERRAIN_PERIOD {
            w.step(&log);
        }
        let grand_total = |e: Element| -> u64 {
            let on_terrain: u64 = (0..w.terrain.side)
                .flat_map(|yy| (0..w.terrain.side).map(move |xx| (xx, yy)))
                .map(|(xx, yy)| w.terrain.cell(xx, yy)[e] as u64)
                .sum();
            on_terrain + w.terrain.overflow_of(race, e)
        };
        assert_eq!(grand_total(Element::Wood), tailings, "Wood: still exactly conserved after many retry ticks, whether landed or still banked");
        assert_eq!(grand_total(Element::Water), huge, "Water: still exactly conserved after many retry ticks, whether landed or still banked");
        assert_eq!(grand_total(Element::Earth), huge, "Earth: still exactly conserved after many retry ticks, whether landed or still banked");

        // A second, realistic-but-still-well-beyond-a-lifetime magnitude
        // (200,000 -- about 3x the bug report's own 68,000 example) *does*
        // fully drain within one grid's worth of retry ticks, since it's
        // well under total grid capacity -- proving the "eventually lands"
        // liveness property actually holds at every magnitude gameplay
        // could plausibly reach.
        let big: u64 = 200_000;
        let mut carried2 = PerElement::filled(0u64);
        carried2[Element::Fire] = big;
        w.terrain.cell_mut(x, y)[Element::Fire] = u16::MAX - 50;
        w.charge_death(race, pos, 0, &carried2, &[]);
        assert_eq!(w.terrain.overflow_of(race, Element::Fire), big - 50, "shortfall banked as expected");
        w.terrain.cell_mut(x, y)[Element::Fire] = 0;
        for _ in 0..TERRAIN_PERIOD {
            w.step(&log);
        }
        assert_eq!(w.terrain.overflow_of(race, Element::Fire), 0, "a realistic-magnitude banked shortfall does fully drain within one grid's retry ticks");
    }

    #[test]
    fn reviewer1_break_item_banks_a_saturated_shortfall_and_it_lands_on_retry() {
        // Round-3 independent re-verification, targeting `break_item`
        // specifically (third of the three call sites the bug report names;
        // neither the fix stage's own test nor `deposit_at`'s own unit tests
        // exercise this call site directly).
        let mut w = World::new(22, 16);
        let race = Race { element: Element::Earth, kind: Kind::Animal };
        let pos = V2::new(Fx::from_int(9), Fx::from_int(9));
        let id = w.spawn(race, pos);
        {
            let i = w.find(id).unwrap();
            w.entities[i].items.push(Item { element: Element::Metal, quantity: 68_000 });
        }
        let (x, y) = w.terrain.cell_of(pos);
        w.terrain.cell_mut(x, y)[Element::Metal] = u16::MAX - 100; // only 100 headroom

        w.break_item(id, 0);

        assert_eq!(w.terrain.cell(x, y)[Element::Metal], u16::MAX, "cell fills to exactly its ceiling");
        assert_eq!(
            w.terrain.overflow_of(race, Element::Metal),
            68_000 - 100,
            "break_item's returned-quantity shortfall on a saturated cell must be banked, not destroyed"
        );

        w.terrain.cell_mut(x, y)[Element::Metal] = 0;
        let log = InputLog::new();
        // Two terrain periods, not one: `apportion` caps what it can move
        // into a single cell in a single call at `u16::MAX` (`terrain.rs`'s
        // `let v = amt.min(u16::MAX as u64) as u16`), regardless of how much
        // headroom the cell actually has open. The banked shortfall here
        // (67,900) exceeds that per-tick cap (65,535), so the first terrain
        // tick can only land 65,535 and correctly re-banks the remaining
        // 2,365 for the next one -- this is the same "eventually lands, not
        // necessarily this very tick" liveness property
        // `reviewer8_probe_extreme_carried_and_item_quantities_do_not_panic_or_lose_material`
        // documents at a much larger scale. One extra period is enough here
        // since the remainder is small.
        for _ in 0..(TERRAIN_PERIOD * 2) {
            w.step(&log);
        }

        assert_eq!(w.terrain.overflow_of(race, Element::Metal), 0, "fully retried once headroom reopens across enough terrain ticks");
        let after_total: u64 = (0..w.terrain.side)
            .flat_map(|yy| (0..w.terrain.side).map(move |xx| (xx, yy)))
            .map(|(xx, yy)| w.terrain.cell(xx, yy)[Element::Metal] as u64)
            .sum();
        assert_eq!(after_total, 68_000 - 100, "the banked shortfall actually lands, nothing created or destroyed");
    }

    #[test]
    fn reviewer2_concurrent_saturating_deposits_at_one_cell_in_one_tick_neither_double_count_nor_lose_material() {
        // Round-3 reviewer 2's own angle: `charge_death` fires *multiple*
        // `deposit_at` calls in immediate succession, at the very same
        // position, in the very same tick -- material, then every carried
        // element, then every item -- and several of them can target the
        // very same (race, element) channel (two items of the same
        // element; an item sharing its element with the dying body's own
        // `material`; carried sharing an element with an item). Each
        // individual `deposit_at` call is proven correct by other tests,
        // but nothing yet proves the *sequence* of calls sharing one
        // already-saturated cell in one tick doesn't double-bank a
        // shortfall (crediting the same lost units to `overflow` twice) or
        // lose one (a later call's headroom read stale, pre-mutation state
        // and thinks there's room that a sibling call already claimed).
        //
        // Two elements are pre-saturated to different remaining headrooms
        // (Fire: 50 units, Water: 30 units) and one (Earth) is left with
        // full headroom as a control. `material` and one `Item` both target
        // Fire (the race's own element); `carried` and two separate `Item`s
        // both target Water; `carried` alone targets Earth. This exercises
        // every combination the bug report's three call sites can produce
        // within one `charge_death`: same-channel-via-material-and-item,
        // same-channel-via-two-items, and a channel with room to spare
        // sitting right alongside two that don't.
        let mut w = World::new(23, 16);
        let race = Race { element: Element::Fire, kind: Kind::Animal };
        let other_race = Race { element: Element::Metal, kind: Kind::Animal };
        let pos = V2::new(Fx::from_int(11), Fx::from_int(11));
        let (x, y) = w.terrain.cell_of(pos);
        let fire_baseline = u64::from(u16::MAX - 50); // 50 headroom
        let water_baseline = u64::from(u16::MAX - 30); // 30 headroom
        w.terrain.cell_mut(x, y)[Element::Fire] = u16::MAX - 50;
        w.terrain.cell_mut(x, y)[Element::Water] = u16::MAX - 30;
        w.terrain.cell_mut(x, y)[Element::Earth] = 0; // full headroom -- the control

        let material = 400u64; // Fire, race's own element
        let mut carried = PerElement::filled(0u64);
        carried[Element::Water] = 5_000;
        carried[Element::Earth] = 3_000;
        let items = vec![
            Item { element: Element::Water, quantity: 1_200 },
            Item { element: Element::Water, quantity: 800 },
            Item { element: Element::Fire, quantity: 600 },
        ];

        let fire_total = material + 600;
        let water_total = 5_000 + 1_200 + 800;
        let earth_total = 3_000;

        w.charge_death(race, pos, material, &carried, &items);

        // Every cell caps at exactly its ceiling, never past it, regardless
        // of how many separate calls contributed to filling it.
        assert_eq!(w.terrain.cell(x, y)[Element::Fire], u16::MAX);
        assert_eq!(w.terrain.cell(x, y)[Element::Water], u16::MAX);
        assert!(w.terrain.cell(x, y)[Element::Earth] as u64 <= earth_total, "control channel never over-filled");

        // Conservation per channel: landed-on-terrain plus banked-overflow
        // equals exactly what was requested -- not more (double-count) and
        // not less (lost) -- even though several independent calls shared
        // this one cell in this one tick.
        let fire_landed = w.terrain.cell(x, y)[Element::Fire] as u64;
        let fire_banked = w.terrain.overflow_of(race, Element::Fire);
        assert_eq!(fire_landed + fire_banked, fire_baseline + fire_total, "Fire: material + item, same channel, must sum exactly (including the cell's pre-existing baseline)");

        let water_landed = w.terrain.cell(x, y)[Element::Water] as u64;
        let water_banked = w.terrain.overflow_of(race, Element::Water);
        assert_eq!(water_landed + water_banked, water_baseline + water_total, "Water: carried + two items, same channel, must sum exactly (including the cell's pre-existing baseline)");

        let earth_landed = w.terrain.cell(x, y)[Element::Earth] as u64;
        let earth_banked = w.terrain.overflow_of(race, Element::Earth);
        assert_eq!(earth_landed, earth_total, "Earth had full headroom: nothing should have been banked at all");
        assert_eq!(earth_banked, 0);

        // No cross-contamination: an unrelated race's overflow buckets on
        // these same three channels stay untouched.
        assert_eq!(w.terrain.overflow_of(other_race, Element::Fire), 0);
        assert_eq!(w.terrain.overflow_of(other_race, Element::Water), 0);
        assert_eq!(w.terrain.overflow_of(other_race, Element::Earth), 0);
        // And the dying race's own *other* channels (Wood, Metal) stay at
        // zero too -- nothing leaked sideways into a channel nobody wrote.
        assert_eq!(w.terrain.overflow_of(race, Element::Wood), 0);
        assert_eq!(w.terrain.overflow_of(race, Element::Metal), 0);

        // Now actually run the retry through the real `phase_terrain` path
        // and confirm both banked channels land their exact remainder, with
        // nothing double-applied in the process (which would show up as a
        // grand total exceeding what was actually still banked). The cells
        // are reset to 0 to reopen headroom, deliberately discarding both
        // the pre-existing baseline and whatever this call already landed
        // on top of it -- same discipline the existing
        // `charge_death_banks_a_saturated_deposits_shortfall_instead_of_losing_it`
        // test above uses -- so only the still-banked remainder is tracked
        // from here on.
        let fire_still_banked = fire_banked;
        let water_still_banked = water_banked;
        w.terrain.cell_mut(x, y)[Element::Fire] = 0;
        w.terrain.cell_mut(x, y)[Element::Water] = 0;
        let log = InputLog::new();
        for _ in 0..TERRAIN_PERIOD {
            w.step(&log);
        }

        assert_eq!(w.terrain.overflow_of(race, Element::Fire), 0, "Fire shortfall fully retried");
        assert_eq!(w.terrain.overflow_of(race, Element::Water), 0, "Water shortfall fully retried");

        let grand_total = |e: Element| -> u64 {
            (0..w.terrain.side)
                .flat_map(|yy| (0..w.terrain.side).map(move |xx| (xx, yy)))
                .map(|(xx, yy)| w.terrain.cell(xx, yy)[e] as u64)
                .sum()
        };
        assert_eq!(grand_total(Element::Fire), fire_still_banked, "Fire: exactly the still-banked remainder lands, nothing double-applied");
        assert_eq!(grand_total(Element::Water), water_still_banked, "Water: exactly the still-banked remainder lands, nothing double-applied");
    }

    #[test]
    fn natural_death_deposits_the_dying_bodys_carried_stock_and_items_to_terrain() {
        // Bug 1, the `phase_aging` (old-age/starvation) death path
        // end-to-end: a body that expires of old age this tick must still
        // hand its carried stock and items to terrain, exactly like its own
        // `material` already did.
        let mut w = World::new(2, 16);
        let race = Race { element: Element::Water, kind: Kind::Animal };
        let pos = V2::new(Fx::from_int(4), Fx::from_int(4));
        let id = w.spawn(race, pos);
        let idx = w.entities.iter().position(|e| e.id == id).unwrap();
        w.entities[idx].carried[Element::Wood] = 15;
        w.entities[idx].items.push(Item { element: Element::Fire, quantity: 9 });
        // Force old-age expiry this tick: `phase_aging` increments `age` by
        // one before checking `is_expired`.
        w.entities[idx].age = w.entities[idx].lifespan - 1;

        w.phase_aging();

        assert!(!w.entities[idx].alive, "test setup should have killed the body of old age this tick");
        let (x, y) = w.terrain.cell_of(pos);
        assert_eq!(w.terrain.cell(x, y)[Element::Wood], 15, "carried stock reaches terrain on natural death");
        assert_eq!(w.terrain.cell(x, y)[Element::Fire], 9, "bundled item reaches terrain on natural death");
    }

    #[test]
    fn predation_death_drops_the_preys_carried_and_items_to_terrain_not_to_the_predator() {
        // Bug 1, the `phase_feeding` (predation) death path, and the design
        // decision `charge_death`'s doc comment documents: unlike
        // `material` ("you are what you eat"), a predator never inherits
        // its prey's `carried`/`items` -- both always fall to the ground at
        // the death position, the same as any other death.
        let mut w = World::new(3, 16);
        w.retune_ecology(EcologyTuning {
            satiation: PerElement::filled(0),
            hunt_weight: PerRace::filled(1000),
            ..EcologyTuning::default()
        });
        let pos = V2::new(Fx::from_int(5), Fx::from_int(5));
        let pred_id = w.spawn(Race { element: Element::Fire, kind: Kind::Animal }, pos); // Fire eats Wood.
        let prey_id = w.spawn(Race { element: Element::Wood, kind: Kind::Animal }, pos);
        let pred_idx = w.entities.iter().position(|e| e.id == pred_id).unwrap();
        let prey_idx = w.entities.iter().position(|e| e.id == prey_id).unwrap();
        w.entities[prey_idx].carried[Element::Earth] = 30;
        w.entities[prey_idx].items.push(Item { element: Element::Metal, quantity: 6 });
        w.entities[pred_idx].carried[Element::Water] = 99; // predator's own -- must stay untouched.

        w.phase_feeding();

        assert!(!w.entities[prey_idx].alive, "test setup should have the predator eat this tick");
        assert_eq!(w.entities[pred_idx].carried[Element::Water], 99, "predator's own carried stock is untouched");
        assert_eq!(w.entities[pred_idx].carried[Element::Earth], 0, "predator does not inherit prey's carried stock");
        assert!(w.entities[pred_idx].items.is_empty(), "predator does not inherit prey's items");

        let (x, y) = w.terrain.cell_of(pos);
        assert_eq!(w.terrain.cell(x, y)[Element::Earth], 30, "prey's carried stock falls to the ground");
        assert_eq!(w.terrain.cell(x, y)[Element::Metal], 6, "prey's item falls to the ground");
    }

    #[test]
    fn a_three_body_predation_chain_resolves_in_causal_order_not_array_index() {
        // Bug 5 regression. Ring relation (`Element::eats_animal`, `i - 1`):
        // Metal(3) eats Earth(2); Earth(2) eats Fire(1). Spawn order fixes
        // the array index deliberately *opposite* the predation chain --
        // Z=Fire at index 0, X=Metal at index 1, Y=Earth at index 2 — so
        // pairing discovery still finds both edges (Y eats Z is found while
        // the outer scan sits at Z's index, before Y is itself eaten by X;
        // X eats Y is found once the scan reaches X's index), but naive
        // ascending-array-index *resolution* of the fed list would visit X
        // (index 1) before Y (index 2) and read Y's stale, pre-chain
        // material — exactly the bug: X ends up short, and the orphaned
        // amount would previously have leaked onto terrain typed as Y's own
        // element (Earth) via `charge_death`, not Z's original element
        // (Fire), with no Conversion/mining/smelting action responsible.
        let mut w = World::new(0x5EED, 32);
        w.retune_ecology(EcologyTuning {
            satiation: PerElement::filled(0),
            hunt_weight: PerRace::filled(1000),
            ..EcologyTuning::default()
        });
        let pos = V2::new(Fx::from_int(10), Fx::from_int(10));
        let z_id = w.spawn(Race { element: Element::Fire, kind: Kind::Animal }, pos);
        let x_id = w.spawn(Race { element: Element::Metal, kind: Kind::Animal }, pos);
        let y_id = w.spawn(Race { element: Element::Earth, kind: Kind::Animal }, pos);
        let z_idx = w.entities.iter().position(|e| e.id == z_id).unwrap();
        let x_idx = w.entities.iter().position(|e| e.id == x_id).unwrap();
        let y_idx = w.entities.iter().position(|e| e.id == y_id).unwrap();
        assert_eq!((z_idx, x_idx, y_idx), (0, 1, 2), "test setup requires this exact array order");

        w.entities[z_idx].material = 100;
        w.entities[y_idx].material = 50;
        w.entities[x_idx].material = 0;

        w.phase_feeding();

        assert!(!w.entities[z_idx].alive, "Z should have been eaten (by Y)");
        assert!(!w.entities[y_idx].alive, "Y should have been eaten (by X)");
        assert!(w.entities[x_idx].alive, "X is the surviving top predator");
        assert_eq!(
            w.entities[x_idx].material, 150,
            "X must inherit the full chain: its own 0 + Y's 50 + Z's 100, regardless of array index order"
        );
        assert_eq!(w.entities[y_idx].material, 0, "Y's material transferred out in full, none left to leak to terrain");
        assert_eq!(w.entities[z_idx].material, 0, "Z's material transferred out in full");

        // No phantom cross-element gain: neither intermediate body's own
        // element layer received a stray deposit from this chain -- the old
        // bug would have deposited Z's orphaned 100 units onto the Earth
        // layer (Y's element) via `charge_death`.
        assert_eq!(w.terrain.total(Element::Fire), 0, "no phantom Fire deposit from the chain");
        assert_eq!(w.terrain.total(Element::Earth), 0, "no phantom Earth deposit from the chain");
        assert_eq!(w.terrain.total(Element::Metal), 0, "X is alive -- none of its material has hit terrain yet");
    }
}
