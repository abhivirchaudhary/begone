//! Package managers and build tools keep what they downloaded in folders under the home folder, outside
//! `~/Library/Caches`, where nothing else looks. Each place is on the exact list in `safety::TOOL_CACHES`, and the safety
//! gate admits those places and nothing beside them, so all this has to do is notice which ones exist.
//!
//! `~/.cache` is a convention rather than a list: everything directly inside it is something its tool can make again, so
//! each folder is offered, and only the ones begone knows by name are chosen for you.

use std::fs;
use std::path::Path;
use std::time::Duration;

use crate::model::{Evidence, Kind, SafetyClass, Tag};
use crate::safety::{self, Grant, TOOL_CACHES, ToolCache, XDG_CACHE};
use crate::scan::{Candidate, Ctx};

/// If something inside changed this recently, a tool is probably working in it right now.
const BUSY_WITHIN: Duration = Duration::from_secs(3600);

/// The `~/.cache` folders begone knows: `(folder, tool, what it holds, what happens when it is gone, chosen for you)`.
/// The ones that can take hours to fetch again are known too, but left for you to choose.
const KNOWN_XDG: &[(&str, &str, &str, &str, bool)] = &[
    ("uv", "uv", "Packages and wheels uv downloaded.", "uv downloads them again the next time an install needs them.", true),
    ("pip", "pip", "Packages pip downloaded and built.", "pip fetches or builds them again when an install needs them.", true),
    (
        "pre-commit",
        "pre-commit",
        "Tools pre-commit installed to run your hooks.",
        "pre-commit installs them again the next time the hooks run.",
        true,
    ),
    ("pnpm", "pnpm", "What pnpm learned about packages.", "pnpm fetches it again.", true),
    ("yarn", "Yarn", "Packages Yarn downloaded.", "Yarn downloads them again the next time an install needs them.", true),
    ("puppeteer", "Puppeteer", "Browsers Puppeteer downloaded.", "Puppeteer downloads one again the next time it needs it.", true),
    ("prisma", "Prisma", "Query engines Prisma downloaded.", "Prisma downloads them again when it needs them.", true),
    (
        "huggingface",
        "Hugging Face",
        "Models and datasets downloaded from Hugging Face.",
        "They download again when a program asks for them, which can take a long time.",
        false,
    ),
    (
        "torch",
        "PyTorch",
        "Models PyTorch downloaded.",
        "They download again when a program asks for them, which can take a long time.",
        false,
    ),
    ("bazel", "Bazel", "Bazel's build outputs and downloads.", "Bazel builds them again, which can take a long time.", false),
];

fn real_folder(path: &Path) -> bool {
    path.symlink_metadata().is_ok_and(|m| m.is_dir())
}

fn listed(ctx: &Ctx<'_>, cache: &ToolCache) -> Option<Candidate> {
    let path = ctx.home.join(cache.path);
    real_folder(&path).then(|| {
        let (class, tag) = if cache.costs { (SafetyClass::Rebuildable, Tag::Rebuilds) } else { (SafetyClass::Safe, Tag::Safe) };
        Candidate::new(path, Kind::Caches, Grant::ToolCache, cache.title)
            .src(cache.tool)
            .explain(cache.what, cache.comes_back)
            .ev(Evidence::good(format!("{} keeps this in ~/{}, and only what it can fetch or build again", cache.tool, cache.path)))
            .ev(Evidence::good("Nothing you made is in it"))
            .class(class, tag)
            .in_use_within(BUSY_WITHIN)
    })
}

fn in_dot_cache(ctx: &Ctx<'_>) -> Vec<Candidate> {
    let Ok(entries) = fs::read_dir(ctx.home.join(XDG_CACHE)) else { return Vec::new() };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        if ctx.cancelled() {
            break;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        // The same rule the gate applies, so nothing is offered that would be refused.
        if !entry.file_type().is_ok_and(|t| t.is_dir()) || !safety::is_tool_cache(&Path::new(XDG_CACHE).join(&name)) {
            continue;
        }
        let cand = Candidate::new(entry.path(), Kind::Caches, Grant::ToolCache, &name)
            .class(SafetyClass::Rebuildable, Tag::Rebuilds)
            .ev(Evidence::good("Directly inside ~/.cache, where tools are meant to keep only what they can make again"))
            .in_use_within(BUSY_WITHIN);
        out.push(match KNOWN_XDG.iter().find(|k| k.0 == name) {
            Some((_, tool, what, comes_back, chosen)) => {
                let cand = cand.src(*tool).explain(*what, *comes_back).ev(Evidence::good("Nothing you made is in it"));
                if *chosen {
                    cand
                } else {
                    cand.unchecked().ev(Evidence::warn("Getting it back can take a long time, so it is not chosen for you"))
                }
            }
            None => cand
                .src("~/.cache")
                .explain(
                    "A cache folder in ~/.cache. By convention it only holds what its tool can make again.",
                    "The tool that made it makes it again when it needs it.",
                )
                .unchecked()
                .ev(Evidence::warn("begone does not know this tool, so it is not chosen for you")),
        });
    }
    out
}

pub fn discover(ctx: &Ctx<'_>) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = TOOL_CACHES.iter().filter(|_| !ctx.cancelled()).filter_map(|cache| listed(ctx, cache)).collect();
    out.extend(in_dot_cache(ctx));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safety::tests::{leave, tempdir};
    use crate::scan::procs::Procs;
    use std::sync::atomic::AtomicBool;

    fn found(home: &Path) -> Vec<Candidate> {
        let cancel = AtomicBool::new(false);
        discover(&Ctx { home, procs: &Procs::none(), cancel: &cancel })
    }

    fn titles(found: &[Candidate]) -> Vec<&str> {
        let mut t: Vec<&str> = found.iter().map(|c| c.title.as_str()).collect();
        t.sort_unstable();
        t
    }

    #[test]
    fn a_bare_home_has_none() {
        assert!(found(&tempdir()).is_empty());
    }

    #[test]
    fn only_the_listed_places_that_exist_as_real_folders_are_found() {
        let home = tempdir();
        leave(&home.join(".npm/_cacache"), 10, true);
        leave(&home.join(".cargo/registry/src"), 10, true);
        leave(&home.join(".cargo/registry/cache"), 10, true);
        // What sits beside them is not offered.
        leave(&home.join(".cargo/bin"), 10, true);
        leave(&home.join(".cargo/credentials.toml"), 10, false);
        leave(&home.join(".npm/_logs"), 10, true);
        // A listed place that is a symlink, or a file, is not a cache folder.
        let elsewhere = home.join("elsewhere");
        leave(&elsewhere, 10, true);
        std::fs::create_dir_all(home.join(".gradle")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, home.join(".gradle/caches")).unwrap();
        leave(&home.join(".bun/install/cache"), 10, false);
        let found = found(&home);
        assert_eq!(titles(&found), ["Cargo downloads", "Cargo sources", "npm cache"]);
        let safety = safety::Safety::for_home(&home);
        for cand in &found {
            assert_eq!((cand.kind, cand.grant), (Kind::Caches, Grant::ToolCache));
            assert!(safety.check(&cand.path, cand.grant).is_ok(), "{} would be refused", cand.path.display());
            assert!(!cand.what.is_empty() && !cand.comes_back.is_empty() && !cand.source.is_empty() && cand.evidence.iter().all(|e| e.ok));
        }
    }

    #[test]
    fn what_costs_a_download_says_rebuilds_and_what_does_not_says_safe_and_all_wait_while_a_tool_is_busy() {
        let home = tempdir();
        leave(&home.join(".cargo/registry/src"), 10, true);
        leave(&home.join(".cargo/registry/cache"), 10, true);
        let found = found(&home);
        let by = |t: &str| found.iter().find(|c| c.title == t).unwrap();
        assert_eq!((by("Cargo sources").tag, by("Cargo sources").safety), (Tag::Safe, SafetyClass::Safe));
        assert_eq!((by("Cargo downloads").tag, by("Cargo downloads").safety), (Tag::Rebuilds, SafetyClass::Rebuildable));
        assert!(found.iter().all(|c| c.pre_selected && c.in_use_window == Some(BUSY_WITHIN)));
        assert!(by("Cargo downloads").comes_back.contains("network"));
    }

    #[test]
    fn dot_cache_folders_are_offered_and_only_the_ones_known_to_be_cheap_to_get_back_are_chosen() {
        let home = tempdir();
        for name in ["uv", "huggingface", "mystery-tool", ".hidden"] {
            leave(&home.join(".cache").join(name), 10, true);
        }
        leave(&home.join(".cache/readme.txt"), 10, false);
        let found = found(&home);
        assert_eq!(titles(&found), ["huggingface", "mystery-tool", "uv"], "no files, no hidden folders");
        let by = |t: &str| found.iter().find(|c| c.title == t).unwrap();
        assert!(by("uv").pre_selected && by("uv").evidence.iter().all(|e| e.ok));
        assert!(!by("huggingface").pre_selected && by("huggingface").evidence.iter().any(|e| !e.ok && e.text.contains("long time")));
        assert!(!by("mystery-tool").pre_selected && by("mystery-tool").evidence.iter().any(|e| !e.ok && e.text.contains("does not know")));
        assert_eq!((by("uv").source.as_str(), by("mystery-tool").source.as_str()), ("uv", "~/.cache"));
        // A cache folder that is really a symlink is not followed.
        let elsewhere = home.join("elsewhere");
        leave(&elsewhere, 10, true);
        std::os::unix::fs::symlink(&elsewhere, home.join(".cache/linked")).unwrap();
        assert!(!titles(&self::found(&home)).contains(&"linked"));
    }

    #[test]
    fn a_cancelled_scan_stops_looking() {
        let home = tempdir();
        leave(&home.join(".npm/_cacache"), 10, true);
        leave(&home.join(".cache/uv"), 10, true);
        let cancel = AtomicBool::new(true);
        assert!(discover(&Ctx { home: &home, procs: &Procs::none(), cancel: &cancel }).is_empty());
    }
}
