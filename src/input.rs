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
use crate::race::Kind;

pub const MAGIC: u32 = 0x5047_494C; // "PGIL"
/// v1 (pre-S3) `Spawn` had no `Kind` byte. v2 adds one; `from_bytes` still
/// reads a v1 log, decoding every `Spawn` in it as `Kind::Animal` (§9 of
/// `docs/S3_ECOLOGY_LAYERS_DESIGN.md` — the closest honest default for a
/// world that predates the `Kind` axis). That compat path keeps old logs
/// *readable*, not hash-reproducing: replaying a v1 log against an S3 world
/// will not reproduce its originally recorded hashes, because the
/// simulation itself changed.
///
/// v3 (Invariant VIII / items-inventory) adds four new `CmdKind` variants —
/// `Mine`, `Smelt`, `MakeItem`, `BreakItem` — for the mining/smelting/item
/// layer. Unlike the v1→v2 change, this does **not** touch the byte layout of
/// any existing tag (0/`SetHeading`, 1/`Spawn`, 2/`Kill` are all unchanged),
/// so a v1 or v2 log is not just readable but fully hash-reproducing under
/// v3 code too — there is nothing in an old log to reinterpret, since none of
/// the new tags could ever appear in one. The version is still bumped rather
/// than silently widening `VERSION`'s own meaning, so an *old* reader handed
/// a *new* log (one that actually uses tags 3-6) fails fast with a clean
/// `BadVersion` at the header instead of a confusing mid-stream `BadTag`.
pub const VERSION: u32 = 3;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CmdKind {
    /// Steer. The vector is normalised on application, not on submission.
    SetHeading { dir: V2 },
    Spawn { element: Element, kind: Kind, at: V2 },
    Kill,
    /// Invariant VIII / items: an `Animal` (never a `Plant` — rooted bodies
    /// don't mine) draws up to `RaceAttrs::mining_rate` units of `element`
    /// out of the terrain cell it currently occupies, into its own
    /// `Entity.carried` — a pure 1:1 transfer, capped by whatever the cell
    /// actually holds (never more). See `World::mine`.
    Mine { element: Element },
    /// Invariant VIII / items: an `Animal` converts whole batches of its own
    /// carried `element` into carried `element.generates()`, at the fixed
    /// ratio `World::SMELT_RATIO_IN`:`World::SMELT_RATIO_OUT` (50:1, the
    /// project's own worked example) — the difference (tailings) returns to
    /// terrain at the smelting body's position, as `element`, fully
    /// accounted. See `World::smelt`.
    Smelt { element: Element },
    /// Invariant VIII / items: bundle `quantity` units of carried `element`
    /// (which must be at least that much, else this is a no-op) into a new
    /// `Item` pushed onto `Entity.items`. See `World::make_item`.
    MakeItem { element: Element, quantity: u64 },
    /// Invariant VIII / items: destroy the item at `index` in `Entity.items`
    /// (a no-op if out of range), returning its full quantity to terrain at
    /// the breaking body's position, as the item's own element. See
    /// `World::break_item`.
    BreakItem { index: u32 },
}

impl CmdKind {
    #[inline]
    fn tag(&self) -> u8 {
        match self {
            CmdKind::SetHeading { .. } => 0,
            CmdKind::Spawn { .. } => 1,
            CmdKind::Kill => 2,
            CmdKind::Mine { .. } => 3,
            CmdKind::Smelt { .. } => 4,
            CmdKind::MakeItem { .. } => 5,
            CmdKind::BreakItem { .. } => 6,
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
                CmdKind::Spawn { element, kind, at } => {
                    out.push(element as u8);
                    out.push(kind as u8);
                    out.extend_from_slice(&at.x.raw().to_le_bytes());
                    out.extend_from_slice(&at.y.raw().to_le_bytes());
                }
                CmdKind::Kill => {}
                CmdKind::Mine { element } => {
                    out.push(element as u8);
                }
                CmdKind::Smelt { element } => {
                    out.push(element as u8);
                }
                CmdKind::MakeItem { element, quantity } => {
                    out.push(element as u8);
                    out.extend_from_slice(&quantity.to_le_bytes());
                }
                CmdKind::BreakItem { index } => {
                    out.extend_from_slice(&index.to_le_bytes());
                }
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
        // v1 predates the `Kind` byte on `Spawn` — still readable, decoded
        // as `Kind::Animal` below, but not hash-reproducing against an S3
        // world (see `VERSION`'s own doc comment). v2 and v3 (current) share
        // an identical byte layout for every tag that existed in v2, so both
        // are fully readable *and* hash-reproducing.
        if v != VERSION && v != 1 && v != 2 {
            return Err(LogError::BadVersion(v));
        }
        let n = r.u64()? as usize;
        let mut cmds = Vec::with_capacity(n.min(1 << 20));
        for _ in 0..n {
            let tick = r.u64()?;
            let entity = r.u32()?;
            let cmd_kind = match r.u8()? {
                0 => CmdKind::SetHeading {
                    dir: V2::new(Fx::from_raw(r.i32()?), Fx::from_raw(r.i32()?)),
                },
                1 => {
                    let element = r.element()?;
                    let kind = if v == 1 {
                        Kind::Animal
                    } else {
                        match r.u8()? {
                            0 => Kind::Plant,
                            1 => Kind::Animal,
                            k => return Err(LogError::BadKind(k)),
                        }
                    };
                    CmdKind::Spawn {
                        element,
                        kind,
                        at: V2::new(Fx::from_raw(r.i32()?), Fx::from_raw(r.i32()?)),
                    }
                }
                2 => CmdKind::Kill,
                3 => CmdKind::Mine { element: r.element()? },
                4 => CmdKind::Smelt { element: r.element()? },
                5 => CmdKind::MakeItem { element: r.element()?, quantity: r.u64()? },
                6 => CmdKind::BreakItem { index: r.u32()? },
                t => return Err(LogError::BadTag(t)),
            };
            cmds.push(Command { tick, entity, kind: cmd_kind });
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
    BadKind(u8),
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
    /// Shared by `Spawn`/`Mine`/`Smelt`/`MakeItem` — every `CmdKind` that
    /// carries an `Element` byte decodes it the same validated way.
    fn element(&mut self) -> Result<Element, LogError> {
        let e = self.u8()?;
        if e as usize >= Element::COUNT {
            return Err(LogError::BadElement(e));
        }
        Ok(Element::from_index(e as usize))
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
                kind: Kind::Plant,
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

    /// §9 of `docs/S3_ECOLOGY_LAYERS_DESIGN.md`: a v1 log (pre-`Kind`, no
    /// kind byte on `Spawn`) must still be *readable*, decoding every Spawn
    /// as `Kind::Animal` — hand-built here since `to_bytes` only ever
    /// writes the current `VERSION`.
    #[test]
    fn a_v1_log_decodes_spawn_as_kind_animal() {
        let mut bytes = MAGIC.to_le_bytes().to_vec();
        bytes.extend_from_slice(&1u32.to_le_bytes()); // v1
        bytes.extend_from_slice(&1u64.to_le_bytes()); // one command
        bytes.extend_from_slice(&7u64.to_le_bytes()); // tick
        bytes.extend_from_slice(&3u32.to_le_bytes()); // entity
        bytes.push(CmdKind::Spawn { element: Element::Wood, kind: Kind::Animal, at: V2::ZERO }.tag());
        bytes.push(Element::Wood as u8);
        // No kind byte here — that's the whole point of v1.
        bytes.extend_from_slice(&Fx::from_int(4).raw().to_le_bytes());
        bytes.extend_from_slice(&Fx::from_int(5).raw().to_le_bytes());

        let log = InputLog::from_bytes(&bytes).expect("v1 log must still be readable");
        assert_eq!(
            log.as_slice()[0].kind,
            CmdKind::Spawn {
                element: Element::Wood,
                kind: Kind::Animal,
                at: V2::new(Fx::from_int(4), Fx::from_int(5)),
            }
        );
    }

    /// S3.9 / Invariant VIII: the four new `CmdKind` variants round-trip
    /// through bytes the same way every existing one does — a separate log
    /// from `sample()` above so this doesn't disturb that function's own
    /// pinned tick/entity-count assertions.
    #[test]
    fn new_cmdkinds_round_trip() {
        let mut l = InputLog::new();
        l.push(Command { tick: 1, entity: 1, kind: CmdKind::Mine { element: Element::Wood } });
        l.push(Command { tick: 2, entity: 1, kind: CmdKind::Smelt { element: Element::Fire } });
        l.push(Command {
            tick: 3,
            entity: 1,
            kind: CmdKind::MakeItem { element: Element::Earth, quantity: 12_345 },
        });
        l.push(Command { tick: 4, entity: 1, kind: CmdKind::BreakItem { index: 2 } });
        l.finalize();
        let back = InputLog::from_bytes(&l.to_bytes()).expect("round trip");
        assert_eq!(back.as_slice(), l.as_slice());
    }

    #[test]
    fn a_v2_log_is_still_readable_under_v3() {
        // v2 predates Mine/Smelt/MakeItem/BreakItem, but none of the tags a
        // v2 log could ever contain (0/1/2) changed byte layout going to v3
        // — a hand-built v2 header over an ordinary Kill command must still
        // decode cleanly.
        let mut bytes = MAGIC.to_le_bytes().to_vec();
        bytes.extend_from_slice(&2u32.to_le_bytes()); // v2
        bytes.extend_from_slice(&1u64.to_le_bytes()); // one command
        bytes.extend_from_slice(&9u64.to_le_bytes()); // tick
        bytes.extend_from_slice(&4u32.to_le_bytes()); // entity
        bytes.push(CmdKind::Kill.tag());
        let log = InputLog::from_bytes(&bytes).expect("v2 log must still be readable under v3");
        assert_eq!(log.as_slice()[0].kind, CmdKind::Kill);
    }

    #[test]
    fn empty_log_round_trips() {
        let mut l = InputLog::new();
        l.finalize();
        let back = InputLog::from_bytes(&l.to_bytes()).unwrap();
        assert!(back.is_empty());
    }
}
