//! The Apps tab's data: what is installed, what each app left behind that begone can prove is its own, and what it leaves
//! alone. Nothing here changes the disk.
//!
//! The rule that keeps this honest: everything listed has already been admitted by the gate that will admit it again for
//! removal ([`Safety::check_app`]). A row that could never be removed is never shown, which is exactly where the first
//! uninstaller went wrong.
//!
//! Only two things are removed with an app: the bundle, and the caches, settings, saved state and web storage that carry
//! its exact bundle identifier. Data that may be your own (`Containers`, `Application Support`) is listed as *kept*, with its
//! size, so you can see where the rest of the space went and deal with it yourself.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use crate::model::{Evidence, Kind, SafetyClass, ScanItem, Tag, ago, idle_since};
use crate::safety::{self, Grant, Safety};
use crate::scan::procs::Procs;
use crate::sizing;

/// Ids for what an app's items are called, kept far from the cleaning scan's own so the two never meet.
pub fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1 << 40);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// An installed app, as its own bundle says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppIdentity {
    pub name: String,
    pub bundle_id: String,
    pub version: String,
    pub path: PathBuf,
}

/// Something an app left that begone does not remove, because it may hold your own data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Kept {
    pub path: PathBuf,
    pub what: &'static str,
    pub bytes: u64,
}

/// One installed app, with what removing it would move and what it would leave.
#[derive(Clone, Debug)]
pub struct AppEntry {
    pub identity: AppIdentity,
    /// The bundle first, then its traces. Each is ready to run: the gate has admitted it.
    pub items: Vec<ScanItem>,
    pub kept: Vec<Kept>,
    /// Things worth knowing that change nothing, such as why the app's settings stay.
    pub notes: Vec<String>,
    /// What removing this app would break, said before it is chosen and again when asked.
    pub cautions: Vec<String>,
    pub running: bool,
    pub last_used: Option<SystemTime>,
    /// The person asked begone to keep it: shown, never chosen, never moved. (Not `kept`: that is the data the app keeps.)
    pub kept_by_you: bool,
    /// An app that is no longer installed. There is no bundle: only the traces it left behind, listed by its identifier.
    pub leftover: bool,
}

/// What the machine tells the scan that the disk alone cannot.
#[derive(Clone, Debug, Default)]
pub struct AppsOptions {
    /// Ask Spotlight when each app was last opened. Off for a scratch home, whose apps Spotlight has never seen.
    pub spotlight: bool,
    /// Where `xcode-select` points (`/var/db/xcode_select_link`), so removing the Xcode it points into can say what stops working.
    pub developer_dir: Option<PathBuf>,
    /// What an app that is gone has to have left before it is listed: a few kilobytes of settings are not worth a row.
    pub min_leftover: u64,
}

/// A leftover smaller than this is not worth a line in the list.
pub const MIN_LEFTOVER: u64 = 1_000_000;

impl AppEntry {
    /// Everything that would move: the bundle and its traces.
    pub fn bytes(&self) -> u64 {
        self.items.iter().map(|i| i.size).sum()
    }

    /// What the app's own settings and caches add to the bundle. (A leftover is all traces.)
    pub fn trace_bytes(&self) -> u64 {
        self.items.iter().skip(usize::from(!self.leftover)).map(|i| i.size).sum()
    }

    /// How many traces would go with it.
    pub fn trace_count(&self) -> usize {
        self.items.len() - usize::from(!self.leftover)
    }

    pub fn kept_bytes(&self) -> u64 {
        self.kept.iter().map(|k| k.bytes).sum()
    }

    /// Shown instead of the app's own name when another installed app has the same one, so the list and the question can say
    /// which is which.
    fn relabel(&mut self, label: String) {
        for item in &mut self.items {
            item.source = label.clone();
        }
        self.identity.name = label;
    }

    /// What `x` keeps for this entry: the bundle of an app, since nothing of an app goes without it, and every trace of a
    /// leftover.
    pub fn keep_paths(&self) -> Vec<PathBuf> {
        if self.leftover { self.items.iter().map(|i| i.path.clone()).collect() } else { vec![self.identity.path.clone()] }
    }

    pub fn bundle(&self) -> &ScanItem {
        &self.items[0]
    }

    #[cfg(test)]
    pub fn ids(&self) -> impl Iterator<Item = u64> + '_ {
        self.items.iter().map(|i| i.id)
    }
}

pub enum AppsEvent {
    Progress(f32),
    Found(Box<AppEntry>),
    Finished { elapsed: Duration },
}

// ------------------------------------------------------------------------------------------------ what is installed

/// Read an app's identity from its Info.plist. `None` when it has no usable bundle identifier.
pub fn read_identity(bundle: &Path) -> Option<AppIdentity> {
    let value = plist::Value::from_file(bundle.join("Contents/Info.plist")).ok()?;
    let dict = value.as_dictionary()?;
    let text = |key: &str| dict.get(key).and_then(|v| v.as_string()).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let bundle_id = text("CFBundleIdentifier").filter(|id| safety::well_formed_id(id))?;
    let name = text("CFBundleDisplayName")
        .or_else(|| text("CFBundleName"))
        .or_else(|| bundle.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| bundle_id.clone());
    let version = text("CFBundleShortVersionString").or_else(|| text("CFBundleVersion")).unwrap_or_default();
    Some(AppIdentity { name, bundle_id, version, path: bundle.to_path_buf() })
}

/// The `.app` folders directly inside an apps folder, in whatever order the disk lists them.
fn bundles_in(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else { return Vec::new() };
    entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "app")).collect()
}

/// The third-party apps that can be removed, by name. Only what the gate admits: a direct child of an apps folder, not
/// a symlink, naming a non-Apple identifier.
pub fn discover(safety: &Safety) -> Vec<AppIdentity> {
    let mut apps: Vec<AppIdentity> = safety
        .app_roots()
        .iter()
        .flat_map(|root| bundles_in(root))
        .filter(|path| safety.check_app(path, Grant::AppBundle, None).is_ok())
        .filter_map(|path| read_identity(&path))
        .collect();
    apps.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then_with(|| a.path.cmp(&b.path)));
    apps
}

/// What each app is called in the list. Its own name, unless another installed app has the same one (two Xcodes both call
/// themselves Xcode), when it is the name of its folder, and the folder's place as well if even that is shared.
fn display_names(apps: &[AppIdentity], home: &Path) -> Vec<String> {
    fn counts<'a>(names: impl Iterator<Item = &'a str>) -> HashMap<String, usize> {
        let mut counts = HashMap::new();
        for name in names {
            *counts.entry(name.to_lowercase()).or_insert(0) += 1;
        }
        counts
    }
    let stems: Vec<String> =
        apps.iter().map(|a| a.path.file_stem().map_or_else(|| a.name.clone(), |s| s.to_string_lossy().into_owned())).collect();
    let same_name = counts(apps.iter().map(|a| a.name.as_str()));
    let same_folder = counts(apps.iter().zip(&stems).filter(|(a, _)| same_name[&a.name.to_lowercase()] > 1).map(|(_, s)| s.as_str()));
    apps.iter()
        .zip(stems)
        .map(|(a, stem)| {
            if same_name[&a.name.to_lowercase()] == 1 {
                a.name.clone()
            } else if same_folder[&stem.to_lowercase()] == 1 {
                stem
            } else {
                let place = a.path.parent().unwrap_or(Path::new("/"));
                let place = place.strip_prefix(home).map_or_else(|_| place.display().to_string(), |rest| format!("~/{}", rest.display()));
                format!("{stem} ({place})")
            }
        })
        .collect()
}

/// How many installed bundles carry each identifier, Apple's included. A trace is only removed once nothing installed still
/// claims its identifier. An alias (a symlink) is not an installed copy: it is the same app, and the gate never touches it.
pub fn installed_ids(safety: &Safety) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for bundle in safety.app_roots().iter().flat_map(|root| bundles_in(root)).filter(|b| is_real_folder(b)) {
        if let Some(id) = safety::bundle_identifier(&bundle) {
            *counts.entry(id).or_insert(0) += 1;
        }
    }
    counts
}

// ------------------------------------------------------------------------------------------------ what an app leaves

/// A trace an app leaves, named by its identifier.
#[derive(Clone, Copy, Debug)]
pub struct TraceKind {
    pub folder: &'static str,
    /// What follows the identifier in the name: `""`, `".plist"`, `".savedState"`, `".binarycookies"`.
    pub suffix: &'static str,
    pub what: &'static str,
}

/// The only traces removed with an app. This mirrors the safety gate name for name; the gate decides.
pub const TRACES: [TraceKind; 7] = [
    TraceKind { folder: "Caches", suffix: "", what: "its caches" },
    TraceKind { folder: "Preferences", suffix: ".plist", what: "its settings" },
    TraceKind { folder: "Saved Application State", suffix: ".savedState", what: "its saved window state" },
    TraceKind { folder: "HTTPStorages", suffix: "", what: "its web storage" },
    TraceKind { folder: "HTTPStorages", suffix: ".binarycookies", what: "its web cookies" },
    TraceKind { folder: "WebKit", suffix: "", what: "its web data" },
    TraceKind { folder: "Cookies", suffix: ".binarycookies", what: "its cookies" },
];

fn trace_path(library: &Path, kind: &TraceKind, id: &str) -> PathBuf {
    library.join(kind.folder).join(format!("{id}{}", kind.suffix))
}

/// Folders that may hold an app's own data, listed but never removed.
fn kept_candidates(library: &Path, app: &AppIdentity, names: &HashMap<String, usize>) -> Vec<(PathBuf, &'static str)> {
    let mut out = vec![
        (library.join("Containers").join(&app.bundle_id), "its sandbox container, which may hold your own documents"),
        (library.join("Application Support").join(&app.bundle_id), "its data folder"),
    ];
    // By name only when no other installed app shares it, since a name proves less than an identifier.
    if names.get(&app.name.to_lowercase()) == Some(&1) && app.name != app.bundle_id {
        out.push((library.join("Application Support").join(&app.name), "a folder named like the app"));
    }
    // Xcode keeps most of its space outside the app, where this tab does not touch it.
    if app.bundle_id.eq_ignore_ascii_case("com.apple.dt.Xcode") {
        out.push((library.join("Developer/Xcode"), "its DerivedData, Archives and settings (the Overview offers the DerivedData)"));
        out.push((library.join("Developer/CoreSimulator"), "its simulators"));
    }
    out
}

fn is_real_folder(path: &Path) -> bool {
    path.symlink_metadata().is_ok_and(|m| m.is_dir())
}

fn item_for(
    app: &AppIdentity,
    path: PathBuf,
    grant: Grant,
    m: sizing::Measure,
    what: String,
    comes_back: &str,
    evidence: Vec<Evidence>,
) -> ScanItem {
    ScanItem {
        id: next_id(),
        title: path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        path,
        size: m.bytes,
        files: m.files,
        kind: Kind::Apps,
        safety: if grant == Grant::AppBundle { SafetyClass::Rebuildable } else { SafetyClass::Safe },
        tag: Tag::Safe,
        grant,
        source: app.name.clone(),
        what,
        comes_back: comes_back.to_string(),
        evidence,
        modified: m.newest,
        pre_selected: false,
        owner: Some(app.bundle_id.clone()),
        kept: false,
    }
}

/// Everything about one installed app. `copies` is how many installed bundles share its identifier.
fn build_entry(
    safety: &Safety,
    identity: AppIdentity,
    copies: usize,
    names: &HashMap<String, usize>,
    procs: &Procs,
    last_used: Option<SystemTime>,
    developer_dir: Option<&Path>,
) -> AppEntry {
    let library = safety.home_real().join("Library");
    let running = procs.bundle_running(&identity.path, &identity.bundle_id);
    let id = identity.bundle_id.clone();
    // The few Apple apps that can be removed are removed as a bundle and nothing else: their caches and settings are shared
    // with macOS's own tools.
    let apple = safety::is_apple_id(&id);
    let (mut notes, mut cautions) = (Vec::new(), Vec::new());
    let mut evidence = vec![
        Evidence::good(if apple {
            format!("An Apple app you installed yourself ({id}): the App Store or developer.apple.com brings it back")
        } else {
            format!("An installed app, identified as {id}")
        }),
        Evidence::good("It goes to the Trash, so it can be put back"),
    ];
    if running {
        evidence.push(Evidence::warn("It is running right now: quit it first"));
    }
    let m = sizing::measure(&identity.path);
    let mut items = vec![item_for(
        &identity,
        identity.path.clone(),
        Grant::AppBundle,
        m,
        format!(
            "The app itself, {}.",
            if identity.version.is_empty() { "version unknown".to_string() } else { format!("version {}", identity.version) }
        ),
        if apple { "Download it again from the App Store or developer.apple.com." } else { "Reinstall it from wherever you got it." },
        evidence,
    )];
    if apple {
        notes.push("Its caches and settings stay: macOS's developer tools share them.".to_string());
        if developer_dir.is_some_and(|dir| dir.starts_with(&identity.path)) {
            cautions.push(
                "xcode-select points here, so git, clang and other command line tools stop working until you run: sudo xcode-select --reset"
                    .to_string(),
            );
        }
    } else if copies > 1 {
        notes.push("Another copy of this app is installed, so its settings and caches stay.".to_string());
    } else {
        for kind in &TRACES {
            let path = trace_path(&library, kind, &id);
            if safety.check_app(&path, Grant::AppData, Some(&id)).is_err() {
                continue;
            }
            let m = sizing::measure(&path);
            if m.bytes == 0 {
                continue;
            }
            let evidence = vec![
                Evidence::good(format!("Named exactly {id}, so it belongs to this app")),
                Evidence::good("Settings and state, not your documents"),
            ];
            items.push(item_for(
                &identity,
                path,
                Grant::AppData,
                m,
                format!("{} is {}.", capital(&identity.name), kind.what),
                "The app makes a fresh one if you reinstall it.",
                evidence,
            ));
        }
    }
    let kept = kept_candidates(&library, &identity, names)
        .into_iter()
        .filter(|(path, _)| is_real_folder(path))
        .map(|(path, what)| {
            let bytes = sizing::measure(&path).bytes;
            Kept { path, what, bytes }
        })
        .filter(|k| k.bytes > 0)
        .collect();
    AppEntry { identity, items, kept, notes, cautions, running, last_used, kept_by_you: false, leftover: false }
}

fn capital(s: &str) -> String {
    s.to_string()
}

// ------------------------------------------------------------------------------------------------ what is left of apps that are gone

/// How many installed bundles carry this identifier, whatever case it is written in.
pub fn count_installed(installed: &HashMap<String, usize>, id: &str) -> usize {
    installed.iter().filter(|(known, _)| known.eq_ignore_ascii_case(id)).map(|(_, n)| *n).sum()
}

/// Could this identifier be an installed app's, or a part or helper of one (`com.vendor.app.ShipIt`)? Any dotted prefix
/// between the two says it could, so what carries it is not taken for the leftovers of an app that is gone.
fn related_to_installed(installed: &HashMap<String, usize>, id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    installed.keys().any(|known| {
        let known = known.to_ascii_lowercase();
        known == id || id.starts_with(&format!("{known}.")) || known.starts_with(&format!("{id}."))
    })
}

/// The identifiers that the folders and files apps leave in the Library are named by.
fn trace_names(library: &Path) -> std::collections::BTreeSet<String> {
    let mut ids = std::collections::BTreeSet::new();
    for kind in &TRACES {
        let Ok(entries) = std::fs::read_dir(library.join(kind.folder)) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(id) = name.strip_suffix(kind.suffix) else { continue };
            // `x.plist` in a folder of folders is not an identifier: it is a file that belongs to `x`.
            if [".plist", ".savedState", ".binarycookies"].iter().any(|s| id.ends_with(s)) {
                continue;
            }
            if safety::well_formed_id(id) && !safety::is_apple_id(id) {
                ids.insert(id.to_string());
            }
        }
    }
    ids
}

/// Identifiers named in the Library that no installed app carries and no running app is using.
fn leftover_ids(safety: &Safety, procs: &Procs, installed: &HashMap<String, usize>) -> Vec<String> {
    trace_names(&safety.home_real().join("Library"))
        .into_iter()
        .filter(|id| !related_to_installed(installed, id) && procs.app_for(id).is_none())
        .collect()
}

/// Does Spotlight know of an app with this identifier anywhere (another folder, an external disk)?
fn spotlight_has_app(id: &str) -> bool {
    // The identifier is made only of letters, digits, `.`, `-` and `_`, so it cannot change what is asked.
    let query = format!("kMDItemCFBundleIdentifier == '{id}'c");
    std::process::Command::new("/usr/bin/mdfind").arg(query).output().is_ok_and(|out| out.stdout.iter().any(|b| !b.is_ascii_whitespace()))
}

/// What a gone app left in the Library, as one entry named by its identifier. `None` when nothing it left can be removed, or
/// when Spotlight says the app is somewhere after all.
fn build_leftover(safety: &Safety, id: &str, spotlight: bool, min_bytes: u64) -> Option<AppEntry> {
    let library = safety.home_real().join("Library");
    let mut identity = AppIdentity { name: id.to_string(), bundle_id: id.to_string(), version: String::new(), path: PathBuf::new() };
    let mut items = Vec::new();
    for kind in &TRACES {
        let path = trace_path(&library, kind, id);
        if safety.check_app(&path, Grant::AppData, Some(id)).is_err() {
            continue;
        }
        let m = sizing::measure(&path);
        if m.bytes == 0 {
            continue;
        }
        let mut evidence = vec![
            Evidence::good(format!("Named exactly {id}, so it belonged to that app")),
            Evidence::good("No app with that identifier is installed"),
            Evidence::good("Settings and state, not your documents"),
        ];
        if let Some(idle) = idle_since(m.newest) {
            evidence.push(Evidence::good(format!("Last changed {}", ago(idle))));
        }
        items.push(item_for(
            &identity,
            path,
            Grant::AppData,
            m,
            format!("Left behind by {id}: {}.", kind.what),
            "Nothing needs it while the app is gone. If you install the app again it makes a fresh one.",
            evidence,
        ));
    }
    if items.is_empty() || items.iter().map(|i| i.size).sum::<u64>() < min_bytes || (spotlight && spotlight_has_app(id)) {
        return None;
    }
    items.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path)));
    identity.path = items[0].path.clone();
    let newest = items.iter().filter_map(|i| i.modified).max();
    let mut cautions = Vec::new();
    if let Some(idle) = idle_since(newest).filter(|idle| *idle < Duration::from_secs(30 * 86_400)) {
        cautions.push(format!(
            "Something in it changed {}, so an app begone cannot see may still use it. Check before you remove it.",
            ago(idle)
        ));
    }
    let kept = [("Containers", "its sandbox container, which may hold your own documents"), ("Application Support", "its data folder")]
        .into_iter()
        .map(|(folder, what)| (library.join(folder).join(id), what))
        .filter(|(path, _)| is_real_folder(path))
        .map(|(path, what)| Kept { bytes: sizing::measure(&path).bytes, path, what })
        .filter(|k| k.bytes > 0)
        .collect();
    Some(AppEntry {
        identity,
        items,
        kept,
        notes: vec!["The app is not installed. This is only what it left behind.".to_string()],
        cautions,
        running: false,
        last_used: None,
        kept_by_you: false,
        leftover: true,
    })
}

// ------------------------------------------------------------------------------------------------ the scan

/// Find every removable app and size it with what it left. Entries arrive as they are finished, biggest work last.
///
/// `options` carries what only the real machine can say; the default is right for a scratch home and for tests.
pub fn scan(safety: &Safety, procs: &Procs, options: &AppsOptions, cancel: &AtomicBool, on_event: &mut dyn FnMut(AppsEvent)) {
    let started = Instant::now();
    let apps = discover(safety);
    let labels = display_names(&apps, safety.home_real());
    on_event(AppsEvent::Progress(0.0));
    let mut copies: HashMap<&str, usize> = HashMap::new();
    let mut names: HashMap<String, usize> = HashMap::new();
    for app in &apps {
        *copies.entry(app.bundle_id.as_str()).or_insert(0) += 1;
        *names.entry(app.name.to_lowercase()).or_insert(0) += 1;
    }
    // A copy of an app that is not removable here (Apple's, say) still counts as installed.
    let installed = installed_ids(safety);
    // What is left of apps that are gone. On the real machine Spotlight must answer for an app that is certainly there
    // before its silence about another is believed: with Spotlight off, nothing could be proven gone.
    let leftovers =
        if !options.spotlight || spotlight_has_app("com.apple.finder") { leftover_ids(safety, procs, &installed) } else { Vec::new() };
    let total = apps.len() + leftovers.len();
    let next = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel::<Option<AppEntry>>();
    std::thread::scope(|scope| {
        for _ in 0..4.min(total.max(1)) {
            let (tx, next, apps, copies, names, installed, labels, leftovers) =
                (tx.clone(), &next, &apps, &copies, &names, &installed, &labels, &leftovers);
            let developer_dir = options.developer_dir.as_deref();
            scope.spawn(move || {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= total || cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    if i >= apps.len() {
                        if tx.send(build_leftover(safety, &leftovers[i - apps.len()], options.spotlight, options.min_leftover)).is_err() {
                            break;
                        }
                        continue;
                    }
                    let identity = apps[i].clone();
                    let last_used = if options.spotlight { spotlight_last_used(&identity.path) } else { None };
                    let shared = copies
                        .get(identity.bundle_id.as_str())
                        .copied()
                        .unwrap_or(1)
                        .max(installed.get(&identity.bundle_id).copied().unwrap_or(1));
                    let mut entry = build_entry(safety, identity, shared, names, procs, last_used, developer_dir);
                    if entry.identity.name != labels[i] {
                        entry.relabel(labels[i].clone());
                    }
                    entry.kept_by_you = safety.is_kept(&entry.identity.path);
                    if tx.send(Some(entry)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        let mut done = 0;
        for entry in rx {
            done += 1;
            on_event(AppsEvent::Progress(done as f32 / total.max(1) as f32));
            if let Some(mut entry) = entry {
                entry.kept_by_you = entry.keep_paths().iter().any(|p| safety.is_kept(p));
                on_event(AppsEvent::Found(Box::new(entry)));
            }
        }
    });
    on_event(AppsEvent::Finished { elapsed: started.elapsed() });
}

/// When Spotlight last saw the app opened, if it knows.
pub(crate) fn spotlight_last_used(bundle: &Path) -> Option<SystemTime> {
    let out = std::process::Command::new("/usr/bin/mdls").args(["-raw", "-name", "kMDItemLastUsedDate"]).arg(bundle).output().ok()?;
    parse_mdls_date(&String::from_utf8_lossy(&out.stdout))
}

/// `2026-11-03 10:12:00 +0000`, as `mdls` writes a date.
pub fn parse_mdls_date(text: &str) -> Option<SystemTime> {
    let mut parts = text.split_whitespace();
    let (date, time) = (parts.next()?, parts.next()?);
    let zone = parts.next().unwrap_or("+0000");
    let mut d = date.split('-').map(|n| n.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let mut t = time.split(':').map(|n| n.parse::<i64>().ok());
    let (hh, mm, ss) = (t.next()??, t.next()??, t.next()??);
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    let offset = {
        let sign = if zone.starts_with('-') { -1 } else { 1 };
        let digits: String = zone.chars().filter(char::is_ascii_digit).collect();
        if digits.len() != 4 {
            return None;
        }
        sign * (digits[..2].parse::<i64>().ok()? * 3600 + digits[2..].parse::<i64>().ok()? * 60)
    };
    // Days from the civil calendar (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hh * 3600 + mm * 60 + ss - offset;
    u64::try_from(secs).ok().map(|s| SystemTime::UNIX_EPOCH + Duration::from_secs(s))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safety::tests::{fake_app, leave, tempdir};
    use std::fs;

    fn scan_all(safety: &Safety, procs: &Procs) -> Vec<AppEntry> {
        scan_with(safety, procs, None)
    }

    /// The same, told where `xcode-select` points.
    fn scan_with(safety: &Safety, procs: &Procs, developer_dir: Option<PathBuf>) -> Vec<AppEntry> {
        let mut found = Vec::new();
        let options = AppsOptions { spotlight: false, developer_dir, min_leftover: 0 };
        scan(safety, procs, &options, &AtomicBool::new(false), &mut |e| {
            if let AppsEvent::Found(entry) = e {
                found.push(*entry);
            }
        });
        found.sort_by(|a, b| a.identity.name.cmp(&b.identity.name));
        found
    }

    /// What a real Xcode leaves around it: the app, caches and settings that macOS's developer tools share, and the big
    /// folders under Library/Developer.
    fn put_xcode_in(home: &Path) -> PathBuf {
        let lib = home.join("Library");
        let xcode = fake_app(&home.join("Applications"), "Xcode", "com.apple.dt.Xcode", 9000);
        fs::create_dir_all(xcode.join("Contents/Developer")).unwrap();
        leave(&lib.join("Caches/com.apple.dt.Xcode"), 3000, true);
        leave(&lib.join("Preferences/com.apple.dt.Xcode.plist"), 100, false);
        leave(&lib.join("Saved Application State/com.apple.dt.Xcode.savedState"), 200, true);
        leave(&lib.join("Developer/Xcode/DerivedData/Project-abc"), 5000, true);
        leave(&lib.join("Developer/Xcode/Archives"), 400, true);
        leave(&lib.join("Developer/CoreSimulator/Devices/ABC"), 8000, true);
        xcode
    }

    #[test]
    fn only_what_the_gate_would_admit_is_listed_and_in_name_order() {
        let home = tempdir();
        let apps = home.join("Applications");
        fake_app(&apps, "zoom.us", "us.zoom.xos", 10);
        fake_app(&apps, "Alpha", "com.vendor.alpha", 10);
        fake_app(&apps, "Pages", "com.apple.iWork.Pages", 10);
        fake_app(&apps.join("Suite"), "Nested", "com.vendor.nested", 10);
        fake_app(&home.join("code"), "Elsewhere", "com.vendor.elsewhere", 10);
        fs::create_dir_all(apps.join("NoPlist.app/Contents")).unwrap();
        std::os::unix::fs::symlink(apps.join("Alpha.app"), apps.join("Link.app")).unwrap();
        leave(&apps.join("notanapp.txt"), 5, false);
        let safety = Safety::for_home(&home);
        let names: Vec<String> = discover(&safety).into_iter().map(|a| a.name).collect();
        assert_eq!(names, ["Alpha", "zoom.us"], "no Apple app, nothing nested, no symlink, no bundle without an identifier");
        let ids = installed_ids(&safety);
        assert_eq!(ids.get("com.apple.iWork.Pages"), Some(&1), "Apple's apps still count as installed");
        assert_eq!(ids.get("com.vendor.alpha"), Some(&1));
        assert!(!ids.contains_key("com.vendor.nested"));
    }

    #[test]
    fn an_identity_falls_back_from_display_name_to_name_to_the_folder_name() {
        let home = tempdir();
        let bundle = fake_app(&home.join("Applications"), "Thing", "com.vendor.thing", 1);
        let id = read_identity(&bundle).unwrap();
        assert_eq!((id.name.as_str(), id.bundle_id.as_str(), id.version.as_str()), ("Thing", "com.vendor.thing", "1.2.3"));
        fs::write(
            bundle.join("Contents/Info.plist"),
            "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>CFBundleIdentifier</key><string>com.vendor.thing</string><key>CFBundleDisplayName</key><string>  A Nicer Name </string></dict></plist>",
        )
        .unwrap();
        let id = read_identity(&bundle).unwrap();
        assert_eq!((id.name.as_str(), id.version.as_str()), ("A Nicer Name", ""));
        fs::write(bundle.join("Contents/Info.plist"), "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>CFBundleIdentifier</key><string>com.vendor.thing</string></dict></plist>").unwrap();
        assert_eq!(read_identity(&bundle).unwrap().name, "Thing");
        fs::write(bundle.join("Contents/Info.plist"), "not a plist").unwrap();
        assert!(read_identity(&bundle).is_none());
    }

    #[test]
    fn an_app_comes_with_its_exact_traces_and_leaves_what_may_be_your_data() {
        let home = tempdir();
        let lib = home.join("Library");
        fake_app(&home.join("Applications"), "Slack", "com.tinyspeck.slackmacgap", 5000);
        leave(&lib.join("Caches/com.tinyspeck.slackmacgap"), 3000, true);
        leave(&lib.join("Preferences/com.tinyspeck.slackmacgap.plist"), 100, false);
        leave(&lib.join("Saved Application State/com.tinyspeck.slackmacgap.savedState"), 200, true);
        leave(&lib.join("HTTPStorages/com.tinyspeck.slackmacgap"), 300, true);
        // Look-alikes that are not its own.
        leave(&lib.join("Caches/com.tinyspeck.slackmacgap.ShipIt"), 400, true);
        leave(&lib.join("Caches/com.tinyspeck.other"), 400, true);
        leave(&lib.join("Preferences/com.tinyspeck.slackmacgap.plist.lockfile"), 40, false);
        // Data that may be yours: kept, not removed.
        leave(&lib.join("Containers/com.tinyspeck.slackmacgap"), 7000, true);
        leave(&lib.join("Application Support/Slack"), 9000, true);
        let safety = Safety::for_home(&home);
        let entries = scan_all(&safety, &Procs::none());
        // The look-alike of another app is not Slack's: it shows up as what an app that is gone left, under its own name.
        let (left, installed): (Vec<&AppEntry>, Vec<&AppEntry>) = entries.iter().partition(|e| e.leftover);
        assert_eq!(left.iter().map(|e| e.identity.name.as_str()).collect::<Vec<_>>(), ["com.tinyspeck.other"]);
        assert_eq!(installed.len(), 1);
        let slack = installed[0];
        let names: Vec<&str> = slack.items.iter().map(|i| i.title.as_str()).collect();
        assert_eq!(
            names,
            [
                "Slack.app",
                "com.tinyspeck.slackmacgap",
                "com.tinyspeck.slackmacgap.plist",
                "com.tinyspeck.slackmacgap.savedState",
                "com.tinyspeck.slackmacgap"
            ]
        );
        assert!(
            slack.items.iter().all(|i| i.kind == Kind::Apps && !i.pre_selected && i.owner.as_deref() == Some("com.tinyspeck.slackmacgap"))
        );
        assert_eq!(slack.items[0].grant, Grant::AppBundle);
        assert!(slack.items[1..].iter().all(|i| i.grant == Grant::AppData));
        for item in &slack.items {
            assert!(
                safety.check_app(&item.path, item.grant, item.owner.as_deref()).is_ok(),
                "{} was listed but would be refused",
                item.path.display()
            );
            assert!(item.size > 0 && !item.evidence.is_empty());
        }
        assert!(slack.bytes() > slack.trace_bytes() && slack.trace_bytes() >= 3600, "{} {}", slack.bytes(), slack.trace_bytes());
        let kept: Vec<(&str, u64)> = slack.kept.iter().map(|k| (k.path.file_name().unwrap().to_str().unwrap(), k.bytes)).collect();
        assert_eq!(kept.len(), 2, "{kept:?}");
        assert!(slack.kept_bytes() >= 16_000);
        assert!(
            slack.kept.iter().any(|k| k.what.contains("documents")) && slack.kept.iter().any(|k| k.what.contains("named like the app"))
        );
        assert!(!slack.running && slack.notes.is_empty());
    }

    #[test]
    fn a_second_copy_of_an_app_keeps_the_traces_out_of_the_list() {
        let home = tempdir();
        let lib = home.join("Library");
        fake_app(&home.join("Applications"), "Twin", "com.vendor.twin", 100);
        leave(&lib.join("Caches/com.vendor.twin"), 500, true);
        let safety = Safety::for_home(&home);
        assert_eq!(scan_all(&safety, &Procs::none())[0].items.len(), 2, "one copy: the bundle and its cache");
        // Another copy somewhere else under the apps folder's reach: a second bundle with the same identifier.
        let second_root = home.join("Applications");
        let other = second_root.join("Twin 2.app");
        fs::create_dir_all(other.join("Contents")).unwrap();
        fs::copy(second_root.join("Twin.app/Contents/Info.plist"), other.join("Contents/Info.plist")).unwrap();
        fs::write(other.join("Contents/data"), vec![1u8; 100]).unwrap();
        let entries = scan_all(&safety, &Procs::none());
        assert_eq!(entries.len(), 2);
        for e in &entries {
            assert_eq!(
                e.items.len(),
                1,
                "with two copies installed, neither lists the traces: {:?}",
                e.items.iter().map(|i| &i.title).collect::<Vec<_>>()
            );
            assert!(e.notes.iter().any(|n| n.contains("Another copy")));
        }
    }

    #[test]
    fn a_name_shared_by_two_apps_proves_nothing_so_only_the_identifier_folder_is_kept() {
        let home = tempdir();
        let lib = home.join("Library");
        fake_app(&home.join("Applications"), "Same", "com.one.same", 10);
        fake_app(&home.join("Applications"), "Same Too", "com.two.same", 10);
        fs::write(
            home.join("Applications/Same Too.app/Contents/Info.plist"),
            "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>CFBundleIdentifier</key><string>com.two.same</string><key>CFBundleName</key><string>Same</string></dict></plist>",
        )
        .unwrap();
        leave(&lib.join("Application Support/Same"), 900, true);
        leave(&lib.join("Application Support/com.one.same"), 700, true);
        let entries = scan_all(&Safety::for_home(&home), &Procs::none());
        let one = entries.iter().find(|e| e.identity.bundle_id == "com.one.same").unwrap();
        assert_eq!(one.kept.len(), 1, "the folder named Same could be either app's");
        assert!(one.kept[0].path.ends_with("com.one.same"));
    }

    #[test]
    fn a_running_app_is_marked_and_says_so() {
        let home = tempdir();
        fake_app(&home.join("Applications"), "Busy", "com.vendor.busy", 10);
        let procs = Procs::with(&[("com.vendor.busy", "Busy")]);
        let entry = &scan_all(&Safety::for_home(&home), &procs)[0];
        assert!(entry.running && entry.bundle().evidence.iter().any(|e| !e.ok && e.text.contains("running")));
    }

    #[test]
    fn the_scan_streams_every_app_once_reports_progress_and_stops_when_asked() {
        let home = tempdir();
        for n in 0..9 {
            fake_app(&home.join("Applications"), &format!("App{n}"), &format!("com.vendor.app{n}"), 10 + n);
        }
        let safety = Safety::for_home(&home);
        let (mut found, mut progress, mut finished) = (0, Vec::new(), None);
        scan(&safety, &Procs::none(), &AppsOptions::default(), &AtomicBool::new(false), &mut |e| match e {
            AppsEvent::Found(_) => found += 1,
            AppsEvent::Progress(p) => progress.push(p),
            AppsEvent::Finished { .. } => finished = Some(9),
        });
        assert_eq!((found, finished), (9, Some(9)));
        assert!(
            progress.windows(2).all(|p| p[0] <= p[1]) && progress.first() == Some(&0.0) && (progress.last().unwrap() - 1.0).abs() < 1e-6
        );
        let mut found = 0;
        scan(&safety, &Procs::none(), &AppsOptions::default(), &AtomicBool::new(true), &mut |e| {
            if let AppsEvent::Found(_) = e {
                found += 1;
            }
        });
        assert_eq!(found, 0, "cancelled before it began");
        let ids: std::collections::HashSet<u64> =
            scan_all(&safety, &Procs::none()).iter().flat_map(|e| e.ids().collect::<Vec<_>>()).collect();
        assert_eq!(ids.len(), 9, "every item has an id of its own");
        assert!(ids.iter().all(|id| *id >= 1 << 40), "and none can be mistaken for a cleaning item");
    }

    #[test]
    fn xcode_is_offered_as_the_app_alone_the_rest_of_apple_is_not_and_what_stays_is_listed() {
        let home = tempdir();
        let xcode = put_xcode_in(&home);
        fake_app(&home.join("Applications"), "Pages", "com.apple.iWork.Pages", 10);
        let safety = Safety::for_home(&home);
        let entries = scan_all(&safety, &Procs::none());
        assert_eq!(entries.len(), 1, "Pages is Apple's and stays out of the list: Xcode is the one Apple app on it");
        let x = &entries[0];
        assert_eq!((x.identity.name.as_str(), x.identity.bundle_id.as_str()), ("Xcode", "com.apple.dt.Xcode"));
        assert_eq!(x.items.len(), 1, "the app alone: its caches and settings are shared with macOS's developer tools");
        assert_eq!((&x.items[0].path, x.items[0].grant), (&xcode, Grant::AppBundle));
        assert!(x.bytes() >= 9000 && x.trace_bytes() == 0);
        assert!(x.items[0].evidence.iter().any(|e| e.ok && e.text.contains("Apple app you installed yourself")));
        assert!(x.items[0].comes_back.contains("App Store"), "{}", x.items[0].comes_back);
        assert!(x.notes.iter().any(|n| n.contains("caches and settings stay")), "{:?}", x.notes);
        assert!(safety.check_app(&x.items[0].path, x.items[0].grant, x.items[0].owner.as_deref()).is_ok());
        // The big folders under Library/Developer are listed so the number is honest, and never offered.
        let kept: Vec<&Path> = x.kept.iter().map(|k| k.path.as_path()).collect();
        assert_eq!(kept, [home.join("Library/Developer/Xcode"), home.join("Library/Developer/CoreSimulator")]);
        assert!(x.kept[0].bytes >= 5400 && x.kept[1].bytes >= 8000 && x.kept_bytes() >= 13_400, "{:?}", x.kept);
        assert!(x.kept[0].what.contains("DerivedData") && x.kept[1].what.contains("simulators"));
    }

    #[test]
    fn xcode_says_what_stops_working_only_when_xcode_select_points_into_it() {
        let home = tempdir();
        let xcode = put_xcode_in(&home);
        let safety = Safety::for_home(&home);
        let pointed = scan_with(&safety, &Procs::none(), Some(xcode.join("Contents/Developer")));
        assert_eq!(pointed[0].cautions.len(), 1, "{:?}", pointed[0].cautions);
        let caution = &pointed[0].cautions[0];
        assert!(
            caution.contains("xcode-select") && caution.contains("git") && caution.contains("sudo xcode-select --reset"),
            "it says what breaks and how to mend it: {caution}"
        );
        for elsewhere in [
            None,
            Some(PathBuf::from("/Library/Developer/CommandLineTools")),
            Some(home.join("Applications/Xcode-beta.app/Contents/Developer")),
            Some(home.join("Applications/Xcode.app.old/Contents/Developer")),
        ] {
            let entries = scan_with(&safety, &Procs::none(), elsewhere.clone());
            assert!(entries[0].cautions.is_empty(), "{elsewhere:?} is not inside this Xcode: {:?}", entries[0].cautions);
        }
    }

    #[test]
    fn two_copies_of_xcode_are_each_the_bundle_alone_and_only_the_one_xcode_select_uses_is_cautioned() {
        let home = tempdir();
        put_xcode_in(&home);
        let old = fake_app(&home.join("Applications"), "Xcode-15.4", "com.apple.dt.Xcode", 4000);
        let entries = scan_with(&Safety::for_home(&home), &Procs::none(), Some(old.join("Contents/Developer")));
        assert_eq!(entries.len(), 2);
        for e in &entries {
            assert_eq!(e.items.len(), 1, "{:?}", e.items.iter().map(|i| &i.title).collect::<Vec<_>>());
            assert_eq!(e.items[0].grant, Grant::AppBundle);
        }
        let cautioned: Vec<&Path> = entries.iter().filter(|e| !e.cautions.is_empty()).map(|e| e.identity.path.as_path()).collect();
        assert_eq!(cautioned, [old.as_path()], "only removing the copy the command line tools use would break them");
    }

    #[test]
    fn of_two_xcodes_only_the_one_that_is_open_is_running() {
        let home = tempdir();
        let new = put_xcode_in(&home);
        let old = fake_app(&home.join("Applications"), "Xcode-15.4", "com.apple.dt.Xcode", 4000);
        let open = Procs::from_ps(&format!("{}/Contents/MacOS/Xcode -psn_0_1\n", new.display()));
        let entries = scan_all(&Safety::for_home(&home), &open);
        let running: Vec<&Path> = entries.iter().filter(|e| e.running).map(|e| e.identity.path.as_path()).collect();
        assert_eq!(running, [new.as_path()], "the old copy can be removed while the new one is open");
        let old_entry = entries.iter().find(|e| e.identity.path == old).unwrap();
        assert!(!old_entry.running && !old_entry.bundle().evidence.iter().any(|e| !e.ok));
        let busy = scan_all(&Safety::for_home(&home), &Procs::with(&[("com.apple.dt.Xcode", "Xcode")]));
        assert!(busy.iter().all(|e| e.running), "told only that Xcode is open, neither copy is taken for idle");
    }

    #[test]
    fn apps_that_share_a_name_are_told_apart_by_their_folders_and_where_those_are() {
        let id = |name: &str, path: &str| AppIdentity {
            name: name.to_string(),
            bundle_id: "com.vendor.x".to_string(),
            version: String::new(),
            path: PathBuf::from(path),
        };
        let apps = [
            id("Slack", "/Applications/Slack.app"),
            id("Xcode", "/Applications/Xcode.app"),
            id("xcode", "/Applications/Xcode-15.4.app"),
            id("Figma", "/Applications/Figma.app"),
            id("Figma", "/Users/me/Applications/Figma.app"),
        ];
        assert_eq!(
            display_names(&apps, Path::new("/Users/me")),
            ["Slack", "Xcode", "Xcode-15.4", "Figma (/Applications)", "Figma (~/Applications)"],
            "a name that is alone is kept; shared ones use their folder, and the place when the folders are shared too"
        );
    }

    #[test]
    fn two_xcodes_that_both_call_themselves_xcode_are_listed_by_their_folders_and_claim_no_data_by_name() {
        let home = tempdir();
        put_xcode_in(&home);
        let old = fake_app(&home.join("Applications"), "Xcode-15.4", "com.apple.dt.Xcode", 4000);
        fs::write(
            old.join("Contents/Info.plist"),
            "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>CFBundleIdentifier</key><string>com.apple.dt.Xcode</string><key>CFBundleName</key><string>Xcode</string><key>CFBundleShortVersionString</key><string>15.4</string></dict></plist>",
        )
        .unwrap();
        leave(&home.join("Library/Application Support/Xcode"), 500, true);
        let entries = scan_all(&Safety::for_home(&home), &Procs::none());
        let names: Vec<&str> = entries.iter().map(|e| e.identity.name.as_str()).collect();
        assert_eq!(names, ["Xcode", "Xcode-15.4"], "they could not be told apart in the list or in the question otherwise");
        for e in &entries {
            assert!(e.items.iter().all(|i| i.source == e.identity.name));
            assert!(
                e.kept.iter().all(|k| !k.path.ends_with("Application Support/Xcode")),
                "a folder named Xcode could be either copy's, so neither claims it"
            );
        }
        assert_eq!(entries[1].identity.version, "15.4");
    }

    fn backdate(path: &Path) {
        assert!(std::process::Command::new("touch").args(["-t", "202501010000"]).arg(path).status().unwrap().success());
    }

    #[test]
    fn what_an_app_left_when_it_was_removed_is_listed_by_its_identifier_when_nothing_installed_claims_it() {
        let home = tempdir();
        let lib = home.join("Library");
        // An installed app and what belongs to it, a helper of it, and one spelled in another case: none of them is a leftover.
        fake_app(&home.join("Applications"), "Here", "com.vendor.here", 100);
        leave(&lib.join("Caches/com.vendor.here"), 500, true);
        leave(&lib.join("Caches/com.vendor.here.ShipIt"), 500, true);
        fake_app(&home.join("Applications"), "Cased", "com.Cased.App", 100);
        leave(&lib.join("Caches/com.cased.app"), 500, true);
        // An app that is gone.
        leave(&lib.join("Caches/com.gone.app"), 3000, true);
        leave(&lib.join("Preferences/com.gone.app.plist"), 100, false);
        leave(&lib.join("Saved Application State/com.gone.app.savedState"), 200, true);
        leave(&lib.join("Containers/com.gone.app"), 7000, true);
        leave(&lib.join("Application Support/com.gone.app"), 9000, true);
        // Never leftovers: Apple's, no identifier, a running app's, a file where a folder belongs, a symlink.
        leave(&lib.join("Caches/com.apple.Safari"), 500, true);
        leave(&lib.join("Caches/Homebrew"), 500, true);
        leave(&lib.join("Caches/com.running.app"), 500, true);
        leave(&lib.join("Caches/com.file.app"), 500, false);
        std::os::unix::fs::symlink(lib.join("Caches/com.gone.app"), lib.join("Caches/com.link.app")).unwrap();
        let safety = Safety::for_home(&home);
        let entries = scan_all(&safety, &Procs::with(&[("com.running.app", "Running")]));
        let left: Vec<&AppEntry> = entries.iter().filter(|e| e.leftover).collect();
        assert_eq!(left.iter().map(|e| e.identity.name.as_str()).collect::<Vec<_>>(), ["com.gone.app"]);
        assert_eq!(entries.len(), 3, "the two installed apps and the one that is gone");
        let gone = left[0];
        let mut titles: Vec<&str> = gone.items.iter().map(|i| i.title.as_str()).collect();
        titles.sort_unstable();
        assert_eq!(titles, ["com.gone.app", "com.gone.app.plist", "com.gone.app.savedState"]);
        assert!(gone.items.iter().all(|i| i.kind == Kind::Apps && i.grant == Grant::AppData && !i.pre_selected));
        assert!(gone.items.iter().all(|i| i.owner.as_deref() == Some("com.gone.app") && i.source == "com.gone.app"));
        for item in &gone.items {
            assert!(safety.check_app(&item.path, item.grant, item.owner.as_deref()).is_ok(), "{} would be refused", item.path.display());
        }
        assert_eq!((gone.trace_count(), gone.trace_bytes()), (3, gone.bytes()), "all of it is traces: there is no bundle");
        assert_eq!(gone.identity.path, gone.bundle().path, "the biggest trace stands for it");
        assert!(gone.items.windows(2).all(|w| w[0].size >= w[1].size), "biggest first");
        let kept: Vec<&str> = gone.kept.iter().map(|k| k.path.parent().unwrap().file_name().unwrap().to_str().unwrap()).collect();
        assert_eq!(kept, ["Containers", "Application Support"], "what may be your documents stays, and says so");
        assert!(gone.notes.iter().any(|n| n.contains("not installed")) && !gone.running);
        assert_eq!(gone.keep_paths().len(), 3, "keeping it keeps every trace");
        let here = entries.iter().find(|e| e.identity.bundle_id == "com.vendor.here").unwrap();
        assert!(!here.leftover && here.items.len() == 2, "an installed app is what it was");
    }

    #[test]
    fn an_app_that_is_installed_somewhere_else_or_comes_back_before_the_scan_ends_is_not_a_leftover() {
        let home = tempdir();
        leave(&home.join("Library/Caches/com.vendor.back"), 500, true);
        let safety = Safety::for_home(&home);
        assert_eq!(scan_all(&safety, &Procs::none()).len(), 1, "gone");
        fake_app(&home.join("Applications"), "Back", "com.vendor.back", 10);
        let entries = scan_all(&safety, &Procs::none());
        assert!(entries.iter().all(|e| !e.leftover), "installed again: its cache is its own");
        assert!(count_installed(&installed_ids(&safety), "COM.VENDOR.BACK") == 1);
    }

    #[test]
    fn a_leftover_changed_lately_says_so_one_untouched_for_months_does_not_and_one_with_nothing_in_it_is_not_listed() {
        let home = tempdir();
        let lib = home.join("Library");
        leave(&lib.join("Caches/com.fresh.app"), 500, true);
        leave(&lib.join("Caches/com.stale.app"), 500, true);
        backdate(&lib.join("Caches/com.stale.app/data"));
        backdate(&lib.join("Caches/com.stale.app"));
        fs::create_dir_all(lib.join("Caches/com.empty.app")).unwrap();
        leave(&lib.join("Preferences/com.zero.app.plist"), 0, false);
        let entries = scan_all(&Safety::for_home(&home), &Procs::none());
        let names: Vec<&str> = entries.iter().map(|e| e.identity.name.as_str()).collect();
        assert_eq!(names, ["com.fresh.app", "com.stale.app"], "no bytes, nothing to remove");
        assert!(entries[0].cautions.iter().any(|c| c.contains("changed") && c.contains("may still use it")), "{:?}", entries[0].cautions);
        assert!(entries[1].cautions.is_empty());
        assert!(entries[1].items[0].evidence.iter().any(|e| e.text.starts_with("Last changed")));
    }

    #[test]
    fn a_leftover_too_small_to_be_worth_a_row_is_not_listed() {
        let home = tempdir();
        let lib = home.join("Library");
        leave(&lib.join("Caches/com.small.app"), 3_000, true);
        leave(&lib.join("Caches/com.big.app"), 3_000_000, true);
        let safety = Safety::for_home(&home);
        let found = |min_leftover| {
            let mut names = Vec::new();
            let options = AppsOptions { min_leftover, ..AppsOptions::default() };
            scan(&safety, &Procs::none(), &options, &AtomicBool::new(false), &mut |e| {
                if let AppsEvent::Found(entry) = e {
                    names.push(entry.identity.name.clone());
                }
            });
            names.sort_unstable();
            names
        };
        assert_eq!(found(0), ["com.big.app", "com.small.app"]);
        assert_eq!(found(MIN_LEFTOVER), ["com.big.app"]);
    }

    #[test]
    fn spotlight_does_not_know_an_invented_identifier() {
        assert!(!spotlight_has_app("com.begone-test.never-installed.zzz"));
    }

    #[test]
    fn a_spotlight_date_is_read_in_any_zone_and_nonsense_is_not() {
        let at = |s: &str| parse_mdls_date(s).map(|t| t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs());
        assert_eq!(at("1970-01-01 00:00:00 +0000"), Some(0));
        assert_eq!(at("2000-03-01 00:00:00 +0000"), Some(951_868_800));
        assert_eq!(at("2026-11-03 10:12:00 +0000"), Some(1_793_700_720));
        assert_eq!(at("2026-11-03 12:12:00 +0200"), Some(1_793_700_720));
        assert_eq!(at("2026-11-03 05:12:00 -0500"), Some(1_793_700_720));
        for bad in ["(null)", "", "2026-13-01 00:00:00 +0000", "2026-11-03", "x-y-z 00:00:00 +0000", "2026-11-03 25:00:00 +0000"] {
            assert_eq!(at(bad), None, "{bad:?}");
        }
    }
}
