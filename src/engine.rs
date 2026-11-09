//! The engine: the only code that changes the user's disk.
//!
//! Every function here is plain and synchronous and reports through a callback, so the
//! TUI can run it on a worker thread and the tests can run it directly. Rules, in order:
//!
//! 1. **Re-check, always.** The scan may be minutes old. A folder that was a cache at scan
//!    time could be a symlink now. The same safety gate that admitted a row at scan time
//!    runs again immediately before the row is touched.
//! 2. **Trash, never `rm`.** Everything is moved to the Trash and journaled. The single
//!    exception is [`empty_trash`], which is its own action and says it cannot be undone.
//! 3. **Fail closed.** No working undo history, no move.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::journal::{self, Journal, RunId, UndoPlan};
use crate::model::{Evidence, Kind, ScanItem, Tag, ago, idle_since};
use crate::safety::{Grant, Safety, home_dir};
use crate::scan::procs::Procs;
use crate::scan::trash::{self, TrashEntry, TrashState};
use crate::scan::{self, Candidate, Ctx};
use crate::sizing::{self, Measure};
use crate::trashy::{self, DirTrash, SystemTrash, Trasher};

/// Everything the engine needs to know about where it is working.
pub struct Env {
    pub home: PathBuf,
    pub safety: Safety,
    pub journal: Journal,
    pub trasher: Box<dyn Trasher>,
    /// True when `home` is a scratch folder (`--path`), whose Trash is a plain folder too.
    pub sandboxed: bool,
    /// Which apps are running, when a test says so. Otherwise the system is asked, just before an app is removed.
    pub procs_override: Option<Procs>,
    /// The optional config file's tuning of the size and age thresholds. The defaults when there is no file.
    pub config: crate::config::Config,
}

impl Env {
    pub fn new(home: PathBuf) -> Env {
        let sandboxed = match (home.canonicalize(), home_dir().canonicalize()) {
            (Ok(a), Ok(b)) => a != b,
            _ => true,
        };
        let config = crate::config::load(&home);
        let bytes = config.big_file_min_bytes.unwrap_or(crate::safety::BIG_DOWNLOAD_MIN_BYTES);
        let age = config.big_file_min_age.unwrap_or(crate::safety::BIG_DOWNLOAD_MIN_AGE);
        let safety = Safety::for_home(&home).with_big_download(bytes, age);
        let trasher: Box<dyn Trasher> =
            if sandboxed { Box::new(DirTrash(safety.trash_dir().to_path_buf())) } else { Box::new(SystemTrash) };
        Env { journal: Journal::new(&home), safety, home, trasher, sandboxed, procs_override: None, config }
    }

    /// The Trash as it is now, with what begone moved there marked.
    pub fn read_trash(&self) -> TrashState {
        trash::summarize_marked(self.safety.trash_dir(), &self.journal.moved_identities())
    }
}

// ---------------------------------------------------------------- scanning

pub enum ScanEvent {
    /// Overall progress in 0..=1.
    Progress(f32),
    /// The folder being measured right now.
    Reading(PathBuf),
    Found(Box<ScanItem>),
    Trash(TrashState),
    Finished(ScanSummary),
}

#[derive(Clone, Debug, Default)]
pub struct ScanSummary {
    pub elapsed: Duration,
    pub items: usize,
    pub files: u64,
    pub bytes: u64,
    pub cancelled: bool,
}

/// Turn a measured candidate into a row, or drop it.
pub fn finish(mut cand: Candidate, m: Measure, id: u64) -> Option<ScanItem> {
    if m.bytes == 0 {
        return None;
    }
    let idle = idle_since(m.newest);
    if let (Some(min), Some(idle)) = (cand.min_idle, idle)
        && idle < min
    {
        return None;
    }
    if let Some(idle) = idle {
        if cand.in_use_window.is_some_and(|w| idle < w) {
            cand.tag = Tag::InUse;
            cand.pre_selected = false;
            cand.evidence.push(Evidence::warn(format!("Used recently: something inside changed {}", ago(idle))));
        } else if idle >= Duration::from_secs(86_400) {
            cand.evidence.push(Evidence::good(format!("Last changed {}", ago(idle))));
        }
    }
    Some(ScanItem {
        id,
        path: cand.path,
        size: m.bytes,
        files: m.files,
        kind: cand.kind,
        safety: cand.safety,
        tag: cand.tag,
        grant: cand.grant,
        title: cand.title,
        source: cand.source,
        what: cand.what,
        comes_back: cand.comes_back,
        evidence: cand.evidence,
        modified: m.newest,
        pre_selected: cand.pre_selected,
        owner: None,
        kept: false,
    })
}

/// How many trees are measured side by side, and the walker threads each one gets.
const MEASURE_WORKERS: usize = 4;
const THREADS_PER_TREE: usize = 3;

enum Measured {
    Started(PathBuf),
    Done(usize, Option<Measure>),
}

pub fn scan(env: &Env, cancel: &AtomicBool, on_event: &mut dyn FnMut(ScanEvent)) {
    let started = Instant::now();
    let procs = Procs::detect();
    let ctx = Ctx { home: &env.home, procs: &procs, cancel };
    let mut summary = ScanSummary::default();
    let mut next_id = 1u64;
    let mut base = 0.0f32;

    for (_, share, discover) in scan::STAGES {
        let candidates = discover(&ctx);
        let total = candidates.len().max(1) as f32;
        let next = AtomicUsize::new(0);
        let (tx, rx) = mpsc::channel::<Measured>();
        std::thread::scope(|scope| {
            for _ in 0..MEASURE_WORKERS.min(candidates.len()) {
                let (tx, next, candidates) = (tx.clone(), &next, &candidates);
                scope.spawn(move || {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(cand) = candidates.get(i) else { break };
                        if cancel.load(Ordering::Relaxed) {
                            break;
                        }
                        // The same gate the deleter uses: a row that could never be removed is never shown.
                        let measured = env.safety.check(&cand.path, cand.grant).ok().map(|real| {
                            let _ = tx.send(Measured::Started(cand.path.clone()));
                            sizing::measure_with(&real, THREADS_PER_TREE)
                        });
                        let _ = tx.send(Measured::Done(i, measured));
                    }
                });
            }
            drop(tx);
            let mut done = 0usize;
            for message in rx {
                match message {
                    Measured::Started(path) => on_event(ScanEvent::Reading(path)),
                    Measured::Done(i, measured) => {
                        done += 1;
                        if let Some(mut item) = measured.and_then(|m| finish(candidates[i].clone(), m, next_id)) {
                            item.kept = env.safety.is_kept(&item.path);
                            next_id += 1;
                            summary.items += 1;
                            summary.files += item.files;
                            summary.bytes += item.size;
                            on_event(ScanEvent::Found(Box::new(item)));
                        }
                        on_event(ScanEvent::Progress(base + share * done as f32 / total));
                    }
                }
            }
        });
        base += share;
        if cancel.load(Ordering::Relaxed) {
            summary.cancelled = true;
            break;
        }
    }
    if !summary.cancelled {
        on_event(ScanEvent::Reading(env.safety.trash_dir().to_path_buf()));
        on_event(ScanEvent::Trash(env.read_trash()));
        on_event(ScanEvent::Progress(1.0));
    }
    summary.elapsed = started.elapsed();
    on_event(ScanEvent::Finished(summary));
}

// ---------------------------------------------------------------- moving

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Moved {
        to: Option<PathBuf>,
    },
    /// The safety layer said no. Nothing was touched.
    Refused(String),
    Failed(String),
    /// A stop was requested first.
    NotAttempted,
}

#[derive(Clone, Debug)]
pub struct ItemResult {
    pub id: u64,
    pub title: String,
    pub kind: Kind,
    pub bytes: u64,
    pub outcome: Outcome,
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub results: Vec<ItemResult>,
    pub cancelled: bool,
    pub warnings: Vec<String>,
    pub elapsed: Duration,
}

impl Report {
    pub fn moved(&self) -> impl Iterator<Item = &ItemResult> {
        self.results.iter().filter(|r| matches!(r.outcome, Outcome::Moved { .. }))
    }
    pub fn moved_count(&self) -> usize {
        self.moved().count()
    }
    pub fn moved_bytes(&self) -> u64 {
        self.moved().map(|r| r.bytes).sum()
    }
    /// Everything that was asked for but did not move.
    pub fn kept(&self) -> Vec<&ItemResult> {
        self.results.iter().filter(|r| !matches!(r.outcome, Outcome::Moved { .. })).collect()
    }
}

pub enum RunEvent {
    Started { items: usize, bytes: u64 },
    Item(ItemResult),
    Finished(Report),
}

pub fn delete(env: &Env, items: &[ScanItem], cancel: &AtomicBool, on_event: &mut dyn FnMut(RunEvent)) -> Report {
    let started = Instant::now();
    let run = journal::new_run_id();
    on_event(RunEvent::Started { items: items.len(), bytes: items.iter().map(|i| i.size).sum() });
    let mut report = Report::default();
    let mut appender = match env.journal.appender() {
        Ok(a) => Some(a),
        Err(err) => {
            report.warnings.push(format!("The undo history could not be opened ({err}), so nothing was moved."));
            None
        }
    };
    // Removing an app needs two things the gate alone cannot know: whether it is running, and what else is installed.
    let mut apps = items.iter().any(|i| matches!(i.grant, Grant::AppBundle | Grant::AppData)).then(|| AppRun::start(env));
    for item in items {
        let outcome = match appender.as_mut() {
            None => Outcome::Failed("the undo history is unavailable".to_string()),
            Some(_) if cancel.load(Ordering::Relaxed) => {
                report.cancelled = true;
                Outcome::NotAttempted
            }
            Some(app) => move_one(env, app, run, item, &mut report.warnings, apps.as_mut()),
        };
        let result = ItemResult { id: item.id, title: item.title.clone(), kind: item.kind, bytes: item.size, outcome };
        on_event(RunEvent::Item(result.clone()));
        report.results.push(result);
    }
    if let Some(app) = appender.as_mut() {
        let _ = app.sync();
    }
    report.elapsed = started.elapsed();
    on_event(RunEvent::Finished(report.clone()));
    report
}

/// What a run that removes apps knows about the machine, taken once at the start.
struct AppRun {
    procs: Procs,
    /// Installed bundles per identifier, before anything moved.
    installed: HashMap<String, usize>,
    /// Bundles this run has moved, per identifier.
    removed: HashMap<String, usize>,
}

impl AppRun {
    fn start(env: &Env) -> AppRun {
        AppRun {
            procs: env.procs_override.clone().unwrap_or_else(Procs::detect),
            installed: crate::apps::installed_ids(&env.safety),
            removed: HashMap::new(),
        }
    }

    /// Something that could stop this item for reasons the gate cannot see.
    fn refusal(&self, item: &ScanItem) -> Option<String> {
        let owner = item.owner.as_deref()?;
        match item.grant {
            Grant::AppBundle if self.procs.bundle_running(&item.path, owner) => Some("it is running: quit it first".to_string()),
            // A trace goes only when nothing installed still claims its identifier: the bundle moved first, in this run.
            Grant::AppData if crate::apps::count_installed(&self.installed, owner) > self.removed.get(owner).copied().unwrap_or(0) => {
                Some("the app is still installed, so its settings stay".to_string())
            }
            // The leftovers of an app that was gone when they were listed: it may have been opened since.
            Grant::AppData if self.procs.app_for(owner).is_some() => Some("the app is running, so what it keeps stays".to_string()),
            _ => None,
        }
    }
}

fn move_one(
    env: &Env,
    app: &mut journal::Appender,
    run: RunId,
    item: &ScanItem,
    warnings: &mut Vec<String>,
    mut apps: Option<&mut AppRun>,
) -> Outcome {
    let real = match env.safety.check_move(&item.path, item.grant, item.owner.as_deref()) {
        Ok(real) => real,
        Err(refusal) => return Outcome::Refused(refusal.to_string()),
    };
    if let Some(why) = apps.as_deref().and_then(|a| a.refusal(item)) {
        return Outcome::Refused(why);
    }
    if real.to_str().is_none() {
        return Outcome::Failed("its path is not valid UTF-8".to_string());
    }
    // Write it down first. If that fails, leave the item alone.
    if let Err(err) = app.intent(run, &real, item.size, item.kind) {
        return Outcome::Failed(format!("the undo history could not be written ({err}), so it was left alone"));
    }
    match env.trasher.trash(&real) {
        Ok(to) => {
            if let Err(err) = app.moved(run, &real, to.as_deref()) {
                warnings.push(format!("{} moved, but its undo record could not be written ({err}).", item.title));
            }
            if let (Grant::AppBundle, Some(owner), Some(apps)) = (item.grant, item.owner.as_ref(), apps.as_mut()) {
                *apps.removed.entry(owner.clone()).or_insert(0) += 1;
            }
            Outcome::Moved { to }
        }
        Err(err) => {
            let _ = app.failed(run, &real);
            let hint = if item.grant == Grant::AppBundle && looks_like_a_permission_error(&err) {
                " macOS may need App Management permission for your terminal (System Settings > Privacy & Security > App Management)."
            } else {
                ""
            };
            Outcome::Failed(format!("{err}{hint}"))
        }
    }
}

fn looks_like_a_permission_error(text: &str) -> bool {
    let lower = text.to_lowercase();
    ["permission", "not permitted", "operation not allowed", "not allowed", "protected"].iter().any(|w| lower.contains(w))
}

// ---------------------------------------------------------------- undo

#[derive(Clone, Debug, Default)]
pub struct UndoReport {
    pub restored: Vec<journal::Restorable>,
    pub failed: Vec<(journal::Restorable, String)>,
    pub unresolved: usize,
}

pub enum UndoEvent {
    Started { items: usize, bytes: u64 },
    Item,
    Finished(UndoReport),
}

pub fn undo(env: &Env, plan: &UndoPlan, cancel: &AtomicBool, on_event: &mut dyn FnMut(UndoEvent)) -> UndoReport {
    on_event(UndoEvent::Started { items: plan.restorable.len(), bytes: plan.bytes() });
    let mut report = UndoReport { unresolved: plan.unresolved, ..UndoReport::default() };
    let mut appender = env.journal.appender().ok();
    for r in &plan.restorable {
        let outcome = if cancel.load(Ordering::Relaxed) {
            Err("stopped before this one".to_string())
        } else if !r.from.starts_with(env.safety.trash_dir()) {
            Err(format!("{} is not inside the Trash", r.from.display()))
        } else if let Err(refusal) = env.safety.check_restore_target(&r.to) {
            Err(format!("it would be put back somewhere it should not go ({refusal})"))
        } else if plan.strict && r.to.parent().is_some_and(|folder| !folder.is_dir()) {
            // Out of order, a newer run may be what moved the folder, and making it again would block that run's undo.
            Err(format!("its folder {} is not there now: put back what came after it first", r.to.parent().unwrap_or(&r.to).display()))
        } else {
            trashy::restore(&r.from, &r.to)
        };
        match outcome {
            Ok(()) => {
                if let Some(app) = appender.as_mut() {
                    let _ = app.restored(plan.run, &r.to);
                }
                report.restored.push(r.clone());
            }
            Err(err) => report.failed.push((r.clone(), err)),
        }
        on_event(UndoEvent::Item);
    }
    if let Some(app) = appender.as_mut() {
        let _ = app.sync();
    }
    on_event(UndoEvent::Finished(report.clone()));
    report
}

// ---------------------------------------------------------------- emptying the Trash

#[derive(Clone, Debug, Default)]
pub struct EmptyReport {
    pub removed: usize,
    pub bytes: u64,
    pub kept: Vec<(PathBuf, String)>,
    /// What went, so the screen can drop exactly those from its list of the Trash.
    pub gone: Vec<PathBuf>,
}

/// What an emptying takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmptyScope {
    /// Everything in the Trash, what is yours included.
    Everything,
    /// Only what begone moved there, known by the very file it is, not by its name.
    OnlyBegones,
}

pub enum EmptyEvent {
    Started {
        items: usize,
        bytes: u64,
    },
    /// One entry of the Trash has been dealt with. `bytes` is what actually went: 0 for one that was kept.
    Item {
        bytes: u64,
    },
    Finished(EmptyReport),
}

/// The one place begone really deletes. It touches only direct children of the Trash, and
/// removes a symlink as the link, never what it points at.
pub fn empty_trash(env: &Env, scope: EmptyScope, cancel: &AtomicBool, on_event: &mut dyn FnMut(EmptyEvent)) -> EmptyReport {
    // What begone moved is decided here, from the history as it is now, and proved again for each entry as it goes.
    let mine: HashSet<(u64, u64)> = if scope == EmptyScope::OnlyBegones { env.journal.moved_identities() } else { HashSet::new() };
    let state = trash::summarize_marked(env.safety.trash_dir(), &mine);
    let entries: Vec<&TrashEntry> = state.entries.iter().filter(|e| scope == EmptyScope::Everything || e.begone).collect();
    on_event(EmptyEvent::Started { items: entries.len(), bytes: entries.iter().map(|e| e.bytes).sum() });
    let mut report = EmptyReport::default();
    for entry in entries {
        let mut gone = 0;
        if cancel.load(Ordering::Relaxed) {
            report.kept.push((entry.path.clone(), "stopped before this one".to_string()));
        } else {
            let removed = env.safety.check(&entry.path, Grant::InTrash).map_err(|r| r.to_string()).and_then(|real| {
                if scope == EmptyScope::OnlyBegones && !journal::identity(&real).is_some_and(|id| mine.contains(&id)) {
                    return Err("it is not something begone moved".to_string());
                }
                remove_for_good(&real)
            });
            match removed {
                Ok(()) => {
                    report.removed += 1;
                    report.bytes += entry.bytes;
                    report.gone.push(entry.path.clone());
                    gone = entry.bytes;
                }
                Err(why) => report.kept.push((entry.path.clone(), why)),
            }
        }
        on_event(EmptyEvent::Item { bytes: gone });
    }
    if report.removed > 0
        && let Ok(mut app) = env.journal.appender()
    {
        let _ = app.emptied(report.removed as u64, report.bytes);
        let _ = app.sync();
    }
    on_event(EmptyEvent::Finished(report.clone()));
    report
}

fn remove_for_good(path: &Path) -> Result<(), String> {
    let meta = path.symlink_metadata().map_err(|e| e.to_string())?;
    let result = if meta.is_dir() { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) };
    result.map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safety::tests::{TempDir, tempdir};
    use std::fs;

    fn blob(path: &Path, bytes: usize) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![7u8; bytes]).unwrap();
    }

    fn backdate(path: &Path) {
        assert!(std::process::Command::new("touch").args(["-t", "202501010000"]).arg(path).status().unwrap().success());
    }

    /// A small but realistic home folder.
    fn fake_home() -> (TempDir, Env) {
        let home = tempdir();
        blob(&home.join("Library/Caches/com.example.one/data"), 40_000);
        blob(&home.join("Library/Caches/com.example.two/data"), 20_000);
        blob(&home.join("Library/Logs/App/old.log"), 8_192);
        backdate(&home.join("Library/Logs/App/old.log"));
        blob(&home.join("Library/Logs/App/fresh.log"), 8_192);
        blob(&home.join("Library/Developer/Xcode/DerivedData/Atlas-abcdefghijklmnopqrstuvwxyzab/x"), 30_000);
        blob(&home.join("code/app/Cargo.toml"), 10);
        blob(&home.join("code/app/target/debug/x"), 50_000);
        backdate(&home.join("code/app/target"));
        backdate(&home.join("code/app/target/debug"));
        backdate(&home.join("code/app/target/debug/x"));
        blob(&home.join(".Trash/old.pdf"), 16_384);
        blob(&home.join("Documents/thesis.txt"), 100);
        let env = Env::new(home.to_path_buf());
        (home, env)
    }

    fn scan_all(env: &Env) -> (Vec<ScanItem>, Option<TrashState>, ScanSummary) {
        let (mut items, mut trash, mut summary) = (Vec::new(), None, ScanSummary::default());
        scan(env, &AtomicBool::new(false), &mut |e| match e {
            ScanEvent::Found(i) => items.push(*i),
            ScanEvent::Trash(t) => trash = Some(t),
            ScanEvent::Finished(s) => summary = s,
            _ => {}
        });
        (items, trash, summary)
    }

    fn run_delete(env: &Env, items: &[ScanItem]) -> Report {
        delete(env, items, &AtomicBool::new(false), &mut |_| {})
    }

    #[test]
    fn a_scan_finds_each_kind_and_sizes_it() {
        let (_home, env) = fake_home();
        let (items, trash, summary) = scan_all(&env);
        let of = |k| items.iter().filter(|i| i.kind == k).count();
        assert_eq!((of(Kind::Caches), of(Kind::Logs), of(Kind::Xcode), of(Kind::Developer)), (2, 1, 1, 1), "fresh.log is under a week old");
        assert!(items.iter().all(|i| i.size > 0 && i.files > 0));
        assert_eq!(trash.unwrap().entries.len(), 1);
        assert_eq!(summary.items, 5);
        assert_eq!(summary.bytes, items.iter().map(|i| i.size).sum::<u64>());
    }

    /// A home with the places package managers keep what they downloaded, old enough that nothing is at work in them, and the
    /// things that sit beside those places and must never be touched.
    fn home_with_tool_caches() -> (TempDir, Env) {
        let home = tempdir();
        for rel in [
            ".npm/_cacache/content-v2/a",
            ".cargo/registry/src/index.crates.io-abc/serde-1.0/lib.rs",
            ".cargo/registry/cache/index.crates.io-abc/serde-1.0.crate",
            ".gradle/caches/modules-2/x.jar",
            ".cache/uv/wheels/y.whl",
            ".cache/mystery/z",
        ] {
            blob(&home.join(rel), 20_000);
        }
        for rel in
            [".cargo/bin/cargo", ".cargo/credentials.toml", ".cargo/config.toml", ".gradle/gradle.properties", ".npmrc", ".npm/_logs/x.log"]
        {
            blob(&home.join(rel), 100);
        }
        for dir in [".npm", ".cargo", ".gradle", ".cache"] {
            let mut stack = vec![home.join(dir)];
            while let Some(path) = stack.pop() {
                if path.is_dir() {
                    stack.extend(fs::read_dir(&path).unwrap().flatten().map(|e| e.path()));
                }
                backdate(&path);
            }
        }
        let env = no_apps_running(Env::new(home.to_path_buf()));
        (home, env)
    }

    #[test]
    fn package_manager_caches_are_found_trashed_and_restored_and_what_sits_beside_them_is_never_touched() {
        let (home, env) = home_with_tool_caches();
        let (items, _, _) = scan_all(&env);
        let tools: Vec<&ScanItem> = items.iter().filter(|i| i.grant == Grant::ToolCache).collect();
        let mut titles: Vec<&str> = tools.iter().map(|i| i.title.as_str()).collect();
        titles.sort_unstable();
        assert_eq!(titles, ["Cargo downloads", "Cargo sources", "Gradle caches", "mystery", "npm cache", "uv"]);
        assert!(tools.iter().all(|i| i.kind == Kind::Caches && i.size >= 20_000));
        let chosen: Vec<&str> = tools.iter().filter(|i| i.pre_selected).map(|i| i.title.as_str()).collect();
        assert_eq!(chosen.len(), 5, "everything but the folder begone does not know: {chosen:?}");
        for item in &tools {
            assert!(env.safety.check(&item.path, item.grant).is_ok(), "{} would be refused", item.path.display());
        }
        let moving: Vec<ScanItem> = tools.iter().map(|i| (*i).clone()).collect();
        let report = run_delete(&env, &moving);
        assert_eq!(report.moved_count(), 6, "{:?}", report.results.iter().map(|r| format!("{:?}", r.outcome)).collect::<Vec<_>>());
        for gone in [".npm/_cacache", ".cargo/registry/src", ".cargo/registry/cache", ".gradle/caches", ".cache/uv", ".cache/mystery"] {
            assert!(!home.join(gone).exists(), "{gone}");
        }
        for stays in
            [".cargo/bin/cargo", ".cargo/credentials.toml", ".cargo/config.toml", ".gradle/gradle.properties", ".npmrc", ".npm/_logs/x.log"]
        {
            assert!(home.join(stays).exists(), "{stays} is beside a cache, not in one");
        }
        let plan = env.journal.last_undo().unwrap();
        let undone = undo(&env, &plan, &AtomicBool::new(false), &mut |_| {});
        assert_eq!((undone.restored.len(), undone.failed.len()), (6, 0), "{:?}", undone.failed);
        assert_eq!(fs::read(home.join(".cargo/registry/cache/index.crates.io-abc/serde-1.0.crate")).unwrap(), vec![7u8; 20_000]);
        assert!(home.join(".gradle/caches/modules-2/x.jar").exists() && home.join(".cache/mystery/z").exists());
    }

    #[test]
    fn a_kept_place_is_shown_by_the_scan_but_never_moved_even_when_it_is_asked_for() {
        let (home, env) = fake_home();
        env.safety.set_kept(vec![home.join("Library/Caches/com.example.one"), home.join("code/app")]);
        let (items, _, _) = scan_all(&env);
        let kept: Vec<&str> = items.iter().filter(|i| i.kept).map(|i| i.title.as_str()).collect();
        assert_eq!(kept.len(), 2, "{kept:?}: the cache, and the build folder inside the project that is kept");
        assert!(items.iter().any(|i| i.title == "com.example.two" && !i.kept));
        // Everything is asked for, kept rows included, as a forged request would.
        let report = run_delete(&env, &items);
        let outcome = |title: &str| &report.results.iter().find(|r| r.title == title).unwrap().outcome;
        assert!(matches!(outcome("com.example.one"), Outcome::Refused(w) if w.contains("keep")), "{:?}", outcome("com.example.one"));
        assert!(matches!(outcome("target/"), Outcome::Refused(w) if w.contains("keep")));
        assert!(matches!(outcome("com.example.two"), Outcome::Moved { .. }));
        assert!(home.join("Library/Caches/com.example.one/data").exists() && home.join("code/app/target/debug/x").exists());
        assert!(!home.join("Library/Caches/com.example.two").exists());
    }

    #[test]
    fn a_tool_cache_being_written_to_right_now_is_left_unchecked() {
        let (home, env) = home_with_tool_caches();
        blob(&home.join(".npm/_cacache/content-v2/fresh"), 4096);
        let (items, _, _) = scan_all(&env);
        let npm = items.iter().find(|i| i.title == "npm cache").unwrap();
        assert_eq!((npm.tag, npm.pre_selected), (Tag::InUse, false));
        assert!(npm.evidence.iter().any(|e| !e.ok && e.text.contains("Used recently")));
        let cargo = items.iter().find(|i| i.title == "Cargo sources").unwrap();
        assert!(cargo.pre_selected, "a tool that is not at work does not hold the others back");
    }

    #[test]
    fn every_row_a_scan_shows_can_actually_be_removed() {
        // The first version showed 8.9 GiB of rows that were refused at delete time.
        let (_home, env) = fake_home();
        let (items, _, _) = scan_all(&env);
        for item in &items {
            assert!(env.safety.check(&item.path, item.grant).is_ok(), "{} would be refused", item.path.display());
        }
    }

    #[test]
    fn build_output_built_today_is_called_in_use_and_left_unchecked() {
        let (home, env) = fake_home();
        blob(&home.join("code/app/target/debug/fresh"), 4096);
        let (items, _, _) = scan_all(&env);
        let target = items.iter().find(|i| i.kind == Kind::Developer).unwrap();
        assert_eq!((target.tag, target.pre_selected), (Tag::InUse, false));
    }

    #[test]
    fn deleting_moves_to_the_trash_and_writes_the_history_first() {
        let (home, env) = fake_home();
        let (items, _, _) = scan_all(&env);
        let chosen: Vec<ScanItem> = items.into_iter().filter(|i| i.kind != Kind::Developer).collect();
        let report = run_delete(&env, &chosen);
        assert_eq!(report.moved_count(), chosen.len());
        assert!(!home.join("Library/Caches/com.example.one").exists());
        assert!(home.join(".Trash/com.example.one").exists());
        assert!(home.join("Documents/thesis.txt").exists(), "nothing outside the chosen rows is touched");
        let plan = env.journal.last_undo().unwrap();
        assert_eq!(plan.restorable.len(), chosen.len());
    }

    #[test]
    fn a_row_that_turned_into_a_symlink_is_refused_and_its_target_survives() {
        let (home, env) = fake_home();
        let (items, _, _) = scan_all(&env);
        let victim = items.iter().find(|i| i.path.ends_with("com.example.one")).unwrap().clone();
        fs::remove_dir_all(&victim.path).unwrap();
        std::os::unix::fs::symlink(home.join("Documents"), &victim.path).unwrap();
        let report = run_delete(&env, &[victim]);
        assert!(matches!(report.results[0].outcome, Outcome::Refused(_)));
        assert!(home.join("Documents/thesis.txt").exists());
    }

    #[test]
    fn a_vanished_row_is_reported_as_refused_not_as_freed() {
        let (home, env) = fake_home();
        let (items, _, _) = scan_all(&env);
        let gone = items.iter().find(|i| i.path.ends_with("com.example.two")).unwrap().clone();
        fs::remove_dir_all(&gone.path).unwrap();
        let report = run_delete(&env, &[gone]);
        assert!(matches!(report.results[0].outcome, Outcome::Refused(_)));
        assert_eq!(report.moved_bytes(), 0);
        let _ = home;
    }

    #[test]
    fn a_stale_proof_is_rerun_so_a_changed_folder_is_not_taken() {
        let (home, env) = fake_home();
        let (items, _, _) = scan_all(&env);
        let target = items.iter().find(|i| i.kind == Kind::Developer).unwrap().clone();
        fs::remove_file(home.join("code/app/Cargo.toml")).unwrap();
        let report = run_delete(&env, &[target]);
        assert!(matches!(report.results[0].outcome, Outcome::Refused(_)));
        assert!(home.join("code/app/target").exists());
    }

    #[test]
    fn without_a_working_history_nothing_is_moved() {
        let (home, env) = fake_home();
        let (items, _, _) = scan_all(&env);
        // A file where the history folder must go makes the journal impossible to open.
        fs::write(home.join(".local"), b"in the way").unwrap();
        let report = run_delete(&env, &items);
        assert_eq!(report.moved_count(), 0);
        assert!(items.iter().all(|i| i.path.exists()), "fail closed: everything is still where it was");
        assert!(report.warnings.iter().any(|w| w.contains("undo history")));
    }

    #[test]
    fn stopping_leaves_the_rest_alone() {
        let (_home, env) = fake_home();
        let (items, _, _) = scan_all(&env);
        let cancel = AtomicBool::new(false);
        let mut seen = 0;
        let report = delete(&env, &items, &cancel, &mut |e| {
            if let RunEvent::Item(_) = e {
                seen += 1;
                if seen == 2 {
                    cancel.store(true, Ordering::Relaxed);
                }
            }
        });
        assert!(report.cancelled);
        assert_eq!(report.moved_count(), 2);
        assert!(report.results[2..].iter().all(|r| r.outcome == Outcome::NotAttempted));
    }

    #[test]
    fn undo_puts_everything_back_and_then_offers_the_older_run() {
        let (home, env) = fake_home();
        let (items, _, _) = scan_all(&env);
        let first: Vec<ScanItem> = items.iter().filter(|i| i.kind == Kind::Logs).cloned().collect();
        let second: Vec<ScanItem> = items.iter().filter(|i| i.kind == Kind::Caches).cloned().collect();
        run_delete(&env, &first);
        std::thread::sleep(Duration::from_millis(5));
        run_delete(&env, &second);

        let plan = env.journal.last_undo().unwrap();
        assert_eq!(plan.restorable.len(), second.len());
        let report = undo(&env, &plan, &AtomicBool::new(false), &mut |_| {});
        assert_eq!((report.restored.len(), report.failed.len()), (second.len(), 0));
        assert!(home.join("Library/Caches/com.example.one/data").exists());

        let older = env.journal.last_undo().unwrap();
        assert_eq!(older.restorable.len(), first.len(), "restoring a run makes the one before it reachable");
        undo(&env, &older, &AtomicBool::new(false), &mut |_| {});
        assert!(home.join("Library/Logs/App/old.log").exists());
        assert!(env.journal.last_undo().is_none());
    }

    #[test]
    fn undo_refuses_to_clobber_something_that_came_back() {
        let (home, env) = fake_home();
        let (items, _, _) = scan_all(&env);
        let cache = items.iter().find(|i| i.path.ends_with("com.example.one")).unwrap().clone();
        run_delete(&env, std::slice::from_ref(&cache));
        blob(&cache.path.join("new-data"), 10);
        let plan = env.journal.last_undo().unwrap();
        let report = undo(&env, &plan, &AtomicBool::new(false), &mut |_| {});
        assert_eq!(report.failed.len(), 1);
        assert!(report.failed[0].1.contains("already exists"));
        assert!(home.join(".Trash/com.example.one/data").exists(), "the trashed copy stays put");
        assert!(env.journal.last_undo().is_some(), "and it can be retried");
    }

    #[test]
    fn a_tampered_history_cannot_move_things_into_place_or_out_of_the_trash() {
        let (home, env) = fake_home();
        blob(&home.join("elsewhere/secret"), 10);
        let mut app = env.journal.appender().unwrap();
        app.intent(9, Path::new("/etc/evil"), 1, Kind::Logs).unwrap();
        app.moved(9, Path::new("/etc/evil"), Some(&home.join(".Trash/x"))).unwrap();
        app.intent(9, &home.join("Library/Caches/ok"), 1, Kind::Caches).unwrap();
        app.moved(9, &home.join("Library/Caches/ok"), Some(&home.join("elsewhere/secret"))).unwrap();
        let plan = env.journal.last_undo().unwrap();
        let report = undo(&env, &plan, &AtomicBool::new(false), &mut |_| {});
        assert_eq!(report.restored.len(), 0);
        assert_eq!(report.failed.len(), 2);
        assert!(home.join("elsewhere/secret").exists());
    }

    #[test]
    fn emptying_the_trash_removes_its_contents_and_nothing_else() {
        let (home, env) = fake_home();
        fs::create_dir_all(home.join(".Trash/folder/sub")).unwrap();
        fs::write(home.join(".Trash/folder/sub/f"), b"x").unwrap();
        std::os::unix::fs::symlink(home.join("Documents"), home.join(".Trash/link")).unwrap();
        let report = empty_trash(&env, EmptyScope::Everything, &AtomicBool::new(false), &mut |_| {});
        assert_eq!(report.removed, 3);
        assert!(report.kept.is_empty(), "{:?}", report.kept);
        assert_eq!(fs::read_dir(home.join(".Trash")).unwrap().count(), 0);
        assert!(home.join("Documents/thesis.txt").exists(), "a symlink is removed as the link, never followed");
        assert!(home.join("Library/Caches/com.example.one").exists());
        assert!(fs::read_to_string(env.journal.path()).unwrap().contains("emptied"));
    }

    #[test]
    fn each_emptied_entry_reports_the_bytes_that_went_so_a_count_down_can_follow_the_real_progress() {
        let (home, env) = fake_home();
        blob(&home.join(".Trash/big/data"), 30_000);
        blob(&home.join(".Trash/small"), 5_000);
        blob(&home.join(".Trash/medium/data"), 7_000);
        let mut events = Vec::new();
        let cancel = AtomicBool::new(false);
        let report = empty_trash(&env, EmptyScope::Everything, &cancel, &mut |e| events.push(e));
        let started =
            events.iter().find_map(|e| if let EmptyEvent::Started { items, bytes } = e { Some((*items, *bytes)) } else { None }).unwrap();
        let sent: Vec<u64> = events.iter().filter_map(|e| if let EmptyEvent::Item { bytes } = e { Some(*bytes) } else { None }).collect();
        assert!(started.0 >= 3, "the three just added, and whatever the fixture already had");
        assert_eq!(sent.len(), started.0, "one event per entry");
        assert_eq!(sent.iter().sum::<u64>(), started.1, "everything that was there went, so the sizes add up to the total");
        assert_eq!(sent.iter().sum::<u64>(), report.bytes);
        assert!(matches!(events.last(), Some(EmptyEvent::Finished(_))));

        // An entry that is not removed reports nothing gone.
        blob(&home.join(".Trash/stopped/data"), 9_000);
        let mut sent = Vec::new();
        let stop = AtomicBool::new(true);
        empty_trash(&env, EmptyScope::Everything, &stop, &mut |e| {
            if let EmptyEvent::Item { bytes } = e {
                sent.push(bytes);
            }
        });
        assert!(!sent.is_empty() && sent.iter().all(|b| *b == 0), "stopped before them, so none of it went: {sent:?}");
    }

    // ------------------------------------------------------------------------------ apps

    use crate::apps::{self, AppEntry, AppsEvent};
    use crate::safety::tests::{fake_app, leave};

    /// A scratch home with one installed app and what it left, and the entry the Apps scan builds for it.
    fn home_with_app() -> (TempDir, Env, AppEntry) {
        let home = tempdir();
        let lib = home.join("Library");
        fake_app(&home.join("Applications"), "Slack", "com.tinyspeck.slackmacgap", 5000);
        leave(&lib.join("Caches/com.tinyspeck.slackmacgap"), 3000, true);
        leave(&lib.join("Preferences/com.tinyspeck.slackmacgap.plist"), 100, false);
        leave(&lib.join("Saved Application State/com.tinyspeck.slackmacgap.savedState"), 200, true);
        leave(&lib.join("Containers/com.tinyspeck.slackmacgap"), 7000, true);
        leave(&lib.join("Application Support/Slack"), 9000, true);
        let env = no_apps_running(Env::new(home.to_path_buf()));
        let entry = scan_app(&env, Procs::none());
        (home, env, entry)
    }

    /// Whatever the machine running the tests has open, a scratch home's apps are not running unless a test says so.
    fn no_apps_running(mut env: Env) -> Env {
        env.procs_override = Some(Procs::none());
        env
    }

    fn scan_app(env: &Env, procs: Procs) -> AppEntry {
        let mut found = Vec::new();
        apps::scan(&env.safety, &procs, &apps::AppsOptions::default(), &AtomicBool::new(false), &mut |e| {
            if let AppsEvent::Found(entry) = e {
                found.push(*entry);
            }
        });
        assert_eq!(found.len(), 1);
        found.remove(0)
    }

    #[test]
    fn an_app_goes_to_the_trash_with_its_traces_and_comes_back_in_one_undo() {
        let (home, env, entry) = home_with_app();
        assert_eq!(entry.items.len(), 4, "the bundle, its caches, its settings, its saved state");
        let report = delete(&env, &entry.items, &AtomicBool::new(false), &mut |_| {});
        assert_eq!(report.moved_count(), 4, "{:?}", report.kept().iter().map(|r| (&r.title, &r.outcome)).collect::<Vec<_>>());
        assert!(!home.join("Applications/Slack.app").exists());
        assert!(!home.join("Library/Caches/com.tinyspeck.slackmacgap").exists());
        assert!(!home.join("Library/Preferences/com.tinyspeck.slackmacgap.plist").exists());
        assert!(home.join("Library/Containers/com.tinyspeck.slackmacgap/data").exists(), "your data is left where it was");
        assert!(home.join("Library/Application Support/Slack/data").exists());
        assert!(home.join(".Trash/Slack.app/Contents/Info.plist").exists());
        assert!(report.results.iter().all(|r| r.kind == Kind::Apps));
        // One undo puts everything back, bundle included, byte for byte.
        let plan = env.journal.last_undo().unwrap();
        assert_eq!(plan.restorable.len(), 4);
        let undone = undo(&env, &plan, &AtomicBool::new(false), &mut |_| {});
        assert_eq!((undone.restored.len(), undone.failed.len()), (4, 0), "{:?}", undone.failed);
        assert!(home.join("Applications/Slack.app/Contents/Info.plist").exists());
        assert_eq!(fs::read(home.join("Applications/Slack.app/Contents/MacOS/Slack")).unwrap(), vec![7u8; 5000]);
        assert_eq!(fs::read(home.join("Library/Caches/com.tinyspeck.slackmacgap/data")).unwrap(), vec![3u8; 3000]);
        assert!(home.join("Library/Preferences/com.tinyspeck.slackmacgap.plist").exists());
    }

    #[test]
    fn a_running_app_is_refused_and_so_are_its_traces() {
        let (home, mut env, entry) = home_with_app();
        env.procs_override = Some(Procs::with(&[("com.tinyspeck.slackmacgap", "Slack")]));
        let report = delete(&env, &entry.items, &AtomicBool::new(false), &mut |_| {});
        assert_eq!(report.moved_count(), 0);
        let why: Vec<String> = report.results.iter().map(|r| format!("{:?}", r.outcome)).collect();
        assert!(why[0].contains("running"), "{why:?}");
        assert!(why[1..].iter().all(|w| w.contains("still installed")), "{why:?}");
        assert!(home.join("Applications/Slack.app").exists() && home.join("Library/Caches/com.tinyspeck.slackmacgap/data").exists());
    }

    #[test]
    fn if_the_bundle_cannot_move_its_traces_stay_so_a_half_removed_app_is_never_left() {
        let (home, env, entry) = home_with_app();
        let apps_dir = home.join("Applications");
        fs::set_permissions(&apps_dir, std::os::unix::fs::PermissionsExt::from_mode(0o555)).unwrap();
        let report = delete(&env, &entry.items, &AtomicBool::new(false), &mut |_| {});
        fs::set_permissions(&apps_dir, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        assert_eq!(report.moved_count(), 0, "{:?}", report.results.iter().map(|r| format!("{:?}", r.outcome)).collect::<Vec<_>>());
        assert!(matches!(report.results[0].outcome, Outcome::Failed(_)));
        assert!(report.results[1..].iter().all(|r| matches!(&r.outcome, Outcome::Refused(why) if why.contains("still installed"))));
        assert!(home.join("Applications/Slack.app").exists());
        assert!(home.join("Library/Caches/com.tinyspeck.slackmacgap/data").exists(), "its settings and caches are exactly where they were");
    }

    #[test]
    fn what_changed_since_the_scan_is_caught_when_the_app_is_removed() {
        let (home, env, entry) = home_with_app();
        // The bundle now says it is a different app, and a cache has become a symlink.
        fs::write(
            home.join("Applications/Slack.app/Contents/Info.plist"),
            "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>CFBundleIdentifier</key><string>com.other.app</string></dict></plist>",
        )
        .unwrap();
        let cache = home.join("Library/Caches/com.tinyspeck.slackmacgap");
        fs::remove_dir_all(&cache).unwrap();
        std::os::unix::fs::symlink(home.join("Documents"), &cache).unwrap();
        let report = delete(&env, &entry.items, &AtomicBool::new(false), &mut |_| {});
        assert_eq!(report.moved_count(), 2, "only the settings and the saved state, which are still exactly what they were");
        assert!(matches!(&report.results[0].outcome, Outcome::Refused(w) if w.contains("identifier")), "{:?}", report.results[0].outcome);
        assert!(matches!(&report.results[1].outcome, Outcome::Refused(w) if w.contains("symlink")), "{:?}", report.results[1].outcome);
    }

    #[test]
    fn an_item_cannot_talk_its_way_past_the_app_gates() {
        let (home, env, entry) = home_with_app();
        let forged = |path: PathBuf, grant: Grant, owner: Option<&str>| {
            let mut item = entry.items[1].clone();
            item.path = path;
            item.grant = grant;
            item.owner = owner.map(str::to_string);
            item
        };
        let items = vec![
            forged(home.join("Library/Application Support/Slack"), Grant::AppData, Some("com.tinyspeck.slackmacgap")),
            forged(home.join("Library/Containers/com.tinyspeck.slackmacgap"), Grant::AppData, Some("com.tinyspeck.slackmacgap")),
            forged(home.join("Library/Caches/com.tinyspeck.slackmacgap"), Grant::AppData, None),
            forged(home.join("Library/Caches/com.tinyspeck.slackmacgap"), Grant::AppData, Some("com.apple.Safari")),
            forged(home.join("Library"), Grant::AppBundle, Some("com.tinyspeck.slackmacgap")),
            forged(home.join("Applications"), Grant::AppBundle, Some("com.tinyspeck.slackmacgap")),
            forged(PathBuf::from("/Applications/Safari.app"), Grant::AppBundle, Some("com.apple.Safari")),
        ];
        let report = delete(&env, &items, &AtomicBool::new(false), &mut |_| {});
        assert_eq!(report.moved_count(), 0, "{:?}", report.results.iter().map(|r| format!("{:?}", r.outcome)).collect::<Vec<_>>());
        assert!(report.results.iter().all(|r| matches!(r.outcome, Outcome::Refused(_))));
        assert!(
            home.join("Library/Application Support/Slack/data").exists()
                && home.join("Library/Containers/com.tinyspeck.slackmacgap/data").exists()
        );
    }

    /// A scratch home with an old installer, an iPhone backup and the other things that sit beside them, and what the Files
    /// tab would list.
    fn home_with_files() -> (TempDir, Env, Vec<ScanItem>) {
        let home = tempdir();
        blob(&home.join("Downloads/Slack.dmg"), 30_000);
        blob(&home.join("Downloads/thesis.pdf"), 20_000);
        blob(&home.join("Downloads/photos.zip"), 20_000);
        let backup = home.join("Library/Application Support/MobileSync/Backup/00008110-ABC");
        blob(&backup.join("Manifest.db"), 40_000);
        fs::write(
            backup.join("Info.plist"),
            "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>Device Name</key><string>Abhi's iPhone</string><key>Product Type</key><string>iPhone15,2</string></dict></plist>",
        )
        .unwrap();
        blob(&home.join("Library/Application Support/Other App/data"), 10_000);
        let env = no_apps_running(Env::new(home.to_path_buf()));
        let mut items = Vec::new();
        crate::files::scan(&env.safety, &crate::files::FilesOptions::default(), &AtomicBool::new(false), &mut |e| {
            if let crate::files::FilesEvent::Found(item) = e {
                items.push(*item);
            }
        });
        (home, env, items)
    }

    #[test]
    fn an_installer_and_a_device_backup_go_to_the_trash_and_come_back_in_one_undo_and_what_sits_beside_them_never_moves() {
        let (home, env, items) = home_with_files();
        assert_eq!(items.len(), 2, "{:?}", items.iter().map(|i| &i.title).collect::<Vec<_>>());
        let report = run_delete(&env, &items);
        assert_eq!(report.moved_count(), 2, "{:?}", report.results.iter().map(|r| format!("{:?}", r.outcome)).collect::<Vec<_>>());
        assert!(report.results.iter().all(|r| r.kind == Kind::Files));
        assert!(
            !home.join("Downloads/Slack.dmg").exists() && !home.join("Library/Application Support/MobileSync/Backup/00008110-ABC").exists()
        );
        assert!(home.join(".Trash/Slack.dmg").exists() && home.join(".Trash/00008110-ABC/Info.plist").exists());
        for stays in ["Downloads/thesis.pdf", "Downloads/photos.zip", "Library/Application Support/Other App/data"] {
            assert!(home.join(stays).exists(), "{stays} is not one of the files begone offers");
        }
        let plan = env.journal.last_undo().unwrap();
        let undone = undo(&env, &plan, &AtomicBool::new(false), &mut |_| {});
        assert_eq!((undone.restored.len(), undone.failed.len()), (2, 0), "{:?}", undone.failed);
        assert_eq!(fs::read(home.join("Downloads/Slack.dmg")).unwrap(), vec![7u8; 30_000]);
        assert_eq!(
            fs::read(home.join("Library/Application Support/MobileSync/Backup/00008110-ABC/Manifest.db")).unwrap(),
            vec![7u8; 40_000]
        );
    }

    #[test]
    fn a_big_old_download_goes_to_the_trash_and_comes_back_and_one_that_changed_since_the_scan_is_refused() {
        let (home, mut env, _) = home_with_files();
        env.safety = env.safety.clone().with_big_download(50_000, Duration::from_secs(3_600));
        blob(&home.join("Downloads/Movie.mov"), 60_000);
        backdate(&home.join("Downloads/Movie.mov"));
        blob(&home.join("Downloads/Other.mov"), 60_000);
        backdate(&home.join("Downloads/Other.mov"));
        let mut items = Vec::new();
        crate::files::scan(&env.safety, &crate::files::FilesOptions::default(), &AtomicBool::new(false), &mut |e| {
            if let crate::files::FilesEvent::Found(item) = e {
                items.push(*item);
            }
        });
        let bigs: Vec<ScanItem> = items.into_iter().filter(|i| i.grant == Grant::BigDownload).collect();
        assert_eq!(bigs.len(), 2);
        // One is written to after the scan: it is not what was listed any more.
        blob(&home.join("Downloads/Other.mov"), 60_000);
        let report = run_delete(&env, &bigs);
        let by = |t: &str| report.results.iter().find(|r| r.title == t).unwrap();
        assert!(matches!(by("Movie.mov").outcome, Outcome::Moved { .. }), "{:?}", by("Movie.mov").outcome);
        assert!(matches!(&by("Other.mov").outcome, Outcome::Refused(w) if w.contains("too recently")), "{:?}", by("Other.mov").outcome);
        assert!(!home.join("Downloads/Movie.mov").exists() && home.join("Downloads/Other.mov").exists());
        let plan = env.journal.last_undo().unwrap();
        let undone = undo(&env, &plan, &AtomicBool::new(false), &mut |_| {});
        assert_eq!((undone.restored.len(), undone.failed.len()), (1, 0), "{:?}", undone.failed);
        assert_eq!(fs::read(home.join("Downloads/Movie.mov")).unwrap(), vec![7u8; 60_000]);
    }

    #[test]
    fn a_file_that_stopped_being_what_was_listed_is_refused_when_it_would_move() {
        let (home, env, items) = home_with_files();
        // The installer was replaced by a link to something that matters, and the backup lost its Info.plist.
        fs::remove_file(home.join("Downloads/Slack.dmg")).unwrap();
        std::os::unix::fs::symlink(home.join("Downloads/thesis.pdf"), home.join("Downloads/Slack.dmg")).unwrap();
        fs::remove_file(home.join("Library/Application Support/MobileSync/Backup/00008110-ABC/Info.plist")).unwrap();
        let report = run_delete(&env, &items);
        assert_eq!(report.moved_count(), 0, "{:?}", report.results.iter().map(|r| format!("{:?}", r.outcome)).collect::<Vec<_>>());
        assert!(report.results.iter().all(|r| matches!(r.outcome, Outcome::Refused(_))));
        assert!(
            home.join("Downloads/thesis.pdf").exists()
                && home.join("Library/Application Support/MobileSync/Backup/00008110-ABC/Manifest.db").exists()
        );
        // And a kept one is never moved, whoever asks.
        let (home, env, items) = home_with_files();
        env.safety.set_kept(vec![home.join("Downloads")]);
        let report = run_delete(&env, &items);
        let slack = report.results.iter().find(|r| r.title == "Slack.dmg").unwrap();
        assert!(matches!(&slack.outcome, Outcome::Refused(w) if w.contains("keep")), "{:?}", slack.outcome);
        assert!(home.join("Downloads/Slack.dmg").exists());
    }

    #[test]
    fn a_duplicate_copy_goes_to_the_trash_leaving_the_original_and_the_last_copy_is_never_taken() {
        let home = tempdir();
        // Two identical files (blob writes the same byte), one in Downloads and one in Documents.
        blob(&home.join("Downloads/clip copy.mov"), 40_000);
        blob(&home.join("Documents/clip.mov"), 40_000);
        let env = no_apps_running(Env::new(home.to_path_buf()));
        let options = crate::files::FilesOptions {
            dupes: crate::dupes::DupeOptions { min_size: 10, ..Default::default() },
            ..crate::files::FilesOptions::default()
        };
        let mut items = Vec::new();
        crate::files::scan(&env.safety, &options, &AtomicBool::new(false), &mut |e| {
            if let crate::files::FilesEvent::Found(item) = e {
                items.push(*item);
            }
        });
        let dups: Vec<ScanItem> = items.into_iter().filter(|i| i.grant == Grant::DuplicateCopy).collect();
        assert_eq!(dups.len(), 1, "the Downloads copy is offered; the Documents original is kept");
        assert_eq!(dups[0].title, "clip copy.mov");
        let report = run_delete(&env, &dups);
        assert_eq!(report.moved_count(), 1);
        assert!(!home.join("Downloads/clip copy.mov").exists(), "the copy moved");
        assert!(home.join("Documents/clip.mov").exists(), "the original is untouched");
        assert!(home.join(".Trash/clip copy.mov").exists());
        // Undo brings the copy back, byte for byte.
        let plan = env.journal.last_undo().unwrap();
        let undone = undo(&env, &plan, &AtomicBool::new(false), &mut |_| {});
        assert_eq!((undone.restored.len(), undone.failed.len()), (1, 0), "{:?}", undone.failed);
        assert_eq!(fs::read(home.join("Downloads/clip copy.mov")).unwrap(), vec![7u8; 40_000]);
        // With the original gone, the very same copy is refused at the moment it would move: the last copy is never taken.
        fs::remove_file(home.join("Documents/clip.mov")).unwrap();
        let report = run_delete(&env, &dups);
        assert!(matches!(&report.results[0].outcome, Outcome::Refused(w) if w.contains("original")), "{:?}", report.results[0].outcome);
        assert!(home.join("Downloads/clip copy.mov").exists(), "nothing was taken");
    }

    /// A scratch home with a fake Xcode and what a real one leaves around it, and the entry the Apps scan builds for it.
    fn home_with_xcode() -> (TempDir, Env, AppEntry) {
        let home = tempdir();
        let lib = home.join("Library");
        fake_app(&home.join("Applications"), "Xcode", "com.apple.dt.Xcode", 9000);
        leave(&lib.join("Caches/com.apple.dt.Xcode"), 3000, true);
        leave(&lib.join("Preferences/com.apple.dt.Xcode.plist"), 100, false);
        leave(&lib.join("Developer/Xcode/DerivedData/Project-abc"), 5000, true);
        leave(&lib.join("Developer/CoreSimulator/Devices/ABC"), 8000, true);
        let env = no_apps_running(Env::new(home.to_path_buf()));
        let entry = scan_app(&env, Procs::none());
        (home, env, entry)
    }

    /// A scratch home where an app was removed and left its traces, and the entry the Apps scan builds for what is left.
    fn home_with_leftovers() -> (TempDir, Env, AppEntry) {
        let home = tempdir();
        let lib = home.join("Library");
        leave(&lib.join("Caches/com.gone.app"), 3000, true);
        leave(&lib.join("Preferences/com.gone.app.plist"), 100, false);
        leave(&lib.join("Containers/com.gone.app"), 7000, true);
        leave(&lib.join("Application Support/com.gone.app"), 9000, true);
        let env = no_apps_running(Env::new(home.to_path_buf()));
        let entry = scan_app(&env, Procs::none());
        (home, env, entry)
    }

    #[test]
    fn what_an_app_left_behind_goes_to_the_trash_and_comes_back_in_one_undo_and_what_the_app_kept_stays() {
        let (home, env, entry) = home_with_leftovers();
        assert!(entry.leftover && entry.items.len() == 2, "the caches and the settings, and no bundle");
        let report = delete(&env, &entry.items, &AtomicBool::new(false), &mut |_| {});
        assert_eq!(report.moved_count(), 2, "{:?}", report.results.iter().map(|r| format!("{:?}", r.outcome)).collect::<Vec<_>>());
        assert!(!home.join("Library/Caches/com.gone.app").exists() && !home.join("Library/Preferences/com.gone.app.plist").exists());
        assert!(home.join(".Trash/com.gone.app/data").exists());
        assert!(home.join("Library/Containers/com.gone.app/data").exists(), "your data is left where it was");
        assert!(home.join("Library/Application Support/com.gone.app/data").exists());
        assert!(report.results.iter().all(|r| r.kind == Kind::Apps));
        let plan = env.journal.last_undo().unwrap();
        let undone = undo(&env, &plan, &AtomicBool::new(false), &mut |_| {});
        assert_eq!((undone.restored.len(), undone.failed.len()), (2, 0), "{:?}", undone.failed);
        assert_eq!(fs::read(home.join("Library/Caches/com.gone.app/data")).unwrap(), vec![3u8; 3000]);
        assert!(home.join("Library/Preferences/com.gone.app.plist").exists());
    }

    #[test]
    fn leftovers_are_refused_if_the_app_was_installed_again_or_is_running_before_they_move() {
        let (home, mut env, entry) = home_with_leftovers();
        // It was installed again after they were listed (and in another case, to be sure).
        fake_app(&home.join("Applications"), "Gone", "com.Gone.App", 10);
        let report = delete(&env, &entry.items, &AtomicBool::new(false), &mut |_| {});
        assert_eq!(report.moved_count(), 0);
        assert!(
            report.results.iter().all(|r| matches!(&r.outcome, Outcome::Refused(w) if w.contains("still installed"))),
            "{:?}",
            report.results.iter().map(|r| format!("{:?}", r.outcome)).collect::<Vec<_>>()
        );
        assert!(home.join("Library/Caches/com.gone.app/data").exists());
        // Or it is open now, from somewhere begone does not look.
        fs::remove_dir_all(home.join("Applications/Gone.app")).unwrap();
        env.procs_override = Some(Procs::with(&[("com.gone.app", "Gone")]));
        let report = delete(&env, &entry.items, &AtomicBool::new(false), &mut |_| {});
        assert_eq!(report.moved_count(), 0);
        assert!(report.results.iter().all(|r| matches!(&r.outcome, Outcome::Refused(w) if w.contains("running"))));
        assert!(home.join("Library/Caches/com.gone.app/data").exists());
    }

    const SHARED_WITH_APPLES_TOOLS: [&str; 4] = [
        "Library/Caches/com.apple.dt.Xcode/data",
        "Library/Preferences/com.apple.dt.Xcode.plist",
        "Library/Developer/Xcode/DerivedData/Project-abc/data",
        "Library/Developer/CoreSimulator/Devices/ABC/data",
    ];

    #[test]
    fn xcode_alone_goes_to_the_trash_and_one_undo_brings_it_back_byte_for_byte() {
        let (home, env, entry) = home_with_xcode();
        assert_eq!(entry.items.len(), 1, "the app, and nothing that Apple's tools share");
        let report = delete(&env, &entry.items, &AtomicBool::new(false), &mut |_| {});
        assert_eq!(report.moved_count(), 1, "{:?}", report.results.iter().map(|r| format!("{:?}", r.outcome)).collect::<Vec<_>>());
        assert!(!home.join("Applications/Xcode.app").exists());
        assert!(home.join(".Trash/Xcode.app/Contents/Info.plist").exists());
        for stays in SHARED_WITH_APPLES_TOOLS {
            assert!(home.join(stays).exists(), "{stays} stays where it was");
        }
        let plan = env.journal.last_undo().unwrap();
        assert_eq!(plan.restorable.len(), 1);
        let undone = undo(&env, &plan, &AtomicBool::new(false), &mut |_| {});
        assert_eq!((undone.restored.len(), undone.failed.len()), (1, 0), "{:?}", undone.failed);
        assert_eq!(fs::read(home.join("Applications/Xcode.app/Contents/MacOS/Xcode")).unwrap(), vec![7u8; 9000]);
        assert!(home.join("Applications/Xcode.app/Contents/Info.plist").exists());
    }

    #[test]
    fn the_exception_for_xcode_covers_its_bundle_and_nothing_else_of_apples() {
        let (home, env, entry) = home_with_xcode();
        let pages = fake_app(&home.join("Applications"), "Pages", "com.apple.iWork.Pages", 100);
        let forged = |path: PathBuf, grant: Grant, owner: &str| {
            let mut item = entry.items[0].clone();
            item.path = path;
            item.grant = grant;
            item.owner = Some(owner.to_string());
            item
        };
        let xcode = "com.apple.dt.Xcode";
        let items = vec![
            forged(home.join("Library/Caches/com.apple.dt.Xcode"), Grant::AppData, xcode),
            forged(home.join("Library/Preferences/com.apple.dt.Xcode.plist"), Grant::AppData, xcode),
            forged(home.join("Library/Developer/Xcode"), Grant::AppData, xcode),
            forged(home.join("Library/Developer/CoreSimulator"), Grant::AppData, xcode),
            forged(pages.clone(), Grant::AppBundle, "com.apple.iWork.Pages"),
            forged(pages.clone(), Grant::AppBundle, xcode),
        ];
        let report = delete(&env, &items, &AtomicBool::new(false), &mut |_| {});
        assert_eq!(report.moved_count(), 0, "{:?}", report.results.iter().map(|r| format!("{:?}", r.outcome)).collect::<Vec<_>>());
        assert!(report.results.iter().all(|r| matches!(r.outcome, Outcome::Refused(_))));
        assert!(pages.join("Contents/Info.plist").exists() && home.join("Applications/Xcode.app/Contents/Info.plist").exists());
        for stays in SHARED_WITH_APPLES_TOOLS {
            assert!(home.join(stays).exists(), "{stays}");
        }
    }

    #[test]
    fn a_running_xcode_is_refused_but_an_old_copy_beside_it_is_not() {
        let (home, mut env, _) = home_with_xcode();
        let old = fake_app(&home.join("Applications"), "Xcode-15.4", "com.apple.dt.Xcode", 4000);
        let mut found = Vec::new();
        apps::scan(&env.safety, &Procs::none(), &apps::AppsOptions::default(), &AtomicBool::new(false), &mut |e| {
            if let AppsEvent::Found(entry) = e {
                found.push(*entry);
            }
        });
        let by_name = |name: &str| found.iter().find(|e| e.identity.name == name).unwrap_or_else(|| panic!("{name} was not found"));
        let new = home.join("Applications/Xcode.app");
        env.procs_override = Some(Procs::from_ps(&format!("{}/Contents/MacOS/Xcode -psn_0_1\n", new.display())));
        let refused = delete(&env, &by_name("Xcode").items, &AtomicBool::new(false), &mut |_| {});
        assert_eq!(refused.moved_count(), 0);
        assert!(matches!(&refused.results[0].outcome, Outcome::Refused(w) if w.contains("running")), "{:?}", refused.results[0].outcome);
        assert!(new.join("Contents/Info.plist").exists());
        let moved = delete(&env, &by_name("Xcode-15.4").items, &AtomicBool::new(false), &mut |_| {});
        assert_eq!(moved.moved_count(), 1, "{:?}", moved.results.iter().map(|r| format!("{:?}", r.outcome)).collect::<Vec<_>>());
        assert!(!old.exists() && new.join("Contents/Info.plist").exists(), "the old one went, the open one is untouched");
    }

    #[test]
    fn a_failed_move_of_an_app_says_how_to_give_the_terminal_permission() {
        assert!(looks_like_a_permission_error("could not move x to y: Operation not permitted (os error 1)"));
        assert!(looks_like_a_permission_error("Permission denied"));
        assert!(!looks_like_a_permission_error("No such file or directory"));
    }

    #[test]
    fn a_run_can_be_put_back_out_of_order_but_not_into_a_folder_that_a_newer_run_took_away() {
        let (home, env) = fake_home();
        let (items, _, _) = scan_all(&env);
        let one: Vec<ScanItem> = items.iter().filter(|i| i.title == "com.example.one").cloned().collect();
        let log: Vec<ScanItem> = items.iter().filter(|i| i.kind == Kind::Logs).cloned().collect();
        assert_eq!((one.len(), log.len()), (1, 1));
        run_delete(&env, &one);
        let first = env.journal.last_undo().unwrap().run;
        std::thread::sleep(std::time::Duration::from_millis(5));
        run_delete(&env, &log);
        let second = env.journal.last_undo().unwrap().run;
        assert_ne!(first, second);
        // The older run first, with the newer one still in the Trash: nothing stops it.
        let plan = env.journal.undo_plan(first, true).unwrap();
        let undone = undo(&env, &plan, &AtomicBool::new(false), &mut |_| {});
        assert_eq!((undone.restored.len(), undone.failed.len()), (1, 0), "{:?}", undone.failed);
        assert!(home.join("Library/Caches/com.example.one/data").exists());
        assert_eq!(env.journal.last_undo().unwrap().run, second, "and the newer one is still the newest left");
        // Now the folder of the newer run's item is gone, as if something removed it: out of order it will not be remade.
        let (home2, env2) = fake_home();
        let (items2, _, _) = scan_all(&env2);
        let logs2: Vec<ScanItem> = items2.iter().filter(|i| i.kind == Kind::Logs).cloned().collect();
        run_delete(&env2, &logs2);
        fs::remove_dir_all(home2.join("Library/Logs/App")).unwrap();
        let run = env2.journal.last_undo().unwrap().run;
        let strict = env2.journal.undo_plan(run, true).unwrap();
        let refused = undo(&env2, &strict, &AtomicBool::new(false), &mut |_| {});
        assert_eq!((refused.restored.len(), refused.failed.len()), (0, 1));
        assert!(refused.failed[0].1.contains("put back what came after it first"), "{}", refused.failed[0].1);
        assert!(!home2.join("Library/Logs/App").exists(), "the folder was not made again");
        let ordinary = env2.journal.undo_plan(run, false).unwrap();
        let restored = undo(&env2, &ordinary, &AtomicBool::new(false), &mut |_| {});
        assert_eq!(restored.restored.len(), 1, "in order, the folder is made again as it always was");
    }

    #[test]
    fn only_what_begone_moved_is_emptied_and_a_file_of_yours_with_the_same_name_stays() {
        let (home, env) = fake_home();
        let (items, _, _) = scan_all(&env);
        let caches: Vec<ScanItem> = items.into_iter().filter(|i| i.kind == Kind::Caches).collect();
        assert_eq!(caches.len(), 2);
        run_delete(&env, &caches);
        let trash = home.join(".Trash");
        // The Trash holds: begone's two, the user's own old.pdf (already there), and after Finder emptied one of begone's, a NEW
        // file of the user's that was given the same name.
        fs::remove_dir_all(trash.join("com.example.two")).unwrap();
        blob(&trash.join("com.example.two/mine.txt"), 5_000);
        let state = env.read_trash();
        let mark = |name: &str| state.entries.iter().find(|e| e.path.ends_with(name)).map(|e| e.begone);
        assert_eq!(
            (mark("com.example.one"), mark("com.example.two"), mark("old.pdf")),
            (Some(true), Some(false), Some(false)),
            "known by the file itself: the same name proves nothing"
        );
        let mut events = Vec::new();
        let report = empty_trash(&env, EmptyScope::OnlyBegones, &AtomicBool::new(false), &mut |e| events.push(e));
        assert_eq!((report.removed, report.gone.clone()), (1, vec![trash.join("com.example.one")]), "{:?}", report.kept);
        assert!(report.kept.is_empty(), "what is not begone's is not even attempted");
        assert!(!trash.join("com.example.one").exists());
        assert!(trash.join("com.example.two/mine.txt").exists() && trash.join("old.pdf").exists(), "yours stays in the Trash");
        let started =
            events.iter().find_map(|e| if let EmptyEvent::Started { items, bytes } = e { Some((*items, *bytes)) } else { None }).unwrap();
        assert_eq!(started.0, 1, "the count is of what will go, not of the whole Trash");
        assert_eq!(started.1, report.bytes);
        assert!(fs::read_to_string(env.journal.path()).unwrap().contains("\"emptied\""), "recorded, for the history");
        // Everything still empties everything.
        let all = empty_trash(&env, EmptyScope::Everything, &AtomicBool::new(false), &mut |_| {});
        assert_eq!(all.removed, 2);
        assert!(!trash.join("old.pdf").exists());
    }

    #[test]
    fn a_record_from_before_identities_and_a_swapped_entry_are_never_taken_for_begones() {
        let (home, env) = fake_home();
        let trash = home.join(".Trash");
        // An old-style history line says begone moved old.pdf; with no identity it proves nothing.
        let path = env.journal.path().to_path_buf();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            format!(
                "{{\"op\":\"intent\",\"run\":1,\"at\":1,\"path\":\"/x/old.pdf\",\"bytes\":1,\"kind\":\"caches\"}}\n{{\"op\":\"moved\",\"run\":1,\"at\":1,\"path\":\"/x/old.pdf\",\"trashed_to\":\"{}\"}}\n",
                trash.join("old.pdf").display()
            ),
        )
        .unwrap();
        let report = empty_trash(&env, EmptyScope::OnlyBegones, &AtomicBool::new(false), &mut |_| {});
        assert_eq!(report.removed, 0);
        assert!(trash.join("old.pdf").exists());
        // An entry swapped for another object between listing and removal is caught by the proof at the moment of removal.
        let (items, _, _) = scan_all(&env);
        let one: Vec<ScanItem> = items.into_iter().filter(|i| i.title == "com.example.one").collect();
        run_delete(&env, &one);
        let mine = env.journal.moved_identities();
        assert_eq!(mine.len(), 1);
        let real = trash.join("com.example.one");
        assert_eq!(journal::identity(&real).map(|id| mine.contains(&id)), Some(true));
        fs::remove_dir_all(&real).unwrap();
        blob(&real.join("replacement"), 100);
        assert_eq!(journal::identity(&real).map(|id| mine.contains(&id)), Some(false), "a new object under the same name");
    }

    #[test]
    fn what_begone_moved_can_be_emptied_and_then_cannot_be_undone() {
        let (home, env) = fake_home();
        let (items, _, _) = scan_all(&env);
        let caches: Vec<ScanItem> = items.into_iter().filter(|i| i.kind == Kind::Caches).collect();
        run_delete(&env, &caches);
        empty_trash(&env, EmptyScope::Everything, &AtomicBool::new(false), &mut |_| {});
        let plan = env.journal.last_undo().unwrap();
        let report = undo(&env, &plan, &AtomicBool::new(false), &mut |_| {});
        assert_eq!(report.restored.len(), 0);
        assert!(report.failed[0].1.contains("no longer in the Trash"));
        let _ = home;
    }
}
