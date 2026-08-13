//! The input log — Invariant V and VI.
//!
//! Cross-process traffic is inputs, not state, and every tick in history is
//! reproducible from `(world_seed, input_log)` alone. That makes this file the
//! canonical record of a run: if it round-trips through bytes and replays to
//! the same hashes, the simulation is sound.
//!
//! Commands are held in a total order — `(tick, entity, discriminant)` — so
//! that two processes handed the same set in different arrival orders still
//! apply them identically.

use crate::element::Element;
use crate::fx::{Fx, V2};

pub const MAGIC: u32 = 0x5047_494C; // "PGIL"
pub const VERSION: u32 = 1;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CmdKind {
    /// Steer. The vector is normalised on application, not on submission.
    SetHeading { dir: V2 },
    Spawn { element: Element, at: V2 },
    Kill,
}

impl CmdKind {
    #[inline]
    fn tag(&self) -> u8 {
        match self {
            CmdKind::SetHeading { .. } => 0,
            CmdKind::Spawn { .. } => 1,
            CmdKind::Kill => 2,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Command {
    pub tick: u64,
    pub entity: u32,
    pub kind: CmdKind,
}

impl Command {
    #[inline]
    fn order_key(&self) -> (u64, u32, u8) {
        (self.tick, self.entity, self.kind.tag())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InputLog {
    cmds: Vec<Command>,
    sorted: bool,
}

impl InputLog {
    pub fn new() -> InputLog {
        InputLog { cmds: Vec::new(), sorted: true }
    }

    pub fn push(&mut self, c: Command) {
        self.cmds.push(c);
        self.sorted = false;
    }

    pub fn len(&self) -> usize {
        self.cmds.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cmds.is_empty()
    }

    /// Impose the total order. Idempotent, and cheap when already sorted.
    pub fn finalize(&mut self) {
        if !self.sorted {
            self.cmds.sort_by_key(|c| c.order_key());
            self.sorted = true;
        }
    }

    pub fn as_slice(&self) -> &[Command] {
        debug_assert!(self.sorted, "call finalize() before reading the log");
        &self.cmds
    }

    /// All commands stamped for `tick`, in canonical order.
    pub fn at(&self, tick: u64) -> &[Command] {
        debug_assert!(self.sorted, "call finalize() before reading the log");
        let lo = self.cmds.partition_point(|c| c.tick < tick);
        let hi = self.cmds.partition_point(|c| c.tick <= tick);
        &self.cmds[lo..hi]
    }

    pub fn last_tick(&self) -> u64 {
        self.cmds.last().map(|c| c.tick).unwrap_or(0)
    }

    // ------------------------------------------------------------------
    // Serialisation. A log that cannot round-trip through bytes cannot be
    // shipped to a neighbouring territory, so this is tested as hard as the
    // simulation itself.
    // ------------------------------------------------------------------

    pub fn to_bytes(&self) -> Vec<u8> {
        debug_assert!(self.sorted, "call finalize() before serialising");
        let mut out = Vec::with_capacity(24 + self.cmds.len() * 24);
        out.extend_from_slice(&MAGIC.to_le_bytes());
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&(self.cmds.len() as u64).to_le_bytes());
        for c in &self.cmds {
            out.extend_from_slice(&c.tick.to_le_bytes());
            out.extend_from_slice(&c.entity.to_le_bytes());
            out.push(c.kind.tag());
            match c.kind {
                CmdKind::SetHeading { dir } => {
                    out.extend_from_slice(&dir.x.raw().to_le_bytes());
                    out.extend_from_slice(&dir.y.raw().to_le_bytes());
                }
                CmdKind::Spawn { element, at } => {
                    out.push(element as u8);
                    out.extend_from_slice(&at.x.raw().to_le_bytes());
                    out.extend_from_slice(&at.y.raw().to_le_bytes());
                }
                CmdKind::Kill => {}
            }
        }
        out
    }

    pub fn from_bytes(b: &[u8]) -> Result<InputLog, LogError> {
        let mut r = Reader { b, at: 0 };
        if r.u32()? != MAGIC {
            return Err(LogError::BadMagic);
        }
        let v = r.u32()?;
        if v != VERSION {
            return Err(LogError::BadVersion(v));
        }
        let n = r.u64()? as usize;
        let mut cmds = Vec::with_capacity(n.min(1 << 20));
        for _ in 0..n {
            let tick = r.u64()?;
            let entity = r.u32()?;
            let kind = match r.u8()? {
                0 => CmdKind::SetHeading {
                    dir: V2::new(Fx::from_raw(r.i32()?), Fx::from_raw(r.i32()?)),
                },
                1 => {
                    let e = r.u8()?;
                    if e as usize >= Element::COUNT {
                        return Err(LogError::BadElement(e));
                    }
                    CmdKind::Spawn {
                        element: Element::from_index(e as usize),
                        at: V2::new(Fx::from_raw(r.i32()?), Fx::from_raw(r.i32()?)),
                    }
                }
                2 => CmdKind::Kill,
                t => return Err(LogError::BadTag(t)),
            };
            cmds.push(Command { tick, entity, kind });
        }
        let mut log = InputLog { cmds, sorted: false };
        log.finalize();
        Ok(log)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LogError {
    BadMagic,
    BadVersion(u32),
    BadTag(u8),
    BadElement(u8),
    Truncated,
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], LogError> {
        if self.at + n > self.b.len() {
            return Err(LogError::Truncated);
        }
        let s = &self.b[self.at..self.at + n];
        self.at += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, LogError> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, LogError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn i32(&mut self) -> Result<i32, LogError> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, LogError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> InputLog {
        let mut l = InputLog::new();
        l.push(Command {
            tick: 5,
            entity: 2,
            kind: CmdKind::Kill,
        });
        l.push(Command {
            tick: 1,
            entity: 9,
            kind: CmdKind::SetHeading {
                dir: V2::new(Fx::ratio(3, 5), Fx::ratio(-4, 5)),
            },
        });
        l.push(Command {
            tick: 5,
            entity: 1,
            kind: CmdKind::Spawn {
                element: Element::Earth,
                at: V2::new(Fx::from_int(12), Fx::from_int(30)),
            },
        });
        l.finalize();
        l
    }

    #[test]
    fn finalize_imposes_a_total_order() {
        let l = sample();
        let keys: Vec<_> = l.as_slice().iter().map(|c| (c.tick, c.entity)).collect();
        assert_eq!(keys, vec![(1, 9), (5, 1), (5, 2)]);
    }

    #[test]
    fn at_returns_only_that_tick() {
        let l = sample();
        assert_eq!(l.at(1).len(), 1);
        assert_eq!(l.at(5).len(), 2);
        assert_eq!(l.at(4).len(), 0);
        assert_eq!(l.at(999).len(), 0);
    }

    #[test]
    fn bytes_round_trip_exactly() {
        let l = sample();
        let back = InputLog::from_bytes(&l.to_bytes()).expect("round trip");
        assert_eq!(back.as_slice(), l.as_slice());
    }

    #[test]
    fn insertion_order_does_not_affect_the_result() {
        // Two processes handed the same commands in different arrival orders
        // must end up with identical logs.
        let a = sample();
        let mut b = InputLog::new();
        for c in a.as_slice().iter().rev() {
            b.push(*c);
        }
        b.finalize();
        assert_eq!(a.as_slice(), b.as_slice());
    }

    #[test]
    fn rejects_a_corrupt_header() {
        assert_eq!(InputLog::from_bytes(&[0; 4]), Err(LogError::BadMagic));
        let mut bad = MAGIC.to_le_bytes().to_vec();
        bad.extend_from_slice(&99u32.to_le_bytes());
        bad.extend_from_slice(&0u64.to_le_bytes());
        assert_eq!(InputLog::from_bytes(&bad), Err(LogError::BadVersion(99)));
    }

    #[test]
    fn rejects_truncation() {
        let l = sample();
        let bytes = l.to_bytes();
        assert_eq!(
            InputLog::from_bytes(&bytes[..bytes.len() - 3]),
            Err(LogError::Truncated)
        );
    }

    #[test]
    fn empty_log_round_trips() {
        let mut l = InputLog::new();
        l.finalize();
        let back = InputLog::from_bytes(&l.to_bytes()).unwrap();
        assert!(back.is_empty());
    }
}
