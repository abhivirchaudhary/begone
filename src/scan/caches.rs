//! `~/Library/Caches`: every app stashes regenerable data here.
//!
//! One candidate per top-level folder, never individual files, because deleting half a
//! cache can confuse an app more than deleting all of it. Caches of apps that are running
//! right now, and Apple's own, are listed but left unchecked.

use std::fs;

use crate::model::{Evidence, Kind, SafetyClass, Tag};
use crate::safety::Grant;
use crate::scan::{Candidate, Ctx};

pub fn discover(ctx: &Ctx<'_>) -> Vec<Candidate> {
    let root = ctx.home.join("Library/Caches");
    let Ok(entries) = fs::read_dir(&root) else { return Vec::new() };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        if ctx.cancelled() {
            break;
        }
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.to_lowercase().contains("begone") {
            continue;
        }
        let mut cand = Candidate::new(entry.path(), Kind::Caches, Grant::Allowlisted, &name)
            .src("app cache")
            .explain("A cache an app keeps so it starts faster. It holds nothing you made.", "The app rebuilds it the next time it runs.")
            .ev(Evidence::good("Inside ~/Library/Caches, where apps expect to rebuild"));
        if let Some(app) = ctx.procs.app_for(&name) {
            cand = cand.class(SafetyClass::Rebuildable, Tag::Running).unchecked().ev(Evidence::warn(format!("{app} is running right now")));
        } else if name.starts_with("com.apple.") {
            cand = cand
                .class(SafetyClass::Rebuildable, Tag::System)
                .unchecked()
                .ev(Evidence::warn("Made by Apple. It rebuilds, so it stays unchecked unless you choose it"));
        } else {
            cand = cand.ev(Evidence::good("No running app uses it"));
        }
        out.push(cand);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safety::tests::tempdir;
    use crate::scan::procs::Procs;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn lists_top_level_folders_only_and_never_files_or_begone() {
        let home = tempdir();
        let caches = home.join("Library/Caches");
        fs::create_dir_all(caches.join("com.example.one/nested/deep")).unwrap();
        fs::create_dir_all(caches.join("com.example.two")).unwrap();
        fs::create_dir_all(caches.join("com.example.begone")).unwrap();
        fs::write(caches.join("stray-file"), b"x").unwrap();
        let cancel = AtomicBool::new(false);
        let ctx = Ctx { home: &home, procs: &Procs::none(), cancel: &cancel };
        let mut names: Vec<String> = discover(&ctx).iter().map(|c| c.title.clone()).collect();
        names.sort();
        assert_eq!(names, ["com.example.one", "com.example.two"]);
    }

    #[test]
    fn running_and_apple_caches_are_listed_but_unchecked() {
        let home = tempdir();
        for name in ["com.spotify.client", "com.apple.Safari", "com.quiet.app"] {
            fs::create_dir_all(home.join("Library/Caches").join(name)).unwrap();
        }
        let cancel = AtomicBool::new(false);
        let procs = Procs::with(&[("com.spotify.client", "Spotify")]);
        let ctx = Ctx { home: &home, procs: &procs, cancel: &cancel };
        let found = discover(&ctx);
        let by = |n: &str| found.iter().find(|c| c.title == n).unwrap();
        assert_eq!((by("com.spotify.client").tag, by("com.spotify.client").pre_selected), (Tag::Running, false));
        assert_eq!((by("com.apple.Safari").tag, by("com.apple.Safari").pre_selected), (Tag::System, false));
        assert_eq!((by("com.quiet.app").tag, by("com.quiet.app").pre_selected), (Tag::Safe, true));
        assert!(by("com.spotify.client").evidence.iter().any(|e| !e.ok && e.text.contains("Spotify")));
    }
}
