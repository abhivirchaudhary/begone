//! Xcode's debris: DerivedData, old device support, simulator caches. Anybody who builds
//! iOS apps has gigabytes here without knowing.
//!
//! `Archives` is never offered, because that is where signed release builds live and
//! deleting one can lose the only copy of a shipped binary. Simulator *devices* are left
//! alone for now: removing their folders behind `simctl`'s back is not something begone
//! can vouch for.

use std::fs;
use std::path::Path;
use std::time::Duration;

use crate::model::{Evidence, Kind, SafetyClass, Tag};
use crate::safety::Grant;
use crate::scan::{Candidate, Ctx};

const SUPPORT_MIN_AGE: Duration = Duration::from_secs(30 * 86_400);

pub fn discover(ctx: &Ctx<'_>) -> Vec<Candidate> {
    let developer = ctx.home.join("Library/Developer");
    let xcode_running = ctx.procs.app_for("com.apple.dt.Xcode").is_some();
    let mut out = Vec::new();

    // DerivedData: one folder per project, pure rebuildable index and build output.
    for dir in subdirs(&developer.join("Xcode/DerivedData")) {
        let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut cand = Candidate::new(dir.clone(), Kind::Xcode, Grant::Allowlisted, project_name(&name))
            .src("DerivedData")
            .class(SafetyClass::Rebuildable, Tag::Rebuilds)
            .explain("Xcode's build output and index for one project.", "Xcode rebuilds it on your next build, which will be slower once.")
            .ev(Evidence::good("Inside DerivedData, which Xcode treats as disposable"));
        if let Some(workspace) = workspace_path(&dir) {
            cand = if Path::new(&workspace).exists() {
                cand.ev(Evidence::good(format!("Built from {workspace}")))
            } else {
                cand.ev(Evidence::good(format!("Its project, {workspace}, is gone: nothing will rebuild this")))
            };
        }
        if xcode_running {
            cand =
                cand.class(SafetyClass::Rebuildable, Tag::Running).unchecked().ev(Evidence::warn("Xcode is running and may be using it"));
        }
        out.push(cand);
    }

    // Per-OS-version symbol stores, only when untouched for a month.
    for platform in ["iOS", "watchOS", "tvOS"] {
        for dir in subdirs(&developer.join(format!("Xcode/{platform} DeviceSupport"))) {
            let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            out.push(
                Candidate::new(dir, Kind::Xcode, Grant::Allowlisted, name)
                    .src(format!("{platform} DeviceSupport"))
                    .explain(
                        "Debug symbols Xcode copied from a device for one OS version.",
                        "Xcode copies them again the next time you debug on that version.",
                    )
                    .ev(Evidence::good("Inside DeviceSupport, which Xcode refills on demand"))
                    .idle_at_least(SUPPORT_MIN_AGE),
            );
        }
    }

    // Simulator runtime caches, never the devices or runtimes themselves.
    for dir in subdirs(&developer.join("CoreSimulator/Caches")) {
        let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        out.push(
            Candidate::new(dir, Kind::Xcode, Grant::Allowlisted, name)
                .src("Simulator caches")
                .class(SafetyClass::Rebuildable, Tag::Rebuilds)
                .explain("Caches the iOS Simulator builds to boot faster.", "The Simulator rebuilds them, so its next boot is slower once.")
                .ev(Evidence::good("Inside CoreSimulator/Caches, rebuilt on demand")),
        );
    }
    out
}

fn subdirs(dir: &Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
    entries.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path()).collect()
}

/// `Atlas-bxqfdhgwjkluvnhdcxcjxpphlmwr` is the project `Atlas` and a 28-letter hash.
fn project_name(folder: &str) -> String {
    if let Some((name, hash)) = folder.rsplit_once('-')
        && hash.len() == 28
        && hash.chars().all(|c| c.is_ascii_lowercase())
    {
        return name.to_string();
    }
    folder.to_string()
}

fn workspace_path(derived: &Path) -> Option<String> {
    let value = plist::Value::from_file(derived.join("info.plist")).ok()?;
    value.as_dictionary()?.get("WorkspacePath")?.as_string().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safety::tests::tempdir;
    use crate::scan::procs::Procs;
    use std::sync::atomic::AtomicBool;

    fn run(home: &Path, procs: &Procs) -> Vec<Candidate> {
        let cancel = AtomicBool::new(false);
        discover(&Ctx { home, procs, cancel: &cancel })
    }

    #[test]
    fn finds_derived_data_and_never_archives_or_simulator_devices() {
        let home = tempdir();
        let dev = home.join("Library/Developer");
        for d in [
            "Xcode/DerivedData/Atlas-bxqfdhgwjkluvnhdcxcjxpphlmwr",
            "Xcode/Archives/2025-01-01/App.xcarchive",
            "CoreSimulator/Devices/AAAA-BBBB",
            "Xcode/iOS DeviceSupport/18.0",
            "CoreSimulator/Caches/dyld",
        ] {
            fs::create_dir_all(dev.join(d)).unwrap();
        }
        let found = run(&home, &Procs::none());
        let paths: Vec<String> = found.iter().map(|c| c.path.to_string_lossy().into_owned()).collect();
        assert!(paths.iter().any(|p| p.ends_with("Atlas-bxqfdhgwjkluvnhdcxcjxpphlmwr")));
        assert!(!paths.iter().any(|p| p.contains("Archives") || p.contains("Devices")));
        assert_eq!(found.iter().find(|c| c.source == "DerivedData").unwrap().title, "Atlas");
        assert_eq!(found.iter().find(|c| c.source == "iOS DeviceSupport").unwrap().min_idle, Some(SUPPORT_MIN_AGE));
    }

    #[test]
    fn a_running_xcode_leaves_derived_data_unchecked() {
        let home = tempdir();
        fs::create_dir_all(home.join("Library/Developer/Xcode/DerivedData/Proj-abc")).unwrap();
        let running = Procs::with(&[("com.apple.dt.Xcode", "Xcode")]);
        let found = run(&home, &running);
        assert_eq!((found[0].tag, found[0].pre_selected), (Tag::Running, false));
        assert!(run(&home, &Procs::none())[0].pre_selected);
    }

    #[test]
    fn a_gone_project_is_said_to_be_gone() {
        let home = tempdir();
        let dd = home.join("Library/Developer/Xcode/DerivedData/Old-abc");
        fs::create_dir_all(&dd).unwrap();
        let mut dict = plist::Dictionary::new();
        dict.insert("WorkspacePath".into(), plist::Value::String("/nonexistent/Old.xcodeproj".into()));
        plist::to_file_xml(dd.join("info.plist"), &plist::Value::Dictionary(dict)).unwrap();
        let found = run(&home, &Procs::none());
        assert!(found[0].evidence.iter().any(|e| e.text.contains("is gone")));
    }

    #[test]
    fn project_names_lose_only_a_genuine_hash() {
        assert_eq!(project_name("My-App-bxqfdhgwjkluvnhdcxcjxpphlmwr"), "My-App");
        assert_eq!(project_name("ModuleCache.noindex"), "ModuleCache.noindex");
        assert_eq!(project_name("Plain-name"), "Plain-name");
    }
}
