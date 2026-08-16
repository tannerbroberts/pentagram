//! Long headless run with a per-race report. The beginning of the tuning
//! harness — the numbers printed here are the ones §8 wants plotted.
//!
//!   cargo run --release --bin soak [ticks] [per_race]

// Reporting only. Floats are fine for a throughput figure printed to a
// terminal and are forbidden everywhere the simulation can see them — the
// crate-wide lint is what made this exception visible, which is the point.
#![allow(clippy::float_arithmetic)]

use pentagram::race::{attrs, ActionSlot, Race, RateLaw, TERRAIN_PERIOD};
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

    println!("\n{:<14} {:>5} {:>10} {:>10} {:>10}", "race", "alive", "exist rate", "ratio in", "ratio out");
    println!("{}", "-".repeat(60));

    // `population()` aggregates by element across both kinds — S3.1 has no
    // per-kind population split yet — so the alive count printed here is
    // shared between a race's Plant and Animal row.
    let pop = w.population();
    for race in Race::ALL {
        // Action-recipe system: there is no more population-aggregate
        // governor to report (retired along with `race::Conversion`) — this
        // table reports the per-entity `Exist` recipe every race's own
        // existence dispatches through instead.
        let exist = w.races[race].action(ActionSlot::Exist);
        let rate = exist.map_or(0, |a| match a.rate {
            RateLaw::Flat(n) => n as i64,
            RateLaw::NeighborScaled { base, .. } => base as i64,
        });
        println!(
            "{:<7}-{:<6} {:>5} {:>10} {:>10} {:>10}",
            race.element.name(),
            race.kind.name(),
            pop[race.element],
            rate,
            exist.map_or(0, |a| a.ratio_in),
            exist.map_or(0, |a| a.ratio_out),
        );
    }

    println!("\nlifespan / turnover");
    for race in Race::ALL {
        let a = attrs(race);
        println!(
            "  {:<6}-{:<6} {:>9} ticks  ({:>6} sim-min)",
            race.element.name(),
            race.kind.name(),
            a.lifespan,
            a.lifespan / TERRAIN_PERIOD,
        );
    }

    println!("\nfinal state hash  {:#018x}", w.state_hash());
}
