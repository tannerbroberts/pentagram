//! Invariant VIII (material conservation), proved end-to-end against a real
//! run rather than only by construction. `race::Conversion`,
//! `terrain::apply_conversion`, and `Entity.material`/`carried`/`items` are
//! all covered by their own unit tests already; this file's job is the one
//! those cannot do alone -- run the whole simulation, with growth, death,
//! mining, smelting, and item bundling all actually happening, and show the
//! books balance, the same way `tests/determinism.rs` proves its own
//! properties against a real `World::step` loop rather than a mock.
//!
//! A raw before/after equality per element would trivially fail the moment
//! any conversion fires -- that is the whole point of a conversion, moving
//! mass from one element's ledger to another's. So this test computes the
//! *expected* per-element delta by replicating, from the outside, the exact
//! arithmetic the two conversions in play use internally (the coupled
//! deposit/consume conversion, `terrain::apply_conversion`'s own doc
//! comment, and smelting, `World::smelt`'s), then asserts the observed delta
//! matches it exactly, element by element.
//!
//! Two mechanisms are deliberately neutralised for this run, both flagged in
//! their own comments below rather than silently avoided:
//!
//! - **Climate's ambient influx** (`climate.rs`) is a genuine, pre-existing,
//!   non-entity-mediated source term -- "weather, mineral seepage,
//!   background decay" -- that predates Invariant VIII and is explicitly out
//!   of its scope (the law governs terrain/body/item material that moves
//!   through *entity-mediated* transfers and conversions; climate is neither).
//!   Zeroed here so this test proves the closed part of the economy exactly,
//!   rather than also having to reconstruct climate's own saturating-add
//!   arithmetic to account for it.
//! - **Predation's reach** (`EcologyTuning::forage_radius`) is zeroed so no
//!   kill ever happens. Predation moves material too ("you are what you
//!   eat" -- prey's entire `material` transfers to the predator's, in full),
//!   but reconstructing *which* pairs would match this tick from outside
//!   `World::phase_feeding` would mean duplicating its own matching logic
//!   rather than testing against it. Growth-via-conversion, natural/
//!   starvation death, mining, smelting, and item bundling/breaking are all
//!   still fully live and exercised below.

use pentagram::element::{Element, PerElement};
use pentagram::fx::{Fx, V2};
use pentagram::input::{CmdKind, Command, InputLog};
use pentagram::race::{Kind, Race, TERRAIN_PERIOD};
use pentagram::world::{SMELT_RATIO_IN, SMELT_RATIO_OUT};
use pentagram::{ClimateTuning, EcologyTuning, World};

/// Every pool Invariant VIII's ledger actually covers, summed per element:
/// terrain stock, every living body's own held material, everything it
/// carries of other elements, and every item it holds. A transfer between
/// any two of these pools (mining, death, predation, make/break-item) is
/// arithmetically invisible to this sum by construction -- it only moves
/// within it -- so only an actual ring-conversion can change one of these
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
    total
}

#[test]
fn material_is_conserved_across_growth_death_mining_smelting_and_items() {
    let mut w = World::new(0xBEEF, 24);

    w.retune_climate(ClimateTuning {
        floor: PerElement::filled(0),
        season_peak: PerElement::filled(0),
        ..ClimateTuning::default()
    });
    w.retune_ecology(EcologyTuning { forage_radius: PerElement::filled(Fx::ZERO), ..EcologyTuning::default() });

    // Seed every element generously across the whole grid -- terrain starts
    // all-zero and climate is off above, so without this there is nothing
    // to mine, and (more importantly for the accounting below) every race's
    // habitat draw would be starved at the start of an otherwise-ordinary
    // run. `terrain::apply_conversion` correctly caps a race's produced
    // output at whatever its occupied cells actually hold (a real,
    // freshly-fixed edge case -- see `apply_conversion`'s own doc comment
    // and `terrain::tests::apply_conversion_caps_production_at_the_actual_
    // available_habitat_stock`), but replicating *that* cap from outside
    // `Occupancy`'s private cell weighting is its own separate test, not
    // this one. This run stays deliberately clear of the cap entirely: 20
    // 000 per cell per element is far beyond any race's governed
    // per-settlement ceiling (at most a few thousand, even summed across
    // this run's three settlements), so every conversion here always gets
    // exactly what it asks for, and the simple external replication below
    // matches `apply_conversion`'s arithmetic exactly.
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
    // Wood-Animal mines Water three times (mining_rate=40/race, well under
    // the seeded 40 000).
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
            // the carried stock the scripted Smelt command below is about to
            // see -- replicate `World::smelt`'s own batch arithmetic (its
            // own doc comment) to know the exact expected cross-element
            // delta rather than merely that one exists.
            let idx = w.entities.iter().position(|e| e.id == wood_animal).expect("wood_animal still alive at tick 40");
            let have = w.entities[idx].carried[Element::Water];
            let batches = have / SMELT_RATIO_IN;
            let produced = batches * SMELT_RATIO_OUT;
            expected_delta[Element::Water] -= produced as i64;
            expected_delta[Element::Water.generates()] += produced as i64;
        }

        w.step(&log);

        // A terrain tick just settled and `apply_conversion` ran inside this
        // same `step` call -- `last_consume` is this tick's freshly granted
        // habitat draw per race. Replicate `apply_conversion`'s own
        // documented arithmetic exactly (batches = N / ratio_in, produced =
        // batches * ratio_out, net habitat removed = produced) so the
        // expected delta is exact rather than approximate.
        if w.tick % TERRAIN_PERIOD == 0 && w.tick > 0 {
            for race in Race::ALL {
                let n = w.last_consume[race].granted;
                if n == 0 {
                    continue;
                }
                let conv = w.races[race].conversion;
                let batches = n / conv.ratio_in as u64;
                let produced = batches * conv.ratio_out as u64;
                if produced == 0 {
                    continue;
                }
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
            "{}: observed delta {} != expected {} accounting for every ring-conversion this run \
             (coupled deposit/consume conversion plus smelting)",
            e.name(),
            observed,
            expected_delta[e]
        );
        if expected_delta[e] != 0 {
            any_conversion = true;
        }
    }
    assert!(any_conversion, "test is vacuous -- no ring-conversion actually fired during the run");

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
