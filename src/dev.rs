//! Headless rendering for development and the docs: run the real engine against a folder,
//! play a script of keys and time, and write the frames out as text for a rasteriser.
//!
//! Hidden behind `--dev-snap`. Script steps are space-separated: `wait:1.5` (seconds of
//! animation), `key:j` (also space, enter, esc, tab, up, down, left, right), `snap`, and
//! `splash` (replay the launch splash).

use std::collections::VecDeque;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};

use crate::app::{Effect, State};
use crate::engine::{self, Env, ScanEvent};
use crate::ui;

pub struct Options {
    pub home: PathBuf,
    pub out: PathBuf,
    pub size: (u16, u16),
    pub script: String,
    pub scan_secs: f32,
}

const DT: f32 = 1.0 / 30.0;

struct Rig {
    env: Env,
    st: State,
    pending: VecDeque<ScanEvent>,
    total: usize,
    scan_clock: f32,
    scan_secs: f32,
}

impl Rig {
    fn start_scan(&mut self, replace: bool) {
        self.st.begin_scan(replace);
        let mut events = Vec::new();
        engine::scan(&self.env, &AtomicBool::new(false), &mut |e| events.push(e));
        self.total = events.len();
        self.pending = events.into();
        self.scan_clock = 0.0;
    }

    /// Feed the scan's events in proportion to virtual time, so scan-phase frames are reproducible.
    fn feed(&mut self, dt: f32) {
        if self.pending.is_empty() {
            return;
        }
        self.scan_clock += dt;
        let want = if self.scan_secs <= 0.0 {
            self.total
        } else {
            ((self.scan_clock / self.scan_secs).min(1.0) * self.total as f32).ceil() as usize
        };
        while self.total - self.pending.len() < want {
            let Some(event) = self.pending.pop_front() else { break };
            self.st.apply_scan(event);
        }
    }

    fn effect(&mut self, effect: Effect) {
        let cancel = AtomicBool::new(false);
        match effect {
            Effect::StartMove(items) => {
                let st = &mut self.st;
                engine::delete(&self.env, &items, &cancel, &mut |e| {
                    st.apply_run(e);
                });
                self.st.apply_trash(self.env.read_trash());
            }
            Effect::Undo => match self.env.journal.last_undo() {
                Some(plan) => {
                    self.st.begin_undo(&plan);
                    let st = &mut self.st;
                    engine::undo(&self.env, &plan, &cancel, &mut |e| {
                        st.apply_undo(e);
                    });
                    self.st.apply_trash(self.env.read_trash());
                }
                None => self.st.say("Nothing to undo."),
            },
            Effect::StartEmpty(scope) => {
                let st = &mut self.st;
                engine::empty_trash(&self.env, scope, &cancel, &mut |e| {
                    st.apply_empty(e);
                });
                self.st.apply_trash(self.env.read_trash());
            }
            Effect::StartScan { replace } => self.start_scan(replace),
            Effect::RefreshDisk => self.st.refresh_disk(),
            // The headless run maps on the spot, so a snapshot after `key:m` shows the finished map.
            Effect::BuildMap(root) => {
                let tree = crate::map::build(&root, &AtomicBool::new(false), &mut |_, _| {});
                self.st.apply_map(tree.map_or(crate::map::MapEvent::Failed, |t| crate::map::MapEvent::Done(std::sync::Arc::new(t))));
            }
            // So is the apps list.
            Effect::ScanApps => {
                let st = &mut self.st;
                let options = crate::apps::AppsOptions::default();
                crate::apps::scan(&self.env.safety, &crate::scan::procs::Procs::none(), &options, &AtomicBool::new(false), &mut |e| {
                    st.apply_apps(e)
                });
            }
            Effect::DeleteSimulators(_) => {
                self.st.worker_failed();
                self.st.warn("The headless run has no Xcode to ask.");
            }
            Effect::OpenHistory => self.st.apply_history(self.env.journal.history(self.env.safety.trash_dir())),
            Effect::UndoRun { run, strict } => match self.env.journal.undo_plan(run, strict) {
                Some(plan) => {
                    self.st.begin_undo(&plan);
                    let st = &mut self.st;
                    engine::undo(&self.env, &plan, &cancel, &mut |e| {
                        st.apply_undo(e);
                    });
                    self.st.apply_trash(self.env.read_trash());
                }
                None => self.st.say("Nothing of that run is left to put back."),
            },
            Effect::ScanFiles => {
                let st = &mut self.st;
                // Honour the config file's thresholds, as the real workers do, so a headless render matches.
                let mut options = crate::files::FilesOptions::default();
                if let Some(min) = self.env.config.duplicate_min_size {
                    options.dupes.min_size = min;
                }
                if let Some(min) = self.env.config.simulator_min {
                    options.min_simulator = min;
                }
                crate::files::scan(&self.env.safety, &options, &AtomicBool::new(false), &mut |e| st.apply_files(e));
            }
            Effect::Keep { paths, keep } => {
                let _ = crate::keep::apply(&self.env.safety, &self.env.home, &paths, keep);
            }
            Effect::Quit | Effect::Stop | Effect::Reveal(_) | Effect::None => {}
        }
    }

    fn step(&mut self, dt: f32) {
        self.feed(dt);
        let tick = self.st.tick(dt);
        self.effect(tick.effect);
    }

    fn key(&mut self, name: &str) {
        let code = match name {
            "space" => KeyCode::Char(' '),
            "enter" => KeyCode::Enter,
            "esc" => KeyCode::Esc,
            "tab" => KeyCode::Tab,
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            s if s.chars().count() == 1 => KeyCode::Char(s.chars().next().unwrap_or(' ')),
            other => panic!("unknown key {other}"),
        };
        let effect = self.st.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
        self.effect(effect);
    }
}

fn hex(c: Color) -> String {
    match c {
        Color::Rgb(r, g, b) => format!("{r:02x}{g:02x}{b:02x}"),
        _ => "-".to_string(),
    }
}

fn dump(w: &mut impl Write, buf: &Buffer) -> std::io::Result<()> {
    writeln!(w, "#FRAME {} {}", buf.area.width, buf.area.height)?;
    for c in &buf.content {
        let b = u8::from(c.modifier.contains(Modifier::BOLD));
        writeln!(w, "{}\x1f{}\x1f{}\x1f{}", c.symbol(), hex(c.fg), hex(c.bg), b)?;
    }
    Ok(())
}

pub fn run(o: Options) -> std::io::Result<()> {
    // The headless run draws as if the cells were this tall over wide (a real Ghostty with a big line height is about 2.77).
    if let Some(a) = std::env::var("BEGONE_DEV_ASPECT").ok().and_then(|v| v.parse::<f32>().ok()) {
        ui::gfx::set_aspect(a);
    }
    let env = Env::new(o.home.clone());
    let kept = crate::keep::load(&o.home);
    env.safety.set_kept(kept.clone());
    let mut st = State::new(o.home);
    st.set_kept(kept);
    st.anim.splash_over = true;
    let mut rig = Rig { env, st: State::new(PathBuf::new()), pending: VecDeque::new(), total: 0, scan_clock: 0.0, scan_secs: o.scan_secs };
    std::mem::swap(&mut rig.st, &mut st);
    rig.start_scan(false);
    let mut out = BufWriter::new(std::fs::File::create(&o.out)?);
    let area = Rect::new(0, 0, o.size.0, o.size.1);

    for step in o.script.split_whitespace() {
        match step.split_once(':') {
            Some(("wait", secs)) => {
                let secs: f32 = secs.parse().unwrap_or(0.0);
                for _ in 0..(secs / DT).round().max(1.0) as usize {
                    rig.step(DT);
                }
            }
            Some(("key", name)) => rig.key(name),
            None if step == "snap" => {
                let mut buf = Buffer::empty(area);
                ui::draw(&mut buf, area, &rig.st);
                dump(&mut out, &buf)?;
            }
            None if step == "splash" => {
                rig.st.anim.splash_over = false;
                rig.st.anim.clock = 0.0;
            }
            _ => panic!("unknown step {step}"),
        }
    }
    out.flush()
}
