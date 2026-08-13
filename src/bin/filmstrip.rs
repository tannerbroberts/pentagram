//! Headless filmstrip capture — a replayable artifact, one PPM frame per cadence.
//!
//!   cargo run --release --bin filmstrip -- [seed] [size] [per_race] [ticks] [cadence] [outdir]

use pentagram::race::TICKS_PER_HOUR;
use pentagram::replay::{build, scripted_log};
use pentagram::terrain::render_ppm;

fn main() {
    let mut args = std::env::args().skip(1);
    let seed: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(0xBEEF);
    let size: i32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(96);
    let per_race: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(20);
    let ticks: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(4_320_000);
    let cadence: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(TICKS_PER_HOUR);
    let outdir: String = args.next().unwrap_or_else(|| "./filmstrip".to_string());

    std::fs::create_dir_all(&outdir).expect("create outdir");

    let log = scripted_log(seed, ticks, per_race * 5);
    let mut w = build(seed, size, per_race);

    let mut manifest = format!("seed={seed:#x} size={size} cadence={cadence}\n");
    let mut frame_index: u32 = 0;

    println!("filmstrip: seed={seed:#x} size={size} per_race={per_race} ticks={ticks} cadence={cadence} outdir={outdir}");

    for t in 0..ticks {
        // Capture at tick 0 (the initial state) and every cadence-th tick
        // after, i.e. before that tick's step runs.
        if t % cadence == 0 {
            let frame = render_ppm(&w.terrain);
            let path = format!("{outdir}/frame_{frame_index:06}.ppm");
            std::fs::write(&path, &frame).expect("write frame");
            manifest.push_str(&format!("{frame_index:06} tick={t}\n"));
            if frame_index.is_multiple_of(60) {
                println!("  frame {frame_index:06}  tick {t}/{ticks}");
            }
            frame_index += 1;
        }
        w.step(&log);
    }

    std::fs::write(format!("{outdir}/manifest.txt"), &manifest).expect("write manifest");
    println!("done: {frame_index} frames written to {outdir}");
}
