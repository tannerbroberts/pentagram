//! Invariant VIII (material conservation), proved end-to-end against a real
//! run rather than only by construction. The action-recipe system's own
//! pieces -- `race::ActionRecipe`, `World::apply_action_recipe`,
//! `Entity.material`/`carried`/`items` -- are all covered by their own unit
//! tests already; this file's job is the one those cannot do alone -- run
//! the whole simulation, with growth, death, mining, smelting, and item
//! bundling all actually happening, and show the books balance, the same
//! way `tests/determinism.rs` proves its own properties against a real
//! `World::step` loop rather than a mock.
//!
//! A raw before/after equality per element would trivially fail the moment
//! any conversion fires -- that is the whole point of a conversion, moving
//! mass from one element's ledger to another's. So this test computes the
//! *expected* per-element delta by replicating, from the outside, the exact
//! arithmetic the mechanisms in play use internally (`Exist`'s per-entity
//! habitat draw -- see `race.rs`'s module doc -- and `Smelt`'s carried-stock
//! conversion), then asserts the observed delta matches it exactly, element
//! by element.
//!
//! One mechanism is deliberately neutralised for this run, flagged in its
//! own comment below rather than silently avoided:
//!
//! - **Predation's reach** (`EcologyTuning::forage_radius`) is zeroed so no
//!   kill ever happens. Predation moves material too ("you are what you
//!   eat" -- prey's entire `material` transfers to the predator's, in full),
//!   but reconstructing *which* pairs would match this tick from outside
//!   `World::phase_feeding` would mean duplicating its own matching logic
//!   rather than testing against it. Growth-via-`Exist`, natural/starvation
//!   death, mining, smelting, and item bundling/breaking are all still
//!   fully live and exercised below.
//!
//! An always-on, per-cell, population-independent terrain influx mechanism
//! used to be a second, genuinely exogenous source term here -- material
//! entering the ledger with no entity-mediated transfer behind it,
//! explicitly out of Invariant VIII's scope. That mechanism has since been
//! torn out entirely, so that carve-out is gone too: every unit this test
//! tracks now traces to an explicit transfer or conversion, full stop, with
//! nothing left to neutralise.

use pentagram::element::{Element, PerElement};
use pentagram::entity::Item;
use pentagram::fx::{Fx, V2};
use pentagram::input::{CmdKind, Command, InputLog};
use pentagram::race::{ActionSlot, Kind, PerRace, Race, RateLaw, TERRAIN_PERIOD};
use pentagram::{EcologyTuning, World};

/// Every pool Invariant VIII's ledger actually covers, summed per element:
/// terrain stock, every living body's own held material, everything it
/// carries of other elements, every item it holds, and every item lying on
/// the ground independent of any entity (`World::ground_items`, populated by
/// `charge_death`'s item split). A transfer between any two of these pools
/// (mining, death, predation, make/break-item, pickup) is arithmetically
/// invisible to this sum by construction -- it only moves within it -- so
/// only an actual conversion (`Exist`, `Smelt`) can change one of these
/// per-element totals.
fn total_material(w: &World) -> PerElement<u64> {
    let mut total = PerElement::filled(0u64);
    for e in Element::ALL {
        total[e] += w.terrain.total(e);
    }
    for ent in &w.entities {
        if !ent.alive {
            continue; // defensive -- phase_reap already clears these by the time step() returns
        }
        total[ent.element] += ent.material;
        for e in Element::ALL {
            total[e] += ent.carried[e];
        }
        for item in &ent.items {
            total[item.element] += item.quantity;
        }
    }
    for g in &w.ground_items {
        total[g.element] += g.quantity;
    }
    total
}

#[test]
fn material_is_conserved_across_growth_death_mining_smelting_and_items() {
    let mut w = World::new(0xBEEF, 24);

    w.retune_ecology(EcologyTuning { forage_radius: PerElement::filled(Fx::ZERO), ..EcologyTuning::default() });

    // Seed every element generously across the whole grid -- World::new only
    // seeds Earth (GENESIS_EARTH), so without this there is nothing of any
    // other element to mine, and (more importantly for the accounting
    // below) every race's `Exist` habitat draw would be starved at the
    // start of an otherwise-ordinary run. `apply_action_recipe` correctly
    // caps a firing at whatever the entity's own cell actually holds, but
    // this run stays deliberately clear of that cap entirely: 20 000 per
    // cell per element is far beyond any shipped `Exist`/`Mine`/`Smelt`
    // recipe's per-firing rate (at most a couple thousand), so every
    // conversion here always gets exactly what it asks for, and the simple
    // external replication below matches `apply_action_recipe`'s arithmetic
    // exactly.
    for y in 0..24i32 {
        for x in 0..24i32 {
            for e in Element::ALL {
                w.terrain.cell_mut(x, y)[e] = 20_000;
            }
        }
    }

    let wood_animal = w.spawn(Race { element: Element::Wood, kind: Kind::Animal }, V2::new(Fx::from_int(2), Fx::from_int(2)));
    let earth_animal = w.spawn(Race { element: Element::Earth, kind: Kind::Animal }, V2::new(Fx::from_int(10), Fx::from_int(10)));
    // A mixed population beyond the two instrumented actors, so ordinary
    // growth and death are genuinely exercised across every race, not just
    // the scripted mine/smelt/item chain on two bodies.
    w.seed_population(3);

    let mut log = InputLog::new();
    // Wood-Animal mines Water three times (well under the seeded 20 000).
    for t in [5u64, 6, 7] {
        log.push(Command { tick: t, entity: wood_animal, kind: CmdKind::Mine { element: Element::Water } });
    }
    // Smelt whatever ended up carried at tick 40 -- read fresh from the
    // entity below rather than assumed, so this holds even if movement cost
    // it a cell boundary along the way.
    log.push(Command { tick: 40, entity: wood_animal, kind: CmdKind::Smelt { element: Element::Water } });
    // Bundle 20 of whatever carried Water remains into an item, then break
    // it straight back -- exercises both `MakeItem` and `BreakItem` as a
    // round trip. A no-op (neither created nor destroyed either way) if
    // fewer than 20 happen to remain, which the conservation check below
    // does not depend on either way.
    log.push(Command { tick: 45, entity: wood_animal, kind: CmdKind::MakeItem { element: Element::Water, quantity: 20 } });
    log.push(Command { tick: 46, entity: wood_animal, kind: CmdKind::BreakItem { index: 0 } });
    // Earth-Animal mines Fire, exercising Mine alone on a second race/element
    // pair with nothing downstream done to it.
    for t in [5u64, 6] {
        log.push(Command { tick: t, entity: earth_animal, kind: CmdKind::Mine { element: Element::Fire } });
    }
    log.finalize();

    let before = total_material(&w);
    let mut expected_delta: PerElement<i64> = PerElement::filled(0);

    // Three full settlement windows -- comfortably inside every race's
    // multi-thousand-tick starvation grace period (feeding is disabled
    // above, so nobody can eat, but nobody needs to starve within this
    // window either) and well past the scripted mine/smelt/item ticks.
    let ticks = TERRAIN_PERIOD * 3;
    for t in 0..ticks {
        if t == 40 {
            // `phase_commands` runs first inside `step`, so this is exactly
            // the carried stock the scripted Smelt command below is about
            // to see -- replicate `World::apply_action_recipe`'s own batch
            // arithmetic against Wood-Animal's shipped `Smelt` recipe, so
            // the exact expected cross-element delta is known rather than
            // merely that one exists.
            let idx = w.entities.iter().position(|e| e.id == wood_animal).expect("wood_animal still alive at tick 40");
            let have = w.entities[idx].carried[Element::Water];
            let smelt = w.races[Race { element: Element::Wood, kind: Kind::Animal }].action(ActionSlot::Smelt).unwrap();
            let batches = have / smelt.ratio_in as u64;
            let produced = batches * smelt.ratio_out as u64;
            expected_delta[Element::Water] -= produced as i64;
            expected_delta[Element::Water.generates()] += produced as i64;
        }

        w.step(&log);

        // A terrain tick just settled and every living entity's `Exist`
        // recipe (if it has one) just fired inside this same `step` call,
        // once per body, at whatever cell it occupies -- see
        // `World::phase_terrain`. Genesis-seeding every cell far above any
        // recipe's per-firing rate (this test's own setup above) means no
        // firing is ever capped by actual stock, so the exact count of
        // currently-living bodies per race (observable right now, since
        // `phase_reap` -- which runs after `phase_terrain` in the same
        // `step` -- only removes bodies, never changes who was alive when
        // `Exist` fired) is enough to replicate the aggregate delta exactly,
        // with no need to track individual positions.
        if w.tick % TERRAIN_PERIOD == 0 && w.tick > 0 {
            let mut living: PerRace<u64> = PerRace::filled(0);
            for e in &w.entities {
                if e.alive {
                    *living.get_mut(e.race()) += 1;
                }
            }
            for race in Race::ALL {
                let n = living[race];
                if n == 0 {
                    continue;
                }
                let Some(exist) = w.races[race].action(ActionSlot::Exist) else { continue };
                let rate = match exist.rate {
                    RateLaw::Flat(r) => r as u64,
                    RateLaw::NeighborScaled { .. } => panic!("test assumes every shipped Exist recipe is Flat"),
                };
                let batches_per_body = rate / exist.ratio_in as u64;
                let produced_per_body = batches_per_body * exist.ratio_out as u64;
                if produced_per_body == 0 {
                    continue;
                }
                let produced = produced_per_body * n;
                expected_delta[race.element.habitat()] -= produced as i64;
                expected_delta[race.element] += produced as i64;
            }
        }
    }

    let after = total_material(&w);

    let mut any_conversion = false;
    for e in Element::ALL {
        let observed = after[e] as i64 - before[e] as i64;
        assert_eq!(
            observed,
            expected_delta[e],
            "{}: observed delta {} != expected {} accounting for every conversion this run \
             (per-entity Exist draws plus smelting)",
            e.name(),
            observed,
            expected_delta[e]
        );
        if expected_delta[e] != 0 {
            any_conversion = true;
        }
    }
    assert!(any_conversion, "test is vacuous -- no conversion actually fired during the run");

    // Every mechanism active in this run is either a same-element transfer
    // (invisible to `total_material`'s per-element sum by construction) or a
    // ring-conversion that moves mass between exactly two elements' ledgers
    // without changing their sum -- so the five-element grand total must be
    // exactly conserved regardless of which specific conversions fired,
    // strictly stronger than (and a good sanity check on) the per-element
    // accounting above.
    let grand_before: u64 = Element::ALL.iter().map(|e| before[*e]).sum();
    let grand_after: u64 = Element::ALL.iter().map(|e| after[*e]).sum();
    assert_eq!(grand_before, grand_after, "grand total across all five elements must be exactly conserved");
}

/// Companion to the test above, closing a real gap an adversarial reviewer
/// found in *that* test's own scenario: `forage_radius` zeroed (no predation
/// ever happens) and a run window safely inside every race's starvation
/// grace period meant its whole run never produced a single death (see the
/// standalone `tests/rev9_deaths_probe.rs`, which asserts exactly that
/// against that test's own scenario, byte-for-byte). That made two of the
/// five Invariant VIII bugs this crate's adversarial review found --
/// including bug 1, the single most severe one -- structurally invisible to
/// the existing end-to-end conservation test, even though it is *the* test
/// whose whole job is to catch exactly this kind of gap.
///
/// This test forces both death paths to actually fire within its own run
/// window and checks the same kind of per-element ledger the primary test
/// above does:
///
/// - **Natural (old-age) death of a body holding nonzero `carried` and
///   `items`** (bug 1's `phase_aging` path) -- scripted deterministically by
///   setting `age` to one tick short of `lifespan` before the run starts, so
///   `phase_aging` kills it on this run's very first tick.
/// - **A three-body same-tick predation chain, X eats Y eats Z** (bug 5),
///   spawned with the array order deliberately opposite the causal chain --
///   see `world::tests::a_three_body_predation_chain_resolves_in_causal_
///   order_not_array_index`, which this mirrors, for exactly why that
///   ordering matters. `forage_radius` is widened to cover the whole grid
///   and `hunt_weight`/`satiation` are tuned so neither roll nor cooldown can
///   ever block it, so the chain resolves on this run's first
///   `phase_feeding` regardless of exactly where movement/jitter has put
///   anyone by then. The chain's own bodies also carry nonzero
///   `carried`/`items`, so bug 1's fix is exercised on the predation death
///   path too, not just the natural-death path above.
///
/// This run is only 10 ticks -- nowhere near a terrain-tick boundary
/// (`TERRAIN_PERIOD` = 100) -- so `phase_terrain` (and therefore every
/// shipped race's `Exist` recipe, whatever habitat stock genesis seeding
/// gave it) never fires at all, and `World::smelt` is never invoked either.
/// So the *only* mechanism in this run that ever moves a unit between two
/// different elements' `total_material` buckets is the predation chain's own
/// material transfer -- predation retypes a killed body's material to its
/// predator's own element (`Entity.material`'s own doc comment: "you are
/// what you eat"), so unlike natural death's carried/items (which fall to
/// terrain, or into `World::ground_items` for bundled items, at their own
/// unchanged element and are therefore invisible to the per-element sums), a
/// predation chain's material really does move mass from the prey's element
/// bucket to the predator's. `expected_delta` below is that one exact,
/// hand-computed transfer -- Fire and Earth each lose exactly what Z and Y
/// started with, Metal gains exactly their sum. A bug-5-style misrouted
/// chain would show up here as a wrong per-element delta even though the
/// *grand* total across all five elements would still balance (misrouted
/// mass is still mass, just typed wrong) -- which is exactly why this checks
/// every element individually, not only the grand total.
#[test]
fn material_is_conserved_through_a_natural_death_and_a_predation_chain() {
    let mut w = World::new(0xDEAD, 24);
    w.retune_ecology(EcologyTuning {
        // Comfortably larger than any body's per-tick movement (fastest
        // shipped Animal speed is 0.46/tick) could carry the three chain
        // members apart in a handful of ticks starting from one shared
        // position, but far smaller than the ~25-cell gap to the `dying`
        // body below -- so the chain always finds itself, and never
        // accidentally reaches across the map to prey on `dying` (which
        // Fire *would* otherwise be eligible to hunt: `Element::eats_animal`
        // puts Fire-eats-Wood on the same ring).
        forage_radius: PerElement::filled(Fx::from_int(6)),
        hunt_weight: PerRace::filled(1000), // the hunt-weight roll never blocks a hunt
        satiation: PerElement::filled(0),   // the satiation cooldown never blocks a hunt
        ..EcologyTuning::default()
    });

    // -- Natural (old-age) death holding nonzero carried/items. Bug 1's
    // `phase_aging` path. Positioned far from the predation chain below so
    // the two scenarios cannot interfere with each other. --
    let dying = w.spawn(Race { element: Element::Wood, kind: Kind::Animal }, V2::new(Fx::from_int(2), Fx::from_int(2)));
    {
        let idx = w.entities.iter().position(|e| e.id == dying).unwrap();
        w.entities[idx].carried[Element::Water] = 40;
        w.entities[idx].carried[Element::Fire] = 5;
        w.entities[idx].items.push(Item { element: Element::Metal, quantity: 17 });
        w.entities[idx].items.push(Item { element: Element::Earth, quantity: 3 });
        // `phase_aging` increments `age` by one before checking expiry, so
        // this dies of old age on this run's very first tick.
        w.entities[idx].age = w.entities[idx].lifespan - 1;
    }

    // -- Three-body same-tick predation chain: Metal eats Earth eats Fire
    // (`Element::eats_animal`'s ring). Spawned in the array order opposite
    // the causal chain -- Z (bottom of the chain) first, then X (top
    // predator), then Y (middle) -- so naive ascending-array-index
    // resolution would have X read Y's stale, pre-chain material. Bug 5's
    // fix must resolve this correctly regardless of that order. Z and Y
    // start with nonzero `material` (mirroring
    // `world::tests::a_three_body_predation_chain_resolves_in_causal_
    // order_not_array_index` exactly) so the chain's material transfer is
    // actually exercised -- an all-zero chain would make this whole check
    // vacuous. --
    let chain_pos = V2::new(Fx::from_int(20), Fx::from_int(20));
    let z_fire = w.spawn(Race { element: Element::Fire, kind: Kind::Animal }, chain_pos);
    let x_metal = w.spawn(Race { element: Element::Metal, kind: Kind::Animal }, chain_pos);
    let y_earth = w.spawn(Race { element: Element::Earth, kind: Kind::Animal }, chain_pos);
    {
        let zi = w.entities.iter().position(|e| e.id == z_fire).unwrap();
        w.entities[zi].material = 100;
        w.entities[zi].carried[Element::Wood] = 11;
        let yi = w.entities.iter().position(|e| e.id == y_earth).unwrap();
        w.entities[yi].material = 50;
        w.entities[yi].items.push(Item { element: Element::Water, quantity: 9 });
    }

    let before = total_material(&w);
    let mut expected_delta: PerElement<i64> = PerElement::filled(0);
    // The chain's material transfer, computed by hand: Z's 100 (Fire) and
    // Y's 50 (Earth) both end up retyped as X's own element (Metal) once the
    // chain fully resolves -- see this test's own doc comment above.
    expected_delta[Element::Fire] -= 100;
    expected_delta[Element::Earth] -= 50;
    expected_delta[Element::Metal] += 150;

    // No scripted commands -- every death here comes purely from the state
    // and tuning set up above.
    let empty_log = InputLog::new();

    // The old-age death and the predation chain both resolve on tick 0; run
    // a little further so `phase_reap` has unambiguously run and nothing is
    // left half-applied. Well short of a terrain-tick boundary (see this
    // test's own doc comment) -- `Exist`/`Smelt` never fire in this window.
    for _ in 0..10 {
        w.step(&empty_log);
    }

    // Both kinds of death actually happened -- otherwise this test would be
    // exactly the vacuous scenario it exists to replace.
    assert!(!w.entities.iter().any(|e| e.id == dying), "the old-age death must have actually reaped the body");
    assert!(!w.entities.iter().any(|e| e.id == z_fire), "Z must have been eaten (by Y) this run");
    assert!(!w.entities.iter().any(|e| e.id == y_earth), "Y must have been eaten (by X) this run");
    let x = w.entities.iter().find(|e| e.id == x_metal).expect("X is the surviving top predator");
    assert_eq!(x.material, 150, "X must inherit the full chain: its own 0 + Y's 50 + Z's 100");

    let after = total_material(&w);
    for e in Element::ALL {
        let observed = after[e] as i64 - before[e] as i64;
        assert_eq!(
            observed,
            expected_delta[e],
            "{}: observed delta {} != expected {} -- every unit of the natural death's carried/items and the \
             predation chain's material transfer must be traceable to a pool `total_material` sums over, with the \
             chain's cross-element retyping landing exactly on Metal",
            e.name(),
            observed,
            expected_delta[e]
        );
    }

    let grand_before: u64 = Element::ALL.iter().map(|e| before[*e]).sum();
    let grand_after: u64 = Element::ALL.iter().map(|e| after[*e]).sum();
    assert_eq!(grand_before, grand_after, "grand total across all five elements must be exactly conserved");
}
