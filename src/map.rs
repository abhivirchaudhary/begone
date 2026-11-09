//! The disk map: where the bytes in a folder live, as a tree you can open one level at a time.
//!
//! Read-only by construction. It lists directories and reads metadata, never opens a file, never
//! follows a symlink, and never leaves the filesystem it started on (a mounted volume or a cloud
//! folder inside your home is not this disk's bytes). Sizes follow the same rules as everywhere
//! else in begone: allocated blocks, a clone's shared data once, a hard link once.
//!
//! The walk keeps a small table for every folder, then folds the result into a tree with at most
//! [`KEEP`] children each; whatever does not make the cut is one "smaller items" entry, so a
//! home folder of a million files costs a few megabytes, not a few hundred.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::sizing::Facts;

/// Children kept per folder. The rest are folded into one "smaller items" entry.
pub const KEEP: usize = 48;
/// The largest files remembered per folder while walking.
const TOP_FILES: usize = 12;
/// Folders nested deeper than this are shown as a single entry.
const MAX_DEPTH: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    Dir,
    File,
    /// Everything in a folder that did not make the cut, as one entry. `files` says how many things.
    Smaller,
}

#[derive(Clone, Debug)]
pub struct MapNode {
    pub name: String,
    pub kind: NodeKind,
    pub bytes: u64,
    /// Files inside (1 for a file, the number folded in for `Smaller`).
    pub files: u64,
    /// For a folder: biggest first, with the `Smaller` entry last. Empty for everything else.
    pub children: Vec<MapNode>,
}

#[derive(Clone, Debug)]
pub struct MapTree {
    pub root: PathBuf,
    pub node: MapNode,
    /// Folders or entries that could not be read (macOS privacy, or something vanished mid-walk).
    pub unreadable: u64,
    pub files: u64,
    pub secs: f32,
}

impl MapTree {
    /// The folder reached by opening these children in turn, or `None` if the path leaves the tree.
    pub fn node_at(&self, path: &[usize]) -> Option<&MapNode> {
        let mut node = &self.node;
        for &i in path {
            node = node.children.get(i)?;
        }
        Some(node)
    }

    /// Where on disk the folder reached by `path` is. A `Smaller` entry has no place of its own, so the
    /// path stops at the folder that holds it.
    pub fn path_to(&self, path: &[usize]) -> PathBuf {
        let mut out = self.root.clone();
        let mut node = &self.node;
        for &i in path {
            let Some(next) = node.children.get(i) else { break };
            if next.kind == NodeKind::Smaller {
                break;
            }
            out.push(&next.name);
            node = next;
        }
        out
    }
}

/// What the mapping worker tells the loop.
pub enum MapEvent {
    /// How far it has got.
    Progress {
        files: u64,
        bytes: u64,
    },
    Done(Arc<MapTree>),
    /// The walk died. Nothing on disk was touched, since it only reads.
    Failed,
}

// ---------------------------------------------------------------------------------------- the walk

/// What a walker reports.
enum Visit<'a> {
    Dir {
        id: usize,
        parent: Option<usize>,
        depth: u16,
        name: String,
    },
    File {
        parent: usize,
        name: &'a OsStr,
        facts: Facts,
    },
    /// Something could not be read.
    Failed,
}

#[cfg(target_os = "macos")]
fn walk_dir(root: &Path, threads: usize, cancel: &AtomicBool, visit: &mut dyn FnMut(Visit<'_>)) -> bool {
    use dua_core::{Options, Order, walk};
    use std::os::unix::fs::MetadataExt;
    let root_dev = std::fs::symlink_metadata(root).map(|m| m.dev()).ok();
    let mut options = Options::default();
    options.apfs_clone_metadata = true;
    let mut entries = walk(root, threads.max(1), Order::ParentFirst, options, move |e| match (&e.metadata, root_dev) {
        (Some(Ok(meta)), Some(dev)) => meta.dev() == dev,
        _ => true,
    });
    while let Some(item) = entries.next_cancellable(cancel) {
        let Ok(entry) = item else {
            visit(Visit::Failed);
            continue;
        };
        if entry.file_type.is_dir() {
            let Some(id) = entry.directory_id else { continue };
            visit(Visit::Dir {
                id: id.index(),
                parent: entry.parent_directory_id.map(|p| p.index()),
                depth: entry.depth.min(usize::from(u16::MAX)) as u16,
                name: entry.file_name.to_string_lossy().into_owned(),
            });
            continue;
        }
        let (Some(parent), Some(Ok(meta))) = (entry.parent_directory_id, &entry.metadata) else {
            visit(Visit::Failed);
            continue;
        };
        visit(Visit::File {
            parent: parent.index(),
            name: &entry.file_name,
            facts: Facts {
                dev: meta.dev(),
                ino: meta.ino(),
                nlink: meta.nlink(),
                allocated: meta.allocated_size(),
                data_allocated: meta.data_allocated_size(),
                clone_id: meta.clone_id().map(std::num::NonZeroU64::get),
                modified: meta.modified().ok(),
            },
        });
    }
    !cancel.load(Ordering::Relaxed)
}

#[cfg(not(target_os = "macos"))]
fn walk_dir(root: &Path, _threads: usize, cancel: &AtomicBool, visit: &mut dyn FnMut(Visit<'_>)) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = std::fs::symlink_metadata(root) else {
        visit(Visit::Failed);
        return true;
    };
    let dev = meta.dev();
    visit(Visit::Dir {
        id: 0,
        parent: None,
        depth: 0,
        name: root.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
    });
    let mut next_id = 1usize;
    let mut stack = vec![(root.to_path_buf(), 0usize, 0u16)];
    while let Some((dir, id, depth)) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            visit(Visit::Failed);
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                visit(Visit::Failed);
                continue;
            };
            if meta.is_dir() {
                let child = next_id;
                next_id += 1;
                visit(Visit::Dir {
                    id: child,
                    parent: Some(id),
                    depth: depth.saturating_add(1),
                    name: entry.file_name().to_string_lossy().into_owned(),
                });
                if meta.dev() == dev {
                    stack.push((entry.path(), child, depth.saturating_add(1)));
                }
            } else {
                visit(Visit::File { parent: id, name: &entry.file_name(), facts: crate::sizing::facts_from_std(&meta) });
            }
        }
    }
    true
}

// ------------------------------------------------------------------------------------- the builder

/// What the walk remembers about one folder.
#[derive(Default)]
struct Raw {
    parent: Option<usize>,
    depth: u16,
    name: String,
    /// Files directly inside.
    bytes: u64,
    files: u64,
    /// The largest of those, biggest unordered.
    top: Vec<(u64, String)>,
}

#[derive(Default)]
struct Builder {
    dirs: Vec<Raw>,
    files: u64,
    bytes: u64,
    unreadable: u64,
    links: HashSet<(u64, u64)>,
    clones: HashSet<(u64, u64)>,
}

impl Builder {
    fn dir(&mut self, id: usize) -> &mut Raw {
        if self.dirs.len() <= id {
            self.dirs.resize_with(id + 1, Raw::default);
        }
        &mut self.dirs[id]
    }

    /// What this file adds to the total: clones share their data and hard links share everything, so the
    /// second and later names of either add only what is genuinely theirs.
    fn counted(&mut self, f: &Facts) -> u64 {
        let mut bytes = f.allocated;
        if let Some(id) = f.clone_id
            && !self.clones.insert((f.dev, id))
        {
            bytes = f.allocated.saturating_sub(f.data_allocated);
        }
        if f.nlink > 1 && !self.links.insert((f.dev, f.ino)) {
            bytes = 0;
        }
        bytes
    }

    fn visit(&mut self, v: Visit<'_>, progress: &mut dyn FnMut(u64, u64)) {
        match v {
            Visit::Failed => self.unreadable += 1,
            Visit::Dir { id, parent, depth, name } => {
                let raw = self.dir(id);
                raw.parent = parent;
                raw.depth = depth;
                raw.name = name;
            }
            Visit::File { parent, name, facts } => {
                let bytes = self.counted(&facts);
                self.files += 1;
                self.bytes += bytes;
                let raw = self.dir(parent);
                raw.bytes += bytes;
                raw.files += 1;
                // Only a file that makes the folder's top list is worth a name.
                if raw.top.len() < TOP_FILES {
                    raw.top.push((bytes, name.to_string_lossy().into_owned()));
                } else if let Some((at, least)) = raw.top.iter().enumerate().min_by_key(|(_, t)| t.0)
                    && bytes > least.0
                {
                    raw.top[at] = (bytes, name.to_string_lossy().into_owned());
                }
                if self.files.is_multiple_of(4096) {
                    progress(self.files, self.bytes);
                }
            }
        }
    }

    fn finish(mut self, root: &Path, secs: f32) -> MapTree {
        self.dir(0);
        let n = self.dirs.len();
        let mut total_bytes: Vec<u64> = self.dirs.iter().map(|d| d.bytes).collect();
        let mut total_files: Vec<u64> = self.dirs.iter().map(|d| d.files).collect();
        // Deepest first, so every folder is complete before it is added to its parent.
        let mut order: Vec<usize> = (0..self.dirs.len()).collect();
        order.sort_unstable_by(|a, b| self.dirs[*b].depth.cmp(&self.dirs[*a].depth));
        for &i in &order {
            if let Some(p) = self.dirs[i].parent
                && p != i
            {
                total_bytes[p] += total_bytes[i];
                total_files[p] += total_files[i];
            }
        }
        // Children by parent, without a vector per folder.
        let mut start = vec![0usize; n + 1];
        for d in &self.dirs {
            if let Some(p) = d.parent {
                start[p + 1] += 1;
            }
        }
        for i in 0..n {
            start[i + 1] += start[i];
        }
        let mut fill = start.clone();
        let mut kids = vec![0usize; start[n]];
        for (i, d) in self.dirs.iter().enumerate() {
            if let Some(p) = d.parent {
                kids[fill[p]] = i;
                fill[p] += 1;
            }
        }
        let mut names: Vec<String> = self.dirs.iter_mut().map(|d| std::mem::take(&mut d.name)).collect();
        let mut tops: Vec<Vec<(u64, String)>> = self.dirs.iter_mut().map(|d| std::mem::take(&mut d.top)).collect();
        let ctx = Fold { start: &start, kids: &kids, total_bytes: &total_bytes, total_files: &total_files };
        let mut node = ctx.node(0, &mut names, &mut tops, 0);
        if node.name.is_empty() {
            node.name = root.file_name().map_or_else(|| root.display().to_string(), |n| n.to_string_lossy().into_owned());
        }
        MapTree { root: root.to_path_buf(), node, unreadable: self.unreadable, files: self.files, secs }
    }
}

struct Fold<'a> {
    start: &'a [usize],
    kids: &'a [usize],
    total_bytes: &'a [u64],
    total_files: &'a [u64],
}

enum Cand {
    Dir(usize),
    File(String),
}

impl Fold<'_> {
    fn node(&self, id: usize, names: &mut [String], tops: &mut [Vec<(u64, String)>], depth: usize) -> MapNode {
        let mut node = MapNode {
            name: std::mem::take(&mut names[id]),
            kind: NodeKind::Dir,
            bytes: self.total_bytes[id],
            files: self.total_files[id],
            children: Vec::new(),
        };
        if depth >= MAX_DEPTH {
            return node;
        }
        let mut cands: Vec<(u64, u64, Cand)> = Vec::new();
        for &child in &self.kids[self.start[id]..self.start[id + 1]] {
            cands.push((self.total_bytes[child], self.total_files[child], Cand::Dir(child)));
        }
        for (bytes, name) in std::mem::take(&mut tops[id]) {
            cands.push((bytes, 1, Cand::File(name)));
        }
        // Biggest first; names break ties so the same disk always draws the same list.
        let label = |c: &(u64, u64, Cand), names: &[String]| match &c.2 {
            Cand::Dir(i) => names[*i].clone(),
            Cand::File(n) => n.clone(),
        };
        cands.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| label(a, names).cmp(&label(b, names))));
        let (mut kept_bytes, mut kept_files) = (0u64, 0u64);
        for c in cands.into_iter().take(KEEP) {
            kept_bytes += c.0;
            kept_files += c.1;
            match c.2 {
                Cand::Dir(child) => node.children.push(self.node(child, names, tops, depth + 1)),
                Cand::File(name) => node.children.push(MapNode { name, kind: NodeKind::File, bytes: c.0, files: 1, children: Vec::new() }),
            }
        }
        let (rest_bytes, rest_files) = (node.bytes.saturating_sub(kept_bytes), node.files.saturating_sub(kept_files));
        if rest_bytes > 0 {
            node.children.push(MapNode {
                name: String::new(),
                kind: NodeKind::Smaller,
                bytes: rest_bytes,
                files: rest_files,
                children: Vec::new(),
            });
        }
        node
    }
}

/// Map everything under `root`. `progress` hears `(files, bytes)` every few thousand files. Returns `None` if
/// `cancel` was set before the walk finished.
pub fn build(root: &Path, cancel: &AtomicBool, progress: &mut dyn FnMut(u64, u64)) -> Option<MapTree> {
    let started = Instant::now();
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(2, 8);
    let mut builder = Builder::default();
    let finished = walk_dir(root, threads, cancel, &mut |v| builder.visit(v, progress));
    if !finished {
        return None;
    }
    progress(builder.files, builder.bytes);
    Some(builder.finish(root, started.elapsed().as_secs_f32()))
}

/// A small made-up tree for tests elsewhere in the program, shaped like a home folder: `Library/Caches` holds
/// a file, a folder and a folded entry; `code` is a chain of folders.
#[cfg(test)]
pub fn demo(root: &Path) -> MapTree {
    const GB: u64 = 1_000_000_000;
    let dir = |name: &str, bytes: u64, files: u64, children: Vec<MapNode>| MapNode {
        name: name.into(),
        kind: NodeKind::Dir,
        bytes,
        files,
        children,
    };
    let file = |name: &str, bytes: u64| MapNode { name: name.into(), kind: NodeKind::File, bytes, files: 1, children: Vec::new() };
    let smaller = |files: u64, bytes: u64| MapNode { name: String::new(), kind: NodeKind::Smaller, bytes, files, children: Vec::new() };
    let caches = dir(
        "Caches",
        50 * GB,
        2500,
        vec![
            file("big.cache", 30 * GB),
            dir("com.test.app", 15 * GB, 100, vec![file("data", 10 * GB), file("index", 5 * GB)]),
            smaller(40, 5 * GB),
        ],
    );
    let library = dir("Library", 60 * GB, 3000, vec![caches, dir("Logs", 10 * GB, 500, vec![file("app.log", 10 * GB)])]);
    let code = dir("code", 30 * GB, 1500, vec![dir("target", 30 * GB, 1500, vec![file("lib.rlib", 30 * GB)])]);
    let top = dir("", 100 * GB, 5000, vec![library, code, file("movie.mov", 5 * GB), smaller(300, 5 * GB)]);
    MapTree { root: root.to_path_buf(), node: top, unreadable: 0, files: 5000, secs: 0.4 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safety::tests::tempdir;
    use std::fs;

    fn write(path: &Path, bytes: usize) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, vec![7u8; bytes]).unwrap();
    }

    fn map(dir: &Path) -> MapTree {
        build(dir, &AtomicBool::new(false), &mut |_, _| {}).expect("not cancelled")
    }

    #[test]
    fn a_folder_adds_up_to_what_measuring_it_says_and_the_biggest_comes_first() {
        let dir = tempdir();
        write(&dir.join("a/big.bin"), 65_536);
        write(&dir.join("a/small.bin"), 4_096);
        write(&dir.join("b/mid.bin"), 16_384);
        write(&dir.join("top.txt"), 100);
        let tree = map(&dir);
        assert_eq!(tree.node.bytes, crate::sizing::measure(&dir).bytes, "the map and the sizing layer must agree");
        let names: Vec<&str> = tree.node.children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "top.txt"], "biggest first");
        assert!(tree.node.children.windows(2).all(|w| w[0].bytes >= w[1].bytes));
        let a = &tree.node.children[0];
        assert_eq!(a.kind, NodeKind::Dir);
        assert_eq!(a.children.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["big.bin", "small.bin"]);
        assert_eq!(a.children[0].kind, NodeKind::File);
        assert_eq!((tree.node.files, tree.files), (4, 4));
    }

    #[test]
    fn every_level_adds_up_to_its_children() {
        let dir = tempdir();
        for n in 0..30 {
            write(&dir.join(format!("d{}/e{}/f.bin", n % 5, n % 7)), 4_096 * (1 + n % 4));
        }
        let tree = map(&dir);
        fn check(node: &MapNode) {
            if node.kind == NodeKind::Dir && !node.children.is_empty() {
                let sum: u64 = node.children.iter().map(|c| c.bytes).sum();
                assert_eq!(sum, node.bytes, "{} does not add up to its children", node.name);
                node.children.iter().for_each(check);
            }
        }
        check(&tree.node);
    }

    #[test]
    fn a_crowded_folder_keeps_the_biggest_and_folds_the_rest_into_one_entry() {
        let dir = tempdir();
        for n in 0..(KEEP + 12) {
            write(&dir.join(format!("d{n:03}/f.bin")), 4_096 * (1 + n % 9));
        }
        let tree = map(&dir);
        assert_eq!(tree.node.children.len(), KEEP + 1, "{KEEP} kept, plus the one that holds the rest");
        let last = tree.node.children.last().unwrap();
        assert_eq!(last.kind, NodeKind::Smaller);
        assert_eq!(last.files, 12, "twelve folders of one file each were folded in");
        let sum: u64 = tree.node.children.iter().map(|c| c.bytes).sum();
        assert_eq!(sum, tree.node.bytes, "folding loses nothing");
        let smallest_kept = tree.node.children[..KEEP].iter().map(|c| c.bytes).min().unwrap();
        assert!(last.bytes <= smallest_kept * 12, "the folded ones are the small ones");
    }

    #[test]
    fn a_folder_of_many_files_names_only_its_biggest() {
        let dir = tempdir();
        for n in 0..40 {
            write(&dir.join(format!("f{n:02}.bin")), 4_096 * (1 + n));
        }
        let tree = map(&dir);
        let files: Vec<&MapNode> = tree.node.children.iter().filter(|c| c.kind == NodeKind::File).collect();
        assert_eq!(files.len(), TOP_FILES, "the walk remembers {TOP_FILES} names a folder");
        assert_eq!(files[0].name, "f39.bin");
        let rest = tree.node.children.last().unwrap();
        assert_eq!((rest.kind, rest.files), (NodeKind::Smaller, 40 - TOP_FILES as u64));
        let total: u64 = tree.node.children.iter().map(|c| c.bytes).sum();
        assert_eq!(total, tree.node.bytes);
    }

    #[test]
    fn hard_links_count_once_and_symlinks_are_not_followed() {
        let dir = tempdir();
        write(&dir.join("inside/a"), 16_384);
        fs::hard_link(dir.join("inside/a"), dir.join("inside/b")).unwrap();
        let outside = tempdir();
        write(&outside.join("huge.bin"), 1_000_000);
        std::os::unix::fs::symlink(&*outside, dir.join("link")).unwrap();
        let tree = map(&dir);
        let physical = crate::sizing::facts_from_std(&fs::metadata(dir.join("inside/a")).unwrap()).allocated;
        let inside = tree.node.children.iter().find(|c| c.name == "inside").unwrap();
        assert_eq!(inside.bytes, physical, "two names for one file are one file's bytes");
        assert!(tree.node.bytes < physical + 4_096, "the link's target was counted: {}", tree.node.bytes);
    }

    #[test]
    fn a_folder_that_cannot_be_opened_is_counted_and_does_not_stop_the_map() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir();
        write(&dir.join("ok/f.bin"), 8_192);
        write(&dir.join("locked/secret.bin"), 8_192);
        fs::set_permissions(dir.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
        let tree = map(&dir);
        fs::set_permissions(dir.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(tree.unreadable >= 1, "the locked folder was not reported");
        let ok = tree.node.children.iter().find(|c| c.name == "ok").expect("the rest of the map is there");
        assert!(ok.bytes >= 8_192);
    }

    #[test]
    fn an_empty_folder_maps_to_nothing_and_a_cancelled_walk_returns_nothing() {
        let dir = tempdir();
        let tree = map(&dir);
        assert_eq!((tree.node.bytes, tree.node.children.len(), tree.files), (0, 0, 0));
        write(&dir.join("x/f.bin"), 4_096);
        assert!(
            build(&dir, &AtomicBool::new(true), &mut |_, _| {}).is_none(),
            "a cancelled walk must not pass off half a map as a whole one"
        );
    }

    /// `BEGONE_MAP_BENCH=/some/folder cargo test --release map_speed -- --ignored --nocapture`
    #[test]
    #[ignore = "a measurement, not a check"]
    fn map_speed() {
        let Some(dir) = std::env::var_os("BEGONE_MAP_BENCH") else { return };
        let started = Instant::now();
        let tree = build(Path::new(&dir), &AtomicBool::new(false), &mut |_, _| {}).unwrap();
        fn count(n: &MapNode) -> usize {
            1 + n.children.iter().map(count).sum::<usize>()
        }
        println!(
            "{} files, {:.1} GB, {} nodes kept, {} unreadable, built in {:.2} s (reported {:.2} s)",
            tree.files,
            tree.node.bytes as f64 / 1e9,
            count(&tree.node),
            tree.unreadable,
            started.elapsed().as_secs_f32(),
            tree.secs
        );
    }

    #[test]
    fn progress_is_reported_and_the_path_of_a_node_can_be_found_again() {
        let dir = tempdir();
        for n in 0..5000 {
            write(&dir.join(format!("p{}/f{n}", n % 10)), 0);
        }
        write(&dir.join("deep/er/est/leaf.bin"), 4_096);
        let mut heard = Vec::new();
        let tree = build(&dir, &AtomicBool::new(false), &mut |f, b| heard.push((f, b))).unwrap();
        assert!(heard.len() >= 2 && heard.windows(2).all(|w| w[0].0 <= w[1].0), "progress only moves forward: {heard:?}");
        let deep = tree.node.children.iter().position(|c| c.name == "deep").unwrap();
        let er = tree.node.children[deep].children.iter().position(|c| c.name == "er").unwrap();
        assert_eq!(tree.path_to(&[deep, er]), dir.join("deep/er"));
        assert_eq!(tree.node_at(&[deep, er]).map(|n| n.name.as_str()), Some("er"));
        assert!(tree.node_at(&[999]).is_none());
        assert_eq!(tree.path_to(&[]), dir.to_path_buf());
    }
}
