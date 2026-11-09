//! Build artifacts: the biggest wins on a working machine, and the thing generic cleaners
//! and disk maps do not know how to recognise.
//!
//! A folder is only build output if a marker proves it. A `target/` is Cargo's only when
//! `Cargo.toml` sits beside it, and a `.venv` only when it holds `pyvenv.cfg`. The proof is
//! shown to you in the inspector and re-run by the safety layer when the folder is moved,
//! so a folder that merely happens to be called `dist` is never touched. The search is
//! depth-limited and skips cloud folders and package bundles, so it never becomes a
//! full-disk walk.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::model::{Evidence, Kind, SafetyClass, Tag};
use crate::safety::{ArtifactRule, Grant, artifact_rule, prove_artifact};
use crate::scan::{Candidate, Ctx};

const MAX_DEPTH: usize = 6;
const MAX_DIRS: usize = 150_000;
/// Something touched in the last week is probably still being worked on, and rebuilding it
/// costs real time, so it is listed but left unchecked.
const IN_USE: Duration = Duration::from_secs(7 * 86_400);

/// Folders directly inside home that are never searched.
const TOP_SKIP: &[&str] = &[
    "Library",
    "Applications",
    "Pictures",
    "Movies",
    "Music",
    "Public",
    "Downloads",
    "Dropbox",
    "Google Drive",
    "OneDrive",
    "Box",
    "Creative Cloud Files",
];

/// Package bundles and libraries: big, slow to walk, and never a project.
const BUNDLE_EXTENSIONS: &[&str] = &[
    "app",
    "photoslibrary",
    "imovielibrary",
    "fcpbundle",
    "lrlibrary",
    "tvlibrary",
    "musiclibrary",
    "sparsebundle",
    "framework",
    "xcodeproj",
    "xcworkspace",
    "playground",
    "bundle",
    "plugin",
];

/// Caches regenerate for free; the rest cost a build or an install.
const CHEAP: &[&str] = &["__pycache__", ".pytest_cache", ".mypy_cache", ".ruff_cache", ".turbo", ".parcel-cache"];

type Found = (std::path::PathBuf, &'static ArtifactRule, String);

pub fn discover(ctx: &Ctx<'_>) -> Vec<Candidate> {
    let mut found: Vec<Found> = Vec::new();
    let mut visited = 0;
    walk(ctx.home, 0, ctx, &mut visited, &mut found);
    // Each one asks git a question, so ask several at once.
    let chunk = found.len().div_ceil(4).max(1);
    let mut out = Vec::with_capacity(found.len());
    std::thread::scope(|scope| {
        let handles: Vec<_> = found
            .chunks(chunk)
            .map(|part| scope.spawn(move || part.iter().map(|(p, r, proof)| artifact(p, r, proof.clone(), ctx)).collect::<Vec<_>>()))
            .collect();
        for handle in handles {
            out.extend(handle.join().unwrap_or_default());
        }
    });
    out
}

fn walk(dir: &Path, depth: usize, ctx: &Ctx<'_>, visited: &mut usize, out: &mut Vec<Found>) {
    if depth > MAX_DEPTH || *visited >= MAX_DIRS || ctx.cancelled() {
        return;
    }
    *visited += 1;
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        // `DirEntry::file_type` does not follow links, so a symlinked folder is skipped.
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if let Some(rule) = artifact_rule(&name) {
            if let Ok(proof) = prove_artifact(&path) {
                out.push((path, rule, proof));
            }
            // Never descend into build output, proven or not.
            continue;
        }
        if name.starts_with('.') || (depth == 0 && TOP_SKIP.contains(&name.as_str())) {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()).is_some_and(|e| BUNDLE_EXTENSIONS.contains(&e.to_lowercase().as_str())) {
            continue;
        }
        walk(&path, depth + 1, ctx, visited, out);
    }
}

fn artifact(path: &Path, rule: &ArtifactRule, proof: String, ctx: &Ctx<'_>) -> Candidate {
    let parent = path.parent().unwrap_or(path);
    let project = parent.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let (safety, tag) = if CHEAP.contains(&rule.name) { (SafetyClass::Safe, Tag::Safe) } else { (SafetyClass::Rebuildable, Tag::Rebuilds) };
    let mut cand = Candidate::new(path.to_path_buf(), Kind::Developer, Grant::ProvenArtifact, format!("{}/", rule.name))
        .src(project)
        .class(safety, tag)
        .explain(rule.what, rule.comes_back)
        .ev(Evidence::good(proof))
        .in_use_within(IN_USE);

    if !rule.lockfiles.is_empty() {
        cand = match rule.lockfiles.iter().find(|l| parent.join(l).is_file()) {
            Some(lock) => cand.ev(Evidence::good(format!("{lock} is beside it, so the same versions come back"))),
            None => cand.ev(Evidence::warn("No lockfile beside it: reinstalling may pick newer versions")),
        };
    }
    if path.join("CACHEDIR.TAG").is_file() {
        cand = cand.ev(Evidence::good("Marked as a cache by CACHEDIR.TAG"));
    }
    match git_ignored(parent, name_of(path), ctx.home) {
        Some(true) => cand = cand.ev(Evidence::good("Ignored by git: nothing in it is tracked")),
        Some(false) => cand = cand.ev(Evidence::warn("Not ignored by git: check that nothing in it is tracked")).unchecked(),
        None => {
            if !rule.strong {
                cand = cand.ev(Evidence::warn("A generic name, so begone only chooses it when git ignores it")).unchecked();
            }
        }
    }
    if !rule.strong && !cand.evidence.iter().any(|e| e.ok && e.text.starts_with("Ignored by git")) {
        cand = cand.unchecked();
    }
    cand
}

fn name_of(path: &Path) -> &str {
    path.file_name().and_then(|n| n.to_str()).unwrap_or_default()
}

/// Does git, if it is there, ignore `name` inside `parent`? `None` when it cannot say.
fn git_ignored(parent: &Path, name: &str, home: &Path) -> Option<bool> {
    let in_repo = parent.ancestors().take_while(|a| *a != home && a.parent().is_some()).any(|a| a.join(".git").exists());
    if !in_repo {
        return None;
    }
    let status = Command::new("git")
        .arg("-C")
        .arg(parent)
        .args(["check-ignore", "-q", "--", name])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    match status.code() {
        Some(0) => Some(true),
        Some(1) => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safety::tests::tempdir;
    use crate::scan::procs::Procs;
    use std::sync::atomic::AtomicBool;

    fn write(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"x").unwrap();
    }

    fn find(home: &Path) -> Vec<Candidate> {
        let cancel = AtomicBool::new(false);
        discover(&Ctx { home, procs: &Procs::none(), cancel: &cancel })
    }

    #[test]
    fn finds_proven_artifacts_and_only_those() {
        let home = tempdir();
        write(&home.join("code/real/Cargo.toml"));
        fs::create_dir_all(home.join("code/real/target/debug")).unwrap();
        fs::create_dir_all(home.join("code/not-a-project/target")).unwrap();
        write(&home.join("code/web/package.json"));
        fs::create_dir_all(home.join("code/web/node_modules/lib/node_modules/nested")).unwrap();
        write(&home.join("Desktop/proj/package.json"));
        fs::create_dir_all(home.join("Desktop/proj/node_modules")).unwrap();
        let found = find(&home);
        let rel: Vec<String> = found.iter().map(|c| c.path.strip_prefix(&*home).unwrap().to_string_lossy().into_owned()).collect();
        assert!(rel.contains(&"code/real/target".to_string()));
        assert!(rel.contains(&"code/web/node_modules".to_string()));
        assert!(rel.contains(&"Desktop/proj/node_modules".to_string()), "a project on the Desktop still counts");
        assert!(!rel.iter().any(|p| p.contains("not-a-project")), "an unproven target must be ignored");
        assert_eq!(rel.iter().filter(|p| p.contains("node_modules")).count(), 2, "never descend into found build output");
    }

    #[test]
    fn never_searches_places_that_are_not_projects() {
        let home = tempdir();
        for base in ["Library/Caches", "Downloads", "Dropbox", "Pictures", ".cache", "app.photoslibrary"] {
            write(&home.join(base).join("p/package.json"));
            fs::create_dir_all(home.join(base).join("p/node_modules")).unwrap();
        }
        assert!(find(&home).is_empty());
    }

    #[test]
    fn a_virtualenv_needs_its_own_marker() {
        let home = tempdir();
        fs::create_dir_all(home.join("code/a/.venv")).unwrap();
        write(&home.join("code/b/.venv/pyvenv.cfg"));
        let found = find(&home);
        assert_eq!(found.len(), 1);
        assert!(found[0].path.ends_with("code/b/.venv"));
    }

    #[test]
    fn a_generic_name_is_never_chosen_by_default() {
        let home = tempdir();
        write(&home.join("code/web/package.json"));
        fs::create_dir_all(home.join("code/web/dist")).unwrap();
        let found = find(&home);
        assert_eq!(found.len(), 1);
        assert!(!found[0].pre_selected);
        assert!(found[0].evidence.iter().any(|e| !e.ok));
    }

    #[test]
    fn a_strong_artifact_is_chosen_and_explains_itself() {
        let home = tempdir();
        write(&home.join("code/real/Cargo.toml"));
        write(&home.join("code/real/Cargo.lock"));
        fs::create_dir_all(home.join("code/real/target")).unwrap();
        let c = &find(&home)[0];
        assert!(c.pre_selected);
        assert_eq!((c.safety, c.tag, c.source.as_str(), c.title.as_str()), (SafetyClass::Rebuildable, Tag::Rebuilds, "real", "target/"));
        assert!(c.evidence.iter().any(|e| e.ok && e.text.contains("Cargo.toml sits beside it")));
        assert!(c.evidence.iter().any(|e| e.ok && e.text.contains("Cargo.lock")));
        assert_eq!(c.in_use_window, Some(IN_USE));
    }

    #[test]
    fn git_can_veto_a_choice_it_does_not_ignore() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let home = tempdir();
        let proj = home.join("code/real");
        write(&proj.join("Cargo.toml"));
        fs::create_dir_all(proj.join("target")).unwrap();
        assert!(Command::new("git").arg("-C").arg(&proj).args(["init", "-q"]).status().unwrap().success());
        assert!(!find(&home)[0].pre_selected, "tracked-looking build output stays unchecked");
        fs::write(proj.join(".gitignore"), "target/\n").unwrap();
        let c = &find(&home)[0];
        assert!(c.pre_selected && c.evidence.iter().any(|e| e.ok && e.text.starts_with("Ignored by git")));
    }
}
