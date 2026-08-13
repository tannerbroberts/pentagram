//! The simulation.
//!
//! The tick order below is part of the specification, not an implementation
//! detail. Reordering any two phases changes results and therefore invalidates
//! every recorded replay — treat it the way you would treat a wire format.
//!
//! `commands → aging → movement → collisions → feeding → settle → terrain →
//! reap` — `feeding` (S2, `phase_feeding`) runs after collisions so predator
//! and prey are compared at this tick's settled positions, and before settle
//! so a kill's `OnDeath` demand and a meal's `OnConsume` demand both land in
//! the same terrain tick's settlement as everything else that happened this
//! tick. `terrain` (S1) runs the six fixed-order operators described in
//! `docs/S1_TERRAIN_DESIGN.md` and `terrain.rs`'s own doc comment.

use crate::behavior::{BehaviorTuning, Drive};
use crate::climate::{Climate, ClimateTuning};
use crate::ecology::EcologyTuning;
#[cfg(test)]
use crate::element::Element;
use crate::element::PerElement;
use crate::entity::{Entity, ACTION_THRESHOLD, MAX_HP};
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

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Stats {
    pub births: u64,
    pub deaths: u64,
    pub collisions: u64,
    pub actions: u64,
    /// Total demand refused by the deposit governors. A rising value means
    /// somebody is pushing on a rate limit.
    pub deposit_clipped: u64,
    /// Total emitted purely to honour a floor. A rising value means a race is
    /// absent or idle and the world is turning over without it.
    pub deposit_forced: u64,
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

    /// S1: the terrain field and the tuning tables its six operators read.
    /// Covered by [`World::state_hash`] the same way `races` is — a
    /// retuned world must not hash the same as an untuned one.
    pub terrain: Terrain,
    pub terrain_tuning: TerrainTuning,
    pub climate: Climate,
    pub climate_tuning: ClimateTuning,

    /// S2: feeding/starvation/reproduction rates. Covered by
    /// [`World::state_hash`] the same way `terrain_tuning` is.
    pub ecology: EcologyTuning,

    /// S3.4: the animal FSM's rate/reach knobs. Covered by
    /// [`World::state_hash`] the same way `ecology` is.
    pub behavior: BehaviorTuning,

    deposit_gov: PerRace<Governor>,
    consume_gov: PerRace<Governor>,
    /// Accumulated in milli-units between terrain ticks.
    deposit_demand: PerRace<u64>,
    consume_demand: PerRace<u64>,

    pub last_deposit: PerRace<Grant>,
    pub last_consume: PerRace<Grant>,
    pub stats: Stats,
}

impl World {
    pub fn new(seed: u64, size_cells: i32) -> World {
        // `size` is an `Fx`, which saturates past `i32::MAX >> Fx::SHIFT`
        // cells — clamping here, once, before it reaches `Fx::from_int`,
        // `Terrain::new` or `Climate::new` keeps all three in agreement.
        // Leaving `Terrain`/`Climate` free to construct at a raw, unclamped
        // size that `Fx` would have silently shrunk would desync the
        // terrain grid from the entity coordinate space the design's 1:1
        // resolution decision depends on.
        let size_cells = size_cells.clamp(1, i32::MAX >> crate::fx::SHIFT);
        let climate_tuning = ClimateTuning::default();
        World {
            seed,
            tick: 0,
            entities: Vec::new(),
            next_id: 1,
            size: Fx::from_int(size_cells),
            races: RACES,
            terrain: Terrain::new(size_cells),
            terrain_tuning: TerrainTuning::default(),
            climate: Climate::new(seed, size_cells, &climate_tuning),
            climate_tuning,
            ecology: EcologyTuning::default(),
            behavior: BehaviorTuning::default(),
            deposit_gov: PerRace(Race::ALL.map(|r| Governor::new(attrs(r).deposit))),
            consume_gov: PerRace(Race::ALL.map(|r| Governor::new(attrs(r).consume))),
            deposit_demand: PerRace::filled(0),
            consume_demand: PerRace::filled(0),
            last_deposit: PerRace::default(),
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
    pub fn retune(&mut self, races: PerRace<RaceAttrs>) {
        self.races = races;
        for r in Race::ALL {
            self.deposit_gov.get_mut(r).set_band(races[r].deposit);
            self.consume_gov.get_mut(r).set_band(races[r].consume);
        }
    }

    /// Swap the terrain operators' tuning table on a running world. A
    /// straight field replacement — unlike the race governors, none of the
    /// six operators carry internal state that needs reconciling.
    pub fn retune_terrain(&mut self, terrain_tuning: TerrainTuning) {
        self.terrain_tuning = terrain_tuning;
    }

    /// Swap the climate tuning table. The static geography cache is a pure
    /// function of `(seed, side, tuning.base_range)`, so it is rebuilt here
    /// rather than left stale — otherwise a retuned `base_range` would
    /// silently disagree with the cache still baked from the old one.
    pub fn retune_climate(&mut self, climate_tuning: ClimateTuning) {
        self.climate = Climate::new(self.seed, self.terrain.side, &climate_tuning);
        self.climate_tuning = climate_tuning;
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
        let id = self.next_id;
        self.next_id += 1;
        let a = self.races[race];
        let e = Entity::spawn(id, race.element, self.clamp_to_bounds(at), self.seed, self.tick, &a);
        // Ids are handed out ascending, so pushing preserves the sort.
        self.entities.push(e);
        self.stats.births += 1;
        *self.deposit_demand.get_mut(race) += a.deposit_per(DepChannel::OnBirth);
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
        }
    }

    /// 2 — age every body, drain `hp` for anyone past their starvation grace
    /// period (S2), and mark the expired ones. Death demand is charged here
    /// so a body that dies this tick still contributes its corpse.
    fn phase_aging(&mut self) {
        let ecology = self.ecology;
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
                    if e.is_expired() || e.hp <= 0 {
                        e.alive = false;
                        Some((e.race(), starving))
                    } else {
                        None
                    }
                }
            };
            if let Some((race, starving)) = dead {
                if starving {
                    self.stats.starved += 1;
                }
                self.charge_death(race);
            }
        }
    }

    /// Charge the `OnDeath` demand for one body's race and count the
    /// death. Shared by natural/starvation death (`phase_aging`) and
    /// predation (`phase_feeding`) — a corpse terraforms the same way
    /// regardless of what ended the body.
    fn charge_death(&mut self, race: Race) {
        let a = self.races[race];
        self.stats.deaths += 1;
        *self.deposit_demand.get_mut(race) += a.deposit_per(DepChannel::OnDeath);
        *self.consume_demand.get_mut(race) += a.consume_per(DepChannel::OnDeath);
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
                *self.deposit_demand.get_mut(race) += races[race].deposit_per(DepChannel::OnAction) * *n;
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
                let min = self.races[a_race].radius + self.races[b_race].radius;
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
    /// and which is within `ecology.forage_radius` of prey on the ring edge
    /// it eats (`Element::eats`) consumes it outright: the prey dies exactly
    /// as it would from age or starvation (`charge_death`), and the
    /// predator's `hp` rises and fires `OnConsume` — the channel every
    /// race's deposit/consume mix has carried a nonzero share for since
    /// Stage 0, with nothing to fire it until now. A meal that carries a
    /// body's `hp` up across `repro_threshold` spawns one offspring through
    /// the ordinary `World::spawn` path, so it charges `OnBirth` the same
    /// way a command-spawned or seeded body always has. Without the
    /// `satiation` gate every predator in reach eats every single tick it
    /// can, which empirically collapses every prey population within a few
    /// hundred ticks. The shipped `EcologyTuning` defaults are, like
    /// `TerrainTuning`'s and `ClimateTuning`'s before them, a first guess
    /// for the live tuning loop — a uniform five-way predation ring is a
    /// hard balance problem, and nothing here promises the shipped numbers
    /// converge to a stable population on their own.
    ///
    /// Pairwise, same O(n²) shape as `phase_collisions` and for the same
    /// reason — correct and fast enough here, and a future uniform-grid
    /// broadphase would want to serve both passes at once.
    /// `element.rs`'s ring arithmetic guarantees at most one direction of any
    /// pair can be a predation match (no element eats its own eater — see
    /// `nothing_beats_itself_and_the_two_edges_never_coincide`), so every
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

        for i in 0..n {
            if !self.entities[i].alive || eaten[i] {
                continue;
            }
            for j in (i + 1)..n {
                if !self.entities[j].alive || eaten[j] {
                    continue;
                }
                // Pairing derivation is still purely element-based (the ring
                // edge, `Element::eats`) — Kind only gates *who is allowed to
                // be the predator side* of that edge, below.
                let a = self.entities[i].element;
                let b = self.entities[j].element;
                let (pred, prey) = if a.eats() == b {
                    (i, j)
                } else if b.eats() == a {
                    (j, i)
                } else {
                    continue;
                };
                // A Plant is never a predator. Animal-vs-Animal predation
                // additionally rolls a per-race hunt-weight gate below
                // (S3.3) — grazing Plant prey stays fully unconditional. See
                // `docs/S3_ECOLOGY_LAYERS_DESIGN.md` §5.
                if self.entities[pred].kind != Kind::Animal {
                    continue;
                }
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
            *self.deposit_demand.get_mut(race) += races[race].deposit_per(DepChannel::OnConsume);
            *self.consume_demand.get_mut(race) += races[race].consume_per(DepChannel::OnConsume);

            if before < ecology.repro_threshold[el] && after >= ecology.repro_threshold[el] {
                births.push((race, self.entities[i].pos, self.entities[i].id));
            }
        }

        for (i, &was_eaten) in eaten.iter().enumerate() {
            if was_eaten {
                let race = self.entities[i].race();
                self.entities[i].alive = false;
                self.entities[i].hp = 0;
                self.charge_death(race);
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

    /// 6 — at a terrain-tick boundary, charge existence and settle every
    /// governor. This is the only place demand becomes terrain change.
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
                *self.deposit_demand.get_mut(race) +=
                    races[race].deposit_per(DepChannel::OnExistence) * *n;
                *self.consume_demand.get_mut(race) +=
                    races[race].consume_per(DepChannel::OnExistence) * *n;
            }
        }

        for race in Race::ALL {
            let d = self.deposit_demand[race] / MILLI;
            let grant = self.deposit_gov.get_mut(race).settle(d);
            self.stats.deposit_clipped += grant.clipped;
            self.stats.deposit_forced += grant.forced;
            self.last_deposit[race] = grant;
            self.deposit_demand[race] = 0;

            let c = self.consume_demand[race] / MILLI;
            self.last_consume[race] = self.consume_gov.get_mut(race).settle(c);
            self.consume_demand[race] = 0;
        }
    }

    /// 7 — the six fixed-order operator slots gated at the same terrain-tick
    /// boundary `phase_settle` uses so every operator sees this tick's
    /// freshly computed `last_deposit`/`last_consume` grants. See
    /// `docs/S1_TERRAIN_DESIGN.md` and `terrain.rs`'s own doc comment for why
    /// this exact order — deposit, consume, attrition, suppression, climate,
    /// diffusion — is a wire format, not a stylistic choice. Slots 3 and 4
    /// used to be terrain's own `ring`/`star` operators, converting and
    /// nullifying stock with no entity involved at all; terrain isn't its
    /// own actor, so those slots are now `ecology::apply_attrition`/
    /// `apply_suppression` — the same ring/star *relations*, now read from
    /// terrain and applied to the bodies standing in it.
    fn phase_terrain(&mut self) {
        if !(self.tick + 1).is_multiple_of(TERRAIN_PERIOD) {
            return;
        }
        let terrain_tick = (self.tick + 1) / TERRAIN_PERIOD;
        let occ = Occupancy::build(&self.entities, &self.terrain);
        crate::terrain::apply_deposit(&mut self.terrain, &occ, &self.last_deposit, self.seed, terrain_tick);
        crate::terrain::apply_consume(&mut self.terrain, &occ, &self.last_consume, self.seed, terrain_tick);
        crate::ecology::apply_attrition(&mut self.entities, &self.terrain, &self.ecology);
        crate::ecology::apply_suppression(&mut self.entities, &self.terrain, &self.ecology);
        crate::climate::apply_influx(&mut self.terrain, &self.climate, &self.climate_tuning, terrain_tick);
        crate::terrain::apply_diffusion(&mut self.terrain, &self.terrain_tuning);
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
        // the same as an untuned one — otherwise `retune`/`retune_terrain`/
        // `retune_climate` would be a silent divergence.
        for (_, a) in self.races.iter() {
            a.hash_into(&mut h);
        }
        self.terrain_tuning.hash_into(&mut h);
        self.climate_tuning.hash_into(&mut h);
        self.ecology.hash_into(&mut h);
        self.behavior.hash_into(&mut h);
        for (_, g) in self.deposit_gov.iter() {
            g.hash_into(&mut h);
        }
        for (_, g) in self.consume_gov.iter() {
            g.hash_into(&mut h);
        }
        for (_, d) in self.deposit_demand.iter() {
            h.u64(*d);
        }
        for (_, d) in self.consume_demand.iter() {
            h.u64(*d);
        }
        for (_, g) in self.last_deposit.iter() {
            g.hash_into(&mut h);
        }
        for (_, g) in self.last_consume.iter() {
            g.hash_into(&mut h);
        }
        h.u64(self.stats.births)
            .u64(self.stats.deaths)
            .u64(self.stats.collisions)
            .u64(self.stats.actions)
            .u64(self.stats.deposit_clipped)
            .u64(self.stats.deposit_forced)
            .u64(self.stats.feedings)
            .u64(self.stats.starved)
            .u64(self.stats.grazed)
            .u64(self.stats.hunted)
            .u64(self.stats.fled);
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
    fn governors_always_grant_inside_their_band() {
        let mut w = world();
        let log = InputLog::new();
        for _ in 0..4000 {
            w.step(&log);
            for race in Race::ALL {
                let b = attrs(race).deposit;
                let g = w.last_deposit[race];
                if g.granted == 0 {
                    continue; // before the first settlement
                }
                assert!(
                    g.granted >= b.floor as u64 && g.granted <= b.ceiling as u64,
                    "{}-{} granted {} outside [{}, {}]",
                    race.element.name(),
                    race.kind.name(),
                    g.granted,
                    b.floor,
                    b.ceiling
                );
            }
        }
    }

    #[test]
    fn an_extinct_race_still_churns_its_terrain() {
        // Only Earth-Animal exists. Every other race — including Earth-Plant
        // — must still be granted its floor, which is what stops a lost
        // biome becoming an absorbing state.
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
                w.last_deposit[race].granted,
                attrs(race).deposit.floor as u64,
                "{}-{} should be churning at its floor",
                race.element.name(),
                race.kind.name()
            );
            assert!(w.last_deposit[race].forced > 0);
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

    // S3.0: `retune_ecology`/`retune_terrain`/`retune_climate` each already
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
        // Fire eats Wood. Spawn a satiated, in-reach Fire::Animal predator
        // against BOTH a Wood::Animal and a Wood::Plant, with hunt_weight
        // zeroed for every Animal race — the Animal prey must survive every
        // tick (the roll can never succeed at weight 0), while the Plant
        // prey, ungated, still gets grazed.
        let mut w = World::new(20, 32);
        w.retune_ecology(EcologyTuning { hunt_weight: PerRace::filled(0), ..EcologyTuning::default() });
        let center = V2::new(Fx::from_int(16), Fx::from_int(16));
        let pred_id = w.spawn(Race { element: Element::Fire, kind: Kind::Animal }, center);
        let prey_animal_id = w.spawn(Race { element: Element::Wood, kind: Kind::Animal }, center);
        w.spawn(Race { element: Element::Wood, kind: Kind::Plant }, center);
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
            !w.entities.iter().any(|e| e.element == Element::Wood && e.kind == Kind::Plant),
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
}
