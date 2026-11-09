//! What the Files tab reads: old installers and disk images in Downloads, and iPhone and iPad backups. Neither is junk begone
//! can prove: an installer may be the one you still need, and a backup may be the only copy of a lost phone. So they are only
//! listed, never chosen for you, and chosen one at a time. Everything listed has passed the same gate that admits it again at
//! the moment it moves.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use crate::apps::next_id;
use crate::dupes::{self, DupeOptions};
use crate::model::{Evidence, Kind, SafetyClass, ScanItem, Tag, ago, human, idle_since};
use crate::safety::{DEV_STORES, Grant, INSTALLER_EXTENSIONS, Safety, VM_STORES, dev_store_meta, vm_store_for};
use crate::simulators::{self, Simctl};
use crate::sizing;

pub enum FilesEvent {
    Progress(f32),
    Found(Box<ScanItem>),
    /// Something that could not be read, and why, for the screen to say.
    Note(String),
    Finished {
        elapsed: Duration,
    },
}

#[derive(Clone, Debug, Default)]
pub struct FilesOptions {
    /// How to ask Xcode's `simctl` about simulators, when there is an Xcode to ask.
    pub simctl: Option<Arc<dyn Simctl>>,
    /// A simulator smaller than this is not worth a row: Xcode makes the empty ones again.
    pub min_simulator: u64,
    /// An installer has to be this old to be listed: one downloaded yesterday is probably about to be used.
    pub installer_min_age: Duration,
    /// Ask Spotlight when each big download was last opened, so one opened lately is not called untouched. Off for a scratch
    /// home, whose files Spotlight has never seen.
    pub spotlight: bool,
    /// How hard to look for byte-for-byte duplicates, and where to stop so a full disk stays responsive.
    pub dupes: DupeOptions,
}

/// Installers younger than this are not listed.
pub const INSTALLER_MIN_AGE: Duration = Duration::from_secs(14 * 86_400);

/// A simulator under this is the empty one Xcode makes for itself.
pub const MIN_SIMULATOR: u64 = 50_000_000;

/// Does this say there is no Xcode to ask, rather than that asking went wrong?
fn no_xcode(why: &str) -> bool {
    let why = why.to_lowercase();
    ["unable to find utility", "no developer tools", "no such file", "could not run", "not a developer tool"]
        .iter()
        .any(|w| why.contains(w))
}

fn installer_words(extension: &str) -> &'static str {
    match extension {
        "pkg" | "mpkg" => "An installer package you downloaded.",
        "iso" => "A disk image, such as an operating system installer.",
        "xip" => "A signed archive, such as an Xcode download.",
        _ => "A disk image, most often an app's installer.",
    }
}

fn installer_item(safety: &Safety, path: PathBuf, extension: &str, m: sizing::Measure) -> ScanItem {
    let mut evidence = vec![Evidence::good(format!("A .{extension} file directly in your Downloads"))];
    if let Some(idle) = idle_since(m.newest) {
        evidence.push(Evidence::good(format!("Last changed {}", ago(idle))));
    }
    evidence.push(Evidence::warn("begone cannot tell whether you still need it, so it is not chosen for you"));
    ScanItem {
        id: next_id(),
        title: path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        kept: safety.is_kept(&path),
        path,
        size: m.bytes,
        files: m.files,
        kind: Kind::Files,
        safety: SafetyClass::Yours,
        tag: Tag::Yours,
        grant: Grant::Installer,
        source: "installer".to_string(),
        what: installer_words(extension).to_string(),
        comes_back: "Download it again from where you got it, if it is still offered.".to_string(),
        evidence,
        modified: m.newest,
        pre_selected: false,
        owner: None,
    }
}

/// Where a big file sits, said in words: one of the staging folders, or loose at the top of the home folder.
fn big_file_where(safety: &Safety, path: &Path) -> &'static str {
    let parent = path.parent();
    if safety.is_external() {
        "the top of this drive"
    } else if parent == Some(safety.home_real().join("Downloads").as_path()) {
        "your Downloads"
    } else if parent == Some(safety.home_real().join("Desktop").as_path()) {
        "your Desktop"
    } else {
        "the top of your home folder"
    }
}

fn big_item(safety: &Safety, path: PathBuf, m: sizing::Measure, last_opened: Option<SystemTime>) -> ScanItem {
    let where_ = big_file_where(safety, &path);
    let mut evidence = vec![Evidence::good(format!("A finished file of {} in {where_}", human(m.bytes)))];
    let idle = idle_since(m.newest);
    if let Some(idle) = idle {
        evidence.push(Evidence::good(format!("Last changed {}", ago(idle))));
    }
    if let Some(opened) = idle_since(last_opened) {
        evidence.push(Evidence::good(format!("Not opened since {}", ago(opened))));
    }
    evidence.push(Evidence::warn("begone cannot tell whether you still need it, so it is not chosen for you"));
    ScanItem {
        id: next_id(),
        title: path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        kept: safety.is_kept(&path),
        path,
        size: m.bytes,
        files: m.files,
        kind: Kind::Files,
        safety: SafetyClass::Yours,
        tag: Tag::Yours,
        grant: Grant::BigDownload,
        source: "big file".to_string(),
        what: match idle {
            Some(idle) => format!("A {} file in {where_}, last changed {}.", human(m.bytes), ago(idle)),
            None => format!("A {} file in {where_}.", human(m.bytes)),
        },
        comes_back: "If you downloaded or made it, you would have to get or make it again.".to_string(),
        evidence,
        modified: m.newest,
        pre_selected: false,
        owner: None,
    }
}

/// What a device backup's `Info.plist` says about it.
struct BackupInfo {
    device: String,
    product: String,
    ios: Option<String>,
    last_backup: Option<SystemTime>,
}

fn read_backup_info(folder: &Path) -> BackupInfo {
    let dict = plist::Value::from_file(folder.join("Info.plist")).ok().and_then(|v| v.into_dictionary());
    let text = |key: &str| {
        dict.as_ref().and_then(|d| d.get(key)).and_then(|v| v.as_string()).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
    };
    BackupInfo {
        device: text("Device Name")
            .or_else(|| text("Display Name"))
            .unwrap_or_else(|| folder.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()),
        product: text("Product Type").unwrap_or_default(),
        ios: text("Product Version"),
        last_backup: dict.as_ref().and_then(|d| d.get("Last Backup Date")).and_then(|v| v.as_date()).map(SystemTime::from),
    }
}

fn is_encrypted(folder: &Path) -> bool {
    plist::Value::from_file(folder.join("Manifest.plist"))
        .ok()
        .and_then(|v| v.into_dictionary())
        .and_then(|d| d.get("IsEncrypted").and_then(|v| v.as_boolean()))
        .unwrap_or(false)
}

fn backup_item(safety: &Safety, path: PathBuf, m: sizing::Measure) -> ScanItem {
    let info = read_backup_info(&path);
    let made = info.last_backup.or(m.newest);
    let kind_word = if info.product.starts_with("iPad") {
        "iPad backup"
    } else if info.product.starts_with("iPhone") {
        "iPhone backup"
    } else {
        "device backup"
    };
    let when = idle_since(made).map_or_else(String::new, |idle| format!(", last made {}", ago(idle)));
    let ios = info.ios.as_ref().map_or_else(String::new, |v| format!(" (iOS {v})"));
    let mut evidence = vec![Evidence::good("Its Info.plist names the device it was made from")];
    if let Some(idle) = idle_since(made) {
        evidence.push(Evidence::good(format!("Last backed up {}", ago(idle))));
    }
    evidence.push(Evidence::warn("If you no longer have this device, this may be the only copy of what was on it"));
    if is_encrypted(&path) {
        evidence.push(Evidence::warn("Encrypted: it can only be restored with its password"));
    }
    ScanItem {
        id: next_id(),
        title: info.device.clone(),
        kept: safety.is_kept(&path),
        path,
        size: m.bytes,
        files: m.files,
        kind: Kind::Files,
        safety: SafetyClass::Yours,
        tag: Tag::Yours,
        grant: Grant::DeviceBackup,
        source: kind_word.to_string(),
        what: format!("A backup of {}{ios}{when}.", info.device),
        comes_back: "It cannot be made again once the device is gone: only the device itself can make a new one.".to_string(),
        evidence,
        modified: made,
        pre_selected: false,
        owner: None,
    }
}

fn dev_store_item(safety: &Safety, path: PathBuf, m: sizing::Measure) -> ScanItem {
    let relative = path.strip_prefix(safety.home_real()).unwrap_or(&path).to_path_buf();
    let store = dev_store_meta(&relative);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let (tool, title, what, comes_back) = match store {
        Some(s) if s.each_child => (s.tool, name.clone(), s.what.to_string(), s.comes_back.to_string()),
        Some(s) => (s.tool, s.title.to_string(), s.what.to_string(), s.comes_back.to_string()),
        None => ("developer tool", name.clone(), "A developer tool's store.".to_string(), String::new()),
    };
    let mut evidence = vec![
        Evidence::good(format!("A folder {tool} keeps under your home that it can fetch or build again")),
        Evidence::good("Nothing you made is in it"),
    ];
    if let Some(idle) = idle_since(m.newest) {
        evidence.push(Evidence::good(format!("Last changed {}", ago(idle))));
    }
    evidence.push(Evidence::warn("begone cannot tell whether you still want it, so it is not chosen for you"));
    ScanItem {
        id: next_id(),
        kept: safety.is_kept(&path),
        path,
        size: m.bytes,
        files: m.files,
        kind: Kind::Files,
        safety: SafetyClass::Rebuildable,
        tag: Tag::Rebuilds,
        grant: Grant::DevStore,
        source: tool.to_string(),
        title,
        what,
        comes_back,
        evidence,
        modified: m.newest,
        pre_selected: false,
        owner: None,
    }
}

/// The developer tool stores worth a row: a whole-folder store that exists, and each direct child of a per-child store. Every
/// one passes the same gate that will admit it again at the moment it moves.
fn dev_stores(safety: &Safety) -> Vec<PathBuf> {
    let home = safety.home_real();
    let mut found = Vec::new();
    for store in DEV_STORES {
        let base = home.join(store.path);
        if store.each_child {
            let Ok(entries) = std::fs::read_dir(&base) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                if entry.file_type().is_ok_and(|t| t.is_dir()) && safety.check(&path, Grant::DevStore).is_ok() {
                    found.push(path);
                }
            }
        } else if safety.check(&base, Grant::DevStore).is_ok() {
            found.push(base);
        }
    }
    found
}

fn vm_item(safety: &Safety, path: PathBuf, m: sizing::Measure) -> ScanItem {
    let relative = path.strip_prefix(safety.home_real()).unwrap_or(&path).to_path_buf();
    let store = vm_store_for(&relative);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let (tool, title, what, comes_back, warn) = match store {
        Some(s) if s.each_child => (s.tool, name.clone(), s.what, s.comes_back, s.warn),
        Some(s) => (s.tool, s.title.to_string(), s.what, s.comes_back, s.warn),
        None => ("virtual machine", name.clone(), "A virtual machine.", "Rebuild or reinstall it.", "Be sure you will not need it."),
    };
    let mut evidence = vec![Evidence::good(format!("A {tool} machine or disk, where {tool} keeps it"))];
    if let Some(idle) = idle_since(m.newest) {
        evidence.push(Evidence::good(format!("Last changed {}", ago(idle))));
    }
    evidence.push(Evidence::warn(warn));
    ScanItem {
        id: next_id(),
        kept: safety.is_kept(&path),
        size: m.bytes,
        files: m.files,
        kind: Kind::Files,
        safety: SafetyClass::Yours,
        tag: Tag::Yours,
        grant: Grant::VmImage,
        source: tool.to_string(),
        title,
        what: what.to_string(),
        comes_back: comes_back.to_string(),
        evidence,
        modified: m.newest,
        pre_selected: false,
        owner: None,
        path,
    }
}

/// Virtual machines and container stores, each at the exact place its app keeps it; every one passes the gate that will admit
/// it again when it moves. A store that does not exist (no Docker, no VMs) simply yields nothing.
fn vm_images(safety: &Safety) -> Vec<PathBuf> {
    let home = safety.home_real();
    let mut out = Vec::new();
    for store in VM_STORES {
        let base = home.join(store.path);
        if store.each_child {
            let Ok(entries) = std::fs::read_dir(&base) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|x| x.to_str()) == Some(store.ext) && safety.check(&path, Grant::VmImage).is_ok() {
                    out.push(path);
                }
            }
        } else if safety.check(&base, Grant::VmImage).is_ok() {
            out.push(base);
        }
    }
    out
}

/// `~/Pictures/x` rather than the whole path, given the home folder.
fn short(home: &Path, path: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// The folders begone looks in for duplicates: where copies of real files tend to collect. The home's own Library and dot-folders
/// are left to the other scanners; project build output and photo and music libraries are skipped inside the walk.
fn dedup_roots(safety: &Safety) -> Vec<PathBuf> {
    let home = safety.home_real();
    // On an external or secondary volume there are no home folders, so the whole volume is walked for duplicates.
    if safety.is_external() {
        return vec![home.to_path_buf()];
    }
    ["Downloads", "Desktop", "Documents", "Movies", "Music", "Pictures"].iter().map(|d| home.join(d)).filter(|p| p.is_dir()).collect()
}

fn duplicate_item(safety: &Safety, copy: PathBuf, keeper: &Path, m: sizing::Measure) -> ScanItem {
    let kept_at = short(safety.home_real(), keeper);
    let mut evidence = vec![Evidence::good(format!("Byte for byte identical to {kept_at}, which is not being removed"))];
    if let Some(idle) = idle_since(m.newest) {
        evidence.push(Evidence::good(format!("Last changed {}", ago(idle))));
    }
    evidence.push(Evidence::good("An identical copy stays, so removing this frees the space and loses nothing"));
    ScanItem {
        id: next_id(),
        title: copy.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        kept: safety.is_kept(&copy),
        size: m.bytes,
        files: m.files,
        kind: Kind::Files,
        safety: SafetyClass::Safe,
        tag: Tag::Safe,
        grant: Grant::DuplicateCopy,
        source: "duplicate".to_string(),
        what: format!("A copy of a file you already have. An identical one stays at {kept_at}."),
        comes_back: "Nothing is lost: the identical copy stays. Copy it back if you ever want two again.".to_string(),
        evidence,
        modified: m.newest,
        pre_selected: false,
        // The original the gate re-checks this copy against at the moment it moves.
        owner: Some(keeper.to_string_lossy().into_owned()),
        path: copy,
    }
}

/// The installers in Downloads old enough to list, as paths the gate admits.
fn installers(safety: &Safety, options: &FilesOptions, note: &mut dyn FnMut(String)) -> Vec<(PathBuf, String)> {
    let downloads = safety.home_real().join("Downloads");
    let entries = match std::fs::read_dir(&downloads) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            note(format!("Your Downloads folder could not be read ({err}). macOS may need to be asked to let your terminal in."));
            return Vec::new();
        }
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let extension = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
        if !entry.file_type().is_ok_and(|t| t.is_file()) || !INSTALLER_EXTENSIONS.contains(&extension.as_str()) {
            continue;
        }
        let old_enough = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .is_none_or(|modified| idle_since(Some(modified)).is_some_and(|idle| idle >= options.installer_min_age));
        if old_enough && safety.check(&path, Grant::Installer).is_ok() {
            found.push((path, extension));
        }
    }
    found
}

/// Big files that have sat untouched for months in the staging folders where they pile up — Downloads, the Desktop, and loose
/// at the top of the home folder. An installer or disk image in Downloads is left for the installer list, so it is not shown
/// twice; the same kind of file elsewhere is just a big file.
fn big_files(safety: &Safety, options: &FilesOptions) -> Vec<(PathBuf, Option<SystemTime>)> {
    let (_, min_age) = safety.big_download_rules();
    let downloads = safety.home_real().join("Downloads");
    let mut found = Vec::new();
    for root in safety.big_file_roots() {
        let Ok(entries) = std::fs::read_dir(&root) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let extension = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
            if !entry.file_type().is_ok_and(|t| t.is_file()) {
                continue;
            }
            if root == downloads && INSTALLER_EXTENSIONS.contains(&extension.as_str()) {
                continue;
            }
            if safety.check(&path, Grant::BigDownload).is_err() {
                continue;
            }
            // One opened lately is not untouched, however old its date.
            let opened = if options.spotlight { crate::apps::spotlight_last_used(&path) } else { None };
            if idle_since(opened).is_some_and(|idle| idle < min_age) {
                continue;
            }
            found.push((path, opened));
        }
    }
    found
}

/// The device backups, as folders the gate admits. Finder's own store and the third-party tools that use the same format are
/// all looked in; a folder is a backup only if it still carries a device Info.plist, which the gate checks again.
fn backups(safety: &Safety, note: &mut dyn FnMut(String)) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut said = false;
    for folder in safety.backup_roots() {
        match std::fs::read_dir(&folder) {
            Ok(entries) => {
                found.extend(entries.flatten().map(|e| e.path()).filter(|p| safety.check(p, Grant::DeviceBackup).is_ok()));
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                if !said {
                    note("iPhone and iPad backups could not be read: macOS needs Full Disk Access for your terminal.".to_string());
                    said = true;
                }
            }
        }
    }
    found
}

/// Look for what the tab offers. Items arrive as they are measured; nothing is changed.
pub fn scan(safety: &Safety, options: &FilesOptions, cancel: &AtomicBool, on_event: &mut dyn FnMut(FilesEvent)) {
    let started = Instant::now();
    on_event(FilesEvent::Progress(0.0));
    let mut notes = Vec::new();
    let installers = installers(safety, options, &mut |n| notes.push(n));
    let bigs = big_files(safety, options);
    let backups = backups(safety, &mut |n| notes.push(n));
    let dev = dev_stores(safety);
    let vms = vm_images(safety);
    for note in notes {
        on_event(FilesEvent::Note(note));
    }
    // Xcode's answer can take a while, so it is asked for while the rest is measured.
    let simulators = options.simctl.clone().map(|simctl| std::thread::spawn(move || simulators::list(simctl.as_ref())));
    let total = installers.len() + bigs.len() + backups.len() + dev.len() + vms.len();
    let mut done = 0;
    let mut finish = |item: Option<ScanItem>, on_event: &mut dyn FnMut(FilesEvent)| {
        done += 1;
        on_event(FilesEvent::Progress(done as f32 / total.max(1) as f32));
        if let Some(item) = item {
            on_event(FilesEvent::Found(Box::new(item)));
        }
    };
    for (path, extension) in installers {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let m = sizing::measure(&path);
        let item = (m.bytes > 0).then(|| installer_item(safety, path, &extension, m));
        finish(item, on_event);
    }
    for (path, opened) in bigs {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let m = sizing::measure(&path);
        let item = (m.bytes > 0).then(|| big_item(safety, path, m, opened));
        finish(item, on_event);
    }
    for path in backups {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let m = sizing::measure(&path);
        let item = (m.bytes > 0).then(|| backup_item(safety, path, m));
        finish(item, on_event);
    }
    for path in dev {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let m = sizing::measure(&path);
        let item = (m.bytes > 0).then(|| dev_store_item(safety, path, m));
        finish(item, on_event);
    }
    for path in vms {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let m = sizing::measure(&path);
        let item = (m.bytes > 0).then(|| vm_item(safety, path, m));
        finish(item, on_event);
    }
    if let Some(asked) = simulators {
        match asked.join() {
            Ok(Ok(list)) => {
                for sim in list.iter().filter(|s| s.bytes >= options.min_simulator && !cancel.load(Ordering::Relaxed)) {
                    on_event(FilesEvent::Found(Box::new(simulators::scan_item(sim, safety))));
                }
            }
            Ok(Err(why)) if !no_xcode(&why) => on_event(FilesEvent::Note(format!("Xcode's simulators could not be read ({why})."))),
            _ => {}
        }
    }
    // Duplicates come last: the quick rows are already on screen, and the heavy reading cannot hold them up. Each copy is
    // measured and offered as its group is confirmed; the keeper is never a row, so the last copy can never be chosen.
    if !cancel.load(Ordering::Relaxed) {
        let roots = dedup_roots(safety);
        let truncated = dupes::find(&roots, safety.home_real(), &options.dupes, cancel, &mut |group| {
            for copy in group.copies {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                let m = sizing::measure(&copy);
                if m.bytes > 0 {
                    on_event(FilesEvent::Found(Box::new(duplicate_item(safety, copy, &group.keeper, m))));
                }
            }
        });
        if truncated && !cancel.load(Ordering::Relaxed) {
            on_event(FilesEvent::Note("There were more files to compare for duplicates; the biggest ones are listed.".to_string()));
        }
    }
    on_event(FilesEvent::Progress(1.0));
    on_event(FilesEvent::Finished { elapsed: started.elapsed() });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safety::tests::{leave, tempdir};
    use std::fs;

    fn backup(home: &Path, udid: &str, device: &str, product: &str, encrypted: bool) -> PathBuf {
        backup_in(home, "Library/Application Support/MobileSync/Backup", udid, device, product, encrypted)
    }

    fn backup_in(home: &Path, root: &str, udid: &str, device: &str, product: &str, encrypted: bool) -> PathBuf {
        let dir = home.join(root).join(udid);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("Info.plist"),
            format!(
                "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>Device Name</key><string>{device}</string><key>Product Type</key><string>{product}</string><key>Product Version</key><string>17.2</string><key>Last Backup Date</key><date>2025-01-01T00:00:00Z</date></dict></plist>"
            ),
        )
        .unwrap();
        // Whether a backup is encrypted is written in its Manifest.plist, not its Info.plist.
        fs::write(
            dir.join("Manifest.plist"),
            format!("<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>IsEncrypted</key><{encrypted}/></dict></plist>"),
        )
        .unwrap();
        fs::write(dir.join("Manifest.db"), vec![9u8; 5000]).unwrap();
        dir
    }

    fn old(path: &Path) {
        assert!(std::process::Command::new("touch").args(["-t", "202501010000"]).arg(path).status().unwrap().success());
    }

    fn read(safety: &Safety, options: &FilesOptions) -> (Vec<ScanItem>, Vec<String>, Vec<f32>) {
        let (mut items, mut notes, mut progress) = (Vec::new(), Vec::new(), Vec::new());
        scan(safety, options, &AtomicBool::new(false), &mut |e| match e {
            FilesEvent::Found(item) => items.push(*item),
            FilesEvent::Note(n) => notes.push(n),
            FilesEvent::Progress(p) => progress.push(p),
            FilesEvent::Finished { .. } => {}
        });
        items.sort_by(|a, b| a.title.cmp(&b.title));
        (items, notes, progress)
    }

    #[test]
    fn old_installers_in_downloads_are_listed_and_nothing_else_in_it_is() {
        let home = tempdir();
        let down = home.join("Downloads");
        for name in ["Slack.dmg", "Tool.pkg", "ubuntu.iso", "Xcode_16.xip"] {
            leave(&down.join(name), 5000, false);
            old(&down.join(name));
        }
        leave(&down.join("fresh.dmg"), 5000, false);
        for name in ["thesis.pdf", "photos.zip", "notes.txt"] {
            leave(&down.join(name), 5000, false);
            old(&down.join(name));
        }
        leave(&down.join("sub/Nested.dmg"), 5000, false);
        let safety = Safety::for_home(&home);
        let (items, notes, progress) =
            read(&safety, &FilesOptions { installer_min_age: Duration::from_secs(14 * 86_400), ..FilesOptions::default() });
        assert_eq!(items.iter().map(|i| i.title.as_str()).collect::<Vec<_>>(), ["Slack.dmg", "Tool.pkg", "Xcode_16.xip", "ubuntu.iso"]);
        assert!(notes.is_empty());
        for item in &items {
            assert_eq!(
                (item.kind, item.grant, item.pre_selected, item.tag, item.safety),
                (Kind::Files, Grant::Installer, false, Tag::Yours, SafetyClass::Yours)
            );
            assert!(item.size > 0 && item.owner.is_none() && !item.kept && item.source == "installer");
            assert!(safety.check(&item.path, item.grant).is_ok());
            assert!(item.evidence.iter().any(|e| e.ok && e.text.contains("directly in your Downloads")));
            assert!(item.evidence.iter().any(|e| !e.ok && e.text.contains("cannot tell whether you still need it")));
            assert!(item.evidence.iter().any(|e| e.text.starts_with("Last changed")));
        }
        assert!(
            items[3].what.contains("operating system") && items[2].what.contains("Xcode"),
            "{:?}",
            items.iter().map(|i| &i.what).collect::<Vec<_>>()
        );
        assert!(
            progress.windows(2).all(|p| p[0] <= p[1]) && progress.first() == Some(&0.0) && (progress.last().unwrap() - 1.0).abs() < 1e-6
        );
        // With no minimum age, the new one is listed too.
        let (all, ..) = read(&safety, &FilesOptions::default());
        assert!(all.iter().any(|i| i.title == "fresh.dmg") && all.len() == 5);
    }

    #[test]
    fn big_old_downloads_are_listed_installers_are_not_listed_twice_and_a_new_small_or_unfinished_one_is_not() {
        let home = tempdir();
        let down = home.join("Downloads");
        for (name, bytes, is_old) in [
            ("Movie.mov", 60_000, true),
            ("Slack.dmg", 60_000, true),
            ("small.zip", 2_000, true),
            ("fresh.mp4", 60_000, false),
            (".hidden.bin", 60_000, true),
            ("Movie.mov.crdownload", 60_000, true),
        ] {
            leave(&down.join(name), bytes, false);
            if is_old {
                old(&down.join(name));
            }
        }
        leave(&down.join("sub/Nested.mov"), 60_000, false);
        old(&down.join("sub/Nested.mov"));
        let safety = Safety::for_home(&home).with_big_download(50_000, Duration::from_secs(3_600));
        let (items, notes, _) = read(&safety, &FilesOptions::default());
        assert!(notes.is_empty());
        assert_eq!(
            items.iter().map(|i| (i.title.as_str(), i.grant)).collect::<Vec<_>>(),
            [("Movie.mov", Grant::BigDownload), ("Slack.dmg", Grant::Installer)]
        );
        let movie = &items[0];
        assert_eq!((movie.kind, movie.source.as_str(), movie.pre_selected, movie.tag), (Kind::Files, "big file", false, Tag::Yours));
        assert!(
            movie.what.contains("file in your Downloads, last changed") && movie.comes_back.contains("get or make it again"),
            "{}",
            movie.what
        );
        assert!(movie.evidence.iter().any(|e| e.ok && e.text.contains("in your Downloads")));
        assert!(movie.evidence.iter().any(|e| !e.ok && e.text.contains("cannot tell whether you still need it")));
        assert!(safety.check(&movie.path, movie.grant).is_ok());
        // With the real rules a 60 KB file is nowhere near big.
        let (none, ..) = read(&Safety::for_home(&home), &FilesOptions::default());
        assert!(none.iter().all(|i| i.grant != Grant::BigDownload));
    }

    #[test]
    fn big_old_files_are_found_in_the_staging_folders_not_only_downloads_and_never_in_the_libraries() {
        let home = tempdir();
        // One big old file in each place begone looks, and one in a place it keeps whole.
        for rel in
            ["Downloads/clip.mov", "Desktop/recording.mov", "loose-at-home.mov", "Documents/thesis-video.mov", "Movies/home-video.mov"]
        {
            leave(&home.join(rel), 60_000, false);
            old(&home.join(rel));
        }
        // A disk image in Downloads is the installer list's; the same kind of file on the Desktop is just a big file.
        leave(&home.join("Downloads/Installer.dmg"), 60_000, false);
        old(&home.join("Downloads/Installer.dmg"));
        leave(&home.join("Desktop/OldImage.dmg"), 60_000, false);
        old(&home.join("Desktop/OldImage.dmg"));
        let safety = Safety::for_home(&home).with_big_download(50_000, Duration::from_secs(3_600));
        let (items, notes, _) = read(&safety, &FilesOptions::default());
        assert!(notes.is_empty());
        let big: std::collections::BTreeMap<&str, &str> =
            items.iter().filter(|i| i.grant == Grant::BigDownload).map(|i| (i.title.as_str(), i.source.as_str())).collect();
        assert_eq!(
            big.keys().copied().collect::<Vec<_>>(),
            ["OldImage.dmg", "clip.mov", "loose-at-home.mov", "recording.mov"],
            "Downloads, Desktop and the top of home — but not Documents or Movies, which are kept whole"
        );
        // The Downloads disk image is the installer list's, so it is not also a big file.
        assert!(items.iter().any(|i| i.title == "Installer.dmg" && i.grant == Grant::Installer));
        assert!(!items.iter().any(|i| i.title == "Installer.dmg" && i.grant == Grant::BigDownload));
        // Each big file names where it really is, and each still passes the gate.
        let by = |t: &str| items.iter().find(|i| i.title == t).unwrap();
        assert!(by("recording.mov").what.contains("your Desktop"), "{}", by("recording.mov").what);
        assert!(by("loose-at-home.mov").what.contains("top of your home folder"), "{}", by("loose-at-home.mov").what);
        for title in ["OldImage.dmg", "clip.mov", "loose-at-home.mov", "recording.mov"] {
            assert!(safety.check(&by(title).path, Grant::BigDownload).is_ok(), "{title} should pass its gate");
        }
    }

    #[derive(Debug)]
    struct Broken(&'static str);
    impl Simctl for Broken {
        fn run(&self, _: &[&str], _: Duration) -> Result<String, String> {
            Err(self.0.to_string())
        }
    }

    #[test]
    fn simulators_come_from_xcodes_tool_when_there_is_one_and_the_small_empty_ones_are_left_out() {
        let home = tempdir();
        let safety = Safety::for_home(&home);
        let with = |simctl: Option<Arc<dyn Simctl>>, min_simulator| FilesOptions { simctl, min_simulator, ..FilesOptions::default() };
        let fake: Arc<dyn Simctl> = Arc::new(simulators::tests::fake());
        let (items, notes, progress) = read(&safety, &with(Some(fake.clone()), 50_000_000));
        assert!(notes.is_empty());
        let mut titles: Vec<&str> = items.iter().map(|i| i.title.as_str()).collect();
        titles.sort_unstable();
        assert_eq!(
            titles,
            ["iOS 26.5 simulator runtime", "iPhone 17 Pro (iOS 26.5)"],
            "the 9 MB and 18 MB ones are the empty devices Xcode makes itself"
        );
        assert!(items.iter().all(|i| i.grant == Grant::Simulator && i.kind == Kind::Files && !i.pre_selected));
        assert!(progress.windows(2).all(|p| p[0] <= p[1]) && (progress.last().unwrap() - 1.0).abs() < 1e-6);
        // With no minimum every one is listed, and the ones you keep say so.
        safety.set_kept(vec![PathBuf::from("/Sims/Devices/064A5F07-ADB2-4AA2-95E1-FEEB341A883B/data")]);
        let (all, ..) = read(&safety, &with(Some(fake), 0));
        assert_eq!((all.len(), all.iter().filter(|i| i.kept).count()), (4, 1));
        // No Xcode to ask: nothing, and no complaint. Something else going wrong: said.
        for (why, want_note) in [
            ("xcrun: error: unable to find utility \"simctl\", not a developer tool or in PATH", false),
            ("could not run /usr/bin/xcrun (No such file)", false),
            ("simctl did not answer in time", true),
        ] {
            let (none, notes, _) = read(&safety, &with(Some(Arc::new(Broken(why))), 0));
            assert!(none.iter().all(|i| i.grant != Grant::Simulator), "{why}");
            assert_eq!(!notes.is_empty(), want_note, "{why}: {notes:?}");
        }
        let (plain, notes, _) = read(&safety, &with(None, 0));
        assert!(plain.is_empty() && notes.is_empty(), "no tool, no rows");
    }

    #[test]
    fn a_device_backup_is_listed_by_the_device_with_when_it_was_made_and_a_warning() {
        let home = tempdir();
        backup(&home, "AAA-111", "Abhi's iPhone", "iPhone15,2", false);
        backup(&home, "BBB-222", "Studio iPad", "iPad13,4", true);
        leave(&home.join("Library/Application Support/MobileSync/Backup/not-a-backup"), 5000, true);
        let safety = Safety::for_home(&home);
        let (items, notes, _) = read(&safety, &FilesOptions::default());
        assert!(notes.is_empty());
        assert_eq!(
            items.iter().map(|i| i.title.as_str()).collect::<Vec<_>>(),
            ["Abhi's iPhone", "Studio iPad"],
            "the folder without an Info.plist is not a backup"
        );
        let (phone, pad) = (&items[0], &items[1]);
        assert_eq!((phone.source.as_str(), pad.source.as_str()), ("iPhone backup", "iPad backup"));
        assert_eq!((phone.grant, phone.kind, phone.pre_selected, phone.tag), (Grant::DeviceBackup, Kind::Files, false, Tag::Yours));
        assert!(
            phone.what.contains("Abhi's iPhone") && phone.what.contains("iOS 17.2") && phone.what.contains("last made"),
            "{}",
            phone.what
        );
        assert!(phone.comes_back.contains("cannot be made again"));
        assert!(phone.evidence.iter().any(|e| !e.ok && e.text.contains("only copy of what was on it")));
        assert!(phone.evidence.iter().any(|e| e.text.starts_with("Last backed up")), "{:?}", phone.evidence);
        assert!(
            !phone.evidence.iter().any(|e| e.text.contains("Encrypted"))
                && pad.evidence.iter().any(|e| !e.ok && e.text.contains("Encrypted"))
        );
        assert!(
            phone.idle().is_some_and(|idle| idle > Duration::from_secs(86_400 * 30)),
            "its age is the day it was backed up, 2025-01-01"
        );
        for item in &items {
            assert!(item.size >= 5000 && safety.check(&item.path, item.grant).is_ok());
        }
    }

    #[test]
    fn identical_copies_are_offered_against_an_original_that_stays_and_the_original_is_never_a_row() {
        let home = tempdir();
        let payload = vec![4u8; 40_000];
        let put = |rel: &str, bytes: &[u8]| {
            let p = home.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, bytes).unwrap();
        };
        // Three identical files across staging folders, plus a unique file of the same size.
        for rel in ["Downloads/a.bin", "Desktop/b.bin", "Documents/c.bin"] {
            put(rel, &payload);
        }
        put("Downloads/unique.bin", &vec![5u8; 40_000]);
        let safety = Safety::for_home(&home);
        let options = FilesOptions { dupes: DupeOptions { min_size: 10, ..DupeOptions::default() }, ..FilesOptions::default() };
        let (items, notes, _) = read(&safety, &options);
        assert!(notes.is_empty());
        let keeper = safety.home_real().join("Documents/c.bin");
        let dups: Vec<&ScanItem> = items.iter().filter(|i| i.grant == Grant::DuplicateCopy).collect();
        let mut names: Vec<&str> = dups.iter().map(|i| i.title.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["a.bin", "b.bin"], "the Documents copy is kept; the unique file is not a duplicate");
        for d in &dups {
            assert_eq!((d.kind, d.safety, d.tag, d.pre_selected), (Kind::Files, SafetyClass::Safe, Tag::Safe, false));
            assert_eq!(d.owner.as_deref(), keeper.to_str(), "each copy names the original it is checked against");
            assert!(
                safety.check_move(&d.path, Grant::DuplicateCopy, d.owner.as_deref()).is_ok(),
                "{} should pass its gate",
                d.path.display()
            );
            assert!(d.what.contains("identical one stays at ~/Documents/c.bin"), "{}", d.what);
            assert!(d.evidence.iter().any(|e| e.ok && e.text.contains("identical to ~/Documents/c.bin")));
            assert!(d.comes_back.contains("Nothing is lost"));
        }
        // The copy that stays is never itself offered.
        assert!(!items.iter().any(|i| i.path == keeper), "the kept original is not a removable row");
    }

    #[test]
    fn virtual_machines_and_container_stores_are_listed_warned_and_never_chosen() {
        let home = tempdir();
        let utm = "Library/Containers/com.utmapp.UTM/Data/Documents";
        leave(&home.join(format!("{utm}/Ubuntu.utm")), 9000, true);
        leave(&home.join("Library/Containers/com.docker.docker/Data/vms/0/data/Docker.raw"), 8000, false);
        leave(&home.join(".orbstack/data"), 7000, true);
        leave(&home.join(format!("{utm}/notes.txt")), 100, false); // not a VM
        let safety = Safety::for_home(&home);
        let (items, notes, _) = read(&safety, &FilesOptions::default());
        assert!(notes.is_empty());
        let vms: Vec<&ScanItem> = items.iter().filter(|i| i.grant == Grant::VmImage).collect();
        let mut titles: Vec<&str> = vms.iter().map(|i| i.title.as_str()).collect();
        titles.sort_unstable();
        assert_eq!(titles, ["Docker disk image", "OrbStack data", "Ubuntu.utm"], "the plain file is not a VM");
        for item in &vms {
            assert_eq!((item.kind, item.safety, item.tag, item.pre_selected), (Kind::Files, SafetyClass::Yours, Tag::Yours, false));
            assert!(safety.check(&item.path, Grant::VmImage).is_ok(), "{} should pass its gate", item.path.display());
            assert!(item.evidence.iter().any(|e| !e.ok), "a VM row carries a warning");
            assert!(!item.what.is_empty() && item.owner.is_none());
        }
        let by = |t: &str| vms.iter().find(|i| i.title == t).unwrap();
        assert_eq!(by("Ubuntu.utm").source, "UTM");
        assert!(by("Docker disk image").evidence.iter().any(|e| !e.ok && e.text.contains("resets Docker")));
    }

    #[test]
    fn backups_from_a_third_party_tool_are_found_when_they_keep_the_standard_format() {
        let home = tempdir();
        // Finder's own store, and a third-party tool that keeps a backup in the same format in its own folder.
        backup(&home, "AAA-111", "Finder iPhone", "iPhone15,2", false);
        backup_in(
            &home,
            "Library/Application Support/app.reperio.recovery/iPhone backups",
            "BBB-222",
            "Recovered iPhone",
            "iPhone14,5",
            false,
        );
        // A folder of the tool's that is not a backup (no device Info.plist) is never offered.
        leave(&home.join("Library/Application Support/app.reperio.recovery/scans/x.json"), 5000, false);
        let safety = Safety::for_home(&home);
        let (items, notes, _) = read(&safety, &FilesOptions::default());
        assert!(notes.is_empty());
        let mut names: Vec<&str> = items.iter().filter(|i| i.grant == Grant::DeviceBackup).map(|i| i.title.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["Finder iPhone", "Recovered iPhone"], "both the Finder backup and the third-party one, nothing else");
        for item in items.iter().filter(|i| i.grant == Grant::DeviceBackup) {
            assert!(safety.check(&item.path, Grant::DeviceBackup).is_ok(), "{} should pass its gate", item.path.display());
        }
    }

    #[test]
    fn what_is_kept_is_marked_and_what_cannot_be_read_is_said() {
        let home = tempdir();
        leave(&home.join("Downloads/Slack.dmg"), 5000, false);
        let safety = Safety::for_home(&home);
        safety.set_kept(vec![home.join("Downloads/Slack.dmg")]);
        let (items, ..) = read(&safety, &FilesOptions::default());
        assert!(items[0].kept, "shown, and kept");
        // A backup folder macOS will not let anyone read.
        let folder = home.join("Library/Application Support/MobileSync/Backup");
        fs::create_dir_all(&folder).unwrap();
        fs::set_permissions(&folder, std::os::unix::fs::PermissionsExt::from_mode(0o000)).unwrap();
        let (_, notes, _) = read(&safety, &FilesOptions::default());
        fs::set_permissions(&folder, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("Full Disk Access"), "{notes:?}");
    }

    #[test]
    fn a_bare_home_has_nothing_and_a_cancelled_look_stops() {
        let home = tempdir();
        let safety = Safety::for_home(&home);
        let (items, notes, _) = read(&safety, &FilesOptions::default());
        assert!(items.is_empty() && notes.is_empty());
        leave(&home.join("Downloads/Slack.dmg"), 5000, false);
        let mut found = 0;
        scan(&safety, &FilesOptions::default(), &AtomicBool::new(true), &mut |e| {
            if matches!(e, FilesEvent::Found(_)) {
                found += 1;
            }
        });
        assert_eq!(found, 0);
        let ids: std::collections::HashSet<u64> = (0..3).map(|_| next_id()).collect();
        assert_eq!(ids.len(), 3);
    }

    #[test]
    fn developer_tool_stores_are_listed_one_at_a_time_and_each_passes_its_gate() {
        let home = tempdir();
        leave(&home.join(".rustup/toolchains/stable-aarch64-apple-darwin"), 9000, true);
        leave(&home.join(".rustup/toolchains/nightly-2024-01-01"), 7000, true);
        leave(&home.join(".nvm/versions/node/v20.11.1"), 5000, true);
        leave(&home.join(".pyenv/versions/3.12.1"), 6000, true);
        leave(&home.join(".ollama/models"), 8000, true);
        // The tool's own folder is never a row, and neither is an empty store.
        leave(&home.join(".cargo/bin"), 3000, true);
        fs::create_dir_all(home.join(".rustup/downloads")).unwrap();
        let safety = Safety::for_home(&home);
        let (items, notes, _) = read(&safety, &FilesOptions::default());
        assert!(notes.is_empty());
        let dev: Vec<&ScanItem> = items.iter().filter(|i| i.grant == Grant::DevStore).collect();
        let mut titles: Vec<&str> = dev.iter().map(|i| i.title.as_str()).collect();
        titles.sort_unstable();
        assert_eq!(
            titles,
            ["3.12.1", "Ollama models", "nightly-2024-01-01", "stable-aarch64-apple-darwin", "v20.11.1"],
            "each toolchain, Node version, Python version and the models folder, and nothing else (the empty downloads folder has no bytes)"
        );
        for item in &dev {
            assert_eq!(
                (item.kind, item.safety, item.tag, item.pre_selected),
                (Kind::Files, SafetyClass::Rebuildable, Tag::Rebuilds, false)
            );
            assert!(safety.check(&item.path, item.grant).is_ok(), "{} would be refused", item.path.display());
            assert!(!item.what.is_empty() && !item.comes_back.is_empty() && !item.source.is_empty() && item.owner.is_none());
            assert!(item.evidence.iter().any(|e| e.ok && e.text.contains("fetch or build again")));
            assert!(item.evidence.iter().any(|e| !e.ok && e.text.contains("cannot tell whether you still want it")));
        }
        let by = |t: &str| dev.iter().find(|i| i.title == t).unwrap();
        assert_eq!((by("v20.11.1").source.as_str(), by("Ollama models").source.as_str()), ("nvm", "Ollama"));
        assert_eq!(by("stable-aarch64-apple-darwin").source, "rustup");
        assert!(by("Ollama models").what.contains("pulled with Ollama"));
    }
}
