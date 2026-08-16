//! Pentagram — the windowed client.
//!
//! A 2D map you can point at: every body — plant and animal alike — drawn at
//! its real position, the terrain coloured by what has soaked into it, and
//! the same tuning table the terminal client (`chaos tui`) drives, so a
//! slider dragged here and a knob nudged there move the identical numbers.
//! There is no player-embodiment layer in this design — pentagram is pure
//! spectator/tuning, unlike the sibling project this binary's structure is
//! adapted from — so every interaction here is either a view toggle or a
//! tuning edit; nothing here ever steers a body or claims one.
//!
//! `eframe`/`egui` are presentation dependencies, quarantined to this binary
//! (see `Cargo.toml`'s own comment). The simulation library underneath
//! remains zero-dependency and never sees a float — this file reads sim
//! state and submits ordinary input commands (`CmdKind::Spawn`,
//! `CmdKind::SetTarget`), exactly the way the terminal client does.

#![allow(clippy::float_arithmetic)]

use std::collections::VecDeque;
use std::time::Instant;

use eframe::egui::{self, Color32, Pos2, Rect, RichText, Sense, Stroke, Vec2};

use pentagram::element::{Element, PerElement};
use pentagram::entity::Entity;
use pentagram::input::{CmdKind, Command, InputLog};
use pentagram::race::{Kind, PerRace, Race, TERRAIN_PERIOD};
use pentagram::rand::{rand_below, Channel};
use pentagram::terrain::blend_rgb;
use pentagram::tile::Tile;
use pentagram::tuning::{abbrev, grouped, write_table, Axis, Knob, Tuning, PAGES, RGB};
use pentagram::world::World;

/// Population-history samples kept per race, ring-buffered.
const HISTORY: usize = 900;
/// Ceiling on bodies of one race the restock logic spawns per frame.
const RESTOCK_MAX: u64 = 24;

fn main() -> eframe::Result {
    let a = Args::parse();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 880.0])
            .with_min_inner_size([900.0, 620.0])
            .with_title("Chaotic Nature — pentagram"),
        ..Default::default()
    };
    eframe::run_native(
        "Chaotic Nature — pentagram",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, a)))),
    )
}

struct Args {
    size: i32,
    pop: u32,
    speed: f64,
    seed: u64,
}

impl Args {
    fn parse() -> Args {
        let mut a = Args { size: 96, pop: 60, speed: 120.0, seed: 0xBEEF };
        let argv: Vec<String> = std::env::args().skip(1).collect();
        for (i, arg) in argv.iter().enumerate() {
            let v = argv.get(i + 1).and_then(|s| s.parse::<i64>().ok());
            match arg.as_str() {
                "--size" => a.size = v.unwrap_or(96).clamp(16, 4096) as i32,
                "--pop" => a.pop = v.unwrap_or(60).clamp(0, 250) as u32,
                "--speed" => a.speed = v.unwrap_or(120).clamp(1, 200_000) as f64,
                "--seed" => a.seed = v.unwrap_or(0xBEEF) as u64,
                _ => {}
            }
        }
        a
    }
}

/// Pure view state: which bodies and which terrain layers get drawn. Never
/// touches `World` or `Tuning` — hiding a race must never affect the
/// simulation, only what this frame paints.
struct Visibility {
    animals_on: bool,
    plants_on: bool,
    terrain_on: bool,
    animal_el: PerElement<bool>,
    plant_el: PerElement<bool>,
    terrain_el: PerElement<bool>,
}

impl Default for Visibility {
    fn default() -> Visibility {
        Visibility {
            animals_on: true,
            plants_on: true,
            terrain_on: true,
            animal_el: PerElement::filled(true),
            plant_el: PerElement::filled(true),
            terrain_el: PerElement::filled(true),
        }
    }
}

impl Visibility {
    /// A body is visible only if both its Kind master toggle and its own
    /// per-element checkbox are on.
    fn body(&self, e: &Entity) -> bool {
        match e.kind {
            Kind::Animal => self.animals_on && self.animal_el[e.element],
            Kind::Plant => self.plants_on && self.plant_el[e.element],
        }
    }
}

struct App {
    w: World,
    t: Tuning,
    seed: u64,
    size: i32,
    pop: u32,

    speed: f64,
    paused: bool,
    carry: f64,
    last: Instant,
    retuned: bool,
    wander_pct: u32,

    history: PerRace<VecDeque<u32>>,
    frame: u64,
    notice: Option<(String, Instant)>,

    terrain_tex: Option<egui::TextureHandle>,
    knob_element: Element,
    knob_kind: Kind,
    vis: Visibility,
    show_help: bool,
}

impl App {
    fn new(_cc: &eframe::CreationContext<'_>, a: Args) -> App {
        let t = Tuning::new(a.pop);
        let mut w = World::new(a.seed, a.size);
        w.seed_population(a.pop);
        App {
            w,
            t,
            seed: a.seed,
            size: a.size,
            pop: a.pop,
            speed: a.speed,
            paused: false,
            carry: 0.0,
            last: Instant::now(),
            retuned: false,
            wander_pct: 5,
            history: PerRace(Race::ALL.map(|_| VecDeque::new())),
            frame: 0,
            notice: None,
            terrain_tex: None,
            knob_element: Element::Fire,
            knob_kind: Kind::Animal,
            vis: Visibility::default(),
            show_help: true,
        }
    }

    fn say(&mut self, msg: impl Into<String>) {
        self.notice = Some((msg.into(), Instant::now()));
    }

    fn restart(&mut self, seed: u64) {
        self.seed = seed;
        self.w = World::new(seed, self.size);
        self.w.retune(self.t.races.clone());
        self.w.retune_terrain(self.t.terrain);
        self.w.retune_ecology(self.t.ecology);
        self.w.retune_behavior(self.t.behavior);
        self.w.retune_propagation(self.t.propagation);
        self.w.seed_population(self.pop);
        for r in Race::ALL {
            self.history.get_mut(r).clear();
        }
    }

    // ------------------------------------------------------------------
    // Simulation driving — identical in spirit to the terminal client:
    // everything the view does to the world goes through input commands.
    // ------------------------------------------------------------------

    fn advance(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.last).as_secs_f64().min(0.25);
        self.last = now;
        if self.paused {
            return;
        }
        self.carry += dt * self.speed;
        let mut ticks = self.carry.floor() as u64;
        self.carry -= ticks as f64;
        ticks = ticks.min(50_000);
        if ticks == 0 {
            return;
        }

        // The knobs are the authority; push them at the world every frame,
        // same as the terminal client's main loop.
        self.w.retune(self.t.races.clone());
        self.w.retune_terrain(self.t.terrain);
        self.w.retune_ecology(self.t.ecology);
        self.w.retune_behavior(self.t.behavior);
        self.w.retune_propagation(self.t.propagation);

        let log = self.inputs(ticks);
        let budget = Instant::now();
        for _ in 0..ticks {
            self.w.step(&log);
            if budget.elapsed().as_millis() > 12 {
                break;
            }
        }
    }

    /// The view's own input stream: top the population back up per race, and
    /// steer some of it around. Both are ordinary commands stamped for the
    /// ticks about to run — the same path a player's input would take. See
    /// `src/bin/chaos/main.rs`'s `inputs`, adapted from per-Element to
    /// per-Race so a Plant and an Animal of the same element restock
    /// independently.
    fn inputs(&mut self, ticks: u64) -> InputLog {
        let mut log = InputLog::new();
        let tick = self.w.tick;
        let size = self.w.size.max(1) as u32;

        let mut pop: PerRace<u32> = PerRace::filled(0);
        for e in &self.w.entities {
            if e.alive {
                *pop.get_mut(e.race()) += 1;
            }
        }

        let allowance = (ticks / 20).clamp(1, RESTOCK_MAX) as u32;
        for r in Race::ALL {
            let deficit = self.t.restock[r].saturating_sub(pop[r]).min(allowance);
            for k in 0..deficit {
                let salt = (r.index() as u32) * 7919 + k;
                let x = rand_below(self.seed, tick, salt, Channel::SpawnPlacement, size);
                let y = rand_below(self.seed, tick, salt ^ 0x5A5A, Channel::SpawnPlacement, size);
                log.push(Command {
                    tick,
                    entity: 0,
                    kind: CmdKind::Spawn {
                        element: r.element,
                        kind: r.kind,
                        at: Tile::new(x as i32, y as i32),
                    },
                });
            }
        }

        // No longer load-bearing the way it was under continuous movement
        // (Graze already re-rolls a random step every eligible tick on its
        // own now), but still exercises `SetTarget` as ordinary
        // player-shaped input, same as before.
        let n = self.w.entities.len() as u64;
        if self.wander_pct > 0 && n > 0 {
            let steers = (n * self.wander_pct as u64 * ticks / 100 / 100).max(1).min(n);
            for s in 0..steers {
                let at = rand_below(self.seed, tick, s as u32, Channel::Wander, n as u32) as usize;
                let id = self.w.entities[at].id;
                let to = Tile::new(
                    rand_below(self.seed, tick, s as u32 + 1, Channel::Wander, size) as i32,
                    rand_below(self.seed, tick, s as u32 + 2, Channel::Wander, size) as i32,
                );
                log.push(Command {
                    tick: tick + (s * ticks / steers.max(1)),
                    entity: id,
                    kind: CmdKind::SetTarget { to: Some(to) },
                });
            }
        }

        log.finalize();
        log
    }
}

// ----------------------------------------------------------------------
// Colours.
// ----------------------------------------------------------------------

fn ecolor(e: Element) -> Color32 {
    let c = RGB[e];
    Color32::from_rgb(c.0, c.1, c.2)
}

/// Matches `terrain::blend_rgb`'s own fallback when every saturation is zero
/// — kept as a separate constant here only because that one is a private
/// item inside `blend_rgb`'s body.
const GROUND: Color32 = Color32::from_rgb(18, 18, 24);

// ----------------------------------------------------------------------
// The frame.
// ----------------------------------------------------------------------

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.advance();

        self.frame += 1;
        if self.frame.is_multiple_of(3) {
            let mut counts: PerRace<u32> = PerRace::filled(0);
            for e in self.w.entities.iter().filter(|e| e.alive) {
                *counts.get_mut(e.race()) += 1;
            }
            for r in Race::ALL {
                let h = self.history.get_mut(r);
                h.push_back(counts[r]);
                if h.len() > HISTORY {
                    h.pop_front();
                }
            }
        }

        if ctx.input(|i| i.key_pressed(egui::Key::Space)) {
            self.paused = !self.paused;
        }

        self.top_bar(ctx);
        self.side_panel(ctx);
        self.map_panel(ctx);

        ctx.request_repaint();
    }
}

impl App {
    fn top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("PENTAGRAM").strong().size(16.0));
                ui.separator();

                let btn = if self.paused { "▶ resume" } else { "⏸ pause" };
                if ui.button(btn).clicked() {
                    self.paused = !self.paused;
                }

                ui.label("speed");
                ui.add(
                    egui::Slider::new(&mut self.speed, 1.0..=200_000.0)
                        .logarithmic(true)
                        .custom_formatter(|v, _| format!("{v:.0} t/s"))
                        .custom_parser(|s| s.parse::<f64>().ok()),
                )
                .on_hover_text("Simulation ticks per second — matches the terminal client's --speed range.");

                ui.separator();
                let min = self.w.tick / TERRAIN_PERIOD;
                ui.label(format!("day {} · {:02}:{:02}", min / 1440, (min / 60) % 24, min % 60))
                    .on_hover_text("Simulated time. One terrain tick — when the ground itself updates — is one simulated minute.");

                ui.separator();
                ui.label(format!("{} alive", self.w.alive_count()));

                if self.retuned {
                    ui.separator();
                    ui.label(RichText::new("✎ retuned").italics()).on_hover_text(
                        "Knobs have been changed while this world runs — this run is no longer reproducible from the shipped tables. Restart to get a clean run.",
                    );
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some((msg, at)) = &self.notice {
                        if at.elapsed().as_secs_f32() < 6.0 {
                            ui.label(RichText::new(msg).strong());
                        } else {
                            self.notice = None;
                        }
                    }
                });
            });
        });
    }

    fn side_panel(&mut self, ctx: &egui::Context) {
        egui::SidePanel::right("side")
            .resizable(true)
            .default_width(380.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    self.visibility_card(ui);
                    ui.add_space(8.0);
                    self.terrain_summary_card(ui);
                    ui.add_space(8.0);
                    self.population_card(ui);
                    ui.add_space(8.0);
                    self.knobs_card(ui);
                    ui.add_space(8.0);
                    self.legend_card(ui);
                });
            });
    }

    fn visibility_card(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new(RichText::new("Visibility").strong())
            .default_open(true)
            .show(ui, |ui| {
                ui.checkbox(&mut self.vis.animals_on, "Animals")
                    .on_hover_text("Master toggle — hides every Animal race's bodies at once.");
                ui.indent("vis_animals", |ui| {
                    ui.horizontal_wrapped(|ui| {
                        for e in Element::ALL {
                            ui.checkbox(&mut self.vis.animal_el[e], RichText::new(e.name()).color(ecolor(e)));
                        }
                    });
                });

                ui.checkbox(&mut self.vis.plants_on, "Plants")
                    .on_hover_text("Master toggle — hides every Plant race's bodies at once.");
                ui.indent("vis_plants", |ui| {
                    ui.horizontal_wrapped(|ui| {
                        for e in Element::ALL {
                            ui.checkbox(&mut self.vis.plant_el[e], RichText::new(e.name()).color(ecolor(e)));
                        }
                    });
                });

                ui.checkbox(&mut self.vis.terrain_on, "Terrain colouring")
                    .on_hover_text("Off: the map paints flat ground instead of blended terrain saturations.");
                ui.indent("vis_terrain", |ui| {
                    ui.horizontal_wrapped(|ui| {
                        for e in Element::ALL {
                            ui.checkbox(&mut self.vis.terrain_el[e], RichText::new(e.name()).color(ecolor(e)));
                        }
                    });
                });
            });
    }

    fn terrain_summary_card(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("Terrain concentration (map-wide)").strong());
        let mut totals: PerElement<u64> = PerElement::filled(0);
        for e in Element::ALL {
            totals[e] = self.w.terrain.total(e);
        }
        let max = Element::ALL.iter().map(|e| totals[*e]).max().unwrap_or(1).max(1);
        for e in Element::ALL {
            let v = totals[e];
            let frac = (v as f32 / max as f32).clamp(0.0, 1.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{:<6}", e.name())).color(ecolor(e)));
                let (r, _) = ui.allocate_exact_size(Vec2::new(140.0, 10.0), Sense::hover());
                let p = ui.painter_at(r);
                p.rect_filled(r, 2.0, Color32::from_gray(35));
                let mut fr = r;
                fr.set_width(r.width() * frac.max(0.01));
                p.rect_filled(fr, 2.0, ecolor(e));
                ui.label(abbrev(v as i64)).on_hover_text(format!("{} exactly", grouped(v as i64)));
            });
        }
    }

    fn population_card(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("Population — solid = Animal, dashed = Plant").strong());
        let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 110.0), Sense::hover());
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 3.0, GROUND);
        let len = self.history[Race::ALL[0]].len();
        if len < 2 {
            return;
        }
        let peak = Race::ALL
            .iter()
            .flat_map(|r| self.history[*r].iter().copied())
            .max()
            .unwrap_or(1)
            .max(1) as f32;
        for r in Race::ALL {
            let h = &self.history[r];
            if h.len() < 2 {
                continue;
            }
            let pts: Vec<Pos2> = h
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    let x = rect.left() + i as f32 / (len - 1) as f32 * rect.width();
                    let y = rect.bottom() - (*v as f32 / peak) * (rect.height() - 4.0) - 2.0;
                    Pos2::new(x, y)
                })
                .collect();
            let stroke = Stroke::new(1.5_f32, ecolor(r.element));
            if r.kind == Kind::Animal {
                p.add(egui::Shape::line(pts, stroke));
            } else {
                // Dashed: draw every other segment, so the Plant series of a
                // colour reads as related to, but distinct from, its Animal
                // twin drawn solid above.
                for w in pts.windows(2).step_by(2) {
                    p.line_segment([w[0], w[1]], stroke);
                }
            }
        }
    }

    fn knobs_card(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new(RichText::new("Tuning — speed, lifespan, deposition, consumption…").strong())
            .default_open(false)
            .show(ui, |ui| {
                ui.label(RichText::new("Pick an element and a kind, drag a slider — the running world updates. Hover any slider for what it does.").weak());

                ui.horizontal_wrapped(|ui| {
                    for e in Element::ALL {
                        let sel = self.knob_element == e;
                        if ui.selectable_label(sel, RichText::new(e.name()).color(ecolor(e))).clicked() {
                            self.knob_element = e;
                        }
                    }
                });
                ui.horizontal(|ui| {
                    for k in [Kind::Animal, Kind::Plant] {
                        let sel = self.knob_kind == k;
                        if ui.selectable_label(sel, k.name()).clicked() {
                            self.knob_kind = k;
                        }
                    }
                });
                ui.separator();

                let race = Race { element: self.knob_element, kind: self.knob_kind };
                for page in PAGES {
                    egui::CollapsingHeader::new(page.title)
                        .default_open(page.title.starts_with("body"))
                        .show(ui, |ui| {
                            if page.axis == Axis::Element {
                                ui.label(RichText::new("element-scoped — the Kind selector above has no effect on this page").weak());
                            }
                            for k in page.knobs {
                                knob_slider(ui, k, &mut self.t, race, &mut self.retuned);
                            }
                        });
                }

                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("wander steering");
                    ui.add(egui::Slider::new(&mut self.wander_pct, 0..=50).suffix("%"))
                        .on_hover_text("Share of the population the view re-steers each second, as ordinary input commands. Without it, bodies drift on straight rails between real behaviour's flee/hunt/graze decisions.");
                });

                ui.horizontal(|ui| {
                    if ui.button("Restart world").on_hover_text("Same seed, tick 0, current knobs — a reproducible run.").clicked() {
                        self.restart(self.seed);
                        self.retuned = false;
                        self.say("Restarted at tick 0 — this run is reproducible from these knobs.");
                    }
                    if ui.button("New world").on_hover_text("A different seed: fresh population placement and randomness, same knobs.").clicked() {
                        let seed = self.seed.wrapping_add(1);
                        self.restart(seed);
                        self.say(format!("New world — seed {seed:#x}."));
                    }
                    if ui.button("Write tuned table").on_hover_text("Dump the current tables to src/race.tuned.rs to copy back into the source.").clicked() {
                        match write_table(&self.t) {
                            Ok(p) => self.say(format!("Wrote {p}")),
                            Err(e) => self.say(format!("Could not write the table: {e}")),
                        }
                    }
                });
            });
    }

    fn legend_card(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new(RichText::new("What am I looking at?").strong())
            .default_open(self.show_help)
            .show(ui, |ui| {
                self.show_help = false;
                ui.label("Each tile blends its five elemental saturations into one colour — the terrain remembering what has lived, died, moved and fed on it.");
                ui.horizontal_wrapped(|ui| {
                    for e in Element::ALL {
                        ui.label(RichText::new(format!("■ {}", e.name())).color(ecolor(e)));
                    }
                });
                ui.add_space(4.0);
                ui.label("• ● circles — Animal bodies, coloured by element");
                ui.label("• ■ squares — Plant bodies, coloured by element; a seedling's square grows toward full size as it matures");
                ui.label("• hover any ground cell for its exact per-element saturations");
                ui.label("• the Visibility card's checkboxes hide bodies and terrain layers for this view only — the simulation underneath never notices");
                ui.add_space(4.0);
                ui.label(RichText::new(
                    "The cycle: everything eats its neighbour — Wood→Fire→Earth→Metal→Water→Wood. \
                     Corpses and mere existence feed the ground; plants root where their own element \
                     runs rich; animals flee what hunts them, hunt what they can catch, and graze what \
                     they can't. Kind (Plant/Animal) is a second axis crossed with the five-element \
                     ring — every element has both a rooted race and a mobile one.",
                ).weak());
            });
    }

    // ------------------------------------------------------------------
    // The map.
    // ------------------------------------------------------------------

    fn map_panel(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(Color32::from_rgb(10, 11, 15)))
            .show(ctx, |ui| {
                let side = self.w.terrain.side.max(1) as usize;

                // Terrain texture, rebuilt every frame.
                let mut img = egui::ColorImage::new([side, side], GROUND);
                for (i, px) in img.pixels.iter_mut().enumerate() {
                    let x = (i % side) as i32;
                    let y = (i / side) as i32;
                    let cell = self.w.terrain.cell(x, y);
                    let mut sat: PerElement<u64> = PerElement::filled(0);
                    if self.vis.terrain_on {
                        for e in Element::ALL {
                            if self.vis.terrain_el[e] {
                                sat[e] = cell[e] as u64;
                            }
                        }
                    }
                    let (r, g, b) = blend_rgb(&sat);
                    *px = Color32::from_rgb(r, g, b);
                }
                // NEAREST, not LINEAR: cells are tiles, and tiles have edges.
                let opts = egui::TextureOptions::NEAREST;
                match &mut self.terrain_tex {
                    Some(t) => t.set(img, opts),
                    None => self.terrain_tex = Some(ctx.load_texture("terrain", img, opts)),
                }

                let avail = ui.available_rect_before_wrap();
                let dim = avail.width().min(avail.height());
                let rect = Rect::from_center_size(avail.center(), Vec2::splat(dim));
                let (resp, painter) = ui.allocate_painter(avail.size(), Sense::hover());

                if let Some(tex) = &self.terrain_tex {
                    painter.image(
                        tex.id(),
                        rect,
                        Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0)),
                        Color32::WHITE,
                    );
                }
                painter.rect_stroke(rect, 0.0, Stroke::new(1.0_f32, Color32::from_gray(60)));

                let scale = rect.width() / side as f32;
                let to_screen = |x: f32, y: f32| -> Pos2 {
                    Pos2::new(rect.left() + x * scale, rect.top() + y * scale)
                };
                let to_world = |p: Pos2| -> (f32, f32) {
                    ((p.x - rect.left()) / scale, (p.y - rect.top()) / scale)
                };

                // Bodies — Animal drawn as filled circles, Plant as filled
                // squares, so the two Kinds read apart at a glance.
                for ent in &self.w.entities {
                    if !ent.alive || !self.vis.body(ent) {
                        continue;
                    }
                    // +0.5: centre the sprite within its tile rather than
                    // pinning it to the tile's top-left corner.
                    let p = to_screen(ent.pos.x as f32 + 0.5, ent.pos.y as f32 + 0.5);
                    let full_r = (self.t.races[ent.race()].radius.to_f32_render() * scale * 0.6).clamp(1.5, 8.0);
                    let grow = (ent.size as f32 / 1000.0).clamp(0.15, 1.0);
                    let r = full_r * grow;
                    let c = ecolor(ent.element);
                    match ent.kind {
                        Kind::Animal => {
                            painter.circle_filled(p, r, c);
                        }
                        Kind::Plant => {
                            let sq = Rect::from_center_size(p, Vec2::splat(r * 1.7));
                            painter.rect_filled(sq, 1.0, c);
                        }
                    }
                }

                // Hover: exact per-element saturations of the cell under the
                // pointer.
                if let Some(hp) = resp.hover_pos() {
                    if rect.contains(hp) {
                        let (wx, wy) = to_world(hp);
                        let cx = (wx as i32).clamp(0, side as i32 - 1);
                        let cy = (wy as i32).clamp(0, side as i32 - 1);
                        let cell = self.w.terrain.cell(cx, cy);
                        egui::show_tooltip_at_pointer(ctx, ui.layer_id(), egui::Id::new("celltip"), |ui| {
                            ui.label(RichText::new(format!("ground at ({cx}, {cy})")).weak());
                            for e in Element::ALL {
                                let s = cell[e];
                                let frac = s as f32 / 65_535.0;
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(format!("{:<6}", e.name())).color(ecolor(e)));
                                    let (r, _) = ui.allocate_exact_size(Vec2::new(80.0, 8.0), Sense::hover());
                                    let p = ui.painter_at(r);
                                    p.rect_filled(r, 2.0, Color32::from_gray(35));
                                    let mut fr = r;
                                    fr.set_width(r.width() * frac.max(0.01));
                                    p.rect_filled(fr, 2.0, ecolor(e));
                                    ui.label(format!("{s}"));
                                });
                            }
                        });
                    }
                }
            });
    }
}

/// One knob as a slider, driven entirely by the shared tuning table — name,
/// range, logarithmic feel and hover help all come from the same row the
/// terminal client uses. Race-scoped, not Element-scoped: `Knob::get`/`set`
/// take a `Race`, so the caller supplies the currently selected
/// `(element, kind)` pair — for `Axis::Element` pages every row ignores the
/// `kind` half of that pair, which is exactly what the page's own header
/// note above the knob list says.
fn knob_slider(ui: &mut egui::Ui, k: &Knob, t: &mut Tuning, r: Race, retuned: &mut bool) {
    let mut v = k.value(t, r);
    let log = matches!(k.step, pentagram::tuning::Step::Scale) && k.lo >= 0 && k.hi > 1000;
    let resp = ui.add(
        egui::Slider::new(&mut v, k.lo.max(if log { 1 } else { k.lo })..=k.hi)
            .logarithmic(log)
            .custom_formatter(move |x, _| k.short(x as i64))
            .custom_parser(|s| s.parse::<f64>().ok())
            .text(k.name),
    );
    let resp = resp.on_hover_text(k.help);
    if resp.changed() {
        (k.set)(t, r, v);
        *retuned = true;
    }
}
