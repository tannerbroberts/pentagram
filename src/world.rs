//! The simulation.
//!
//! The tick order below is part of the specification, not an implementation
//! detail. Reordering any two phases changes results and therefore invalidates
//! every recorded replay — treat it the way you would treat a wire format.
//!
//! `commands → aging → movement → feeding → flora → terrain → reap` —
//! `movement` (tile-grid rewrite) is discrete tile-stepping with built-in
//! occupancy blocking (decide → resolve conflicts → apply), replacing the
//! old separate `movement`/`collisions` pair — a body simply cannot step
//! onto an already-occupied tile, so there is no longer a pairwise
//! push-apart pass to run after it. `feeding` (S2, `phase_feeding`) runs
//! after movement so predator and prey are compared at this tick's settled
//! positions. `flora` (S3.5, `phase_flora`) runs right after feeding, before
//! terrain, so it never grows `phase_terrain`'s own fixed operator sequence
//! into an extra slot. `terrain` (S1, extended by the action-recipe
//! migration) runs the fixed sequence described in `terrain.rs`'s own doc
//! comment — bounded diffusion, plus every living body's `Exist` action,
//! auto-fired once per terrain tick. The old `settle` phase (accumulating
//! per-race existence demand for the now-retired `Conversion`/`Governor`
//! pipeline) is gone: `Exist` reads terrain directly, per body, the same way
//! `Mine` already does, with no population-aggregate pre-settlement step to
//! run first.

use crate::behavior::{BehaviorTuning, Drive};
use crate::ecology::{EcologyTuning, PropagationTuning};
use crate::element::{Element, PerElement};
use crate::entity::{Entity, Item, MAX_HP};
use crate::fx::Fx;
use crate::governor::Governor;
use crate::hash::{Hashable, Hasher};
use crate::input::{CmdKind, Command, InputLog};
use crate::race::{ActionSlot, ElementTransform, Kind, PerRace, Race, RaceAttrs, RateLaw, RecipeSlot, TERRAIN_PERIOD};
use crate::rand::{rand_chance, rand_range, Channel};
use crate::terrain::{Occupancy, Terrain, TerrainTuning};
use crate::tile::{chebyshev_dist, Tile};

/// Max per-axis tile offset for an offspring spawned by `phase_feeding`, so a
/// cohort born from the same parent does not stack exactly on top of it.
pub const BIRTH_SCATTER: i32 = 2;

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
    /// The simulated square is `[0, size) × [0, size)` in tiles — always
    /// equal to `terrain.side`.
    pub size: i32,

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
    pub pos: Tile,
}

impl Hashable for GroundItem {
    fn hash_into(&self, h: &mut Hasher) {
        h.u8(self.element as u8).u64(self.quantity);
        self.pos.hash_into(h);
    }
}

/// Shared by `apply_action_recipe`'s `Ground`-slot arms — `Pickup`'s
/// proximity gate. Same Chebyshev-tile-distance shape `phase_feeding`'s own
/// reach check already uses.
#[inline]
fn near(a: Tile, b: Tile, reach: i32) -> bool {
    chebyshev_dist(a, b) <= reach
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
        // `size` is a plain tile count now, so the only floor is "at least
        // one cell" — `Terrain::new` clamps the same way internally, this
        // just keeps `World.size` in agreement with it.
        let size_cells = size_cells.max(1);
        let mut terrain = Terrain::new(size_cells);
        terrain.seed_uniform(Element::Earth, GENESIS_EARTH);
        let terrain_tuning = TerrainTuning::default();
        World {
            seed,
            tick: 0,
            entities: Vec::new(),
            next_id: 1,
            size: size_cells,
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
                self.spawn(r, Tile::new(x, y));
            }
        }
    }

    fn scatter(&self, salt: u32, axis: u32) -> i32 {
        crate::rand::rand_below(self.seed, u64::from(axis), salt, Channel::SpawnPlacement, self.size.max(1) as u32) as i32
    }

    pub fn spawn(&mut self, race: Race, at: Tile) -> u32 {
        self.spawn_sized(race, at, 1000)
    }

    /// Same as `World::spawn`, except the newborn's `size` (per-mille of
    /// full size) is set explicitly rather than defaulting to fully grown;
    /// only `phase_flora`'s plant offspring (S3.5) ever pass anything but
    /// 1000 -- see `Entity.size`'s own doc comment.
    pub fn spawn_sized(&mut self, race: Race, at: Tile, size: u16) -> u32 {
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

    fn clamp_to_bounds(&self, p: Tile) -> Tile {
        p.clamp(self.size)
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
            CmdKind::SetTarget { to } => {
                if let Some(i) = self.find(c.entity) {
                    self.entities[i].move_target = to;
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

        let pos = self.entities[i].pos;
        let idx = self.terrain.index(pos.x, pos.y) as u32;
        let neighbors = occ.count(race, idx);
        let size = self.entities[i].size;
        let requested = match recipe.rate {
            RateLaw::Flat(n) => n as u64,
            RateLaw::NeighborScaled { base, per_neighbor, per_size } => (base as u64)
                .saturating_add((per_neighbor as u64).saturating_mul(neighbors.saturating_sub(1) as u64))
                .saturating_add((per_size as u64).saturating_mul(size as u64) / 1000),
        };
        // Opt-in (`ActionRecipe.vitality_scaled`, off for every shipped
        // recipe): the same continuous hunger-nerf primitive
        // `phase_feeding`'s hunting edge uses, generalized to any recipe.
        let requested = if recipe.vitality_scaled {
            let hunger = self.entities[i].hunger;
            let scale = crate::ecology::vitality_scale(hunger, self.ecology.satiation[element]);
            requested * scale as u64 / 1000
        } else {
            requested
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
    fn recipe_stock(&self, i: usize, slot: RecipeSlot, element: Element, reach: i32) -> u64 {
        match slot {
            RecipeSlot::Terrain => {
                let pos = self.entities[i].pos;
                self.terrain.cell(pos.x, pos.y)[element] as u64
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
    fn recipe_withdraw(&mut self, i: usize, slot: RecipeSlot, element: Element, reach: i32, amount: u64) {
        match slot {
            RecipeSlot::Terrain => {
                let pos = self.entities[i].pos;
                let amt16 = amount.min(u16::MAX as u64) as u16;
                let c = self.terrain.cell_mut(pos.x, pos.y);
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
                        let stock = self.terrain.cell(e.pos.x, e.pos.y)[e.element];
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
    fn charge_death(&mut self, race: Race, pos: Tile, material: u64, carried: &PerElement<u64>, items: &[Item]) {
        self.stats.deaths += 1;
        crate::terrain::deposit_at(&mut self.terrain, race, race.element, material, pos);
        for (e, &amt) in carried.iter() {
            crate::terrain::deposit_at(&mut self.terrain, race, e, amt, pos);
        }
        for item in items {
            self.ground_items.push(GroundItem { element: item.element, quantity: item.quantity, pos });
        }
    }

    /// 3 — discrete tile-stepping movement with built-in occupancy blocking
    /// (the OSRS-style tile-grid rewrite). Replaces the old separate
    /// movement/collision pair entirely: a body simply cannot step onto an
    /// already-occupied tile, so there is no push-apart pass to run after
    /// this one. Three fully-materialized, textually separate passes —
    /// decide, resolve conflicts, apply — the same discipline
    /// `phase_feeding`'s own `eaten`/`fed`/`ate` scratch vectors already use,
    /// so no borrow of `self.entities` is ever held both immutably
    /// (deciding/resolving) and mutably (applying) at once.
    ///
    /// **Movement cadence.** Each eligible non-Plant entity banks
    /// `races[race].speed` into `Entity.move_accum` every tick (a
    /// Bresenham/DDA-style fractional-tile accumulator — pure `Fx`, no
    /// floats) and is eligible for one tile step this tick once that reaches
    /// `Fx::ONE`; the accumulator is clamped to a small ceiling (4 tiles'
    /// worth) purely as a defensive bound against a pathological >1-tile/tick
    /// tuning value, since every shipped race's speed keeps it far below
    /// that ceiling in practice. `Fx::ONE` is spent only on a step that
    /// actually lands — a step refused by occupancy blocking leaves the
    /// banked fraction untouched, so contention never silently throttles a
    /// race's tuned `speed` (an explicitly load-bearing ecology knob, see
    /// `race.rs`'s own doc comment on `speed`). Phase 1 applies at most one
    /// step per entity per tick even if more than one tile's worth is
    /// banked — a deliberate simplification (no shipped race can bank enough
    /// in realistic contention to need more), flagged for revisit only if
    /// soak-testing shows otherwise.
    ///
    /// **Occupancy blocking.** One `SpatialIndex` snapshot of pre-tick
    /// positions serves both Hunt-sensing (via `behavior::drive`) and the
    /// occupancy check — its CSR ranges answer "is this tile occupied," with
    /// the lowest-id occupant already first per cell, for free. Desired
    /// destinations are grouped by target tile via a sort over just this
    /// tick's movers (not a full-grid table — the map can be far larger than
    /// the live population); a tile the snapshot shows occupied blocks every
    /// mover that wants it, and a contested empty tile goes to the lowest
    /// entity id (Invariant IV — the same tie-break `phase_feeding`/Hunt
    /// sensing already use). A body vacating tile T1 for T2 while another
    /// wants to step into T1 is resolved against the *pre-tick* snapshot —
    /// the second body waits one tick rather than being cycle-resolved
    /// within the same tick. Deliberate phase-1 simplification, not solved
    /// here — flagged for revisit only if soak-testing shows it materially
    /// throttles movement. Full one-body-per-tile exclusion (including
    /// permanently-occupied Plant tiles) is a materially different regime
    /// from the old soft radius-based push-apart, especially at high
    /// population — expect emergent traffic-jam dynamics; this is the
    /// direct, intended consequence of tile-grid navigation, not a defect.
    fn phase_movement(&mut self) {
        let (seed, tick) = (self.seed, self.tick);
        let races = &self.races;
        let ecology = self.ecology;
        let behavior = self.behavior;
        let mut acted: PerRace<u64> = PerRace::filled(0);
        let (mut grazed, mut hunted, mut fled) = (0u64, 0u64, 0u64);

        let n = self.entities.len();
        // Snapshot of pre-tick positions — see this phase's own doc comment.
        let index = crate::terrain::SpatialIndex::build(&self.entities, &self.terrain);

        // Decide: every eligible entity's desired destination tile, derived
        // only from the frozen pre-tick snapshot above — never from another
        // entity's already-applied move this same tick (S3.4's own
        // snapshot-then-apply reasoning, unchanged by this rewrite).
        let mut eligible = vec![false; n];
        let mut desired: Vec<Option<Tile>> = vec![None; n];
        let mut drive_of: Vec<Option<Drive>> = vec![None; n];
        const MAX_BANKED: Fx = Fx::from_int(4);
        for i in 0..n {
            if !self.entities[i].alive || self.entities[i].kind == Kind::Plant {
                // Rooted: a structural skip. See
                // `docs/S3_ECOLOGY_LAYERS_DESIGN.md` §4.
                continue;
            }
            let race = self.entities[i].race();
            let accum = (self.entities[i].move_accum + races[race].speed).min(MAX_BANKED);
            self.entities[i].move_accum = accum;
            if accum < Fx::ONE {
                continue;
            }
            eligible[i] = true;

            let (d, target) =
                crate::behavior::drive(&self.entities, &self.terrain, &index, &ecology, &behavior, seed, tick, i);
            drive_of[i] = Some(d);
            if let Some(t) = target {
                let t = t.clamp(self.size);
                if t != self.entities[i].pos {
                    desired[i] = Some(t);
                }
            }
        }

        // Resolve conflicts: group this tick's movers by desired destination
        // tile via a sort (not a full-grid table — `side` can be far larger
        // than the live population), lowest id first within a group.
        let mut movers: Vec<(u32, u32, usize)> = Vec::new(); // (dest_idx, id, entity index)
        // Indexes both `desired` and `self.entities` by the same `i` — not a
        // single-collection iteration `enumerate()` would simplify.
        #[allow(clippy::needless_range_loop)]
        for i in 0..n {
            if let Some(t) = desired[i] {
                let dest_idx = self.terrain.index(t.x, t.y) as u32;
                movers.push((dest_idx, self.entities[i].id, i));
            }
        }
        movers.sort_unstable();
        let mut winner = vec![false; n];
        let mut k = 0;
        while k < movers.len() {
            let dest_idx = movers[k].0;
            let mut end = k + 1;
            while end < movers.len() && movers[end].0 == dest_idx {
                end += 1;
            }
            if !index.is_occupied(dest_idx) {
                // `movers` is sorted by `(dest_idx, id)`, so `movers[k]` is
                // this group's lowest id.
                winner[movers[k].2] = true;
            }
            k = end;
        }

        // Apply: only now does any borrow of `self.entities` become mutable.
        for i in 0..n {
            if !eligible[i] {
                continue;
            }
            if let Some(d) = drive_of[i] {
                match d {
                    Drive::Graze => grazed += 1,
                    Drive::Hunt => hunted += 1,
                    Drive::Flee => fled += 1,
                }
            }
            if !winner[i] {
                continue;
            }
            let race = self.entities[i].race();
            let from = self.entities[i].pos;
            let to = desired[i].expect("winner[i] implies desired[i] is Some");
            self.entities[i].pos = to;
            self.entities[i].facing = Tile::new((to.x - from.x).signum(), (to.y - from.y).signum());
            self.entities[i].move_accum -= Fx::ONE;
            self.entities[i].acted = true;
            *acted.get_mut(race) += 1;
        }

        self.stats.grazed += grazed;
        self.stats.hunted += hunted;
        self.stats.fled += fled;

        for (_, n) in acted.iter() {
            self.stats.actions += *n;
        }
    }

    /// 4 — feeding (S2). A body within `ecology.forage_radius` of prey it can
    /// pair against consumes it outright: the prey dies exactly as it would
    /// from age or starvation (`charge_death`), and the predator's `hp`
    /// rises and fires `OnConsume` — the channel every race's deposit/
    /// consume mix has carried a nonzero share for since Stage 0, with
    /// nothing to fire it until now. Pairing is now split by prey Kind
    /// (S3.8): an Animal grazes a same-element Plant (`Element::eats_plant`,
    /// the Kind-sibling's product) — gated by `hunger >= ecology.satiation`,
    /// a hard gate — or hunts a ring-adjacent Animal (`Element::eats_animal`,
    /// the original ring relation) — no hard satiation gate, `hunger`
    /// instead continuously nerfs the hunt-weight roll via
    /// `ecology::vitality_scale` (see this function body's own comment) — a
    /// Plant is never a predator either way. A meal that carries a body's
    /// `hp` up across `repro_threshold` spawns one offspring through the
    /// ordinary `World::spawn` path, so it charges `OnBirth` the same way a
    /// command-spawned or seeded body always has. Without grazing's
    /// `satiation` gate every predator in reach eats every single tick it
    /// can, which empirically collapses every prey population within a few
    /// hundred ticks. The shipped `EcologyTuning`
    /// defaults are, like `TerrainTuning`'s before it, a first guess for the
    /// live tuning loop — a uniform five-way
    /// predation ring is a hard balance problem, and nothing here promises
    /// the shipped numbers converge to a stable population on their own.
    ///
    /// Pairwise, broadphase-accelerated the same way `phase_movement`'s
    /// occupancy check is. At most one
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

        // Two scratch passes, the same decide-then-apply shape
        // `phase_movement`'s own `desired`/`winner` buffers use: decide
        // every outcome first, against a fixed snapshot of who is alive,
        // then apply — so which pairs are found never depends on the order
        // mutations happened to land in.
        let mut eaten = vec![false; n];
        let mut fed = vec![false; n];
        // Invariant VIII: which prey index (if any) each predator index
        // ate this tick, so the material transfer below knows the pairing
        // — `eaten`/`fed` alone only record booleans, not who-ate-whom.
        // Each predator eats at most one prey per tick (`fed[pred]` blocks
        // further matches within the same scan), so this is a genuine 1:1
        // mapping.
        let mut ate: Vec<Option<usize>> = vec![None; n];

        // Broadphase: this scan's result genuinely depends on visiting order
        // (a body can only feed once per tick, enforced by `eaten`/`fed`
        // flags checked mid-scan — see this function's own doc comment on
        // Invariant IV), so `query_ring`'s ascending-index guarantee is
        // load-bearing here, not just a nicety. Which of `(i, j)` ends up
        // `pred` isn't known until the Kind/element match below runs, so the
        // reach bound has to cover *either* direction: `max_forage`, the
        // largest `forage_radius` any element ships (already tiles, no
        // conversion needed), dominates whichever `ecology.forage_radius
        // [pred_el]` this pair actually resolves to.
        let index = crate::terrain::SpatialIndex::build(&self.entities, &self.terrain);
        let max_forage = ecology.forage_radius.iter().map(|(_, r)| *r).max().unwrap_or(0);

        for i in 0..n {
            if !self.entities[i].alive || eaten[i] {
                continue;
            }
            let pos = self.entities[i].pos;
            for j in index.query_ring(pos.x, pos.y, max_forage) {
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
                let pred_el = self.entities[pred].element;
                let d = chebyshev_dist(self.entities[prey].pos, self.entities[pred].pos);
                let reach = ecology.forage_radius[pred_el];
                if d > reach {
                    continue;
                }
                // S3.3, generalized: grazing (Plant prey) keeps its original
                // satiation gate — not an `hp`-below-max gate: a predator
                // that just ate ignores further prey in reach until
                // `hunger` has built back up. Gating on `hp < MAX_HP`
                // instead would deadlock the whole ring — every body spawns
                // below the cap on purpose (`Entity::spawn`), but would
                // still only need one meal to top out, after which it stops
                // hunting even though `hunger` (ticks since last meal) has
                // reset to zero and prey is still plentiful.
                //
                // Hunting (Animal prey) no longer hard-gates on satiation at
                // all — a body is never denied an attempt purely for being
                // under-satiated, only nerfed: `hunt_weight`'s roll is
                // scaled by `ecology::vitality_scale`, which ramps
                // continuously from 0 at `hunger == 0` up to `hunt_weight`'s
                // full value once `hunger` reaches `satiation`, exactly
                // matching the old gate's effective behavior at and above
                // the threshold while softening the hard wall below it.
                // Rolled on (seed, tick, predator id) only — never on prey
                // id — so the result is the same no matter which prey
                // candidate this predator is being tested against this tick
                // (required so a future Hunt-drive steering pass, S3.4, can
                // agree with this phase about which prey class a predator
                // will actually take).
                if self.entities[prey].kind == Kind::Animal {
                    let pred_race = self.entities[pred].race();
                    let scale = crate::ecology::vitality_scale(self.entities[pred].hunger, ecology.satiation[pred_el]);
                    let weight = (ecology.hunt_weight[pred_race] as u32 * scale as u32) / 1000;
                    if !rand_chance(self.seed, self.tick, self.entities[pred].id, Channel::Hunt, weight, 1000) {
                        continue;
                    }
                } else if self.entities[pred].hunger < ecology.satiation[pred_el] {
                    continue;
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

        let mut births: Vec<(Race, Tile, u32)> = Vec::new();
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
        // itself get to move or eat again until its own next tick. An
        // offspring is the same race as its parent — same element and same
        // kind.
        let (seed, tick) = (self.seed, self.tick);
        for (race, pos, parent_id) in births {
            let dx = rand_range(seed, tick, parent_id, Channel::Forage, -BIRTH_SCATTER, BIRTH_SCATTER + 1);
            let dy = rand_range(seed, tick, parent_id.wrapping_add(0x0F0D), Channel::Forage, -BIRTH_SCATTER, BIRTH_SCATTER + 1);
            self.spawn(race, pos.offset(dx, dy));
        }
    }

    /// 5 — plant reproduction (S3.5). Gated at the same terrain-tick
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

        let mut offspring: Vec<(Race, Tile)> = Vec::new();
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

            let max = propagation.dispersal[el];
            let dx = rand_range(self.seed, self.tick, e.id, Channel::Disperse, -max, max + 1);
            let dy = rand_range(self.seed, self.tick, e.id.wrapping_add(0x0D15), Channel::Disperse, -max, max + 1);
            let candidate = self.clamp_to_bounds(e.pos.offset(dx, dy));

            // S3.8: root_min gates on the candidate cell's stock of the
            // Plant's *habitat* element (what it draws down from terrain to
            // sustain itself), not its own element -- terrain rich in what
            // this plant consumes is good habitat for it.
            let stock = self.terrain.cell(candidate.x, candidate.y)[el.habitat()];
            if (stock as u32) < propagation.root_min[el] as u32 {
                self.stats.rooted_rejected += 1;
                continue;
            }
            let idx = self.terrain.index(candidate.x, candidate.y) as u32;
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

    /// 6 — the fixed-order operator sequence gated at a terrain-tick
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

    /// 7 — remove the dead. `retain` is order-preserving, so the id sort holds.
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
            .i32(self.size)
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
