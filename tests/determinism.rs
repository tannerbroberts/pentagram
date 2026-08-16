//! The Stage 0 exit condition, and the properties that would make it a lie if
//! they failed quietly.

use pentagram::input::InputLog;
use pentagram::race::TERRAIN_PERIOD;
use pentagram::replay::{build, record, scripted_log, verify};

const SEED: u64 = 0xC0FFEE;
const SIZE: i32 = 64;
const PER_RACE: u32 = 12;

/// **The exit condition.** 10 000 ticks, asserted at every tick.
#[test]
fn ten_thousand_ticks_replay_bit_identically() {
    let ticks = 10_000;
    let log = scripted_log(0x5EED, ticks, PER_RACE * 10);
    let trace = record(SEED, SIZE, PER_RACE, &log, ticks);
    assert_eq!(trace.ticks(), ticks as usize);
    if let Err(d) = verify(&trace, &log) {
        panic!("{d}");
    }
}

#[test]
fn replay_survives_the_log_being_serialised_and_shipped() {
    let ticks = 4_000;
    let log = scripted_log(0x11, ticks, PER_RACE * 10);
    let shipped = InputLog::from_bytes(&log.to_bytes()).expect("round trip");
    let trace = record(SEED, SIZE, PER_RACE, &log, ticks);
    if let Err(d) = verify(&trace, &shipped) {
        panic!("shipped log diverged: {d}");
    }
}

#[test]
fn three_independent_runs_agree() {
    let ticks = 3_000;
    let log = scripted_log(0x22, ticks, PER_RACE * 10);
    let a = record(SEED, SIZE, PER_RACE, &log, ticks);
    let b = record(SEED, SIZE, PER_RACE, &log, ticks);
    let c = record(SEED, SIZE, PER_RACE, &log, ticks);
    assert_eq!(a.hashes, b.hashes);
    assert_eq!(b.hashes, c.hashes);
}

#[test]
fn a_one_bit_seed_change_diverges() {
    // Guards against the hash being insensitive to state — a check that always
    // passes is worse than no check.
    let ticks = 1_000;
    let log = scripted_log(0x33, ticks, PER_RACE * 10);
    let a = record(SEED, SIZE, PER_RACE, &log, ticks);
    let b = record(SEED ^ 1, SIZE, PER_RACE, &log, ticks);
    assert_ne!(a.final_hash(), b.final_hash());
}

#[test]
fn stepping_in_two_halves_matches_stepping_straight_through() {
    // There must be no hidden per-run setup that a resumed simulation misses.
    let ticks = 2_000u64;
    let log = scripted_log(0x44, ticks, PER_RACE * 10);
    let straight = record(SEED, SIZE, PER_RACE, &log, ticks);

    let mut w = build(SEED, SIZE, PER_RACE);
    let mut halves = Vec::new();
    for _ in 0..(ticks / 2) {
        w.step(&log);
        halves.push(w.state_hash());
    }
    let mut resumed = w.clone();
    for _ in 0..(ticks / 2) {
        resumed.step(&log);
        halves.push(resumed.state_hash());
    }
    assert_eq!(straight.hashes, halves);
}

#[test]
fn an_empty_server_stays_up_and_stays_empty() {
    // Action-recipe migration: every action (including `Exist`) dispatches
    // per living entity, so a zero-population server has nothing to
    // dispatch at all -- there is no separate per-race governor/demand
    // pipeline left whose "granted nothing" guarantee is worth asserting on
    // its own. What's left worth checking here: the world runs a long
    // stretch with zero population without panicking, and stays at zero.
    let ticks = TERRAIN_PERIOD * 50;
    let log = InputLog::new();
    let mut w = build(SEED, SIZE, 0);
    for _ in 0..ticks {
        w.step(&log);
    }
    assert_eq!(w.alive_count(), 0);
}
