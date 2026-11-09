//! The undo history: an append-only JSON-lines record of every move.
//!
//! Three rules, and each exists because the first version broke it:
//!
//! 1. **Write it down first.** An `intent` line is written before an item is trashed, so
//!    a crash mid-run still leaves a record of what was about to happen.
//! 2. **Fail closed.** If that line cannot be written, the item is not moved. An item
//!    trashed with no way back is worse than an item left alone.
//! 3. **Never rewrite.** Records are only appended. The first version rewrote the whole
//!    file for every item, which cost 45 seconds at 20,000 items and meant a crash
//!    could lose the entire history. A torn last line is simply skipped on read.
//!
//! An `intent` also writes down which filesystem object the item is (its device and inode). A move is a rename, so the thing
//! in the Trash is that very object, and that is how "empty only what begone moved" knows it from a file of yours that merely
//! has the same name. Records from before this was written have no identity, so they are never taken for begone's.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::model::Kind;

/// Milliseconds since the epoch when the run began, so runs sort by time.
pub type RunId = u64;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Rec {
    /// About to move `path`. Written before the move. `dev` and `ino` say which object it is (0 when not known).
    Intent {
        run: RunId,
        at: u64,
        path: String,
        bytes: u64,
        kind: String,
        #[serde(default)]
        dev: u64,
        #[serde(default)]
        ino: u64,
    },
    /// The move happened. `trashed_to` is where it landed, when the platform said.
    Moved { run: RunId, at: u64, path: String, trashed_to: Option<String> },
    /// The move was attempted and failed, so the intent is resolved.
    Failed { run: RunId, at: u64, path: String },
    /// An undo put it back.
    Restored { run: RunId, at: u64, path: String },
    /// The Trash was emptied, all of it or only what begone moved. Recorded for the history; it cannot be undone.
    Emptied { run: RunId, at: u64, items: u64, bytes: u64 },
    /// Something was deleted for good by the tool that owns it (a simulator). Recorded for the history; it cannot be undone.
    Deleted { run: RunId, at: u64, what: String, bytes: u64 },
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

pub fn new_run_id() -> RunId {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// Which filesystem object a path names: its device and inode, not looking through a symlink. A move is a rename, so the item
/// keeps both, and what sits in the Trash can be known as the thing that was moved.
pub fn identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let meta = path.symlink_metadata().ok()?;
    Some((meta.dev(), meta.ino()))
}

pub struct Journal {
    path: PathBuf,
}

impl Journal {
    pub fn new(home: &Path) -> Self {
        Self { path: home.join(".local/share/begone/history.jsonl") }
    }

    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Open for appending, creating the folder and file if needed.
    pub fn appender(&self) -> io::Result<Appender> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok(Appender { file: OpenOptions::new().create(true).append(true).open(&self.path)? })
    }

    fn read(&self) -> Vec<Rec> {
        let Ok(text) = fs::read_to_string(&self.path) else { return Vec::new() };
        text.lines().filter(|l| !l.trim().is_empty()).filter_map(|l| serde_json::from_str(l).ok()).collect()
    }

    /// The newest run that still has something to put back.
    pub fn last_undo(&self) -> Option<UndoPlan> {
        plan_from(&self.read())
    }

    /// What it would take to put back one particular run, whichever it is: `None` when nothing of it is left to restore.
    /// `strict` refuses an item whose folder is no longer there instead of making the folder again, which is what undoing out
    /// of order needs: a newer run may be what removed it.
    pub fn undo_plan(&self, run: RunId, strict: bool) -> Option<UndoPlan> {
        let (runs, _) = accumulate(&self.read());
        let mut plan = runs.get(&run).map(|acc| plan_for(run, acc))?;
        plan.strict = strict;
        (!plan.restorable.is_empty() || plan.unresolved > 0).then_some(plan)
    }

    /// The device and inode of everything begone moved to the Trash and has not put back: what a Trash entry has to be to be
    /// called begone's.
    pub fn moved_identities(&self) -> HashSet<(u64, u64)> {
        let (runs, _) = accumulate(&self.read());
        runs.values()
            .flat_map(|acc| acc.items.values())
            .filter(|item| matches!(&item.state, ItemState::Moved(Some(dest)) if !dest.is_empty()) && item.ino != 0)
            .map(|item| (item.dev, item.ino))
            .collect()
    }

    /// Every run and every irreversible step, newest first. `trash` is where moved things sit now, so each run can say how much
    /// of it is still there to put back.
    pub fn history(&self, trash: &Path) -> Vec<HistoryEntry> {
        let (runs, others) = accumulate(&self.read());
        let mut out: Vec<(Moment, HistoryEntry)> = Vec::new();
        for (run, acc) in &runs {
            let mut summary = RunSummary { run: *run, at: acc.at, ..RunSummary::default() };
            let mut kinds: BTreeMap<Kind, usize> = BTreeMap::new();
            for path in &acc.order {
                let item = &acc.items[path];
                match &item.state {
                    ItemState::Moved(dest) => {
                        summary.moved += 1;
                        summary.bytes += item.bytes;
                        *kinds.entry(item.kind).or_insert(0) += 1;
                        match dest {
                            Some(d) if !d.is_empty() => {
                                let there = Path::new(d).starts_with(trash) && Path::new(d).symlink_metadata().is_ok();
                                if there {
                                    summary.in_trash += 1;
                                    summary.in_trash_bytes += item.bytes;
                                }
                            }
                            _ => summary.unresolved += 1,
                        }
                    }
                    ItemState::Restored => {
                        summary.moved += 1;
                        summary.bytes += item.bytes;
                        summary.restored += 1;
                        *kinds.entry(item.kind).or_insert(0) += 1;
                    }
                    ItemState::Intent => summary.unresolved += 1,
                    ItemState::Failed => {}
                }
            }
            summary.kinds = kinds.into_iter().collect();
            if summary.moved > 0 || summary.unresolved > 0 {
                out.push(((acc.at, *run), HistoryEntry::Run(summary)));
            }
        }
        out.extend(others);
        // Newest first, and by the id the moment began with when two things happened in the same second.
        out.sort_by_key(|(moment, _)| std::cmp::Reverse(*moment));
        out.into_iter().map(|(_, e)| e).collect()
    }
}

/// One line of the history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryEntry {
    /// Things begone moved to the Trash in one go.
    Run(RunSummary),
    /// The Trash was emptied: all of it, or only what begone moved. It cannot be undone.
    Emptied { at: u64, items: u64, bytes: u64 },
    /// Something was deleted for good by the tool that owns it. It cannot be undone.
    Deleted { at: u64, what: String, bytes: u64 },
}

/// What one run did, and what is left of it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunSummary {
    pub run: RunId,
    pub at: u64,
    /// Items that moved to the Trash, and their size.
    pub moved: usize,
    pub bytes: u64,
    /// Of those, how many are still in the Trash to put back, and their size.
    pub in_trash: usize,
    pub in_trash_bytes: u64,
    /// How many an undo already put back.
    pub restored: usize,
    /// Items with no record of where they went.
    pub unresolved: usize,
    pub kinds: Vec<(Kind, usize)>,
}

pub struct Appender {
    file: File,
}

impl Appender {
    fn write(&mut self, rec: &Rec) -> io::Result<()> {
        let mut line = serde_json::to_string(rec).map_err(io::Error::other)?;
        line.push('\n');
        // One write per record: with O_APPEND a small write lands whole.
        self.file.write_all(line.as_bytes())
    }

    /// About to move `path`: write down what it is, and which object, first.
    pub fn intent(&mut self, run: RunId, path: &Path, bytes: u64, kind: Kind) -> io::Result<()> {
        let (dev, ino) = identity(path).unwrap_or((0, 0));
        self.write(&Rec::Intent {
            run,
            at: now_secs(),
            path: path.to_string_lossy().into_owned(),
            bytes,
            kind: kind.key().to_string(),
            dev,
            ino,
        })
    }

    pub fn moved(&mut self, run: RunId, path: &Path, trashed_to: Option<&Path>) -> io::Result<()> {
        self.write(&Rec::Moved {
            run,
            at: now_secs(),
            path: path.to_string_lossy().into_owned(),
            trashed_to: trashed_to.map(|p| p.to_string_lossy().into_owned()),
        })
    }

    pub fn failed(&mut self, run: RunId, path: &Path) -> io::Result<()> {
        self.write(&Rec::Failed { run, at: now_secs(), path: path.to_string_lossy().into_owned() })
    }

    pub fn restored(&mut self, run: RunId, path: &Path) -> io::Result<()> {
        self.write(&Rec::Restored { run, at: now_secs(), path: path.to_string_lossy().into_owned() })
    }

    pub fn emptied(&mut self, items: u64, bytes: u64) -> io::Result<()> {
        self.write(&Rec::Emptied { run: new_run_id(), at: now_secs(), items, bytes })
    }

    /// Something was deleted for good by the tool that owns it.
    pub fn deleted(&mut self, what: &str, bytes: u64) -> io::Result<()> {
        self.write(&Rec::Deleted { run: new_run_id(), at: now_secs(), what: what.to_string(), bytes })
    }

    /// Flush to disk once, at the end of a run.
    pub fn sync(&mut self) -> io::Result<()> {
        self.file.sync_data()
    }
}

/// One item an undo can put back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Restorable {
    pub from: PathBuf,
    pub to: PathBuf,
    pub bytes: u64,
    pub kind: Kind,
}

/// Everything undo needs to know about a run that still has something to restore.
#[derive(Clone, Debug)]
pub struct UndoPlan {
    pub run: RunId,
    pub at: u64,
    pub restorable: Vec<Restorable>,
    /// Items whose move was started but never confirmed, so nothing says where they went.
    pub unresolved: usize,
    /// Refuse an item whose folder is gone instead of making the folder again.
    pub strict: bool,
}

impl UndoPlan {
    pub fn bytes(&self) -> u64 {
        self.restorable.iter().map(|r| r.bytes).sum()
    }
}

#[derive(Default)]
struct Acc {
    at: u64,
    order: Vec<String>,
    items: HashMap<String, Item>,
}

struct Item {
    bytes: u64,
    kind: Kind,
    state: ItemState,
    dev: u64,
    ino: u64,
}

enum ItemState {
    Intent,
    Moved(Option<String>),
    Restored,
    Failed,
}

/// The runs, and the steps that were not runs (an emptying, a deletion) with when they were.
/// When something happened, to the second, and the id of the moment it began with: the order of the history.
type Moment = (u64, RunId);

fn accumulate(recs: &[Rec]) -> (BTreeMap<RunId, Acc>, Vec<(Moment, HistoryEntry)>) {
    let mut runs: BTreeMap<RunId, Acc> = BTreeMap::new();
    let mut others = Vec::new();
    for rec in recs {
        match rec {
            Rec::Intent { run, at, path, bytes, kind, dev, ino } => {
                let acc = runs.entry(*run).or_default();
                acc.at = *at;
                let item = Item {
                    bytes: *bytes,
                    kind: Kind::from_key(kind).unwrap_or(Kind::Caches),
                    state: ItemState::Intent,
                    dev: *dev,
                    ino: *ino,
                };
                if acc.items.insert(path.clone(), item).is_none() {
                    acc.order.push(path.clone());
                }
            }
            Rec::Moved { run, path, trashed_to, .. } => {
                if let Some(item) = runs.get_mut(run).and_then(|a| a.items.get_mut(path)) {
                    item.state = ItemState::Moved(trashed_to.clone());
                }
            }
            Rec::Failed { run, path, .. } => {
                if let Some(item) = runs.get_mut(run).and_then(|a| a.items.get_mut(path)) {
                    item.state = ItemState::Failed;
                }
            }
            Rec::Restored { run, path, .. } => {
                if let Some(item) = runs.get_mut(run).and_then(|a| a.items.get_mut(path)) {
                    item.state = ItemState::Restored;
                }
            }
            Rec::Emptied { run, at, items, bytes } => {
                others.push(((*at, *run), HistoryEntry::Emptied { at: *at, items: *items, bytes: *bytes }));
            }
            Rec::Deleted { run, at, what, bytes } => {
                others.push(((*at, *run), HistoryEntry::Deleted { at: *at, what: what.clone(), bytes: *bytes }));
            }
        }
    }
    (runs, others)
}

fn plan_for(run: RunId, acc: &Acc) -> UndoPlan {
    let mut restorable = Vec::new();
    let mut unresolved = 0;
    for path in &acc.order {
        let item = &acc.items[path];
        match &item.state {
            ItemState::Moved(Some(dest)) if !dest.is_empty() => {
                restorable.push(Restorable { from: PathBuf::from(dest), to: PathBuf::from(path), bytes: item.bytes, kind: item.kind });
            }
            ItemState::Moved(_) | ItemState::Intent => unresolved += 1,
            ItemState::Restored | ItemState::Failed => {}
        }
    }
    UndoPlan { run, at: acc.at, restorable, unresolved, strict: false }
}

fn plan_from(recs: &[Rec]) -> Option<UndoPlan> {
    let (runs, _) = accumulate(recs);
    let mut fallback = None;
    for (run, acc) in runs.iter().rev() {
        let plan = plan_for(*run, acc);
        if !plan.restorable.is_empty() {
            return Some(plan);
        }
        if plan.unresolved > 0 && fallback.is_none() {
            fallback = Some(plan);
        }
    }
    fallback
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safety::tests::tempdir;

    fn journal() -> (crate::safety::tests::TempDir, Journal) {
        let home = tempdir();
        let j = Journal::new(&home);
        (home, j)
    }

    fn moved_item(a: &mut Appender, run: RunId, path: &str, to: &str, bytes: u64) {
        a.intent(run, Path::new(path), bytes, Kind::Caches).unwrap();
        a.moved(run, Path::new(path), Some(Path::new(to))).unwrap();
    }

    #[test]
    fn an_empty_history_has_nothing_to_undo() {
        let (_home, j) = journal();
        assert!(j.last_undo().is_none());
    }

    #[test]
    fn the_newest_run_is_the_one_undo_offers() {
        let (_home, j) = journal();
        let mut a = j.appender().unwrap();
        moved_item(&mut a, 10, "/a", "/T/a", 5);
        moved_item(&mut a, 11, "/c", "/T/c", 7);
        moved_item(&mut a, 11, "/d", "/T/d", 8);
        let plan = j.last_undo().unwrap();
        assert_eq!(plan.run, 11);
        assert_eq!(plan.restorable.len(), 2);
        assert_eq!(plan.bytes(), 15);
    }

    #[test]
    fn restoring_a_run_makes_the_one_before_it_reachable() {
        // Regression: undoing twice used to replay the same run and report failure,
        // so older runs could never be reached.
        let (_home, j) = journal();
        let mut a = j.appender().unwrap();
        moved_item(&mut a, 10, "/a", "/T/a", 5);
        moved_item(&mut a, 11, "/c", "/T/c", 7);
        a.restored(11, Path::new("/c")).unwrap();
        let plan = j.last_undo().unwrap();
        assert_eq!(plan.run, 10);
        assert_eq!(plan.restorable[0].to, Path::new("/a"));
    }

    #[test]
    fn a_failed_move_leaves_nothing_to_restore() {
        let (_home, j) = journal();
        let mut a = j.appender().unwrap();
        a.intent(5, Path::new("/x"), 1, Kind::Logs).unwrap();
        a.failed(5, Path::new("/x")).unwrap();
        assert!(j.last_undo().is_none());
    }

    #[test]
    fn a_crash_between_intent_and_move_is_reported_not_hidden() {
        let (_home, j) = journal();
        let mut a = j.appender().unwrap();
        a.intent(5, Path::new("/x"), 1, Kind::Logs).unwrap();
        let plan = j.last_undo().unwrap();
        assert_eq!((plan.restorable.len(), plan.unresolved), (0, 1));
    }

    #[test]
    fn an_unknown_destination_cannot_be_restored() {
        let (_home, j) = journal();
        let mut a = j.appender().unwrap();
        a.intent(5, Path::new("/x"), 1, Kind::Logs).unwrap();
        a.moved(5, Path::new("/x"), None).unwrap();
        a.intent(5, Path::new("/y"), 1, Kind::Logs).unwrap();
        a.moved(5, Path::new("/y"), Some(Path::new("/T/y"))).unwrap();
        let plan = j.last_undo().unwrap();
        assert_eq!((plan.restorable.len(), plan.unresolved), (1, 1));
    }

    #[test]
    fn a_torn_last_line_and_foreign_lines_are_skipped() {
        let (_home, j) = journal();
        let mut a = j.appender().unwrap();
        moved_item(&mut a, 3, "/a", "/T/a", 5);
        drop(a);
        let mut f = OpenOptions::new().append(true).open(j.path()).unwrap();
        writeln!(f, "not json at all").unwrap();
        write!(f, r#"{{"op":"intent","run":4,"at":1,"pa"#).unwrap();
        assert_eq!(j.last_undo().unwrap().run, 3);
    }

    #[test]
    fn emptying_the_trash_is_recorded_but_never_offered_for_undo() {
        let (_home, j) = journal();
        let mut a = j.appender().unwrap();
        a.emptied(4, 1_000).unwrap();
        assert!(j.last_undo().is_none());
        assert!(fs::read_to_string(j.path()).unwrap().contains("\"emptied\""));
    }

    fn real(home: &Path, name: &str) -> PathBuf {
        let path = home.join(name);
        fs::write(&path, b"x").unwrap();
        path
    }

    #[test]
    fn what_was_moved_is_known_by_the_object_it_is_and_nothing_else_is() {
        let (home, j) = journal();
        let mut a = j.appender().unwrap();
        let (moved, put_back, unsure, intent_only) =
            (real(&home, "moved"), real(&home, "put-back"), real(&home, "unsure"), real(&home, "intent-only"));
        let ids: Vec<_> = [&moved, &put_back, &unsure, &intent_only].iter().map(|p| identity(p).unwrap()).collect();
        for p in [&moved, &put_back, &unsure, &intent_only] {
            a.intent(7, p, 1, Kind::Caches).unwrap();
        }
        a.moved(7, &moved, Some(Path::new("/T/moved"))).unwrap();
        a.moved(7, &put_back, Some(Path::new("/T/put-back"))).unwrap();
        a.restored(7, &put_back).unwrap();
        a.moved(7, &unsure, None).unwrap();
        let known = j.moved_identities();
        assert_eq!(known, HashSet::from([ids[0]]), "only a move whose landing place is known and not undone");
        // A record from before identities were written names no object, so it is never taken for begone's.
        let mut f = OpenOptions::new().append(true).open(j.path()).unwrap();
        writeln!(f, r#"{{"op":"intent","run":3,"at":1,"path":"/old","bytes":1,"kind":"caches"}}"#).unwrap();
        writeln!(f, r#"{{"op":"moved","run":3,"at":1,"path":"/old","trashed_to":"/T/old"}}"#).unwrap();
        assert_eq!(j.moved_identities(), known);
        assert_eq!(j.last_undo().unwrap().run, 7, "and the old record still loads, as an ordinary one");
        assert!(identity(Path::new("/nonexistent/never")).is_none());
    }

    fn write_lines(j: &Journal, lines: &[&str]) {
        fs::create_dir_all(j.path().parent().unwrap()).unwrap();
        fs::write(j.path(), lines.join("\n") + "\n").unwrap();
    }

    #[test]
    fn the_history_lists_every_run_and_every_irreversible_step_newest_first_with_what_is_still_in_the_trash() {
        let (home, j) = journal();
        let trash = home.join(".Trash");
        fs::create_dir_all(&trash).unwrap();
        for name in ["a", "b", "c", "d"] {
            fs::write(trash.join(name), b"x").unwrap();
        }
        let t = |n: &str| trash.join(n).display().to_string();
        let lines = [
            // Run 10, the oldest: two items, both still in the Trash.
            r#"{"op":"intent","run":10,"at":100,"path":"/h/a","bytes":5,"kind":"caches"}"#.to_string(),
            format!(r#"{{"op":"moved","run":10,"at":100,"path":"/h/a","trashed_to":"{}"}}"#, t("a")),
            r#"{"op":"intent","run":10,"at":100,"path":"/h/b","bytes":7,"kind":"logs"}"#.to_string(),
            format!(r#"{{"op":"moved","run":10,"at":100,"path":"/h/b","trashed_to":"{}"}}"#, t("b")),
            // Run 11: one item, put back since.
            r#"{"op":"intent","run":11,"at":200,"path":"/h/c","bytes":9,"kind":"xcode"}"#.to_string(),
            format!(r#"{{"op":"moved","run":11,"at":200,"path":"/h/c","trashed_to":"{}"}}"#, t("c")),
            r#"{"op":"restored","run":11,"at":210,"path":"/h/c"}"#.to_string(),
            // The Trash was emptied at 300, and what run 12 moved at 250 is gone from it.
            r#"{"op":"intent","run":12,"at":250,"path":"/h/gone","bytes":11,"kind":"caches"}"#.to_string(),
            format!(r#"{{"op":"moved","run":12,"at":250,"path":"/h/gone","trashed_to":"{}"}}"#, t("gone")),
            r#"{"op":"emptied","run":300000,"at":300,"items":1,"bytes":11}"#.to_string(),
            // A simulator deleted for good at 400, and a run where nothing moved at all.
            r#"{"op":"deleted","run":400000,"at":400,"what":"2 simulators","bytes":4000}"#.to_string(),
            r#"{"op":"intent","run":13,"at":500,"path":"/h/x","bytes":1,"kind":"caches"}"#.to_string(),
            r#"{"op":"failed","run":13,"at":500,"path":"/h/x"}"#.to_string(),
        ];
        write_lines(&j, &lines.iter().map(String::as_str).collect::<Vec<_>>());
        let history = j.history(&trash);
        let shape: Vec<String> = history
            .iter()
            .map(|e| match e {
                HistoryEntry::Run(r) => format!("run {} moved {} in_trash {} restored {}", r.run, r.moved, r.in_trash, r.restored),
                HistoryEntry::Emptied { items, .. } => format!("emptied {items}"),
                HistoryEntry::Deleted { what, .. } => format!("deleted {what}"),
            })
            .collect();
        assert_eq!(
            shape,
            [
                "deleted 2 simulators",
                "emptied 1",
                "run 12 moved 1 in_trash 0 restored 0",
                "run 11 moved 1 in_trash 0 restored 1",
                "run 10 moved 2 in_trash 2 restored 0"
            ],
            "newest first; the run where nothing moved is not a run"
        );
        let HistoryEntry::Run(ten) = &history[4] else { panic!() };
        assert_eq!((ten.bytes, ten.in_trash_bytes, ten.kinds.clone()), (12, 12, vec![(Kind::Caches, 1), (Kind::Logs, 1)]));
        // Anything of begone's outside the Trash does not count as in it.
        assert!(matches!(&j.history(&home.join("elsewhere"))[4], HistoryEntry::Run(r) if r.in_trash == 0));
    }

    #[test]
    fn any_run_can_be_planned_for_undo_whatever_is_newer_and_strictness_is_the_callers_to_ask_for() {
        let (_home, j) = journal();
        let mut a = j.appender().unwrap();
        moved_item(&mut a, 10, "/a", "/T/a", 5);
        moved_item(&mut a, 11, "/c", "/T/c", 7);
        a.restored(11, Path::new("/c")).unwrap();
        moved_item(&mut a, 12, "/d", "/T/d", 8);
        let ten = j.undo_plan(10, true).unwrap();
        assert_eq!((ten.run, ten.restorable.len(), ten.strict), (10, 1, true), "the newer run 12 is not in its way");
        assert!(!j.undo_plan(10, false).unwrap().strict);
        assert!(j.undo_plan(11, false).is_none(), "everything of it was put back");
        assert!(j.undo_plan(99, false).is_none(), "no such run");
        assert_eq!(j.undo_plan(12, false).unwrap().restorable[0].to, Path::new("/d"));
        assert_eq!(j.last_undo().unwrap().run, 12, "and the ordinary undo is unchanged");
    }

    #[test]
    fn a_rename_keeps_the_identity_so_the_thing_in_the_trash_is_the_thing_that_moved() {
        let (home, j) = journal();
        let mut a = j.appender().unwrap();
        let from = real(&home, "before");
        a.intent(1, &from, 1, Kind::Logs).unwrap();
        let to = home.join("after");
        fs::rename(&from, &to).unwrap();
        a.moved(1, &from, Some(&to)).unwrap();
        assert_eq!(identity(&to), j.moved_identities().into_iter().next());
        // A different file at the same name is a different object.
        fs::remove_file(&to).unwrap();
        fs::write(&to, b"someone else's").unwrap();
        assert!(!j.moved_identities().contains(&identity(&to).unwrap()));
    }

    #[test]
    fn appending_stays_cheap_at_scale() {
        // Regression: rewriting the file per item took 45 seconds at 20,000 items.
        let (_home, j) = journal();
        let mut a = j.appender().unwrap();
        let started = std::time::Instant::now();
        for i in 0..20_000 {
            moved_item(&mut a, 1, &format!("/Users/x/Library/Logs/App/file-{i}.log"), &format!("/T/f{i}"), 4096);
        }
        assert!(started.elapsed().as_secs() < 5, "took {:?}", started.elapsed());
        assert_eq!(j.last_undo().unwrap().restorable.len(), 20_000);
    }
}
