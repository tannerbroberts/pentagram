//! Drawing. Everything here is presentation — no value computed in this file
//! ever reaches the simulation.

// Presentation only. The simulation this renders never sees a float.
#![allow(clippy::float_arithmetic)]

use std::io::Write;

use pentagram::element::{Element, PerElement};
use pentagram::race::{Kind, Race, RateBand};
use pentagram::world::World;

use crate::knobs::{abbrev, grouped, Tuning, PAGES};

const SPARK: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
/// Half-cell glyphs, indexed by how crowded the sampled cell is: a lone body
/// gets a mark, a crowd gets a solid half-block.
const UPPER: [char; 5] = ['˙', '▔', '▀', '▀', '▀'];
const LOWER: [char; 5] = ['.', '▁', '▄', '▄', '▄'];
/// Population samples kept. One per frame at 20 fps, so about a minute of wall
/// clock — long enough to see a boom and the bust that follows it.
pub const HISTORY: usize = 48;

/// Element colours, matching the design document.
const RGB: PerElement<(u8, u8, u8)> = PerElement([
    (127, 176, 105), // Wood
    (226, 105, 74),  // Fire
    (211, 164, 69),  // Earth
    (173, 164, 206), // Metal
    (89, 160, 198),  // Water
]);

const DIM: &str = "\x1b[38;2;110;118;134m";
const BRIGHT: &str = "\x1b[38;2;232;234;239m";
const INVERT: &str = "\x1b[7m";
const RESET: &str = "\x1b[0m";
const CLEAR_EOL: &str = "\x1b[K";

fn col(c: (u8, u8, u8)) -> String {
    format!("\x1b[38;2;{};{};{}m", c.0, c.1, c.2)
}

/// Where the cursor is and what is switched on. The only mutable UI state.
pub struct View {
    pub page: usize,
    pub row: usize,
    pub col: usize,
    pub show_map: bool,
    pub history: PerElement<Vec<u32>>,
    /// Transient message shown in the footer, with the frames it has left.
    pub notice: Option<(String, u32)>,
}

impl View {
    pub fn new() -> View {
        View {
            page: 0,
            row: 0,
            col: 0,
            show_map: true,
            history: PerElement([vec![], vec![], vec![], vec![], vec![]]),
            notice: None,
        }
    }

    pub fn say(&mut self, msg: impl Into<String>) {
        self.notice = Some((msg.into(), 60));
    }

    pub fn element(&self) -> Element {
        Element::from_index(self.col)
    }

    pub fn knobs(&self) -> &'static [crate::knobs::Knob] {
        PAGES[self.page].knobs
    }

    pub fn pages() -> usize {
        PAGES.len()
    }

    pub fn sample(&mut self, w: &World) {
        let pop = w.population();
        for e in Element::ALL {
            let h = self.history.get_mut(e);
            h.push(pop[e]);
            if h.len() > HISTORY {
                h.remove(0);
            }
        }
    }

    /// Keep the cursor on a real cell after a page change.
    pub fn clamp(&mut self) {
        self.page %= PAGES.len();
        let n = PAGES[self.page].knobs.len();
        if self.row >= n {
            self.row = n - 1;
        }
        self.col %= Element::COUNT;
    }
}

/// The per-frame numbers the header reports, gathered by the caller because
/// they are about the *runner*, not the world.
pub struct Run {
    pub paused: bool,
    pub speed: u64,
    pub realtime: Option<u64>,
    pub retuned: bool,
    pub wander: u32,
}

pub fn draw(out: &mut impl Write, v: &mut View, w: &World, t: &Tuning, run: &Run, rows: usize, cols: usize) {
    let cols = cols.max(60);
    let knob_rows = v.knobs().len();

    // Everything except the map has a fixed height; the map gets what is left.
    // Below about 30 rows there is nothing left, and the map turns itself off
    // rather than squeezing the readouts that actually mean something.
    // header, race header + 5 races, knob page title + column header + rows +
    // the pressure line, then the detail and key lines.
    let fixed = 1 + 1 + 5 + 1 + 1 + knob_rows + 1 + 2;
    let map_h = if v.show_map { rows.saturating_sub(fixed + 3) } else { 0 };
    let map_h = if map_h < 4 { 0 } else { map_h.min(24) };

    let _ = write!(out, "\x1b[H");
    header(out, w, run, cols);
    if map_h > 0 {
        map(out, w, map_h, cols);
    }
    races(out, v, w, t, cols);
    grid(out, v, t, cols);
    footer(out, v, t, run, cols);
    let _ = write!(out, "\x1b[J");
    let _ = out.flush();
}

fn header(out: &mut impl Write, w: &World, run: &Run, _cols: usize) {
    let min = w.tick / pentagram::race::TERRAIN_PERIOD;
    let rate = match run.realtime {
        Some(x) => format!("{x}× real"),
        None => "measuring…".into(),
    };
    let state = if run.paused {
        format!("{BRIGHT}▮▮ PAUSED{RESET}")
    } else {
        format!("{DIM}▶{RESET} {} t/s", run.speed)
    };
    let tuned = if run.retuned {
        format!("  {BRIGHT}✎ retuned{RESET}")
    } else {
        String::new()
    };
    let _ = writeln!(
        out,
        "{BRIGHT}CHAOTIC NATURE{RESET}{DIM} · live{RESET}   tick {}  ·  {}d {:02}h {:02}m sim  ·  \
         {}  ·  {}  ·  {} alive{DIM}  ·  {} fed, {} starved{RESET}{}{CLEAR_EOL}",
        grouped(w.tick as i64),
        min / 1440,
        (min / 60) % 24,
        min % 60,
        state,
        rate,
        w.alive_count(),
        grouped(w.stats.feedings as i64),
        grouped(w.stats.starved as i64),
        tuned
    );
}

/// The world, at two sampled rows per terminal row.
///
/// A terminal cell is about twice as tall as it is wide, so half-block glyphs
/// buy back exactly the vertical resolution that costs — the sampled grid comes
/// out square, and a hundred wandering bodies read as a field instead of as
/// scattered punctuation. `▀` paints the upper half in the foreground colour
/// and the lower half in the background colour, so one glyph carries two cells.
///
/// The terrain is sampled into the same visual grid and blended (the same
/// integer blend `terrain::render_ppm` uses) into a background colour behind
/// every half-cell an entity doesn't already fully cover — entity glyphs are
/// drawn on top, unchanged.
fn map(out: &mut impl Write, w: &World, h: usize, cols: usize) {
    let mw = (h * 2).min(cols.saturating_sub(4));
    let mh = h * 2;
    let mut counts = vec![[0u16; Element::COUNT]; mw * mh];
    let size = w.size.floor_int().max(1) as usize;
    for e in &w.entities {
        if !e.alive {
            continue;
        }
        let x = ((e.pos.x.floor_int().max(0) as usize) * mw / size).min(mw - 1);
        let y = ((e.pos.y.floor_int().max(0) as usize) * mh / size).min(mh - 1);
        counts[y * mw + x][e.element.index()] += 1;
    }

    let side = w.terrain.side.max(1) as usize;
    let mut sums = vec![PerElement::filled(0u64); mw * mh];
    for y in 0..w.terrain.side {
        for x in 0..w.terrain.side {
            let vx = ((x as usize) * mw / side).min(mw - 1);
            let vy = ((y as usize) * mh / side).min(mh - 1);
            let cell = w.terrain.cell(x, y);
            let sum = &mut sums[vy * mw + vx];
            for e in Element::ALL {
                sum[e] += cell[e] as u64;
            }
        }
    }
    let terrain_bg: Vec<(u8, u8, u8)> = sums.iter().map(pentagram::terrain::blend_rgb).collect();

    /// Dominant element in a sampled cell, and how crowded it is.
    fn occupant(cell: &[u16; Element::COUNT]) -> Option<(usize, usize)> {
        let total: u16 = cell.iter().sum();
        if total == 0 {
            return None;
        }
        let best = cell
            .iter()
            .enumerate()
            .max_by_key(|(_, n)| **n)
            .map(|(i, _)| i)
            .unwrap_or(0);
        Some((best, (total as usize - 1).min(4)))
    }

    let _ = writeln!(out, "{DIM}┌{}┐{RESET}{CLEAR_EOL}", "─".repeat(mw));
    for r in 0..h {
        let _ = write!(out, "{DIM}│{RESET}");
        for c in 0..mw {
            let top = occupant(&counts[(r * 2) * mw + c]);
            let bot = occupant(&counts[(r * 2 + 1) * mw + c]);
            let top_bg = terrain_bg[(r * 2) * mw + c];
            let bot_bg = terrain_bg[(r * 2 + 1) * mw + c];
            match (top, bot) {
                (None, None) => {
                    let _ = write!(
                        out,
                        "\x1b[0m\x1b[48;2;{};{};{}m{}▀",
                        bot_bg.0, bot_bg.1, bot_bg.2,
                        col(top_bg)
                    );
                }
                (Some((e, d)), None) => {
                    let _ = write!(
                        out,
                        "\x1b[0m\x1b[48;2;{};{};{}m{}{}",
                        bot_bg.0, bot_bg.1, bot_bg.2,
                        col(RGB.0[e]),
                        UPPER[d]
                    );
                }
                (None, Some((e, d))) => {
                    let _ = write!(
                        out,
                        "\x1b[0m\x1b[48;2;{};{};{}m{}{}",
                        top_bg.0, top_bg.1, top_bg.2,
                        col(RGB.0[e]),
                        LOWER[d]
                    );
                }
                // Both halves occupied: a full half-block splits the cell
                // cleanly between the two colours, and the crowding is already
                // implied by there being something in both.
                (Some((a, _)), Some((b, _))) => {
                    let bg = RGB.0[b];
                    let _ = write!(
                        out,
                        "\x1b[48;2;{};{};{}m{}▀",
                        bg.0, bg.1, bg.2,
                        col(RGB.0[a])
                    );
                }
            }
        }
        let _ = writeln!(out, "{RESET}{DIM}│{RESET}{CLEAR_EOL}");
    }
    let _ = writeln!(
        out,
        "{DIM}└{}┘  terrain dominant-element colour behind bodies{RESET}{CLEAR_EOL}",
        "─".repeat(mw)
    );
}

fn races(out: &mut impl Write, v: &View, w: &World, t: &Tuning, _cols: usize) {
    let _ = writeln!(
        out,
        "{DIM}race    alive  population        deposit: floor╵ nominal┊ granted▓ ceiling→ \
         │      granted state{RESET}{CLEAR_EOL}"
    );

    let peak = Element::ALL
        .iter()
        .flat_map(|e| v.history[*e].iter().copied())
        .max()
        .unwrap_or(1)
        .max(1);

    let pop = w.population();
    for e in Element::ALL {
        // S3.1 compile fix: this row stays Element-scoped and shows the
        // Animal governor/band only, same Kind::Animal hardcoding as the
        // rest of the S3.1 chaos TUI — see `main.rs`'s `race` binding.
        let race = Race { element: e, kind: Kind::Animal };
        let g = w.last_deposit[race];
        let band = t.races[race].deposit;
        let c = col(RGB[e]);
        let state = if pop[e] == 0 {
            format!("{DIM}extinct — still churning at its floor{RESET}")
        } else if g.clipped > 0 {
            format!("{c}rate-limited{RESET}{DIM} — {} refused{RESET}", abbrev(g.clipped as i64))
        } else if g.forced > 0 {
            format!("{DIM}idle — floor is doing the work{RESET}")
        } else {
            format!("{DIM}inside its band{RESET}")
        };
        let _ = writeln!(
            out,
            "{c}{:<7}{RESET}{:>5}  {c}{:<16}{RESET} {} {:>7}/{:<7} {}{CLEAR_EOL}",
            e.name(),
            pop[e],
            spark(&v.history[e], peak, 16),
            gauge(g.granted, band, &c, 26),
            g.granted,
            band.ceiling,
            state
        );
    }
}

/// The band, drawn to scale from 0 to the ceiling: filled to `granted`, with
/// the floor and nominal marked in place. This is the whole governor story in
/// one line — where the rate is allowed to sit, and where it actually is.
fn gauge(granted: u64, b: RateBand, c: &str, w: usize) -> String {
    let ceil = b.ceiling.max(1) as u64;
    let at = |x: u64| ((x * w as u64) / ceil).min(w as u64 - 1) as usize;
    let fill = ((granted * w as u64) / ceil).min(w as u64) as usize;

    let mut chars = vec!['░'; w];
    for ch in chars.iter_mut().take(fill) {
        *ch = '▓';
    }
    chars[at(b.floor as u64)] = '╵';
    chars[at(b.nominal as u64)] = '┊';

    let lit: String = chars[..fill].iter().collect();
    let rest: String = chars[fill..].iter().collect();
    format!("{DIM}│{c}{lit}{DIM}{rest}│{RESET}")
}

fn spark(h: &[u32], max: u32, w: usize) -> String {
    let start = h.len().saturating_sub(w);
    let mut s: String = h[start..]
        .iter()
        .map(|v| SPARK[(((*v as u64 * 7) / max.max(1) as u64).min(7)) as usize])
        .collect();
    while s.chars().count() < w {
        s.insert(0, ' ');
    }
    s
}

fn grid(out: &mut impl Write, v: &View, t: &Tuning, cols: usize) {
    let knobs = v.knobs();
    let cw = ((cols.saturating_sub(20)) / 5).clamp(8, 14);

    let _ = writeln!(
        out,
        "{DIM}── knobs ── {}   (tab: page {}/{}){RESET}{CLEAR_EOL}",
        PAGES[v.page].title,
        v.page + 1,
        PAGES.len()
    );

    let mut hdr = format!("{:<18}", "");
    for e in Element::ALL {
        hdr.push_str(&format!("{}{:>w$}{RESET}", col(RGB[e]), e.name(), w = cw));
    }
    let _ = writeln!(out, "{hdr}{CLEAR_EOL}");

    for (i, k) in knobs.iter().enumerate() {
        let selected_row = i == v.row;
        let label = if selected_row {
            format!("{BRIGHT}{:<18}{RESET}", k.name)
        } else {
            format!("{DIM}{:<18}{RESET}", k.name)
        };
        let mut line = label;
        for e in Element::ALL {
            let cell = k.short(k.value(t, Race { element: e, kind: Kind::Animal }));
            if selected_row && e.index() == v.col {
                line.push_str(&format!("{INVERT}{:>w$}{RESET}", cell, w = cw));
            } else if selected_row {
                line.push_str(&format!("{BRIGHT}{:>w$}{RESET}", cell, w = cw));
            } else {
                line.push_str(&format!("{:>w$}", cell, w = cw));
            }
        }
        let _ = writeln!(out, "{line}{CLEAR_EOL}");
    }

    // §3.1, computed rather than hoped for. `deposit_unit / lifespan` is what
    // keeps a race that lives eight minutes from reshaping the map faster than
    // one that lives a fortnight, and it is the first thing a lifespan or
    // deposit-unit edit breaks.
    let p: Vec<u64> = Element::ALL
        .iter()
        .map(|e| t.races[Race { element: *e, kind: Kind::Animal }].terraform_pressure())
        .collect();
    let (lo, hi) = (
        p.iter().copied().min().unwrap_or(1).max(1),
        p.iter().copied().max().unwrap_or(1),
    );
    let spread = hi * 100 / lo;
    let verdict = if spread <= 200 {
        format!("{DIM}spread {}.{:02}× — inside parity{RESET}", spread / 100, spread % 100)
    } else {
        format!("{BRIGHT}spread {}.{:02}× — OUTSIDE the 2× parity band{RESET}", spread / 100, spread % 100)
    };
    let mut line = format!("{DIM}{:<18}{RESET}", "pressure");
    for e in Element::ALL {
        line.push_str(&format!("{DIM}{:>w$}{RESET}", p[e.index()], w = cw));
    }
    let _ = writeln!(out, "{line}  {verdict}{CLEAR_EOL}");
}

fn footer(out: &mut impl Write, v: &View, t: &Tuning, run: &Run, _cols: usize) {
    let k = &v.knobs()[v.row];
    let e = v.element();
    let stepping = match k.step {
        crate::knobs::Step::Add(n) => format!("-/+ ±{n}   [/] ±{}", n * 10),
        crate::knobs::Step::Scale => "-/+ ±10%   [/] ×2".to_string(),
    };
    let _ = writeln!(
        out,
        "{}▸ {} {}{RESET} = {BRIGHT}{}{RESET}   {DIM}{}   ·   {}{RESET}{CLEAR_EOL}",
        col(RGB[e]),
        e.name(),
        k.name,
        k.long(k.value(t, Race { element: e, kind: Kind::Animal })),
        stepping,
        k.help
    );

    match &v.notice {
        Some((m, _)) => {
            let _ = writeln!(out, "{BRIGHT}{m}{RESET}{CLEAR_EOL}");
        }
        None => {
            let _ = writeln!(
                out,
                "{DIM}↑↓←→/hjkl move · -/+ [/] adjust · space pause · < > speed {} t/s · \
                 . advance 1 min · w wander {}% · tab page · m map · r/R reset · z restart · \
                 T write table · q quit{RESET}{CLEAR_EOL}",
                run.speed, run.wander
            );
        }
    }
}
