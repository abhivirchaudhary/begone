//! Which apps are running, so a cache or build folder in use can say so.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default)]
pub struct Procs {
    /// Lowercased bundle identifier to app name.
    apps: HashMap<String, String>,
    /// Where each running app lives, by lowercased identifier, when `ps` said so.
    homes: HashMap<String, Vec<PathBuf>>,
}

impl Procs {
    pub fn none() -> Self {
        Self::default()
    }

    pub fn detect() -> Self {
        let Ok(out) = std::process::Command::new("ps").args(["-axo", "command="]).output() else {
            return Self::none();
        };
        Self::from_ps(&String::from_utf8_lossy(&out.stdout))
    }

    /// Build from `ps -axo command=` output: every `Foo.app/Contents/` path names a running app.
    pub fn from_ps(text: &str) -> Self {
        let mut bundles: HashSet<&str> = HashSet::new();
        for line in text.lines() {
            let line = line.trim_start();
            if !line.starts_with('/') {
                continue;
            }
            // The first `.app/` is the outermost bundle, so a helper inside it counts for the app.
            if let Some(at) = line.find(".app/") {
                bundles.insert(&line[..at + 4]);
            }
        }
        let (mut apps, mut homes) = (HashMap::new(), HashMap::<String, Vec<PathBuf>>::new());
        for bundle in bundles {
            let plist = std::path::Path::new(bundle).join("Contents/Info.plist");
            let Ok(value) = plist::Value::from_file(&plist) else { continue };
            let Some(dict) = value.as_dictionary() else { continue };
            let Some(id) = dict.get("CFBundleIdentifier").and_then(|v| v.as_string()) else { continue };
            let name = dict
                .get("CFBundleDisplayName")
                .or_else(|| dict.get("CFBundleName"))
                .and_then(|v| v.as_string())
                .map(str::to_string)
                .or_else(|| std::path::Path::new(bundle).file_stem().map(|s| s.to_string_lossy().into_owned()))
                .unwrap_or_else(|| id.to_string());
            homes.entry(id.to_lowercase()).or_default().push(PathBuf::from(bundle));
            apps.insert(id.to_lowercase(), name);
        }
        Self { apps, homes }
    }

    /// The running app a folder named `id` belongs to, if any.
    pub fn app_for(&self, id: &str) -> Option<&str> {
        let id = id.to_lowercase();
        if let Some(name) = self.apps.get(&id) {
            return Some(name);
        }
        // `com.foo.app.helper` belongs to `com.foo.app`.
        self.apps.iter().find(|(known, _)| id.starts_with(&format!("{known}."))).map(|(_, n)| n.as_str())
    }

    /// Whether the app installed at `bundle`, whose identifier is `id`, is running. Two copies of an app often share an
    /// identifier (a new Xcode beside an old one), so where `ps` said which copy runs, only that one counts.
    pub fn bundle_running(&self, bundle: &Path, id: &str) -> bool {
        let id = id.to_lowercase();
        match self.homes.get(&id) {
            Some(running) => running.iter().any(|home| same_place(home, bundle)),
            None => self.apps.contains_key(&id),
        }
    }

    #[cfg(test)]
    pub fn with(pairs: &[(&str, &str)]) -> Self {
        Self { apps: pairs.iter().map(|(i, n)| (i.to_lowercase(), (*n).to_string())).collect(), homes: HashMap::new() }
    }
}

fn same_place(a: &Path, b: &Path) -> bool {
    a == b || matches!((a.canonicalize(), b.canonicalize()), (Ok(a), Ok(b)) if a == b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_named_for_a_running_app_is_matched_case_insensitively() {
        let procs = Procs::with(&[("com.Spotify.client", "Spotify")]);
        assert_eq!(procs.app_for("com.spotify.client"), Some("Spotify"));
        assert_eq!(procs.app_for("com.spotify.client.helper"), Some("Spotify"));
        assert_eq!(procs.app_for("com.spotify.clientele"), None);
        assert_eq!(procs.app_for("org.other"), None);
    }

    #[test]
    fn two_copies_sharing_an_identifier_are_told_apart_by_where_the_running_one_lives() {
        use crate::safety::tests::{fake_app, tempdir};
        let dir = tempdir();
        let (new, old) = (fake_app(&dir, "Xcode", "com.apple.dt.Xcode", 10), fake_app(&dir, "Xcode-15.4", "com.apple.dt.Xcode", 10));
        let other = fake_app(&dir, "Pages", "com.apple.iWork.Pages", 10);
        let ps = format!(
            "{}/Contents/MacOS/Xcode -psn_0_1\n{}/Contents/Frameworks/Helper.app/Contents/MacOS/Helper\n",
            new.display(),
            other.display()
        );
        let procs = Procs::from_ps(&ps);
        assert!(procs.bundle_running(&new, "com.apple.dt.Xcode"), "the copy that runs");
        assert!(!procs.bundle_running(&old, "com.apple.dt.Xcode"), "the copy beside it that does not");
        assert!(procs.bundle_running(&other, "com.apple.iWork.Pages"), "a helper inside a bundle counts for the bundle");
        assert!(procs.bundle_running(&new, "COM.APPLE.DT.XCODE"), "identifiers are compared without regard to case");
        assert!(!procs.bundle_running(&new, "com.vendor.other"));
        // The same copy reached by another route is the same copy.
        let via = dir.join("via");
        std::os::unix::fs::symlink(&*dir, &via).unwrap();
        assert!(procs.bundle_running(&via.join("Xcode.app"), "com.apple.dt.Xcode"));
        // With nothing but an identifier to go on, any copy of it counts.
        let by_id = Procs::with(&[("com.apple.dt.Xcode", "Xcode")]);
        assert!(by_id.bundle_running(&new, "com.apple.dt.Xcode") && by_id.bundle_running(&old, "com.apple.dt.Xcode"));
        assert!(!Procs::none().bundle_running(&new, "com.apple.dt.Xcode"));
    }

    #[test]
    fn ps_lines_without_an_app_path_are_ignored() {
        let procs = Procs::from_ps("/usr/bin/ssh-agent\n/sbin/launchd\nzsh\n");
        assert_eq!(procs.app_for("anything"), None);
    }
}
