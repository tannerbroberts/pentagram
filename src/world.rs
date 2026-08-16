//! The simulation.
//!
//! The tick order below is part of the specification, not an implementation
//! detail. Reordering any two phases changes results and therefore invalidates
//! every recorded replay — treat it the way you would treat a wire format.
//!
//! `commands → aging → movement → collisions → feeding → flora → terrain →
//! reap` — `feeding` (S2, `phase_feeding`) runs after collisions so predator
//! and prey are compared at this tick's settled positions. `flora` (S3.5,
//! `phase_flora`) runs right after feeding, before terrain, so it never grows
//! `phase_terrain`'s own fixed operator sequence into an extra slot.
//! `terrain` (S1, extended by the action-recipe migration) runs the fixed
//! sequence described in `terrain.rs`'s own doc comment — bounded diffusion,
//! plus every living body's `Exist` action, auto-fired once per terrain tick.
//! The old `settle` phase (accumulating per-race existence demand for the
//! now-retired `Conversion`/`Governor` pipeline) is gone: `Exist` reads
//! terrain directly, per body, the same way `Mine` already does, with no
//! population-aggregate pre-settlement step to run first.

use crate::behavior::{BehaviorTuning, Drive};
use crate::ecology::{EcologyTuning, PropagationTuning};
use crate::element::{Element, PerElement};
use crate::entity::{Entity, Item, ACTION_THRESHOLD, MAX_HP};
use crate::fx::{Fx, V2};
use crate::governor::Governor;
use crate::hash::{Hashable, Hasher};
use crate::input::{CmdKind, Command, InputLog};
#[cfg(test)]
use crate::race::ActionRecipe;
use crate::race::{ActionSlot, ElementTransform, Kind, PerRace, Race, RaceAttrs, RateLaw, RecipeSlot, TERRAIN_PERIOD};
use crate::rand::{rand_chance, rand_signed, Channel};
use crate::terrain::{Occupancy, Terrain, TerrainTuning};

/// Per-tick positional noise, so entities do not travel on perfect rails.
pub const JITTER: Fx = Fx::ratio(1, 400);

/// Positional spread for an offspring spawned by `phase_feeding`, so a
/// cohort born from the same parent does not stack exactly on top of it.
pub const BIRTH_SCATTER: Fx = Fx::ratio(150, 100);

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Stats {
    pub births: u64,
    pub deaths: u64,
    pub collisions: u64,
    pub actions: u64,
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

    /// The tuning table this world is running, seeded (action-populated) from
    /// [`crate::race::seeded_races`] and changeable at runtime through
    /// [`World::retune`]. It lives here rather than in a global so that the
    /// live view can turn a knob without any other world — a soak, a
    /// verification replay — seeing it.
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

    /// Action-recipe system: material dropped on the ground, independent of
    /// any entity — populated by `charge_death`'s item split, reachable by a
    /// `Pickup` command, decayed at a bounded per-terrain-tick rate
    /// (`ground_decay_gov`) back into terrain by `decay_ground_items`. See
    /// `race.rs`'s module doc.
    pub ground_items: Vec<GroundItem>,
    ground_decay_gov: Governor,

    pub stats: Stats,
}

/// A pile of loose material lying on the ground, independent of any entity.
/// See `World.ground_items`'s own doc comment.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GroundItem {
    pub element: Element,
    pub quantity: u64,
    pub pos: V2,
}

impl Hashable for GroundItem {
    fn hash_into(&self, h: &mut Hasher) {
        h.u8(self.element as u8)
            .u64(self.quantity)
            .i32(self.pos.x.raw())
            .i32(self.pos.y.raw());
    }
}

/// Shared by `apply_action_recipe`'s `Ground`-slot arms — `Pickup`'s
/// proximity gate. Same squared-distance-compare shape `phase_feeding`'s own
/// reach check already uses.
#[inline]
fn near(a: V2, b: V2, reach: Fx) -> bool {
    (a - b).len_sq() <= reach * reach
}

/// Genesis terrain state (`World::new`'s one-time `seed_uniform` call): every
/// cell starts holding this much Earth, nothing else. A fresh world used to
/// rely on an always-on per-tick population-independent influx to ever have
/// anything for a race's habitat draw to work with; with that mechanism torn
/// out entirely, `World::new` has to choose a starting condition itself,
/// once, the same way it already chooses starting `hp`/positions -- see
/// `Terrain::seed_uniform`'s own doc comment for why this is a genesis
/// choice, not a repeat of the exogenous-source problem.
/// First guess, not a derived constant -- the live-tuning loop's next target
/// once there's a client for it.
const GENESIS_EARTH: u16 = 1000;

impl World {
    pub fn new(seed: u64, size_cells: i32) -> World {
        // `size` is an `Fx`, which saturates past `i32::MAX >> Fx::SHIFT`
        // cells — clamping here, once, before it reaches `Fx::from_int` or
        // `Terrain::new`, keeps both in agreement. Leaving `Terrain` free to
        // construct at a raw, unclamped size that `Fx` would have silently
        // shrunk would desync the terrain grid from the entity coordinate
        // space the design's 1:1 resolution decision depends on.
        let size_cells = size_cells.clamp(1, i32::MAX >> crate::fx::SHIFT);
        let mut terrain = Terrain::new(size_cells);
        terrain.seed_uniform(Element::Earth, GENESIS_EARTH);
        let terrain_tuning = TerrainTuning::default();
        World {
            seed,
            tick: 0,
            entities: Vec::new(),
            next_id: 1,
            size: Fx::from_int(size_cells),
            races: crate::race::seeded_races(),
            terrain,
            ground_decay_gov: Governor::new(terrain_tuning.ground_decay),
            terrain_tuning,
            ecology: EcologyTuning::default(),
            behavior: BehaviorTuning::default(),
            propagation: PropagationTuning::default(),
            ground_items: Vec::new(),
            stats: Stats::default(),
        }
    }

    /// Swap the tuning table on a running world. A straight field
    /// replacement — the action-recipe table carries no governor-style
    /// internal state to reconcile (unlike the old per-race `consume`
    /// `Governor`, retired along with `Conversion`), and no cross-field sum
    /// invariant to re-clamp (`ActionRecipe` has none of `Conversion`'s
    /// three-way-split bookkeeping). Lifespan is still the one knob that does
    /// *not* reach back — every body already alive keeps the span it rolled
    /// at birth, so lowering it thins the population by attrition rather
    /// than by mass execution.
    pub fn retune(&mut self, races: PerRace<RaceAttrs>) {
        self.races = races;
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
        let a = &self.races[race];
        let mut e = Entity::spawn(id, race.element, self.clamp_to_bounds(at), self.seed, self.tick, a);
        e.size = size;
        // Ids are handed out ascending, so pushing preserves the sort.
        self.entities.push(e);
        self.stats.births += 1;
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
        self.phase_terrain();
        self.phase_reap();
        self.tick += 1;
    }

    /// 1 — apply every command stamped for this tick, in canonical order.
    /// Builds one `Occupancy` snapshot up front (entity positions don't move
    /// again until `phase_movement`) and threads it through every
    /// action-recipe-dispatched command — same snapshot-then-apply
    /// discipline `phase_movement`/`phase_feeding` already document.
    fn phase_commands(&mut self, log: &InputLog) {
        let occ = Occupancy::build(&self.entities, &self.terrain);
        for c in log.at(self.tick) {
            self.apply(*c, &occ);
        }
    }

    fn apply(&mut self, c: Command, occ: &Occupancy) {
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
            CmdKind::Mine { element } => self.apply_action_recipe(c.entity, ActionSlot::Mine, element, occ),
            CmdKind::Smelt { element } => self.apply_action_recipe(c.entity, ActionSlot::Smelt, element, occ),
            CmdKind::Pickup { element } => self.apply_action_recipe(c.entity, ActionSlot::Pickup, element, occ),
            CmdKind::MakeItem { element, quantity } => self.make_item(c.entity, element, quantity),
            CmdKind::BreakItem { index } => self.break_item(c.entity, index),
        }
    }

    /// The one generic dispatcher every command-triggered action (`Mine`,
    /// `Smelt`, `Pickup`) and the one auto-fired action (`Exist`, from
    /// `phase_terrain`) goes through — see `race.rs`'s module doc for the
    /// design this replaces. A no-op for a dead entity, an unknown id, a
    /// race with no recipe in this `slot`, or a firing still on cooldown.
    ///
    /// Mirrors the borrow shape the old bespoke `mine`/`smelt` bodies already
    /// used: `Copy` scalars (`race`, the recipe itself) are read out first,
    /// then `recipe_stock`/`recipe_withdraw`/`recipe_deposit` each borrow
    /// exactly one field of `self` at a time, so no two field-borrows of
    /// `self` are ever live across a single expression.
    fn apply_action_recipe(&mut self, id: u32, slot: ActionSlot, element: Element, occ: &Occupancy) {
        let Some(i) = self.find(id) else { return };
        if !self.entities[i].alive {
            return;
        }
        let race = self.entities[i].race();
        let Some(recipe) = self.races[race].action(slot).copied() else { return };
        let tick = self.tick;
        if tick < self.entities[i].action_ready_at[slot as usize] {
            return;
        }

        let (x, y) = self.terrain.cell_of(self.entities[i].pos);
        let idx = self.terrain.index(x, y) as u32;
        let neighbors = occ.count(race, idx);
        let size = self.entities[i].size;
        let requested = match recipe.rate {
            RateLaw::Flat(n) => n as u64,
            RateLaw::NeighborScaled { base, per_neighbor, per_size } => (base as u64)
                .saturating_add((per_neighbor as u64).saturating_mul(neighbors.saturating_sub(1) as u64))
                .saturating_add((per_size as u64).saturating_mul(size as u64) / 1000),
        };

        let have = self.recipe_stock(i, recipe.input, element, recipe.reach);
        let batches = requested.min(have) / recipe.ratio_in as u64;
        if batches == 0 {
            return;
        }
        let consumed = batches * recipe.ratio_in as u64;
        let produced = batches * recipe.ratio_out as u64;
        let out_element = match recipe.transform {
            ElementTransform::Identity => element,
            ElementTransform::Generates => element.generates(),
        };

        self.recipe_withdraw(i, recipe.input, element, recipe.reach, consumed);
        self.recipe_deposit(i, recipe.output, out_element, produced);

        // Tailings (the unconverted remainder of every batch) always return
        // to terrain as the *input* element, at this body's own position —
        // regardless of which slot the input itself came from. Matches the
        // old bespoke `smelt`'s own tailings behavior exactly (carried in,
        // terrain tailings out).
        let tailings = consumed - produced;
        if tailings > 0 {
            let pos = self.entities[i].pos;
            crate::terrain::deposit_at(&mut self.terrain, race, element, tailings, pos);
        }

        self.entities[i].action_ready_at[slot as usize] = tick + recipe.cooldown_ticks as u64;
    }

    /// How many units of `element` this entity can currently draw from
    /// `slot` — the `have` side of `apply_action_recipe`'s batch math.
    fn recipe_stock(&self, i: usize, slot: RecipeSlot, element: Element, reach: Fx) -> u64 {
        match slot {
            RecipeSlot::Terrain => {
                let (x, y) = self.terrain.cell_of(self.entities[i].pos);
                self.terrain.cell(x, y)[element] as u64
            }
            RecipeSlot::Carried => self.entities[i].carried[element],
            RecipeSlot::Item => self.entities[i]
                .items
                .iter()
                .filter(|it| it.element == element)
                .map(|it| it.quantity)
                .sum(),
            RecipeSlot::Ground => {
                let pos = self.entities[i].pos;
                self.ground_items
                    .iter()
                    .filter(|g| g.element == element && near(g.pos, pos, reach))
                    .map(|g| g.quantity)
                    .sum()
            }
            RecipeSlot::Body => self.entities[i].material,
        }
    }

    /// Remove `amount` units of `element` from `slot` — always called with
    /// `amount <= recipe_stock(..)`, so every arm below is infallible.
    fn recipe_withdraw(&mut self, i: usize, slot: RecipeSlot, element: Element, reach: Fx, amount: u64) {
        match slot {
            RecipeSlot::Terrain => {
                let (x, y) = self.terrain.cell_of(self.entities[i].pos);
                let amt16 = amount.min(u16::MAX as u64) as u16;
                let c = self.terrain.cell_mut(x, y);
                c[element] = c[element].saturating_sub(amt16);
            }
            RecipeSlot::Carried => {
                self.entities[i].carried[element] = self.entities[i].carried[element].saturating_sub(amount);
            }
            RecipeSlot::Item => {
                let mut remaining = amount;
                self.entities[i].items.retain_mut(|it| {
                    if remaining == 0 || it.element != element {
                        return true;
                    }
                    if it.quantity <= remaining {
                        remaining -= it.quantity;
                        false
                    } else {
                        it.quantity -= remaining;
                        remaining = 0;
                        true
                    }
                });
            }
            RecipeSlot::Ground => {
                let pos = self.entities[i].pos;
                let mut remaining = amount;
                self.ground_items.retain_mut(|g| {
                    if remaining == 0 || g.element != element || !near(g.pos, pos, reach) {
                        return true;
                    }
                    if g.quantity <= remaining {
                        remaining -= g.quantity;
                        false
                    } else {
                        g.quantity -= remaining;
                        remaining = 0;
                        true
                    }
                });
            }
            RecipeSlot::Body => {
                self.entities[i].material = self.entities[i].material.saturating_sub(amount);
            }
        }
    }

    /// Add `amount` units of `element` to `slot` — the produced side of
    /// `apply_action_recipe`'s batch math. A no-op for `amount == 0`.
    fn recipe_deposit(&mut self, i: usize, slot: RecipeSlot, element: Element, amount: u64) {
        if amount == 0 {
            return;
        }
        match slot {
            RecipeSlot::Terrain => {
                let race = self.entities[i].race();
                let pos = self.entities[i].pos;
                crate::terrain::deposit_at(&mut self.terrain, race, element, amount, pos);
            }
            RecipeSlot::Carried => {
                self.entities[i].carried[element] = self.entities[i].carried[element].saturating_add(amount);
            }
            RecipeSlot::Item => {
                self.entities[i].items.push(Item { element, quantity: amount });
            }
            RecipeSlot::Ground => {
                let pos = self.entities[i].pos;
                self.ground_items.push(GroundItem { element, quantity: amount, pos });
            }
            RecipeSlot::Body => {
                self.entities[i].material = self.entities[i].material.saturating_add(amount);
            }
        }
    }

    /// Test-surface preservation: every existing `w.mine(id, element)` unit
    /// test call site stays valid, each building its own one-off `Occupancy`
    /// (`phase_commands` builds one shared snapshot for the real command
    /// path instead, to avoid rebuilding it per command).
    #[cfg(test)]
    fn mine(&mut self, id: u32, element: Element) {
        let occ = Occupancy::build(&self.entities, &self.terrain);
        self.apply_action_recipe(id, ActionSlot::Mine, element, &occ);
    }

    #[cfg(test)]
    fn smelt(&mut self, id: u32, element: Element) {
        let occ = Occupancy::build(&self.entities, &self.terrain);
        self.apply_action_recipe(id, ActionSlot::Smelt, element, &occ);
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

    /// Count the death and return the body's own held material and carried
    /// stock to terrain. Shared by natural/starvation death (`phase_aging`)
    /// and predation (`phase_feeding`) — a corpse decomposes the same way
    /// regardless of what ended the body.
    ///
    /// `material` is the dying body's own `Entity.material` at the moment of
    /// death, deposited back to terrain as `race.element` at `pos` — this
    /// lands at exactly one cell: a corpse decomposes where it fell, not
    /// smeared across the map. `phase_feeding` transfers a killed body's
    /// material to its predator *before* calling this, so `material` is
    /// already `0` for prey eaten this tick — the material moved to the
    /// predator, it did not also fall to the ground, so there is no double
    /// count.
    ///
    /// `carried` (loose stock of other elements) always falls to the ground
    /// at the death position too, the same design decision as before: unlike
    /// `material`, a predator never inherits a prey's `carried`/`items`.
    /// `items` (bundled `Item`s), unlike `carried`, no longer deposits
    /// straight to terrain — it populates `World::ground_items` at the death
    /// position instead, reachable later by a `Pickup` command rather than
    /// vanishing into the terrain layer immediately.
    fn charge_death(&mut self, race: Race, pos: V2, material: u64, carried: &PerElement<u64>, items: &[Item]) {
        self.stats.deaths += 1;
        crate::terrain::deposit_at(&mut self.terrain, race, race.element, material, pos);
        for (e, &amt) in carried.iter() {
            crate::terrain::deposit_at(&mut self.terrain, race, e, amt, pos);
        }
        for item in items {
            self.ground_items.push(GroundItem { element: item.element, quantity: item.quantity, pos });
        }
    }

    /// 3 — move, jitter, and reflect off the bounds.
    fn phase_movement(&mut self) {
        let (seed, tick, size) = (self.seed, self.tick, self.size);
        let races = &self.races;
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
        // Built once, shared across every Animal's Hunt scan this phase —
        // see `SpatialIndex`'s own doc comment for why it can't be reused
        // past this phase (positions move again in phase_collisions).
        let index = crate::terrain::SpatialIndex::build(&self.entities, &self.terrain);
        let mut drives: Vec<Option<(Drive, Option<V2>)>> = vec![None; n];
        for i in 0..n {
            if self.entities[i].alive && self.entities[i].kind == Kind::Animal {
                drives[i] = Some(crate::behavior::drive(&self.entities, &self.terrain, &index, &ecology, &behavior, i));
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

        for (_, n) in acted.iter() {
            self.stats.actions += *n;
        }
    }

    /// 4 — pairwise separation. O(n²) is correct and fast enough for Stage 0;
    /// a uniform-grid broadphase arrives with the terrain field at S1, and it
    /// must iterate cells in index order to stay deterministic.
    fn phase_collisions(&mut self) {
        let n = self.entities.len();
        let mut fix = vec![V2::ZERO; n];

        // Broadphase: the same unordered-pair set a brute-force `for j in
        // (i+1)..n` scan would find, just without visiting every other
        // entity to find it. `fix`'s accumulation and `stats.collisions`
        // are both order-independent (every qualifying pair contributes
        // exactly once, and addition commutes), so — unlike phase_feeding
        // just below — this rewire only needs the *set* of pairs to match,
        // not the visiting order. `max_radius` bounds how far any race's
        // collision footprint can reach, so `a_radius + max_radius` is a
        // safe upper bound on `min` for every possible `b`, regardless of
        // which race `b` turns out to be — `SpatialIndex` doesn't know
        // about per-race radii, only cells.
        let index = crate::terrain::SpatialIndex::build(&self.entities, &self.terrain);
        let max_radius = self.races.iter().map(|(_, a)| a.radius).max().unwrap_or(Fx::ZERO);

        for i in 0..n {
            if !self.entities[i].alive {
                continue;
            }
            let a = &self.entities[i];
            let a_race = a.race();
            let a_radius = self.races[a_race].radius * Fx::ratio(a.size as i32, 1000);
            let (cx, cy) = self.terrain.cell_of(a.pos);
            let r = crate::terrain::SpatialIndex::radius_cells(a_radius + max_radius);
            for j in index.query_ring(cx, cy, r) {
                let j = j as usize;
                if j <= i || !self.entities[j].alive {
                    continue;
                }
                let b = &self.entities[j];
                let d = b.pos - self.entities[i].pos;
                let b_race = b.race();
                // S3.5: a seedling's collision footprint scales with its
                // current growth (Entity.size, per-mille of full size) --
                // the one place size is read. Animals and mature/unrooted
                // Plants are always at size 1000, so this is a no-op for
                // them (radius times 1.0 equals radius).
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

        // Broadphase: unlike phase_collisions, this scan's result genuinely
        // depends on visiting order (a body can only feed once per tick,
        // enforced by `eaten`/`fed` flags checked mid-scan — see this
        // function's own doc comment on Invariant IV), so `query_ring`'s
        // ascending-index guarantee is load-bearing here, not just a nicety.
        // Which of `(i, j)` ends up `pred` isn't known until the Kind/element
        // match below runs, so the reach bound has to cover *either*
        // direction: `max_forage`, the largest `forage_radius` any element
        // ships, dominates whichever `ecology.forage_radius[pred_el]` this
        // pair actually resolves to.
        let index = crate::terrain::SpatialIndex::build(&self.entities, &self.terrain);
        let max_forage = ecology.forage_radius.iter().map(|(_, r)| *r).max().unwrap_or(Fx::ZERO);
        let search_r = crate::terrain::SpatialIndex::radius_cells(max_forage);

        for i in 0..n {
            if !self.entities[i].alive || eaten[i] {
                continue;
            }
            let (cx, cy) = self.terrain.cell_of(self.entities[i].pos);
            for j in index.query_ring(cx, cy, search_r) {
                let j = j as usize;
                if j <= i {
                    continue;
                }
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
    /// boundary `phase_terrain` uses. A new phase rather than folding into
    /// `phase_terrain`'s own operator sequence, so it never grows that fixed
    /// sequence by an extra slot — see `terrain.rs`'s own module doc.
    /// Snapshot-then-apply, the same shape
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

    /// 7 — the fixed-order operator sequence gated at a terrain-tick
    /// boundary. See `terrain.rs`'s own doc comment for why this exact order
    /// — existence, attrition, suppression, diffusion, ground decay — is a
    /// wire format, not a stylistic choice.
    ///
    /// **Action-recipe migration.** The old `phase_settle` (population-
    /// aggregate existence demand, settled through a per-race `Governor`
    /// into a granted draw `apply_conversion` then read) is gone entirely —
    /// existence is now just another `ActionRecipe`, dispatched per living
    /// body through the exact same `apply_action_recipe` every command goes
    /// through, gated by `ActionSlot::Exist`. A race with no `Exist` recipe
    /// does nothing here, the same way an action-less race already does
    /// nothing on `Mine`. Terrain isn't its own actor: `ecology::
    /// apply_attrition`/`apply_suppression` read terrain and act on bodies
    /// (the ring/star relations), never the other way around.
    fn phase_terrain(&mut self) {
        if !(self.tick + 1).is_multiple_of(TERRAIN_PERIOD) {
            return;
        }
        let occ = Occupancy::build(&self.entities, &self.terrain);
        for i in 0..self.entities.len() {
            if !self.entities[i].alive {
                continue;
            }
            let id = self.entities[i].id;
            // `Exist` draws the race's habitat element (what it eats) and
            // produces its own element -- the `ElementTransform::Generates`
            // on the shipped `Exist` recipe turns `habitat()` back into
            // `element` (see race.rs's `seed_actions`).
            let habitat = self.entities[i].element.habitat();
            self.apply_action_recipe(id, ActionSlot::Exist, habitat, &occ);
        }
        crate::ecology::apply_attrition(&mut self.entities, &self.terrain, &self.ecology);
        crate::ecology::apply_suppression(&mut self.entities, &self.terrain, &self.ecology);
        crate::terrain::apply_diffusion(&mut self.terrain, &self.terrain_tuning);
        self.decay_ground_items();
    }

    /// Bounded per-terrain-tick return of `World::ground_items` back to
    /// terrain — same `RateBand`/`Governor::settle` aggregate-then-clamp
    /// discipline every other bounded rate in this crate follows (Invariant
    /// VII), rather than an unclamped per-tick drain. `race` attribution for
    /// `deposit_at`'s overflow bucketing is a placeholder (a ground item has
    /// no owning race by construction) — not load-bearing for correctness,
    /// since `deposit_at`'s `race` parameter only ever affects which race's
    /// `Terrain::overflow` bucket a saturated shortfall banks under.
    fn decay_ground_items(&mut self) {
        let total: u64 = self.ground_items.iter().map(|g| g.quantity).sum();
        if total == 0 {
            return;
        }
        let grant = self.ground_decay_gov.settle(total);
        let mut remaining = grant.granted;
        let mut i = 0;
        while i < self.ground_items.len() && remaining > 0 {
            let g = self.ground_items[i];
            let take = g.quantity.min(remaining);
            let race = Race { element: g.element, kind: Kind::Animal };
            crate::terrain::deposit_at(&mut self.terrain, race, g.element, take, g.pos);
            self.ground_items[i].quantity -= take;
            remaining -= take;
            if self.ground_items[i].quantity == 0 {
                self.ground_items.swap_remove(i);
            } else {
                i += 1;
            }
        }
    }

    /// 8 — remove the dead. `retain` is order-preserving, so the id sort holds.
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
        self.ground_decay_gov.hash_into(&mut h);
        h.u32(self.ground_items.len() as u32);
        for g in &self.ground_items {
            g.hash_into(&mut h);
        }
        h.u64(self.stats.births)
            .u64(self.stats.deaths)
            .u64(self.stats.collisions)
            .u64(self.stats.actions)
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
    fn a_fresh_world_starts_with_genesis_earth_everywhere_and_nothing_else() {
        let w = World::new(0xC0FFEE, 5);
        for y in 0..5i32 {
            for x in 0..5i32 {
                assert_eq!(w.terrain.cell(x, y)[Element::Earth], GENESIS_EARTH);
                for e in Element::ALL {
                    if e != Element::Earth {
                        assert_eq!(w.terrain.cell(x, y)[e], 0, "{} should start at zero", e.name());
                    }
                }
            }
        }
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

    #[test]
    fn a_retuned_race_table_takes_effect_and_never_panics_over_a_run() {
        // `World::retune` is a bare field replacement now (Conversion's
        // three-way-split clamp it used to need is retired along with
        // Conversion itself — see `World::retune`'s own doc comment). This
        // still exercises the one thing worth guarding: a retuned table
        // takes effect and a run through it doesn't panic.
        let mut w = world();
        let race = Race { element: Element::Wood, kind: Kind::Plant };
        let mut races = w.races.clone();
        races[race].lifespan *= 2;
        let expected = races[race].lifespan;
        w.retune(races);
        assert_eq!(w.races[race].lifespan, expected, "retune must actually take effect");

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
    fn an_extinct_races_terrain_only_changes_via_other_races_own_actions() {
        // Action-recipe migration: `Exist` (and every other action) dispatches
        // per living entity (`phase_terrain`'s loop over `self.entities`), so
        // a race with zero living bodies is automatically never dispatched —
        // there is no population-aggregate governor step left to separately
        // guarantee this. Only Earth-Animal exists; every other race's own
        // habitat/self element pair should see no *Exist*-driven change (the
        // ring relations `apply_attrition`/`apply_suppression` still touch
        // every layer regardless of who's alive, which is a separate,
        // deliberate design — see `terrain.rs`'s own module doc).
        let mut w = World::new(3, 32);
        let earth_animal = Race { element: Element::Earth, kind: Kind::Animal };
        for k in 0..4 {
            w.spawn(earth_animal, V2::new(Fx::from_int(k * 3), Fx::from_int(k * 3)));
        }
        for r in Race::ALL {
            if r != earth_animal {
                assert!(w.races[r].action(crate::race::ActionSlot::Exist).is_some(), "sanity: every race ships an Exist recipe");
            }
        }
        let log = InputLog::new();
        for _ in 0..(TERRAIN_PERIOD * 3) {
            w.step(&log);
        }
        // No panic, and the extinct races' own bodies never existed to be
        // dispatched -- the property under test is structural (see the doc
        // comment above), so a clean run to completion is the assertion.
        // 300 ticks is a tiny fraction of Earth-Animal's fortnight-scale
        // lifespan, so the seeded population should still be alive too.
        assert!(w.alive_count() > 0, "the seeded Earth-Animal population should still be alive over a short run");
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
        let mut races = a.races.clone();
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
        w.retune_terrain(TerrainTuning {
            diffuse_rate: PerElement::filled(0),
            diffuse_cap: PerElement::filled(0),
            ground_decay: w.terrain_tuning.ground_decay,
        });
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

    // Every shipped Animal row's `Smelt` recipe (race.rs's `RACES` table) —
    // pinned here as plain constants the same way `World::SMELT_RATIO_IN`/
    // `SMELT_RATIO_OUT` used to be, before smelting became per-race.
    const SMELT_RATIO_IN: u64 = 50;
    const SMELT_RATIO_OUT: u64 = 1;

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

        let rate = match w.races[race].action(ActionSlot::Mine).unwrap().rate {
            RateLaw::Flat(n) => n as u64,
            RateLaw::NeighborScaled { .. } => panic!("test assumes Mine ships a flat rate"),
        };
        assert!(rate > 0, "test assumes the shipped table gives Fire-Animal a nonzero Mine rate");
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
        assert_eq!(25 + 2 * SMELT_RATIO_IN, 125);
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
            e.carried[Element::Wood] = SMELT_RATIO_IN - 1;
        }

        w.smelt(id, Element::Wood);

        let e = w.entities.iter().find(|e| e.id == id).unwrap();
        assert_eq!(e.carried[Element::Wood], SMELT_RATIO_IN - 1, "nothing consumed below one batch");
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
        // Genesis seeds every cell with GENESIS_EARTH -- an unrelated
        // confound for this probe's exact-conservation arithmetic below.
        w.terrain.seed_uniform(Element::Earth, 0);
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
        // Action-recipe migration: items no longer deposit straight to
        // terrain -- they populate `World::ground_items` at the death
        // position instead, reachable later by a `Pickup` command.
        assert_eq!(w.terrain.cell(x, y)[Element::Metal], 0, "bundled items no longer reach terrain directly");
        assert_eq!(w.terrain.cell(x, y)[Element::Earth], 0, "bundled items no longer reach terrain directly");
        assert_eq!(
            w.ground_items,
            vec![
                GroundItem { element: Element::Metal, quantity: 12, pos },
                GroundItem { element: Element::Earth, quantity: 3, pos },
            ],
            "bundled items land in ground_items instead"
        );
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

        // Known hole from the action-recipe migration (`deposit_at`'s own
        // doc comment): `apply_conversion`, the only code that ever retried
        // a banked shortfall, is gone -- running the world for real no
        // longer drains it, even once headroom reopens.
        w.terrain.cell_mut(x, y)[Element::Water] = 0;
        let log = InputLog::new();
        for _ in 0..TERRAIN_PERIOD {
            w.step(&log);
        }
        assert_eq!(w.terrain.overflow_of(race, Element::Water), 68_000 - 100, "nothing drains the bank on its own");
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
            w.entities[i].carried[Element::Wood] = 2000 * SMELT_RATIO_IN; // far more than one firing can touch
        }
        let (x, y) = w.terrain.cell_of(pos);
        w.terrain.cell_mut(x, y)[Element::Wood] = u16::MAX - 100; // only 100 headroom for tailings

        w.smelt(id, Element::Wood);

        // Smelt's shipped recipe caps a single firing at `Flat(u16::MAX)`
        // input units -- only that many actually convert this call, not all
        // 2000 batches carried.
        let batches = (u16::MAX as u64) / SMELT_RATIO_IN;
        let tailings = batches * (SMELT_RATIO_IN - SMELT_RATIO_OUT);
        assert_eq!(w.terrain.cell(x, y)[Element::Wood], u16::MAX, "cell fills to exactly its ceiling");
        assert_eq!(
            w.terrain.overflow_of(race, Element::Wood),
            tailings - 100,
            "smelt's tailings shortfall on a saturated cell must be banked, not destroyed"
        );

        // Known hole from the action-recipe migration (`deposit_at`'s own
        // doc comment): nothing retries a banked shortfall anymore, even
        // once headroom reopens and the world keeps running.
        w.terrain.cell_mut(x, y)[Element::Wood] = 0;
        let log = InputLog::new();
        for _ in 0..TERRAIN_PERIOD {
            w.step(&log);
        }
        assert_eq!(w.terrain.overflow_of(race, Element::Wood), tailings - 100, "nothing drains the bank on its own");
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
        // Genesis seeds every cell with GENESIS_EARTH -- an unrelated
        // confound for this probe's exact-conservation arithmetic below.
        w.terrain.seed_uniform(Element::Earth, 0);
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
        // Smelt's shipped recipe caps a single firing at `Flat(u16::MAX)`
        // input units (`race.rs`'s `RACES` migration doc comment) -- at this
        // scale `have` vastly exceeds that cap, so only one firing's worth
        // (65 535, floored to a whole batch) actually converts; the rest
        // stays carried, untouched, ready for a later firing.
        w.smelt(id, Element::Wood);
        let batches = (u16::MAX as u64) / SMELT_RATIO_IN;
        let consumed = batches * SMELT_RATIO_IN;
        let produced = batches * SMELT_RATIO_OUT;
        let tailings = consumed - produced;
        {
            let i = w.find(id).unwrap();
            assert_eq!(w.entities[i].carried[Element::Wood], huge - consumed, "only one firing's capped batch converts, the rest stays carried");
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
        let water_landed = w.terrain.cell(x, y)[Element::Water] as u64;
        let water_banked = w.terrain.overflow_of(race, Element::Water);
        assert_eq!(water_landed + water_banked, huge, "carried Water: every unit is either on terrain or banked");

        // `charge_death`'s `items` no longer go through `deposit_at` at all
        // (action-recipe migration) -- they populate `World::ground_items`
        // instead. The huge Earth item quantity should land there, whole,
        // not split across terrain/overflow.
        let earth_on_ground: u64 = w.ground_items.iter().filter(|g| g.element == Element::Earth).map(|g| g.quantity).sum();
        assert_eq!(earth_on_ground, huge, "item Earth quantity: lands whole in ground_items, not on terrain");

        // Known hole from the action-recipe migration (`deposit_at`'s own
        // doc comment): `apply_conversion`, the only code that ever retried
        // a banked shortfall, is gone -- running the world for real must
        // not panic at this magnitude, and nothing banked drains on its own.
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
        assert_eq!(grand_total(Element::Wood), tailings, "Wood: still exactly conserved, nothing drains or vanishes");
        assert_eq!(grand_total(Element::Water), huge, "Water: still exactly conserved, nothing drains or vanishes");

        // A second, realistic-but-still-well-beyond-a-lifetime magnitude
        // (200,000 -- about 3x the bug report's own 68,000 example) stays
        // banked too, same as any other magnitude now -- the old "eventually
        // lands within one grid's retry ticks" liveness property no longer
        // holds for any magnitude, large or small.
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
        assert_eq!(w.terrain.overflow_of(race, Element::Fire), big - 50, "a banked shortfall stays banked -- nothing drains it anymore, at any magnitude");
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
        // Known hole from the action-recipe migration (`deposit_at`'s own
        // doc comment): nothing retries a banked shortfall anymore, even
        // once headroom reopens and the world keeps running.
        for _ in 0..(TERRAIN_PERIOD * 2) {
            w.step(&log);
        }

        assert_eq!(w.terrain.overflow_of(race, Element::Metal), 68_000 - 100, "nothing drains the bank on its own");
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

        // Action-recipe migration: items no longer deposit to terrain at
        // all (they populate `World::ground_items` instead), so only
        // `material`/`carried` compete for this cell's saturated headroom.
        let fire_total = material;
        let water_total = 5_000u64;
        let earth_total = 3_000;

        w.charge_death(race, pos, material, &carried, &items);

        assert_eq!(
            w.ground_items,
            vec![
                GroundItem { element: Element::Water, quantity: 1_200, pos },
                GroundItem { element: Element::Water, quantity: 800, pos },
                GroundItem { element: Element::Fire, quantity: 600, pos },
            ],
            "items land whole in ground_items, unaffected by terrain saturation"
        );

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

        // Known hole from the action-recipe migration (`deposit_at`'s own
        // doc comment): nothing retries a banked shortfall anymore, even
        // once headroom reopens and the world keeps running.
        w.terrain.cell_mut(x, y)[Element::Fire] = 0;
        w.terrain.cell_mut(x, y)[Element::Water] = 0;
        let log = InputLog::new();
        for _ in 0..TERRAIN_PERIOD {
            w.step(&log);
        }

        assert_eq!(w.terrain.overflow_of(race, Element::Fire), fire_banked, "nothing drains the Fire bank on its own");
        assert_eq!(w.terrain.overflow_of(race, Element::Water), water_banked, "nothing drains the Water bank on its own");
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
        assert_eq!(w.terrain.cell(x, y)[Element::Fire], 0, "bundled items no longer reach terrain directly");
        assert_eq!(
            w.ground_items,
            vec![GroundItem { element: Element::Fire, quantity: 9, pos }],
            "bundled item lands in ground_items instead"
        );
    }

    #[test]
    fn predation_death_drops_the_preys_carried_and_items_to_terrain_not_to_the_predator() {
        // Bug 1, the `phase_feeding` (predation) death path, and the design
        // decision `charge_death`'s doc comment documents: unlike
        // `material` ("you are what you eat"), a predator never inherits
        // its prey's `carried`/`items` -- both always fall to the ground at
        // the death position, the same as any other death.
        let mut w = World::new(3, 16);
        // Genesis seeds every cell with GENESIS_EARTH -- an unrelated
        // confound for this probe's exact-conservation arithmetic below.
        w.terrain.seed_uniform(Element::Earth, 0);
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
        assert_eq!(w.terrain.cell(x, y)[Element::Earth], 30, "prey's carried stock falls to terrain");
        assert_eq!(w.terrain.cell(x, y)[Element::Metal], 0, "prey's item no longer falls to terrain directly");
        assert_eq!(
            w.ground_items,
            vec![GroundItem { element: Element::Metal, quantity: 6, pos }],
            "prey's item lands in ground_items instead"
        );
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
        // Genesis seeds every cell with GENESIS_EARTH -- an unrelated
        // confound for the exact-zero "no phantom Earth deposit" check below.
        w.terrain.seed_uniform(Element::Earth, 0);
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

    // ------------------------------------------------------------------
    // Action-recipe system: Exist auto-fire, Pickup, ground-item decay. No
    // shipped race carries a Pickup recipe yet, so those tests retune one in.
    // ------------------------------------------------------------------

    #[test]
    fn exist_grows_body_material_once_per_terrain_tick_for_races_that_have_it() {
        let mut w = World::new(60, 8);
        let race = Race { element: Element::Fire, kind: Kind::Animal };
        let pos = V2::new(Fx::from_int(3), Fx::from_int(3));
        // Seed the whole grid, not just the spawn cell -- an Animal walks
        // over 100 ticks (`speed`/jitter), so a single-cell seed would leave
        // it on unseeded ground well before the first terrain tick fires.
        for y in 0..8i32 {
            for x in 0..8i32 {
                w.terrain.cell_mut(x, y)[Element::Fire.habitat()] = 20_000;
            }
        }
        let id = w.spawn(race, pos);

        let log = InputLog::new();
        for _ in 0..TERRAIN_PERIOD {
            w.step(&log);
        }

        let e = w.entities.iter().find(|e| e.id == id).unwrap();
        assert!(e.material > 0, "Exist should have grown this body's own material by the first terrain tick");
    }

    #[test]
    fn a_race_with_no_exist_recipe_never_grows_material_from_existing() {
        // "No action, no effect" -- the rule the action-recipe migration
        // replaced the old unconditional `Conversion` mechanism with (see
        // race.rs's module doc).
        let mut w = World::new(61, 8);
        let race = Race { element: Element::Fire, kind: Kind::Animal };
        let mut races = w.races.clone();
        races[race].actions.retain(|a| a.slot != ActionSlot::Exist);
        w.retune(races);
        let pos = V2::new(Fx::from_int(3), Fx::from_int(3));
        for y in 0..8i32 {
            for x in 0..8i32 {
                w.terrain.cell_mut(x, y)[Element::Fire.habitat()] = 20_000;
            }
        }
        let id = w.spawn(race, pos);

        let log = InputLog::new();
        for _ in 0..TERRAIN_PERIOD {
            w.step(&log);
        }

        let e = w.entities.iter().find(|e| e.id == id).unwrap();
        assert_eq!(e.material, 0, "no Exist recipe means existence does nothing");
    }

    /// A race with a `Pickup` recipe retuned in — no shipped race carries
    /// one by default (this migration's design leaves it ready for a future
    /// race to opt in, see race.rs's module doc).
    fn race_with_pickup(w: &World, race: Race, reach: Fx) -> PerRace<RaceAttrs> {
        let mut races = w.races.clone();
        races[race].actions.push(ActionRecipe {
            slot: ActionSlot::Pickup,
            input: RecipeSlot::Ground,
            output: RecipeSlot::Carried,
            transform: ElementTransform::Identity,
            ratio_in: 1,
            ratio_out: 1,
            rate: RateLaw::Flat(u16::MAX),
            cooldown_ticks: 0,
            reach,
        });
        races
    }

    #[test]
    fn pickup_draws_from_ground_items_within_reach_and_ignores_out_of_reach() {
        let mut w = World::new(62, 16);
        let race = Race { element: Element::Fire, kind: Kind::Animal };
        let races = race_with_pickup(&w, race, Fx::ONE);
        w.retune(races);
        let pos = V2::new(Fx::from_int(5), Fx::from_int(5));
        let id = w.spawn(race, pos);
        w.ground_items.push(GroundItem { element: Element::Water, quantity: 40, pos });
        w.ground_items.push(GroundItem {
            element: Element::Water,
            quantity: 999,
            pos: V2::new(Fx::from_int(15), Fx::from_int(15)), // well outside reach
        });

        let occ = Occupancy::build(&w.entities, &w.terrain);
        w.apply_action_recipe(id, ActionSlot::Pickup, Element::Water, &occ);

        let e = w.entities.iter().find(|e| e.id == id).unwrap();
        assert_eq!(e.carried[Element::Water], 40, "picked up the whole in-reach pile");
        let still_on_ground: u64 = w.ground_items.iter().filter(|g| g.element == Element::Water).map(|g| g.quantity).sum();
        assert_eq!(still_on_ground, 999, "the out-of-reach pile is untouched");
    }

    #[test]
    fn pickup_is_a_noop_with_nothing_in_reach() {
        let mut w = World::new(63, 16);
        let race = Race { element: Element::Fire, kind: Kind::Animal };
        let races = race_with_pickup(&w, race, Fx::ONE);
        w.retune(races);
        let pos = V2::new(Fx::from_int(5), Fx::from_int(5));
        let id = w.spawn(race, pos);

        let occ = Occupancy::build(&w.entities, &w.terrain);
        w.apply_action_recipe(id, ActionSlot::Pickup, Element::Water, &occ);

        let e = w.entities.iter().find(|e| e.id == id).unwrap();
        assert_eq!(e.carried[Element::Water], 0);
    }

    #[test]
    fn ground_items_decay_back_into_terrain_at_a_bounded_rate() {
        let mut w = World::new(64, 8);
        let pos = V2::new(Fx::from_int(3), Fx::from_int(3));
        w.ground_items.push(GroundItem { element: Element::Wood, quantity: 10_000, pos });

        let log = InputLog::new();
        for _ in 0..TERRAIN_PERIOD {
            w.step(&log);
        }

        let remaining: u64 = w.ground_items.iter().filter(|g| g.element == Element::Wood).map(|g| g.quantity).sum();
        assert!(remaining < 10_000, "one terrain tick of decay should have moved something back to terrain");
        let (x, y) = w.terrain.cell_of(pos);
        let landed = w.terrain.cell(x, y)[Element::Wood] as u64;
        assert_eq!(landed + remaining, 10_000, "every unit is either landed on terrain or still on the ground -- none vanished");
    }
}
