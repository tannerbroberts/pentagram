//! Pentagram — Stage 0: the determinism skeleton.
//!
//! Exit condition for this stage: 10 000 ticks replay bit-identical from
//! `(seed, input_log)`, asserted at every tick rather than only at the end.
//!
//! # The invariants this crate exists to hold
//!
//! | | | Where it lives |
//! |---|---|---|
//! | I   | Bounded propagation — nothing acts instantly at a distance | [`terrain::apply_diffusion`] |
//! | II  | No floating point in simulation state | [`fx`] |
//! | III | Randomness is stateless | [`rand`] |
//! | IV  | Iteration order is defined | [`world`], [`element::PerElement`] |
//! | V   | Inputs replicate; state does not | [`input`] |
//! | VI  | Every tick is reproducible | [`replay`] |
//! | VII | Bounded churn — rates are floored and capped | [`governor`] |
//! | VIII | Material is conserved — every unit of terrain/body/item material traces to an explicit transfer or ring-ordered conversion, never created or destroyed | [`terrain::apply_conversion`], `world`'s mining/smelting/item commands |
//!
//! # Reading order
//!
//! [`fx`] and [`rand`] are the foundation — everything else is downstream of
//! those two files being correct. [`element`] is the five-cycle. [`race`] and
//! [`governor`] carry the design's rate model. [`terrain`] is Stage 1's field
//! and `phase_terrain`'s fixed-order operator slots — see
//! `docs/S1_TERRAIN_DESIGN.md`. Terrain is not its own actor: only the
//! conversion and diffusion slots live in `terrain` itself; the other two are
//! `ecology`'s
//! `apply_attrition`/`apply_suppression`, which read terrain and act on
//! bodies rather than the other way around. [`ecology`] is Stage 2's
//! feeding/starvation/reproduction rates plus those two terrain-tick-gated
//! operators, read by `world`'s `phase_feeding`, `phase_aging`, and
//! `phase_terrain`. [`behavior`] is Stage 3's animal FSM — Flee, Hunt, and
//! Graze, derived fresh every tick from `(hunger, sensed neighbourhood,
//! terrain)` rather than stored on `Entity` — read by `world`'s
//! `phase_movement`. [`world`] is the tick loop, and its phase order is a
//! wire format, not an implementation detail.
//!
//! **Invariant VIII (material conservation).** [`entity::Entity`]'s
//! `material`/`carried`/`items` fields, [`race::Conversion`], and
//! `terrain::apply_conversion` are the core of it — a race's habitat draw
//! becomes its own element via a fixed ratio, split across background
//! deposit / the body's own held material / explicit waste, never conjuring
//! or discarding a unit. The items/inventory layer built on top —
//! mining (terrain → `Entity.carried`), smelting (`Entity.carried` X →
//! `Entity.carried` X.generates(), tailings to terrain), and
//! [`entity::Item`] (a portable single-element bundle, made from and broken
//! back into `carried`/terrain) — is `world`'s `CmdKind::Mine`/`Smelt`/
//! `MakeItem`/`BreakItem`, gated the same command-log-driven, replay-safe
//! way every other explicit action in this crate is (`input`).

pub mod behavior;
pub mod ecology;
pub mod element;
pub mod entity;
pub mod fx;
pub mod governor;
pub mod hash;
pub mod input;
pub mod race;
pub mod rand;
pub mod replay;
pub mod terrain;
pub mod tuning;
pub mod world;

pub use behavior::{BehaviorTuning, Drive};
pub use ecology::EcologyTuning;
pub use element::{Element, PerElement};
pub use fx::{Fx, V2};
pub use governor::{Governor, Grant};
pub use input::{CmdKind, Command, InputLog};
pub use race::{attrs, RaceAttrs, RateBand, TERRAIN_PERIOD};
pub use replay::{record, verify, Divergence, Trace};
pub use terrain::{Terrain, TerrainTuning};
pub use world::World;
