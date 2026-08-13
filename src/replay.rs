//! Replay and verification — Invariant VI made into an instrument.
//!
//! A trace records the state hash at *every* tick, not just the last one.
//! That distinction is the whole value: a final-hash-only check tells you the
//! run diverged, while a per-tick trace tells you the tick it diverged on,
//! which is the difference between a afternoon and a fortnight.

use crate::element::Element;
use crate::fx::{Fx, V2};
use crate::input::{CmdKind, Command, InputLog};
use crate::rand::{rand_below, rand_signed, Channel};
use crate::world::World;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trace {
    pub seed: u64,
    pub size: i32,
    pub per_race: u32,
    /// `hashes[i]` is the state hash after `i + 1` steps.
    pub hashes: Vec<u64>,
}

impl Trace {
    pub fn ticks(&self) -> usize {
        self.hashes.len()
    }

    pub fn final_hash(&self) -> u64 {
        self.hashes.last().copied().unwrap_or(0)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Divergence {
    /// The first tick whose hash disagreed — where to start looking.
    pub tick: u64,
    pub expected: u64,
    pub actual: u64,
}

impl core::fmt::Display for Divergence {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "diverged at tick {}: expected {:#018x}, got {:#018x}",
            self.tick, self.expected, self.actual
        )
    }
}

pub fn build(seed: u64, size: i32, per_race: u32) -> World {
    let mut w = World::new(seed, size);
    w.seed_population(per_race);
    w
}

/// Run the simulation and capture a hash per tick.
pub fn record(seed: u64, size: i32, per_race: u32, log: &InputLog, ticks: u64) -> Trace {
    let mut w = build(seed, size, per_race);
    let mut hashes = Vec::with_capacity(ticks as usize);
    for _ in 0..ticks {
        w.step(log);
        hashes.push(w.state_hash());
    }
    Trace { seed, size, per_race, hashes }
}

/// Re-run from the same seed and log, asserting agreement at every tick.
pub fn verify(trace: &Trace, log: &InputLog) -> Result<(), Divergence> {
    let mut w = build(trace.seed, trace.size, trace.per_race);
    for (i, expected) in trace.hashes.iter().enumerate() {
        w.step(log);
        let actual = w.state_hash();
        if actual != *expected {
            return Err(Divergence {
                tick: i as u64,
                expected: *expected,
                actual,
            });
        }
    }
    Ok(())
}

/// A deterministic, adversarial-ish command stream: steering changes, spawns
/// and kills scattered across the run. Exercising commands matters because an
/// empty log would let a whole class of ordering bugs hide.
pub fn scripted_log(seed: u64, ticks: u64, live_ids: u32) -> InputLog {
    let mut log = InputLog::new();
    for t in 0..ticks {
        // Steer somebody roughly every third tick.
        if rand_below(seed, t, 0, Channel::Wander, 3) == 0 {
            let target = 1 + rand_below(seed, t, 1, Channel::Wander, live_ids.max(1));
            log.push(Command {
                tick: t,
                entity: target,
                kind: CmdKind::SetHeading {
                    dir: V2::new(
                        rand_signed(seed, t, 2, Channel::Wander),
                        rand_signed(seed, t, 3, Channel::Wander),
                    ),
                },
            });
        }
        // Occasional incarnation.
        if rand_below(seed, t, 4, Channel::SpawnPlacement, 40) == 0 {
            let e = Element::from_index(rand_below(seed, t, 5, Channel::SpawnPlacement, 5) as usize);
            log.push(Command {
                tick: t,
                entity: 0,
                kind: CmdKind::Spawn {
                    element: e,
                    at: V2::new(
                        Fx::from_int(rand_below(seed, t, 6, Channel::SpawnPlacement, 60) as i32),
                        Fx::from_int(rand_below(seed, t, 7, Channel::SpawnPlacement, 60) as i32),
                    ),
                },
            });
        }
        // Occasional violent death.
        if rand_below(seed, t, 8, Channel::Crit, 90) == 0 {
            let target = 1 + rand_below(seed, t, 9, Channel::Crit, live_ids.max(1));
            log.push(Command {
                tick: t,
                entity: target,
                kind: CmdKind::Kill,
            });
        }
    }
    log.finalize();
    log
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_reproduces_itself() {
        let log = scripted_log(5, 400, 40);
        let t = record(0xABC, 48, 6, &log, 400);
        assert_eq!(verify(&t, &log), Ok(()));
    }

    #[test]
    fn verify_reports_the_first_bad_tick() {
        let log = scripted_log(5, 200, 40);
        let mut t = record(0xABC, 48, 6, &log, 200);
        t.hashes[57] ^= 1;
        match verify(&t, &log) {
            Err(d) => assert_eq!(d.tick, 57),
            Ok(()) => panic!("corrupted trace verified clean"),
        }
    }

    #[test]
    fn a_different_seed_gives_a_different_run() {
        let log = scripted_log(5, 200, 40);
        let a = record(1, 48, 6, &log, 200);
        let b = record(2, 48, 6, &log, 200);
        assert_ne!(a.final_hash(), b.final_hash());
    }

    #[test]
    fn a_different_log_gives_a_different_run() {
        let a_log = scripted_log(5, 200, 40);
        let b_log = scripted_log(6, 200, 40);
        let a = record(1, 48, 6, &a_log, 200);
        let b = record(1, 48, 6, &b_log, 200);
        assert_ne!(a.final_hash(), b.final_hash());
    }

    #[test]
    fn a_log_survives_a_round_trip_through_bytes() {
        // Invariant V: the log is what ships between processes, so a
        // serialisation bug is a desync bug.
        let log = scripted_log(9, 300, 40);
        let shipped = InputLog::from_bytes(&log.to_bytes()).expect("round trip");
        let a = record(77, 48, 6, &log, 300);
        let b = record(77, 48, 6, &shipped, 300);
        assert_eq!(a.hashes, b.hashes);
    }
}
