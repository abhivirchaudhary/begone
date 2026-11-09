//! Application state and keyboard handling.
//!
//! This module knows nothing about terminals. Keys and worker messages go in, a [`State`]
//! that the UI reads comes out, and every consequence that touches the disk or the loop is
//! returned as an [`Effect`] for the caller to carry out. That is why the dangerous paths,
//! such as "what does `d` do with nothing chosen" and "does the cursor stay put while rows
//! stream in", are tested headlessly in milliseconds.
//!
//! What the program is *doing* ([`Phase`]) is deliberately separate from what dialog is
//! *open* ([`Dialog`]), and from what the scan is doing. The first version folded all
//! three into one enum, so a scan finishing could close a dialog or wipe the result banner.

use std::cell::Cell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::apps::{AppEntry, AppsEvent};
use crate::engine::{EmptyEvent, EmptyScope, Outcome, Report, RunEvent, ScanEvent, UndoEvent};
use crate::files::FilesEvent;
use crate::journal::{HistoryEntry, RunId, UndoPlan};
use crate::map::{MapEvent, MapNode, MapTree, NodeKind};
use crate::model::{Kind, SafetyClass, ScanItem, SortBy, ago, human, parse_size};
use crate::scan::trash::TrashState;
use crate::sys;

/// The order rows are listed, and the order they are moved in, so the animation and the
/// real work agree about what happens first.
pub const PANEL: [Kind; 5] = [Kind::Developer, Kind::Xcode, Kind::Caches, Kind::Trash, Kind::Logs];

/// The splash gathers, holds, blows away and hands over to the app in this long.
pub const SPLASH_SECS: f32 = 1.6;
/// Even when the work is instant, the motion takes at least this long to read.
pub const MIN_RUN_SECS: f32 = 2.4;
/// How long the receipt takes to print out.
pub const PRINT_SECS: f32 = 1.3;
/// Departures fill the first 80% of the motion; the rest is dust still in flight.
const DEPART_SPAN: f32 = 0.8;
/// How much later, on the motion's axis, dust lands than it left.
pub const FLIGHT: f32 = 0.15;
const NOTICE_SECS: f32 = 7.0;
/// More than this many rows in one group collapse into "N smaller items".
pub const MAX_SHOWN: usize = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Overview,
    Review,
    Map,
    Apps,
    Files,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// The scan is running. Rows stream in and everything but the action stays usable.
    Scanning,
    Ready,
    Reclaiming,
    /// A run just finished and its receipt is on screen.
    Receipt,
    Undoing,
    Emptying,
}

impl Phase {
    /// A worker is changing the disk, so nothing else may be started.
    pub fn busy(self) -> bool {
        matches!(self, Phase::Reclaiming | Phase::Undoing | Phase::Emptying)
    }
}

#[derive(Clone, Debug)]
pub struct ConfirmView {
    pub count: usize,
    pub bytes: u64,
    pub per_kind: Vec<(Kind, usize, u64)>,
    /// What is already in the Trash, if it could be read.
    pub trash_bytes: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct EmptyView {
    pub items: usize,
    pub bytes: u64,
    /// How much of it begone moved there, so the rest is clearly yours.
    pub from_begone: u64,
    /// `(items, bytes)` of what begone moved, when the Trash holds that and more: the question then also offers to empty only
    /// that. It is known by the very file each entry is, so nothing of yours with a similar name can be in it.
    pub mine: Option<(usize, u64)>,
}

#[derive(Clone, Debug)]
pub enum Dialog {
    Help,
    ConfirmMove(ConfirmView),
    ConfirmUninstall(UninstallView),
    ConfirmEmpty(EmptyView),
    /// "How much do you want to free?", with what has been typed so far.
    Goal(GoalView),
    /// Every earlier run, to put back any one of them.
    History(HistoryView),
}

/// The history list: what happened, newest first, and which line the cursor is on.
#[derive(Clone, Debug)]
pub struct HistoryView {
    pub entries: Vec<HistoryEntry>,
    pub cursor: usize,
    /// Written by the drawing code so the cursor stays on screen.
    pub scroll: Cell<usize>,
}

#[derive(Clone, Debug, Default)]
pub struct GoalView {
    pub text: String,
    /// Why Enter did not work.
    pub error: Option<String>,
}

/// One chosen app, or the leftovers of one, in the question about removing them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UninstallRow {
    pub name: String,
    pub bytes: u64,
    /// How many caches, settings and the like go with it (all there is, for a leftover).
    pub traces: usize,
    /// An app that is already gone: only what it left behind moves.
    pub leftover: bool,
}

/// What the confirmation for removing apps says: which apps, and how much goes with them.
#[derive(Clone, Debug)]
pub struct UninstallView {
    pub apps: Vec<UninstallRow>,
    pub bytes: u64,
    /// Data the apps keep: containers and application support, which begone never removes.
    pub kept_bytes: u64,
    /// What removing them would break, said once each.
    pub cautions: Vec<String>,
    pub trash_bytes: Option<u64>,
    /// The question is about files you chose on the Files tab, not about apps.
    pub files: bool,
    /// The question is about Xcode simulators, which are deleted for good and do not go to the Trash.
    pub sims: bool,
}

/// What the caller has to do after a key or message.
#[derive(Clone, Debug)]
pub enum Effect {
    None,
    Quit,
    StartScan {
        replace: bool,
    },
    StartMove(Vec<ScanItem>),
    /// Read the history and start an undo, or say there is nothing to undo.
    Undo,
    StartEmpty(EmptyScope),
    /// Ask the running worker to stop after the current item.
    Stop,
    RefreshDisk,
    /// Walk this folder and send back the map of where its bytes are. Read-only.
    BuildMap(PathBuf),
    /// Show this path in Finder.
    Reveal(PathBuf),
    /// Look at the installed apps and what each left behind. Read-only.
    ScanApps,
    /// Add these places to the list begone keeps, or take them off it.
    Keep {
        paths: Vec<PathBuf>,
        keep: bool,
    },
    /// Look for old installers and device backups. Read-only.
    ScanFiles,
    /// Delete these Xcode simulators through `simctl`. It cannot be undone.
    DeleteSimulators(Vec<ScanItem>),
    /// Read the history of runs, to show it.
    OpenHistory,
    /// Put back one particular run. `strict` when it is not the newest, so a folder a newer run removed is not made again.
    UndoRun {
        run: RunId,
        strict: bool,
    },
}

#[derive(Clone, Copy, Debug, Default)]
pub struct KindTotals {
    pub count: usize,
    pub chosen_count: usize,
    pub found: u64,
    pub chosen: u64,
    /// Rows of this kind that you asked begone to keep. They are not in the figures above.
    pub kept: usize,
}

#[derive(Clone, Debug, Default)]
pub struct Disk {
    pub total: u64,
    pub free: u64,
    /// More than `free` that macOS clears by itself when it needs room (local snapshots, caches). Finder counts it as available
    /// and a plain reading of the volume does not, which is why freeing 10 GB can move the number less than expected.
    pub purgeable: u64,
}

impl Disk {
    /// The volume holding `home`, as the system describes it.
    pub fn of(home: &Path) -> Option<Disk> {
        let (total, free) = sys::disk_usage(home)?;
        let purgeable = sys::available_with_purgeable(home).map_or(0, |available| available.saturating_sub(free));
        Some(Disk { total, free, purgeable })
    }
}

#[derive(Clone, Debug)]
pub struct Notice {
    pub text: String,
    pub warn: bool,
    pub born: f32,
}

#[derive(Clone, Debug, Default)]
pub struct ScanView {
    pub progress: f32,
    pub shown: f32,
    pub reading: String,
    pub files: u64,
    pub elapsed: Option<f32>,
    /// A rescan builds its rows aside and swaps them in, so the screen never collapses.
    pub replace: bool,
}

/// Progress of a move, an undo, or emptying the Trash: real work and the motion that shows it.
#[derive(Clone, Debug, Default)]
pub struct RunView {
    pub items: usize,
    pub bytes: u64,
    pub done_items: usize,
    pub done_bytes: u64,
    pub finished: bool,
    pub report: Option<Report>,
    /// 0..=1. For a move it climbs; for an undo it falls.
    pub display: f32,
    /// Chosen bytes per kind at the start, indexed by `Kind::index()`.
    pub plan: [u64; Kind::COUNT],
    /// How many items each kind had at the start, so a row's caption holds still while it plays.
    pub counts: [usize; Kind::COUNT],
    /// Where each kind's share of the motion starts and ends on the 0..=1 axis.
    pub windows: [(f32, f32); Kind::COUNT],
    pub trash_base: u64,
    /// Where each item of a move was, by id, so what the scan also listed there can be dropped once it has moved.
    pub paths: Vec<(u64, PathBuf)>,
    pub undo: bool,
    pub restored: usize,
    pub unresolved: usize,
    pub failures: Vec<String>,
    /// An emptying that takes only what begone moved.
    pub only_begone: bool,
    /// Not the Trash but Xcode simulators: the same count down and the same words about it not being undoable.
    pub simulators: bool,
}

impl RunView {
    fn real(&self) -> f32 {
        if self.finished {
            return 1.0;
        }
        if self.bytes > 0 {
            (self.done_bytes as f32 / self.bytes as f32).min(1.0)
        } else if self.items > 0 {
            (self.done_items as f32 / self.items as f32).min(1.0)
        } else {
            0.0
        }
    }

    /// How much of one kind has left its row, by the motion's clock.
    pub fn departed_fraction(&self, kind: Kind) -> f32 {
        self.window_fraction(kind, 0.0)
    }

    /// How much of one kind has landed in the ring, a beat after it left its row.
    pub fn moved_fraction(&self, kind: Kind) -> f32 {
        self.window_fraction(kind, FLIGHT)
    }

    fn window_fraction(&self, kind: Kind, delay: f32) -> f32 {
        let (s, e) = self.windows[kind.index()];
        if e <= s {
            return if self.display >= e + delay { 1.0 } else { 0.0 };
        }
        let t = ((self.display - delay - s) / (e - s)).clamp(0.0, 1.0);
        t * t * (3.0 - 2.0 * t)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineStyle {
    Blank,
    Title,
    Sub,
    Rule,
    Normal,
    Bold,
    Dim,
    Accent,
    Barcode,
}

#[derive(Clone, Debug)]
pub struct ReceiptLine {
    pub left: String,
    pub right: String,
    pub style: LineStyle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKey {
    Item(u64),
    More(Kind),
}

#[derive(Clone, Debug)]
pub enum ReviewRow {
    Header(Kind),
    /// An index into `State::items`.
    Item(usize),
    More {
        kind: Kind,
        count: usize,
        bytes: u64,
        chosen: usize,
        expanded: bool,
    },
}

pub struct ReviewView {
    pub cursor: Option<RowKey>,
    pub expanded: [bool; Kind::COUNT],
    /// Written by the drawing code so the cursor stays on screen.
    pub scroll: Cell<usize>,
}

/// The Apps tab: what is installed, biggest first, and which apps are chosen to be removed.
pub struct AppsView {
    pub list: Vec<AppEntry>,
    pub scanning: bool,
    /// A scan has finished at least once.
    pub scanned: bool,
    /// How long the last reading took.
    pub read_in: Option<std::time::Duration>,
    pub progress: f32,
    /// Something moved or came back since the list was made.
    pub stale: bool,
    /// The chosen apps, by the id of their bundle.
    pub chosen: HashSet<u64>,
    pub cursor: usize,
    /// The person has moved or chosen something, so the cursor now stays on its app while others arrive.
    pub touched: bool,
    /// Written by the drawing code so the cursor stays on screen.
    pub scroll: Cell<usize>,
}

/// The Files tab: old installers and device backups, biggest first. They are chosen one at a time, never for you.
pub struct FilesView {
    pub list: Vec<ScanItem>,
    pub scanning: bool,
    pub scanned: bool,
    pub read_in: Option<std::time::Duration>,
    pub progress: f32,
    /// Something moved or came back since the list was made.
    pub stale: bool,
    pub chosen: HashSet<u64>,
    pub cursor: usize,
    /// The person has moved or chosen something, so the cursor now stays on its row while others arrive.
    pub touched: bool,
    pub scroll: Cell<usize>,
    /// What could not be read, said on the tab.
    pub notes: Vec<String>,
}

/// The disk map: a tree of where the bytes are, and where in it the person is looking.
pub struct MapView {
    pub tree: Option<Arc<MapTree>>,
    pub building: bool,
    /// Files and bytes seen so far by the walk in progress.
    pub seen: (u64, u64),
    /// Something moved or came back since the tree was built, so it is out of date.
    pub stale: bool,
    pub failed: bool,
    /// The folder being shown, as the child to open at each level from the root.
    pub path: Vec<usize>,
    /// The selected row at each level, so going up lands where you came from. One longer than `path`.
    pub cursor: Vec<usize>,
    /// Written by the drawing code so the cursor stays on screen.
    pub scroll: Cell<usize>,
}

/// Values that ease toward a target instead of jumping.
#[derive(Clone, Debug, Default)]
pub struct Anim {
    pub clock: f32,
    /// Displayed bytes per kind in the ring.
    pub arc: [f32; Kind::COUNT],
    pub hero: f32,
    /// The biggest value the big number has had since it last settled. Its unit follows this, so a count down stays in the
    /// unit it started in and ends on 0, instead of running on through the smaller units.
    pub hero_scale: f32,
    /// The biggest bar value since the bars last settled, which says how close to its target a bar has to be to count as there.
    pub arc_scale: f32,
    /// How much of the receipt has printed, 0 to 1. It starts over each time a run finishes.
    pub receipt: f32,
    pub splash_over: bool,
    /// The sky's own clock. It runs only while something else is moving, so a resting screen is
    /// perfectly still, costs nothing, and picks up where it left off instead of jumping.
    pub sky: f32,
}

pub fn approach(cur: f32, target: f32, dt: f32, rate: f32) -> f32 {
    let v = cur + (target - cur) * (1.0 - (-rate * dt).exp());
    if (v - target).abs() < target.abs().max(1.0) * 1e-3 { target } else { v }
}

pub struct State {
    pub home: PathBuf,
    pub items: Vec<ScanItem>,
    incoming: Vec<ScanItem>,
    pub chosen: HashSet<u64>,
    pub totals: [KindTotals; Kind::COUNT],
    pub trash: Option<TrashState>,
    /// Bytes begone moved to the Trash since the scan read it.
    pub moved_to_trash: u64,
    /// How many items that was, so "is the Trash empty?" is answered correctly before the re-read lands.
    pub moved_items: usize,
    pub phase: Phase,
    pub dialog: Option<Dialog>,
    pub tab: Tab,
    pub scan: ScanView,
    pub kind_cursor: usize,
    pub review: ReviewView,
    pub anim: Anim,
    pub notice: Option<Notice>,
    pub disk: Disk,
    pub run: RunView,
    pub receipt: Vec<ReceiptLine>,
    pub quit_after: bool,
    pub has_history: bool,
    pub map: MapView,
    pub apps: AppsView,
    pub files: FilesView,
    /// The items the open "uninstall?" question is about.
    pub pending_uninstall: Vec<ScanItem>,
    /// The places you asked begone to keep, as the list file has them.
    pub kept: Vec<PathBuf>,
    /// How the Files and Apps lists are ordered. `s` cycles it.
    pub sort: SortBy,
}

/// Order two files for the list under the current sort. Smaller comes first on screen.
pub fn file_order(sort: SortBy, a: &ScanItem, b: &ScanItem) -> std::cmp::Ordering {
    let name = |i: &ScanItem| i.title.to_lowercase();
    match sort {
        SortBy::Size => b.size.cmp(&a.size).then_with(|| name(a).cmp(&name(b))).then_with(|| a.path.cmp(&b.path)),
        SortBy::Name => name(a).cmp(&name(b)).then_with(|| b.size.cmp(&a.size)).then_with(|| a.path.cmp(&b.path)),
        // Oldest first; a row with no known date sorts last, so the list still means something.
        SortBy::Date => {
            a.modified.is_none().cmp(&b.modified.is_none()).then_with(|| a.modified.cmp(&b.modified)).then_with(|| name(a).cmp(&name(b)))
        }
    }
}

/// The same, for an installed app (its size is the whole bundle and its traces; its date is when it was last opened).
pub fn app_order(sort: SortBy, a: &AppEntry, b: &AppEntry) -> std::cmp::Ordering {
    let name = |e: &AppEntry| e.identity.name.to_lowercase();
    match sort {
        SortBy::Size => b.bytes().cmp(&a.bytes()).then_with(|| name(a).cmp(&name(b))),
        SortBy::Name => name(a).cmp(&name(b)).then_with(|| b.bytes().cmp(&a.bytes())),
        SortBy::Date => a
            .last_used
            .is_none()
            .cmp(&b.last_used.is_none())
            .then_with(|| a.last_used.cmp(&b.last_used))
            .then_with(|| name(a).cmp(&name(b))),
    }
}

pub struct Tick {
    pub moving: bool,
    pub effect: Effect,
}

impl State {
    pub fn new(home: PathBuf) -> State {
        let disk = Disk::of(&home).unwrap_or_default();
        State {
            home,
            items: Vec::new(),
            incoming: Vec::new(),
            chosen: HashSet::new(),
            totals: [KindTotals::default(); Kind::COUNT],
            trash: None,
            moved_to_trash: 0,
            moved_items: 0,
            phase: Phase::Scanning,
            dialog: None,
            tab: Tab::Overview,
            scan: ScanView::default(),
            kind_cursor: 0,
            review: ReviewView { cursor: None, expanded: [false; Kind::COUNT], scroll: Cell::new(0) },
            anim: Anim::default(),
            notice: None,
            disk,
            run: RunView::default(),
            receipt: Vec::new(),
            quit_after: false,
            has_history: false,
            apps: AppsView {
                list: Vec::new(),
                scanning: false,
                scanned: false,
                read_in: None,
                progress: 0.0,
                stale: false,
                chosen: HashSet::new(),
                cursor: 0,
                touched: false,
                scroll: Cell::new(0),
            },
            files: FilesView {
                list: Vec::new(),
                scanning: false,
                scanned: false,
                read_in: None,
                progress: 0.0,
                stale: false,
                chosen: HashSet::new(),
                cursor: 0,
                touched: false,
                scroll: Cell::new(0),
                notes: Vec::new(),
            },
            pending_uninstall: Vec::new(),
            kept: Vec::new(),
            map: MapView {
                tree: None,
                building: false,
                seen: (0, 0),
                stale: false,
                failed: false,
                path: Vec::new(),
                cursor: vec![0],
                scroll: Cell::new(0),
            },
            sort: SortBy::default(),
        }
    }

    pub fn say(&mut self, text: impl Into<String>) {
        self.notice = Some(Notice { text: text.into(), warn: false, born: self.anim.clock });
    }

    pub fn warn(&mut self, text: impl Into<String>) {
        self.notice = Some(Notice { text: text.into(), warn: true, born: self.anim.clock });
    }

    pub fn refresh_disk(&mut self) {
        if let Some(disk) = Disk::of(&self.home) {
            self.disk = disk;
        }
    }

    pub fn in_splash(&self) -> bool {
        !self.anim.splash_over && self.anim.clock < SPLASH_SECS
    }

    pub fn skip_splash(&mut self) {
        self.anim.splash_over = true;
    }

    // ------------------------------------------------------------ numbers

    pub fn trash_bytes(&self) -> u64 {
        self.trash.as_ref().map_or(0, |t| t.bytes) + self.moved_to_trash
    }

    /// Items in the Trash, counting what begone just moved there even if the Trash has not been re-read yet.
    pub fn trash_items(&self) -> usize {
        self.trash.as_ref().map_or(0, |t| t.entries.len()) + self.moved_items
    }

    /// Files in the rows still on the list.
    pub fn files_found(&self) -> u64 {
        self.items.iter().map(|i| i.files).sum()
    }

    /// A fresh reading of the Trash from disk replaces every estimate.
    pub fn apply_trash(&mut self, trash: TrashState) {
        self.trash = Some(trash);
        self.moved_to_trash = 0;
        self.moved_items = 0;
        self.recompute();
    }

    pub fn trash_readable(&self) -> bool {
        self.trash.as_ref().is_none_or(|t| t.readable)
    }

    pub fn chosen_bytes(&self) -> u64 {
        self.totals.iter().filter(|_| true).enumerate().filter(|(i, _)| *i != Kind::Trash.index()).map(|(_, t)| t.chosen).sum()
    }

    pub fn chosen_count(&self) -> usize {
        self.items.iter().filter(|i| self.chosen.contains(&i.id)).count()
    }

    pub fn found_bytes(&self) -> u64 {
        self.items.iter().filter(|i| !i.kept).map(|i| i.size).sum()
    }

    pub fn recompute(&mut self) {
        let mut totals = [KindTotals::default(); Kind::COUNT];
        for item in &self.items {
            let t = &mut totals[item.kind.index()];
            if item.kept {
                t.kept += 1;
                continue;
            }
            t.count += 1;
            t.found += item.size;
            if self.chosen.contains(&item.id) {
                t.chosen_count += 1;
                t.chosen += item.size;
            }
        }
        let trash = self.trash.as_ref().map_or(0, |t| t.bytes) + self.moved_to_trash;
        totals[Kind::Trash.index()] =
            KindTotals { count: self.trash.as_ref().map_or(0, |t| t.entries.len()), found: trash, chosen: trash, chosen_count: 0, kept: 0 };
        self.totals = totals;
    }

    /// What the ring shows right now, as bytes per kind.
    fn ring_targets(&self) -> [f32; Kind::COUNT] {
        let mut v = [0.0; Kind::COUNT];
        if matches!(self.phase, Phase::Reclaiming | Phase::Undoing) {
            let mut moved = 0.0;
            for kind in Kind::ALL {
                if kind == Kind::Trash {
                    continue;
                }
                let plan = self.run.plan[kind.index()] as f32;
                let f = self.run.moved_fraction(kind);
                v[kind.index()] = plan * (1.0 - f);
                moved += plan * f;
            }
            v[Kind::Trash.index()] = self.run.trash_base as f32 + moved;
        } else {
            for kind in Kind::ALL {
                v[kind.index()] = match kind {
                    Kind::Trash if self.phase == Phase::Emptying => self.trash_left() as f32,
                    Kind::Trash => self.trash_bytes() as f32,
                    _ => self.totals[kind.index()].chosen as f32,
                };
            }
        }
        v
    }

    /// The headline number: what Begone will move, or, once things have moved, what the Trash holds.
    pub fn hero_target(&self) -> f32 {
        match self.phase {
            Phase::Reclaiming | Phase::Undoing | Phase::Receipt | Phase::Emptying => self.ring_targets()[Kind::Trash.index()],
            _ => self.chosen_bytes() as f32,
        }
    }

    // ------------------------------------------------------------ time

    /// Ease the big number toward its target. True while it is still moving, and on the tick that lands it.
    fn step_hero(&mut self, dt: f32, direct: bool) -> bool {
        let target = self.hero_target();
        let before = self.anim.hero;
        let scale = self.anim.hero_scale.max(before).max(target);
        // While a move plays the number follows it closely, but never by jumping: at the start it glides from the plan down to what
        // the Trash holds, instead of cutting there in one frame.
        let mut hero = approach(before, target, dt, if direct { 14.0 } else { 6.0 });
        // Three figures are shown, so within a two-thousandth of the biggest value it has had is as good as there.
        if (hero - target).abs() <= (scale * 0.0005).max(1.0) {
            hero = target;
        }
        self.anim.hero = hero;
        self.anim.hero_scale = if hero == target && !direct { target } else { scale };
        hero != before || hero != target
    }

    /// The big number and its unit, as they are written on screen: in the unit of the biggest value of the move it is part of,
    /// which includes where it is going, so the unit is right from the first frame.
    pub fn hero_parts(&self) -> (String, &'static str) {
        let value = self.anim.hero.max(0.0);
        let scale = self.anim.hero_scale.max(value).max(self.hero_target());
        crate::model::human_parts_in(f64::from(value), scale as u64)
    }

    pub fn tick(&mut self, dt: f32) -> Tick {
        let dt = dt.clamp(0.0, 0.25);
        self.anim.clock += dt;
        let mut moving = self.in_splash();
        let mut effect = Effect::None;

        self.scan.shown = approach(self.scan.shown, self.scan.progress, dt, 8.0);
        moving |= self.phase == Phase::Scanning || (self.scan.shown - self.scan.progress).abs() > 1e-3;

        moving |= self.map.building && self.tab == Tab::Map;

        match self.phase {
            Phase::Reclaiming => {
                let step = dt / MIN_RUN_SECS;
                self.run.display = (self.run.display + step).min(self.run.real());
                moving = true;
                if self.run.finished && self.run.display >= 0.999 {
                    self.run.display = 1.0;
                    self.phase = Phase::Receipt;
                    self.anim.receipt = 0.0;
                    if self.quit_after {
                        effect = Effect::Quit;
                    }
                }
            }
            Phase::Undoing => {
                let step = dt / MIN_RUN_SECS;
                self.run.display = (self.run.display - step).max(1.0 - self.run.real());
                moving = true;
                if self.run.finished && self.run.display <= 0.001 {
                    self.run.display = 0.0;
                    self.finish_undo();
                    effect = if self.quit_after { Effect::Quit } else { Effect::StartScan { replace: true } };
                }
            }
            Phase::Receipt => {
                self.anim.receipt = (self.anim.receipt + dt / PRINT_SECS).min(1.0);
                moving |= self.anim.receipt < 1.0;
            }
            Phase::Emptying => moving = true,
            _ => {}
        }

        // The ring and the number ease, except while a run is being shown, where they follow
        // the motion's own clock exactly so the picture and the progress bar never disagree.
        let targets = self.ring_targets();
        let direct = matches!(self.phase, Phase::Reclaiming | Phase::Undoing);
        // The bars are drawn to a few hundred steps, so within a two-thousandth of the biggest one they have had is as good as there.
        let scale = self.anim.arc.iter().copied().chain(targets).fold(self.anim.arc_scale.max(1.0), f32::max);
        let reach = scale * 0.0005;
        for (arc, target) in self.anim.arc.iter_mut().zip(targets) {
            let before = *arc;
            let mut next = if direct { target } else { approach(*arc, target, dt, 6.0) };
            if (next - target).abs() <= reach {
                next = target;
            }
            *arc = next;
            // A tick that changes what is on screen is drawn, including the one that lands on the final value.
            moving |= *arc != before || (*arc - target).abs() > 1.0;
        }
        self.anim.arc_scale = if !direct && self.anim.arc.iter().zip(targets).all(|(a, t)| *a == t) { 0.0 } else { scale };
        moving |= self.step_hero(dt, direct);

        if let Some(n) = &self.notice
            && self.anim.clock - n.born > NOTICE_SECS
        {
            self.notice = None;
        }
        if moving {
            self.anim.sky += dt;
        }
        Tick { moving, effect }
    }

    // ------------------------------------------------------------ messages from workers

    pub fn begin_scan(&mut self, replace: bool) {
        self.phase = Phase::Scanning;
        self.scan = ScanView { replace, shown: if replace { 0.0 } else { self.scan.shown }, ..ScanView::default() };
        self.incoming.clear();
        if !replace {
            self.items.clear();
            self.chosen.clear();
            self.trash = None;
        }
        self.recompute();
    }

    pub fn apply_scan(&mut self, event: ScanEvent) -> Effect {
        match event {
            ScanEvent::Progress(p) => self.scan.progress = p,
            ScanEvent::Reading(path) => {
                self.scan.reading = match path.strip_prefix(&self.home) {
                    Ok(rest) => format!("~/{}", rest.display()),
                    Err(_) => path.display().to_string(),
                };
            }
            ScanEvent::Found(item) => {
                self.scan.files += item.files;
                let mut item = *item;
                item.kept = item.kept || self.kept_by(&item.path).is_some();
                if self.scan.replace {
                    self.incoming.push(item);
                } else {
                    if item.pre_selected && !item.kept {
                        self.chosen.insert(item.id);
                    }
                    self.items.push(item);
                    self.recompute();
                }
            }
            ScanEvent::Trash(t) => {
                self.trash = Some(t);
                self.recompute();
            }
            ScanEvent::Finished(summary) => {
                if self.scan.replace {
                    let kept: HashSet<PathBuf> = self.incoming.iter().map(|i| i.path.clone()).collect();
                    let previously: HashSet<&Path> = self.items.iter().map(|i| i.path.as_path()).collect();
                    // Rows that were already there keep the user's choice; new ones get the default.
                    let old_choice: HashSet<PathBuf> =
                        self.items.iter().filter(|i| self.chosen.contains(&i.id)).map(|i| i.path.clone()).collect();
                    let fresh: Vec<ScanItem> = std::mem::take(&mut self.incoming);
                    let known: Vec<bool> = fresh.iter().map(|i| previously.contains(i.path.as_path())).collect();
                    self.chosen.clear();
                    for (item, was_known) in fresh.iter().zip(known) {
                        if !item.kept && ((was_known && old_choice.contains(&item.path)) || (!was_known && item.pre_selected)) {
                            self.chosen.insert(item.id);
                        }
                    }
                    self.items = fresh;
                    self.moved_items = 0;
                    let _ = kept;
                    self.moved_to_trash = 0;
                    self.recompute();
                }
                self.scan.progress = 1.0;
                self.scan.elapsed = Some(summary.elapsed.as_secs_f32());
                if self.phase == Phase::Scanning {
                    self.phase = Phase::Ready;
                }
                self.fix_cursors();
                return Effect::RefreshDisk;
            }
        }
        Effect::None
    }

    pub fn begin_move(&mut self, items: &[ScanItem]) {
        let mut run =
            RunView { items: items.len(), bytes: items.iter().map(|i| i.size).sum(), trash_base: self.trash_bytes(), ..RunView::default() };
        for item in items {
            run.plan[item.kind.index()] += item.size;
            run.counts[item.kind.index()] += 1;
        }
        run.windows = windows(&run.plan);
        run.paths = items.iter().map(|i| (i.id, i.path.clone())).collect();
        self.run = run;
        self.phase = Phase::Reclaiming;
        self.tab = Tab::Overview;
        self.receipt.clear();
    }

    pub fn apply_run(&mut self, event: RunEvent) -> Effect {
        match event {
            RunEvent::Started { items, bytes } => {
                self.run.items = items;
                self.run.bytes = bytes;
            }
            RunEvent::Item(result) => {
                self.run.done_items += 1;
                self.run.done_bytes += result.bytes;
            }
            RunEvent::Finished(report) => {
                let moved: HashSet<u64> = report.moved().map(|r| r.id).collect();
                let kept: HashSet<u64> = report.kept().iter().map(|r| r.id).collect();
                self.items.retain(|i| !moved.contains(&i.id));
                // The cleaning scan may also have listed something at, or inside, a place that has just moved.
                let gone: Vec<&PathBuf> = self.run.paths.iter().filter(|(id, _)| moved.contains(id)).map(|(_, p)| p).collect();
                self.items.retain(|i| !gone.iter().any(|p| i.path.starts_with(p)));
                self.apps.list.retain(|e| !moved.contains(&e.bundle().id));
                self.apps.chosen.retain(|id| !moved.contains(id) && !kept.contains(id));
                self.apps.stale |= self.run.plan[Kind::Apps.index()] > 0;
                self.files.list.retain(|i| !moved.contains(&i.id));
                self.files.chosen.retain(|id| !moved.contains(id) && !kept.contains(id));
                self.files.stale |= self.run.plan[Kind::Files.index()] > 0;
                for id in moved.iter().chain(kept.iter()) {
                    self.chosen.remove(id);
                }
                self.moved_to_trash += report.moved_bytes();
                self.moved_items += report.moved_count();
                self.run.finished = true;
                self.receipt = build_receipt(self, &report);
                self.run.report = Some(report);
                self.has_history = true;
                self.map.stale = true;
                self.apps.stale = true;
                self.recompute();
                self.fix_cursors();
                return Effect::RefreshDisk;
            }
        }
        Effect::None
    }

    pub fn begin_undo(&mut self, plan: &UndoPlan) {
        let mut run = RunView {
            items: plan.restorable.len(),
            bytes: plan.bytes(),
            trash_base: self.trash_bytes(),
            undo: true,
            display: 1.0,
            ..RunView::default()
        };
        for r in &plan.restorable {
            run.plan[r.kind.index()] += r.bytes;
            run.counts[r.kind.index()] += 1;
        }
        run.windows = windows(&run.plan);
        self.run = run;
        self.phase = Phase::Undoing;
        self.tab = Tab::Overview;
        self.dialog = None;
        let age = std::time::Duration::from_secs((sys::now_epoch() - plan.at as i64).max(0) as u64);
        self.say(format!("Putting back the run from {}: {} items, {}.", ago(age), plan.restorable.len(), human(plan.bytes())));
    }

    pub fn apply_undo(&mut self, event: UndoEvent) -> Effect {
        match event {
            UndoEvent::Started { items, bytes } => {
                self.run.items = items;
                self.run.bytes = bytes;
            }
            UndoEvent::Item => self.run.done_items += 1,
            UndoEvent::Finished(report) => {
                self.run.done_bytes = self.run.bytes;
                self.run.finished = true;
                self.run.restored = report.restored.len();
                self.run.unresolved = report.unresolved;
                self.run.failures = report.failed.iter().map(|(_, why)| why.clone()).collect();
                let back: u64 = report.restored.iter().map(|r| r.bytes).sum();
                self.moved_to_trash = self.moved_to_trash.saturating_sub(back);
                self.moved_items = self.moved_items.saturating_sub(report.restored.len());
                self.map.stale = true;
                self.apps.stale = true;
                self.files.stale = true;
                return Effect::RefreshDisk;
            }
        }
        Effect::None
    }

    fn finish_undo(&mut self) {
        let (n, failed) = (self.run.restored, self.run.failures.len());
        if failed > 0 {
            let first = self.run.failures.first().cloned().unwrap_or_default();
            self.warn(format!("Put back {n}, but {failed} could not come back: {first}"));
        } else if self.run.unresolved > 0 {
            self.warn(format!("Put back {n}. {} more may have moved without a record: look in the Trash.", self.run.unresolved));
        } else {
            self.say(format!("Put back {n} {}. Everything is where it was.", if n == 1 { "item" } else { "items" }));
        }
        self.receipt.clear();
    }

    /// A background worker died. Put the screen back in a state where the user can carry on.
    pub fn worker_failed(&mut self) {
        self.phase = if self.scan.elapsed.is_some() { Phase::Ready } else { Phase::Scanning };
        self.warn("Something went wrong in the background. Nothing further was touched.");
        self.recompute();
    }

    pub fn begin_empty(&mut self, scope: EmptyScope) {
        let trash = self.trash.as_ref();
        let (items, bytes) = trash.map_or((0, 0), |t| {
            t.entries.iter().filter(|e| scope == EmptyScope::Everything || e.begone).fold((0, 0), |(n, b), e| (n + 1, b + e.bytes))
        });
        self.run = RunView { items, bytes, only_begone: scope == EmptyScope::OnlyBegones, ..RunView::default() };
        self.phase = Phase::Emptying;
        self.dialog = None;
        // The confirmation just said how much is about to go, so the number starts there and counts down.
        let left = self.trash_left() as f32;
        self.anim.hero = left;
        self.anim.hero_scale = left;
    }

    /// Start deleting Xcode simulators: the same count down as emptying the Trash, because it is just as final.
    pub fn begin_sim_delete(&mut self, items: &[ScanItem]) {
        self.run = RunView { items: items.len(), bytes: items.iter().map(|i| i.size).sum(), simulators: true, ..RunView::default() };
        self.phase = Phase::Emptying;
        self.tab = Tab::Overview;
        self.dialog = None;
        let left = self.trash_left() as f32;
        self.anim.hero = left;
        self.anim.hero_scale = left;
    }

    /// What is still in the Trash while it is being emptied.
    fn trash_left(&self) -> u64 {
        self.run.bytes.saturating_sub(self.run.done_bytes)
    }

    pub fn apply_empty(&mut self, event: EmptyEvent) -> Effect {
        match event {
            EmptyEvent::Started { items, bytes } => {
                self.run.items = items;
                self.run.bytes = bytes;
            }
            EmptyEvent::Item { bytes } => {
                self.run.done_items += 1;
                self.run.done_bytes += bytes;
            }
            EmptyEvent::Finished(report) => {
                self.map.stale = true;
                self.apps.stale = true;
                self.files.stale = true;
                if let Some(t) = self.trash.as_mut() {
                    t.entries.retain(|e| !report.gone.contains(&e.path));
                    t.bytes = t.entries.iter().map(|e| e.bytes).sum();
                }
                self.moved_to_trash = 0;
                self.moved_items = 0;
                self.receipt.clear();
                self.phase = Phase::Ready;
                if self.run.simulators {
                    self.files.list.retain(|i| !report.gone.contains(&i.path));
                    let left: HashSet<u64> = self.files.list.iter().map(|i| i.id).collect();
                    self.files.chosen.retain(|id| left.contains(id));
                    let what = if report.removed == 1 { "simulator" } else { "simulators" };
                    if report.kept.is_empty() {
                        self.say(format!("Deleted {} {what}: {} is free again.", report.removed, human(report.bytes)));
                    } else {
                        let first = report.kept.first().map(|(_, why)| why.clone()).unwrap_or_default();
                        self.warn(format!("Deleted {}, but {} stayed: {first}", report.removed, report.kept.len()));
                    }
                } else if report.kept.is_empty() && self.run.only_begone && report.removed == 0 {
                    self.say("Nothing that begone moved is still in the Trash, so nothing was emptied. What is there is yours.");
                } else if report.kept.is_empty() && self.run.only_begone {
                    self.say(format!(
                        "Emptied what begone moved: {} is free again. What was yours is still in the Trash.",
                        human(report.bytes)
                    ));
                } else if report.kept.is_empty() {
                    self.say(format!("Emptied the Trash: {} is free again.", human(report.bytes)));
                } else {
                    let first = report.kept.first().map(|(_, why)| why.clone()).unwrap_or_default();
                    self.warn(format!("Emptied {}, but {} items stayed: {first}", human(report.bytes), report.kept.len()));
                }
                self.recompute();
                if self.quit_after {
                    return Effect::Quit;
                }
                return Effect::RefreshDisk;
            }
        }
        Effect::None
    }

    // ------------------------------------------------------------ selection

    /// Everything chosen, in the order it will be moved.
    pub fn chosen_items(&self) -> Vec<ScanItem> {
        let rank = |k: Kind| PANEL.iter().position(|p| *p == k).unwrap_or(PANEL.len());
        let mut out: Vec<ScanItem> = self.items.iter().filter(|i| self.chosen.contains(&i.id) && !i.kept).cloned().collect();
        out.sort_by(|a, b| rank(a.kind).cmp(&rank(b.kind)).then_with(|| b.size.cmp(&a.size)).then_with(|| a.path.cmp(&b.path)));
        out
    }

    fn items_of(&self, kind: Kind) -> impl Iterator<Item = &ScanItem> {
        self.items.iter().filter(move |i| i.kind == kind)
    }

    /// Choose what begone recommends for a kind, or clear the kind if that is already the choice.
    pub fn toggle_kind(&mut self, kind: Kind) {
        if kind == Kind::Trash {
            self.say("Emptying the Trash is its own step, and it cannot be undone. Press e.");
            return;
        }
        let recommended: Vec<u64> = self.items_of(kind).filter(|i| i.pre_selected && !i.kept).map(|i| i.id).collect();
        let all: Vec<u64> = self.items_of(kind).map(|i| i.id).collect();
        if recommended.is_empty() {
            self.say("Nothing here is recommended. Open Review to choose items one by one.");
        } else if recommended.iter().all(|id| self.chosen.contains(id)) {
            for id in all {
                self.chosen.remove(&id);
            }
        } else {
            for id in all {
                self.chosen.remove(&id);
            }
            self.chosen.extend(recommended);
        }
        self.recompute();
    }

    pub fn choose_recommended(&mut self) {
        self.chosen = self.items.iter().filter(|i| i.pre_selected && !i.kept).map(|i| i.id).collect();
        self.recompute();
        self.say("Chose everything begone recommends.");
    }

    pub fn choose_none(&mut self) {
        self.chosen.clear();
        self.recompute();
        self.say("Cleared every choice.");
    }

    // ------------------------------------------------------------ the history

    /// The history has been read: show it, or say there is none.
    pub fn apply_history(&mut self, entries: Vec<HistoryEntry>) {
        if entries.is_empty() {
            self.say("Nothing to undo yet: begone has not moved anything.");
        } else if self.dialog.is_none() && !self.phase.busy() {
            self.dialog = Some(Dialog::History(HistoryView { entries, cursor: 0, scroll: Cell::new(0) }));
        }
    }

    /// The newest run that still has something in the Trash: undoing it is the ordinary undo, and any older one is out of order.
    fn newest_restorable(entries: &[HistoryEntry]) -> Option<RunId> {
        entries.iter().find_map(|e| match e {
            HistoryEntry::Run(r) if r.in_trash > 0 => Some(r.run),
            _ => None,
        })
    }

    fn history_key(&mut self, key: KeyEvent, mut view: HistoryView) -> Effect {
        let last = view.entries.len().saturating_sub(1);
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => view.cursor = (view.cursor + 1).min(last),
            KeyCode::Up | KeyCode::Char('k') => view.cursor = view.cursor.saturating_sub(1),
            KeyCode::PageDown => view.cursor = (view.cursor + 8).min(last),
            KeyCode::PageUp => view.cursor = view.cursor.saturating_sub(8),
            KeyCode::Home | KeyCode::Char('g') => view.cursor = 0,
            KeyCode::End | KeyCode::Char('G') => view.cursor = last,
            KeyCode::Esc | KeyCode::Char('U' | 'q') => {
                self.dialog = None;
                return Effect::None;
            }
            KeyCode::Enter | KeyCode::Char('u') => match view.entries.get(view.cursor) {
                Some(HistoryEntry::Run(r)) if r.in_trash > 0 => {
                    let strict = Self::newest_restorable(&view.entries) != Some(r.run);
                    self.dialog = None;
                    return Effect::UndoRun { run: r.run, strict };
                }
                Some(HistoryEntry::Run(_)) => self.say("Nothing of that run is still in the Trash, so there is nothing to put back."),
                Some(HistoryEntry::Emptied { .. }) => self.say("The Trash was emptied then. That cannot be undone."),
                Some(HistoryEntry::Deleted { .. }) => self.say("That was deleted for good by the tool that owns it. It cannot be undone."),
                None => {}
            },
            _ => {}
        }
        self.dialog = Some(Dialog::History(view));
        Effect::None
    }

    // ------------------------------------------------------------ a size to free

    /// `t`: ask how much space is wanted.
    fn begin_goal(&mut self) {
        if self.phase == Phase::Scanning {
            self.say("Still looking. Try again in a moment.");
        } else if self.phase.busy() {
        } else if !matches!(self.tab, Tab::Overview | Tab::Review) {
            self.say("Free up a size from the Overview or Review: press 1 or 2 first.");
        } else {
            self.dialog = Some(Dialog::Goal(GoalView::default()));
        }
    }

    fn goal_key(&mut self, key: KeyEvent, mut view: GoalView) {
        match key.code {
            KeyCode::Esc => {
                self.dialog = None;
                self.say("Cancelled. Nothing was chosen.");
                return;
            }
            KeyCode::Enter => match parse_size(&view.text) {
                Some(bytes) => {
                    self.dialog = None;
                    self.choose_for_goal(bytes);
                    return;
                }
                None if view.text.trim().is_empty() => view.error = Some("Type a size first, such as 20 GB.".to_string()),
                None => view.error = Some("That is not a size. Try 20 GB, 500 MB or 1.5 TB.".to_string()),
            },
            KeyCode::Backspace => {
                view.text.pop();
                view.error = None;
            }
            KeyCode::Char(c) if (c.is_ascii_alphanumeric() || matches!(c, '.' | ',' | ' ')) && view.text.chars().count() < 12 => {
                view.text.push(c);
                view.error = None;
            }
            _ => {}
        }
        self.dialog = Some(Dialog::Goal(view));
    }

    /// What begone recommends and could choose toward a goal: not kept, not in use, never the Trash.
    pub fn recommended_bytes(&self) -> u64 {
        self.items.iter().filter(|i| i.pre_selected && !i.kept && i.kind != Kind::Trash).map(|i| i.size).sum()
    }

    /// Choose the rows begone recommends, safest first, until `target` bytes would be freed. Within a kind of safety, one row
    /// that covers what is still needed beats several, so as little as possible more than asked for is chosen. Nothing moves
    /// until `d`; the choice can be changed row by row like any other.
    pub fn choose_for_goal(&mut self, target: u64) {
        let mut pool: Vec<&ScanItem> = self.items.iter().filter(|i| i.pre_selected && !i.kept && i.kind != Kind::Trash).collect();
        pool.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path)));
        let mut chosen: Vec<u64> = Vec::new();
        let mut got = 0u64;
        for class in [SafetyClass::Safe, SafetyClass::Rebuildable] {
            let mut of: Vec<&ScanItem> = pool.iter().copied().filter(|i| i.safety == class).collect();
            while got < target && !of.is_empty() {
                let need = target - got;
                let pick = of.iter().rposition(|i| i.size >= need).unwrap_or(0);
                let item = of.remove(pick);
                chosen.push(item.id);
                got += item.size;
            }
        }
        let rows = chosen.len();
        self.chosen = chosen.into_iter().collect();
        self.recompute();
        let available = self.recommended_bytes();
        if got >= target {
            self.say(format!(
                "Chose {} in {rows} {} to free {}. Nothing moves until you press d.",
                human(got),
                if rows == 1 { "row" } else { "rows" },
                human(target)
            ));
        } else {
            self.say(format!(
                "begone recommends only {}, {} short of {}. It is all chosen. Review has more you can choose yourself.",
                human(available),
                human(target - got),
                human(target)
            ));
        }
    }

    // ------------------------------------------------------------ the places you keep

    /// The kept place that holds this one, is this one, or sits inside it.
    pub fn kept_by(&self, path: &Path) -> Option<&PathBuf> {
        self.kept.iter().find(|k| path.starts_with(k) || k.starts_with(path))
    }

    /// Start from the list the file has.
    pub fn set_kept(&mut self, list: Vec<PathBuf>) {
        self.kept = list;
        self.refresh_kept();
    }

    /// Recompute what is kept after the list changed: nothing kept stays chosen.
    fn refresh_kept(&mut self) {
        let flags: Vec<bool> = self.items.iter().map(|i| self.kept_by(&i.path).is_some()).collect();
        for (item, kept) in self.items.iter_mut().zip(flags) {
            item.kept = kept;
        }
        let kept: HashSet<u64> = self.items.iter().filter(|i| i.kept).map(|i| i.id).collect();
        self.chosen.retain(|id| !kept.contains(id));
        let flags: Vec<bool> = self.apps.list.iter().map(|e| e.keep_paths().iter().any(|p| self.kept_by(p).is_some())).collect();
        for (entry, kept) in self.apps.list.iter_mut().zip(flags) {
            entry.kept_by_you = kept;
        }
        let kept: HashSet<u64> = self.apps.list.iter().filter(|e| e.kept_by_you).map(|e| e.bundle().id).collect();
        self.apps.chosen.retain(|id| !kept.contains(id));
        let flags: Vec<bool> = self.files.list.iter().map(|i| self.kept_by(&i.path).is_some()).collect();
        for (item, kept) in self.files.list.iter_mut().zip(flags) {
            item.kept = kept;
        }
        let kept: HashSet<u64> = self.files.list.iter().filter(|i| i.kept).map(|i| i.id).collect();
        self.files.chosen.retain(|id| !kept.contains(id));
        self.recompute();
    }

    /// `x`: keep the row the cursor is on for good, or let it go again. A kept place is never offered or moved.
    fn keep_selected(&mut self) -> Effect {
        let paths: Vec<PathBuf> = match self.tab {
            Tab::Overview => {
                self.say("Open Review, the Map or Apps first: x keeps the row you are on.");
                return Effect::None;
            }
            Tab::Review => match self.current_item() {
                Some(item) => vec![item.path.clone()],
                None => {
                    self.say("Move to a row first: x keeps the row you are on.");
                    return Effect::None;
                }
            },
            Tab::Map => match self.map_path_of(self.map_cursor()) {
                Some(path) => vec![path],
                None => {
                    self.say("Open the folder first: x keeps one thing in it, not the small things folded together.");
                    return Effect::None;
                }
            },
            Tab::Apps => match self.apps_selected() {
                Some(entry) => entry.keep_paths(),
                None => return Effect::None,
            },
            Tab::Files => match self.files_selected() {
                Some(item) => vec![item.path.clone()],
                None => return Effect::None,
            },
        };
        let short = |p: &Path| match p.strip_prefix(&self.home) {
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => p.display().to_string(),
        };
        let name = short(&paths[0]);
        // Kept only because a bigger place is kept (or one inside it): that one is the one to let go.
        if let Some(holder) = paths.iter().find_map(|p| self.kept_by(p).filter(|k| !paths.contains(*k)).cloned()) {
            self.say(format!("Kept because {} is kept. x on that place lets it go.", short(&holder)));
            return Effect::None;
        }
        if paths.iter().all(|p| self.kept.contains(p)) {
            self.kept.retain(|k| !paths.contains(k));
            self.refresh_kept();
            self.say(format!("Let go of {name}. begone may offer it again."));
            return Effect::Keep { paths, keep: false };
        }
        for path in &paths {
            if !self.kept.contains(path) {
                self.kept.push(path.clone());
            }
        }
        self.refresh_kept();
        self.say(format!("Kept {name}. begone will not offer it or move it. x lets it go again."));
        Effect::Keep { paths, keep: true }
    }

    pub fn review_rows(&self) -> Vec<ReviewRow> {
        let mut rows = Vec::new();
        for kind in PANEL {
            if kind == Kind::Trash {
                continue;
            }
            let mut idx: Vec<usize> = self.items.iter().enumerate().filter(|(_, i)| i.kind == kind).map(|(n, _)| n).collect();
            if idx.is_empty() {
                continue;
            }
            idx.sort_by(|a, b| self.items[*b].size.cmp(&self.items[*a].size).then_with(|| self.items[*a].path.cmp(&self.items[*b].path)));
            rows.push(ReviewRow::Header(kind));
            let expanded = self.review.expanded[kind.index()];
            let collapsible = idx.len() > MAX_SHOWN + 2;
            let shown = if collapsible && !expanded { MAX_SHOWN } else { idx.len() };
            rows.extend(idx[..shown].iter().map(|n| ReviewRow::Item(*n)));
            if collapsible {
                let hidden = &idx[MAX_SHOWN..];
                rows.push(ReviewRow::More {
                    kind,
                    count: hidden.len(),
                    bytes: hidden.iter().map(|n| self.items[*n].size).sum(),
                    chosen: hidden.iter().filter(|n| self.chosen.contains(&self.items[**n].id)).count(),
                    expanded,
                });
            }
        }
        rows
    }

    pub fn row_key(&self, row: &ReviewRow) -> Option<RowKey> {
        match row {
            ReviewRow::Header(_) => None,
            ReviewRow::Item(n) => Some(RowKey::Item(self.items[*n].id)),
            ReviewRow::More { kind, .. } => Some(RowKey::More(*kind)),
        }
    }

    fn selectable(&self) -> Vec<RowKey> {
        self.review_rows().iter().filter_map(|r| self.row_key(r)).collect()
    }

    /// Keep both cursors pointing at something real after the rows change.
    fn fix_cursors(&mut self) {
        let keys = self.selectable();
        if !self.review.cursor.is_some_and(|c| keys.contains(&c)) {
            self.review.cursor = keys.first().copied();
        }
    }

    fn move_review(&mut self, delta: isize) {
        let keys = self.selectable();
        if keys.is_empty() {
            return;
        }
        let at = self.review.cursor.and_then(|c| keys.iter().position(|k| *k == c)).unwrap_or(0) as isize;
        self.review.cursor = Some(keys[(at + delta).clamp(0, keys.len() as isize - 1) as usize]);
    }

    fn jump_group(&mut self, forward: bool) {
        let rows = self.review_rows();
        let here = self.review.cursor.and_then(|c| rows.iter().position(|r| self.row_key(r) == Some(c))).unwrap_or(0);
        let headers: Vec<usize> = rows.iter().enumerate().filter(|(_, r)| matches!(r, ReviewRow::Header(_))).map(|(n, _)| n).collect();
        let target =
            if forward { headers.iter().find(|h| **h > here) } else { headers.iter().rev().find(|h| **h < here.saturating_sub(1)) };
        if let Some(h) = target
            && let Some(key) = rows.get(h + 1).and_then(|r| self.row_key(r))
        {
            self.review.cursor = Some(key);
        }
    }

    /// The item under the review cursor, if the cursor is on an item.
    pub fn current_item(&self) -> Option<&ScanItem> {
        match self.review.cursor? {
            RowKey::Item(id) => self.items.iter().find(|i| i.id == id),
            RowKey::More(_) => None,
        }
    }

    pub fn toggle_current(&mut self) {
        match self.review.cursor {
            Some(RowKey::Item(id)) => {
                if self.items.iter().any(|i| i.id == id && i.kept) {
                    self.say("You asked begone to keep this, so it cannot be chosen. x lets it go.");
                    return;
                }
                if !self.chosen.remove(&id) {
                    self.chosen.insert(id);
                }
            }
            Some(RowKey::More(kind)) => {
                let rows = self.review_rows();
                let hidden: Vec<u64> = {
                    let mut idx: Vec<&ScanItem> = self.items_of(kind).collect();
                    idx.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path)));
                    idx.into_iter().skip(MAX_SHOWN).map(|i| i.id).collect()
                };
                let _ = rows;
                if hidden.iter().all(|id| self.chosen.contains(id)) {
                    for id in &hidden {
                        self.chosen.remove(id);
                    }
                } else {
                    let rec: Vec<u64> =
                        self.items_of(kind).filter(|i| i.pre_selected && !i.kept && hidden.contains(&i.id)).map(|i| i.id).collect();
                    self.chosen.extend(rec);
                }
            }
            None => {}
        }
        self.recompute();
    }

    // ------------------------------------------------------------ dialogs and actions

    pub fn begin_confirm(&mut self) {
        if self.phase == Phase::Scanning {
            self.say("Still looking. Begone is ready in a moment.");
            return;
        }
        if self.phase.busy() {
            return;
        }
        let items = self.chosen_items();
        if items.is_empty() {
            self.say("Nothing is chosen. Space chooses a row, and a chooses everything begone recommends.");
            return;
        }
        let mut per_kind = Vec::new();
        for kind in PANEL {
            let of: Vec<&ScanItem> = items.iter().filter(|i| i.kind == kind).collect();
            if !of.is_empty() {
                per_kind.push((kind, of.len(), of.iter().map(|i| i.size).sum()));
            }
        }
        self.dialog = Some(Dialog::ConfirmMove(ConfirmView {
            count: items.len(),
            bytes: items.iter().map(|i| i.size).sum(),
            per_kind,
            trash_bytes: self.trash.as_ref().filter(|t| t.readable).map(|t| t.bytes),
        }));
    }

    fn begin_confirm_empty(&mut self) {
        if self.phase == Phase::Scanning {
            self.say("Still looking. Try again in a moment.");
            return;
        }
        if self.phase.busy() {
            return;
        }
        let Some(trash) = self.trash.as_ref() else { return };
        if !trash.readable {
            self.warn(trash.note.clone().unwrap_or_else(|| "The Trash cannot be read.".to_string()) + ". Empty it from Finder instead.");
            return;
        }
        if self.trash_items() == 0 {
            self.say("The Trash is already empty.");
            return;
        }
        let (items, bytes) = (self.trash_items(), self.trash_bytes());
        // What the history proves begone moved, when it is only part of what is there.
        let (mine_items, mine_bytes) = trash.entries.iter().filter(|e| e.begone).fold((0, 0), |(n, b), e| (n + 1, b + e.bytes));
        let partly = mine_items > 0 && mine_items < trash.entries.len();
        let from_begone = if mine_items > 0 { mine_bytes } else { self.moved_to_trash };
        self.dialog = Some(Dialog::ConfirmEmpty(EmptyView { items, bytes, from_begone, mine: partly.then_some((mine_items, mine_bytes)) }));
    }

    fn request_quit(&mut self) -> Effect {
        if self.phase.busy() {
            self.quit_after = true;
            self.say("Finishing safely, then quitting.");
            return Effect::Stop;
        }
        Effect::Quit
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Effect {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return self.request_quit();
        }
        if self.in_splash() {
            self.skip_splash();
            return Effect::None;
        }

        match self.dialog.clone() {
            Some(Dialog::Help) => {
                self.dialog = None;
                return Effect::None;
            }
            Some(Dialog::ConfirmMove(_)) => {
                match key.code {
                    KeyCode::Char('y' | 'Y') => {
                        self.dialog = None;
                        let items = self.chosen_items();
                        self.begin_move(&items);
                        return Effect::StartMove(items);
                    }
                    KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                        self.dialog = None;
                        self.say("Cancelled. Nothing was touched.");
                    }
                    _ => {}
                }
                return Effect::None;
            }
            Some(Dialog::ConfirmUninstall(view)) => {
                match key.code {
                    KeyCode::Char('y' | 'Y') if view.sims => {
                        self.dialog = None;
                        let items = std::mem::take(&mut self.pending_uninstall);
                        self.begin_sim_delete(&items);
                        return Effect::DeleteSimulators(items);
                    }
                    KeyCode::Char('y' | 'Y') => {
                        self.dialog = None;
                        let items = std::mem::take(&mut self.pending_uninstall);
                        self.begin_move(&items);
                        return Effect::StartMove(items);
                    }
                    KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                        self.dialog = None;
                        self.pending_uninstall.clear();
                        self.say("Cancelled. Nothing was touched.");
                    }
                    _ => {}
                }
                return Effect::None;
            }
            Some(Dialog::Goal(view)) => {
                self.goal_key(key, view);
                return Effect::None;
            }
            Some(Dialog::History(view)) => return self.history_key(key, view),
            Some(Dialog::ConfirmEmpty(view)) => {
                match key.code {
                    KeyCode::Char('y' | 'Y') => {
                        self.begin_empty(EmptyScope::Everything);
                        return Effect::StartEmpty(EmptyScope::Everything);
                    }
                    KeyCode::Char('b' | 'B') if view.mine.is_some() => {
                        self.begin_empty(EmptyScope::OnlyBegones);
                        return Effect::StartEmpty(EmptyScope::OnlyBegones);
                    }
                    KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                        self.dialog = None;
                        self.say("Cancelled. The Trash was not touched.");
                    }
                    _ => {}
                }
                return Effect::None;
            }
            None => {}
        }

        if self.phase.busy() {
            return match key.code {
                KeyCode::Esc => {
                    self.say("Stopping after this item.");
                    Effect::Stop
                }
                KeyCode::Char('q') => self.request_quit(),
                _ => Effect::None,
            };
        }
        if self.phase == Phase::Receipt && matches!(key.code, KeyCode::Enter | KeyCode::Esc | KeyCode::Char(' ')) {
            self.phase = Phase::Ready;
            return Effect::None;
        }

        let mut effect = Effect::None;
        match key.code {
            KeyCode::Char('q') => return Effect::Quit,
            KeyCode::Char('?') => self.dialog = Some(Dialog::Help),
            KeyCode::Tab | KeyCode::BackTab => {
                self.cycle_tab(key.code == KeyCode::Tab);
                self.phase = if self.phase == Phase::Receipt { Phase::Ready } else { self.phase };
                effect = match self.tab {
                    Tab::Map => self.map_wanted(),
                    Tab::Apps => self.apps_wanted(),
                    Tab::Files => self.files_wanted(),
                    _ => Effect::None,
                };
            }
            KeyCode::Char('4') => {
                self.tab = Tab::Apps;
                effect = self.apps_wanted();
            }
            KeyCode::Char('5') => {
                self.tab = Tab::Files;
                effect = self.files_wanted();
            }
            KeyCode::Char('d') if self.tab == Tab::Files => self.begin_confirm_files(),
            KeyCode::Char('a') if self.tab == Tab::Files => self.files_choose_all_duplicates(),
            KeyCode::Char('n') if self.tab == Tab::Files => self.files.chosen.clear(),
            // On the Apps tab these keys are about apps, not about the cleaning pile.
            KeyCode::Char('d') if self.tab == Tab::Apps => self.begin_confirm_apps(),
            KeyCode::Char('a') if self.tab == Tab::Apps => self.say("Apps are never chosen for you. Space picks the ones to remove."),
            KeyCode::Char('n') if self.tab == Tab::Apps => self.apps.chosen.clear(),
            KeyCode::Char('x') => effect = self.keep_selected(),
            KeyCode::Char('t') => self.begin_goal(),
            KeyCode::Char('U') => {
                if self.phase == Phase::Scanning {
                    self.say("Still looking. The history is ready in a moment.");
                } else {
                    effect = Effect::OpenHistory;
                }
            }
            KeyCode::Char('m') => {
                // Already on the map: m maps again.
                if self.tab == Tab::Map {
                    self.map.stale = true;
                    self.apps.stale = true;
                }
                self.tab = Tab::Map;
                effect = self.map_wanted();
            }
            // The tabs by number, from anywhere: 1 Overview, 2 Review, 3 Map. (The letters work too.)
            KeyCode::Char('1' | 'o') => self.tab = Tab::Overview,
            KeyCode::Char('2' | 'r') => self.tab = Tab::Review,
            KeyCode::Char('3') => {
                self.tab = Tab::Map;
                effect = self.map_wanted();
            }
            KeyCode::Char('d') => self.begin_confirm(),
            KeyCode::Char('u') => {
                if self.phase == Phase::Scanning {
                    self.say("Still looking. Undo is ready in a moment.");
                } else {
                    return Effect::Undo;
                }
            }
            KeyCode::Char('e') => self.begin_confirm_empty(),
            KeyCode::Char('a') => self.choose_recommended(),
            KeyCode::Char('n') => self.choose_none(),
            _ => return self.handle_tab_key(key),
        }
        if self.phase == Phase::Receipt {
            self.phase = Phase::Ready;
        }
        effect
    }

    fn cycle_tab(&mut self, forward: bool) {
        self.tab = match (self.tab, forward) {
            (Tab::Overview, true) | (Tab::Map, false) => Tab::Review,
            (Tab::Review, true) | (Tab::Apps, false) => Tab::Map,
            (Tab::Map, true) | (Tab::Files, false) => Tab::Apps,
            (Tab::Apps, true) | (Tab::Overview, false) => Tab::Files,
            (Tab::Files, true) | (Tab::Review, false) => Tab::Overview,
        };
    }

    // ------------------------------------------------------------ the apps

    /// Ask for the list of apps if there is none, or it is out of date, and one is not already being read.
    fn apps_wanted(&mut self) -> Effect {
        if self.apps.scanning || (self.apps.scanned && !self.apps.stale) {
            return Effect::None;
        }
        self.begin_apps_scan();
        Effect::ScanApps
    }

    pub fn begin_apps_scan(&mut self) {
        self.apps.scanning = true;
        self.apps.progress = 0.0;
        self.apps.stale = false;
        self.apps.list.clear();
        self.apps.chosen.clear();
        self.apps.cursor = 0;
        self.apps.touched = false;
        self.apps.scroll.set(0);
    }

    pub fn apply_apps(&mut self, event: AppsEvent) {
        match event {
            AppsEvent::Progress(p) => self.apps.progress = p,
            AppsEvent::Found(entry) => {
                // Biggest first, by name among equals. Once the person has moved, the cursor stays on its app while others
                // arrive; until then it stays at the top, where the biggest app will be.
                let selected = self.apps.touched.then(|| self.apps_selected().map(|e| e.bundle().id)).flatten();
                let at = self.apps.list.partition_point(|e| app_order(self.sort, e, &entry).is_le());
                self.apps.list.insert(at, *entry);
                self.apps.cursor = selected.and_then(|id| self.apps.list.iter().position(|e| e.bundle().id == id)).unwrap_or(0);
            }
            AppsEvent::Finished { elapsed } => {
                self.apps.read_in = Some(elapsed);
                self.apps.scanning = false;
                self.apps.scanned = true;
                self.apps.progress = 1.0;
            }
        }
    }

    /// The app the cursor is on.
    pub fn apps_selected(&self) -> Option<&AppEntry> {
        self.apps.list.get(self.apps.cursor.min(self.apps.list.len().saturating_sub(1)))
    }

    /// The chosen apps, with everything that would move with each.
    pub fn apps_chosen(&self) -> impl Iterator<Item = &AppEntry> {
        self.apps.list.iter().filter(|e| self.apps.chosen.contains(&e.bundle().id))
    }

    pub fn apps_chosen_bytes(&self) -> u64 {
        self.apps_chosen().map(AppEntry::bytes).sum()
    }

    fn apps_key(&mut self, key: KeyEvent) -> Effect {
        let last = self.apps.list.len().saturating_sub(1);
        let at = self.apps.cursor.min(last);
        if matches!(
            key.code,
            KeyCode::Down
                | KeyCode::Up
                | KeyCode::PageDown
                | KeyCode::PageUp
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::Char('j' | 'k' | 'g' | 'G' | ' ')
        ) {
            self.apps.touched = true;
        }
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.apps.cursor = (at + 1).min(last),
            KeyCode::Up | KeyCode::Char('k') => self.apps.cursor = at.saturating_sub(1),
            KeyCode::PageDown => self.apps.cursor = (at + 10).min(last),
            KeyCode::PageUp => self.apps.cursor = at.saturating_sub(10),
            KeyCode::Char('g') | KeyCode::Home => self.apps.cursor = 0,
            KeyCode::Char('G') | KeyCode::End => self.apps.cursor = last,
            KeyCode::Char(' ') => self.apps_toggle(),
            KeyCode::Enter => self.begin_confirm_apps(),
            KeyCode::Char('f') => {
                if let Some(entry) = self.apps_selected() {
                    return Effect::Reveal(entry.identity.path.clone());
                }
            }
            KeyCode::Char('s') => self.cycle_sort(),
            KeyCode::Esc => self.tab = Tab::Overview,
            _ => {}
        }
        Effect::None
    }

    /// Reorder the Files and Apps lists, keeping each cursor on the row it was on.
    fn cycle_sort(&mut self) {
        self.sort = self.sort.next();
        let files_id = self.files_selected().map(|i| i.id);
        self.files.list.sort_by(|a, b| file_order(self.sort, a, b));
        if let Some(id) = files_id {
            self.files.cursor = self.files.list.iter().position(|i| i.id == id).unwrap_or(0);
        }
        let apps_id = self.apps_selected().map(|e| e.bundle().id);
        self.apps.list.sort_by(|a, b| app_order(self.sort, a, b));
        if let Some(id) = apps_id {
            self.apps.cursor = self.apps.list.iter().position(|e| e.bundle().id == id).unwrap_or(0);
        }
        self.say(format!("Sorted by {}.", self.sort.label()));
    }

    fn apps_toggle(&mut self) {
        let Some(entry) = self.apps_selected() else { return };
        let (id, running, name) = (entry.bundle().id, entry.running, entry.identity.name.clone());
        if entry.kept_by_you {
            self.say(format!("You asked begone to keep {name}, so it cannot be chosen. x lets it go."));
        } else if running {
            self.say(format!("{name} is running. Quit it first, then it can be chosen."));
        } else if !self.apps.chosen.remove(&id) {
            self.apps.chosen.insert(id);
        }
    }

    /// Ask before removing the chosen apps.
    fn begin_confirm_apps(&mut self) {
        if self.phase.busy() {
            return;
        }
        if self.apps.scanning {
            self.say("Still reading your apps. Try again in a moment.");
            return;
        }
        let chosen: Vec<&AppEntry> = self.apps_chosen().collect();
        if chosen.is_empty() {
            self.say("No app is chosen. Space chooses one.");
            return;
        }
        let items: Vec<ScanItem> = chosen.iter().flat_map(|e| e.items.iter().cloned()).collect();
        let view = UninstallView {
            apps: chosen
                .iter()
                .map(|e| UninstallRow { name: e.identity.name.clone(), bytes: e.bytes(), traces: e.trace_count(), leftover: e.leftover })
                .collect(),
            bytes: chosen.iter().map(|e| e.bytes()).sum(),
            kept_bytes: chosen.iter().map(|e| e.kept_bytes()).sum(),
            cautions: {
                let mut seen: Vec<String> = Vec::new();
                for caution in chosen.iter().flat_map(|e| e.cautions.iter()) {
                    if !seen.contains(caution) {
                        seen.push(caution.clone());
                    }
                }
                seen
            },
            trash_bytes: self.trash.as_ref().filter(|t| t.readable).map(|t| t.bytes),
            files: false,
            sims: false,
        };
        self.pending_uninstall = items;
        self.dialog = Some(Dialog::ConfirmUninstall(view));
    }

    // ------------------------------------------------------------ the files

    /// Ask for the list if there is none or it is out of date, and one is not already being read.
    fn files_wanted(&mut self) -> Effect {
        if self.files.scanning || (self.files.scanned && !self.files.stale) {
            return Effect::None;
        }
        self.begin_files_scan();
        Effect::ScanFiles
    }

    pub fn begin_files_scan(&mut self) {
        self.files.scanning = true;
        self.files.progress = 0.0;
        self.files.stale = false;
        self.files.list.clear();
        self.files.chosen.clear();
        self.files.notes.clear();
        self.files.cursor = 0;
        self.files.touched = false;
        self.files.scroll.set(0);
    }

    pub fn apply_files(&mut self, event: FilesEvent) {
        match event {
            FilesEvent::Progress(p) => self.files.progress = p,
            FilesEvent::Note(note) => self.files.notes.push(note),
            FilesEvent::Found(item) => {
                let mut item = *item;
                item.kept = item.kept || self.kept_by(&item.path).is_some();
                // Biggest first, by name among equals; the cursor stays on its row once the person has moved.
                let selected = self.files.touched.then(|| self.files_selected().map(|i| i.id)).flatten();
                let at = self.files.list.partition_point(|i| file_order(self.sort, i, &item).is_le());
                self.files.list.insert(at, item);
                self.files.cursor = selected.and_then(|id| self.files.list.iter().position(|i| i.id == id)).unwrap_or(0);
            }
            FilesEvent::Finished { elapsed } => {
                self.files.read_in = Some(elapsed);
                self.files.scanning = false;
                self.files.scanned = true;
                self.files.progress = 1.0;
            }
        }
    }

    /// The row the cursor is on.
    pub fn files_selected(&self) -> Option<&ScanItem> {
        self.files.list.get(self.files.cursor.min(self.files.list.len().saturating_sub(1)))
    }

    pub fn files_chosen(&self) -> impl Iterator<Item = &ScanItem> {
        self.files.list.iter().filter(|i| self.files.chosen.contains(&i.id) && !i.kept)
    }

    pub fn files_chosen_bytes(&self) -> u64 {
        self.files_chosen().map(|i| i.size).sum()
    }

    fn files_key(&mut self, key: KeyEvent) -> Effect {
        let last = self.files.list.len().saturating_sub(1);
        let at = self.files.cursor.min(last);
        if matches!(
            key.code,
            KeyCode::Down
                | KeyCode::Up
                | KeyCode::PageDown
                | KeyCode::PageUp
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::Char('j' | 'k' | 'g' | 'G' | ' ')
        ) {
            self.files.touched = true;
        }
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.files.cursor = (at + 1).min(last),
            KeyCode::Up | KeyCode::Char('k') => self.files.cursor = at.saturating_sub(1),
            KeyCode::PageDown => self.files.cursor = (at + 10).min(last),
            KeyCode::PageUp => self.files.cursor = at.saturating_sub(10),
            KeyCode::Char('g') | KeyCode::Home => self.files.cursor = 0,
            KeyCode::Char('G') | KeyCode::End => self.files.cursor = last,
            KeyCode::Char(' ') => self.files_toggle(),
            KeyCode::Enter => self.begin_confirm_files(),
            KeyCode::Char('f') => {
                if let Some(item) = self.files_selected() {
                    return Effect::Reveal(item.path.clone());
                }
            }
            KeyCode::Char('s') => self.cycle_sort(),
            KeyCode::Esc => self.tab = Tab::Overview,
            _ => {}
        }
        Effect::None
    }

    /// Choose every duplicate copy at once — they are the one safe thing to pick in bulk, since each leaves an identical
    /// original. A second press clears them.
    fn files_choose_all_duplicates(&mut self) {
        let dupes: Vec<u64> =
            self.files.list.iter().filter(|i| i.grant == crate::safety::Grant::DuplicateCopy && !i.kept).map(|i| i.id).collect();
        if dupes.is_empty() {
            self.say("No duplicates to choose here. Space chooses one item at a time.");
            return;
        }
        let plural = if dupes.len() == 1 { "" } else { "s" };
        if dupes.iter().all(|id| self.files.chosen.contains(id)) {
            for id in &dupes {
                self.files.chosen.remove(id);
            }
            self.say(format!("Cleared {} duplicate{plural}.", dupes.len()));
        } else {
            for id in &dupes {
                self.files.chosen.insert(*id);
            }
            self.say(format!("Chose {} duplicate{plural} — an identical copy stays for each.", dupes.len()));
        }
    }

    fn files_toggle(&mut self) {
        let Some(item) = self.files_selected() else { return };
        let (id, kept) = (item.id, item.kept);
        if kept {
            self.say("You asked begone to keep this, so it cannot be chosen. x lets it go.");
        } else if self.files_selected().is_some_and(|i| i.tag == crate::model::Tag::Running) {
            self.say("That simulator is running. Shut it down first, then it can be chosen.");
        } else if !self.files.chosen.remove(&id) {
            self.files.chosen.insert(id);
        }
    }

    /// Ask before moving the chosen files to the Trash.
    fn begin_confirm_files(&mut self) {
        if self.phase.busy() {
            return;
        }
        if self.files.scanning {
            self.say("Still looking. Try again in a moment.");
            return;
        }
        let chosen: Vec<ScanItem> = self.files_chosen().cloned().collect();
        if chosen.is_empty() {
            self.say("No file is chosen. Space chooses one.");
            return;
        }
        let sims = chosen.iter().filter(|i| i.grant == crate::safety::Grant::Simulator).count();
        if sims > 0 && sims < chosen.len() {
            self.say("Simulators are deleted for good, so they are asked about on their own. Choose files or simulators, not both.");
            return;
        }
        let backups = chosen.iter().filter(|i| i.grant == crate::safety::Grant::DeviceBackup).count();
        let vms = chosen.iter().filter(|i| i.grant == crate::safety::Grant::VmImage).count();
        let mut cautions = Vec::new();
        if backups > 0 {
            cautions.push("A device backup may be the only copy of what is on that device. Be sure you will not need it.".to_string());
        }
        if vms > 0 {
            cautions.push("A virtual machine or container store holds everything inside it. Be sure you will not need it.".to_string());
        }
        let view = UninstallView {
            apps: chosen.iter().map(|i| UninstallRow { name: i.title.clone(), bytes: i.size, traces: 0, leftover: false }).collect(),
            bytes: chosen.iter().map(|i| i.size).sum(),
            kept_bytes: 0,
            cautions,
            trash_bytes: self.trash.as_ref().filter(|t| t.readable).map(|t| t.bytes),
            files: true,
            sims: sims > 0,
        };
        self.pending_uninstall = chosen;
        self.dialog = Some(Dialog::ConfirmUninstall(view));
    }

    // ------------------------------------------------------------ the map

    /// Ask for a map if there is none, or it is out of date, and one is not already being made.
    fn map_wanted(&mut self) -> Effect {
        if self.map.building || (self.map.tree.is_some() && !self.map.stale) {
            return Effect::None;
        }
        self.begin_map();
        Effect::BuildMap(self.home.clone())
    }

    pub fn begin_map(&mut self) {
        self.map.building = true;
        self.map.failed = false;
        self.map.seen = (0, 0);
    }

    pub fn apply_map(&mut self, event: MapEvent) {
        match event {
            MapEvent::Progress { files, bytes } => self.map.seen = (files, bytes),
            MapEvent::Done(tree) => {
                self.map.tree = Some(tree);
                self.map.building = false;
                self.map.stale = false;
                self.map.path.clear();
                self.map.cursor = vec![0];
                self.map.scroll.set(0);
            }
            MapEvent::Failed => {
                self.map.building = false;
                self.map.failed = true;
            }
        }
    }

    /// The folder the map is showing, once there is a map.
    pub fn map_here(&self) -> Option<&MapNode> {
        self.map.tree.as_ref()?.node_at(&self.map.path)
    }

    /// The rest of the disk, which is not in the folder being mapped: apps, macOS, other users. Only meaningful
    /// at the top of the real home folder, where it is shown as one dim row.
    pub fn map_rest(&self) -> Option<u64> {
        let tree = self.map.tree.as_ref()?;
        if !self.map.path.is_empty() || self.disk.total == 0 || self.home != crate::safety::home_dir() {
            return None;
        }
        let rest = self.disk.total.saturating_sub(self.disk.free).saturating_sub(tree.node.bytes);
        (rest > 0).then_some(rest)
    }

    /// Rows in the list: the children, then the rest of the disk when there is one.
    pub fn map_rows(&self) -> usize {
        self.map_here().map_or(0, |n| n.children.len()) + usize::from(self.map_rest().is_some())
    }

    pub fn map_cursor(&self) -> usize {
        self.map.cursor.last().copied().unwrap_or(0).min(self.map_rows().saturating_sub(1))
    }

    /// Where on disk a child of the folder being shown is. `None` for the folded "smaller items" entry.
    pub fn map_path_of(&self, child: usize) -> Option<PathBuf> {
        let tree = self.map.tree.as_ref()?;
        let node = self.map_here()?.children.get(child)?;
        (node.kind != NodeKind::Smaller).then(|| tree.path_to(&self.map.path).join(&node.name))
    }

    /// Bytes of what begone found that lie under `path` (or are it), and how many of those are chosen.
    pub fn junk_under(&self, path: &Path) -> (u64, u64) {
        let mut all = 0;
        let mut chosen = 0;
        for item in self.items.iter().filter(|i| i.path.starts_with(path) && !i.kept) {
            all += item.size;
            if self.chosen.contains(&item.id) {
                chosen += item.size;
            }
        }
        (all, chosen)
    }

    fn map_move(&mut self, to: usize) {
        let last = self.map_rows().saturating_sub(1);
        if let Some(c) = self.map.cursor.last_mut() {
            *c = to.min(last);
        }
    }

    /// One folder up. False when already at the top.
    fn map_up(&mut self) -> bool {
        if self.map.path.is_empty() {
            return false;
        }
        self.map.path.pop();
        self.map.cursor.pop();
        self.map.scroll.set(0);
        true
    }

    fn map_open(&mut self) {
        let at = self.map_cursor();
        let Some(node) = self.map_here() else { return };
        let Some(child) = node.children.get(at) else {
            self.say("That is the rest of the disk, outside your home: apps, macOS and other users.");
            return;
        };
        match child.kind {
            NodeKind::Dir if !child.children.is_empty() => {
                self.map.path.push(at);
                self.map.cursor.push(0);
                self.map.scroll.set(0);
            }
            NodeKind::Dir => self.say("Nothing to open: it is empty, or too small to list."),
            NodeKind::File => self.say("That is a file. Press f to show it in Finder."),
            NodeKind::Smaller => {
                self.say("Small things, folded together to keep the list short. Open one of the bigger folders to see more.")
            }
        }
    }

    fn map_reveal(&mut self) -> Effect {
        let Some(tree) = self.map.tree.as_ref() else { return Effect::None };
        let here = tree.path_to(&self.map.path);
        let at = self.map_cursor();
        match self.map_here().and_then(|n| n.children.get(at)) {
            Some(_) => match self.map_path_of(at) {
                Some(path) => Effect::Reveal(path),
                // The folded entry has no single place, so show the folder that holds it.
                None => Effect::Reveal(here),
            },
            None if self.map_rest().is_some() => {
                self.say("That is the rest of the disk, outside your home.");
                Effect::None
            }
            None => Effect::Reveal(here),
        }
    }

    /// Space on the map chooses what begone found at exactly this place, the same as in Review. Nothing else is
    /// ever chosen from here: begone only moves what it can prove is safe.
    fn map_toggle(&mut self) {
        let at = self.map_cursor();
        let Some(path) = self.map_path_of(at) else {
            self.say("Nothing here for begone to choose. It only moves what it can prove is safe.");
            return;
        };
        if let Some(item) = self.items.iter().find(|i| i.path == path) {
            let id = item.id;
            if item.kept {
                self.say("You asked begone to keep this, so it cannot be chosen. x lets it go.");
                return;
            }
            if !self.chosen.remove(&id) {
                self.chosen.insert(id);
            }
            self.recompute();
        } else if self.junk_under(&path).0 > 0 {
            self.say("Part of this can go. Open it and choose the item, or use Review.");
        } else {
            self.say("begone only moves what it can prove is safe, and this is not one of those. f shows it in Finder.");
        }
    }

    fn map_key(&mut self, key: KeyEvent) -> Effect {
        let at = self.map_cursor();
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.map_move(at + 1),
            KeyCode::Up | KeyCode::Char('k') => self.map_move(at.saturating_sub(1)),
            KeyCode::PageDown => self.map_move(at + 10),
            KeyCode::PageUp => self.map_move(at.saturating_sub(10)),
            KeyCode::Char('g') | KeyCode::Home => self.map_move(0),
            KeyCode::Char('G') | KeyCode::End => self.map_move(usize::MAX),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => self.map_open(),
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => {
                self.map_up();
            }
            KeyCode::Esc => {
                if !self.map_up() {
                    self.tab = Tab::Overview;
                }
            }
            KeyCode::Char('f') => return self.map_reveal(),
            KeyCode::Char(' ') => self.map_toggle(),
            _ => {}
        }
        Effect::None
    }

    fn handle_tab_key(&mut self, key: KeyEvent) -> Effect {
        if self.tab == Tab::Map {
            return self.map_key(key);
        }
        if self.tab == Tab::Apps {
            return self.apps_key(key);
        }
        if self.tab == Tab::Files {
            return self.files_key(key);
        }
        match self.tab {
            Tab::Overview => match key.code {
                KeyCode::Down | KeyCode::Char('j') => self.kind_cursor = (self.kind_cursor + 1).min(PANEL.len() - 1),
                KeyCode::Up | KeyCode::Char('k') => self.kind_cursor = self.kind_cursor.saturating_sub(1),
                KeyCode::Char('g') | KeyCode::Home => self.kind_cursor = 0,
                KeyCode::Char('G') | KeyCode::End => self.kind_cursor = PANEL.len() - 1,
                KeyCode::Char(' ') => self.toggle_kind(PANEL[self.kind_cursor]),
                KeyCode::Enter => self.begin_confirm(),
                KeyCode::Right | KeyCode::Char('l') => {
                    self.tab = Tab::Review;
                    let kind = PANEL[self.kind_cursor];
                    if let Some(item) = self.review_rows().iter().find_map(|r| match r {
                        ReviewRow::Item(n) if self.items[*n].kind == kind => Some(self.items[*n].id),
                        _ => None,
                    }) {
                        self.review.cursor = Some(RowKey::Item(item));
                    }
                }
                _ => {}
            },
            Tab::Map | Tab::Apps | Tab::Files => {}
            Tab::Review => match key.code {
                KeyCode::Down | KeyCode::Char('j') => self.move_review(1),
                KeyCode::Up | KeyCode::Char('k') => self.move_review(-1),
                KeyCode::PageDown => self.move_review(10),
                KeyCode::PageUp => self.move_review(-10),
                KeyCode::Char('g') | KeyCode::Home => self.move_review(-100_000),
                KeyCode::Char('G') | KeyCode::End => self.move_review(100_000),
                KeyCode::Char(']') => self.jump_group(true),
                KeyCode::Char('[') => self.jump_group(false),
                KeyCode::Char(' ') => self.toggle_current(),
                KeyCode::Char('f') => {
                    if let Some(RowKey::Item(id)) = self.review.cursor
                        && let Some(item) = self.items.iter().find(|i| i.id == id)
                    {
                        return Effect::Reveal(item.path.clone());
                    }
                }
                KeyCode::Enter | KeyCode::Right | KeyCode::Left | KeyCode::Char('l' | 'h') => {
                    if let Some(RowKey::More(kind)) = self.review.cursor {
                        let e = &mut self.review.expanded[kind.index()];
                        *e = !*e;
                    }
                }
                KeyCode::Esc => self.tab = Tab::Overview,
                _ => {}
            },
        }
        Effect::None
    }
}

/// Split the 0..=1 motion between the kinds that have something to move, in proportion to
/// their bytes, with a floor so a tiny kind still gets a moment of its own.
fn windows(plan: &[u64; Kind::COUNT]) -> [(f32, f32); Kind::COUNT] {
    let total: u64 = plan.iter().enumerate().filter(|(i, _)| *i != Kind::Trash.index()).map(|(_, b)| *b).sum();
    let mut out = [(0.0, 0.0); Kind::COUNT];
    if total == 0 {
        return out;
    }
    let order: Vec<Kind> =
        PANEL.iter().copied().chain([Kind::Apps, Kind::Files]).filter(|k| *k != Kind::Trash && plan[k.index()] > 0).collect();
    let shares: Vec<f32> = order.iter().map(|k| (plan[k.index()] as f32 / total as f32).max(0.08)).collect();
    let sum: f32 = shares.iter().sum();
    let mut at = 0.0;
    for (kind, share) in order.iter().zip(shares) {
        let width = share / sum * DEPART_SPAN;
        out[kind.index()] = (at, at + width);
        at += width;
    }
    out
}

/// The receipt: what moved, what was left alone, and how to undo it.
pub fn build_receipt(state: &State, report: &Report) -> Vec<ReceiptLine> {
    let mk = |left: &str, right: &str, style| ReceiptLine { left: left.to_string(), right: right.to_string(), style };
    let mut lines = vec![
        mk("", "", LineStyle::Blank),
        mk("B E G O N E", "", LineStyle::Title),
        mk("say it and it's done", "", LineStyle::Sub),
        mk(&sys::local_stamp(sys::now_epoch()), "", LineStyle::Sub),
        mk("", "", LineStyle::Rule),
    ];
    for kind in PANEL.into_iter().chain([Kind::Apps, Kind::Files]) {
        let of: Vec<_> = report.moved().filter(|r| r.kind == kind).collect();
        if !of.is_empty() {
            let bytes: u64 = of.iter().map(|r| r.bytes).sum();
            lines.push(mk(&format!("{} · {}", kind.label(), of.len()), &human(bytes), LineStyle::Normal));
        }
    }
    lines.push(mk("", "", LineStyle::Rule));
    let moved = report.moved_bytes();
    lines.push(mk("Moved to the Trash", &human(moved), LineStyle::Bold));
    let before = state.trash.as_ref().filter(|t| t.readable).map(|t| t.bytes);
    if let Some(before) = before {
        lines.push(mk("Was already there", &human(before), LineStyle::Dim));
        lines.push(mk("FREE WHEN YOU EMPTY IT", &human(moved + before), LineStyle::Accent));
    } else {
        lines.push(mk("FREE WHEN YOU EMPTY IT", &human(moved), LineStyle::Accent));
        lines.push(mk("Your Trash could not be read, so its own contents are not counted.", "", LineStyle::Dim));
    }
    let kept = report.kept();
    // After removing apps, the cleaning list's unchosen rows are not what was left alone.
    let unchecked = if state.run.plan[Kind::Apps.index()] + state.run.plan[Kind::Files.index()] > 0 {
        0
    } else {
        state.items.iter().filter(|i| !i.kept && !state.chosen.contains(&i.id)).count()
    };
    if !kept.is_empty() || unchecked > 0 {
        lines.push(mk("", "", LineStyle::Rule));
        lines.push(mk("Left alone:", "", LineStyle::Dim));
        for r in kept.iter().take(3) {
            let why = match &r.outcome {
                Outcome::Refused(w) | Outcome::Failed(w) => w.clone(),
                _ => "not attempted".to_string(),
            };
            lines.push(mk(&format!("  {}: {why}", r.title), "", LineStyle::Dim));
        }
        if kept.len() > 3 {
            lines.push(mk(&format!("  and {} more", kept.len() - 3), "", LineStyle::Dim));
        }
        if unchecked > 0 {
            lines.push(mk(&format!("  {unchecked} items you left unchecked"), "", LineStyle::Dim));
        }
    }
    lines.push(mk("", "", LineStyle::Rule));
    lines.push(mk("u  bring it all back", "", LineStyle::Normal));
    lines.push(mk("e  empty the Trash for good", "", LineStyle::Normal));
    lines.push(mk("", "", LineStyle::Blank));
    lines.push(mk("", "", LineStyle::Barcode));
    lines.push(mk("", "", LineStyle::Blank));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{ItemResult, ScanSummary};
    use crate::model::sample;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }
    fn ch(c: char) -> KeyEvent {
        key(KeyCode::Char(c))
    }

    /// A finished scan with a few rows.
    fn ready(items: Vec<ScanItem>) -> State {
        let mut st = State::new(PathBuf::from("/Users/test"));
        st.anim.splash_over = true;
        for item in items {
            st.apply_scan(ScanEvent::Found(Box::new(item)));
        }
        st.apply_scan(ScanEvent::Trash(TrashState { readable: true, entries: Vec::new(), bytes: 0, note: None }));
        st.apply_scan(ScanEvent::Finished(ScanSummary::default()));
        st
    }

    fn three() -> Vec<ScanItem> {
        vec![
            sample(1, Kind::Developer, 9_000_000_000, true),
            sample(2, Kind::Caches, 1_000_000_000, true),
            sample(3, Kind::Xcode, 500_000_000, false),
        ]
    }

    #[test]
    fn rows_arrive_chosen_by_their_own_recommendation() {
        let st = ready(three());
        assert_eq!(st.phase, Phase::Ready);
        assert_eq!(st.chosen_count(), 2);
        assert_eq!(st.chosen_bytes(), 10_000_000_000);
        assert_eq!(st.found_bytes(), 10_500_000_000);
    }

    #[test]
    fn begone_with_nothing_chosen_opens_no_dialog_and_says_why() {
        let mut st = ready(three());
        st.choose_none();
        assert!(matches!(st.handle_key(ch('d')), Effect::None));
        assert!(st.dialog.is_none());
        assert!(st.notice.as_ref().unwrap().text.contains("Nothing is chosen"));
    }

    #[test]
    fn begone_waits_for_the_scan() {
        let mut st = State::new(PathBuf::from("/Users/test"));
        st.anim.splash_over = true;
        st.apply_scan(ScanEvent::Found(Box::new(sample(1, Kind::Caches, 5, true))));
        st.handle_key(ch('d'));
        assert!(st.dialog.is_none());
        assert!(st.notice.as_ref().unwrap().text.contains("Still looking"));
    }

    #[test]
    fn confirming_hands_the_chosen_rows_over_in_panel_order() {
        let mut st = ready(three());
        assert!(matches!(st.handle_key(ch('d')), Effect::None));
        let Some(Dialog::ConfirmMove(view)) = st.dialog.clone() else { panic!("a dialog should open") };
        assert_eq!((view.count, view.bytes), (2, 10_000_000_000));
        let Effect::StartMove(items) = st.handle_key(ch('y')) else { panic!("y confirms") };
        assert_eq!(items.iter().map(|i| i.id).collect::<Vec<_>>(), [1, 2], "Developer before Caches");
        assert_eq!(st.phase, Phase::Reclaiming);
        assert!(st.dialog.is_none());
    }

    #[test]
    fn anything_but_y_cancels_the_dialog_and_enter_is_not_a_yes() {
        let mut st = ready(three());
        st.handle_key(ch('d'));
        st.handle_key(key(KeyCode::Enter));
        assert!(st.dialog.is_some(), "enter must not confirm, and must not dismiss by reflex either");
        assert!(matches!(st.handle_key(ch('n')), Effect::None));
        assert!(st.dialog.is_none() && st.phase == Phase::Ready);
    }

    #[test]
    fn the_cursor_stays_on_its_row_while_bigger_rows_stream_in() {
        // Regression: the cursor tracked a position, so a larger arrival made space toggle the wrong item.
        let mut st = ready(vec![sample(1, Kind::Caches, 10, false)]);
        st.tab = Tab::Review;
        st.fix_cursors();
        assert_eq!(st.review.cursor, Some(RowKey::Item(1)));
        st.apply_scan(ScanEvent::Found(Box::new(sample(2, Kind::Caches, 9_000, false))));
        assert_eq!(st.review.cursor, Some(RowKey::Item(1)));
        st.handle_key(ch(' '));
        assert!(st.chosen.contains(&1) && !st.chosen.contains(&2));
    }

    #[test]
    fn f_shows_the_selected_review_row_in_finder() {
        let mut st = ready(vec![sample(1, Kind::Caches, 10, false)]);
        st.tab = Tab::Review;
        st.fix_cursors();
        assert_eq!(st.review.cursor, Some(RowKey::Item(1)));
        let Effect::Reveal(p) = st.handle_key(ch('f')) else { panic!("f must reveal the selected item") };
        assert_eq!(p, PathBuf::from("/Users/test/Library/Caches/item-1"));
        // On a kind header, not an item, f does nothing rash.
        st.review.cursor = Some(RowKey::More(Kind::Caches));
        assert!(matches!(st.handle_key(ch('f')), Effect::None));
    }

    #[test]
    fn s_cycles_the_files_list_through_size_name_and_date_and_keeps_the_cursor_on_its_row() {
        let mut st = ready(Vec::new());
        st.tab = Tab::Files;
        let mk = |id: u64, title: &str, size: u64, days_old: u64| {
            let mut it = sample(id, Kind::Files, size, false);
            it.title = title.to_string();
            it.modified = Some(std::time::SystemTime::now() - std::time::Duration::from_secs(days_old * 86_400));
            it
        };
        st.apply_files(FilesEvent::Found(Box::new(mk(1, "banana", 100, 10))));
        st.apply_files(FilesEvent::Found(Box::new(mk(2, "apple", 300, 2))));
        st.apply_files(FilesEvent::Found(Box::new(mk(3, "cherry", 200, 30))));
        let order = |st: &State| st.files.list.iter().map(|i| i.title.clone()).collect::<Vec<_>>();
        // Default is biggest first.
        assert_eq!(st.sort, SortBy::Size);
        assert_eq!(order(&st), ["apple", "cherry", "banana"]);
        // Put the cursor on "cherry" (the 200 MB row), then prove it follows the row through every order.
        st.files.cursor = 1;
        st.files.touched = true;
        st.handle_key(ch('s'));
        assert_eq!(st.sort, SortBy::Name);
        assert_eq!(order(&st), ["apple", "banana", "cherry"]);
        assert_eq!(st.files_selected().unwrap().title, "cherry");
        st.handle_key(ch('s'));
        assert_eq!(st.sort, SortBy::Date);
        assert_eq!(order(&st), ["cherry", "banana", "apple"], "oldest first");
        assert_eq!(st.files_selected().unwrap().title, "cherry");
        // A new row streaming in while sorted by name lands in the right place, not at the end.
        st.handle_key(ch('s'));
        assert_eq!(st.sort, SortBy::Size);
        st.handle_key(ch('s'));
        assert_eq!(st.sort, SortBy::Name);
        st.apply_files(FilesEvent::Found(Box::new(mk(4, "blueberry", 50, 1))));
        assert_eq!(order(&st), ["apple", "banana", "blueberry", "cherry"]);
    }

    #[test]
    fn a_chooses_every_duplicate_at_once_and_a_second_press_clears_them() {
        let mut st = ready(Vec::new());
        st.tab = Tab::Files;
        let dup = |id: u64, title: &str| {
            let mut i = sample(id, Kind::Files, 1000, false);
            i.title = title.to_string();
            i.grant = crate::safety::Grant::DuplicateCopy;
            i.owner = Some("/keep/original".to_string());
            i
        };
        let mut big = sample(9, Kind::Files, 5000, false);
        big.grant = crate::safety::Grant::BigDownload;
        st.apply_files(FilesEvent::Found(Box::new(big)));
        st.apply_files(FilesEvent::Found(Box::new(dup(1, "copy-a"))));
        st.apply_files(FilesEvent::Found(Box::new(dup(2, "copy-b"))));
        // a chooses both duplicates and not the big download.
        st.handle_key(ch('a'));
        assert_eq!(st.files.chosen, HashSet::from([1, 2]));
        // a again clears them.
        st.handle_key(ch('a'));
        assert!(st.files.chosen.is_empty());
        // A duplicate the person asked to keep is never chosen by the bulk action.
        st.files.list.iter_mut().find(|i| i.id == 1).unwrap().kept = true;
        st.handle_key(ch('a'));
        assert_eq!(st.files.chosen, HashSet::from([2]));
    }

    #[test]
    fn space_on_a_kind_toggles_what_is_recommended_and_nothing_else() {
        let mut st = ready(vec![sample(1, Kind::Caches, 10, true), sample(2, Kind::Caches, 5, false)]);
        st.kind_cursor = PANEL.iter().position(|k| *k == Kind::Caches).unwrap();
        st.handle_key(ch(' '));
        assert!(st.chosen.is_empty(), "already recommended, so this clears");
        st.handle_key(ch(' '));
        assert_eq!(st.chosen, HashSet::from([1]), "the unrecommended row stays unchosen");
    }

    #[test]
    fn the_trash_row_explains_instead_of_toggling() {
        let mut st = ready(three());
        st.kind_cursor = PANEL.iter().position(|k| *k == Kind::Trash).unwrap();
        st.handle_key(ch(' '));
        assert!(st.notice.as_ref().unwrap().text.contains("cannot be undone"));
    }

    #[test]
    fn many_rows_collapse_into_a_group_that_toggles_together() {
        let items: Vec<ScanItem> = (1..=20).map(|i| sample(i, Kind::Logs, 1_000 - i, true)).collect();
        let mut st = ready(items);
        let rows = st.review_rows();
        assert_eq!(rows.len(), 1 + MAX_SHOWN + 1, "a header, the biggest dozen, and one group row");
        let ReviewRow::More { count, chosen, .. } = rows.last().unwrap().clone() else { panic!("last is the group") };
        assert_eq!((count, chosen), (8, 8));
        st.tab = Tab::Review;
        st.review.cursor = Some(RowKey::More(Kind::Logs));
        st.handle_key(ch(' '));
        assert_eq!(st.chosen.len(), MAX_SHOWN, "the group toggled only the hidden eight");
    }

    #[test]
    fn a_finished_run_removes_what_moved_and_unchooses_what_was_kept() {
        let mut st = ready(three());
        st.handle_key(ch('d'));
        let Effect::StartMove(items) = st.handle_key(ch('y')) else { panic!() };
        let mut report = Report::default();
        for (n, item) in items.iter().enumerate() {
            let outcome = if n == 0 { Outcome::Moved { to: None } } else { Outcome::Refused("changed".into()) };
            report.results.push(ItemResult { id: item.id, title: item.title.clone(), kind: item.kind, bytes: item.size, outcome });
        }
        st.apply_run(RunEvent::Finished(report));
        assert!(st.items.iter().all(|i| i.id != 1), "moved rows are gone");
        assert!(st.items.iter().any(|i| i.id == 2) && !st.chosen.contains(&2), "kept rows stay, unchosen");
        assert_eq!(st.moved_to_trash, 9_000_000_000);
        assert!(st.receipt.iter().any(|l| l.left.contains("Left alone")));
        assert!(st.receipt.iter().any(|l| l.style == LineStyle::Accent && l.right == "9.0 GB"));
    }

    #[test]
    fn the_motion_takes_at_least_its_minimum_even_when_the_work_is_instant() {
        let mut st = ready(three());
        st.handle_key(ch('d'));
        let Effect::StartMove(items) = st.handle_key(ch('y')) else { panic!() };
        st.apply_run(RunEvent::Started { items: items.len(), bytes: 0 });
        let mut report = Report::default();
        for item in &items {
            report.results.push(ItemResult {
                id: item.id,
                title: item.title.clone(),
                kind: item.kind,
                bytes: item.size,
                outcome: Outcome::Moved { to: None },
            });
        }
        st.apply_run(RunEvent::Finished(report));
        st.tick(0.5);
        assert_eq!(st.phase, Phase::Reclaiming, "half a second in, the receipt must not have appeared");
        for _ in 0..40 {
            st.tick(0.1);
        }
        assert_eq!(st.phase, Phase::Receipt);
    }

    #[test]
    fn a_dialog_survives_the_scan_finishing_underneath_it() {
        // Regression: the dialog and the scan shared one phase, so finishing closed the dialog.
        let mut st = State::new(PathBuf::from("/Users/test"));
        st.anim.splash_over = true;
        st.dialog = Some(Dialog::Help);
        st.apply_scan(ScanEvent::Finished(ScanSummary::default()));
        assert!(matches!(st.dialog, Some(Dialog::Help)));
        assert_eq!(st.phase, Phase::Ready);
    }

    #[test]
    fn quitting_mid_run_stops_safely_and_quits_afterwards() {
        let mut st = ready(three());
        st.handle_key(ch('d'));
        st.handle_key(ch('y'));
        assert!(matches!(st.handle_key(ch('q')), Effect::Stop));
        assert!(st.quit_after);
    }

    #[test]
    fn emptying_needs_a_readable_nonempty_trash_and_a_yes() {
        let mut st = ready(three());
        st.handle_key(ch('e'));
        assert!(st.notice.as_ref().unwrap().text.contains("already empty"));
        st.trash = Some(TrashState { readable: false, entries: vec![], bytes: 0, note: Some("needs Full Disk Access".into()) });
        st.handle_key(ch('e'));
        assert!(st.notice.as_ref().unwrap().warn && st.dialog.is_none());
        st.trash = Some(TrashState {
            readable: true,
            entries: vec![crate::scan::trash::TrashEntry { path: "/x".into(), bytes: 5, begone: false }],
            bytes: 5,
            note: None,
        });
        st.handle_key(ch('e'));
        assert!(matches!(st.dialog, Some(Dialog::ConfirmEmpty(_))));
        assert!(matches!(st.handle_key(ch('y')), Effect::StartEmpty(_)));
        assert_eq!(st.phase, Phase::Emptying);
    }

    #[test]
    fn a_rescan_swaps_rows_in_without_emptying_the_screen_first() {
        let mut st = ready(three());
        st.choose_none();
        st.begin_scan(true);
        assert_eq!(st.items.len(), 3, "the old rows stay on screen while the rescan runs");
        st.apply_scan(ScanEvent::Found(Box::new(sample(10, Kind::Developer, 9_000_000_000, true))));
        assert_eq!(st.items.len(), 3);
        st.apply_scan(ScanEvent::Finished(ScanSummary::default()));
        assert_eq!(st.items.len(), 1);
        assert_eq!(st.phase, Phase::Ready);
    }

    #[test]
    fn the_hero_is_the_plan_until_something_moves_and_then_the_trash() {
        let mut st = ready(three());
        assert_eq!(st.hero_target(), 10_000_000_000.0);
        st.moved_to_trash = 123;
        st.recompute();
        st.phase = Phase::Receipt;
        assert_eq!(st.hero_target(), 123.0, "after a run the number is what the Trash holds");
    }

    #[test]
    fn the_window_for_each_kind_tiles_the_whole_motion() {
        let mut plan = [0u64; Kind::COUNT];
        plan[Kind::Developer.index()] = 9_000;
        plan[Kind::Logs.index()] = 1;
        let w = windows(&plan);
        let (dev, logs) = (w[Kind::Developer.index()], w[Kind::Logs.index()]);
        assert!(dev.0 == 0.0 && (dev.1 - logs.0).abs() < 1e-6 && (logs.1 - DEPART_SPAN).abs() < 1e-6);
        assert!(logs.1 - logs.0 >= 0.05, "a tiny kind still gets a moment of its own");
    }

    #[test]
    fn what_begone_just_moved_can_be_emptied_even_when_the_trash_was_empty_at_scan_time() {
        // Regression: the Trash was read once, before the run, so `e` said "already empty" while
        // the screen said "50 MB already in the Trash".
        let mut st = ready(vec![sample(1, Kind::Caches, 50_000_000, true)]);
        assert_eq!(st.trash_items(), 0);
        st.handle_key(ch('d'));
        let Effect::StartMove(items) = st.handle_key(ch('y')) else { panic!() };
        let mut report = Report::default();
        for i in &items {
            report.results.push(ItemResult {
                id: i.id,
                title: i.title.clone(),
                kind: i.kind,
                bytes: i.size,
                outcome: Outcome::Moved { to: None },
            });
        }
        st.apply_run(RunEvent::Finished(report));
        for _ in 0..200 {
            st.tick(0.05);
        }
        st.handle_key(ch(' '));
        assert_eq!((st.trash_items(), st.trash_bytes()), (1, 50_000_000));
        st.handle_key(ch('e'));
        let Some(Dialog::ConfirmEmpty(view)) = st.dialog.clone() else { panic!("the empty dialog must open, not say it is already empty") };
        assert_eq!((view.items, view.bytes, view.from_begone), (1, 50_000_000, 50_000_000));
    }

    #[test]
    fn a_fresh_reading_of_the_trash_replaces_every_estimate() {
        let mut st = ready(three());
        st.moved_to_trash = 999;
        st.moved_items = 7;
        st.apply_trash(TrashState {
            readable: true,
            entries: vec![crate::scan::trash::TrashEntry { path: "/x".into(), bytes: 5, begone: false }],
            bytes: 5,
            note: None,
        });
        assert_eq!((st.trash_bytes(), st.trash_items(), st.moved_to_trash, st.moved_items), (5, 1, 0, 0));
    }

    #[test]
    fn the_file_count_follows_the_rows_still_listed() {
        let st = ready(three());
        assert_eq!(st.files_found(), 9, "three items of three files each");
    }

    // ------------------------------------------------------------------------------ the map

    fn said(st: &State) -> String {
        st.notice.as_ref().map(|n| n.text.clone()).unwrap_or_default()
    }

    fn at_home(item: &mut ScanItem, rest: &str) {
        item.path = PathBuf::from("/Users/test").join(rest);
    }

    /// A finished scan, then the demo map of the test home, with the Map tab showing.
    fn mapped(items: Vec<ScanItem>) -> State {
        let mut st = ready(items);
        let Effect::BuildMap(root) = st.handle_key(ch('m')) else { panic!("m must ask for a map") };
        st.apply_map(MapEvent::Done(Arc::new(crate::map::demo(&root))));
        st
    }

    #[test]
    fn m_opens_the_map_and_asks_for_a_walk_only_when_there_is_none_or_when_asked_again() {
        let mut st = ready(three());
        let Effect::BuildMap(root) = st.handle_key(ch('m')) else { panic!("the first m must ask for a map") };
        assert_eq!(root, PathBuf::from("/Users/test"));
        assert_eq!(st.tab, Tab::Map);
        assert!(st.map.building);
        assert!(matches!(st.handle_key(ch('m')), Effect::None), "pressing m while it maps must not start a second walk");
        st.apply_map(MapEvent::Done(Arc::new(crate::map::demo(&root))));
        assert!(!st.map.building && st.map.tree.is_some());
        st.handle_key(ch('o'));
        assert!(matches!(st.handle_key(ch('m')), Effect::None), "coming back to a good map costs nothing");
        assert!(matches!(st.handle_key(ch('m')), Effect::BuildMap(_)), "m on the map maps again");
    }

    #[test]
    fn tab_walks_overview_review_map_apps_files_and_back_and_asks_for_what_each_tab_needs_on_arrival() {
        let mut st = ready(three());
        assert_eq!(st.tab, Tab::Overview);
        st.handle_key(key(KeyCode::Tab));
        assert_eq!(st.tab, Tab::Review);
        let arrive = st.handle_key(key(KeyCode::Tab));
        assert_eq!(st.tab, Tab::Map);
        assert!(matches!(arrive, Effect::BuildMap(_)), "arriving by Tab starts the map too");
        let arrive = st.handle_key(key(KeyCode::Tab));
        assert_eq!(st.tab, Tab::Apps);
        assert!(matches!(arrive, Effect::ScanApps), "and the list of apps");
        let arrive = st.handle_key(key(KeyCode::Tab));
        assert_eq!(st.tab, Tab::Files);
        assert!(matches!(arrive, Effect::ScanFiles), "and the look for old installers and backups");
        st.handle_key(key(KeyCode::Tab));
        assert_eq!(st.tab, Tab::Overview);
        st.handle_key(key(KeyCode::BackTab));
        assert_eq!(st.tab, Tab::Files);
        st.handle_key(key(KeyCode::BackTab));
        assert_eq!(st.tab, Tab::Apps);
        st.handle_key(key(KeyCode::BackTab));
        assert_eq!(st.tab, Tab::Map);
        st.handle_key(key(KeyCode::BackTab));
        assert_eq!(st.tab, Tab::Review);
    }

    #[test]
    fn a_map_that_fails_can_be_tried_again_and_progress_only_counts_while_it_works() {
        let mut st = ready(three());
        st.handle_key(ch('m'));
        st.apply_map(MapEvent::Progress { files: 10, bytes: 99 });
        assert_eq!(st.map.seen, (10, 99));
        st.apply_map(MapEvent::Failed);
        assert!(!st.map.building && st.map.failed && st.map.tree.is_none());
        assert!(matches!(st.handle_key(ch('m')), Effect::BuildMap(_)), "a failure is not remembered");
        assert!(st.map.building && !st.map.failed && st.map.seen == (0, 0));
    }

    #[test]
    fn a_new_map_starts_at_the_top_of_the_home() {
        let mut st = mapped(Vec::new());
        st.handle_key(key(KeyCode::Enter));
        st.handle_key(ch('j'));
        assert_eq!(st.map.path, [0]);
        st.handle_key(ch('m'));
        st.apply_map(MapEvent::Done(Arc::new(crate::map::demo(&st.home))));
        assert!(st.map.path.is_empty());
        assert_eq!((st.map.cursor.clone(), st.map_cursor()), (vec![0], 0));
    }

    #[test]
    fn enter_opens_a_folder_backspace_goes_up_and_esc_leaves_from_the_top() {
        let mut st = mapped(Vec::new());
        st.handle_key(key(KeyCode::Enter));
        assert_eq!(st.map_here().map(|n| n.name.as_str()), Some("Library"));
        st.handle_key(key(KeyCode::Enter));
        assert_eq!((st.map.path.clone(), st.map_here().map(|n| n.name.clone())), (vec![0, 0], Some("Caches".to_string())));
        st.handle_key(key(KeyCode::Backspace));
        assert_eq!(st.map.path, [0]);
        st.handle_key(key(KeyCode::Left));
        assert!(st.map.path.is_empty());
        st.handle_key(key(KeyCode::Backspace));
        assert_eq!(st.tab, Tab::Map, "backspace at the top stays put");
        st.handle_key(key(KeyCode::Right));
        st.handle_key(key(KeyCode::Esc));
        assert!(st.map.path.is_empty() && st.tab == Tab::Map, "esc goes up a level first");
        st.handle_key(key(KeyCode::Esc));
        assert_eq!(st.tab, Tab::Overview, "esc at the top leaves the map");
    }

    #[test]
    fn each_level_remembers_where_the_cursor_was() {
        let mut st = mapped(Vec::new());
        st.handle_key(ch('j'));
        st.handle_key(key(KeyCode::Enter));
        assert_eq!(st.map_cursor(), 0, "a folder opens on its biggest row");
        st.handle_key(key(KeyCode::Backspace));
        assert_eq!(st.map_cursor(), 1, "and going back lands on the row you came from");
    }

    #[test]
    fn the_cursor_stays_inside_the_list_whatever_is_pressed() {
        let mut st = mapped(Vec::new());
        let last = st.map_rows() - 1;
        assert_eq!(last, 3, "Library, code, movie.mov and the folded entry");
        for _ in 0..50 {
            st.handle_key(ch('j'));
        }
        assert_eq!(st.map_cursor(), last);
        for _ in 0..50 {
            st.handle_key(key(KeyCode::Up));
        }
        assert_eq!(st.map_cursor(), 0);
        st.handle_key(ch('G'));
        assert_eq!(st.map_cursor(), last);
        st.handle_key(ch('g'));
        assert_eq!(st.map_cursor(), 0);
        st.handle_key(key(KeyCode::PageDown));
        assert_eq!(st.map_cursor(), last);
        st.handle_key(key(KeyCode::PageUp));
        assert_eq!(st.map_cursor(), 0);
    }

    #[test]
    fn opening_a_file_a_folded_entry_or_an_empty_folder_says_why_and_goes_nowhere() {
        let mut st = mapped(Vec::new());
        st.handle_key(ch('j'));
        st.handle_key(ch('j'));
        st.handle_key(key(KeyCode::Enter));
        assert!(st.map.path.is_empty() && said(&st).contains("file"), "{}", said(&st));
        st.handle_key(ch('j'));
        st.handle_key(key(KeyCode::Enter));
        assert!(st.map.path.is_empty() && said(&st).contains("folded"), "{}", said(&st));
        // A folder with nothing listed in it: take the demo's `code` and empty it.
        let mut tree = crate::map::demo(&st.home);
        tree.node.children[1].children.clear();
        st.apply_map(MapEvent::Done(Arc::new(tree)));
        st.handle_key(ch('j'));
        st.handle_key(key(KeyCode::Enter));
        assert!(st.map.path.is_empty() && said(&st).contains("Nothing to open"), "{}", said(&st));
    }

    #[test]
    fn f_shows_the_selection_in_finder_and_a_folded_entry_shows_the_folder_that_holds_it() {
        let mut st = mapped(Vec::new());
        let Effect::Reveal(p) = st.handle_key(ch('f')) else { panic!("f must reveal") };
        assert_eq!(p, PathBuf::from("/Users/test/Library"));
        st.handle_key(key(KeyCode::Enter));
        st.handle_key(key(KeyCode::Enter));
        st.handle_key(ch('G'));
        let Effect::Reveal(p) = st.handle_key(ch('f')) else { panic!("f must reveal") };
        assert_eq!(p, PathBuf::from("/Users/test/Library/Caches"), "the folded entry has no place of its own");
    }

    #[test]
    fn space_chooses_only_what_begone_found_at_exactly_that_place() {
        let mut item = sample(1, Kind::Caches, 15_000_000_000, true);
        at_home(&mut item, "Library/Caches/com.test.app");
        let mut st = mapped(vec![item]);
        assert_eq!(st.chosen.len(), 1);
        // Library holds the item but is not it: space does nothing to the choice and says so.
        st.handle_key(ch(' '));
        assert_eq!(st.chosen.len(), 1);
        assert!(said(&st).contains("Part of this can go"), "{}", said(&st));
        st.handle_key(key(KeyCode::Enter));
        st.handle_key(key(KeyCode::Enter));
        // big.cache is just a file: begone found nothing there.
        st.handle_key(ch(' '));
        assert_eq!(st.chosen.len(), 1);
        assert!(said(&st).contains("only moves what it can prove is safe"), "{}", said(&st));
        // com.test.app is the item itself.
        st.handle_key(ch('j'));
        st.handle_key(ch(' '));
        assert!(st.chosen.is_empty(), "space clears what is chosen");
        st.handle_key(ch(' '));
        assert!(st.chosen.contains(&1), "and chooses it again");
        // The folded entry is nothing begone can touch.
        st.handle_key(ch('G'));
        st.handle_key(ch(' '));
        assert_eq!(st.chosen.len(), 1);
        assert!(said(&st).contains("Nothing here for begone to choose"), "{}", said(&st));
    }

    #[test]
    fn what_can_go_is_counted_under_a_folder_and_not_beside_it() {
        let mut a = sample(1, Kind::Caches, 1_000_000_000, true);
        at_home(&mut a, "Library/Caches/a");
        let mut b = sample(2, Kind::Caches, 2_000_000_000, false);
        at_home(&mut b, "Library/Caches-old/b");
        let mut c = sample(3, Kind::Developer, 4_000_000_000, true);
        at_home(&mut c, "code/target");
        let st = ready(vec![a, b, c]);
        let under = |p: &str| st.junk_under(&PathBuf::from("/Users/test").join(p));
        assert_eq!(under("Library/Caches"), (1_000_000_000, 1_000_000_000), "Caches-old is a neighbour, not inside");
        assert_eq!(under("Library"), (3_000_000_000, 1_000_000_000), "b is found but not chosen");
        assert_eq!(under("code/target"), (4_000_000_000, 4_000_000_000), "an item counts for itself");
        assert_eq!(under("code/target/deeper"), (0, 0), "and nothing below it is a separate find");
        assert_eq!(under(""), (7_000_000_000, 5_000_000_000));
    }

    #[test]
    fn the_rest_of_the_disk_is_only_ever_shown_for_the_real_home() {
        let mut st = mapped(Vec::new());
        st.disk.total = 994_000_000_000;
        st.disk.free = 252_000_000_000;
        assert!(st.map_rest().is_none(), "a sandbox's disk numbers say nothing about a sandbox");
        assert_eq!(st.map_rows(), 4);
        st.home = crate::safety::home_dir();
        assert_eq!(st.map_rest(), Some(994_000_000_000 - 252_000_000_000 - 100_000_000_000));
        assert_eq!(st.map_rows(), 5);
        st.handle_key(ch('G'));
        st.handle_key(key(KeyCode::Enter));
        assert!(said(&st).contains("rest of the disk"), "{}", said(&st));
        let Effect::None = st.handle_key(ch('f')) else { panic!("there is nothing to reveal for the rest of the disk") };
        st.handle_key(ch('g'));
        st.handle_key(key(KeyCode::Enter));
        assert!(st.map_rest().is_none(), "inside a folder it is not shown");
    }

    #[test]
    fn a_finished_run_makes_the_map_stale_so_the_next_visit_maps_again() {
        let mut st = mapped(three());
        assert!(!st.map.stale);
        st.handle_key(ch('d'));
        let Effect::StartMove(items) = st.handle_key(ch('y')) else { panic!("expected a move") };
        let mut report = Report::default();
        for i in &items {
            report.results.push(ItemResult {
                id: i.id,
                title: i.title.clone(),
                kind: i.kind,
                bytes: i.size,
                outcome: Outcome::Moved { to: None },
            });
        }
        st.apply_run(RunEvent::Finished(report));
        assert!(st.map.stale, "what is on disk changed, so the map is out of date");
        assert_eq!(st.tab, Tab::Overview, "a run plays out on the Overview, so the map is not on screen while it is out of date");
        st.phase = Phase::Ready;
        assert!(matches!(st.handle_key(ch('m')), Effect::BuildMap(_)), "the next visit maps again");
        assert!(st.map.building);
    }

    #[test]
    fn no_run_of_map_keys_can_break_the_map_or_ask_for_anything_but_reading() {
        let keys = [
            KeyCode::Down,
            KeyCode::Up,
            KeyCode::Enter,
            KeyCode::Backspace,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::PageDown,
            KeyCode::PageUp,
            KeyCode::Home,
            KeyCode::End,
            KeyCode::Esc,
            KeyCode::Char('j'),
            KeyCode::Char('k'),
            KeyCode::Char('g'),
            KeyCode::Char('G'),
            KeyCode::Char('h'),
            KeyCode::Char('l'),
            KeyCode::Char('f'),
            KeyCode::Char(' '),
        ];
        let mut item = sample(1, Kind::Caches, 15_000_000_000, true);
        at_home(&mut item, "Library/Caches/com.test.app");
        let mut st = mapped(vec![item]);
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        for step in 0..6000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let code = keys[(seed % keys.len() as u64) as usize];
            match st.handle_key(key(code)) {
                Effect::None => {}
                Effect::Reveal(p) => assert!(p.starts_with("/Users/test"), "step {step}: revealed {p:?}"),
                other => panic!("step {step}: {code:?} asked for {other:?}"),
            }
            // Esc at the top leaves the map: go back to it, as a person would.
            st.tab = Tab::Map;
            assert!(st.map_here().is_some(), "step {step}: the path must always lead to a folder");
            assert!(st.map_cursor() < st.map_rows().max(1), "step {step}: the cursor is off the list");
            assert_eq!(st.map.cursor.len(), st.map.path.len() + 1, "step {step}: one cursor per level");
            assert!(st.dialog.is_none(), "step {step}: a map key opened a dialog");
            assert!(st.chosen.iter().all(|id| st.items.iter().any(|i| i.id == *id)), "step {step}: chose something that is not found");
        }
    }

    #[test]
    fn the_receipt_waits_for_the_motion_then_prints_over_a_second_or_so_and_the_screen_rests() {
        let mut st = ready(three());
        st.handle_key(ch('d'));
        let Effect::StartMove(items) = st.handle_key(ch('y')) else { panic!("expected a move") };
        let mut report = Report::default();
        for i in &items {
            report.results.push(ItemResult {
                id: i.id,
                title: i.title.clone(),
                kind: i.kind,
                bytes: i.size,
                outcome: Outcome::Moved { to: None },
            });
        }
        st.apply_run(RunEvent::Finished(report));
        // A value left over from an earlier receipt must not matter, and nothing prints while the motion plays.
        st.anim.receipt = 1.0;
        let mut t = 0.0;
        while st.phase == Phase::Reclaiming {
            assert_eq!(st.anim.receipt, 1.0, "the clock only runs once the receipt is on screen");
            st.tick(0.05);
            t += 0.05;
            assert!(t < 10.0, "the motion never ended");
        }
        assert_eq!(st.phase, Phase::Receipt);
        assert_eq!(st.anim.receipt, 0.0, "printing starts when the motion ends, not before");
        let mut seen = Vec::new();
        for _ in 0..40 {
            let tick = st.tick(0.05);
            seen.push((st.anim.receipt, tick.moving));
        }
        assert!(seen.windows(2).all(|p| p[0].0 <= p[1].0), "it only ever prints more");
        assert!((seen[12].0 - 0.05 * 13.0 / PRINT_SECS).abs() < 1e-3, "half a second in it is about half done: {:?}", seen[12]);
        assert!(seen[..20].iter().all(|s| s.1), "the screen keeps moving while the paper prints");
        assert_eq!(seen[39].0, 1.0, "after {} s it is all out", 40.0 * 0.05);
        // Then everything settles and the screen costs nothing.
        let mut last = true;
        for _ in 0..200 {
            last = st.tick(0.05).moving;
        }
        assert!(!last, "a finished receipt on a resting screen is not an animation");
    }

    #[test]
    fn the_number_keys_jump_straight_to_a_tab_from_any_tab_and_the_letters_still_do() {
        for (digit, letter, tab) in [('1', 'o', Tab::Overview), ('2', 'r', Tab::Review), ('3', 'm', Tab::Map)] {
            for from in [Tab::Overview, Tab::Review, Tab::Map] {
                let mut st = ready(three());
                st.tab = from;
                st.handle_key(ch(digit));
                assert_eq!(st.tab, tab, "{digit} from {from:?}");
                st.tab = from;
                st.handle_key(ch(letter));
                assert_eq!(st.tab, tab, "{letter} from {from:?}");
            }
        }
    }

    #[test]
    fn three_asks_for_the_map_only_when_it_needs_one_and_does_not_map_again_on_the_map() {
        let mut st = ready(three());
        let Effect::BuildMap(root) = st.handle_key(ch('3')) else { panic!("the first 3 must ask for a map") };
        assert!(matches!(st.handle_key(ch('3')), Effect::None), "3 while it maps asks for nothing more");
        st.apply_map(MapEvent::Done(Arc::new(crate::map::demo(&root))));
        st.handle_key(ch('1'));
        assert!(matches!(st.handle_key(ch('3')), Effect::None), "a good map is shown as it is");
        assert!(matches!(st.handle_key(ch('3')), Effect::None), "3 on the Map does not map again (m does)");
        assert!(matches!(st.handle_key(ch('m')), Effect::BuildMap(_)));
        st.apply_map(MapEvent::Done(Arc::new(crate::map::demo(&root))));
        st.map.stale = true;
        st.handle_key(ch('2'));
        assert!(matches!(st.handle_key(ch('3')), Effect::BuildMap(_)), "a map gone stale is made again when you come back to it");
    }

    #[test]
    fn the_number_keys_wait_while_a_dialog_is_open_or_a_run_is_busy_and_close_a_receipt_like_the_letters() {
        let mut st = ready(three());
        st.handle_key(ch('d'));
        assert!(matches!(st.dialog, Some(Dialog::ConfirmMove(_))));
        for digit in ['1', '2', '3'] {
            st.handle_key(ch(digit));
            assert!(st.tab == Tab::Overview && st.dialog.is_some(), "{digit} must not act under a dialog");
        }
        let Effect::StartMove(items) = st.handle_key(ch('y')) else { panic!("expected a move") };
        assert!(matches!(st.handle_key(ch('3')), Effect::None));
        assert_eq!(st.tab, Tab::Overview, "while the run plays, the screen stays where it is");
        let mut report = Report::default();
        for i in &items {
            report.results.push(ItemResult {
                id: i.id,
                title: i.title.clone(),
                kind: i.kind,
                bytes: i.size,
                outcome: Outcome::Moved { to: None },
            });
        }
        st.apply_run(RunEvent::Finished(report));
        for _ in 0..200 {
            st.tick(0.05);
        }
        assert_eq!(st.phase, Phase::Receipt);
        st.handle_key(ch('2'));
        assert_eq!((st.tab, st.phase), (Tab::Review, Phase::Ready), "a number key puts the receipt away, as the letters do");
    }

    /// A finished scan holding one chosen cache of this size, with the big number settled on it.
    fn one_cache(bytes: u64) -> State {
        let mut st = ready(vec![sample(1, Kind::Caches, bytes, true)]);
        for _ in 0..400 {
            st.tick(0.05);
        }
        assert_eq!(st.anim.hero, bytes as f32);
        st
    }

    /// Tick at 60 frames a second until the screen would rest. Every frame is checked: a tick that changes the big
    /// number or the bars must say it is moving, because the loop only draws a frame when it does.
    fn run_frames(st: &mut State, most: usize) -> Vec<(f32, (String, &'static str))> {
        let mut shown = Vec::new();
        for _ in 0..most {
            let (hero, arcs) = (st.anim.hero, st.anim.arc);
            let tick = st.tick(1.0 / 60.0);
            if st.anim.hero != hero || st.anim.arc != arcs {
                assert!(tick.moving, "this tick changed the screen but said it was not moving, so it would not be drawn");
            }
            shown.push((st.anim.hero, st.hero_parts()));
            if !tick.moving {
                break;
            }
        }
        shown
    }

    #[test]
    fn the_big_number_counts_down_in_the_unit_it_started_in_and_ends_on_zero() {
        let mut st = one_cache(842_000_000);
        st.handle_key(ch('n'));
        let shown = run_frames(&mut st, 600);
        let landed = shown.iter().position(|(hero, _)| *hero == 0.0).expect("it lands on zero");
        assert!(landed > 20, "it counts, it does not jump: landed after {landed} frames");
        assert!(
            shown[..landed].iter().all(|(_, (_, unit))| *unit == "MB"),
            "it must stay in MB all the way down: {:?}",
            shown[..landed].iter().map(|s| &s.1).collect::<Vec<_>>()
        );
        assert!(
            shown[landed..].iter().all(|(hero, parts)| *hero == 0.0 && *parts == ("0".to_string(), "B")),
            "and once there it stays there"
        );
        let digits: Vec<u64> = shown.iter().map(|(_, (n, _))| n.parse().unwrap()).collect();
        assert!(digits[0] <= 842 && digits.windows(2).all(|p| p[0] >= p[1]), "it only ever goes down: {digits:?}");
        assert!(landed < 120, "it lands within two seconds, not after a long tail: {landed} frames");
        assert!(shown.len() <= landed + 10, "and the screen rests soon after: {} frames in all", shown.len());
        assert_eq!(st.anim.hero_scale, 0.0, "the unit lets go once it has arrived");
    }

    #[test]
    fn the_big_number_counts_up_in_the_unit_it_is_going_to_and_lands_on_the_exact_figure() {
        let mut st = one_cache(842_129_216);
        st.handle_key(ch('n'));
        run_frames(&mut st, 600);
        assert_eq!(st.hero_parts(), ("0".to_string(), "B"));
        st.handle_key(ch('a'));
        assert_eq!(st.hero_parts(), ("0".to_string(), "MB"), "before the first tick it already speaks the unit it will count in");
        let shown = run_frames(&mut st, 600);
        assert!(
            shown.iter().all(|(_, (_, unit))| *unit == "MB"),
            "it counts up in MB from the first frame: {:?}",
            shown.iter().take(4).map(|s| &s.1).collect::<Vec<_>>()
        );
        let digits: Vec<u64> = shown.iter().map(|(_, (n, _))| n.parse().unwrap()).collect();
        assert!(digits.windows(2).all(|p| p[0] <= p[1]), "it only ever goes up: {digits:?}");
        assert_eq!(shown.last().unwrap().1, ("842".to_string(), "MB"), "the last frame says 842, not the 841 it was heading for");
        assert_eq!(st.anim.hero, 842_129_216.0);
        assert_eq!(st.anim.hero_scale, 842_129_216.0);
    }

    #[test]
    fn the_unit_follows_the_biggest_value_of_a_move_and_lets_go_on_landing() {
        // 11 GB down to a 3 MB cache that stays chosen: the count is in GB until it lands, then in the figure's own unit.
        let mut st = ready(vec![sample(1, Kind::Developer, 11_000_000_000, true), sample(2, Kind::Caches, 3_000_000, false)]);
        for _ in 0..400 {
            st.tick(0.05);
        }
        assert_eq!(st.hero_parts(), ("11.0".to_string(), "GB"));
        st.handle_key(ch('n'));
        st.chosen.insert(2);
        st.recompute();
        let shown = run_frames(&mut st, 600);
        let landed = shown.iter().position(|(hero, _)| *hero == 3_000_000.0).expect("it lands on 3 MB");
        assert!(
            shown[..landed].iter().all(|(_, (_, unit))| *unit == "GB"),
            "{:?}",
            shown[..landed].iter().map(|s| &s.1).collect::<Vec<_>>()
        );
        assert!(shown[landed..].iter().all(|(_, parts)| *parts == ("3".to_string(), "MB")), "it lands in its own unit");
        assert_eq!(st.hero_parts(), ("3".to_string(), "MB"));
    }

    #[test]
    fn a_resting_screen_stays_resting_and_a_scan_still_streams_the_number_up() {
        let mut st = one_cache(500_000_000);
        for _ in 0..30 {
            assert!(!st.tick(0.05).moving, "nothing is changing, so nothing is drawn");
        }
        // While rows stream in, the number keeps up with them.
        let mut st = State::new(PathBuf::from("/Users/test"));
        st.anim.splash_over = true;
        let mut last = 0.0;
        for n in 1..=20u64 {
            st.apply_scan(ScanEvent::Found(Box::new(sample(n, Kind::Caches, 100_000_000 * n, true))));
            for _ in 0..6 {
                let tick = st.tick(0.016);
                assert!(tick.moving);
            }
            assert!(st.anim.hero >= last, "the number only counts up while the scan finds more");
            last = st.anim.hero;
        }
        assert!(last > 5_000_000_000.0, "{last}");
    }

    fn with_trash(st: &mut State, sizes: &[u64]) {
        let entries: Vec<_> = sizes
            .iter()
            .enumerate()
            .map(|(i, b)| crate::scan::trash::TrashEntry { path: format!("/t/{i}").into(), bytes: *b, begone: false })
            .collect();
        st.apply_trash(TrashState { readable: true, bytes: sizes.iter().sum(), entries, note: None });
    }

    fn all_moved(items: &[ScanItem]) -> Report {
        let mut report = Report::default();
        for i in items {
            report.results.push(ItemResult {
                id: i.id,
                title: i.title.clone(),
                kind: i.kind,
                bytes: i.size,
                outcome: Outcome::Moved { to: None },
            });
        }
        report
    }

    #[test]
    fn a_move_starts_with_a_glide_from_the_plan_to_what_the_trash_holds_and_then_counts_up_without_a_wobble() {
        let mut st = ready(three());
        with_trash(&mut st, &[432_000_000]);
        for _ in 0..400 {
            st.tick(0.05);
        }
        let plan = 10_000_000_000.0f32;
        assert_eq!(st.anim.hero, plan);
        st.handle_key(ch('d'));
        let Effect::StartMove(items) = st.handle_key(ch('y')) else { panic!("expected a move") };
        st.apply_run(RunEvent::Finished(all_moved(&items)));
        let mut shown = vec![plan];
        while st.phase == Phase::Reclaiming {
            st.tick(1.0 / 60.0);
            shown.push(st.anim.hero);
            assert!(shown.len() < 600, "the move never ended");
        }
        // No frame is a cut: the biggest step is a fraction of the gap, where it used to be all of it.
        let biggest = shown.windows(2).map(|p| (p[1] - p[0]).abs()).fold(0.0, f32::max);
        assert!(biggest < 0.25 * plan, "the number jumped by {biggest} in one frame");
        assert!(shown[1] > 0.5 * plan, "the first frame is still near the plan, not already down at the Trash: {}", shown[1]);
        // It reaches the Trash's own figure, and from there only ever rises.
        let low = shown.iter().copied().fold(f32::MAX, f32::min);
        assert!(low < 900_000_000.0, "it came down to about what the Trash held (432 MB): {low}");
        let at = shown.iter().position(|v| *v == low).unwrap();
        assert!(
            shown[at..].windows(2).all(|p| p[1] >= p[0] - 1.0),
            "after the low it only counts up: {:?}",
            &shown[at..at.saturating_add(8).min(shown.len())]
        );
        assert!(shown.iter().all(|v| *v <= 10_432_000_000.0 * 1.005), "and it never overshoots the final Trash");
        // In the receipt it lands exactly on what the Trash holds, in GB the whole way.
        for _ in 0..200 {
            st.tick(0.05);
        }
        assert_eq!(st.phase, Phase::Receipt);
        assert_eq!(st.anim.hero, 10_432_000_000.0);
        assert_eq!(st.hero_parts(), ("10.4".to_string(), "GB"));
    }

    #[test]
    fn an_undo_starts_with_a_glide_up_to_the_trash_instead_of_a_cut() {
        use crate::journal::{Restorable, UndoPlan};
        let mut st = ready(three());
        st.choose_none();
        for _ in 0..400 {
            st.tick(0.05);
        }
        assert_eq!(st.anim.hero, 0.0);
        with_trash(&mut st, &[5_000_000_000, 6_000_000_000]);
        st.begin_undo(&UndoPlan {
            run: 1,
            at: 0,
            restorable: vec![
                Restorable { from: "/t/a".into(), to: "/h/a".into(), bytes: 5_000_000_000, kind: Kind::Developer },
                Restorable { from: "/t/b".into(), to: "/h/b".into(), bytes: 6_000_000_000, kind: Kind::Caches },
            ],
            unresolved: 0,
            strict: false,
        });
        let target = st.hero_target();
        assert!(target > 5_000_000_000.0, "{target}");
        let mut shown = vec![0.0];
        for _ in 0..60 {
            st.tick(1.0 / 60.0);
            shown.push(st.anim.hero);
        }
        let biggest = shown.windows(2).map(|p| (p[1] - p[0]).abs()).fold(0.0, f32::max);
        assert!(biggest < 0.25 * target, "the number jumped by {biggest} in one frame");
        assert!(shown.windows(2).all(|p| p[1] >= p[0]), "and it only rises to the Trash's figure");
    }

    #[test]
    fn emptying_starts_at_what_is_about_to_go_and_counts_the_trash_down_as_each_entry_goes() {
        let mut st = ready(three());
        with_trash(&mut st, &[6_000_000_000, 3_000_000_000, 2_000_000_000]);
        st.choose_none();
        for _ in 0..400 {
            st.tick(0.05);
        }
        assert_eq!(st.hero_parts(), ("0".to_string(), "B"), "nothing is chosen, so it reads 0 before");
        st.handle_key(ch('e'));
        assert!(matches!(st.handle_key(ch('y')), Effect::StartEmpty(_)));
        assert_eq!(st.phase, Phase::Emptying);
        assert_eq!(st.hero_target(), 11_000_000_000.0, "it is the Trash being emptied, not the plan or zero");
        assert_eq!(st.hero_parts(), ("11.0".to_string(), "GB"), "and it starts there at once");
        st.apply_empty(crate::engine::EmptyEvent::Started { items: 3, bytes: 11_000_000_000 });
        let trash = Kind::Trash.index();
        for (bytes, left) in [(6_000_000_000u64, 5_000_000_000u64), (3_000_000_000, 2_000_000_000), (2_000_000_000, 0)] {
            st.apply_empty(crate::engine::EmptyEvent::Item { bytes });
            assert_eq!(st.hero_target(), left as f32);
            let shown = run_frames(&mut st, 120);
            let landed = shown.iter().position(|(hero, _)| *hero == left as f32).expect("it lands on what is left");
            assert!(shown[..landed].iter().all(|(_, (_, unit))| *unit == "GB"), "the unit holds all the way down");
            let digits: Vec<f32> = shown.iter().map(|(hero, _)| *hero).collect();
            assert!(digits.windows(2).all(|p| p[1] <= p[0]), "it only goes down");
            assert_eq!(st.anim.hero, left as f32, "and lands on what is left");
            assert_eq!(st.anim.arc[trash], left as f32, "the strip's Trash bar follows the same figure");
        }
        let report = crate::engine::EmptyReport { removed: 3, bytes: 11_000_000_000, ..Default::default() };
        st.apply_empty(crate::engine::EmptyEvent::Finished(report));
        assert_eq!(st.phase, Phase::Ready);
        let shown = run_frames(&mut st, 600);
        assert_eq!(shown.last().unwrap().1, ("0".to_string(), "B"), "and when it is done the number rests on 0");
        assert!(!st.tick(0.05).moving);
    }

    #[test]
    fn emptying_does_not_count_an_entry_that_stayed() {
        let mut st = ready(three());
        with_trash(&mut st, &[4_000_000_000, 1_000_000_000]);
        st.handle_key(ch('e'));
        st.handle_key(ch('y'));
        st.apply_empty(crate::engine::EmptyEvent::Item { bytes: 0 });
        assert_eq!(st.hero_target(), 5_000_000_000.0, "an entry that could not be removed is still in the Trash");
        st.apply_empty(crate::engine::EmptyEvent::Item { bytes: 4_000_000_000 });
        assert_eq!(st.hero_target(), 1_000_000_000.0);
    }

    // ------------------------------------------------------------------------------ apps

    /// An app as the Apps scan would hand it over: the bundle first, then its traces.
    fn entry(name: &str, id: &str, bundle: u64, traces: &[u64], kept: u64, running: bool) -> AppEntry {
        use crate::apps::{AppIdentity, Kept};
        use crate::safety::Grant;
        let identity = AppIdentity {
            name: name.to_string(),
            bundle_id: id.to_string(),
            version: "1.0".to_string(),
            path: PathBuf::from(format!("/Users/test/Applications/{name}.app")),
        };
        let mk = |path: PathBuf, size: u64, grant: Grant| {
            let mut item = sample(crate::apps::next_id(), Kind::Apps, size, false);
            item.title = path.file_name().unwrap().to_string_lossy().into_owned();
            item.path = path;
            item.grant = grant;
            item.owner = Some(id.to_string());
            item
        };
        let mut items = vec![mk(identity.path.clone(), bundle, Grant::AppBundle)];
        for (n, bytes) in traces.iter().enumerate() {
            items.push(mk(
                PathBuf::from(format!("/Users/test/Library/Caches/{id}{}", if n == 0 { String::new() } else { format!(".{n}") })),
                *bytes,
                Grant::AppData,
            ));
        }
        let kept = if kept > 0 {
            vec![Kept {
                path: PathBuf::from(format!("/Users/test/Library/Containers/{id}")),
                what: "its sandbox container, which may hold your own documents",
                bytes: kept,
            }]
        } else {
            Vec::new()
        };
        AppEntry {
            identity,
            items,
            kept,
            notes: Vec::new(),
            cautions: Vec::new(),
            running,
            last_used: None,
            kept_by_you: false,
            leftover: false,
        }
    }

    /// A finished scan, with the Apps tab open and its list read.
    fn with_apps(entries: Vec<AppEntry>) -> State {
        let mut st = ready(three());
        assert!(matches!(st.handle_key(ch('4')), Effect::ScanApps));
        for e in entries {
            st.apply_apps(AppsEvent::Found(Box::new(e)));
        }
        st.apply_apps(AppsEvent::Finished { elapsed: std::time::Duration::from_millis(40) });
        st
    }

    fn three_apps() -> Vec<AppEntry> {
        vec![
            entry("Small", "com.v.small", 1_000_000, &[], 0, false),
            entry("Big", "com.v.big", 300_000_000, &[50_000_000, 10_000], 20_000_000, false),
            entry("Middle", "com.v.middle", 90_000_000, &[5_000_000], 0, false),
        ]
    }

    #[test]
    fn four_jumps_to_the_apps_and_asks_for_the_list_once_and_again_only_when_it_is_stale() {
        let mut st = ready(three());
        assert!(matches!(st.handle_key(ch('4')), Effect::ScanApps));
        assert_eq!(st.tab, Tab::Apps);
        assert!(st.apps.scanning);
        assert!(matches!(st.handle_key(ch('4')), Effect::None), "while it reads, asking again changes nothing");
        st.apply_apps(AppsEvent::Finished { elapsed: std::time::Duration::ZERO });
        st.handle_key(ch('1'));
        assert!(matches!(st.handle_key(ch('4')), Effect::None), "a good list is shown as it is");
        st.apps.stale = true;
        st.handle_key(ch('1'));
        assert!(matches!(st.handle_key(ch('4')), Effect::ScanApps), "a list gone stale is read again");
        assert!(st.apps.list.is_empty() && st.apps.scanning, "and starts again from nothing");
    }

    #[test]
    fn apps_arrive_biggest_first_and_the_cursor_stays_on_the_top_until_you_move() {
        let mut st = ready(three());
        st.handle_key(ch('4'));
        for e in three_apps() {
            st.apply_apps(AppsEvent::Found(Box::new(e)));
            assert_eq!(st.apps.cursor, 0, "untouched, it stays at the top while others arrive");
        }
        let names: Vec<&str> = st.apps.list.iter().map(|e| e.identity.name.as_str()).collect();
        assert_eq!(names, ["Big", "Middle", "Small"]);
        assert_eq!(st.apps_selected().unwrap().identity.name, "Big");
        // Once moved, it stays on its app as a new, bigger one slots in above.
        st.handle_key(ch('j'));
        assert_eq!(st.apps_selected().unwrap().identity.name, "Middle");
        st.apply_apps(AppsEvent::Found(Box::new(entry("Huge", "com.v.huge", 900_000_000, &[], 0, false))));
        assert_eq!(st.apps_selected().unwrap().identity.name, "Middle");
        assert_eq!(st.apps.list[0].identity.name, "Huge");
    }

    #[test]
    fn space_chooses_an_app_with_everything_that_goes_with_it_and_a_running_app_cannot_be_chosen() {
        let mut st = with_apps({
            let mut v = three_apps();
            v.push(entry("Busy", "com.v.busy", 500_000_000, &[], 0, true));
            v
        });
        assert_eq!(st.apps.list[0].identity.name, "Busy");
        st.handle_key(ch(' '));
        assert!(st.apps.chosen.is_empty());
        assert!(said(&st).contains("running"), "{}", said(&st));
        st.handle_key(ch('j'));
        st.handle_key(ch(' '));
        assert_eq!(st.apps_chosen().count(), 1);
        assert_eq!(st.apps_chosen_bytes(), 300_000_000 + 50_000_000 + 10_000);
        st.handle_key(ch(' '));
        assert!(st.apps.chosen.is_empty(), "space again un-chooses");
        // Moving around never chooses anything.
        for _ in 0..8 {
            st.handle_key(ch('j'));
            st.handle_key(ch('k'));
            st.handle_key(ch('G'));
            st.handle_key(ch('g'));
        }
        assert!(st.apps.chosen.is_empty());
    }

    #[test]
    fn on_the_apps_tab_a_and_n_are_about_apps_and_never_touch_the_cleaning_pile() {
        let mut st = with_apps(three_apps());
        let chosen_before = st.chosen.clone();
        assert!(!chosen_before.is_empty(), "the cleaning pile has things in it");
        st.handle_key(ch(' '));
        assert_eq!(st.apps.chosen.len(), 1);
        st.handle_key(ch('a'));
        assert_eq!(st.chosen, chosen_before, "a does not choose the recommended cleaning rows from here");
        assert!(said(&st).contains("never chosen for you"), "{}", said(&st));
        assert_eq!(st.apps.chosen.len(), 1, "and does not choose apps either");
        st.handle_key(ch('n'));
        assert!(st.apps.chosen.is_empty());
        assert_eq!(st.chosen, chosen_before, "n clears the chosen apps, not the cleaning pile");
    }

    #[test]
    fn d_on_the_apps_tab_asks_first_and_moves_only_the_chosen_apps_never_the_cleaning_pile() {
        let mut st = with_apps(three_apps());
        assert!(!st.chosen.is_empty());
        st.handle_key(ch('d'));
        assert!(st.dialog.is_none() && said(&st).contains("No app is chosen"), "nothing chosen, nothing to ask");
        st.handle_key(ch(' '));
        st.handle_key(ch('j'));
        st.handle_key(ch(' '));
        st.handle_key(ch('d'));
        let Some(Dialog::ConfirmUninstall(view)) = st.dialog.clone() else { panic!("expected the uninstall question") };
        assert_eq!(view.apps.len(), 2);
        assert_eq!(view.apps[0], UninstallRow { name: "Big".to_string(), bytes: 350_010_000, traces: 2, leftover: false });
        assert_eq!(view.kept_bytes, 20_000_000, "it says what stays");
        // Anything but y cancels, and nothing starts.
        st.handle_key(ch('x'));
        assert!(st.dialog.is_some(), "an unrelated key does not answer it");
        st.handle_key(key(KeyCode::Esc));
        assert!(st.dialog.is_none() && st.phase == Phase::Ready && st.pending_uninstall.is_empty());
        assert!(said(&st).contains("Nothing was touched"));
        st.handle_key(ch('d'));
        let Effect::StartMove(items) = st.handle_key(ch('y')) else { panic!("y must start the move") };
        assert_eq!(items.len(), 2 + 2 + 1, "the bundles and their traces, and nothing else");
        assert!(items.iter().all(|i| i.kind == Kind::Apps), "none of the cleaning pile came along");
        assert_eq!(st.phase, Phase::Reclaiming);
        assert_eq!(st.tab, Tab::Overview, "it plays out on the Overview, like any other move");
        assert!(st.pending_uninstall.is_empty());
    }

    /// What a removed app left behind, as the Apps scan hands it over: traces and no bundle.
    fn left(id: &str, traces: &[u64]) -> AppEntry {
        let mut e = entry(id, id, 0, traces, 0, false);
        e.items.remove(0);
        e.leftover = true;
        e.identity.path = e.items[0].path.clone();
        e
    }

    #[test]
    fn leftovers_are_chosen_like_apps_and_the_question_and_the_move_are_about_their_traces_alone() {
        let mut st =
            with_apps(vec![entry("Big", "com.v.big", 300_000_000, &[50_000_000], 0, false), left("com.old.app", &[200_000_000, 1_000])]);
        assert_eq!(st.apps.list[1].identity.name, "com.old.app", "listed among the apps, by size");
        st.handle_key(ch('j'));
        st.handle_key(ch(' '));
        assert_eq!(st.apps_chosen_bytes(), 200_001_000);
        st.handle_key(ch('d'));
        let Some(Dialog::ConfirmUninstall(view)) = st.dialog.clone() else { panic!("expected the question") };
        assert_eq!(view.apps, [UninstallRow { name: "com.old.app".to_string(), bytes: 200_001_000, traces: 2, leftover: true }]);
        let Effect::StartMove(items) = st.handle_key(ch('y')) else { panic!("y must start the move") };
        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|i| i.grant == crate::safety::Grant::AppData), "no bundle: only traces");
    }

    #[test]
    fn x_on_a_leftover_keeps_every_trace_of_it_and_lets_them_go_together() {
        let mut st = with_apps(vec![left("com.old.app", &[200_000_000, 1_000])]);
        let Effect::Keep { paths, keep } = st.handle_key(ch('x')) else { panic!("x must keep it") };
        assert!(keep && paths.len() == 2 && st.apps_selected().unwrap().kept_by_you);
        st.handle_key(ch(' '));
        assert!(st.apps.chosen.is_empty());
        let Effect::Keep { paths, keep } = st.handle_key(ch('x')) else { panic!("x must let it go") };
        assert!(!keep && paths.len() == 2 && !st.apps_selected().unwrap().kept_by_you);
    }

    #[test]
    fn the_uninstall_question_carries_each_distinct_caution_once_in_the_order_they_came() {
        let caution = "xcode-select points here, so the command line tools stop working.";
        let mut new = entry("Xcode", "com.apple.dt.Xcode", 9_000_000_000, &[], 0, false);
        new.cautions.push(caution.to_string());
        let mut old = entry("Xcode-15.4", "com.apple.dt.Xcode", 8_000_000_000, &[], 0, false);
        old.cautions = vec![caution.to_string(), "A second thing worth knowing.".to_string()];
        let plain = entry("Plain", "com.v.plain", 1_000_000, &[], 0, false);
        let mut st = with_apps(vec![new, old, plain]);
        assert_eq!(st.apps.list[2].identity.name, "Plain");
        st.handle_key(ch('G'));
        st.handle_key(ch(' '));
        st.handle_key(ch('d'));
        let Some(Dialog::ConfirmUninstall(view)) = st.dialog.clone() else { panic!("expected the uninstall question") };
        assert!(view.cautions.is_empty(), "an app with nothing to warn about asks for nothing extra");
        st.handle_key(key(KeyCode::Esc));
        st.handle_key(ch('g'));
        st.handle_key(ch(' '));
        st.handle_key(ch('j'));
        st.handle_key(ch(' '));
        st.handle_key(ch('d'));
        let Some(Dialog::ConfirmUninstall(view)) = st.dialog.clone() else { panic!("expected the uninstall question") };
        assert_eq!(view.apps.len(), 3);
        assert_eq!(view.cautions, [caution, "A second thing worth knowing."], "said once each, whoever carried them");
    }

    // ------------------------------------------------------------------------------ the free-space figure

    #[test]
    fn the_disk_is_read_with_what_macos_would_clear_by_itself_and_that_never_exceeds_the_disk() {
        let disk = Disk::of(&std::env::temp_dir()).expect("the temp volume can be read");
        assert!(disk.total > 0 && disk.free <= disk.total && disk.purgeable <= disk.total, "{disk:?}");
        let mut st = State::new(std::env::temp_dir());
        assert_eq!((st.disk.total, st.disk.free > 0), (disk.total, true));
        st.disk = Disk { total: 1, free: 1, purgeable: 99 };
        st.refresh_disk();
        assert_eq!(st.disk.total, disk.total, "a refresh reads the volume again");
        assert!(Disk::of(Path::new("/nonexistent/nowhere/at/all")).is_none());
        assert_eq!(Disk::default().purgeable, 0);
    }

    // ------------------------------------------------------------------------------ simulators

    /// A row of the Files tab that is an Xcode simulator (or runtime) as the scan hands it over.
    fn sim(n: u64, title: &str, bytes: u64, runtime: bool, running: bool) -> ScanItem {
        let mut item = sample(n, Kind::Files, bytes, false);
        item.title = title.to_string();
        item.path = PathBuf::from(format!("/Users/test/Library/Developer/CoreSimulator/Devices/{n}"));
        item.grant = crate::safety::Grant::Simulator;
        item.source = if runtime { "sim runtime" } else { "simulator" }.to_string();
        item.owner = Some(format!("064A5F07-ADB2-4AA2-95E1-FEEB341A8{n:03}"));
        item.tag = if running { crate::model::Tag::Running } else { crate::model::Tag::Yours };
        item
    }

    #[test]
    fn simulators_are_asked_about_on_their_own_in_words_that_say_they_cannot_come_back_and_then_deleted_for_good() {
        let mut st = with_files(vec![
            sim(1, "iPhone 17 Pro (iOS 26.5)", 3_700_000_000, false, false),
            sim(2, "iOS 26.5 simulator runtime", 8_500_000_000, true, false),
            file(3, "Slack.dmg", 30_000_000, false),
        ]);
        // Files and simulators together are refused: one can be put back and the other cannot.
        st.handle_key(ch(' '));
        st.handle_key(ch('G'));
        st.handle_key(ch(' '));
        st.handle_key(ch('d'));
        assert!(st.dialog.is_none() && said(&st).contains("files or simulators, not both"), "{}", said(&st));
        // Only simulators: the question, with the bytes that would go.
        st.handle_key(ch('n'));
        st.handle_key(ch('g'));
        st.handle_key(ch(' '));
        st.handle_key(ch('j'));
        st.handle_key(ch(' '));
        st.handle_key(ch('d'));
        let Some(Dialog::ConfirmUninstall(view)) = st.dialog.clone() else { panic!("expected the question") };
        assert!(view.sims && view.files && view.apps.len() == 2 && view.bytes == 12_200_000_000, "{view:?}");
        assert!(view.cautions.is_empty());
        st.handle_key(ch('n'));
        assert!(st.dialog.is_none() && st.files.chosen.len() == 2, "cancelling leaves the choice");
        st.handle_key(ch('d'));
        let Effect::DeleteSimulators(items) = st.handle_key(ch('y')) else { panic!("y must start the deletion, not a move to the Trash") };
        assert!(items.len() == 2 && items.iter().all(|i| i.grant == crate::safety::Grant::Simulator));
        assert_eq!(
            (st.phase, st.run.simulators, st.run.items, st.run.bytes, st.tab),
            (Phase::Emptying, true, 2, 12_200_000_000, Tab::Overview)
        );
        assert_eq!(st.hero_target(), 12_200_000_000.0, "the number counts down what is being deleted, like the Trash being emptied");
        assert!(st.pending_uninstall.is_empty());
        // It finishes: the deleted rows leave the list, and the words say how much came back.
        st.apply_empty(EmptyEvent::Item { bytes: 3_700_000_000 });
        st.apply_empty(EmptyEvent::Item { bytes: 8_500_000_000 });
        let report = crate::engine::EmptyReport {
            removed: 2,
            bytes: 12_200_000_000,
            kept: Vec::new(),
            gone: items.iter().map(|i| i.path.clone()).collect(),
        };
        st.apply_empty(EmptyEvent::Finished(report));
        assert_eq!(st.phase, Phase::Ready);
        assert!(said(&st).contains("Deleted 2 simulators: 12.2 GB is free again"), "{}", said(&st));
        assert_eq!(st.files.list.iter().map(|i| i.title.as_str()).collect::<Vec<_>>(), ["Slack.dmg"]);
        assert!(st.files.chosen.is_empty() && st.files.stale);
    }

    #[test]
    fn a_simulator_that_stayed_says_why_and_a_running_one_cannot_be_chosen_or_a_kept_one() {
        let mut st =
            with_files(vec![sim(1, "Booted iPhone", 5_000_000_000, false, true), sim(2, "Idle iPhone", 4_000_000_000, false, false)]);
        st.handle_key(ch(' '));
        assert!(st.files.chosen.is_empty() && said(&st).contains("running"), "{}", said(&st));
        st.handle_key(ch('j'));
        st.handle_key(ch(' '));
        assert_eq!(st.files.chosen.len(), 1);
        st.handle_key(ch('d'));
        let Effect::DeleteSimulators(items) = st.handle_key(ch('y')) else { panic!() };
        let report = crate::engine::EmptyReport {
            removed: 0,
            bytes: 0,
            kept: vec![(items[0].path.clone(), "Unable to delete device: it is in use".to_string())],
            gone: Vec::new(),
        };
        st.apply_empty(EmptyEvent::Item { bytes: 0 });
        st.apply_empty(EmptyEvent::Finished(report));
        assert!(said(&st).contains("it is in use") && st.notice.as_ref().is_some_and(|n| n.warn), "{}", said(&st));
        assert_eq!(st.files.list.len(), 2, "what stayed is still listed");
        // Kept: never chosen. (The deletion played out on the Overview, so go back to the tab, and to the list as it is.)
        st.tab = Tab::Files;
        st.files.stale = false;
        st.handle_key(ch('G'));
        st.handle_key(ch('x'));
        st.handle_key(ch(' '));
        assert!(st.files.chosen.is_empty() && said(&st).contains("asked begone to keep"), "{}", said(&st));
    }

    // ------------------------------------------------------------------------------ the history

    fn run_line(run: u64, moved: usize, in_trash: usize) -> HistoryEntry {
        HistoryEntry::Run(crate::journal::RunSummary {
            run,
            at: 1_000 + run,
            moved,
            bytes: moved as u64 * 1_000_000,
            in_trash,
            in_trash_bytes: in_trash as u64 * 1_000_000,
            restored: 0,
            unresolved: 0,
            kinds: vec![(Kind::Caches, moved)],
        })
    }

    fn some_history() -> Vec<HistoryEntry> {
        vec![
            HistoryEntry::Deleted { at: 5_000, what: "2 simulators".to_string(), bytes: 4_000_000_000 },
            HistoryEntry::Emptied { at: 4_000, items: 3, bytes: 900_000 },
            run_line(30, 4, 2),
            run_line(20, 3, 0),
            run_line(10, 2, 1),
        ]
    }

    #[test]
    fn capital_u_asks_for_the_history_and_shows_it_or_says_there_is_nothing_in_it() {
        let mut scanning = State::new(PathBuf::from("/Users/test"));
        scanning.anim.splash_over = true;
        assert!(matches!(scanning.handle_key(ch('U')), Effect::None) && said(&scanning).contains("Still looking"));
        let mut st = ready(three());
        assert!(matches!(st.handle_key(ch('U')), Effect::OpenHistory));
        st.apply_history(Vec::new());
        assert!(st.dialog.is_none() && said(&st).contains("Nothing to undo yet"), "{}", said(&st));
        st.apply_history(some_history());
        let Some(Dialog::History(view)) = &st.dialog else { panic!("the history must open") };
        assert_eq!((view.entries.len(), view.cursor), (5, 0));
        // Something else that is open is not pushed aside by a late answer.
        let mut asking = ready(three());
        asking.handle_key(ch('d'));
        asking.apply_history(some_history());
        assert!(matches!(asking.dialog, Some(Dialog::ConfirmMove(_))));
    }

    #[test]
    fn in_the_history_u_puts_back_the_run_under_the_cursor_and_only_a_run_that_has_something_to_put_back() {
        let mut st = ready(three());
        st.apply_history(some_history());
        // Deleted for good, then emptied: nothing to put back, and each says why.
        st.handle_key(ch('u'));
        assert!(st.dialog.is_some() && said(&st).contains("deleted for good"), "{}", said(&st));
        st.handle_key(ch('j'));
        st.handle_key(key(KeyCode::Enter));
        assert!(st.dialog.is_some() && said(&st).contains("emptied then") || said(&st).contains("The Trash was emptied"), "{}", said(&st));
        // The newest run with something in the Trash is the ordinary undo.
        st.handle_key(ch('j'));
        let Effect::UndoRun { run, strict } = st.handle_key(ch('u')) else { panic!("u must put the run back") };
        assert_eq!((run, strict), (30, false));
        assert!(st.dialog.is_none());
        // A run whose things have all been emptied has nothing left.
        st.apply_history(some_history());
        for _ in 0..3 {
            st.handle_key(ch('j'));
        }
        assert!(matches!(st.handle_key(ch('u')), Effect::None) && said(&st).contains("Nothing of that run"), "{}", said(&st));
        // An older run can be put back too, and says it is out of order.
        st.handle_key(ch('j'));
        let Effect::UndoRun { run, strict } = st.handle_key(key(KeyCode::Enter)) else { panic!("enter must put the run back") };
        assert_eq!((run, strict), (10, true));
    }

    #[test]
    fn the_history_cursor_stays_inside_the_list_and_escape_u_and_q_close_it() {
        let mut st = ready(three());
        st.apply_history(some_history());
        for _ in 0..20 {
            st.handle_key(ch('j'));
        }
        let Some(Dialog::History(view)) = &st.dialog else { panic!() };
        assert_eq!(view.cursor, 4);
        for _ in 0..20 {
            st.handle_key(ch('k'));
        }
        let Some(Dialog::History(view)) = &st.dialog else { panic!() };
        assert_eq!(view.cursor, 0);
        st.handle_key(ch('G'));
        let Some(Dialog::History(view)) = &st.dialog else { panic!() };
        assert_eq!(view.cursor, 4);
        for closer in [key(KeyCode::Esc), ch('U'), ch('q')] {
            st.apply_history(some_history());
            assert!(matches!(st.dialog, Some(Dialog::History(_))));
            assert!(matches!(st.handle_key(closer), Effect::None), "closing the list does not quit");
            assert!(st.dialog.is_none());
        }
        // Keys that mean something elsewhere do nothing here.
        st.apply_history(some_history());
        for c in ['d', 'e', 'a', 'x', 't', '3'] {
            st.handle_key(ch(c));
        }
        assert!(matches!(st.dialog, Some(Dialog::History(_))) && st.tab == Tab::Overview);
    }

    // ------------------------------------------------------------------------------ emptying only what begone moved

    fn trash_of(entries: &[(&str, u64, bool)]) -> TrashState {
        let entries: Vec<crate::scan::trash::TrashEntry> = entries
            .iter()
            .map(|(p, bytes, begone)| crate::scan::trash::TrashEntry { path: (*p).into(), bytes: *bytes, begone: *begone })
            .collect();
        TrashState { readable: true, bytes: entries.iter().map(|e| e.bytes).sum(), entries, note: None }
    }

    #[test]
    fn the_emptying_question_also_offers_only_what_begone_moved_when_the_trash_holds_more_than_that() {
        let mut st = ready(three());
        st.apply_trash(trash_of(&[("/t/mine", 3_000, true), ("/t/also-mine", 1_000, true), ("/t/yours", 9_000, false)]));
        st.handle_key(ch('e'));
        let Some(Dialog::ConfirmEmpty(view)) = st.dialog.clone() else { panic!("expected the question") };
        assert_eq!((view.items, view.bytes, view.from_begone, view.mine), (3, 13_000, 4_000, Some((2, 4_000))));
        // b takes only that: the run is about those two, and the number counts down from their size.
        let Effect::StartEmpty(scope) = st.handle_key(ch('b')) else { panic!("b must start the emptying") };
        assert_eq!(scope, EmptyScope::OnlyBegones);
        assert_eq!((st.phase, st.run.items, st.run.bytes, st.run.only_begone), (Phase::Emptying, 2, 4_000, true));
        assert_eq!(st.hero_target(), 4_000.0);
        st.apply_empty(EmptyEvent::Item { bytes: 3_000 });
        st.apply_empty(EmptyEvent::Item { bytes: 1_000 });
        let report =
            crate::engine::EmptyReport { removed: 2, bytes: 4_000, kept: Vec::new(), gone: vec!["/t/mine".into(), "/t/also-mine".into()] };
        st.apply_empty(EmptyEvent::Finished(report));
        assert_eq!((st.trash_items(), st.trash_bytes()), (1, 9_000), "what was yours is still counted in the Trash");
        assert!(said(&st).contains("Emptied what begone moved") && said(&st).contains("still in the Trash"), "{}", said(&st));
        // If nothing of begone's was left by the time it ran, it says so instead of claiming to have freed nothing.
        st.apply_trash(trash_of(&[("/t/mine", 3_000, true), ("/t/yours", 9_000, false)]));
        st.handle_key(ch('e'));
        st.handle_key(ch('b'));
        st.apply_empty(EmptyEvent::Finished(crate::engine::EmptyReport::default()));
        assert!(said(&st).contains("Nothing that begone moved is still in the Trash"), "{}", said(&st));
        // y still takes everything.
        st.apply_trash(trash_of(&[("/t/mine", 3_000, true), ("/t/yours", 9_000, false)]));
        st.handle_key(ch('e'));
        let Effect::StartEmpty(scope) = st.handle_key(ch('y')) else { panic!("y must start the emptying") };
        assert_eq!((scope, st.run.items, st.run.bytes, st.run.only_begone), (EmptyScope::Everything, 2, 12_000, false));
    }

    #[test]
    fn b_is_only_there_when_there_is_something_of_begones_and_something_that_is_not() {
        for entries in [vec![("/t/yours", 9_000, false)], vec![("/t/mine", 3_000, true), ("/t/also-mine", 1_000, true)]] {
            let mut st = ready(three());
            st.apply_trash(trash_of(&entries));
            st.handle_key(ch('e'));
            let Some(Dialog::ConfirmEmpty(view)) = st.dialog.clone() else { panic!("expected the question") };
            assert_eq!(view.mine, None, "{entries:?}");
            assert!(matches!(st.handle_key(ch('b')), Effect::None), "b does nothing here");
            assert!(matches!(st.dialog, Some(Dialog::ConfirmEmpty(_))), "and the question is still open");
            st.handle_key(ch('n'));
        }
        // Nothing marked and an estimate from this session: the figure still says how much is begone's.
        let mut st = ready(three());
        st.apply_trash(trash_of(&[("/t/a", 5_000, false)]));
        st.moved_to_trash = 2_000;
        st.handle_key(ch('e'));
        let Some(Dialog::ConfirmEmpty(view)) = st.dialog.clone() else { panic!("expected the question") };
        assert_eq!((view.from_begone, view.mine), (2_000, None));
    }

    // ------------------------------------------------------------------------------ the files

    /// A row of the Files tab as the scan hands it over.
    fn file(id: u64, title: &str, bytes: u64, backup: bool) -> ScanItem {
        let mut item = sample(id, Kind::Files, bytes, false);
        item.title = title.to_string();
        item.path = PathBuf::from("/Users/test/Downloads").join(title);
        item.grant = if backup { crate::safety::Grant::DeviceBackup } else { crate::safety::Grant::Installer };
        item.tag = crate::model::Tag::Yours;
        item.safety = crate::model::SafetyClass::Yours;
        item
    }

    fn with_files(items: Vec<ScanItem>) -> State {
        let mut st = ready(three());
        assert!(matches!(st.handle_key(ch('5')), Effect::ScanFiles));
        for item in items {
            st.apply_files(FilesEvent::Found(Box::new(item)));
        }
        st.apply_files(FilesEvent::Finished { elapsed: std::time::Duration::from_millis(30) });
        st
    }

    #[test]
    fn five_opens_the_files_and_asks_for_the_look_once_and_again_only_when_it_is_stale() {
        let mut st = ready(three());
        assert!(matches!(st.handle_key(ch('5')), Effect::ScanFiles));
        assert_eq!(st.tab, Tab::Files);
        assert!(st.files.scanning);
        assert!(matches!(st.handle_key(ch('5')), Effect::None), "while it looks, asking again changes nothing");
        st.apply_files(FilesEvent::Note("iPhone backups could not be read.".to_string()));
        st.apply_files(FilesEvent::Finished { elapsed: std::time::Duration::ZERO });
        st.handle_key(ch('1'));
        assert!(matches!(st.handle_key(ch('5')), Effect::None), "a good list is shown as it is");
        st.files.stale = true;
        st.handle_key(ch('1'));
        assert!(matches!(st.handle_key(ch('5')), Effect::ScanFiles), "a stale one is read again");
        assert!(st.files.list.is_empty() && st.files.notes.is_empty() && st.files.scanning, "from nothing, notes included");
    }

    #[test]
    fn files_arrive_biggest_first_and_are_chosen_one_at_a_time_and_never_for_you() {
        let mut st = with_files(vec![
            file(1, "Small.dmg", 1_000_000, false),
            file(2, "Big.iso", 4_000_000_000, false),
            file(3, "Mid.pkg", 90_000_000, false),
        ]);
        assert_eq!(st.files.list.iter().map(|i| i.title.as_str()).collect::<Vec<_>>(), ["Big.iso", "Mid.pkg", "Small.dmg"]);
        assert!(st.files.chosen.is_empty(), "nothing is chosen for you");
        // With no duplicates present, bulk-choose does nothing and says so; nothing else here is ever chosen for you.
        st.handle_key(ch('a'));
        assert!(st.files.chosen.is_empty() && said(&st).contains("No duplicates"), "{}", said(&st));
        st.handle_key(ch(' '));
        st.handle_key(ch('j'));
        st.handle_key(ch(' '));
        assert_eq!(st.files_chosen().count(), 2);
        assert_eq!(st.files_chosen_bytes(), 4_090_000_000);
        st.handle_key(ch('n'));
        assert!(st.files.chosen.is_empty());
        // The cleaning pile is a different list: nothing here touches it.
        let pile = st.chosen.clone();
        st.handle_key(ch(' '));
        assert_eq!(st.chosen, pile);
    }

    #[test]
    fn d_on_the_files_tab_asks_first_with_a_warning_for_a_backup_and_moves_only_what_is_chosen() {
        let mut st = with_files(vec![file(1, "Slack.dmg", 30_000_000, false), file(2, "Abhi's iPhone", 9_000_000_000, true)]);
        st.handle_key(ch('d'));
        assert!(st.dialog.is_none() && said(&st).contains("No file is chosen"));
        st.handle_key(ch(' '));
        st.handle_key(ch('d'));
        let Some(Dialog::ConfirmUninstall(view)) = st.dialog.clone() else { panic!("expected the question") };
        assert!(view.files && view.apps.len() == 1 && view.cautions.len() == 1, "{view:?}");
        assert!(view.cautions[0].contains("only copy"), "a backup says so");
        st.handle_key(key(KeyCode::Esc));
        assert!(st.dialog.is_none() && st.pending_uninstall.is_empty() && st.files.chosen.len() == 1);
        st.handle_key(ch('j'));
        st.handle_key(ch(' '));
        st.handle_key(ch('d'));
        let Some(Dialog::ConfirmUninstall(view)) = st.dialog.clone() else { panic!("expected the question") };
        assert_eq!((view.apps.len(), view.cautions.len()), (2, 1));
        // A question about files only warns about backups when one is in it.
        st.handle_key(key(KeyCode::Esc));
        st.handle_key(ch('k'));
        st.handle_key(ch(' '));
        st.handle_key(ch('d'));
        let Some(Dialog::ConfirmUninstall(view)) = st.dialog.clone() else { panic!("expected the question") };
        assert!(view.cautions.is_empty() && view.apps.len() == 1 && view.apps[0].name == "Slack.dmg", "{view:?}");
        let Effect::StartMove(items) = st.handle_key(ch('y')) else { panic!("y must start the move") };
        assert!(items.iter().all(|i| i.kind == Kind::Files), "none of the cleaning pile came along");
        assert_eq!(st.phase, Phase::Reclaiming);
        st.apply_run(RunEvent::Finished(all_moved(&items)));
        assert!(st.files.list.iter().all(|i| !items.iter().any(|m| m.id == i.id)), "what moved is gone from the list");
        assert!(st.files.chosen.is_empty());
        assert!(st.receipt.iter().any(|l| l.left.starts_with("Files · ")), "{:?}", st.receipt.iter().map(|l| &l.left).collect::<Vec<_>>());
    }

    #[test]
    fn x_keeps_a_file_and_a_kept_file_cannot_be_chosen_and_an_undo_makes_the_list_stale() {
        let mut st = with_files(vec![file(1, "Slack.dmg", 30_000_000, false), file(2, "Other.pkg", 20_000_000, false)]);
        let Effect::Keep { paths, keep } = st.handle_key(ch('x')) else { panic!("x must keep the file") };
        assert!(keep && paths == [PathBuf::from("/Users/test/Downloads/Slack.dmg")]);
        assert!(st.files_selected().unwrap().kept);
        st.handle_key(ch(' '));
        assert!(st.files.chosen.is_empty() && said(&st).contains("asked begone to keep"), "{}", said(&st));
        let Effect::Keep { keep, .. } = st.handle_key(ch('x')) else { panic!("x must let it go") };
        assert!(!keep && !st.files_selected().unwrap().kept);
        st.files.stale = false;
        st.apply_undo(UndoEvent::Finished(crate::engine::UndoReport::default()));
        assert!(st.files.stale);
    }

    // ------------------------------------------------------------------------------ a size to free

    /// Rows for the goal: `(id, kind, gigabytes, recommended, rebuildable)`.
    fn goal_rows() -> State {
        let rows = [
            (1, Kind::Caches, 10, true, false),
            (2, Kind::Caches, 4, true, false),
            (3, Kind::Logs, 1, true, false),
            (4, Kind::Developer, 8, true, true),
            (5, Kind::Developer, 3, true, true),
            (6, Kind::Developer, 50, false, true),
            (7, Kind::Caches, 6, true, false),
        ];
        let items = rows
            .into_iter()
            .map(|(id, kind, gb, recommended, rebuildable)| {
                let mut item = sample(id, kind, gb * 1_000_000_000, recommended);
                if rebuildable {
                    item.safety = crate::model::SafetyClass::Rebuildable;
                }
                item
            })
            .collect();
        let mut st = ready(items);
        // Row 7 is recommended, and kept.
        let kept = st.items[6].path.clone();
        st.set_kept(vec![kept]);
        st
    }

    fn chosen_ids(st: &State) -> Vec<u64> {
        let mut ids: Vec<u64> = st.chosen.iter().copied().collect();
        ids.sort_unstable();
        ids
    }

    #[test]
    fn a_goal_chooses_the_safest_recommended_rows_with_as_little_to_spare_as_one_row_can_manage() {
        let mut st = goal_rows();
        assert_eq!(st.recommended_bytes(), 26_000_000_000, "not the 50 GB nobody recommends, not the kept 6 GB, not the Trash");
        // 5 GB: the safe rows are 10, 4 and 1, and the 10 is the only one that covers it alone.
        st.choose_for_goal(5_000_000_000);
        assert_eq!(chosen_ids(&st), [1]);
        assert!(said(&st).contains("Chose 10.0 GB in 1 row to free 5.0 GB") && said(&st).contains("Nothing moves"), "{}", said(&st));
        // 12 GB: the biggest, then the smallest that covers what is left.
        st.choose_for_goal(12_000_000_000);
        assert_eq!(chosen_ids(&st), [1, 2]);
        // Whatever was chosen before is replaced, not added to.
        st.choose_for_goal(1_000_000_000);
        assert_eq!(chosen_ids(&st), [3]);
        // More than the safe rows hold: the rebuildable ones follow, safe ones first.
        st.choose_for_goal(17_000_000_000);
        assert_eq!(chosen_ids(&st), [1, 2, 3, 5], "10 + 4 + 1 safe, then the smallest rebuildable row that covers the last 2");
        assert_eq!(st.chosen_bytes(), 18_000_000_000);
        // More than there is: everything recommended, and it says how far short.
        st.choose_for_goal(40_000_000_000);
        assert_eq!(chosen_ids(&st), [1, 2, 3, 4, 5]);
        assert!(said(&st).contains("only 26.0 GB, 14.0 GB short of 40.0 GB") && said(&st).contains("Review has more"), "{}", said(&st));
        // The motion and the question are the same as for any other choice.
        let Some(Dialog::ConfirmMove(view)) = ({
            st.handle_key(ch('d'));
            st.dialog.clone()
        }) else {
            panic!("d must ask first")
        };
        assert_eq!(view.count, 5);
    }

    #[test]
    fn t_asks_how_much_and_takes_what_is_typed() {
        let mut st = goal_rows();
        st.handle_key(ch('t'));
        assert!(matches!(&st.dialog, Some(Dialog::Goal(v)) if v.text.is_empty()));
        let typed = |st: &State| match &st.dialog {
            Some(Dialog::Goal(v)) => (v.text.clone(), v.error.clone()),
            other => panic!("the question went away: {other:?}"),
        };
        // Enter with nothing typed, then with nonsense: it says so and stays open.
        st.handle_key(key(KeyCode::Enter));
        assert!(typed(&st).1.unwrap().contains("Type a size first"));
        for c in "abc!".chars() {
            st.handle_key(ch(c));
        }
        assert_eq!(typed(&st), ("abc".to_string(), None), "typing clears the complaint, and only letters, digits, . , and space are taken");
        st.handle_key(key(KeyCode::Enter));
        assert!(typed(&st).1.unwrap().contains("not a size"));
        // Fix it: backspace, then a size.
        for _ in 0..3 {
            st.handle_key(key(KeyCode::Backspace));
        }
        st.handle_key(key(KeyCode::Backspace));
        for c in "12 gb".chars() {
            st.handle_key(ch(c));
        }
        assert_eq!(typed(&st).0, "12 gb");
        // Keys that mean something elsewhere are only letters here.
        for c in "dqu".chars() {
            st.handle_key(ch(c));
        }
        assert_eq!(typed(&st).0, "12 gbdqu");
        for _ in 0..3 {
            st.handle_key(key(KeyCode::Backspace));
        }
        st.handle_key(key(KeyCode::Enter));
        assert!(st.dialog.is_none());
        assert_eq!(chosen_ids(&st), [1, 2], "12 GB");
    }

    #[test]
    fn escape_cancels_the_question_and_changes_nothing_and_the_text_is_kept_short() {
        let mut st = goal_rows();
        let before = chosen_ids(&st);
        st.handle_key(ch('t'));
        for c in "123456789012345678".chars() {
            st.handle_key(ch(c));
        }
        let Some(Dialog::Goal(v)) = &st.dialog else { panic!() };
        assert_eq!(v.text.len(), 12);
        st.handle_key(key(KeyCode::Esc));
        assert!(st.dialog.is_none() && chosen_ids(&st) == before && said(&st).contains("Nothing was chosen"));
    }

    #[test]
    fn t_waits_for_the_scan_and_for_a_run_and_asks_only_on_the_overview_or_review() {
        let mut scanning = State::new(PathBuf::from("/Users/test"));
        scanning.anim.splash_over = true;
        scanning.handle_key(ch('t'));
        assert!(scanning.dialog.is_none() && said(&scanning).contains("Still looking"));
        let mut st = goal_rows();
        st.handle_key(ch('3'));
        st.handle_key(ch('t'));
        assert!(st.dialog.is_none() && said(&st).contains("Overview or Review"), "{}", said(&st));
        st.handle_key(ch('4'));
        st.handle_key(ch('t'));
        assert!(st.dialog.is_none());
        st.handle_key(ch('2'));
        st.handle_key(ch('t'));
        assert!(matches!(st.dialog, Some(Dialog::Goal(_))), "Review asks too");
        st.handle_key(key(KeyCode::Esc));
        st.phase = Phase::Reclaiming;
        st.handle_key(ch('t'));
        assert!(st.dialog.is_none(), "not while something is moving");
    }

    // ------------------------------------------------------------------------------ the places you keep

    #[test]
    fn x_keeps_the_row_you_are_on_and_a_second_x_lets_it_go() {
        let mut st = ready(three());
        st.handle_key(ch('2'));
        let first = st.current_item().unwrap().clone();
        assert_eq!(first.id, 1);
        let Effect::Keep { paths, keep } = st.handle_key(ch('x')) else { panic!("x must keep the row") };
        assert!(keep && paths == [first.path.clone()]);
        assert!(st.items[0].kept && !st.chosen.contains(&1), "a kept row is not chosen");
        assert_eq!((st.totals[Kind::Developer.index()].count, st.totals[Kind::Developer.index()].kept), (0, 1));
        assert_eq!(st.chosen_items().iter().map(|i| i.id).collect::<Vec<_>>(), [2], "only the other recommended row would move");
        assert!(said(&st).contains("Kept"), "{}", said(&st));
        // Nothing chooses it for you, and you cannot choose it by mistake.
        st.handle_key(ch(' '));
        assert!(!st.chosen.contains(&1) && said(&st).contains("asked begone to keep"), "{}", said(&st));
        st.handle_key(ch('a'));
        st.handle_key(ch('1'));
        st.handle_key(ch(' '));
        assert!(!st.chosen.contains(&1), "neither a nor the kind row picks it up");
        assert_eq!(st.found_bytes(), 1_500_000_000, "the figures leave it out");
        // Let it go again: it is offered again, and unchosen until you choose it.
        st.handle_key(ch('2'));
        let Effect::Keep { keep, .. } = st.handle_key(ch('x')) else { panic!("x must let it go") };
        assert!(!keep && !st.items[0].kept && !st.chosen.contains(&1));
        assert_eq!(st.totals[Kind::Developer.index()].count, 1);
    }

    #[test]
    fn what_a_kept_place_holds_is_kept_and_x_on_the_inside_says_which_place_holds_it() {
        let mut item = sample(1, Kind::Developer, 5_000_000_000, true);
        at_home(&mut item, "code/target");
        let mut st = mapped(vec![item]);
        assert!(st.chosen.contains(&1));
        // On the map, `code` is the second row.
        st.handle_key(ch('j'));
        let Effect::Keep { paths, keep } = st.handle_key(ch('x')) else { panic!("x must keep the folder") };
        assert!(keep && paths == [PathBuf::from("/Users/test/code")]);
        assert!(st.items[0].kept && st.chosen.is_empty(), "the build folder in it is kept with it");
        // From Review the row says why, and x on it changes nothing.
        st.handle_key(ch('2'));
        assert!(matches!(st.handle_key(ch('x')), Effect::None));
        assert!(said(&st).contains("~/code is kept"), "{}", said(&st));
        // Let the folder go from the map.
        st.handle_key(ch('3'));
        let Effect::Keep { keep, .. } = st.handle_key(ch('x')) else { panic!("x must let it go") };
        assert!(!keep && !st.items[0].kept);
        // The folded entry has no single place to keep.
        st.handle_key(ch('G'));
        assert!(matches!(st.handle_key(ch('x')), Effect::None) && said(&st).contains("Open the folder"), "{}", said(&st));
    }

    #[test]
    fn a_scan_that_arrives_after_the_list_was_read_marks_and_never_chooses_what_is_kept() {
        let mut st = State::new(PathBuf::from("/Users/test"));
        st.anim.splash_over = true;
        st.set_kept(vec![PathBuf::from("/Users/test/Library/Caches/com.kept")]);
        let mut kept = sample(1, Kind::Caches, 1_000, true);
        at_home(&mut kept, "Library/Caches/com.kept");
        let mut other = sample(2, Kind::Caches, 1_000, true);
        at_home(&mut other, "Library/Caches/com.other");
        st.apply_scan(ScanEvent::Found(Box::new(kept)));
        st.apply_scan(ScanEvent::Found(Box::new(other)));
        assert!(st.items[0].kept && !st.items[1].kept);
        assert_eq!(st.chosen.iter().copied().collect::<Vec<_>>(), [2]);
        // The same holds for a scan that replaces the rows.
        st.apply_scan(ScanEvent::Finished(ScanSummary::default()));
        st.begin_scan(true);
        for (id, rest) in [(11, "Library/Caches/com.kept"), (12, "Library/Caches/com.other")] {
            let mut item = sample(id, Kind::Caches, 1_000, true);
            at_home(&mut item, rest);
            st.apply_scan(ScanEvent::Found(Box::new(item)));
        }
        st.apply_scan(ScanEvent::Finished(ScanSummary::default()));
        assert_eq!(st.chosen.iter().copied().collect::<Vec<_>>(), [12]);
    }

    #[test]
    fn x_on_an_app_keeps_its_bundle_and_a_kept_app_cannot_be_chosen_or_uninstalled() {
        let mut st = with_apps(three_apps());
        let big = st.apps_selected().unwrap().identity.path.clone();
        let Effect::Keep { paths, keep } = st.handle_key(ch('x')) else { panic!("x must keep the app") };
        assert!(keep && paths == [big.clone()]);
        assert!(st.apps_selected().unwrap().kept_by_you);
        st.handle_key(ch(' '));
        assert!(st.apps.chosen.is_empty() && said(&st).contains("asked begone to keep Big"), "{}", said(&st));
        st.handle_key(ch('d'));
        assert!(st.dialog.is_none() && said(&st).contains("No app is chosen"));
        // One that was chosen before it was kept is no longer chosen.
        st.handle_key(ch('x'));
        st.handle_key(ch(' '));
        assert_eq!(st.apps.chosen.len(), 1);
        st.handle_key(ch('x'));
        assert!(st.apps.chosen.is_empty());
        // The other apps are untouched by all of it.
        st.handle_key(ch('j'));
        st.handle_key(ch(' '));
        assert_eq!(st.apps_chosen().count(), 1);
    }

    #[test]
    fn begone_on_the_overview_never_includes_a_chosen_app() {
        let mut st = with_apps(three_apps());
        st.handle_key(ch(' '));
        st.handle_key(ch('1'));
        st.handle_key(ch('d'));
        let Some(Dialog::ConfirmMove(view)) = st.dialog.clone() else { panic!("expected the cleaning question") };
        assert!(view.per_kind.iter().all(|(k, _, _)| *k != Kind::Apps));
        let Effect::StartMove(items) = st.handle_key(ch('y')) else { panic!() };
        assert!(items.iter().all(|i| i.kind != Kind::Apps), "an app chosen on its own tab is not part of begone's pile");
    }

    #[test]
    fn a_finished_uninstall_clears_the_apps_and_anything_the_cleaning_list_had_inside_them() {
        let mut st = with_apps(three_apps());
        // The cleaning scan also listed the big app's cache folder as junk.
        let mut junk = sample(7_001, Kind::Caches, 50_000_000, true);
        junk.path = PathBuf::from("/Users/test/Library/Caches/com.v.big");
        st.apply_scan(ScanEvent::Found(Box::new(junk)));
        st.handle_key(ch(' '));
        st.handle_key(ch('d'));
        let Effect::StartMove(items) = st.handle_key(ch('y')) else { panic!() };
        st.apply_run(RunEvent::Finished(all_moved(&items)));
        assert!(st.apps.list.iter().all(|e| e.identity.name != "Big"), "the uninstalled app is gone from the list");
        assert_eq!(st.apps.list.len(), 2);
        assert!(st.apps.chosen.is_empty());
        assert!(st.apps.stale, "and the list is read again the next time it is looked at");
        assert!(st.items.iter().all(|i| i.id != 7_001), "the cleaning list no longer offers a cache that has already moved");
        let lines: Vec<&str> = st.receipt.iter().map(|l| l.left.as_str()).collect();
        assert!(lines.iter().any(|l| l.starts_with("Apps · ")), "{lines:?}");
        assert!(!lines.iter().any(|l| l.contains("left unchecked")), "the cleaning rows are not what was left alone: {lines:?}");
    }

    #[test]
    fn an_app_that_stayed_stays_in_the_list_and_an_undo_or_an_emptying_makes_the_list_stale() {
        let mut st = with_apps(three_apps());
        st.handle_key(ch(' '));
        st.handle_key(ch('d'));
        let Effect::StartMove(items) = st.handle_key(ch('y')) else { panic!() };
        let mut report = all_moved(&items);
        for r in &mut report.results {
            r.outcome = Outcome::Refused("it is running: quit it first".to_string());
        }
        st.apply_run(RunEvent::Finished(report));
        assert_eq!(st.apps.list.len(), 3, "nothing moved, so nothing leaves the list");
        assert!(st.apps.chosen.is_empty(), "what was asked for and refused is no longer chosen");
        st.apps.stale = false;
        st.apply_empty(crate::engine::EmptyEvent::Finished(crate::engine::EmptyReport::default()));
        assert!(st.apps.stale, "emptying the Trash can make an uninstall final");
    }
}
