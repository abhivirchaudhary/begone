//! begone: say it and it's done.
//!
//! A disk cleaner that is honest about what it does. It explains every item, moves everything
//! to the Trash instead of deleting it, can always undo, and never touches anything outside a
//! small set of places it can prove are safe. The terminal UI is the only interface, on purpose:
//! no `--json`, no daemon, no telemetry, no nagging.

mod app;
mod apps;
mod config;
mod dev;
mod dupes;
mod engine;
mod files;
mod journal;
mod keep;
mod map;
mod model;
mod safety;
mod scan;
mod simulators;
mod sizing;
mod sys;
mod term;
mod trashy;
mod ui;

use std::io::{self, IsTerminal};
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant};

use clap::Parser;
use crossterm::event::{self, Event, KeyEventKind};

use crate::app::{Effect, State};
use crate::engine::{EmptyEvent, Env, RunEvent, ScanEvent, UndoEvent};
use crate::journal::{HistoryEntry, UndoPlan};
use crate::map::MapEvent;
use crate::model::ScanItem;

#[derive(Parser, Debug)]
#[command(name = "begone", version, about = "say it and it's done: a fast, honest, reversible Mac disk cleaner")]
struct Args {
    /// Pretend this folder is your home. Its Trash is a plain folder inside it, so nothing real is touched.
    #[arg(long, value_name = "DIR")]
    path: Option<PathBuf>,

    /// Scan and clean an external or secondary volume (for example /Volumes/MyDrive) instead of your home folder. What you
    /// remove goes to a Trash on that same volume, and u undoes it. The map, big files, duplicates and build output are what
    /// make sense there; the home-only finds (app caches, backups, and so on) simply find nothing.
    #[arg(long, value_name = "DIR")]
    volume: Option<PathBuf>,

    /// Skip the launch splash.
    #[arg(long)]
    no_splash: bool,

    /// A plain dark background, without the stars.
    #[arg(long, env = "BEGONE_NO_STARS", value_parser = clap::builder::FalseyValueParser::new())]
    no_stars: bool,

    #[arg(long, hide = true)]
    dev_snap: Option<PathBuf>,
    #[arg(long, hide = true, default_value = "140x44")]
    dev_size: String,
    #[arg(long, hide = true, default_value = "wait:3 snap")]
    dev_script: String,
    #[arg(long, hide = true, default_value_t = 0.0)]
    dev_scan_secs: f32,
}

/// What background workers tell the loop.
enum Msg {
    Scan(ScanEvent),
    Run(RunEvent),
    Undo(UndoEvent),
    Empty(EmptyEvent),
    Map(MapEvent),
    Apps(apps::AppsEvent),
    Files(files::FilesEvent),
    /// Every run and irreversible step so far, newest first.
    History(Vec<HistoryEntry>),
    /// The Trash as it is on disk right now, sent after anything that changes it.
    Trash(scan::trash::TrashState),
    /// A worker panicked; the screen must not wait for it forever.
    Failed,
}

struct Workers {
    env: Arc<Env>,
    tx: Sender<Msg>,
    cancel: Arc<AtomicBool>,
    /// How to ask Xcode about simulators, if there is an Xcode to ask.
    simctl: Option<Arc<dyn simulators::Simctl>>,
}

/// Xcode's own tool on a real machine. A scratch home (`--path`) never gets the real one, so trying begone cannot touch real
/// simulators; it gets only a stand-in that `BEGONE_SIMCTL` names, if any, to try the screens with.
fn simctl_for(env: &Env) -> Option<Arc<dyn simulators::Simctl>> {
    if !env.sandboxed {
        return Some(Arc::new(simulators::Xcrun::system()));
    }
    std::env::var_os("BEGONE_SIMCTL").map(|program| Arc::new(simulators::Xcrun::with_program(program)) as Arc<dyn simulators::Simctl>)
}

impl Workers {
    fn spawn(&self, rereads_trash: bool, job: impl FnOnce(&Env, &AtomicBool, &Sender<Msg>) + Send + 'static) {
        self.cancel.store(false, Ordering::Relaxed);
        let (env, cancel, tx) = (self.env.clone(), self.cancel.clone(), self.tx.clone());
        std::thread::spawn(move || {
            if std::panic::catch_unwind(AssertUnwindSafe(|| {
                job(&env, &cancel, &tx);
                if rereads_trash {
                    let _ = tx.send(Msg::Trash(env.read_trash()));
                }
            }))
            .is_err()
            {
                let _ = tx.send(Msg::Failed);
            }
        });
    }
    fn scan(&self) {
        self.spawn(false, |env, cancel, tx| engine::scan(env, cancel, &mut |e| drop(tx.send(Msg::Scan(e)))));
    }
    /// Map a folder on a thread of its own. It only reads, and it must not be stopped by (or stop) a move, so it
    /// shares nothing with the other workers but the channel.
    fn map(&self, root: PathBuf) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let sent = Instant::now();
            let mut last = sent;
            let walked = std::panic::catch_unwind(AssertUnwindSafe(|| {
                map::build(&root, &AtomicBool::new(false), &mut |files, bytes| {
                    if last.elapsed() > Duration::from_millis(80) {
                        last = Instant::now();
                        let _ = tx.send(Msg::Map(MapEvent::Progress { files, bytes }));
                    }
                })
            }));
            let event = match walked {
                Ok(Some(tree)) => MapEvent::Done(Arc::new(tree)),
                _ => MapEvent::Failed,
            };
            let _ = tx.send(Msg::Map(event));
        });
    }
    /// Read the installed apps on a thread of its own: it only looks, and it must not hold up a move.
    fn apps(&self) {
        let (env, tx) = (self.env.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let read = std::panic::catch_unwind(AssertUnwindSafe(|| {
                let procs = env.procs_override.clone().unwrap_or_else(scan::procs::Procs::detect);
                // Spotlight has never seen a scratch home's apps, and a scratch home has no say in which Xcode is selected,
                // so only the real machine is asked either.
                let options = apps::AppsOptions {
                    spotlight: !env.sandboxed,
                    developer_dir: if env.sandboxed { None } else { std::fs::read_link("/var/db/xcode_select_link").ok() },
                    min_leftover: env.config.leftover_min.unwrap_or(apps::MIN_LEFTOVER),
                };
                apps::scan(&env.safety, &procs, &options, &AtomicBool::new(false), &mut |e| drop(tx.send(Msg::Apps(e))));
            }));
            if read.is_err() {
                // Never leave the screen waiting for a list that is not coming.
                let _ = tx.send(Msg::Apps(apps::AppsEvent::Finished { elapsed: Duration::ZERO }));
            }
        });
    }
    /// Read the history on a thread of its own: it only looks, and the journal may be long.
    fn history(&self) {
        let (env, tx) = (self.env.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(Msg::History(env.journal.history(env.safety.trash_dir())));
        });
    }
    /// Look for old installers and device backups on a thread of its own: it only looks.
    fn files(&self) {
        let (env, tx, simctl) = (self.env.clone(), self.tx.clone(), self.simctl.clone());
        std::thread::spawn(move || {
            let read = std::panic::catch_unwind(AssertUnwindSafe(|| {
                let mut dupes = dupes::DupeOptions::default();
                if let Some(min) = env.config.duplicate_min_size {
                    dupes.min_size = min;
                }
                let options = files::FilesOptions {
                    installer_min_age: files::INSTALLER_MIN_AGE,
                    spotlight: !env.sandboxed,
                    simctl,
                    min_simulator: env.config.simulator_min.unwrap_or(files::MIN_SIMULATOR),
                    dupes,
                };
                files::scan(&env.safety, &options, &AtomicBool::new(false), &mut |e| drop(tx.send(Msg::Files(e))));
            }));
            if read.is_err() {
                let _ = tx.send(Msg::Files(files::FilesEvent::Finished { elapsed: Duration::ZERO }));
            }
        });
    }
    /// Ask Xcode's own tool to delete these simulators. It cannot be undone, and it does not touch the Trash.
    fn simulators(&self, items: Vec<ScanItem>, simctl: Arc<dyn simulators::Simctl>) {
        self.spawn(false, move |env, cancel, tx| {
            simulators::delete(env, simctl.as_ref(), &items, cancel, &mut |e| drop(tx.send(Msg::Empty(e))));
        });
    }
    fn delete(&self, items: Vec<ScanItem>) {
        self.spawn(true, move |env, cancel, tx| {
            engine::delete(env, &items, cancel, &mut |e| drop(tx.send(Msg::Run(e))));
        });
    }
    fn undo(&self, plan: UndoPlan) {
        self.spawn(true, move |env, cancel, tx| {
            engine::undo(env, &plan, cancel, &mut |e| drop(tx.send(Msg::Undo(e))));
        });
    }
    fn empty(&self, scope: engine::EmptyScope) {
        self.spawn(true, move |env, cancel, tx| {
            engine::empty_trash(env, scope, cancel, &mut |e| drop(tx.send(Msg::Empty(e))));
        });
    }
}

/// Carry out an effect. Returns true when the program should exit.
fn act(st: &mut State, workers: &Workers, effect: Effect) -> bool {
    match effect {
        Effect::None => {}
        Effect::Quit => return true,
        Effect::StartScan { replace } => {
            st.begin_scan(replace);
            workers.scan();
        }
        Effect::StartMove(items) => workers.delete(items),
        Effect::Undo => match workers.env.journal.last_undo() {
            Some(plan) => {
                st.begin_undo(&plan);
                workers.undo(plan);
            }
            None => st.say("Nothing to undo yet."),
        },
        Effect::StartEmpty(scope) => workers.empty(scope),
        Effect::Stop => workers.cancel.store(true, Ordering::Relaxed),
        Effect::RefreshDisk => st.refresh_disk(),
        Effect::BuildMap(root) => workers.map(root),
        Effect::ScanApps => workers.apps(),
        Effect::ScanFiles => workers.files(),
        Effect::OpenHistory => workers.history(),
        Effect::DeleteSimulators(items) => match workers.simctl.clone() {
            Some(simctl) => workers.simulators(items, simctl),
            None => {
                st.worker_failed();
                st.warn("There is no Xcode tool to ask, so nothing was deleted.");
            }
        },
        Effect::UndoRun { run, strict } => match workers.env.journal.undo_plan(run, strict) {
            Some(plan) => {
                st.begin_undo(&plan);
                workers.undo(plan);
            }
            None => st.say("Nothing of that run is left to put back."),
        },
        Effect::Reveal(path) => sys::reveal(&path),
        Effect::Keep { paths, keep } => {
            if let Err(err) = keep::apply(&workers.env.safety, &workers.env.home, &paths, keep) {
                st.warn(format!("Could not save your list of kept places ({err}). It holds until you quit."));
            }
        }
    }
    false
}

fn main() -> std::process::ExitCode {
    let args = Args::parse();
    ui::sky::set_on(!args.no_stars);
    let home = args.volume.clone().or_else(|| args.path.clone()).unwrap_or_else(safety::home_dir);
    if let Some(out) = args.dev_snap {
        let (w, h) = args.dev_size.split_once('x').map_or((140, 44), |(w, h)| (w.parse().unwrap_or(140), h.parse().unwrap_or(44)));
        let result = dev::run(dev::Options { home, out, size: (w, h), script: args.dev_script, scan_secs: args.dev_scan_secs });
        return if result.is_ok() { std::process::ExitCode::SUCCESS } else { std::process::ExitCode::FAILURE };
    }
    if !home.is_dir() {
        eprintln!("begone: {} is not a folder.", home.display());
        return std::process::ExitCode::from(2);
    }
    if !io::stdout().is_terminal() {
        eprintln!("begone is a terminal app. Run it in a terminal, not a pipe.");
        return std::process::ExitCode::FAILURE;
    }

    let env = Arc::new(Env::new(home.clone()));
    let mut state = State::new(home.clone());
    let kept = keep::load(&home);
    env.safety.set_kept(kept.clone());
    state.set_kept(kept);
    if args.no_splash {
        state.skip_splash();
    }
    if env.safety.is_external() {
        state.say(format!("Scanning the volume at {}. What you remove goes to a Trash on the volume, and u undoes it.", home.display()));
    } else if env.sandboxed {
        state.say(format!("Trying begone on {}. Its Trash is a folder inside it, so nothing real is touched.", home.display()));
    }
    let depth = term::detect_depth();
    ui::set_mono(depth == term::Depth::None);

    let mut terminal = ratatui::init();
    // ratatui restores the terminal on any panic, including in a worker we are about to recover
    // from. Only let a panic on the main thread do that.
    let restoring_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().name() == Some("main") {
            restoring_hook(info);
        }
    }));
    let result = run(&mut terminal, state, env, depth);
    ratatui::restore();
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("begone: {err}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(terminal: &mut ratatui::DefaultTerminal, mut st: State, env: Arc<Env>, depth: term::Depth) -> io::Result<()> {
    use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};

    let (tx, rx) = mpsc::channel::<Msg>();
    let simctl = simctl_for(&env);
    let workers = Workers { env, tx, cancel: Arc::new(AtomicBool::new(false)), simctl };
    st.begin_scan(false);
    workers.scan();

    let mut stdout = io::stdout();
    let mut last = Instant::now();
    let mut dirty = true;

    loop {
        while let Ok(msg) = rx.try_recv() {
            let effect = match msg {
                Msg::Scan(e) => st.apply_scan(e),
                Msg::Run(e) => st.apply_run(e),
                Msg::Undo(e) => st.apply_undo(e),
                Msg::Empty(e) => st.apply_empty(e),
                Msg::Map(e) => {
                    st.apply_map(e);
                    Effect::None
                }
                Msg::Apps(e) => {
                    st.apply_apps(e);
                    Effect::None
                }
                Msg::Files(e) => {
                    st.apply_files(e);
                    Effect::None
                }
                Msg::History(entries) => {
                    st.apply_history(entries);
                    Effect::None
                }
                Msg::Trash(t) => {
                    st.apply_trash(t);
                    Effect::None
                }
                Msg::Failed => {
                    st.worker_failed();
                    Effect::None
                }
            };
            if act(&mut st, &workers, effect) {
                break;
            }
            dirty = true;
        }

        let now = Instant::now();
        let tick = st.tick(now.duration_since(last).as_secs_f32());
        last = now;
        if act(&mut st, &workers, tick.effect) {
            break;
        }
        let animating = tick.moving;

        let started = Instant::now();
        if dirty || animating {
            // Real terminals are not exactly 1:2, so measure the cell and let circles stay round.
            if let Ok(ws) = crossterm::terminal::window_size()
                && ws.width > 0
                && ws.columns > 0
                && ws.rows > 0
            {
                ui::gfx::set_aspect((f32::from(ws.height) / f32::from(ws.rows)) / (f32::from(ws.width) / f32::from(ws.columns)));
            }
            let _ = crossterm::execute!(stdout, BeginSynchronizedUpdate);
            terminal.draw(|frame| {
                let area = frame.area();
                ui::draw(frame.buffer_mut(), area, &st);
                term::finalize(frame.buffer_mut(), depth);
            })?;
            let _ = crossterm::execute!(stdout, EndSynchronizedUpdate);
            dirty = false;
        }

        let wait = if animating { Duration::from_millis(16).saturating_sub(started.elapsed()) } else { Duration::from_millis(250) };
        if event::poll(wait)? {
            loop {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        let effect = st.handle_key(key);
                        if act(&mut st, &workers, effect) {
                            return Ok(());
                        }
                        dirty = true;
                    }
                    Event::Resize(..) => dirty = true,
                    _ => {}
                }
                if !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
    }
    Ok(())
}
