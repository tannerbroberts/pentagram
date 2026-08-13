//! The tuning table, as a thing you can point at.
//!
//! Every knob is one row in [`PAGES`]: a name, how to format it, how big a
//! nudge is, its safe range, and a getter/setter pair. Adding a knob to the
//! live view means adding a row here and nothing else — the grid, the cursor,
//! the stepping and the detail line all fall out of this table.
//!
//! Ranges are not decoration. `deposit_unit` feeds `demand`, which is summed in
//! milli-units over a terrain period; the crate builds with `overflow-checks`
//! in every profile, so a knob with no ceiling is a panic waiting for a curious
//! user. Every `hi` here is chosen to keep the worst case inside `u64`.

use pentagram::climate::ClimateTuning;
use pentagram::ecology::EcologyTuning;
use pentagram::element::{Element, PerElement};
use pentagram::fx::Fx;
use pentagram::race::{Channel, Edge, RaceAttrs, TICKS_PER_DAY, TICKS_PER_MINUTE, RACES};
use pentagram::terrain::TerrainTuning;

/// Everything the live view can change, in one struct.
///
/// `races` is the real tuning table and goes straight into the world.
/// `restock` and `wander` are the *view's* knobs, not the simulation's: Stage 0
/// has no reproduction and no goals, so without a hand on the tiller the map
/// empties out and the survivors travel in straight lines. Both are applied as
/// ordinary input commands, which is exactly what a player would be.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Tuning {
    pub races: PerElement<RaceAttrs>,
    pub restock: PerElement<u32>,
    pub terrain: TerrainTuning,
    pub climate: ClimateTuning,
    pub ecology: EcologyTuning,
}

impl Tuning {
    pub fn new(per_race: u32) -> Tuning {
        Tuning {
            races: RACES,
            restock: PerElement::filled(per_race),
            terrain: TerrainTuning::default(),
            climate: ClimateTuning::default(),
            ecology: EcologyTuning::default(),
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
    pub get: fn(&Tuning, Element) -> i64,
    pub set: fn(&mut Tuning, Element, i64),
}

impl Knob {
    #[inline]
    pub fn value(&self, t: &Tuning, e: Element) -> i64 {
        (self.get)(t, e)
    }

    pub fn nudge(&self, t: &mut Tuning, e: Element, up: bool, coarse: bool) {
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
            get: |$t: &Tuning, $e: Element| -> i64 { $get },
            set: |$tm: &mut Tuning, $em: Element, $v: i64| { $set },
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

static BODY: [Knob; 15] = [
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

    knob!("deposit unit", Fmt::Int, Step::Scale, 1, 100_000_000,
          "total a body writes to the terrain over its ENTIRE life, split across the channels",
          |t, e| t.races[e].deposit_unit as i64,
          |t, e, v| t.races[e].deposit_unit = v as u64),
    edge_knob!("dep floor", deposit, floor, Edge::Floor,
               "granted every terrain tick even at zero demand — the world's own churn"),
    edge_knob!("dep nominal", deposit, nominal, Edge::Nominal,
               "long-run average under sustained demand; the burst bucket refills at this rate"),
    edge_knob!("dep ceiling", deposit, ceiling, Edge::Ceiling,
               "never exceeded in one terrain tick, under any behaviour whatsoever"),
    burst_knob!("dep burst", deposit,
                "terrain ticks of nominal that can be banked, then spent all at once"),

    knob!("consume unit", Fmt::Int, Step::Scale, 1, 100_000_000,
          "total a body takes from the terrain over its entire life",
          |t, e| t.races[e].consume_unit as i64,
          |t, e, v| t.races[e].consume_unit = v as u64),
    edge_knob!("con floor", consume, floor, Edge::Floor,
               "taken every terrain tick even at zero demand"),
    edge_knob!("con nominal", consume, nominal, Edge::Nominal,
               "long-run average consumption under sustained demand"),
    edge_knob!("con ceiling", consume, ceiling, Edge::Ceiling,
               "never exceeded in one terrain tick"),
    burst_knob!("con burst", consume,
                "terrain ticks of nominal consumption that can be banked"),
];

/// One row per channel, twice. The mix is what makes two races with identical
/// rates feel nothing alike, so it gets its own page rather than being buried.
macro_rules! mix_knobs {
    ($prefix:literal, $field:ident, $chan:expr, $help:literal) => {
        knob!($prefix, Fmt::Permille, Step::Add(25), 0, 1000, $help,
              |t, e| t.races[e].$field.permille($chan) as i64,
              |t, e, v| t.races[e].$field.set_rebalanced($chan, v as u16))
    };
}

static MIX: [Knob; 10] = [
    mix_knobs!("dep birth", deposit_mix, Channel::OnBirth,
               "written at the moment of incarnation — fires once per life"),
    mix_knobs!("dep death", deposit_mix, Channel::OnDeath,
               "written by the corpse — fires once per life; dominant for short-lived races"),
    mix_knobs!("dep action", deposit_mix, Channel::OnAction,
               "written by moving — fires every tick a body actually travels"),
    mix_knobs!("dep consume", deposit_mix, Channel::OnConsume,
               "written at the moment of refining what was eaten — one meal per 200 ticks"),
    mix_knobs!("dep existence", deposit_mix, Channel::OnExistence,
               "written by merely being here — once per body per terrain tick"),
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
          |t, e| t.terrain.diffuse_rate[e] as i64,
          |t, e, v| t.terrain.diffuse_rate[e] = v as u16),
    knob!("diffuse cap", Fmt::Int, Step::Add(5), 0, u16::MAX as i64,
          "flat units per edge per terrain tick — no edge can ever carry more than this in one tick",
          |t, e| t.terrain.diffuse_cap[e] as i64,
          |t, e, v| t.terrain.diffuse_cap[e] = v as u16),

    knob!("base lo", Fmt::Int, Step::Add(1), 0, u16::MAX as i64,
          "low end of the one-time per-cell geography draw for this element",
          |t, e| t.climate.base_range[e].0 as i64,
          |t, e, v| t.climate.base_range[e].0 = v as u16),
    knob!("base hi", Fmt::Int, Step::Add(1), 0, u16::MAX as i64,
          "high end of the one-time per-cell geography draw for this element",
          |t, e| t.climate.base_range[e].1 as i64,
          |t, e, v| t.climate.base_range[e].1 = v as u16),
    knob!("climate floor", Fmt::Int, Step::Add(1), 0, u16::MAX as i64,
          "always-on climate influx added every terrain tick, regardless of season",
          |t, e| t.climate.floor[e] as i64,
          |t, e, v| t.climate.floor[e] = v as u16),
    knob!("season peak", Fmt::Int, Step::Add(50), 0, u16::MAX as i64,
          "peak seasonal bonus applied to this element while it is in season",
          |t, e| t.climate.season_peak[e] as i64,
          |t, e, v| t.climate.season_peak[e] = v as u16),
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
          |t, e| (t.ecology.forage_radius[e].raw() as i64 * 100 + 32_768) / 65_536,
          |t, e, v| t.ecology.forage_radius[e] = Fx::ratio(v as i32, 100)),
    knob!("satiation", Fmt::Ticks, Step::Scale, 1, TICKS_PER_DAY as i64,
          "minimum ticks between one body's successful meals",
          |t, e| t.ecology.satiation[e] as i64,
          |t, e, v| t.ecology.satiation[e] = v as u32),
    knob!("feed gain", Fmt::Int, Step::Add(5), 0, 100,
          "hp restored by one successful meal, out of a 0..=100 scale",
          |t, e| t.ecology.feed_gain[e] as i64,
          |t, e, v| t.ecology.feed_gain[e] = v as i32),
    knob!("starve after", Fmt::Ticks, Step::Scale, 1, TICKS_PER_DAY as i64 * 7,
          "ticks without a meal before starvation drain begins",
          |t, e| t.ecology.starve_after[e] as i64,
          |t, e, v| t.ecology.starve_after[e] = v as u32),
    knob!("starve rate", Fmt::Int, Step::Add(1), 0, 100,
          "hp lost per tick once starvation has begun",
          |t, e| t.ecology.starve_rate[e] as i64,
          |t, e, v| t.ecology.starve_rate[e] = v as i32),
    knob!("repro threshold", Fmt::Int, Step::Add(5), 0, 100,
          "hp a meal must reach, from below, to spawn an offspring",
          |t, e| t.ecology.repro_threshold[e] as i64,
          |t, e, v| t.ecology.repro_threshold[e] = v as i32),
    knob!("attrition rate", Fmt::Permille, Step::Add(1), 0, 1000,
          "permille of what eats this element's terrain concentration converted to hp damage each terrain tick",
          |t, e| t.ecology.attrition_rate[e] as i64,
          |t, e, v| t.ecology.attrition_rate[e] = v as u16),
    knob!("suppression rate", Fmt::Permille, Step::Add(1), 0, 1000,
          "permille of this element's suppressor's terrain concentration added to hunger each terrain tick",
          |t, e| t.ecology.suppression_rate[e] as i64,
          |t, e, v| t.ecology.suppression_rate[e] = v as u16),
];

pub static PAGES: &[Page] = &[
    Page { title: "body & rates", knobs: &BODY },
    Page { title: "channel mix ‰  (edits rebalance the rest to keep the sum at 1000)", knobs: &MIX },
    Page { title: "terrain & climate", knobs: &TERRAIN_AND_CLIMATE },
    Page { title: "ecology (S2)", knobs: &ECOLOGY },
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
