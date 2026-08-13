//! Chaotic Nature — the live view.
//!
//! The tuning loop this project is built around, with the table on the screen
//! instead of in an editor: move the cursor to a number, change it, watch what
//! the world does about it.
//!
//! ## What is and is not deterministic here
//!
//! Retuning mid-run is a *deliberate* break with the replay story. `verify` and
//! `soak` run the shipped table start to finish and reproduce bit-identically;
//! this view lets you change the table under a running world, which no recorded
//! trace could reproduce. That is the trade the instrument exists to make, and
//! the header says `✎ retuned` for as long as the world has been touched. Press
//! `z` to restart from tick 0 with the current knobs and get a clean run back.
//!
//! Restocking and wander are not simulation rules either — they are input
//! commands the view submits, exactly as a player would. Stage 0 has no
//! reproduction and no goals, so without them the map empties out and the
//! survivors travel in straight lines forever.
//!
//! Zero dependencies, ANSI escapes only.

// Presentation only. The simulation this drives never sees a float.
#![allow(clippy::float_arithmetic)]

mod knobs;
mod term;
mod view;

use std::io::{BufWriter, Write};
use std::time::{Duration, Instant};

use pentagram::element::Element;
use pentagram::fx::{Fx, V2};
use pentagram::input::{CmdKind, Command, InputLog};
use pentagram::race::{Kind, Race, RACES, TERRAIN_PERIOD};
use pentagram::rand::{rand_below, rand_signed, Channel};
use pentagram::world::World;

use knobs::Tuning;
use term::{Key, Keys, Term};
use view::{Run, View};

/// 20 frames a second. Sim speed is decoupled from this — a frame runs however
/// many ticks the speed knob asks for.
const FRAME: Duration = Duration::from_millis(50);
/// However fast the speed knob is set, one frame gets this long to simulate
/// before we draw anyway. Without it, winding the speed past what the machine
/// can do would lock the keyboard out.
const FRAME_BUDGET: Duration = Duration::from_millis(40);
/// Ceiling on bodies of one race the restock knob may spawn in a single frame.
/// Restocking 200 at once would be a stampede, not a population — but the
/// allowance has to scale with how much time a frame covers, or a race with an
/// eight-minute lifespan quietly dies out whenever the sim speed is wound up.
const RESTOCK_MAX: u64 = 24;

const WANDER_STEPS: [u32; 7] = [0, 1, 2, 5, 10, 25, 50];

struct Args {
    size: i32,
    pop: u32,
    speed: u64,
    seed: u64,
    rows: Option<usize>,
    cols: Option<usize>,
}

impl Args {
    fn parse() -> Args {
        let mut a = Args {
            size: 96,
            pop: 60,
            speed: 120,
            seed: 0xBEEF,
            rows: None,
            cols: None,
        };
        let argv: Vec<String> = std::env::args().skip(1).collect();
        for (i, arg) in argv.iter().enumerate() {
            let v = argv.get(i + 1).and_then(|s| s.parse::<i64>().ok());
            match arg.as_str() {
                "--size" => a.size = v.unwrap_or(96).clamp(16, 4096) as i32,
                "--pop" => a.pop = v.unwrap_or(60).clamp(0, 250) as u32,
                "--speed" => a.speed = v.unwrap_or(120).clamp(1, 200_000) as u64,
                "--seed" => a.seed = v.unwrap_or(0xBEEF) as u64,
                "--rows" => a.rows = v.map(|n| n.clamp(20, 200) as usize),
                "--cols" => a.cols = v.map(|n| n.clamp(60, 400) as usize),
                "--help" | "-h" => {
                    println!(
                        "chaos — Chaotic Nature live view\n\n\
                         Every race attribute is editable on screen; these only set the\n\
                         starting conditions.\n\n\
                         options:\n  \
                         --size N    world edge in cells        (default 96)\n  \
                         --pop N     bodies per race to seed    (default 60)\n  \
                         --speed N   sim ticks per second       (default 120, real time is 1.67)\n  \
                         --seed N    world seed                 (default 0xBEEF)\n  \
                         --rows N    force view height          (default: ask the terminal)\n  \
                         --cols N    force view width\n\n\
                         keys:\n  \
                         ↑↓←→ or hjkl   move the knob cursor (rows are knobs, columns are races)\n  \
                         - +            adjust the selected knob\n  \
                         [ ]            adjust it ten times as hard\n  \
                         < >            halve / double the sim speed\n  \
                         space          pause      .   advance one simulated minute\n  \
                         tab            knob page  m   show or hide the map\n  \
                         w              cycle how much the view steers bodies around\n  \
                         r / R          reset the selected knob / the whole table\n  \
                         z              restart the world at tick 0 with the current knobs\n  \
                         T              write the current table to src/race.tuned.rs\n  \
                         q              quit\n\n\
                         subcommands (via the wrapper): verify · soak · test · edit · watch"
                    );
                    std::process::exit(0);
                }
                _ => {}
            }
        }
        a
    }
}

/// Everything the runner owns that is not the world or the knobs.
struct Sim {
    speed: u64,
    paused: bool,
    /// Fractional tick carry, so speeds below the frame rate still advance.
    carry: u64,
    /// Ticks queued by `.` while paused.
    stepping: u64,
    wander: usize,
    /// Ticks simulated and wall time spent, for the "× real time" readout.
    ticked: u64,
    since: Instant,
    retuned: bool,
}

fn main() {
    let a = Args::parse();
    let mut t = Tuning::new(a.pop);
    let mut w = World::new(a.seed, a.size);
    w.seed_population(a.pop);

    let mut v = View::new();
    let mut sim = Sim {
        speed: a.speed,
        paused: false,
        carry: 0,
        stepping: 0,
        wander: 3,
        ticked: 0,
        since: Instant::now(),
        retuned: false,
    };

    let term = Term::enter();
    let mut keys = Keys::spawn();
    let mut out = BufWriter::new(std::io::stdout());

    'outer: loop {
        let frame_start = Instant::now();

        for k in keys.poll() {
            if handle(k, &mut v, &mut t, &mut sim, &mut w, a.seed, a.size) {
                break 'outer;
            }
        }
        if keys.closed {
            break;
        }

        // The knobs are the authority; push them at the world every frame. It
        // is a handful of small copies, so there is no point being clever
        // about when — a terrain/climate edit shows up on the very next frame.
        w.retune(t.races);
        w.retune_terrain(t.terrain);
        w.retune_climate(t.climate);
        w.retune_ecology(t.ecology);

        let ticks = if sim.stepping > 0 {
            let n = sim.stepping.min(TERRAIN_PERIOD);
            sim.stepping -= n;
            n
        } else if sim.paused {
            0
        } else {
            sim.carry += sim.speed;
            let n = sim.carry / 20;
            sim.carry %= 20;
            n
        };

        if ticks > 0 {
            let log = inputs(&w, &t, WANDER_STEPS[sim.wander], a.seed, ticks);
            for _ in 0..ticks {
                w.step(&log);
                sim.ticked += 1;
                if frame_start.elapsed() > FRAME_BUDGET {
                    break;
                }
            }
        }

        v.sample(&w);
        let secs = sim.since.elapsed().as_secs_f64();
        let run = Run {
            paused: sim.paused,
            speed: sim.speed,
            realtime: if secs > 1.0 {
                Some((sim.ticked as f64 / secs / 1.667) as u64)
            } else {
                None
            },
            retuned: sim.retuned,
            wander: WANDER_STEPS[sim.wander],
        };

        let (rows, cols) = term.size().unwrap_or((40, 100));
        view::draw(
            &mut out,
            &mut v,
            &w,
            &t,
            &run,
            a.rows.unwrap_or(rows),
            a.cols.unwrap_or(cols),
        );

        if let Some((_, n)) = &mut v.notice {
            *n -= 1;
            if *n == 0 {
                v.notice = None;
            }
        }

        if let Some(rest) = FRAME.checked_sub(frame_start.elapsed()) {
            std::thread::sleep(rest);
        }
    }

    // Order matters: flush our last frame, then hand the terminal back.
    let _ = out.flush();
    drop(out);
    drop(term);
}

/// Returns true when the view should quit.
fn handle(
    k: Key,
    v: &mut View,
    t: &mut Tuning,
    sim: &mut Sim,
    w: &mut World,
    seed: u64,
    size: i32,
) -> bool {
    let knobs = v.knobs();
    let e = v.element();
    // S3.1 compile fix (§11 of the design doc): the view's cursor is still
    // Element-scoped (five columns) — Kind is hardcoded to Animal here until
    // S3.6 adds the real two-axis Kind toggle.
    let race = Race { element: e, kind: Kind::Animal };

    match k {
        Key::Char('q') | Key::Char('\x03') | Key::Esc => return true,

        Key::Up | Key::Char('k') => v.row = v.row.checked_sub(1).unwrap_or(knobs.len() - 1),
        Key::Down | Key::Char('j') => v.row = (v.row + 1) % knobs.len(),
        Key::Left | Key::Char('h') => v.col = (v.col + Element::COUNT - 1) % Element::COUNT,
        Key::Right | Key::Char('l') => v.col = (v.col + 1) % Element::COUNT,

        Key::Char('-') | Key::Char('_') => {
            knobs[v.row].nudge(t, race, false, false);
            sim.retuned = true;
        }
        Key::Char('+') | Key::Char('=') => {
            knobs[v.row].nudge(t, race, true, false);
            sim.retuned = true;
        }
        Key::Char('[') | Key::ShiftLeft => {
            knobs[v.row].nudge(t, race, false, true);
            sim.retuned = true;
        }
        Key::Char(']') | Key::ShiftRight => {
            knobs[v.row].nudge(t, race, true, true);
            sim.retuned = true;
        }

        Key::Char('<') | Key::Char(',') => sim.speed = (sim.speed / 2).max(1),
        Key::Char('>') => sim.speed = (sim.speed * 2).min(200_000),
        Key::Char(' ') => sim.paused = !sim.paused,
        // Freeze, then advance exactly one terrain tick — the granularity the
        // governors actually settle at, so every readout moves once.
        Key::Char('.') => {
            sim.paused = true;
            sim.stepping = TERRAIN_PERIOD;
        }

        Key::Char('\t') => {
            v.page = (v.page + 1) % View::pages();
            v.row = 0;
        }
        Key::Char('m') => v.show_map = !v.show_map,
        Key::Char('w') => {
            sim.wander = (sim.wander + 1) % WANDER_STEPS.len();
            let pct = WANDER_STEPS[sim.wander];
            v.say(format!(
                "wander {pct}% — the view re-steers that share of bodies each second"
            ));
        }

        Key::Char('r') => {
            let shipped = Tuning::new(t.restock[e]);
            let val = knobs[v.row].value(&shipped, race);
            (knobs[v.row].set)(t, race, val);
            v.say(format!("{} {} back to the shipped value", e.name(), knobs[v.row].name));
        }
        Key::Char('R') => {
            *t = Tuning {
                races: RACES,
                restock: t.restock,
                terrain: pentagram::terrain::TerrainTuning::default(),
                climate: pentagram::climate::ClimateTuning::default(),
                ecology: pentagram::ecology::EcologyTuning::default(),
            };
            sim.retuned = false;
            v.say("whole table back to shipped defaults");
        }
        Key::Char('z') => {
            *w = World::new(seed, size);
            w.retune(t.races);
            w.retune_terrain(t.terrain);
            w.retune_climate(t.climate);
            w.retune_ecology(t.ecology);
            for el in Element::ALL {
                // Kind::Animal only — see the `race` binding's own comment above.
                let el_race = Race { element: el, kind: Kind::Animal };
                for _ in 0..t.restock[el] {
                    let x = rand_below(seed, w.tick, w.next_id, Channel::SpawnPlacement, size.max(1) as u32);
                    let y = rand_below(seed, w.tick, w.next_id ^ 0x5A5A, Channel::SpawnPlacement, size.max(1) as u32);
                    w.spawn(el_race, V2::new(Fx::from_int(x as i32), Fx::from_int(y as i32)));
                }
            }
            sim.ticked = 0;
            sim.since = Instant::now();
            sim.retuned = false;
            v.history = pentagram::element::PerElement([vec![], vec![], vec![], vec![], vec![]]);
            v.say("restarted at tick 0 — this run is reproducible from these knobs");
        }
        Key::Char('T') => match write_table(t) {
            Ok(p) => v.say(format!("wrote {p}  (diff it against src/race.rs)")),
            Err(e) => v.say(format!("could not write the table: {e}")),
        },

        _ => {}
    }
    v.clamp();
    false
}

/// The view's own input stream: top the population back up, and steer some of
/// it around. Both are ordinary commands stamped for the ticks about to run,
/// which is the same path a player's input would take.
fn inputs(w: &World, t: &Tuning, wander_pct: u32, seed: u64, ticks: u64) -> InputLog {
    let mut log = InputLog::new();
    let tick = w.tick;
    let size = w.size.floor_int().max(1) as u32;
    let pop = w.population();

    let allowance = (ticks / 20).clamp(1, RESTOCK_MAX) as u32;
    for e in Element::ALL {
        let deficit = t.restock[e].saturating_sub(pop[e]).min(allowance);
        for k in 0..deficit {
            let salt = (e.index() as u32) * 7919 + k;
            let x = rand_below(seed, tick, salt, Channel::SpawnPlacement, size);
            let y = rand_below(seed, tick, salt ^ 0x5A5A, Channel::SpawnPlacement, size);
            log.push(Command {
                tick,
                entity: 0,
                kind: CmdKind::Spawn {
                    element: e,
                    kind: Kind::Animal,
                    at: V2::new(Fx::from_int(x as i32), Fx::from_int(y as i32)),
                },
            });
        }
    }

    // Re-steer `wander_pct` of the population per second, spread across the
    // ticks this frame covers. Without it bodies bounce between walls on rails.
    let n = w.entities.len() as u64;
    if wander_pct > 0 && n > 0 {
        let steers = (n * wander_pct as u64 * ticks / 100 / 100).max(1).min(n);
        for s in 0..steers {
            let at = rand_below(seed, tick, s as u32, Channel::Wander, n as u32) as usize;
            log.push(Command {
                tick: tick + (s * ticks / steers.max(1)),
                entity: w.entities[at].id,
                kind: CmdKind::SetHeading {
                    dir: V2::new(
                        rand_signed(seed, tick, s as u32 + 1, Channel::Wander),
                        rand_signed(seed, tick, s as u32 + 2, Channel::Wander),
                    ),
                },
            });
        }
    }

    log.finalize();
    log
}

/// Write the live table back out as Rust, to a *new* file. `src/race.rs` is
/// hand-written and full of comments explaining why each number is what it is;
/// clobbering that from a UI would throw away the part that matters.
fn write_table(t: &Tuning) -> std::io::Result<String> {
    use std::fmt::Write as _;
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
         // ecology.rs by hand.\n\n\
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
             deposit_unit: {},\n        \
             deposit: RateBand::new({}, {}, {}, {}),\n        \
             deposit_mix: ChannelMix::new({}, {}, {}, {}, {}),\n        \
             consume_unit: {},\n        \
             consume: RateBand::new({}, {}, {}, {}),\n        \
             consume_mix: ChannelMix::new({}, {}, {}, {}, {}),\n        \
             fantasy: {:?},\n    }},\n",
            race.element.name(),
            race.kind.name(),
            a.lifespan,
            a.lifespan_variance,
            (a.speed.raw() as i64 * 100 + 32_768) / 65_536,
            (a.radius.raw() as i64 * 100 + 32_768) / 65_536,
            a.deposit_unit,
            a.deposit.floor,
            a.deposit.nominal,
            a.deposit.ceiling,
            a.deposit.burst_ticks,
            mix(&a, false, 0), mix(&a, false, 1), mix(&a, false, 2), mix(&a, false, 3), mix(&a, false, 4),
            a.consume_unit,
            a.consume.floor,
            a.consume.nominal,
            a.consume.ceiling,
            a.consume.burst_ticks,
            mix(&a, true, 0), mix(&a, true, 1), mix(&a, true, 2), mix(&a, true, 3), mix(&a, true, 4),
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

    std::fs::write(&path, s)?;
    Ok(path)
}

/// Whole cells, one fractional digit lost to the same rounding `write_table`
/// already accepts for `speed`/`radius` — this file round-trips for a human
/// to copy by hand, not bit-for-bit.
fn cells(v: Fx) -> i64 {
    (v.raw() as i64 * 100 + 32_768) / 65_536
}

fn mix(a: &pentagram::race::RaceAttrs, consume: bool, i: usize) -> u16 {
    let c = pentagram::race::Channel::ALL[i];
    if consume {
        a.consume_mix.permille(c)
    } else {
        a.deposit_mix.permille(c)
    }
}