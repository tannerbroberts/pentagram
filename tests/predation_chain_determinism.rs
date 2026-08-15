//! Determinism, proved against an adversarial predation-chain-plus-carried-
//! material scenario rather than `tests/determinism.rs`'s scripted-command
//! logs: three independently constructed `World`s, given identical initial
//! state (including nonzero `carried` stock -- so Invariant VIII's
//! terrain-overflow banking, not just entity fields, is part of what has to
//! agree), stepped in lockstep with no player input at all, and checked
//! bit-for-bit via `state_hash()` at every tick. The chain is spawned in an
//! order deliberately opposite its causal resolution (Z, X, Y for an X-eats-
//! Y-eats-Z chain) -- the same array-index-vs-causal-order hazard
//! `world::tests::a_three_body_predation_chain_resolves_in_causal_order_not_
//! array_index` and `tests/conservation.rs`'s own predation test both guard
//! against -- so this also stands as a determinism-flavoured companion to
//! those two, not merely a duplicate of `tests/determinism.rs`'s own
//! `three_independent_runs_agree`.

use pentagram::element::{Element, PerElement};
use pentagram::fx::{Fx, V2};
use pentagram::race::{Kind, PerRace, Race};
use pentagram::world::World;
use pentagram::EcologyTuning;

fn build_chain_world() -> World {
    let mut w = World::new(0x5EED, 32);
    w.retune_ecology(EcologyTuning {
        satiation: PerElement::filled(0),
        hunt_weight: PerRace::filled(1000),
        ..EcologyTuning::default()
    });
    let pos = V2::new(Fx::from_int(10), Fx::from_int(10));
    // Spawned Z, X, Y -- opposite the X-eats-Y-eats-Z causal chain.
    let z_id = w.spawn(Race { element: Element::Fire, kind: Kind::Animal }, pos);
    let x_id = w.spawn(Race { element: Element::Metal, kind: Kind::Animal }, pos);
    let y_id = w.spawn(Race { element: Element::Earth, kind: Kind::Animal }, pos);
    let z_idx = w.entities.iter().position(|e| e.id == z_id).unwrap();
    let x_idx = w.entities.iter().position(|e| e.id == x_id).unwrap();
    let y_idx = w.entities.iter().position(|e| e.id == y_id).unwrap();
    assert_eq!((z_idx, x_idx, y_idx), (0, 1, 2));
    w.entities[z_idx].material = 100;
    w.entities[y_idx].material = 50;
    w.entities[x_idx].material = 0;
    w.entities[z_idx].carried[Element::Wood] = 40;
    w.entities[y_idx].carried[Element::Water] = 20;
    w
}

#[test]
fn predation_chain_resolution_is_bit_identical_across_independent_runs() {
    let mut a = build_chain_world();
    let mut b = build_chain_world();
    let mut c = build_chain_world();

    // step() drives phase_feeding (among other phases) each tick; with
    // satiation=0 and hunt_weight=1000 the X-eats-Y-eats-Z chain resolves
    // on tick 1. Compare full state hashes (which include entity material/
    // carried/items and terrain.overflow) across three independently built
    // worlds run from identical initial state.
    let log = pentagram::input::InputLog::new();
    for _ in 0..50 {
        a.step(&log);
        b.step(&log);
        c.step(&log);
        assert_eq!(a.state_hash(), b.state_hash(), "diverged at tick {}", a.tick);
        assert_eq!(b.state_hash(), c.state_hash(), "diverged at tick {}", b.tick);
    }
}
