//! Ad hoc diagnostic, not a shipped feature: does the generating-ring
//! "recovery relay" actually close, starting from `World::new`'s genesis
//! terrain (1000 Earth/cell, nothing else), under today's *unconditional*
//! `seed_population` (no habitat-based spawn gating -- deliberately left
//! that way, see the design conversation this follows up on)?
//!
//! `cargo run --release --example watch_relay -- [ticks] [per_race] [size] [sample_every_terrain_ticks]`

use pentagram::element::Element;
use pentagram::race::{Kind, Race, TERRAIN_PERIOD, TICKS_PER_DAY};
use pentagram::replay::build;
use pentagram::InputLog;

fn main() {
    let mut args = std::env::args().skip(1);
    let ticks: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(3_000_000);
    let per_race: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(20);
    let size: i32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(48);
    let sample_every_terrain_ticks: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(2_000);

    let mut w = build(0xBEEF, size, per_race);
    let log = InputLog::new();
    let sample_every = sample_every_terrain_ticks * TERRAIN_PERIOD;

    let mut seen_dominant: std::collections::BTreeSet<Element> = std::collections::BTreeSet::new();
    seen_dominant.insert(Element::Earth); // true from tick 0 by construction

    let wall_start = std::time::Instant::now();
    let mut t = 0u64;
    while t < ticks {
        w.step(&log);
        t += 1;
        if t % sample_every != 0 {
            continue;
        }

        let totals: Vec<(Element, u64)> = Element::ALL.into_iter().map(|e| (e, w.terrain.total(e))).collect();
        let dominant = totals.iter().max_by_key(|(_, v)| *v).unwrap().0;
        seen_dominant.insert(dominant);

        let pop = w.population();
        let animal_alive: u32 = Race::ALL.iter().filter(|r| r.kind == Kind::Animal).map(|r| pop[r.element]).sum();
        let plant_alive: u32 = Race::ALL.iter().filter(|r| r.kind == Kind::Plant).map(|r| pop[r.element]).sum();

        println!(
            "day {:>6.1}  dominant={:<5?}  totals[{}]  alive: animal={:<4} plant={:<4}  births={} deaths={}  seen {}/5 {:?}",
            t as f64 / TICKS_PER_DAY as f64,
            dominant,
            totals.iter().map(|(e, v)| format!("{e:?}={v}")).collect::<Vec<_>>().join(" "),
            animal_alive,
            plant_alive,
            w.stats.births,
            w.stats.deaths,
            seen_dominant.len(),
            seen_dominant,
        );

        if seen_dominant.len() == 5 {
            println!(
                "\n*** relay closed: all five elements have been dominant at least once, by day {:.1} ***",
                t as f64 / TICKS_PER_DAY as f64
            );
            break;
        }
    }

    let elapsed = wall_start.elapsed();
    println!(
        "\nran {t} sim ticks ({:.1} sim days) in {:.1}s wall -- {:.0} ticks/s",
        t as f64 / TICKS_PER_DAY as f64,
        elapsed.as_secs_f64(),
        t as f64 / elapsed.as_secs_f64()
    );
    if seen_dominant.len() < 5 {
        println!("did NOT see all five elements dominant within the run -- missing: {:?}",
            Element::ALL.into_iter().filter(|e| !seen_dominant.contains(e)).collect::<Vec<_>>());
    }
}
