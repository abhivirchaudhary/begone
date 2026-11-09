//! Moving things to the Trash, and getting them back.
//!
//! Nothing here deletes anything. On a real Mac the move goes through
//! `NSFileManager -trashItemAtURL:resultingItemURL:error:`, the call Finder makes, for two
//! reasons: Finder understands the metadata it writes, and the call hands back the
//! *destination*, which is the whole undo story. A sandboxed home (`--path`) uses a plain
//! folder as its Trash instead, so trying begone on a scratch folder never touches your real one.

use std::path::{Path, PathBuf};

/// Something that can move a path to a Trash and say where it landed.
pub trait Trasher: Send + Sync {
    /// Returns where the item now lives, if the platform told us.
    fn trash(&self, path: &Path) -> Result<Option<PathBuf>, String>;
}

/// The user's real Trash.
pub struct SystemTrash;

impl Trasher for SystemTrash {
    fn trash(&self, path: &Path) -> Result<Option<PathBuf>, String> {
        system_trash(path)
    }
}

/// A folder standing in for the Trash, for a sandboxed home.
pub struct DirTrash(pub PathBuf);

impl Trasher for DirTrash {
    fn trash(&self, path: &Path) -> Result<Option<PathBuf>, String> {
        std::fs::create_dir_all(&self.0).map_err(|err| format!("could not create {}: {err}", self.0.display()))?;
        let name = path.file_name().ok_or_else(|| format!("{} has no name", path.display()))?.to_string_lossy().into_owned();
        let mut dest = self.0.join(&name);
        let mut n = 2;
        while dest.symlink_metadata().is_ok() {
            dest = self.0.join(format!("{name} {n}"));
            n += 1;
        }
        std::fs::rename(path, &dest).map_err(|err| format!("could not move {} to {}: {err}", path.display(), dest.display()))?;
        Ok(Some(dest))
    }
}

/// Undo a trash: move `from` (inside the Trash) back to `to` (its old home).
///
/// Refuses to clobber: if something exists at `to` now, it stops rather than merge two
/// different trees. `rename` never follows a symlink at the source, so a trashed symlink
/// comes back as a symlink.
pub fn restore(from: &Path, to: &Path) -> Result<(), String> {
    if from.symlink_metadata().is_err() {
        return Err(format!("{} is no longer in the Trash, so it may have been emptied", from.display()));
    }
    if to.symlink_metadata().is_ok() {
        return Err(format!("{} already exists again, so restoring would overwrite it", to.display()));
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(|err| format!("could not recreate {}: {err}", parent.display()))?;
    }
    std::fs::rename(from, to).map_err(|err| format!("could not move {} back to {}: {err}", from.display(), to.display()))
}

#[cfg(target_os = "macos")]
fn system_trash(path: &Path) -> Result<Option<PathBuf>, String> {
    use objc2::rc::Retained;
    use objc2_foundation::{NSFileManager, NSString, NSURL};

    // Paths that are not valid UTF-8 cannot be expressed as an NSString faithfully.
    let Some(path_str) = path.to_str() else {
        return Err(format!("{} is not valid UTF-8", path.display()));
    };
    if path.symlink_metadata().is_err() {
        return Err(format!("{} does not exist", path.display()));
    }
    let url = NSURL::fileURLWithPath(&NSString::from_str(path_str));
    let file_manager = NSFileManager::defaultManager();
    // The `_error:` selector returns a Result; only the out-parameter may be uninitialised,
    // and it is read only on the success path.
    let mut destination: Option<Retained<NSURL>> = None;
    match file_manager.trashItemAtURL_resultingItemURL_error(&url, Some(&mut destination)) {
        Ok(()) => Ok(destination.as_ref().and_then(|url| url.path()).map(|p| PathBuf::from(p.to_string()))),
        Err(err) => Err(err.localizedDescription().to_string()),
    }
}

#[cfg(not(target_os = "macos"))]
fn system_trash(path: &Path) -> Result<Option<PathBuf>, String> {
    trash::delete(path).map(|()| None).map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safety::tests::tempdir;
    use std::fs;

    #[test]
    fn restore_refuses_to_clobber() {
        let root = tempdir();
        let (from, to) = (root.join("from"), root.join("to"));
        fs::create_dir_all(&from).unwrap();
        fs::create_dir_all(&to).unwrap();
        fs::write(to.join("precious"), b"do not overwrite").unwrap();
        let err = restore(&from, &to).expect_err("must refuse");
        assert!(err.contains("already exists"), "{err}");
        assert_eq!(fs::read(to.join("precious")).unwrap(), b"do not overwrite");
    }

    #[test]
    fn restore_moves_a_trashed_tree_back_and_recreates_parents() {
        let root = tempdir();
        let from = root.join("trash/item");
        let to = root.join("home/deep/item");
        fs::create_dir_all(&from).unwrap();
        fs::write(from.join("a.txt"), b"hi").unwrap();
        restore(&from, &to).unwrap();
        assert_eq!(fs::read(to.join("a.txt")).unwrap(), b"hi");
        assert!(!from.exists());
    }

    #[test]
    fn restore_reports_a_vanished_item() {
        let root = tempdir();
        let err = restore(&root.join("gone"), &root.join("somewhere")).expect_err("must fail");
        assert!(err.contains("no longer in the Trash"), "{err}");
    }

    #[test]
    fn a_dir_trash_numbers_duplicates_instead_of_overwriting() {
        let root = tempdir();
        let trash = DirTrash(root.join(".Trash"));
        for round in 0..3 {
            let file = root.join("same.txt");
            fs::write(&file, format!("round {round}")).unwrap();
            let dest = trash.trash(&file).unwrap().unwrap();
            assert!(dest.exists() && !file.exists());
        }
        assert_eq!(fs::read_dir(root.join(".Trash")).unwrap().count(), 3);
    }

    /// The only test that touches the real Finder Trash, and it puts everything back.
    /// If this fails, undo is a lie.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_real_file_survives_the_round_trip_through_the_system_trash() {
        let root = tempdir();
        let original = root.join("begone-roundtrip.txt");
        fs::write(&original, b"proof that undo is real").unwrap();
        let was = crate::journal::identity(&original).expect("it exists");
        let dest = SystemTrash.trash(&original).expect("the OS should trash this").expect("macOS must say where it landed");
        assert!(!original.exists());
        // "Empty only what begone moved" knows an entry by the file it is. That only works if the system's Trash keeps it.
        assert_eq!(crate::journal::identity(&dest), Some(was), "the system Trash moves the very same file, not a copy");
        restore(&dest, &original).expect("it must come back");
        assert_eq!(fs::read(&original).unwrap(), b"proof that undo is real");
        // The same for a folder, which is what most of begone's items are.
        let folder = root.join("begone-roundtrip-folder");
        fs::create_dir_all(folder.join("inner")).unwrap();
        fs::write(folder.join("inner/x"), b"x").unwrap();
        let was = crate::journal::identity(&folder).expect("it exists");
        let dest = SystemTrash.trash(&folder).expect("the OS should trash this").expect("macOS must say where it landed");
        assert_eq!(crate::journal::identity(&dest), Some(was), "a folder keeps its identity through the system Trash too");
        restore(&dest, &folder).expect("it must come back");
        assert!(folder.join("inner/x").exists());
    }
}
