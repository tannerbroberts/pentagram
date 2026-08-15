//! Long headless run with a per-race report. The beginning of the tuning
//! harness — the numbers printed here are the ones §8 wants plotted.
//!
//!   cargo run --release --bin soak [ticks] [per_race]

// Reporting only. Floats are fine for a throughput figure printed to a
// terminal and are forbidden everywhere the simulation can see them — the
// crate-wide lint is what made this exception visible, which is the point.
#![allow(clippy::float_arithmetic)]

use pentagram::race::{attrs, Race, TERRAIN_PERIOD};
use pentagram::replay::{build, scripted_log};

fn main() {
    let mut args = std::env::args().skip(1);
    let ticks: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(60_000);
    let per_race: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(20);

    let log = scripted_log(0x50AC, ticks, per_race * 10);
    let mut w = build(0xBEEF, 96, per_race);

    let start = std::time::Instant::now();
    for _ in 0..ticks {
        w.step(&log);
    }
    let elapsed = start.elapsed();

    let sim_minutes = ticks / TERRAIN_PERIOD;
    println!(
        "ran {ticks} ticks ({} simulated minutes) in {:.2}s — {:.0} ticks/s",
        sim_minutes,
        elapsed.as_secs_f64(),
        ticks as f64 / elapsed.as_secs_f64()
    );
    println!(
        "births {}  deaths {}  collisions {}  actions {}",
        w.stats.births, w.stats.deaths, w.stats.collisions, w.stats.actions
    );

    println!(
        "\n{:<14} {:>5} {:>9} {:>9} {:>9} {:>8} {:>9}",
        "race", "alive", "granted", "floor", "ceiling", "forced", "clipped"
    );
    println!("{}", "-".repeat(70));

    // `population()` aggregates by element across both kinds — S3.1 has no
    // per-kind population split yet — so the alive count printed here is
    // shared between a race's Plant and Animal row.
    let pop = w.population();
    for race in Race::ALL {
        // Invariant VIII: deposit's own governor/band is retired (see
        // `race::Conversion`'s doc comment) — this table now reports the
        // one remaining governor, which gates the habitat draw the
        // conversion runs on.
        let b = attrs(race).consume;
        let g = w.last_consume[race];
        println!(
            "{:<7}-{:<6} {:>5} {:>9} {:>9} {:>9} {:>8} {:>9}",
            race.element.name(),
            race.kind.name(),
            pop[race.element],
            g.granted,
            b.floor,
            b.ceiling,
            g.forced,
            g.clipped
        );
    }

    println!("\nlifespan / turnover");
    for race in Race::ALL {
        let a = attrs(race);
        println!(
            "  {:<6}-{:<6} {:>9} ticks  ({:>6} sim-min)   pressure {:>5}",
            race.element.name(),
            race.kind.name(),
            a.lifespan,
            a.lifespan / TERRAIN_PERIOD,
            a.terraform_pressure()
        );
    }

    println!("\nfinal state hash  {:#018x}", w.state_hash());
}
