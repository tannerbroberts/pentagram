//! The Stage 0 exit condition, as a runnable check.
//!
//!   cargo run --release --bin verify [ticks]

use pentagram::input::InputLog;
use pentagram::replay::{record, scripted_log, verify};

const SIZE: i32 = 64;
const PER_RACE: u32 = 12;

fn main() {
    let ticks: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000);

    println!("Pentagram — Stage 0 verification");
    println!("  ticks     {ticks}");
    println!("  territory {SIZE}×{SIZE} cells");
    println!("  seeded    {PER_RACE} per race\n");

    let log = scripted_log(0x5EED, ticks, PER_RACE * 5);
    println!("input log: {} commands", log.len());

    // 1 — the log must survive the trip it will make between territories.
    let shipped = match InputLog::from_bytes(&log.to_bytes()) {
        Ok(l) => l,
        Err(e) => {
            println!("FAIL  input log did not round-trip: {e:?}");
            std::process::exit(1);
        }
    };
    if shipped.as_slice() != log.as_slice() {
        println!("FAIL  input log changed across serialisation");
        std::process::exit(1);
    }
    println!("  [ok] log round-trips through bytes");

    // 2 — record a per-tick trace.
    let trace = record(0xC0FFEE, SIZE, PER_RACE, &log, ticks);
    println!("  [ok] recorded {} tick hashes", trace.ticks());

    // 3 — replay from seed and log alone.
    match verify(&trace, &log) {
        Ok(()) => println!("  [ok] replay is bit-identical at every tick"),
        Err(d) => {
            println!("FAIL  {d}");
            std::process::exit(1);
        }
    }

    // 4 — replay again from the deserialised log, which is the path a real
    //     territory would actually take.
    match verify(&trace, &shipped) {
        Ok(()) => println!("  [ok] replay from the shipped log agrees"),
        Err(d) => {
            println!("FAIL  shipped log diverges: {d}");
            std::process::exit(1);
        }
    }

    println!("\nfinal state hash  {:#018x}", trace.final_hash());
    println!("PASS — Stage 0 exit condition met");
}
