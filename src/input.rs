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
use crate::race::Kind;
use crate::tile::Tile;

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
/// the new tags could ever appear in one.
///
/// v4 (the generic action-recipe system) adds one more variant, `Pickup`
/// (tag 7) — same story as v3: no existing tag's byte layout changes, so
/// every v1/v2/v3 log stays fully readable *and* hash-reproducing under v4.
///
/// v5 (the OSRS-style tile-grid rewrite) changes what tag 0 and tag 1's
/// *existing* payload bytes mean — the first version bump that is a genuine
/// semantic break, not just an added tag. `SetHeading{dir: V2}` (a
/// continuous steering direction) becomes `SetTarget{to: Option<Tile>}` (an
/// absolute tile-grid movement goal); tag 1's trailing two `i32`s stop being
/// raw `Fx` bits and become plain tile integers. Both stay *readable* under
/// v5 — a v1-v4 log decodes tag 1's old raw-`Fx` position by flooring it into
/// a tile (`raw >> Fx::SHIFT`, matching `Fx::floor_int`'s own arithmetic),
/// and tag 0's old direction decodes to `SetTarget { to: None }` (resume
/// default wander) rather than attempting to reconstruct an absolute tile
/// target from a bare direction with no entity position in scope at decode
/// time — but neither is **hash-reproducing**: the simulation itself
/// changed (continuous physics → discrete tile-stepping), the same honest
/// admission v1's own note above already makes for its `Kind` gap.
pub const VERSION: u32 = 5;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CmdKind {
    /// Commit to a tile-grid movement goal (`Some`), or clear it and resume
    /// default wander (`None`). Phase-1 minimal interpretation: Graze's
    /// fallback steps toward the goal instead of a random neighbour; real
    /// multi-tile pathfinding lands later without changing this wire shape.
    SetTarget { to: Option<Tile> },
    Spawn { element: Element, kind: Kind, at: Tile },
    Kill,
    /// Invariant VIII / items: an `Animal` (never a `Plant` — rooted bodies
    /// don't mine) draws up to this race's `Mine` `ActionRecipe`'s rate-law
    /// amount of `element` out of the terrain cell it currently occupies,
    /// into its own `Entity.carried` — a pure 1:1 transfer, capped by
    /// whatever the cell actually holds (never more). See
    /// `World::apply_action_recipe`.
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
    /// Action-recipe system: proximity-gated pickup — draws up to this
    /// race's `Pickup` `ActionRecipe`'s rate-law amount of `element` out of
    /// `World::ground_items` within that recipe's `reach` of this body's
    /// current position, into its own `Entity.carried`. See
    /// `World::apply_action_recipe`.
    Pickup { element: Element },
}

impl CmdKind {
    #[inline]
    fn tag(&self) -> u8 {
        match self {
            CmdKind::SetTarget { .. } => 0,
            CmdKind::Spawn { .. } => 1,
            CmdKind::Kill => 2,
            CmdKind::Mine { .. } => 3,
            CmdKind::Smelt { .. } => 4,
            CmdKind::MakeItem { .. } => 5,
            CmdKind::BreakItem { .. } => 6,
            CmdKind::Pickup { .. } => 7,
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
                CmdKind::SetTarget { to } => match to {
                    None => out.push(0),
                    Some(t) => {
                        out.push(1);
                        out.extend_from_slice(&t.x.to_le_bytes());
                        out.extend_from_slice(&t.y.to_le_bytes());
                    }
                },
                CmdKind::Spawn { element, kind, at } => {
                    out.push(element as u8);
                    out.push(kind as u8);
                    out.extend_from_slice(&at.x.to_le_bytes());
                    out.extend_from_slice(&at.y.to_le_bytes());
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
                CmdKind::Pickup { element } => {
                    out.push(element as u8);
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
        // world (see `VERSION`'s own doc comment). v2/v3/v4 all share an
        // identical byte layout for every tag that existed in v2. v5 changes
        // what tag 0/1's existing bytes *mean* (see `VERSION`'s own doc
        // comment) — every version below is still readable, none but v5
        // itself is hash-reproducing under v5 code.
        if v != VERSION && v != 1 && v != 2 && v != 3 && v != 4 {
            return Err(LogError::BadVersion(v));
        }
        let n = r.u64()? as usize;
        let mut cmds = Vec::with_capacity(n.min(1 << 20));
        for _ in 0..n {
            let tick = r.u64()?;
            let entity = r.u32()?;
            let cmd_kind = match r.u8()? {
                0 => {
                    if v == VERSION {
                        match r.u8()? {
                            0 => CmdKind::SetTarget { to: None },
                            1 => CmdKind::SetTarget { to: Some(Tile::new(r.i32()?, r.i32()?)) },
                            f => return Err(LogError::BadTag(f)),
                        }
                    } else {
                        // Pre-v5: a continuous steering direction, not an
                        // absolute tile. There is no entity position in
                        // scope at decode time to project it into a target
                        // tile, so it decodes to "clear/resume default
                        // wander" — readable, not hash-reproducing, per
                        // `VERSION`'s own doc comment.
                        r.i32()?;
                        r.i32()?;
                        CmdKind::SetTarget { to: None }
                    }
                }
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
                    let (rx, ry) = (r.i32()?, r.i32()?);
                    let at = if v == VERSION {
                        Tile::new(rx, ry)
                    } else {
                        // Pre-v5: raw `Fx` bits — floor into a tile the same
                        // way `Fx::floor_int` does (`raw >> Fx::SHIFT`).
                        Tile::new(rx >> crate::fx::SHIFT, ry >> crate::fx::SHIFT)
                    };
                    CmdKind::Spawn { element, kind, at }
                }
                2 => CmdKind::Kill,
                3 => CmdKind::Mine { element: r.element()? },
                4 => CmdKind::Smelt { element: r.element()? },
                5 => CmdKind::MakeItem { element: r.element()?, quantity: r.u64()? },
                6 => CmdKind::BreakItem { index: r.u32()? },
                7 => CmdKind::Pickup { element: r.element()? },
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
