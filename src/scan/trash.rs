//! `~/.Trash`: things already thrown away. begone never moves these; it only reports them,
//! because emptying the Trash is the one step that cannot be undone and is therefore its
//! own deliberate action.

use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::journal;
use crate::sizing;

#[derive(Clone, Debug)]
pub struct TrashEntry {
    pub path: PathBuf,
    pub bytes: u64,
    /// begone moved it here: the journal knows this very file, not just a name like it.
    pub begone: bool,
}

#[derive(Clone, Debug, Default)]
pub struct TrashState {
    /// False when the Trash exists but cannot be read, which looks the same as empty unless said.
    pub readable: bool,
    pub entries: Vec<TrashEntry>,
    pub bytes: u64,
    pub note: Option<String>,
}

#[cfg(test)]
pub fn summarize(trash_dir: &Path) -> TrashState {
    summarize_marked(trash_dir, &HashSet::new())
}

/// The Trash, with each entry marked that is one of `mine`: the `(device, inode)` of what begone moved.
pub fn summarize_marked(trash_dir: &Path, mine: &HashSet<(u64, u64)>) -> TrashState {
    let entries = match fs::read_dir(trash_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == ErrorKind::NotFound => return TrashState { readable: true, ..TrashState::default() },
        Err(err) => {
            let note = if err.kind() == ErrorKind::PermissionDenied {
                "begone cannot read your Trash without Full Disk Access".to_string()
            } else {
                format!("the Trash could not be read ({err})")
            };
            return TrashState { readable: false, note: Some(note), ..TrashState::default() };
        }
    };
    let mut state = TrashState { readable: true, ..TrashState::default() };
    for entry in entries.flatten() {
        if entry.file_name() == ".DS_Store" {
            continue;
        }
        let measured = sizing::measure(&entry.path());
        state.bytes += measured.bytes;
        let begone = journal::identity(&entry.path()).is_some_and(|id| mine.contains(&id));
        state.entries.push(TrashEntry { path: entry.path(), bytes: measured.bytes, begone });
    }
    state.entries.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));
    state
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safety::tests::tempdir;

    #[test]
    fn lists_what_is_inside_not_the_folder_and_skips_ds_store() {
        let root = tempdir();
        let trash = root.join(".Trash");
        fs::create_dir_all(trash.join("deleted-folder")).unwrap();
        fs::write(trash.join("old-report.pdf"), vec![0u8; 8192]).unwrap();
        fs::write(trash.join(".DS_Store"), b"").unwrap();
        let state = summarize(&trash);
        assert!(state.readable);
        assert_eq!(state.entries.len(), 2);
        assert_eq!(state.entries[0].path, trash.join("old-report.pdf"), "largest first");
        assert!(state.bytes >= 8192);
    }

    #[test]
    fn a_missing_trash_is_empty_not_unreadable() {
        let root = tempdir();
        let state = summarize(&root.join(".Trash"));
        assert!(state.readable && state.entries.is_empty() && state.note.is_none());
    }

    #[test]
    fn an_unreadable_trash_says_so_instead_of_looking_empty() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempdir();
        let trash = root.join(".Trash");
        fs::create_dir_all(&trash).unwrap();
        fs::set_permissions(&trash, fs::Permissions::from_mode(0o000)).unwrap();
        let state = summarize(&trash);
        fs::set_permissions(&trash, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!state.readable, "running as root would make this readable");
        assert!(state.note.is_some());
    }
}
