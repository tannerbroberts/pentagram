use pentagram::element::PerElement;
use pentagram::fx::{Fx, V2};
use pentagram::race::{Kind, Race, TERRAIN_PERIOD};
use pentagram::{ClimateTuning, EcologyTuning, World};

#[test]
fn rev9_probe_conservation_test_scenario_has_zero_deaths() {
    let mut w = World::new(0xBEEF, 24);
    w.retune_climate(ClimateTuning { floor: PerElement::filled(0), season_peak: PerElement::filled(0), ..ClimateTuning::default() });
    w.retune_ecology(EcologyTuning { forage_radius: PerElement::filled(Fx::ZERO), ..EcologyTuning::default() });
    for y in 0..24i32 {
        for x in 0..24i32 {
            for e in pentagram::element::Element::ALL {
                w.terrain.cell_mut(x, y)[e] = 20_000;
            }
        }
    }
    let _wood_animal = w.spawn(Race { element: pentagram::element::Element::Wood, kind: Kind::Animal }, V2::new(Fx::from_int(2), Fx::from_int(2)));
    let _earth_animal = w.spawn(Race { element: pentagram::element::Element::Earth, kind: Kind::Animal }, V2::new(Fx::from_int(10), Fx::from_int(10)));
    w.seed_population(3);
    let empty_log = pentagram::input::InputLog::new();
    let ticks = TERRAIN_PERIOD * 3;
    let pop_before = w.entities.len();
    for _ in 0..ticks {
        w.step(&empty_log);
    }
    println!("deaths={} pop_before={} pop_after={}", w.stats.deaths, pop_before, w.entities.len());
    assert_eq!(w.stats.deaths, 0, "no deaths occurred in the conservation test's own window/scenario");
}
