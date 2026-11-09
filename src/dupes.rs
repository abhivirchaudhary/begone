//! Finding files that are byte-for-byte identical, so redundant copies can be reclaimed while one is always kept.
//!
//! The work is bounded the way a careful person would do it by hand: two files can only be identical if they are the same size,
//! so a quick pass groups candidates by size and only the sizes that collide are ever read. Within a colliding size, files are
//! compared by their actual bytes (never a hash that could, in theory, collide), and a budget of time and bytes keeps even a
//! very full disk responsive. Every group keeps exactly one copy — the one in the most deliberate place, oldest among equals —
//! and offers only the others, so the finder can never point at the last copy of anything.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::safety::same_contents;

/// One set of byte-for-byte identical files.
#[derive(Clone, Debug)]
pub struct DupeGroup {
    /// The copy that stays: the one in the most deliberate place, oldest among equals.
    pub keeper: PathBuf,
    /// The copies that could go. Never empty, and never the keeper.
    pub copies: Vec<PathBuf>,
}

#[derive(Clone, Copy, Debug)]
pub struct DupeOptions {
    /// Files smaller than this are not worth the risk or the noise: duplicate thumbnails save nothing.
    pub min_size: u64,
    /// Stop reading once this many bytes have been compared, so a huge disk cannot stall the scan.
    pub byte_budget: u64,
    /// Stop looking after this long, for the same reason.
    pub time_budget: Duration,
}

impl Default for DupeOptions {
    fn default() -> Self {
        Self { min_size: 1_000_000, byte_budget: 6 * 1_000_000_000, time_budget: Duration::from_secs(25) }
    }
}

/// Folder names never descended into: another scanner owns them, they are rebuilt from nothing, or they are one bundle rather
/// than a pile of files. Everything beginning with a dot is skipped too, so caches and version-control stores are left alone.
const SKIP_DIRS: &[&str] = &["node_modules", "target", "Pods", "__pycache__", "build", "Library"];

/// Suffixes of folders that are really one document: a photo or music library, an app, a Logic project. What is inside is the
/// library's business, not a loose pile to dedupe.
const SKIP_SUFFIX: &[&str] =
    &[".app", ".photoslibrary", ".photolibrary", ".musiclibrary", ".tvlibrary", ".imovielibrary", ".fcpbundle", ".logicx", ".band"];

fn skip_dir(name: &str) -> bool {
    name.starts_with('.') || SKIP_DIRS.contains(&name) || SKIP_SUFFIX.iter().any(|s| name.ends_with(s))
}

/// How deliberate a file's location is, so the copy that stays is the one a person most likely means to keep. Higher stays.
fn preciousness(path: &Path, home: &Path) -> u8 {
    match path.strip_prefix(home).ok().and_then(|rel| rel.components().next()).and_then(|c| c.as_os_str().to_str()) {
        Some("Documents") => 5,
        Some("Pictures" | "Music" | "Movies") => 4,
        Some("Desktop") => 2,
        Some("Downloads") => 1,
        _ => 3,
    }
}

fn modified(path: &Path) -> Option<std::time::SystemTime> {
    path.metadata().ok().and_then(|m| m.modified().ok())
}

/// Of a set of identical files, the one to keep: the most deliberate place, then the oldest, then the shortest, steady path.
fn keeper_of(paths: &[PathBuf], home: &Path) -> PathBuf {
    paths
        .iter()
        .max_by(|a, b| {
            preciousness(a, home)
                .cmp(&preciousness(b, home))
                // An older file is the original; a newer one is the copy.
                .then_with(|| modified(b).cmp(&modified(a)))
                .then_with(|| b.components().count().cmp(&a.components().count()))
                .then_with(|| b.cmp(a))
        })
        .cloned()
        .expect("a group is never empty")
}

/// Walk a folder, adding every regular file of at least `min_size` to `by_size`, skipping links, caches, bundles and the
/// folders other scanners own. Each real file is counted once, so a hard link does not look like a duplicate of itself.
fn walk(
    root: &Path,
    opts: &DupeOptions,
    cancel: &AtomicBool,
    start: Instant,
    by_size: &mut HashMap<u64, Vec<PathBuf>>,
    seen: &mut HashSet<(u64, u64)>,
) {
    use std::os::unix::fs::MetadataExt;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if cancel.load(Ordering::Relaxed) || start.elapsed() > opts.time_budget {
            return;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else { continue };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                if !skip_dir(&entry.file_name().to_string_lossy()) {
                    stack.push(entry.path());
                }
            } else if file_type.is_file() {
                let Ok(meta) = entry.metadata() else { continue };
                if meta.len() < opts.min_size {
                    continue;
                }
                // One file, counted once: a hard link shares the identity and is not a separate copy.
                if seen.insert((meta.dev(), meta.ino())) {
                    by_size.entry(meta.len()).or_default().push(entry.path());
                }
            }
        }
    }
}

/// Find the groups of identical files across `roots`, keeping one copy in each, and hand each group to `found` as it is
/// confirmed. `home` decides which copy stays. Stops politely at the budgets or when cancelled; the return says whether it had
/// to stop early, so the screen can say the list may be incomplete.
pub fn find(roots: &[PathBuf], home: &Path, opts: &DupeOptions, cancel: &AtomicBool, found: &mut dyn FnMut(DupeGroup)) -> bool {
    let start = Instant::now();
    let mut by_size: HashMap<u64, Vec<PathBuf>> = HashMap::new();
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    for root in roots {
        walk(root, opts, cancel, start, &mut by_size, &mut seen);
    }
    let mut truncated = cancel.load(Ordering::Relaxed) || start.elapsed() > opts.time_budget;
    let mut spent: u64 = 0;
    // Bigger files first: the copies worth the most come back soonest, before any budget runs out.
    let mut sizes: Vec<(u64, Vec<PathBuf>)> = by_size.into_iter().filter(|(_, paths)| paths.len() > 1).collect();
    sizes.sort_by_key(|(size, _)| std::cmp::Reverse(*size));
    for (size, paths) in sizes {
        if cancel.load(Ordering::Relaxed) || start.elapsed() > opts.time_budget || spent > opts.byte_budget {
            truncated = true;
            break;
        }
        // Cluster the same-size files by their actual contents. Each new file is compared with one representative per cluster.
        let mut clusters: Vec<Vec<PathBuf>> = Vec::new();
        for path in paths {
            if spent > opts.byte_budget {
                truncated = true;
                break;
            }
            let mut placed = false;
            for cluster in &mut clusters {
                spent = spent.saturating_add(size);
                if same_contents(&cluster[0], &path).unwrap_or(false) {
                    cluster.push(path.clone());
                    placed = true;
                    break;
                }
            }
            if !placed {
                clusters.push(vec![path]);
            }
        }
        for cluster in clusters {
            if cluster.len() < 2 {
                continue;
            }
            let keeper = keeper_of(&cluster, home);
            let copies: Vec<PathBuf> = cluster.into_iter().filter(|p| *p != keeper).collect();
            if !copies.is_empty() {
                found(DupeGroup { keeper, copies });
            }
        }
    }
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(path: &Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn all(home: &Path, roots: &[&str]) -> Vec<DupeGroup> {
        let roots: Vec<PathBuf> = roots.iter().map(|r| home.join(r)).collect();
        let opts = DupeOptions { min_size: 10, ..DupeOptions::default() };
        let mut groups = Vec::new();
        let truncated = find(&roots, home, &opts, &AtomicBool::new(false), &mut |g| groups.push(g));
        assert!(!truncated, "the test set is tiny");
        groups
    }

    #[test]
    fn identical_files_are_grouped_and_the_copy_kept_is_the_one_in_the_most_deliberate_place() {
        let home = crate::safety::tests::tempdir();
        let a = vec![7u8; 20_000];
        let b = vec![8u8; 20_000]; // same size as a, different bytes: must not be grouped with a
        write(&home.join("Downloads/report.pdf"), &a);
        write(&home.join("Documents/report.pdf"), &a);
        write(&home.join("Desktop/also.pdf"), &a);
        write(&home.join("Downloads/other.pdf"), &b);
        let groups = all(&home, &["Downloads", "Documents", "Desktop"]);
        assert_eq!(groups.len(), 1, "one group of three identical files; the odd file of the same size is on its own");
        let g = &groups[0];
        assert_eq!(g.keeper, home.join("Documents/report.pdf"), "the copy in Documents is the one kept");
        let mut copies = g.copies.clone();
        copies.sort();
        assert_eq!(copies, vec![home.join("Desktop/also.pdf"), home.join("Downloads/report.pdf")]);
        assert!(!g.copies.contains(&g.keeper));
    }

    #[test]
    fn a_lone_file_a_small_file_and_a_hard_link_are_not_duplicates() {
        let home = crate::safety::tests::tempdir();
        write(&home.join("Downloads/alone.bin"), &vec![1u8; 20_000]);
        // Two tiny identical files are below the size floor.
        write(&home.join("Downloads/tiny-a.txt"), b"hi");
        write(&home.join("Downloads/tiny-b.txt"), b"hi");
        // A hard link is the same file, not a copy.
        let real = home.join("Downloads/real.bin");
        write(&real, &vec![5u8; 30_000]);
        fs::hard_link(&real, home.join("Downloads/link.bin")).unwrap();
        let groups = all(&home, &["Downloads"]);
        assert!(groups.is_empty(), "nothing here is a removable duplicate: {groups:?}");
    }

    #[test]
    fn a_symlink_and_the_skipped_folders_are_left_out() {
        let home = crate::safety::tests::tempdir();
        let payload = vec![3u8; 40_000];
        write(&home.join("Documents/keep.iso"), &payload);
        // A copy inside a build folder and inside a library is never offered.
        write(&home.join("Desktop/project/target/copy.iso"), &payload);
        write(&home.join("Pictures/Photos.photoslibrary/originals/copy.iso"), &payload);
        // A symlink to the kept file is not a copy of it.
        std::os::unix::fs::symlink(home.join("Documents/keep.iso"), home.join("Desktop/link.iso")).unwrap();
        let groups = all(&home, &["Documents", "Desktop", "Pictures"]);
        assert!(groups.is_empty(), "the only other copies are in a build folder and a library, which are skipped: {groups:?}");
    }
}
