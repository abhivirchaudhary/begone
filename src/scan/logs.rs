//! `~/Library/Logs`: rotated logs, crash reports, install logs.
//!
//! Individual files, not folders, and only ones untouched for a week, so an active
//! debugging session is never disturbed. The root is walked once, so a file can never
//! be listed twice (it used to be, through overlapping roots).

use std::fs;
use std::path::Path;
use std::time::Duration;

use crate::model::{Evidence, Kind};
use crate::safety::Grant;
use crate::scan::{Candidate, Ctx};

/// Logs younger than this are left alone: somebody is probably reading them.
const MIN_AGE: Duration = Duration::from_secs(7 * 86_400);
const MAX_DEPTH: usize = 4;
const MAX_FILES: usize = 10_000;

pub fn discover(ctx: &Ctx<'_>) -> Vec<Candidate> {
    let root = ctx.home.join("Library/Logs");
    let mut out = Vec::new();
    walk(&root, &root, 0, ctx, &mut out);
    out
}

fn walk(root: &Path, dir: &Path, depth: usize, ctx: &Ctx<'_>, out: &mut Vec<Candidate>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        if ctx.cancelled() || out.len() >= MAX_FILES {
            return;
        }
        let Ok(kind) = entry.file_type() else { continue };
        let path = entry.path();
        if kind.is_dir() {
            if depth < MAX_DEPTH {
                walk(root, &path, depth + 1, ctx, out);
            }
        } else if kind.is_file() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == ".DS_Store" {
                continue;
            }
            let source = path
                .strip_prefix(root)
                .ok()
                .and_then(|p| p.components().next())
                .map_or_else(|| "Logs".to_string(), |c| c.as_os_str().to_string_lossy().into_owned());
            let source = if source == name { "Logs".to_string() } else { source };
            out.push(
                Candidate::new(path, Kind::Logs, Grant::Allowlisted, name)
                    .src(source)
                    .explain("A log file. Nothing reads it unless you are debugging.", "It does not come back, and nothing needs it.")
                    .ev(Evidence::good("Inside ~/Library/Logs, where apps recreate what they need"))
                    .idle_at_least(MIN_AGE),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safety::tests::tempdir;
    use crate::scan::procs::Procs;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn every_log_is_listed_once_even_in_the_old_overlapping_folders() {
        let home = tempdir();
        let logs = home.join("Library/Logs");
        fs::create_dir_all(logs.join("DiagnosticReports")).unwrap();
        fs::create_dir_all(logs.join("SomeApp/nested")).unwrap();
        for f in ["DiagnosticReports/old.crash", "SomeApp/old.log", "SomeApp/nested/deep.log", "top.log", ".DS_Store"] {
            fs::write(logs.join(f), b"x").unwrap();
        }
        let cancel = AtomicBool::new(false);
        let ctx = Ctx { home: &home, procs: &Procs::none(), cancel: &cancel };
        let found = discover(&ctx);
        let mut paths: Vec<_> = found.iter().map(|c| c.path.clone()).collect();
        let total = paths.len();
        paths.sort();
        paths.dedup();
        assert_eq!(paths.len(), total, "no file may appear twice");
        assert_eq!(total, 4, ".DS_Store is skipped");
        assert!(found.iter().all(|c| c.min_idle == Some(MIN_AGE)), "age is enforced after measuring");
        assert_eq!(found.iter().find(|c| c.title == "top.log").unwrap().source, "Logs");
        assert_eq!(found.iter().find(|c| c.title == "deep.log").unwrap().source, "SomeApp");
    }
}
