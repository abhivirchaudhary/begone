//! The places begone has been told to keep. A kept place is never offered and never moved: not as a row, not inside a
//! bigger row, and not as the bigger row that holds it. The list is a plain text file the person can read and edit.

use std::io;
use std::path::{Path, PathBuf};

use crate::safety::{Safety, lexical_normalize};

const HEADER: &str = "# Places begone never offers or moves, one per line. A leading ~/ means your home folder.\n\
# Press x on a row in begone to add one, or delete a line here to let it go.\n";

/// Where the list lives: under the home folder, so a scratch home (`--path`) has a list of its own.
pub fn file(home: &Path) -> PathBuf {
    home.join(".config/begone/keep.txt")
}

/// Read the list. A line that is empty, a comment, not a full path, or the whole home folder is skipped: keeping everything
/// would only hide everything.
pub fn load(home: &Path) -> Vec<PathBuf> {
    std::fs::read_to_string(file(home)).map(|text| parse(&text, home)).unwrap_or_default()
}

pub fn parse(text: &str, home: &Path) -> Vec<PathBuf> {
    let home = lexical_normalize(home);
    let mut out: Vec<PathBuf> = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let path = match line.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None => PathBuf::from(line),
        };
        if !path.is_absolute() {
            continue;
        }
        let path = lexical_normalize(&path);
        if path == home || path.parent().is_none() || out.contains(&path) {
            continue;
        }
        out.push(path);
    }
    out
}

/// The file's text for a list.
pub fn render(paths: &[PathBuf], home: &Path) -> String {
    let home = lexical_normalize(home);
    let mut text = HEADER.to_string();
    for path in paths {
        match path.strip_prefix(&home) {
            Ok(rest) if !rest.as_os_str().is_empty() => text.push_str(&format!("~/{}\n", rest.display())),
            _ => text.push_str(&format!("{}\n", path.display())),
        }
    }
    text
}

/// Write the list, all at once, so a crash cannot leave half of it.
pub fn save(home: &Path, paths: &[PathBuf]) -> io::Result<()> {
    let file = file(home);
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let scratch = file.with_extension("txt.tmp");
    std::fs::write(&scratch, render(paths, home))?;
    std::fs::rename(scratch, file)
}

/// Add `paths` to the list, or take them off it, in the safety layer that guards every move and on disk.
pub fn apply(safety: &Safety, home: &Path, paths: &[PathBuf], keep: bool) -> io::Result<()> {
    let mut list = safety.kept();
    for path in paths.iter().map(|p| lexical_normalize(p)) {
        if keep {
            if !list.contains(&path) {
                list.push(path);
            }
        } else {
            list.retain(|k| *k != path);
        }
    }
    safety.set_kept(list.clone());
    save(home, &list)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safety::tests::tempdir;

    #[test]
    fn the_file_is_plain_text_with_the_home_folder_written_as_a_tilde() {
        let home = Path::new("/Users/me");
        let text = render(&[home.join("code/app/node_modules"), PathBuf::from("/Applications/Figma.app")], home);
        assert!(text.starts_with("# Places begone never offers"));
        assert!(text.contains("\n~/code/app/node_modules\n") && text.contains("\n/Applications/Figma.app\n"), "{text}");
        assert_eq!(parse(&text, home), [home.join("code/app/node_modules"), PathBuf::from("/Applications/Figma.app")]);
    }

    #[test]
    fn what_is_not_a_place_to_keep_is_skipped_and_nothing_is_kept_twice() {
        let home = Path::new("/Users/me");
        let text = "# a comment\n\n  ~/a/b  \n~/a/b\n~/a/../a/b\nrelative/path\n~\n~/\n/\n/Users/me\n/Users/me/\nnot a path at all\n/Applications/X.app\n";
        assert_eq!(parse(text, home), [home.join("a/b"), PathBuf::from("/Applications/X.app")]);
    }

    #[test]
    fn saving_and_loading_round_trip_through_a_home_of_its_own() {
        let home = tempdir();
        assert!(load(&home).is_empty(), "no file, nothing kept");
        let list = vec![home.join("code/a"), home.join("Library/Caches/com.vendor.app")];
        save(&home, &list).unwrap();
        assert_eq!(load(&home), list);
        assert!(file(&home).starts_with(&*home) && file(&home).ends_with(".config/begone/keep.txt"));
        assert!(!file(&home).with_extension("txt.tmp").exists(), "the scratch copy is renamed away");
        save(&home, &[]).unwrap();
        assert!(load(&home).is_empty());
    }

    #[test]
    fn apply_updates_the_guard_and_the_file_together() {
        let home = tempdir();
        let safety = Safety::for_home(&home);
        let a = home.join("code/a");
        apply(&safety, &home, &[a.clone(), a.clone()], true).unwrap();
        assert_eq!(safety.kept(), std::slice::from_ref(&a), "once");
        assert_eq!(load(&home), std::slice::from_ref(&a));
        assert!(safety.is_kept(&a.join("target")) && safety.is_kept(&home.join("code")), "inside it, and what holds it");
        apply(&safety, &home, std::slice::from_ref(&a), false).unwrap();
        assert!(safety.kept().is_empty() && load(&home).is_empty() && !safety.is_kept(&a));
    }
}
