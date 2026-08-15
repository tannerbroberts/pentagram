//! The tuning table, as a thing you can point at.
//!
//! Every knob is one row in [`PAGES`]: a name, how to format it, how big a
//! nudge is, its safe range, and a getter/setter pair. This lives in the
//! library — not in a binary — so that the terminal live view and a windowed
//! client can both drive the exact same knobs from the exact same table,
//! rather than one of them copying it and drifting. Adding a knob means
//! adding a row here and nothing else — any client's grid, cursor, stepping
//! and detail line all fall out of this table.
//!
//! Ranges are not decoration. `deposit_unit` feeds `demand`, which is summed in
//! milli-units over a terrain period; the crate builds with `overflow-checks`
//! in every profile, so a knob with no ceiling is a panic waiting for a curious
//! user. Every `hi` here is chosen to keep the worst case inside `u64`.

use std::fmt::Write as _;

use crate::behavior::BehaviorTuning;
use crate::climate::ClimateTuning;
use crate::ecology::{EcologyTuning, PropagationTuning};
use crate::element::Element;
use crate::fx::Fx;
use crate::race::{
    Channel, Edge, PerRace, Race, RaceAttrs, TICKS_PER_DAY, TICKS_PER_MINUTE, RACES,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Axis {
    /// Every row differs per (element, kind) -- races/behavior knobs.
    Race,
    /// Every row is shared by both kinds of an element -- terrain/climate/
    /// ecology/propagation knobs. The Kind toggle has no effect on this
    /// page's values; the header should say so rather than silently
    /// implying two independent numbers that are actually the same cell.
    Element,
}

/// Everything a client can change, in one struct.
///
/// `races` is the real tuning table and goes straight into the world.
/// `restock` and `wander` are the *client's* knobs, not the simulation's: Stage 0
/// has no reproduction and no goals, so without a hand on the tiller the map
/// empties out and the survivors travel in straight lines. Both are applied as
/// ordinary input commands, which is exactly what a player would be.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Tuning {
    pub races: PerRace<RaceAttrs>,
    pub restock: PerRace<u32>,
    pub terrain: crate::terrain::TerrainTuning,
    pub climate: ClimateTuning,
    pub ecology: EcologyTuning,
    pub propagation: PropagationTuning,
    pub behavior: BehaviorTuning,
}

impl Tuning {
    pub fn new(per_race: u32) -> Tuning {
        Tuning {
            races: RACES,
            restock: PerRace::filled(per_race),
            terrain: crate::terrain::TerrainTuning::default(),
            climate: ClimateTuning::default(),
            ecology: EcologyTuning::default(),
            propagation: PropagationTuning::default(),
            behavior: BehaviorTuning::default(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fmt {
    /// Plain count, abbreviated in the grid and exact in the detail line.
    Int,
    /// Sim ticks, shown as the duration they actually are.
    Ticks,
    /// Per-mille of a whole.
    Permille,
    /// Hundredths of a cell.
    Cells,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step {
    /// Fixed increment. Coarse adjust is ten of them.
    Add(i64),
    /// Proportional: about 10% a press, doubling on coarse adjust. The only
    /// workable choice for knobs that span four orders of magnitude, like
    /// lifespan running from eight minutes to a fortnight.
    Scale,
}

pub struct Knob {
    pub name: &'static str,
    pub help: &'static str,
    pub fmt: Fmt,
    pub step: Step,
    pub lo: i64,
    pub hi: i64,
    pub get: fn(&Tuning, Race) -> i64,
    pub set: fn(&mut Tuning, Race, i64),
}

impl Knob {
    #[inline]
    pub fn value(&self, t: &Tuning, r: Race) -> i64 {
        (self.get)(t, r)
    }

    pub fn nudge(&self, t: &mut Tuning, e: Race, up: bool, coarse: bool) {
        let v = self.value(t, e);
        let next = match self.step {
            Step::Add(n) => {
                let n = if coarse { n * 10 } else { n };
                if up {
                    v.saturating_add(n)
                } else {
                    v.saturating_sub(n)
                }
            }
            Step::Scale if coarse => {
                if up {
                    v.saturating_mul(2).max(v + 1)
                } else {
                    v / 2
                }
            }
            Step::Scale => {
                if up {
                    v.saturating_add((v / 10).max(1))
                } else {
                    v.saturating_sub((v / 11).max(1))
                }
            }
        };
        (self.set)(t, e, next.clamp(self.lo, self.hi));
    }

    /// Compact form, for a grid cell.
    pub fn short(&self, v: i64) -> String {
        match self.fmt {
            Fmt::Int => abbrev(v),
            Fmt::Ticks => duration(v.max(0) as u64),
            Fmt::Permille => format!("{v}"),
            Fmt::Cells => format!("{}.{:02}", v / 100, (v % 100).abs()),
        }
    }

    /// Exact form, for the detail line under the grid.
    pub fn long(&self, v: i64) -> String {
        match self.fmt {
            Fmt::Int => grouped(v),
            Fmt::Ticks => format!("{} ticks · {}", grouped(v), duration(v.max(0) as u64)),
            Fmt::Permille => format!("{v}‰ of the unit"),
            Fmt::Cells => format!("{}.{:02} cells", v / 100, (v % 100).abs()),
        }
    }
}

pub struct Page {
    pub title: &'static str,
    pub knobs: &'static [Knob],
    pub axis: Axis,
}

macro_rules! knob {
    ($name:literal, $fmt:expr, $step:expr, $lo:expr, $hi:expr, $help:literal,
     |$t:ident, $e:ident| $get:expr,
     |$tm:ident, $em:ident, $v:ident| $set:expr) => {
        Knob {
            name: $name,
            help: $help,
            fmt: $fmt,
            step: $step,
            lo: $lo,
            hi: $hi,
            get: |$t: &Tuning, $e: Race| -> i64 { $get },
            set: |$tm: &mut Tuning, $em: Race, $v: i64| { $set },
        }
    };
}

/// A band edge. Deposit and consume are the same three edges twice over, and
/// each one has to go through `set_edge` so the band cannot be left inverted.
macro_rules! edge_knob {
    ($name:literal, $field:ident, $read:ident, $edge:expr, $help:literal) => {
        knob!($name, Fmt::Int, Step::Scale, 0, 1_000_000, $help,
              |t, e| t.races[e].$field.$read as i64,
              |t, e, v| t.races[e].$field.set_edge($edge, v as u32))
    };
}

macro_rules! burst_knob {
    ($name:literal, $field:ident, $help:literal) => {
        knob!($name, Fmt::Int, Step::Add(1), 1, 500, $help,
              |t, e| t.races[e].$field.burst_ticks as i64,
              |t, e, v| t.races[e].$field.burst_ticks = v as u32)
    };
}

static BODY: [Knob; 16] = [
    knob!("lifespan", Fmt::Ticks, Step::Scale, 100, TICKS_PER_DAY as i64 * 90,
          "how long one body persists before it expires of old age",
          |t, e| t.races[e].lifespan as i64,
          |t, e, v| t.races[e].lifespan = v as u64),
    knob!("life variance", Fmt::Permille, Step::Add(10), 0, 900,
          "per-mille spread on lifespan, so a cohort born together does not die together",
          |t, e| t.races[e].lifespan_variance as i64,
          |t, e, v| t.races[e].lifespan_variance = v as u16),
    knob!("speed", Fmt::Cells, Step::Add(1), 0, 400,
          "cells per tick — an ECOLOGY knob: it decides whether biomes coexist or collapse to one",
          |t, e| (t.races[e].speed.raw() as i64 * 100 + 32_768) / 65_536,
          |t, e, v| t.races[e].speed = Fx::ratio(v as i32, 100)),
    knob!("radius", Fmt::Cells, Step::Add(5), 1, 2_000,
          "collision radius in cells; bigger bodies crowd each other out of a region sooner",
          |t, e| (t.races[e].radius.raw() as i64 * 100 + 32_768) / 65_536,
          |t, e, v| t.races[e].radius = Fx::ratio(v as i32, 100)),
    knob!("restock to", Fmt::Int, Step::Add(5), 0, 250,
          "VIEW KNOB: bodies of this race the view keeps spawning back, as ordinary input commands",
          |t, e| t.restock[e] as i64,
          |t, e, v| t.restock[e] = v as u32),

    // Invariant VIII: deposition is no longer an independent rate axis —
    // `deposit_unit`/`deposit`/`deposit_mix` are retired (see
    // `race::Conversion`'s doc comment). These five knobs replace them with
    // the coupled conversion: a fixed ratio from the habitat draw below into
    // the race's own element, and where the produced amount goes.
    knob!("ratio in", Fmt::Int, Step::Add(1), 1, 100_000,
          "habitat-element units consumed per conversion batch",
          |t, e| t.races[e].conversion.ratio_in as i64,
          |t, e, v| t.races[e].conversion.ratio_in = v as u32),
    knob!("ratio out", Fmt::Int, Step::Add(1), 1, 100_000,
          "own-element units produced per batch — never exceeds ratio in; a conversion cannot manufacture mass",
          |t, e| t.races[e].conversion.ratio_out as i64,
          |t, e, v| t.races[e].conversion.ratio_out = (v as u32).min(t.races[e].conversion.ratio_in)),
    knob!("dep share", Fmt::Permille, Step::Add(25), 0, 1000,
          "permille of produced material written to terrain as background deposit",
          |t, e| t.races[e].conversion.deposit_share as i64,
          |t, e, v| t.races[e].conversion.deposit_share = v as u16),
    knob!("body share", Fmt::Permille, Step::Add(25), 0, 1000,
          "permille of produced material added to the body's own held material",
          |t, e| t.races[e].conversion.body_share as i64,
          |t, e, v| t.races[e].conversion.body_share = v as u16),
    knob!("waste share", Fmt::Permille, Step::Add(25), 0, 1000,
          "permille of produced material returned to terrain as an explicit waste byproduct",
          |t, e| t.races[e].conversion.waste_share as i64,
          |t, e, v| t.races[e].conversion.waste_share = v as u16),

    // Items/inventory (Invariant VIII extension): the one new per-race rate
    // knob mining adds — see `race::RaceAttrs::mining_rate`'s own doc
    // comment. Smelting's ratio is a fixed global constant
    // (`World::SMELT_RATIO_IN`/`SMELT_RATIO_OUT`), not a per-race tunable, so
    // it has no row here.
    knob!("mining rate", Fmt::Int, Step::Add(5), 0, 60_000,
          "terrain units of a chosen element drawn into carried stock per Mine command (Animal only)",
          |t, e| t.races[e].mining_rate as i64,
          |t, e, v| t.races[e].mining_rate = v as u32),

    knob!("consume unit", Fmt::Int, Step::Scale, 1, 100_000_000,
          "total a body draws from its habitat element over its entire life",
          |t, e| t.races[e].consume_unit as i64,
          |t, e, v| t.races[e].consume_unit = v as u64),
    edge_knob!("con floor", consume, floor, Edge::Floor,
               "reserve: the bucket is never spent below this in one tick (Invariant VIII — no longer a free minimum)"),
    edge_knob!("con nominal", consume, nominal, Edge::Nominal,
               "long-run average consumption under sustained demand"),
    edge_knob!("con ceiling", consume, ceiling, Edge::Ceiling,
               "never exceeded in one terrain tick"),
    burst_knob!("con burst", consume,
                "terrain ticks of nominal consumption that can be banked"),
];

/// One row per channel. The mix is what makes two races with identical
/// rates feel nothing alike, so it gets its own page rather than being
/// buried. Invariant VIII: `deposit_mix` is retired (deposition is a fixed,
/// same-tick consequence of the draw below, not a separately timed channel
/// of its own — see `race::Conversion`'s doc comment and the "body share"
/// row on the body & rates page, which is what now carries a race's old
/// "terraforms mostly at death" flavour). Only the habitat-draw timing
/// (`consume_mix`) remains a channel mix.
macro_rules! mix_knobs {
    ($prefix:literal, $field:ident, $chan:expr, $help:literal) => {
        knob!($prefix, Fmt::Permille, Step::Add(25), 0, 1000, $help,
              |t, e| t.races[e].$field.permille($chan) as i64,
              |t, e, v| t.races[e].$field.set_rebalanced($chan, v as u16))
    };
}

static MIX: [Knob; 5] = [
    mix_knobs!("con birth", consume_mix, Channel::OnBirth, "taken at incarnation"),
    mix_knobs!("con death", consume_mix, Channel::OnDeath, "taken by the corpse"),
    mix_knobs!("con action", consume_mix, Channel::OnAction, "taken by moving"),
    mix_knobs!("con consume", consume_mix, Channel::OnConsume, "taken by feeding"),
    mix_knobs!("con existence", consume_mix, Channel::OnExistence, "taken by being present"),
];

/// Terrain's two rate/cap knobs, then climate's five — one combined static
/// since Rust statics can't be concatenated. See `TerrainTuning` (src/terrain.rs)
/// and `ClimateTuning` (src/climate.rs) for what each field actually drives.
/// Ring/star used to live here — terrain isn't its own actor, so those two
/// relations moved to the `ecology (S2)` page below as attrition/suppression.
static TERRAIN_AND_CLIMATE: [Knob; 7] = [
    knob!("diffuse rate", Fmt::Permille, Step::Add(1), 0, 1000,
          "permille of the concentration difference across an edge that moves each terrain tick",
          |t, e| t.terrain.diffuse_rate[e.element] as i64,
          |t, e, v| t.terrain.diffuse_rate[e.element] = v as u16),
    knob!("diffuse cap", Fmt::Int, Step::Add(5), 0, u16::MAX as i64,
          "flat units per edge per terrain tick — no edge can ever carry more than this in one tick",
          |t, e| t.terrain.diffuse_cap[e.element] as i64,
          |t, e, v| t.terrain.diffuse_cap[e.element] = v as u16),

    knob!("base lo", Fmt::Int, Step::Add(1), 0, u16::MAX as i64,
          "low end of the one-time per-cell geography draw for this element",
          |t, e| t.climate.base_range[e.element].0 as i64,
          |t, e, v| t.climate.base_range[e.element].0 = v as u16),
    knob!("base hi", Fmt::Int, Step::Add(1), 0, u16::MAX as i64,
          "high end of the one-time per-cell geography draw for this element",
          |t, e| t.climate.base_range[e.element].1 as i64,
          |t, e, v| t.climate.base_range[e.element].1 = v as u16),
    knob!("climate floor", Fmt::Int, Step::Add(1), 0, u16::MAX as i64,
          "always-on climate influx added every terrain tick, regardless of season",
          |t, e| t.climate.floor[e.element] as i64,
          |t, e, v| t.climate.floor[e.element] = v as u16),
    knob!("season peak", Fmt::Int, Step::Add(50), 0, u16::MAX as i64,
          "peak seasonal bonus applied to this element while it is in season",
          |t, e| t.climate.season_peak[e.element] as i64,
          |t, e, v| t.climate.season_peak[e.element] = v as u16),
    knob!("season length", Fmt::Ticks, Step::Scale, 1, TICKS_PER_DAY as i64 * 90,
          "GLOBAL: terrain ticks per season — five seasons, one per element, make one full lap",
          |t, _e| t.climate.season_ticks as i64,
          |t, _e, v| t.climate.season_ticks = v as u64),
];

/// S2's five feeding/starvation knobs, plus attrition and suppression —
/// terrain's old ring/star relations, redirected onto the bodies standing in
/// terrain instead of onto terrain itself. See `EcologyTuning`
/// (src/ecology.rs) for what each field actually drives, and its module doc
/// for why the shipped defaults are a first guess rather than a promised
/// balance.
static ECOLOGY: [Knob; 8] = [
    knob!("forage radius", Fmt::Cells, Step::Add(20), 0, 4_000,
          "how far a body can reach to eat prey on the ring edge it eats",
          |t, e| (t.ecology.forage_radius[e.element].raw() as i64 * 100 + 32_768) / 65_536,
          |t, e, v| t.ecology.forage_radius[e.element] = Fx::ratio(v as i32, 100)),
    knob!("satiation", Fmt::Ticks, Step::Scale, 1, TICKS_PER_DAY as i64,
          "minimum ticks between one body's successful meals",
          |t, e| t.ecology.satiation[e.element] as i64,
          |t, e, v| t.ecology.satiation[e.element] = v as u32),
    knob!("feed gain", Fmt::Int, Step::Add(5), 0, 100,
          "hp restored by one successful meal, out of a 0..=100 scale",
          |t, e| t.ecology.feed_gain[e.element] as i64,
          |t, e, v| t.ecology.feed_gain[e.element] = v as i32),
    knob!("starve after", Fmt::Ticks, Step::Scale, 1, TICKS_PER_DAY as i64 * 7,
          "ticks without a meal before starvation drain begins",
          |t, e| t.ecology.starve_after[e.element] as i64,
          |t, e, v| t.ecology.starve_after[e.element] = v as u32),
    knob!("starve rate", Fmt::Int, Step::Add(1), 0, 100,
          "hp lost per tick once starvation has begun",
          |t, e| t.ecology.starve_rate[e.element] as i64,
          |t, e, v| t.ecology.starve_rate[e.element] = v as i32),
    knob!("repro threshold", Fmt::Int, Step::Add(5), 0, 100,
          "hp a meal must reach, from below, to spawn an offspring",
          |t, e| t.ecology.repro_threshold[e.element] as i64,
          |t, e, v| t.ecology.repro_threshold[e.element] = v as i32),
    knob!("attrition rate", Fmt::Permille, Step::Add(1), 0, 1000,
          "permille of what eats this element's terrain concentration converted to hp damage each terrain tick",
          |t, e| t.ecology.attrition_rate[e.element] as i64,
          |t, e, v| t.ecology.attrition_rate[e.element] = v as u16),
    knob!("suppression rate", Fmt::Permille, Step::Add(1), 0, 1000,
          "permille of this element's suppressor's terrain concentration added to hunger each terrain tick",
          |t, e| t.ecology.suppression_rate[e.element] as i64,
          |t, e, v| t.ecology.suppression_rate[e.element] = v as u16),
];

static PROPAGATION: [Knob; 6] = [
    knob!("period", Fmt::Int, Step::Add(1), 0, 1000,
          "terrain ticks between propagation attempts; 0 = never attempts",
          |t, e| t.propagation.period[e.element] as i64,
          |t, e, v| t.propagation.period[e.element] = v as u64),
    knob!("chance", Fmt::Permille, Step::Add(25), 0, 1000,
          "per-mille, per eligible plant, per attempt",
          |t, e| t.propagation.chance[e.element] as i64,
          |t, e, v| t.propagation.chance[e.element] = v as u16),
    knob!("offspring size", Fmt::Permille, Step::Add(25), 0, 1000,
          "per-mille of full size a new offspring is born at",
          |t, e| t.propagation.offspring_size[e.element] as i64,
          |t, e, v| t.propagation.offspring_size[e.element] = v as u16),
    knob!("root min", Fmt::Int, Step::Add(50), 0, u16::MAX as i64,
          "minimum terrain stock of the plant's own element required at the candidate cell",
          |t, e| t.propagation.root_min[e.element] as i64,
          |t, e, v| t.propagation.root_min[e.element] = v as u16),
    knob!("dispersal", Fmt::Cells, Step::Add(5), 0, 2000,
          "max scatter offset from the parent, in cells",
          |t, e| (t.propagation.dispersal[e.element].raw() as i64 * 100 + 32_768) / 65_536,
          |t, e, v| t.propagation.dispersal[e.element] = Fx::ratio(v as i32, 100)),
    knob!("crowd max", Fmt::Int, Step::Add(1), 0, 1000,
          "max same-race bodies already occupying the candidate cell",
          |t, e| t.propagation.crowd_max[e.element] as i64,
          |t, e, v| t.propagation.crowd_max[e.element] = v as u16),
];

static BEHAVIOR: [Knob; 3] = [
    knob!("flee threshold", Fmt::Int, Step::Add(200), 0, u16::MAX as i64,
          "terrain stock of what eats this race, at its own cell, above which it flees",
          |t, e| t.behavior.flee_threshold[e] as i64,
          |t, e, v| t.behavior.flee_threshold[e] = v as u16),
    knob!("sense radius", Fmt::Cells, Step::Add(20), 0, 4_000,
          "how far a body can sense prey, in cells -- larger than forage radius on purpose",
          |t, e| (t.behavior.sense_radius[e].raw() as i64 * 100 + 32_768) / 65_536,
          |t, e, v| t.behavior.sense_radius[e] = Fx::ratio(v as i32, 100)),
    knob!("turn rate", Fmt::Permille, Step::Add(25), 0, 1000,
          "per-tick steering blend toward the desired heading; 0 never turns, 1000 snaps",
          |t, e| (t.behavior.turn_rate[e].raw() as i64 * 1000) / 65_536,
          |t, e, v| t.behavior.turn_rate[e] = Fx::ratio(v as i32, 1000)),
];

pub static PAGES: &[Page] = &[
    Page { title: "body & rates", knobs: &BODY, axis: Axis::Race },
    Page { title: "channel mix ‰  (edits rebalance the rest to keep the sum at 1000)", knobs: &MIX, axis: Axis::Race },
    Page { title: "terrain & climate", knobs: &TERRAIN_AND_CLIMATE, axis: Axis::Element },
    Page { title: "ecology (S2)", knobs: &ECOLOGY, axis: Axis::Element },
    Page { title: "propagation (S3.5)", knobs: &PROPAGATION, axis: Axis::Element },
    Page { title: "behavior (S3.4)", knobs: &BEHAVIOR, axis: Axis::Race },
];

// ----------------------------------------------------------------------
// Formatting.
// ----------------------------------------------------------------------

/// Sim ticks as the duration they represent. 100 ticks is one simulated minute.
pub fn duration(ticks: u64) -> String {
    let m = ticks / TICKS_PER_MINUTE;
    if m < 1 {
        return format!("{ticks}t");
    }
    if m < 90 {
        return format!("{m}m");
    }
    let h = m / 60;
    if h < 48 {
        return format!("{h}h{:02}", m % 60);
    }
    format!("{}d{:02}h", h / 24, h % 24)
}

/// Three significant-ish figures, so a 12-character column can hold anything
/// from a per-mille to five million.
pub fn abbrev(v: i64) -> String {
    let (sign, a) = if v < 0 { ("-", -v) } else { ("", v) };
    if a < 10_000 {
        format!("{sign}{a}")
    } else if a < 1_000_000 {
        format!("{sign}{}.{}k", a / 1000, (a % 1000) / 100)
    } else {
        format!("{sign}{}.{}M", a / 1_000_000, (a % 1_000_000) / 100_000)
    }
}

/// Digit-grouped with thin spaces, matching how the design doc writes numbers.
pub fn grouped(v: i64) -> String {
    let (sign, a) = if v < 0 { ("-", -v) } else { ("", v) };
    let digits = a.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(' ');
        }
        out.push(c);
    }
    format!("{sign}{out}")
}

// ----------------------------------------------------------------------
// Element colours, shared by every client that draws a map or a legend.
// ----------------------------------------------------------------------

/// Element colours, matching the design document. `terrain.rs` owns the one
/// definition (it needs the values itself, for [`crate::terrain::blend_rgb`]);
/// re-exported here so every client — terminal or windowed — reaches it
/// through the tuning table instead of a second copy drifting out of sync.
pub use crate::terrain::RGB;

// ----------------------------------------------------------------------
// Writing the live table back out as Rust source.
// ----------------------------------------------------------------------

/// Write the live table back out as Rust, to a *new* file. `src/race.rs` is
/// hand-written and full of comments explaining why each number is what it is;
/// clobbering that from a UI would throw away the part that matters.
pub fn write_table(t: &Tuning) -> std::io::Result<String> {
    // `CHAOS_ROOT` is what the wrapper exports, and it is the only thing that
    // reliably identifies *which* checkout is running. Without it, write beside
    // the caller rather than guessing at a path that may belong to a different
    // copy of the tree.
    let path = match std::env::var("CHAOS_ROOT") {
        Ok(root) => format!("{root}/src/race.tuned.rs"),
        Err(_) => "race.tuned.rs".to_string(),
    };

    let mut s = String::from(
        "// Written by the chaos live view. Not compiled — copy the rows you want\n\
         // into the RACES table in race.rs, keeping the comments there. The\n\
         // TERRAIN_TUNING, CLIMATE_TUNING and ECOLOGY_TUNING constants below\n\
         // follow the same rule: copy fields into terrain.rs / climate.rs /\n\
         // ecology.rs by hand. PROPAGATION_TUNING and BEHAVIOR_TUNING follow the\n\
         // same copy-by-hand rule, into ecology.rs and behavior.rs respectively.\n\n\
         pub const RACES: PerRace<RaceAttrs> = PerRace([\n",
    );
    for race in Race::ALL {
        let a = t.races[race];
        let _ = write!(
            s,
            "    RaceAttrs {{\n        \
             element: Element::{},\n        \
             kind: Kind::{},\n        \
             lifespan: {},\n        \
             lifespan_variance: {},\n        \
             speed: Fx::ratio({}, 100),\n        \
             radius: Fx::ratio({}, 100),\n        \
             consume_unit: {},\n        \
             consume: RateBand::new({}, {}, {}, {}),\n        \
             consume_mix: ChannelMix::new({}, {}, {}, {}, {}),\n        \
             conversion: Conversion::new({}, {}, {}, {}, {}),\n        \
             mining_rate: {},\n        \
             fantasy: {:?},\n    }},\n",
            race.element.name(),
            race.kind.name(),
            a.lifespan,
            a.lifespan_variance,
            (a.speed.raw() as i64 * 100 + 32_768) / 65_536,
            (a.radius.raw() as i64 * 100 + 32_768) / 65_536,
            a.consume_unit,
            a.consume.floor,
            a.consume.nominal,
            a.consume.ceiling,
            a.consume.burst_ticks,
            mix(&a, 0), mix(&a, 1), mix(&a, 2), mix(&a, 3), mix(&a, 4),
            a.conversion.ratio_in,
            a.conversion.ratio_out,
            a.conversion.deposit_share,
            a.conversion.body_share,
            a.conversion.waste_share,
            a.mining_rate,
            a.fantasy,
        );
    }
    s.push_str("]);\n\n");

    let tt = &t.terrain;
    let _ = write!(
        s,
        "pub const TERRAIN_TUNING: TerrainTuning = TerrainTuning {{\n    \
         diffuse_rate: PerElement([{}, {}, {}, {}, {}]),\n    \
         diffuse_cap: PerElement([{}, {}, {}, {}, {}]),\n}};\n\n",
        tt.diffuse_rate[Element::ALL[0]], tt.diffuse_rate[Element::ALL[1]], tt.diffuse_rate[Element::ALL[2]],
        tt.diffuse_rate[Element::ALL[3]], tt.diffuse_rate[Element::ALL[4]],
        tt.diffuse_cap[Element::ALL[0]], tt.diffuse_cap[Element::ALL[1]], tt.diffuse_cap[Element::ALL[2]],
        tt.diffuse_cap[Element::ALL[3]], tt.diffuse_cap[Element::ALL[4]],
    );

    let ct = &t.climate;
    let _ = write!(
        s,
        "pub const CLIMATE_TUNING: ClimateTuning = ClimateTuning {{\n    \
         base_range: PerElement([{:?}, {:?}, {:?}, {:?}, {:?}]),\n    \
         floor: PerElement([{}, {}, {}, {}, {}]),\n    \
         season_peak: PerElement([{}, {}, {}, {}, {}]),\n    \
         season_ticks: {},\n}};\n",
        ct.base_range[Element::ALL[0]], ct.base_range[Element::ALL[1]], ct.base_range[Element::ALL[2]],
        ct.base_range[Element::ALL[3]], ct.base_range[Element::ALL[4]],
        ct.floor[Element::ALL[0]], ct.floor[Element::ALL[1]], ct.floor[Element::ALL[2]],
        ct.floor[Element::ALL[3]], ct.floor[Element::ALL[4]],
        ct.season_peak[Element::ALL[0]], ct.season_peak[Element::ALL[1]], ct.season_peak[Element::ALL[2]],
        ct.season_peak[Element::ALL[3]], ct.season_peak[Element::ALL[4]],
        ct.season_ticks,
    );

    let ec = &t.ecology;
    let _ = write!(
        s,
        "pub const ECOLOGY_TUNING: EcologyTuning = EcologyTuning {{\n    \
         forage_radius: PerElement([Fx::ratio({}, 100), Fx::ratio({}, 100), Fx::ratio({}, 100), \
         Fx::ratio({}, 100), Fx::ratio({}, 100)]),\n    \
         satiation: PerElement([{}, {}, {}, {}, {}]),\n    \
         feed_gain: PerElement([{}, {}, {}, {}, {}]),\n    \
         starve_after: PerElement([{}, {}, {}, {}, {}]),\n    \
         starve_rate: PerElement([{}, {}, {}, {}, {}]),\n    \
         repro_threshold: PerElement([{}, {}, {}, {}, {}]),\n    \
         attrition_rate: PerElement([{}, {}, {}, {}, {}]),\n    \
         suppression_rate: PerElement([{}, {}, {}, {}, {}]),\n}};\n",
        cells(ec.forage_radius[Element::ALL[0]]), cells(ec.forage_radius[Element::ALL[1]]),
        cells(ec.forage_radius[Element::ALL[2]]), cells(ec.forage_radius[Element::ALL[3]]),
        cells(ec.forage_radius[Element::ALL[4]]),
        ec.satiation[Element::ALL[0]], ec.satiation[Element::ALL[1]], ec.satiation[Element::ALL[2]],
        ec.satiation[Element::ALL[3]], ec.satiation[Element::ALL[4]],
        ec.feed_gain[Element::ALL[0]], ec.feed_gain[Element::ALL[1]], ec.feed_gain[Element::ALL[2]],
        ec.feed_gain[Element::ALL[3]], ec.feed_gain[Element::ALL[4]],
        ec.starve_after[Element::ALL[0]], ec.starve_after[Element::ALL[1]], ec.starve_after[Element::ALL[2]],
        ec.starve_after[Element::ALL[3]], ec.starve_after[Element::ALL[4]],
        ec.starve_rate[Element::ALL[0]], ec.starve_rate[Element::ALL[1]], ec.starve_rate[Element::ALL[2]],
        ec.starve_rate[Element::ALL[3]], ec.starve_rate[Element::ALL[4]],
        ec.repro_threshold[Element::ALL[0]], ec.repro_threshold[Element::ALL[1]], ec.repro_threshold[Element::ALL[2]],
        ec.repro_threshold[Element::ALL[3]], ec.repro_threshold[Element::ALL[4]],
        ec.attrition_rate[Element::ALL[0]], ec.attrition_rate[Element::ALL[1]], ec.attrition_rate[Element::ALL[2]],
        ec.attrition_rate[Element::ALL[3]], ec.attrition_rate[Element::ALL[4]],
        ec.suppression_rate[Element::ALL[0]], ec.suppression_rate[Element::ALL[1]], ec.suppression_rate[Element::ALL[2]],
        ec.suppression_rate[Element::ALL[3]], ec.suppression_rate[Element::ALL[4]],
    );

    let pt = &t.propagation;
    let _ = write!(
        s,
        "\npub const PROPAGATION_TUNING: PropagationTuning = PropagationTuning {{\n    \
         period: PerElement([{}, {}, {}, {}, {}]),\n    \
         chance: PerElement([{}, {}, {}, {}, {}]),\n    \
         offspring_size: PerElement([{}, {}, {}, {}, {}]),\n    \
         root_min: PerElement([{}, {}, {}, {}, {}]),\n    \
         dispersal: PerElement([Fx::ratio({}, 100), Fx::ratio({}, 100), Fx::ratio({}, 100), \
         Fx::ratio({}, 100), Fx::ratio({}, 100)]),\n    \
         crowd_max: PerElement([{}, {}, {}, {}, {}]),\n}};\n",
        pt.period[Element::ALL[0]], pt.period[Element::ALL[1]], pt.period[Element::ALL[2]],
        pt.period[Element::ALL[3]], pt.period[Element::ALL[4]],
        pt.chance[Element::ALL[0]], pt.chance[Element::ALL[1]], pt.chance[Element::ALL[2]],
        pt.chance[Element::ALL[3]], pt.chance[Element::ALL[4]],
        pt.offspring_size[Element::ALL[0]], pt.offspring_size[Element::ALL[1]], pt.offspring_size[Element::ALL[2]],
        pt.offspring_size[Element::ALL[3]], pt.offspring_size[Element::ALL[4]],
        pt.root_min[Element::ALL[0]], pt.root_min[Element::ALL[1]], pt.root_min[Element::ALL[2]],
        pt.root_min[Element::ALL[3]], pt.root_min[Element::ALL[4]],
        cells(pt.dispersal[Element::ALL[0]]), cells(pt.dispersal[Element::ALL[1]]),
        cells(pt.dispersal[Element::ALL[2]]), cells(pt.dispersal[Element::ALL[3]]),
        cells(pt.dispersal[Element::ALL[4]]),
        pt.crowd_max[Element::ALL[0]], pt.crowd_max[Element::ALL[1]], pt.crowd_max[Element::ALL[2]],
        pt.crowd_max[Element::ALL[3]], pt.crowd_max[Element::ALL[4]],
    );

    // BehaviorTuning is PerRace-shaped (10 rows, not 5) — build each field's
    // array one line per race, same as the RACES loop above, rather than the
    // 5-wide inline style the Element-scoped constants use.
    let bt = &t.behavior;
    let mut flee = String::new();
    let mut sense = String::new();
    let mut turn = String::new();
    for race in Race::ALL {
        let _ = writeln!(flee, "        {}, // {} {}", bt.flee_threshold[race], race.element.name(), race.kind.name());
        let _ = writeln!(
            sense,
            "        Fx::ratio({}, 100), // {} {}",
            cells(bt.sense_radius[race]), race.element.name(), race.kind.name()
        );
        let _ = writeln!(
            turn,
            "        Fx::ratio({}, 1000), // {} {}",
            (bt.turn_rate[race].raw() as i64 * 1000) / 65_536, race.element.name(), race.kind.name()
        );
    }
    let _ = write!(
        s,
        "\npub const BEHAVIOR_TUNING: BehaviorTuning = BehaviorTuning {{\n    \
         flee_threshold: PerRace([\n{}    ]),\n    \
         sense_radius: PerRace([\n{}    ]),\n    \
         turn_rate: PerRace([\n{}    ]),\n}};\n",
        flee, sense, turn,
    );

    std::fs::write(&path, s)?;
    Ok(path)
}

/// Whole cells, one fractional digit lost to the same rounding `write_table`
/// already accepts for `speed`/`radius` — this file round-trips for a human
/// to copy by hand, not bit-for-bit.
fn cells(v: Fx) -> i64 {
    (v.raw() as i64 * 100 + 32_768) / 65_536
}

fn mix(a: &RaceAttrs, i: usize) -> u16 {
    let c = Channel::ALL[i];
    a.consume_mix.permille(c)
}
