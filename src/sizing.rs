//! Measuring what removing a path would really free.
//!
//! `du`-style totals overstate in two ways on a Mac. Hard links are counted once per
//! name, yet removing one name frees nothing while another name remains. And APFS
//! clones are counted once per copy, yet they share their data. So a hard-linked file
//! only counts when *every* one of its names is inside the thing being measured, and a
//! clone group counts once. That is what makes a pnpm `node_modules`, which hard-links
//! into a shared store, show its true cost instead of a flattering lie.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::SystemTime;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Measure {
    pub bytes: u64,
    pub files: u64,
    /// The newest modification time anywhere inside, so it reads as "last used".
    pub newest: Option<SystemTime>,
}

/// What a walker reports about one non-directory entry.
#[derive(Clone, Copy, Debug, Default)]
pub struct Facts {
    pub dev: u64,
    pub ino: u64,
    pub nlink: u64,
    pub allocated: u64,
    /// The data fork's share of `allocated`, which is the part clones share.
    pub data_allocated: u64,
    pub clone_id: Option<u64>,
    pub modified: Option<SystemTime>,
}

struct Link {
    nlink: u64,
    seen: u64,
    bytes: u64,
}

/// Accumulates facts and applies the hard-link and clone rules.
#[derive(Default)]
pub struct Tally {
    bytes: u64,
    files: u64,
    newest: Option<SystemTime>,
    links: HashMap<(u64, u64), Link>,
    clones: HashSet<(u64, u64)>,
}

impl Tally {
    /// A directory contributes its modification time and nothing else.
    pub fn touch(&mut self, modified: Option<SystemTime>) {
        if modified > self.newest {
            self.newest = modified;
        }
    }

    pub fn add(&mut self, f: &Facts) {
        self.files += 1;
        self.touch(f.modified);
        let mut bytes = f.allocated;
        if let Some(id) = f.clone_id
            && !self.clones.insert((f.dev, id))
        {
            // Another clone of this data is already counted; only what is not shared is new.
            bytes = f.allocated.saturating_sub(f.data_allocated);
        }
        if f.nlink > 1 {
            let link = self.links.entry((f.dev, f.ino)).or_insert(Link { nlink: f.nlink, seen: 0, bytes });
            link.seen += 1;
        } else {
            self.bytes += bytes;
        }
    }

    pub fn finish(self) -> Measure {
        let linked: u64 = self.links.values().filter(|l| l.seen >= l.nlink).map(|l| l.bytes).sum();
        Measure { bytes: self.bytes + linked, files: self.files, newest: self.newest }
    }
}

/// Facts for one entry from `lstat`. Blocks are 512 bytes whatever the filesystem says.
pub fn facts_from_std(meta: &std::fs::Metadata) -> Facts {
    use std::os::unix::fs::MetadataExt;
    let allocated = meta.blocks() * 512;
    Facts {
        dev: meta.dev(),
        ino: meta.ino(),
        nlink: meta.nlink(),
        allocated,
        data_allocated: allocated,
        clone_id: None,
        modified: meta.modified().ok(),
    }
}

/// Measure a file or a whole tree. Symlinks are never followed.
pub fn measure(path: &Path) -> Measure {
    measure_with(path, std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(2, 8))
}

/// Like [`measure`], with a cap on the walker's threads, so several trees can be measured at once.
pub fn measure_with(path: &Path, threads: usize) -> Measure {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return Measure::default();
    };
    let mut tally = Tally::default();
    if meta.is_dir() {
        tally.touch(meta.modified().ok());
        walk_tree(path, threads, &mut tally);
    } else {
        tally.add(&facts_from_std(&meta));
    }
    tally.finish()
}

#[cfg(target_os = "macos")]
fn walk_tree(path: &Path, threads: usize, tally: &mut Tally) {
    use dua_core::{Options, Order, walk};
    let mut options = Options::default();
    options.apfs_clone_metadata = true;
    for entry in walk(path, threads.max(1), Order::Completion, options, |_| true).flatten() {
        let Some(Ok(meta)) = entry.metadata else { continue };
        if entry.file_type.is_dir() {
            tally.touch(meta.modified().ok());
            continue;
        }
        tally.add(&Facts {
            dev: meta.dev(),
            ino: meta.ino(),
            nlink: meta.nlink(),
            allocated: meta.allocated_size(),
            data_allocated: meta.data_allocated_size(),
            clone_id: meta.clone_id().map(std::num::NonZeroU64::get),
            modified: meta.modified().ok(),
        });
    }
}

#[cfg(not(target_os = "macos"))]
fn walk_tree(path: &Path, _threads: usize, tally: &mut Tally) {
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else { continue };
            if meta.is_dir() {
                tally.touch(meta.modified().ok());
                stack.push(entry.path());
            } else {
                tally.add(&facts_from_std(&meta));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safety::tests::tempdir;
    use std::fs;
    use std::time::Duration;

    fn facts(ino: u64, nlink: u64, bytes: u64) -> Facts {
        Facts { dev: 1, ino, nlink, allocated: bytes, data_allocated: bytes, ..Facts::default() }
    }

    #[test]
    fn plain_files_add_up() {
        let mut t = Tally::default();
        t.add(&facts(1, 1, 4096));
        t.add(&facts(2, 1, 8192));
        let m = t.finish();
        assert_eq!((m.bytes, m.files), (12_288, 2));
    }

    #[test]
    fn a_hard_link_counts_once_when_every_name_is_inside() {
        let mut t = Tally::default();
        t.add(&facts(7, 2, 4096));
        t.add(&facts(7, 2, 4096));
        assert_eq!(t.finish().bytes, 4096);
    }

    #[test]
    fn a_hard_link_that_lives_on_elsewhere_frees_nothing() {
        // The pnpm case: this tree holds one of three names; the store keeps the rest.
        let mut t = Tally::default();
        t.add(&facts(7, 3, 4096));
        t.add(&facts(8, 1, 100));
        assert_eq!(t.finish().bytes, 100);
    }

    #[test]
    fn clones_share_their_data_but_not_their_metadata() {
        let mut t = Tally::default();
        let clone = |ino| Facts { dev: 1, ino, nlink: 1, allocated: 10_000, data_allocated: 8_000, clone_id: Some(99), modified: None };
        t.add(&clone(1));
        t.add(&clone(2));
        assert_eq!(t.finish().bytes, 12_000, "the data once, plus each copy's own 2000");
    }

    #[test]
    fn the_newest_modification_anywhere_inside_wins() {
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let new = SystemTime::UNIX_EPOCH + Duration::from_secs(9_000);
        let mut t = Tally::default();
        t.add(&Facts { modified: Some(old), ..facts(1, 1, 1) });
        t.touch(Some(new));
        t.add(&Facts { modified: Some(old), ..facts(2, 1, 1) });
        assert_eq!(t.finish().newest, Some(new));
    }

    #[test]
    fn measuring_a_real_tree_counts_files_and_bytes() {
        let dir = tempdir();
        fs::write(dir.join("a.bin"), vec![1u8; 4096]).unwrap();
        fs::create_dir(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/b.bin"), vec![2u8; 8192]).unwrap();
        let m = measure(&dir);
        assert_eq!(m.files, 2);
        assert!(m.bytes >= 12_288, "physical size is at least the logical size, got {}", m.bytes);
        assert!(m.newest.is_some());
    }

    #[test]
    fn a_symlink_adds_nothing_and_is_not_followed() {
        let dir = tempdir();
        let real = dir.join("real");
        fs::create_dir(&real).unwrap();
        fs::write(real.join("big.bin"), vec![3u8; 65_536]).unwrap();
        let tree = dir.join("tree");
        fs::create_dir(&tree).unwrap();
        let alone = measure(&tree).bytes;
        std::os::unix::fs::symlink(&real, tree.join("link")).unwrap();
        assert!(measure(&tree).bytes < alone + 4096, "the link target must not be counted");
    }

    #[test]
    fn real_hard_links_follow_the_all_names_rule() {
        let dir = tempdir();
        let inside = dir.join("inside");
        fs::create_dir(&inside).unwrap();
        fs::write(inside.join("a"), vec![4u8; 16_384]).unwrap();
        fs::hard_link(inside.join("a"), inside.join("b")).unwrap();
        let physical = facts_from_std(&fs::metadata(inside.join("a")).unwrap()).allocated;
        assert_eq!(measure(&inside).bytes, physical, "both names inside: counted once, not twice");
        assert_eq!(measure(&inside.join("a")).bytes, 0, "one name alone: the other keeps the blocks alive");

        let tree = dir.join("tree");
        fs::create_dir(&tree).unwrap();
        fs::hard_link(inside.join("a"), tree.join("c")).unwrap();
        assert_eq!(measure(&tree).bytes, 0, "the other names live elsewhere: nothing would be freed");
    }

    #[test]
    fn a_missing_path_measures_zero() {
        assert_eq!(measure(Path::new("/nonexistent/begone/path")), Measure::default());
    }
}
