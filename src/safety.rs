//! The safety layer. Nothing in this program touches a file that has not passed
//! through here, and the scan and the deleter both ask the same question, so a row
//! that can never be removed is never shown.
//!
//! Every path goes through a shared base check, then the gate its [`Grant`] names:
//!
//! * **Base:** the path must exist, must not be a symlink, must not be the home
//!   folder, and neither its spelling nor its real location may be system territory.
//! * **Allowlisted:** inside a small set of folders apps are required to be able to
//!   rebuild, and not on the never-delete list. The list is checked against the real
//!   location too, because a symlinked ancestor must not launder a protected path.
//! * **Proven artifact:** build output outside the allowlist, admitted one folder at a
//!   time because its name and a marker prove what it is. The proof is re-run when
//!   the item is removed, so a stale scan can't talk its way through.
//! * **Tool cache:** one of a short, exact list of folders that developer tools keep under the home folder
//!   (`~/.npm/_cacache`, `~/.cargo/registry/cache`, ...), or a folder directly inside `~/.cache`. Nothing beside
//!   them is admitted: `~/.cargo/bin` and `~/.cargo/credentials.toml` are not on the list.
//! * **Installer:** a regular file with an installer or disk-image extension, directly inside `~/Downloads`. The
//!   never-delete list protects Downloads for every other gate; this one is the narrow way in.
//! * **Big old download:** any finished file directly inside `~/Downloads` that is at least 100 MB and has not arrived or
//!   changed for three months. The size and the age are checked again at the moment it moves.
//! * **Device backup:** a folder directly inside `~/Library/Application Support/MobileSync/Backup` that holds the
//!   `Info.plist` of a device backup.
//! * **In the Trash:** a direct child of `~/.Trash`, already thrown away by the user.
//! * **App bundle:** a direct child `.app` of an apps folder whose own `Info.plist` names a
//!   non-Apple bundle identifier, or one of the few Apple apps people install themselves
//!   ([`REMOVABLE_APPLE_APPS`]), and which macOS itself does not protect. `/Applications` is system
//!   territory for every other gate; this one is the single, narrow way in, and it proves what it is
//!   from the bundle itself.
//! * **App data:** the caches, settings, saved state and web storage a bundle identifier names,
//!   and nothing else. The folders that may hold your own documents (`Containers`,
//!   `Application Support`) are never admitted: a removed app leaves your data where it is.
//!
//! The never-delete list beats the allowlist deliberately: the allowlist answers "may
//! I clean in here at all", the list answers "must this never be cleaned", and the
//! second question always wins.

use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, SystemTime};

use thiserror::Error;

/// Why a path was refused. Every variant is a sentence, because the UI shows the
/// refusal instead of quietly ignoring the request.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum Refusal {
    #[error("it is no longer there")]
    Missing,
    #[error("its location could not be resolved ({0})")]
    Unresolvable(String),
    #[error("{0} is protected system territory")]
    SystemTerritory(String),
    #[error("it is on the never-delete list ({0})")]
    Denied(&'static str),
    #[error("it is outside every folder begone is allowed to clean")]
    OutsideAllowlist,
    #[error("it is a symlink, and what it points at is not what was measured")]
    Symlink,
    #[error("it is your home folder")]
    HomeRoot,
    #[error("it no longer looks like build output ({0})")]
    NotAnArtifact(String),
    #[error("it is not directly inside the Trash")]
    NotInTrash,
    #[error("it is the folder itself, not something inside it")]
    IsRoot,
    #[error("it is not an app begone can remove ({0})")]
    NotAnApp(String),
    #[error("it is not one of the traces begone removes with an app ({0})")]
    NotAppData(String),
    #[error("it is not one of the tool caches begone removes ({0})")]
    NotAToolCache(String),
    #[error("it is not one of the files begone offers ({0})")]
    NotAPickedFile(String),
    #[error("it is not one of the developer tool stores begone lists ({0})")]
    NotADevStore(String),
    #[error("it is not a copy with an identical original that stays ({0})")]
    NotADuplicate(String),
    #[error("it is not one of the virtual machine or container stores begone lists ({0})")]
    NotAVm(String),
    #[error("you asked begone to keep it")]
    Kept,
}

/// The gate that admits an item. Scanners choose it; they cannot widen it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grant {
    Allowlisted,
    ProvenArtifact,
    InTrash,
    /// An installed app, removed as a whole bundle.
    AppBundle,
    /// A trace an app left in the user's Library, named by its bundle identifier.
    AppData,
    /// A cache a developer tool keeps under the home folder, admitted by its exact place.
    ToolCache,
    /// An installer or disk image sitting directly in Downloads.
    Installer,
    /// A big file that has sat untouched in Downloads for months, of any kind.
    BigDownload,
    /// An Xcode simulator or runtime. Never moved: only `simctl` deletes one, so the gate refuses every request to move it.
    Simulator,
    /// A backup of an iPhone or iPad, one folder of the device backups.
    DeviceBackup,
    /// A large thing a developer tool keeps that it can fetch or build again: a Rust toolchain, a Node version, pulled models.
    /// Admitted by its exact place, either the store folder itself or a direct child of one.
    DevStore,
    /// One copy of a file that is byte-for-byte identical to an original that is staying. Proven at the moment it moves by
    /// re-reading both and the original's path, carried as the item's owner.
    DuplicateCopy,
    /// A virtual machine or a container engine's disk image — a UTM, Parallels or VMware VM, Docker's or OrbStack's store.
    /// Admitted only at the exact place its app keeps it. Huge, and often the only copy of what is inside, so it is only listed.
    VmImage,
}

/// `(rule, reason)`. A rule matches when its components appear anywhere in the path,
/// so `/Documents` protects `/a/Documents` and everything beneath it, but not
/// `/a/DocumentsX`.
const DENIED: &[(&str, &str)] = &[
    ("/Documents", "your documents live here"),
    ("/Desktop", "your desktop lives here"),
    ("/Downloads", "your downloads live here"),
    ("/Pictures", "your pictures live here"),
    ("/Movies", "your movies live here"),
    ("/Music", "your music lives here"),
    ("/Library/Mobile Documents", "iCloud Drive: synced, not local junk"),
    ("/Library/CloudStorage", "cloud storage providers"),
    ("/Library/Keychains", "keychains: credentials are never disposable"),
    ("/.ssh", "SSH keys: credentials are never disposable"),
    ("/.gnupg", "GPG keys: credentials are never disposable"),
    ("/.aws", "cloud credentials are never disposable"),
    ("/.password-store", "your password store"),
    ("/Library/Application Support", "app data, not cache: removing it breaks apps"),
    ("/Library/Preferences", "app settings"),
    ("/Library/Messages", "message history"),
    ("/Library/Mail", "mail storage"),
    ("/Library/Safari", "browsing history and bookmarks"),
    ("/Library/Photos", "the photo library"),
    ("/Library/Developer/Xcode/Archives", "signed release builds: the only copy may be here"),
    ("/Library/Developer/Xcode/UserData", "your Xcode snippets and key bindings"),
];

/// A project can legitimately live under these, and its build output is still build
/// output. Only a proven artifact may look past them.
const PERSONAL: &[&str] = &["/Documents", "/Desktop"];

/// Absolute prefixes that are off limits for every gate and every user.
const SYSTEM_PREFIXES: &[&str] = &[
    "/System",
    "/usr",
    "/bin",
    "/sbin",
    "/private/etc",
    "/private/var/db",
    "/Library/Apple",
    "/Library/SystemExtensions",
    "/Applications",
    "/Volumes",
];

/// What makes a folder provably build output.
#[derive(Clone, Copy, Debug)]
pub enum Proof {
    /// One of these files sits beside the folder.
    Beside(&'static [&'static str]),
    /// One of these files sits inside the folder.
    Inside(&'static [&'static str]),
    /// The folder holds compiled Python bytecode and nothing else of note.
    Bytecode,
}

/// A kind of build output begone can recognise.
#[derive(Clone, Copy, Debug)]
pub struct ArtifactRule {
    pub name: &'static str,
    pub proof: Proof,
    /// Is the proof enough on its own? `dist` and `build` are generic names, so they
    /// also need git to confirm they are generated before they are chosen by default.
    pub strong: bool,
    pub what: &'static str,
    pub comes_back: &'static str,
    /// Files that let the tool restore exactly what was there.
    pub lockfiles: &'static [&'static str],
}

const BUILD: &[&str] = &["Cargo.toml", "pom.xml"];
const NODE: &[&str] = &["package.json"];
const VENV: &[&str] = &["pyvenv.cfg"];
const PY_LOCKS: &[&str] = &["uv.lock", "poetry.lock", "Pipfile.lock", "requirements.txt"];
const JS_LOCKS: &[&str] = &["package-lock.json", "pnpm-lock.yaml", "yarn.lock", "bun.lockb", "bun.lock"];
const GRADLE: &[&str] = &["build.gradle", "build.gradle.kts", "settings.gradle", "settings.gradle.kts"];
const CACHEDIR: &[&str] = &["CACHEDIR.TAG"];

pub const ARTIFACTS: &[ArtifactRule] = &[
    ArtifactRule {
        name: "target",
        proof: Proof::Beside(BUILD),
        strong: true,
        what: "Build output. Compiled code, not code you wrote.",
        comes_back: "Your build tool recreates it (cargo build, mvn package).",
        lockfiles: &["Cargo.lock"],
    },
    ArtifactRule {
        name: "node_modules",
        proof: Proof::Beside(NODE),
        strong: true,
        what: "Packages downloaded for this project. Not code you wrote.",
        comes_back: "Your package manager's install command brings it back.",
        lockfiles: JS_LOCKS,
    },
    ArtifactRule {
        name: ".venv",
        proof: Proof::Inside(VENV),
        strong: true,
        what: "A Python virtual environment for this project.",
        comes_back: "Recreate the environment and reinstall its packages.",
        lockfiles: PY_LOCKS,
    },
    ArtifactRule {
        name: "venv",
        proof: Proof::Inside(VENV),
        strong: true,
        what: "A Python virtual environment for this project.",
        comes_back: "Recreate the environment and reinstall its packages.",
        lockfiles: PY_LOCKS,
    },
    ArtifactRule {
        name: "__pycache__",
        proof: Proof::Bytecode,
        strong: true,
        what: "Compiled Python bytecode.",
        comes_back: "Python rewrites it the next time it imports the module.",
        lockfiles: &[],
    },
    ArtifactRule {
        name: ".next",
        proof: Proof::Beside(NODE),
        strong: true,
        what: "Next.js build output.",
        comes_back: "next build recreates it.",
        lockfiles: &[],
    },
    ArtifactRule {
        name: ".nuxt",
        proof: Proof::Beside(NODE),
        strong: true,
        what: "Nuxt build output.",
        comes_back: "nuxt build recreates it.",
        lockfiles: &[],
    },
    ArtifactRule {
        name: ".turbo",
        proof: Proof::Beside(NODE),
        strong: true,
        what: "Turborepo's local cache.",
        comes_back: "Turborepo refills it on your next build.",
        lockfiles: &[],
    },
    ArtifactRule {
        name: ".parcel-cache",
        proof: Proof::Beside(NODE),
        strong: true,
        what: "Parcel's build cache.",
        comes_back: "Parcel refills it on your next build.",
        lockfiles: &[],
    },
    ArtifactRule {
        name: "dist",
        proof: Proof::Beside(NODE),
        strong: false,
        what: "Output from your bundler.",
        comes_back: "Your build step recreates it.",
        lockfiles: &[],
    },
    ArtifactRule {
        name: "build",
        proof: Proof::Beside(GRADLE),
        strong: false,
        what: "Gradle build output.",
        comes_back: "gradle build recreates it.",
        lockfiles: &[],
    },
    ArtifactRule {
        name: ".gradle",
        proof: Proof::Beside(GRADLE),
        strong: true,
        what: "Gradle's project cache.",
        comes_back: "Gradle recreates it on your next build.",
        lockfiles: &[],
    },
    ArtifactRule {
        name: ".pytest_cache",
        proof: Proof::Inside(CACHEDIR),
        strong: true,
        what: "pytest's cache.",
        comes_back: "pytest recreates it on your next run.",
        lockfiles: &[],
    },
    ArtifactRule {
        name: ".mypy_cache",
        proof: Proof::Inside(CACHEDIR),
        strong: true,
        what: "mypy's cache.",
        comes_back: "mypy recreates it on your next run.",
        lockfiles: &[],
    },
    ArtifactRule {
        name: ".ruff_cache",
        proof: Proof::Inside(CACHEDIR),
        strong: true,
        what: "ruff's cache.",
        comes_back: "ruff recreates it on your next run.",
        lockfiles: &[],
    },
];

pub fn artifact_rule(name: &str) -> Option<&'static ArtifactRule> {
    ARTIFACTS.iter().find(|rule| rule.name == name)
}

/// A cache a developer tool keeps under the home folder, admitted by its exact place and nothing else.
#[derive(Clone, Copy, Debug)]
pub struct ToolCache {
    /// Relative to the home folder.
    pub path: &'static str,
    pub tool: &'static str,
    pub title: &'static str,
    pub what: &'static str,
    pub comes_back: &'static str,
    /// Does getting it back cost a download or a slow rebuild, and not just a moment?
    pub costs: bool,
}

/// Where tools keep what they can download or build again. Each entry is the folder itself, never its parent: `~/.cargo`
/// also holds `bin` (the tools you installed) and `credentials.toml`, and `~/.gradle` holds your settings, so neither is
/// ever admitted. Adding a folder here is a safety decision.
pub const TOOL_CACHES: &[ToolCache] = &[
    ToolCache {
        path: ".npm/_cacache",
        tool: "npm",
        title: "npm cache",
        what: "Packages npm downloaded, kept so installs are quick.",
        comes_back: "npm downloads them again the next time an install needs them.",
        costs: true,
    },
    ToolCache {
        path: ".npm/_npx",
        tool: "npm",
        title: "npx cache",
        what: "Packages npx fetched to run a command once.",
        comes_back: "npx fetches them again the next time you run that command.",
        costs: true,
    },
    ToolCache {
        path: "Library/pnpm/store",
        tool: "pnpm",
        title: "pnpm store",
        what: "pnpm's shared store of downloaded packages.",
        comes_back: "pnpm downloads them again. Projects already installed keep working.",
        costs: true,
    },
    ToolCache {
        path: ".cargo/registry/cache",
        tool: "Cargo",
        title: "Cargo downloads",
        what: "Crates Cargo downloaded from registries.",
        comes_back: "Cargo downloads them again the next time a build needs them, so that build needs the network.",
        costs: true,
    },
    ToolCache {
        path: ".cargo/registry/src",
        tool: "Cargo",
        title: "Cargo sources",
        what: "The unpacked source of downloaded crates.",
        comes_back: "Cargo unpacks them again from its downloads on the next build.",
        costs: false,
    },
    ToolCache {
        path: ".cargo/git/db",
        tool: "Cargo",
        title: "Cargo git clones",
        what: "Clones of the git repositories your projects depend on.",
        comes_back: "Cargo clones them again the next time a build needs them, so that build needs the network.",
        costs: true,
    },
    ToolCache {
        path: ".cargo/git/checkouts",
        tool: "Cargo",
        title: "Cargo git checkouts",
        what: "Working copies of git dependencies at the commits your projects use.",
        comes_back: "Cargo checks them out again from its clones on the next build.",
        costs: false,
    },
    ToolCache {
        path: ".gradle/caches",
        tool: "Gradle",
        title: "Gradle caches",
        what: "Dependencies and build results Gradle keeps across projects.",
        comes_back: "Gradle downloads and rebuilds them the next time a project needs them.",
        costs: true,
    },
    ToolCache {
        path: ".gradle/wrapper/dists",
        tool: "Gradle",
        title: "Gradle versions",
        what: "Copies of Gradle that projects' wrappers downloaded.",
        comes_back: "Each project's gradlew downloads the version it needs again.",
        costs: true,
    },
    ToolCache {
        path: ".bun/install/cache",
        tool: "Bun",
        title: "Bun cache",
        what: "Packages Bun downloaded.",
        comes_back: "Bun downloads them again the next time an install needs them.",
        costs: true,
    },
];

/// A place a developer tool keeps something large that it can fetch or build again — a Rust toolchain, a Node version, pulled
/// models — and that is yours to pick rather than junk begone cleans for you. Each entry names one folder under the home,
/// admitted either whole (`each_child` false) or one direct child at a time (`each_child` true). Like `TOOL_CACHES`, adding a
/// row here is a safety decision: the folder must hold only what its tool can bring back.
#[derive(Clone, Copy, Debug)]
pub struct DevStore {
    /// Relative to the home folder. For `each_child`, the parent whose direct children are offered; otherwise the folder itself.
    pub path: &'static str,
    pub each_child: bool,
    pub tool: &'static str,
    /// Shown for the whole-folder case; a child is shown by its own name.
    pub title: &'static str,
    pub what: &'static str,
    pub comes_back: &'static str,
}

pub const DEV_STORES: &[DevStore] = &[
    DevStore {
        path: ".rustup/toolchains",
        each_child: true,
        tool: "rustup",
        title: "Rust toolchain",
        what: "A Rust toolchain rustup installed: the compiler, standard library and tools for one release.",
        comes_back: "rustup installs it again with `rustup toolchain install`, a download.",
    },
    DevStore {
        path: ".rustup/downloads",
        each_child: false,
        tool: "rustup",
        title: "rustup downloads",
        what: "Files rustup downloaded while installing toolchains.",
        comes_back: "rustup downloads them again the next time it installs a toolchain.",
    },
    DevStore {
        path: ".nvm/versions/node",
        each_child: true,
        tool: "nvm",
        title: "Node version",
        what: "A version of Node.js that nvm installed.",
        comes_back: "nvm installs it again with `nvm install`, a download.",
    },
    DevStore {
        path: ".pyenv/versions",
        each_child: true,
        tool: "pyenv",
        title: "Python version",
        what: "A Python that pyenv built and installed.",
        comes_back: "pyenv builds it again with `pyenv install`, which can take a while.",
    },
    DevStore {
        path: ".ollama/models",
        each_child: false,
        tool: "Ollama",
        title: "Ollama models",
        what: "Models you pulled with Ollama.",
        comes_back: "Ollama pulls each one again the next time you run it, a large download.",
    },
];

/// The store a path belongs to, written relative to the home folder: the folder itself for a whole-folder store, or a direct
/// child of a per-child store's parent. Used by the gate to admit a path and by the scanner to label and explain it.
pub fn dev_store_meta(relative: &Path) -> Option<&'static DevStore> {
    DEV_STORES.iter().find(|store| {
        let store_path = Path::new(store.path);
        if store.each_child { relative.parent() == Some(store_path) && relative != store_path } else { relative == store_path }
    })
}

/// Where a virtual machine or a container engine keeps its (usually enormous) disk. Each is admitted only at this exact place:
/// a VM bundle of a given extension directly inside an app's folder (`each_child`), or one whole store folder or image file.
/// These are the user's own machines — often the only copy of what is inside — so they are only ever listed, never chosen, and
/// the warning says so. Adding a row here is a safety decision, like the other pick tables.
#[derive(Clone, Copy, Debug)]
pub struct VmStore {
    /// Relative to the home folder: for `each_child`, the parent folder scanned; otherwise the store itself.
    pub path: &'static str,
    pub each_child: bool,
    /// The extension that marks a VM bundle inside a per-child parent; empty for a whole-folder or single-file store.
    pub ext: &'static str,
    pub tool: &'static str,
    /// Shown for a whole-folder or single-file store; a bundle is shown by its own name.
    pub title: &'static str,
    pub what: &'static str,
    pub comes_back: &'static str,
    pub warn: &'static str,
}

pub const VM_STORES: &[VmStore] = &[
    VmStore {
        path: "Library/Containers/com.utmapp.UTM/Data/Documents",
        each_child: true,
        ext: "utm",
        tool: "UTM",
        title: "UTM virtual machine",
        what: "A virtual machine you created in UTM, with its disks and saved state.",
        comes_back: "It cannot be downloaded again: you would rebuild or reinstall the machine from scratch.",
        warn: "This is a whole virtual machine. If you still need what is inside it, do not remove it.",
    },
    VmStore {
        path: "Parallels",
        each_child: true,
        ext: "pvm",
        tool: "Parallels",
        title: "Parallels virtual machine",
        what: "A Parallels Desktop virtual machine, with its disks and saved state.",
        comes_back: "It cannot be downloaded again: you would rebuild or reinstall the machine from scratch.",
        warn: "This is a whole virtual machine. If you still need what is inside it, do not remove it.",
    },
    VmStore {
        path: "Virtual Machines.localized",
        each_child: true,
        ext: "vmwarevm",
        tool: "VMware Fusion",
        title: "VMware virtual machine",
        what: "A VMware Fusion virtual machine, with its disks and saved state.",
        comes_back: "It cannot be downloaded again: you would rebuild or reinstall the machine from scratch.",
        warn: "This is a whole virtual machine. If you still need what is inside it, do not remove it.",
    },
    VmStore {
        path: "Library/Containers/com.docker.docker/Data/vms/0/data/Docker.raw",
        each_child: false,
        ext: "",
        tool: "Docker",
        title: "Docker disk image",
        what: "Docker Desktop's disk: every image, container and volume you have built or pulled.",
        comes_back: "Docker makes an empty disk again on its next start; your images, containers and volumes do not come back.",
        warn: "This is all of Docker's data at once. Removing it resets Docker to empty.",
    },
    VmStore {
        path: ".orbstack/data",
        each_child: false,
        ext: "",
        tool: "OrbStack",
        title: "OrbStack data",
        what: "OrbStack's data: your containers, images and Linux machines.",
        comes_back: "OrbStack starts empty again; your containers, images and machines do not come back.",
        warn: "This is all of OrbStack's data at once. Removing it resets OrbStack to empty.",
    },
];

/// The virtual-machine store a path belongs to, written relative to the home folder: a bundle of the right extension directly
/// inside a per-child parent, or a whole-folder or single-file store. Used by the gate and by the scanner.
pub fn vm_store_for(relative: &Path) -> Option<&'static VmStore> {
    VM_STORES.iter().find(|store| {
        let store_path = Path::new(store.path);
        if store.each_child {
            relative.parent() == Some(store_path) && relative.extension().and_then(|e| e.to_str()) == Some(store.ext)
        } else {
            relative == store_path
        }
    })
}

/// What begone treats as an installer or a disk image when it sits directly in Downloads.
pub const INSTALLER_EXTENSIONS: &[&str] = &["dmg", "pkg", "mpkg", "iso", "xip"];

/// A download is a big old one when it is at least this big...
pub const BIG_DOWNLOAD_MIN_BYTES: u64 = 100_000_000;
/// ...and has neither arrived nor changed for this long.
pub const BIG_DOWNLOAD_MIN_AGE: Duration = Duration::from_secs(90 * 86_400);

/// What a browser leaves while a download is still going, or has given up on.
const PARTIAL_DOWNLOADS: &[&str] = &["crdownload", "part", "download", "opdownload", "partial", "tmp"];

/// Folders whose direct children are iPhone or iPad backups in the standard format, each with a device `Info.plist` at the top.
/// The first is Finder's and iTunes's own; the rest are third-party tools that keep backups in that same format in their own
/// place. A backup is recognised by that Info.plist, never by the folder's name, so only a real backup is ever offered, and
/// adding a row here only widens *where* begone looks, never *what* it will move.
pub const BACKUP_ROOTS: &[&str] =
    &["Library/Application Support/MobileSync/Backup", "Library/Application Support/app.reperio.recovery/iPhone backups"];

/// Does this folder hold the `Info.plist` of a device backup, naming the device it came from?
pub fn is_device_backup(folder: &Path) -> bool {
    let Ok(value) = plist::Value::from_file(folder.join("Info.plist")) else { return false };
    value.as_dictionary().is_some_and(|d| ["Device Name", "Product Type", "Unique Identifier"].iter().any(|k| d.contains_key(k)))
}

/// The folder directly inside `~/.cache` that a path names, relative to the home folder: by the XDG convention everything in
/// `~/.cache` is something its tool can make again.
pub const XDG_CACHE: &str = ".cache";

/// Is this place, written relative to the home folder, one of the tool caches begone removes?
pub fn is_tool_cache(relative: &Path) -> bool {
    if TOOL_CACHES.iter().any(|c| Path::new(c.path) == relative) {
        return true;
    }
    let mut parts = relative.components();
    match (parts.next(), parts.next(), parts.next()) {
        (Some(Component::Normal(top)), Some(Component::Normal(name)), None) => {
            let name = name.to_string_lossy();
            top == XDG_CACHE && !name.starts_with('.') && !name.to_lowercase().contains("begone")
        }
        _ => false,
    }
}

/// Check the proof for a folder. Returns a sentence the inspector can show.
pub fn prove_artifact(path: &Path) -> Result<String, String> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let Some(rule) = artifact_rule(name) else {
        return Err(format!("{name} is not a known kind of build output"));
    };
    let Some(parent) = path.parent() else {
        return Err("it has no parent folder".to_string());
    };
    let is_regular = |p: &Path| p.symlink_metadata().map(|m| m.is_file()).unwrap_or(false);
    match rule.proof {
        Proof::Beside(markers) => markers
            .iter()
            .find(|m| is_regular(&parent.join(m)))
            .map(|m| format!("{m} sits beside it"))
            .ok_or_else(|| format!("{} is gone", markers[0])),
        Proof::Inside(markers) => markers
            .iter()
            .find(|m| is_regular(&path.join(m)))
            .map(|m| format!("It holds a {m}, which confirms what it is"))
            .ok_or_else(|| format!("no {} inside", markers[0])),
        Proof::Bytecode => {
            let has_bytecode =
                std::fs::read_dir(path).map(|entries| entries.flatten().any(|e| e.path().extension().is_some_and(|x| x == "pyc")));
            match has_bytecode {
                Ok(true) => Ok("It holds compiled .pyc files".to_string()),
                _ => Err("no .pyc files inside".to_string()),
            }
        }
    }
}

/// The safety context for one run of the program.
#[derive(Clone, Debug)]
pub struct Safety {
    home: PathBuf,
    home_real: PathBuf,
    /// Allowlisted roots, in both spellings: as they are written and as they resolve.
    roots: Vec<PathBuf>,
    roots_real: Vec<PathBuf>,
    trash_real: PathBuf,
    /// The folders apps are installed in: `~/Applications` always, and `/Applications` too on the real machine.
    app_roots: Vec<PathBuf>,
    /// The places the person asked begone to keep. Shared by every copy, so a change made on the screen is seen by the workers.
    kept: Arc<RwLock<Vec<PathBuf>>>,
    /// How big and how long untouched a download must be to be offered: the constants, unless a test says otherwise.
    big_download: (u64, Duration),
    /// Set when begone was pointed at an external or secondary volume under `/Volumes` instead of the home folder: that one
    /// volume is operable, and the `/Volumes` guard is relaxed for paths inside it alone. Every other volume stays off limits.
    external_root: Option<PathBuf>,
}

impl Safety {
    /// Build the safety context for a home folder.
    pub fn for_home(home: &Path) -> Self {
        Self::with_roots(home, default_allowlist())
    }

    /// Build with an explicit allowlist, relative to `home`.
    pub fn with_roots(home: &Path, roots: &[&str]) -> Self {
        let home_real = home.canonicalize().unwrap_or_else(|_| lexical_normalize(home));
        let logical = lexical_normalize(home);
        // A scratch home (`--path`) has its own Applications folder and never sees the real one.
        let on_real_machine = home_dir().canonicalize().is_ok_and(|real| real == home_real);
        let mut app_roots = vec![home_real.join("Applications")];
        if on_real_machine {
            app_roots.push(PathBuf::from("/Applications"));
        }
        // Pointed at a volume under /Volumes (an external or secondary drive), not the home folder: that one volume becomes the
        // operable root. Never /Volumes itself, which is the mount point, not a drive.
        let external_root = (home_real.starts_with("/Volumes") && home_real != Path::new("/Volumes")).then(|| home_real.clone());
        Self {
            roots: roots.iter().map(|r| logical.join(r)).collect(),
            roots_real: roots.iter().map(|r| home_real.join(r)).collect(),
            trash_real: home_real.join(".Trash"),
            home: logical,
            home_real,
            app_roots,
            kept: Arc::new(RwLock::new(Vec::new())),
            big_download: (BIG_DOWNLOAD_MIN_BYTES, BIG_DOWNLOAD_MIN_AGE),
            external_root,
        }
    }

    /// Was begone pointed at an external or secondary volume rather than the home folder?
    pub fn is_external(&self) -> bool {
        self.external_root.is_some()
    }

    /// A system prefix that blocks this path, if any — honouring the one chosen external volume. Inside that volume `/Volumes`
    /// no longer blocks, but every other volume, and all of `/System`, `/usr` and the rest, still do.
    fn system_block(&self, path: &Path) -> Option<&'static str> {
        let prefix = system_prefix(path)?;
        if prefix == "/Volumes" && self.external_root.as_deref().is_some_and(|root| path.starts_with(root)) {
            return None;
        }
        Some(prefix)
    }

    /// The size and age a download has to have to be offered as a big old one.
    pub fn big_download_rules(&self) -> (u64, Duration) {
        self.big_download
    }

    /// The folders begone offers big, old files from: the home folder itself (loose files dropped at the top) and the two
    /// staging folders where big downloads and transfers pile up. The media and document libraries are deliberately left out:
    /// a big, old file there is organised, not clutter, so those are kept whole and only ever shown on the map.
    pub fn big_file_roots(&self) -> Vec<PathBuf> {
        if self.is_external() {
            // On a volume there are no Downloads or Desktop; big files dropped at its top are what this catches.
            return vec![self.home_real.clone()];
        }
        vec![self.home_real.clone(), self.home_real.join("Downloads"), self.home_real.join("Desktop")]
    }

    /// The folders device backups are looked for in, as real paths: Finder's own and the third-party tools that use the same
    /// format. Only a direct child that still carries a device `Info.plist` is ever a backup.
    pub fn backup_roots(&self) -> Vec<PathBuf> {
        BACKUP_ROOTS.iter().map(|r| self.home_real.join(r)).collect()
    }

    /// Override the big-file size and age thresholds (from the config file; tests use small files and short ages).
    pub fn with_big_download(mut self, min_bytes: u64, min_age: Duration) -> Self {
        self.big_download = (min_bytes, min_age);
        self
    }

    /// The folders apps are installed in, as real paths.
    pub fn app_roots(&self) -> &[PathBuf] {
        &self.app_roots
    }

    /// The places begone has been told to keep.
    pub fn kept(&self) -> Vec<PathBuf> {
        self.kept.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    pub fn set_kept(&self, paths: Vec<PathBuf>) {
        *self.kept.write().unwrap_or_else(PoisonError::into_inner) = paths.iter().map(|p| lexical_normalize(p)).collect();
    }

    /// Is this a kept place, inside one, or does it hold one? Moving a folder moves what is in it, so all three keep it.
    pub fn is_kept(&self, path: &Path) -> bool {
        let kept = self.kept.read().unwrap_or_else(PoisonError::into_inner);
        if kept.is_empty() {
            return false;
        }
        let overlaps = |a: &Path, b: &Path| a.starts_with(b) || b.starts_with(a);
        let logical = lexical_normalize(path);
        let real = path.canonicalize().ok();
        kept.iter().any(|k| {
            let k_real = k.canonicalize().ok();
            overlaps(&logical, k)
                || real.as_deref().is_some_and(|r| overlaps(r, k) || k_real.as_deref().is_some_and(|kr| overlaps(r, kr)))
                || k_real.as_deref().is_some_and(|kr| overlaps(&logical, kr))
        })
    }

    /// The question asked at the moment of a move: the gate for the grant, and then whether the person said to keep it.
    pub fn check_move(&self, path: &Path, grant: Grant, owner: Option<&str>) -> Result<PathBuf, Refusal> {
        let real = self.check_app(path, grant, owner)?;
        if grant != Grant::InTrash && (self.is_kept(path) || self.is_kept(&real)) {
            return Err(Refusal::Kept);
        }
        Ok(real)
    }

    /// The home folder, as it really resolves.
    pub fn home_real(&self) -> &Path {
        &self.home_real
    }

    fn is_app_root(&self, dir: &Path) -> bool {
        let Ok(real) = dir.canonicalize() else { return false };
        self.app_roots.iter().any(|r| r.canonicalize().is_ok_and(|root| root == real))
    }

    /// The real location of the user's Trash.
    pub fn trash_dir(&self) -> &Path {
        &self.trash_real
    }

    /// Is removing `path` permitted under `grant`? Returns the real location to act on.
    pub fn check(&self, path: &Path, grant: Grant) -> Result<PathBuf, Refusal> {
        if grant == Grant::InTrash {
            return self.check_in_trash(path);
        }
        if grant == Grant::Simulator {
            return Err(Refusal::Denied("simulators are deleted through Xcode's own tool, never moved"));
        }
        if grant == Grant::DuplicateCopy {
            // A copy is only ever safe to remove against a named original, so it has to come through check_move with its owner.
            return Err(Refusal::NotADuplicate("it can only be checked against the original it copies".to_string()));
        }
        if grant == Grant::VmImage {
            return self.gate_vm(path);
        }
        if matches!(grant, Grant::AppBundle | Grant::AppData) {
            return self.check_app(path, grant, None);
        }
        if matches!(grant, Grant::Installer | Grant::BigDownload | Grant::DeviceBackup) {
            return self.gate_picked(path, grant);
        }
        let (logical, real) = self.base(path)?;
        match grant {
            Grant::Allowlisted => self.gate_allowlisted(&logical, real),
            Grant::ToolCache => self.gate_tool_cache(&logical, real),
            Grant::DevStore => self.gate_dev_store(&logical, real),
            _ => self.gate_artifact(&logical, real),
        }
    }

    /// The same question for an item that may belong to an app, which is told whose it is.
    pub fn check_app(&self, path: &Path, grant: Grant, owner: Option<&str>) -> Result<PathBuf, Refusal> {
        match grant {
            Grant::AppBundle => self.gate_app_bundle(path, owner),
            Grant::AppData => self.gate_app_data(path, owner),
            Grant::DuplicateCopy => self.gate_duplicate(path, owner),
            other => self.check(path, other),
        }
    }

    fn gate_app_bundle(&self, path: &Path, owner: Option<&str>) -> Result<PathBuf, Refusal> {
        let meta = path.symlink_metadata().map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => Refusal::Missing,
            _ => Refusal::Unresolvable(err.to_string()),
        })?;
        if meta.file_type().is_symlink() {
            return Err(Refusal::Symlink);
        }
        if !meta.is_dir() {
            return Err(Refusal::NotAnApp("it is not a folder".to_string()));
        }
        let name = path.file_name().and_then(|n| n.to_str()).ok_or_else(|| Refusal::NotAnApp("its name is not text".to_string()))?;
        if !name.ends_with(".app") || name.len() <= ".app".len() {
            return Err(Refusal::NotAnApp("its name does not end in .app".to_string()));
        }
        // It must be a direct child of an apps folder: not nested, not reached through a symlinked folder.
        let parent = path.parent().ok_or(Refusal::OutsideAllowlist)?;
        if !self.is_app_root(parent) {
            return Err(Refusal::OutsideAllowlist);
        }
        let real = parent.canonicalize().map_err(|err| Refusal::Unresolvable(err.to_string()))?.join(name);
        let id = bundle_identifier(&real).ok_or_else(|| Refusal::NotAnApp("no bundle identifier in its Info.plist".to_string()))?;
        if is_apple_id(&id) && !is_removable_apple_app(&id) {
            return Err(Refusal::Denied("Apple's own apps are not removed"));
        }
        if protected_by_macos(&meta) {
            return Err(Refusal::Denied("macOS protects it (System Integrity Protection)"));
        }
        if let Some(owner) = owner
            && owner != id
        {
            return Err(Refusal::NotAnApp("its identifier is not the one it had when it was scanned".to_string()));
        }
        Ok(real)
    }

    fn gate_app_data(&self, path: &Path, owner: Option<&str>) -> Result<PathBuf, Refusal> {
        let id = owner.ok_or_else(|| Refusal::NotAppData("it has no owner".to_string()))?;
        if !well_formed_id(id) || is_apple_id(id) {
            return Err(Refusal::NotAppData("its owner is not a third-party bundle identifier".to_string()));
        }
        let meta = path.symlink_metadata().map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => Refusal::Missing,
            _ => Refusal::Unresolvable(err.to_string()),
        })?;
        if meta.file_type().is_symlink() {
            return Err(Refusal::Symlink);
        }
        let name = path.file_name().and_then(|n| n.to_str()).ok_or_else(|| Refusal::NotAppData("its name is not text".to_string()))?;
        let parent = path.parent().ok_or(Refusal::OutsideAllowlist)?;
        let parent_real = parent.canonicalize().map_err(|err| Refusal::Unresolvable(err.to_string()))?;
        let library = self.home_real.join("Library");
        // Each folder holds these names for this owner, as a folder (true) or a file (false).
        let names: Vec<(String, bool)> = if parent_real == library.join("Caches") {
            vec![(id.to_string(), true)]
        } else if parent_real == library.join("Preferences") {
            vec![(format!("{id}.plist"), false)]
        } else if parent_real == library.join("Saved Application State") {
            vec![(format!("{id}.savedState"), true)]
        } else if parent_real == library.join("HTTPStorages") {
            vec![(id.to_string(), true), (format!("{id}.binarycookies"), false)]
        } else if parent_real == library.join("WebKit") {
            vec![(id.to_string(), true)]
        } else if parent_real == library.join("Cookies") {
            vec![(format!("{id}.binarycookies"), false)]
        } else {
            return Err(Refusal::NotAppData("it is not in a folder of caches, settings or saved state".to_string()));
        };
        let Some((_, is_folder)) = names.iter().find(|(n, _)| n == name) else {
            return Err(Refusal::NotAppData(format!("its name is not {}", names[0].0)));
        };
        let is_folder = *is_folder;
        // Folders are removed as folders and files as files; anything else is not what was measured.
        if is_folder != meta.is_dir() || !is_folder && !meta.is_file() {
            return Err(Refusal::NotAppData("it is not the kind of thing that folder holds".to_string()));
        }
        Ok(parent_real.join(name))
    }

    /// May something be put back at `to`? It must land inside the home folder, not in
    /// system territory, and not be the home folder itself.
    pub fn check_restore_target(&self, to: &Path) -> Result<(), Refusal> {
        let logical = lexical_normalize(to);
        // An app bundle goes back into an apps folder, and only a bundle into only that folder.
        if let (Some(parent), true) = (logical.parent(), logical.extension().is_some_and(|e| e == "app"))
            && self.app_roots.iter().any(|root| root == parent)
        {
            return Ok(());
        }
        if logical == self.home || logical == self.home_real {
            return Err(Refusal::HomeRoot);
        }
        if !(logical.starts_with(&self.home) || logical.starts_with(&self.home_real)) {
            return Err(Refusal::OutsideAllowlist);
        }
        match self.system_block(&logical) {
            Some(prefix) => Err(Refusal::SystemTerritory(prefix.to_string())),
            None => Ok(()),
        }
    }

    fn base(&self, path: &Path) -> Result<(PathBuf, PathBuf), Refusal> {
        // A symlink is refused before anything resolves it.
        let meta = path.symlink_metadata().map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => Refusal::Missing,
            _ => Refusal::Unresolvable(err.to_string()),
        })?;
        if meta.file_type().is_symlink() {
            return Err(Refusal::Symlink);
        }
        let logical = lexical_normalize(path);
        if logical == self.home || logical == self.home_real {
            return Err(Refusal::HomeRoot);
        }
        if let Some(prefix) = self.system_block(&logical) {
            return Err(Refusal::SystemTerritory(prefix.to_string()));
        }
        let real = path.canonicalize().map_err(|err| Refusal::Unresolvable(err.to_string()))?;
        if real == self.home_real {
            return Err(Refusal::HomeRoot);
        }
        if let Some(prefix) = self.system_block(&real) {
            return Err(Refusal::SystemTerritory(prefix.to_string()));
        }
        Ok((logical, real))
    }

    fn gate_allowlisted(&self, logical: &Path, real: PathBuf) -> Result<PathBuf, Refusal> {
        // Both spellings face the list: a symlinked ancestor must not launder a
        // protected location into an allowed-looking one.
        for candidate in [logical, real.as_path()] {
            if let Some(reason) = denied(candidate, false) {
                return Err(Refusal::Denied(reason));
            }
        }
        if self.roots_real.contains(&real) {
            return Err(Refusal::IsRoot);
        }
        let written_inside = self.roots.iter().chain(&self.roots_real).any(|r| logical.starts_with(r));
        let really_inside = self.roots_real.iter().any(|r| real.starts_with(r));
        if written_inside && really_inside { Ok(real) } else { Err(Refusal::OutsideAllowlist) }
    }

    fn gate_artifact(&self, logical: &Path, real: PathBuf) -> Result<PathBuf, Refusal> {
        // It must sit where it claims to, with no symlinked ancestor in between.
        let relative = logical.strip_prefix(&self.home).or_else(|_| logical.strip_prefix(&self.home_real));
        let Ok(relative) = relative else {
            return Err(Refusal::OutsideAllowlist);
        };
        if real != self.home_real.join(relative) {
            return Err(Refusal::OutsideAllowlist);
        }
        if real.parent() == Some(self.home_real.as_path()) {
            return Err(Refusal::NotAnArtifact("it sits directly in your home folder".to_string()));
        }
        if let Some(reason) = denied(&real, true) {
            return Err(Refusal::Denied(reason));
        }
        if !real.is_dir() {
            return Err(Refusal::NotAnArtifact("it is no longer a folder".to_string()));
        }
        prove_artifact(&real).map_err(Refusal::NotAnArtifact)?;
        Ok(real)
    }

    /// The files begone only offers for you to choose: an installer in Downloads, a device backup. Each is admitted by where it
    /// sits and what it is, and only as itself.
    fn gate_picked(&self, path: &Path, grant: Grant) -> Result<PathBuf, Refusal> {
        let meta = path.symlink_metadata().map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => Refusal::Missing,
            _ => Refusal::Unresolvable(err.to_string()),
        })?;
        if meta.file_type().is_symlink() {
            return Err(Refusal::Symlink);
        }
        let name = path.file_name().and_then(|n| n.to_str()).ok_or_else(|| Refusal::NotAPickedFile("its name is not text".to_string()))?;
        let parent = path.parent().ok_or(Refusal::OutsideAllowlist)?;
        let parent_real = parent.canonicalize().map_err(|err| Refusal::Unresolvable(err.to_string()))?;
        // The real folder, not one reached through a link: a Downloads that points somewhere else is not the one this is about.
        // A big file may sit in any of the staging folders where big downloads and transfers pile up; the rest are kept whole.
        let here = match grant {
            Grant::Installer => parent_real == self.home_real.join("Downloads"),
            Grant::BigDownload => self.big_file_roots().contains(&parent_real),
            _ => self.backup_roots().contains(&parent_real),
        };
        if !here {
            let what = match grant {
                Grant::Installer => "an installer",
                Grant::BigDownload => "a big file",
                _ => "a device backup",
            };
            return Err(Refusal::NotAPickedFile(format!("it is not directly inside a folder {what} is looked for in")));
        }
        if grant == Grant::Installer {
            let extension = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
            if !meta.is_file() || !INSTALLER_EXTENSIONS.contains(&extension.as_str()) {
                return Err(Refusal::NotAPickedFile("it is not an installer or disk image".to_string()));
            }
        } else if grant == Grant::BigDownload {
            let extension = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
            if !meta.is_file() || name.starts_with('.') || PARTIAL_DOWNLOADS.contains(&extension.as_str()) {
                return Err(Refusal::NotAPickedFile("it is not a finished download".to_string()));
            }
            let (min_bytes, min_age) = self.big_download;
            if meta.len() < min_bytes {
                return Err(Refusal::NotAPickedFile("it is smaller than a big download has to be".to_string()));
            }
            // When it last changed or arrived, whichever is later: a file downloaded yesterday with an old date on it is new.
            let newest = [meta.modified().ok(), meta.created().ok()].into_iter().flatten().max();
            if newest.and_then(|t| SystemTime::now().duration_since(t).ok()).is_none_or(|idle| idle < min_age) {
                return Err(Refusal::NotAPickedFile("it arrived or changed too recently".to_string()));
            }
        } else if !meta.is_dir() || !is_device_backup(path) {
            return Err(Refusal::NotAPickedFile("it is not a folder with a device's Info.plist in it".to_string()));
        }
        Ok(parent_real.join(name))
    }

    fn gate_tool_cache(&self, logical: &Path, real: PathBuf) -> Result<PathBuf, Refusal> {
        // It must sit exactly where it claims to, with no symlinked ancestor in between: a `~/.cargo` that points
        // somewhere else is not the folder this list is about.
        let relative = logical.strip_prefix(&self.home).or_else(|_| logical.strip_prefix(&self.home_real));
        let Ok(relative) = relative else {
            return Err(Refusal::OutsideAllowlist);
        };
        if real != self.home_real.join(relative) {
            return Err(Refusal::OutsideAllowlist);
        }
        for candidate in [logical, real.as_path()] {
            if let Some(reason) = denied(candidate, false) {
                return Err(Refusal::Denied(reason));
            }
        }
        if !is_tool_cache(relative) {
            return Err(Refusal::OutsideAllowlist);
        }
        if !real.is_dir() {
            return Err(Refusal::NotAToolCache("it is no longer a folder".to_string()));
        }
        Ok(real)
    }

    /// A developer tool's store: one of the places on `DEV_STORES`, admitted only where it claims to sit, with no symlinked
    /// ancestor, exactly as the tool-cache gate does. Whole-folder stores and one direct child of a per-child store both pass.
    fn gate_dev_store(&self, logical: &Path, real: PathBuf) -> Result<PathBuf, Refusal> {
        let relative = logical.strip_prefix(&self.home).or_else(|_| logical.strip_prefix(&self.home_real));
        let Ok(relative) = relative else {
            return Err(Refusal::OutsideAllowlist);
        };
        if real != self.home_real.join(relative) {
            return Err(Refusal::OutsideAllowlist);
        }
        for candidate in [logical, real.as_path()] {
            if let Some(reason) = denied(candidate, false) {
                return Err(Refusal::Denied(reason));
            }
        }
        if dev_store_meta(relative).is_none() {
            return Err(Refusal::NotADevStore("it is not one of the developer tool stores".to_string()));
        }
        if !real.is_dir() {
            return Err(Refusal::NotADevStore("it is no longer a folder".to_string()));
        }
        Ok(real)
    }

    /// One copy of a file, safe to remove only because an identical original is staying. The original's path rides along as the
    /// item's owner; the gate re-reads both here, at the moment of the move, so a copy that has since changed, or whose original
    /// is gone, is refused. Every removable row is a copy, never the original, so the last copy is never the one that moves.
    fn gate_duplicate(&self, path: &Path, owner: Option<&str>) -> Result<PathBuf, Refusal> {
        let keeper = owner.ok_or_else(|| Refusal::NotADuplicate("it has no original to compare against".to_string()))?;
        let keeper = Path::new(keeper);
        let meta = path.symlink_metadata().map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => Refusal::Missing,
            _ => Refusal::Unresolvable(err.to_string()),
        })?;
        if meta.file_type().is_symlink() {
            return Err(Refusal::Symlink);
        }
        if !meta.is_file() {
            return Err(Refusal::NotADuplicate("it is not a regular file".to_string()));
        }
        let logical = lexical_normalize(path);
        if let Some(prefix) = self.system_block(&logical) {
            return Err(Refusal::SystemTerritory(prefix.to_string()));
        }
        let real = path.canonicalize().map_err(|err| Refusal::Unresolvable(err.to_string()))?;
        if let Some(prefix) = self.system_block(&real) {
            return Err(Refusal::SystemTerritory(prefix.to_string()));
        }
        // The original has to be there, a real file, and a different file: a copy and its original are never the same bytes on
        // disk (same inode), or removing the "copy" would take the original with it.
        let keeper_meta = keeper.symlink_metadata().map_err(|_| Refusal::NotADuplicate("the original it copies is gone".to_string()))?;
        if keeper_meta.file_type().is_symlink() || !keeper_meta.is_file() {
            return Err(Refusal::NotADuplicate("the original it copies is not a regular file".to_string()));
        }
        let keeper_real = keeper.canonicalize().map_err(|_| Refusal::NotADuplicate("the original it copies is gone".to_string()))?;
        if identity_of(&real) == identity_of(&keeper_real) {
            return Err(Refusal::NotADuplicate("it is the same file as the original, not a separate copy".to_string()));
        }
        match same_contents(&real, &keeper_real) {
            Ok(true) => Ok(real),
            Ok(false) => Err(Refusal::NotADuplicate("it no longer matches the original byte for byte".to_string())),
            Err(err) => Err(Refusal::Unresolvable(err.to_string())),
        }
    }

    /// A virtual machine or container store: one of the places on `VM_STORES`, admitted only where it claims to sit, with no
    /// symlinked ancestor, exactly as the other home-relative gates. A per-child bundle must be a folder of the right extension.
    fn gate_vm(&self, path: &Path) -> Result<PathBuf, Refusal> {
        let meta = path.symlink_metadata().map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => Refusal::Missing,
            _ => Refusal::Unresolvable(err.to_string()),
        })?;
        if meta.file_type().is_symlink() {
            return Err(Refusal::Symlink);
        }
        let logical = lexical_normalize(path);
        let relative = logical.strip_prefix(&self.home).or_else(|_| logical.strip_prefix(&self.home_real));
        let Ok(relative) = relative else {
            return Err(Refusal::OutsideAllowlist);
        };
        let Some(store) = vm_store_for(relative) else {
            return Err(Refusal::NotAVm("it is not one of the virtual machine or container stores".to_string()));
        };
        let real = path.canonicalize().map_err(|err| Refusal::Unresolvable(err.to_string()))?;
        if real != self.home_real.join(relative) {
            return Err(Refusal::OutsideAllowlist);
        }
        if store.each_child && !real.is_dir() {
            return Err(Refusal::NotAVm("it is not the virtual machine bundle it was".to_string()));
        }
        Ok(real)
    }

    /// The Trash holds things the user already threw away, so a symlink there is removed as
    /// the link itself, never followed. The only rule is: a direct child of the real Trash.
    fn check_in_trash(&self, path: &Path) -> Result<PathBuf, Refusal> {
        path.symlink_metadata().map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => Refusal::Missing,
            _ => Refusal::Unresolvable(err.to_string()),
        })?;
        let (Some(name), Some(parent)) = (path.file_name(), path.parent()) else {
            return Err(Refusal::NotInTrash);
        };
        let parent_real = parent.canonicalize().map_err(|err| Refusal::Unresolvable(err.to_string()))?;
        if parent_real == self.trash_real { Ok(parent_real.join(name)) } else { Err(Refusal::NotInTrash) }
    }
}

/// The folders begone may clean inside. Deliberately short: each entry is a place apps
/// and tools must be able to rebuild from nothing. The Trash has its own gate.
pub fn default_allowlist() -> &'static [&'static str] {
    &[
        "Library/Caches",
        "Library/Logs",
        "Library/Developer/Xcode/DerivedData",
        "Library/Developer/Xcode/iOS DeviceSupport",
        "Library/Developer/Xcode/watchOS DeviceSupport",
        "Library/Developer/Xcode/tvOS DeviceSupport",
        "Library/Developer/CoreSimulator/Caches",
    ]
}

/// The Apple apps begone will remove: ones people install themselves and can download again. Every other Apple identifier
/// stays protected. Only the bundle goes, never its caches or settings (macOS's own developer tools share those), and adding an
/// app here is a safety decision, not a convenience.
pub const REMOVABLE_APPLE_APPS: &[&str] = &["com.apple.dt.Xcode"];

pub fn is_removable_apple_app(id: &str) -> bool {
    REMOVABLE_APPLE_APPS.iter().any(|app| app.eq_ignore_ascii_case(id))
}

/// `SF_RESTRICTED`: System Integrity Protection guards this item, so nothing may remove it.
const SF_RESTRICTED: u32 = 0x0008_0000;

fn has_restricted_flag(flags: u32) -> bool {
    flags & SF_RESTRICTED != 0
}

#[cfg(target_os = "macos")]
fn protected_by_macos(meta: &std::fs::Metadata) -> bool {
    use std::os::macos::fs::MetadataExt;
    has_restricted_flag(meta.st_flags())
}

#[cfg(not(target_os = "macos"))]
fn protected_by_macos(_: &std::fs::Metadata) -> bool {
    false
}

/// Is this identifier written the way bundle identifiers are: letters, digits, `.`, `-`, `_`, with at least one dot?
pub fn well_formed_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 255
        && id.contains('.')
        && !id.starts_with('.')
        && !id.ends_with('.')
        && !id.contains("..")
        && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// Apple's own identifiers: their apps and their data are never ours to remove.
pub fn is_apple_id(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    id == "com.apple" || id.starts_with("com.apple.")
}

/// The `CFBundleIdentifier` in an app bundle's Info.plist, if it has one.
pub fn bundle_identifier(bundle: &Path) -> Option<String> {
    let value = plist::Value::from_file(bundle.join("Contents/Info.plist")).ok()?;
    let id = value.as_dictionary()?.get("CFBundleIdentifier")?.as_string()?.to_string();
    well_formed_id(&id).then_some(id)
}

/// The user's home folder, without pulling in a dependency for it.
pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/Users/unknown"), PathBuf::from)
}

fn system_prefix(path: &Path) -> Option<&'static str> {
    SYSTEM_PREFIXES.iter().find(|p| path.starts_with(p)).copied()
}

fn denied(path: &Path, proven_artifact: bool) -> Option<&'static str> {
    DENIED
        .iter()
        .filter(|(rule, _)| !(proven_artifact && PERSONAL.contains(rule)))
        .find(|(rule, _)| matches_deny_rule(path, rule))
        .map(|(_, reason)| *reason)
}

/// Collapse `.` and `..` without touching the filesystem, so nothing is followed.
/// A `..` that would climb above the root is dropped.
pub fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out: Vec<Component<'_>> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.last() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => out.push(component),
            },
            other => out.push(other),
        }
    }
    let mut buf = PathBuf::new();
    for component in out {
        buf.push(component.as_os_str());
    }
    if buf.as_os_str().is_empty() { PathBuf::from(".") } else { buf }
}

/// Does a rule's run of components appear anywhere in the path?
fn matches_deny_rule(path: &Path, rule: &str) -> bool {
    let want: Vec<Component<'_>> = Path::new(rule).components().filter(|c| matches!(c, Component::Normal(_))).collect();
    let have: Vec<Component<'_>> = path.components().filter(|c| matches!(c, Component::Normal(_))).collect();
    !want.is_empty() && want.len() <= have.len() && have.windows(want.len()).any(|w| w == want.as_slice())
}

/// A file's identity on disk: its device and inode. A rename keeps both; a separate copy shares neither.
fn identity_of(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(path).ok()?;
    Some((m.dev(), m.ino()))
}

/// Do two files hold exactly the same bytes? Streams both in lockstep and stops at the first difference, so it never reads more
/// than it must. Callers pass real regular files; nothing here follows a link.
pub fn same_contents(a: &Path, b: &Path) -> std::io::Result<bool> {
    let mut fa = std::fs::File::open(a)?;
    let mut fb = std::fs::File::open(b)?;
    if fa.metadata()?.len() != fb.metadata()?.len() {
        return Ok(false);
    }
    let mut ba = [0u8; 65_536];
    let mut bb = [0u8; 65_536];
    loop {
        let na = fill(&mut fa, &mut ba)?;
        let nb = fill(&mut fb, &mut bb)?;
        if na != nb {
            return Ok(false);
        }
        if na == 0 {
            return Ok(true);
        }
        if ba[..na] != bb[..nb] {
            return Ok(false);
        }
    }
}

/// Read until the buffer is full or the file ends, so a short read never makes two equal files look different.
fn fill(file: &mut std::fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    use std::io::Read;
    let mut n = 0;
    while n < buf.len() {
        match file.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::fs;

    /// A unique temp directory per test, removed on drop, with the real path.
    pub struct TempDir(PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    impl std::ops::Deref for TempDir {
        type Target = PathBuf;
        fn deref(&self) -> &PathBuf {
            &self.0
        }
    }
    pub fn tempdir() -> TempDir {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "begone-test-{}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir.canonicalize().unwrap())
    }
    fn write(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"x").unwrap();
    }

    #[test]
    fn allows_inside_the_allowlist() {
        let home = tempdir();
        let target = home.join("Library/Caches/com.example.app");
        fs::create_dir_all(&target).unwrap();
        let safety = Safety::for_home(&home);
        assert_eq!(safety.check(&target, Grant::Allowlisted).unwrap(), target);
    }

    #[test]
    fn refuses_outside_the_allowlist() {
        let home = tempdir();
        let target = home.join("projects/thing/stuff");
        fs::create_dir_all(&target).unwrap();
        let safety = Safety::for_home(&home);
        assert_eq!(safety.check(&target, Grant::Allowlisted), Err(Refusal::OutsideAllowlist));
    }

    #[test]
    fn a_missing_path_is_reported_as_missing() {
        let home = tempdir();
        fs::create_dir_all(home.join("Library/Caches")).unwrap();
        let safety = Safety::for_home(&home);
        assert_eq!(safety.check(&home.join("Library/Caches/ghost"), Grant::Allowlisted), Err(Refusal::Missing));
    }

    #[test]
    fn the_never_delete_list_beats_the_allowlist() {
        let home = tempdir();
        let safety = Safety::with_roots(&home, &["."]);
        for rel in ["Documents", "a/b/Documents", "Library/Messages", "Library/Application Support/MyApp"] {
            let path = home.join(rel);
            fs::create_dir_all(&path).unwrap();
            assert!(matches!(safety.check(&path, Grant::Allowlisted), Err(Refusal::Denied(_))), "{rel}");
        }
    }

    #[test]
    fn a_rule_matches_whole_components_only() {
        assert!(matches_deny_rule(Path::new("/Users/x/Documents/receipts"), "/Documents"));
        assert!(matches_deny_rule(Path::new("/Users/x/Library/Application Support/MyApp"), "/Library/Application Support"));
        assert!(!matches_deny_rule(Path::new("/Users/x/DocumentsX/file"), "/Documents"));
        assert!(!matches_deny_rule(Path::new("/Users/x/Library/Application SupportX"), "/Library/Application Support"));
    }

    #[test]
    fn refuses_symlinks_before_resolving_them() {
        let home = tempdir();
        let real = home.join("Library/Caches/real");
        fs::create_dir_all(&real).unwrap();
        let link = home.join("Library/Caches/link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let safety = Safety::for_home(&home);
        assert_eq!(safety.check(&link, Grant::Allowlisted), Err(Refusal::Symlink));
    }

    #[test]
    fn a_symlinked_ancestor_cannot_launder_a_protected_path() {
        // Regression: the list was only checked against the path as written, so
        // ~/Library/Logs/link/thesis.txt passed even though link -> .../Documents.
        let home = tempdir();
        let safety = Safety::with_roots(&home, &["Library/Logs", "Library/Containers"]);
        let docs = home.join("Library/Containers/com.x/Data/Documents");
        write(&docs.join("thesis.txt"));
        fs::create_dir_all(home.join("Library/Logs")).unwrap();
        std::os::unix::fs::symlink(&docs, home.join("Library/Logs/link")).unwrap();
        assert!(matches!(safety.check(&home.join("Library/Logs/link/thesis.txt"), Grant::Allowlisted), Err(Refusal::Denied(_))));
    }

    #[test]
    fn a_symlinked_ancestor_cannot_leave_the_allowlist() {
        let home = tempdir();
        let outside = home.join("elsewhere");
        write(&outside.join("file"));
        fs::create_dir_all(home.join("Library/Caches")).unwrap();
        std::os::unix::fs::symlink(&outside, home.join("Library/Caches/link")).unwrap();
        let safety = Safety::for_home(&home);
        assert_eq!(safety.check(&home.join("Library/Caches/link/file"), Grant::Allowlisted), Err(Refusal::OutsideAllowlist));
    }

    #[test]
    fn refuses_system_territory_under_every_gate() {
        let safety = Safety::for_home(Path::new("/Users/nobody"));
        for grant in [Grant::Allowlisted, Grant::ProvenArtifact] {
            assert!(matches!(safety.check(Path::new("/System/Library/CoreServices"), grant), Err(Refusal::SystemTerritory(_))));
        }
        assert!(matches!(safety.check(Path::new("/Applications"), Grant::Allowlisted), Err(Refusal::SystemTerritory(_))));
        assert_eq!(safety.check(Path::new("/System/Library/CoreServices"), Grant::InTrash), Err(Refusal::NotInTrash));
    }

    #[test]
    fn a_symlink_in_the_trash_can_go_but_a_dot_dot_cannot_sneak_out() {
        let home = tempdir();
        write(&home.join("Documents/keep.txt"));
        fs::create_dir_all(home.join(".Trash")).unwrap();
        std::os::unix::fs::symlink(home.join("Documents"), home.join(".Trash/link")).unwrap();
        let safety = Safety::for_home(&home);
        assert_eq!(safety.check(&home.join(".Trash/link"), Grant::InTrash).unwrap(), home.join(".Trash/link"));
        assert_eq!(
            safety.check(&home.join(".Trash/link/keep.txt"), Grant::InTrash),
            Err(Refusal::NotInTrash),
            "through the link is outside the Trash"
        );
        assert_eq!(safety.check(&home.join(".Trash/.."), Grant::InTrash), Err(Refusal::NotInTrash));
    }

    #[test]
    fn restore_targets_must_land_inside_home() {
        let home = tempdir();
        let safety = Safety::for_home(&home);
        assert!(safety.check_restore_target(&home.join("Library/Caches/x")).is_ok());
        assert_eq!(safety.check_restore_target(&home), Err(Refusal::HomeRoot));
        assert_eq!(safety.check_restore_target(Path::new("/etc/passwd")), Err(Refusal::OutsideAllowlist));
        assert!(safety.check_restore_target(&home.join("../../System/Library/x")).is_err());
    }

    #[test]
    fn refuses_the_home_folder() {
        let home = tempdir();
        let safety = Safety::with_roots(&home, &["."]);
        assert_eq!(safety.check(&home, Grant::Allowlisted), Err(Refusal::HomeRoot));
    }

    #[test]
    fn traversal_is_judged_by_where_it_really_goes() {
        let home = tempdir();
        fs::create_dir_all(home.join("Documents")).unwrap();
        fs::create_dir_all(home.join("Library/Caches")).unwrap();
        let safety = Safety::for_home(&home);
        let sneaky = home.join("Library/Caches/../../Documents");
        assert!(matches!(safety.check(&sneaky, Grant::Allowlisted), Err(Refusal::Denied(_))));
    }

    #[test]
    fn a_non_canonical_home_still_works() {
        // On macOS /tmp is a symlink to /private/tmp, and HOME may be spelled either way.
        let real = tempdir();
        let alias = std::env::temp_dir().join(format!("begone-alias-{}", std::process::id()));
        let _ = fs::remove_file(&alias);
        std::os::unix::fs::symlink(&*real, &alias).unwrap();
        fs::create_dir_all(real.join("Library/Caches/x")).unwrap();
        let safety = Safety::for_home(&alias);
        assert!(safety.check(&alias.join("Library/Caches/x"), Grant::Allowlisted).is_ok());
        fs::remove_file(&alias).unwrap();
    }

    #[test]
    fn a_proven_artifact_passes_where_the_allowlist_would_not() {
        let home = tempdir();
        let target = home.join("code/app/target");
        fs::create_dir_all(&target).unwrap();
        write(&home.join("code/app/Cargo.toml"));
        let safety = Safety::for_home(&home);
        assert_eq!(safety.check(&target, Grant::Allowlisted), Err(Refusal::OutsideAllowlist));
        assert_eq!(safety.check(&target, Grant::ProvenArtifact).unwrap(), target);
    }

    #[test]
    fn the_proof_is_rerun_every_time() {
        let home = tempdir();
        let target = home.join("code/app/target");
        fs::create_dir_all(&target).unwrap();
        let marker = home.join("code/app/Cargo.toml");
        write(&marker);
        let safety = Safety::for_home(&home);
        assert!(safety.check(&target, Grant::ProvenArtifact).is_ok());
        fs::remove_file(&marker).unwrap();
        assert!(matches!(safety.check(&target, Grant::ProvenArtifact), Err(Refusal::NotAnArtifact(_))));
    }

    #[test]
    fn a_folder_merely_called_target_is_not_proven() {
        let home = tempdir();
        let fake = home.join("code/notes/target");
        fs::create_dir_all(&fake).unwrap();
        assert!(matches!(Safety::for_home(&home).check(&fake, Grant::ProvenArtifact), Err(Refusal::NotAnArtifact(_))));
    }

    #[test]
    fn an_artifact_grant_cannot_reach_arbitrary_folders() {
        let home = tempdir();
        let notes = home.join("code/app/src");
        fs::create_dir_all(&notes).unwrap();
        write(&home.join("code/app/Cargo.toml"));
        assert!(matches!(Safety::for_home(&home).check(&notes, Grant::ProvenArtifact), Err(Refusal::NotAnArtifact(_))));
    }

    #[test]
    fn a_project_on_the_desktop_can_lose_its_build_output_but_nothing_else() {
        let home = tempdir();
        let target = home.join("Desktop/app/target");
        let src = home.join("Desktop/app/src");
        fs::create_dir_all(&target).unwrap();
        fs::create_dir_all(&src).unwrap();
        write(&home.join("Desktop/app/Cargo.toml"));
        let safety = Safety::for_home(&home);
        assert!(safety.check(&target, Grant::ProvenArtifact).is_ok());
        assert!(matches!(safety.check(&src, Grant::ProvenArtifact), Err(Refusal::NotAnArtifact(_))));
        assert!(matches!(safety.check(&src, Grant::Allowlisted), Err(Refusal::Denied(_))));
    }

    #[test]
    fn credentials_stay_off_limits_even_for_a_proven_artifact() {
        let home = tempdir();
        let target = home.join(".ssh/app/target");
        fs::create_dir_all(&target).unwrap();
        write(&home.join(".ssh/app/Cargo.toml"));
        assert!(matches!(Safety::for_home(&home).check(&target, Grant::ProvenArtifact), Err(Refusal::Denied(_))));
    }

    #[test]
    fn a_symlinked_ancestor_cannot_fake_a_proven_artifact() {
        let home = tempdir();
        let real = home.join("code/app");
        fs::create_dir_all(real.join("target")).unwrap();
        write(&real.join("Cargo.toml"));
        std::os::unix::fs::symlink(&real, home.join("code/alias")).unwrap();
        assert_eq!(Safety::for_home(&home).check(&home.join("code/alias/target"), Grant::ProvenArtifact), Err(Refusal::OutsideAllowlist));
    }

    #[test]
    fn the_trash_gate_admits_direct_children_only() {
        let home = tempdir();
        let item = home.join(".Trash/old.pdf");
        write(&item);
        write(&home.join(".Trash/folder/nested.txt"));
        write(&home.join("Library/Caches/x"));
        let safety = Safety::for_home(&home);
        assert_eq!(safety.check(&item, Grant::InTrash).unwrap(), item);
        assert_eq!(safety.check(&home.join(".Trash/folder/nested.txt"), Grant::InTrash), Err(Refusal::NotInTrash));
        assert_eq!(safety.check(&home.join("Library/Caches/x"), Grant::InTrash), Err(Refusal::NotInTrash));
    }

    #[test]
    fn a_thrown_away_folder_named_documents_is_still_trash() {
        let home = tempdir();
        let item = home.join(".Trash/Documents");
        fs::create_dir_all(&item).unwrap();
        assert!(Safety::for_home(&home).check(&item, Grant::InTrash).is_ok());
    }

    #[test]
    fn the_allowlisted_folder_itself_is_never_a_target() {
        let home = tempdir();
        let caches = home.join("Library/Caches");
        fs::create_dir_all(caches.join("x")).unwrap();
        let safety = Safety::for_home(&home);
        assert_eq!(safety.check(&caches, Grant::Allowlisted), Err(Refusal::IsRoot));
        assert!(safety.check(&caches.join("x"), Grant::Allowlisted).is_ok());
    }

    #[test]
    fn xcode_archives_and_user_data_are_out_of_reach() {
        let home = tempdir();
        let safety = Safety::for_home(&home);
        for rel in [
            "Library/Developer/Xcode/Archives/2025-01-01",
            "Library/Developer/Xcode/UserData/KeyBindings",
            "Library/Developer/CoreSimulator/Devices/ABC",
        ] {
            let path = home.join(rel);
            fs::create_dir_all(&path).unwrap();
            assert!(safety.check(&path, Grant::Allowlisted).is_err(), "{rel}");
        }
        let derived = home.join("Library/Developer/Xcode/DerivedData/App-abc");
        fs::create_dir_all(&derived).unwrap();
        assert!(safety.check(&derived, Grant::Allowlisted).is_ok());
    }

    #[test]
    fn a_virtualenv_is_proven_by_its_own_marker() {
        let home = tempdir();
        let venv = home.join("code/app/.venv");
        fs::create_dir_all(&venv).unwrap();
        let safety = Safety::for_home(&home);
        assert!(matches!(safety.check(&venv, Grant::ProvenArtifact), Err(Refusal::NotAnArtifact(_))));
        write(&venv.join("pyvenv.cfg"));
        assert!(safety.check(&venv, Grant::ProvenArtifact).is_ok());
    }

    #[test]
    fn every_rule_has_a_reason_and_every_artifact_explains_itself() {
        for (rule, reason) in DENIED {
            assert!(!reason.is_empty() && rule.starts_with('/'), "{rule}");
        }
        for rule in ARTIFACTS {
            assert!(!rule.what.is_empty() && !rule.comes_back.is_empty(), "{}", rule.name);
        }
    }

    // ------------------------------------------------------------------------------ apps

    /// A fake installed app: `<apps_dir>/<name>.app` with an Info.plist naming `id`, and an executable of `bytes`.
    pub fn fake_app(apps_dir: &Path, name: &str, id: &str, bytes: usize) -> PathBuf {
        let bundle = apps_dir.join(format!("{name}.app"));
        fs::create_dir_all(bundle.join("Contents/MacOS")).unwrap();
        fs::write(
            bundle.join("Contents/Info.plist"),
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>CFBundleIdentifier</key><string>{id}</string><key>CFBundleName</key><string>{name}</string><key>CFBundleShortVersionString</key><string>1.2.3</string></dict></plist>\n"
            ),
        )
        .unwrap();
        fs::write(bundle.join(format!("Contents/MacOS/{name}")), vec![7u8; bytes]).unwrap();
        bundle
    }

    /// Something an app left behind, of `bytes`, at `path` (a file, or a folder holding one file).
    pub fn leave(path: &Path, bytes: usize, folder: bool) {
        if folder {
            fs::create_dir_all(path).unwrap();
            fs::write(path.join("data"), vec![3u8; bytes]).unwrap();
        } else {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, vec![3u8; bytes]).unwrap();
        }
    }

    #[test]
    fn an_app_bundle_is_admitted_as_a_direct_child_of_an_apps_folder_by_its_own_identifier() {
        let home = tempdir();
        let safety = Safety::for_home(&home);
        let bundle = fake_app(&home.join("Applications"), "Slack", "com.tinyspeck.slackmacgap", 100);
        assert_eq!(safety.check_app(&bundle, Grant::AppBundle, Some("com.tinyspeck.slackmacgap")), Ok(bundle.clone()));
        assert_eq!(safety.check(&bundle, Grant::AppBundle), Ok(bundle.clone()), "the owner is optional for a bundle: it says who it is");
        assert_eq!(safety.app_roots(), [home.join("Applications")], "a scratch home never sees the real /Applications");
    }

    #[test]
    fn a_path_that_is_not_an_installed_third_party_bundle_is_refused() {
        let home = tempdir();
        let safety = Safety::for_home(&home);
        let apps = home.join("Applications");
        let good = fake_app(&apps, "Good", "com.vendor.good", 10);
        // Nested one level down.
        let nested = fake_app(&apps.join("Suite"), "Inner", "com.vendor.inner", 10);
        assert_eq!(safety.check(&nested, Grant::AppBundle), Err(Refusal::OutsideAllowlist));
        // Not in an apps folder at all.
        let elsewhere = fake_app(&home.join("code"), "Thing", "com.vendor.thing", 10);
        assert_eq!(safety.check(&elsewhere, Grant::AppBundle), Err(Refusal::OutsideAllowlist));
        // Apple's own.
        let apple = fake_app(&apps, "Pages", "com.apple.iWork.Pages", 10);
        assert!(matches!(safety.check(&apple, Grant::AppBundle), Err(Refusal::Denied(_))));
        // A symlink named like an app.
        std::os::unix::fs::symlink(&good, apps.join("Link.app")).unwrap();
        assert_eq!(safety.check(&apps.join("Link.app"), Grant::AppBundle), Err(Refusal::Symlink));
        // A file, a folder that is not an .app, and an .app with no usable Info.plist.
        write(&apps.join("File.app"));
        assert!(matches!(safety.check(&apps.join("File.app"), Grant::AppBundle), Err(Refusal::NotAnApp(_))));
        fs::create_dir_all(apps.join("Plain")).unwrap();
        assert!(matches!(safety.check(&apps.join("Plain"), Grant::AppBundle), Err(Refusal::NotAnApp(_))));
        fs::create_dir_all(apps.join("Empty.app/Contents")).unwrap();
        assert!(matches!(safety.check(&apps.join("Empty.app"), Grant::AppBundle), Err(Refusal::NotAnApp(_))));
        let badid = fake_app(&apps, "Bad", "not an id", 10);
        assert!(matches!(safety.check(&badid, Grant::AppBundle), Err(Refusal::NotAnApp(_))));
        // The apps folder itself, and a missing one.
        assert!(matches!(safety.check(&apps, Grant::AppBundle), Err(Refusal::NotAnApp(_))));
        assert_eq!(safety.check(&apps.join("Gone.app"), Grant::AppBundle), Err(Refusal::Missing));
        // What was scanned as one app must still be that app.
        assert!(matches!(safety.check_app(&good, Grant::AppBundle, Some("com.vendor.other")), Err(Refusal::NotAnApp(_))));
        assert!(safety.check_app(&good, Grant::AppBundle, Some("com.vendor.good")).is_ok());
    }

    #[test]
    fn app_data_is_admitted_only_by_exact_identifier_in_the_folders_that_hold_traces() {
        let home = tempdir();
        let safety = Safety::for_home(&home);
        let lib = home.join("Library");
        let id = "com.vendor.app";
        let traces = [
            (lib.join("Caches").join(id), true),
            (lib.join("Preferences").join(format!("{id}.plist")), false),
            (lib.join("Saved Application State").join(format!("{id}.savedState")), true),
            (lib.join("HTTPStorages").join(id), true),
            (lib.join("HTTPStorages").join(format!("{id}.binarycookies")), false),
            (lib.join("WebKit").join(id), true),
            (lib.join("Cookies").join(format!("{id}.binarycookies")), false),
        ];
        for (path, folder) in &traces {
            leave(path, 10, *folder);
            assert_eq!(safety.check_app(path, Grant::AppData, Some(id)), Ok(path.clone()), "{}", path.display());
        }
        // Somebody else's, or nearly the same name, or the wrong shape, or no owner at all.
        let other = lib.join("Caches/com.vendor.app2");
        leave(&other, 10, true);
        assert!(matches!(safety.check_app(&other, Grant::AppData, Some(id)), Err(Refusal::NotAppData(_))));
        let longer = lib.join("Caches/com.vendor.app.helper");
        leave(&longer, 10, true);
        assert!(matches!(safety.check_app(&longer, Grant::AppData, Some(id)), Err(Refusal::NotAppData(_))), "a prefix is not a match");
        let wrong_shape = lib.join("Preferences/com.vendor.app.plist.bak");
        leave(&wrong_shape, 10, false);
        assert!(matches!(safety.check_app(&wrong_shape, Grant::AppData, Some(id)), Err(Refusal::NotAppData(_))));
        assert!(matches!(safety.check_app(&traces[0].0, Grant::AppData, None), Err(Refusal::NotAppData(_))));
        assert!(matches!(safety.check(&traces[0].0, Grant::AppData), Err(Refusal::NotAppData(_))));
        // A folder where a plist file should be, and a file where a folder should be.
        fs::remove_file(&traces[1].0).unwrap();
        fs::create_dir_all(&traces[1].0).unwrap();
        assert!(matches!(safety.check_app(&traces[1].0, Grant::AppData, Some(id)), Err(Refusal::NotAppData(_))));
        fs::remove_dir_all(&traces[0].0).unwrap();
        write(&traces[0].0);
        assert!(matches!(safety.check_app(&traces[0].0, Grant::AppData, Some(id)), Err(Refusal::NotAppData(_))));
        // Inside one of them is not the entry itself.
        assert!(matches!(safety.check_app(&traces[2].0.join("data"), Grant::AppData, Some(id)), Err(Refusal::NotAppData(_))));
        // A symlink named exactly right is still a symlink.
        fs::remove_dir_all(&traces[5].0).unwrap();
        std::os::unix::fs::symlink(home.join("Documents"), &traces[5].0).unwrap();
        assert_eq!(safety.check_app(&traces[5].0, Grant::AppData, Some(id)), Err(Refusal::Symlink));
        assert_eq!(safety.check_app(&lib.join("Caches/com.vendor.gone"), Grant::AppData, Some("com.vendor.gone")), Err(Refusal::Missing));
    }

    #[test]
    fn what_may_hold_your_own_documents_is_never_admitted_and_neither_is_apples_data() {
        let home = tempdir();
        let safety = Safety::for_home(&home);
        let lib = home.join("Library");
        let id = "com.vendor.app";
        for path in [
            lib.join("Containers").join(id),
            lib.join("Application Support").join(id),
            lib.join("Group Containers").join(id),
            lib.join("Logs").join(id),
            lib.join("Mail").join(id),
            home.join("Documents").join(id),
            home.join(id),
        ] {
            leave(&path, 10, true);
            assert!(matches!(safety.check_app(&path, Grant::AppData, Some(id)), Err(Refusal::NotAppData(_))), "{}", path.display());
        }
        let apple = lib.join("Caches/com.apple.Safari");
        leave(&apple, 10, true);
        assert!(matches!(safety.check_app(&apple, Grant::AppData, Some("com.apple.Safari")), Err(Refusal::NotAppData(_))));
        for bad in ["", "plain", "com..x", ".com.x", "com.x.", "com.x/y", "com.x y", "../com.x"] {
            assert!(!well_formed_id(bad), "{bad:?}");
        }
        assert!(well_formed_id("com.vendor.App-2_x") && is_apple_id("COM.APPLE.dt.Xcode") && !is_apple_id("com.appleseed.app"));
    }

    #[test]
    fn an_app_goes_back_only_into_an_apps_folder_and_only_as_a_bundle() {
        let home = tempdir();
        let safety = Safety::for_home(&home);
        assert!(safety.check_restore_target(&home.join("Applications/Slack.app")).is_ok());
        assert!(safety.check_restore_target(&home.join("Library/Caches/com.vendor.app")).is_ok());
        // On the real machine /Applications is an apps folder, and nothing else under it is a target.
        let real = Safety::for_home(&home_dir());
        assert!(real.check_restore_target(Path::new("/Applications/Slack.app")).is_ok());
        for refused in [
            "/Applications/Slack",
            "/Applications/Suite/Slack.app",
            "/Applications",
            "/System/Applications/Slack.app",
            "/usr/local/Slack.app",
        ] {
            assert!(real.check_restore_target(Path::new(refused)).is_err(), "{refused}");
        }
        // A scratch home does not get the real /Applications.
        assert!(safety.check_restore_target(Path::new("/Applications/Slack.app")).is_err());
    }

    #[test]
    fn xcode_is_admitted_by_its_exact_identifier_and_no_other_apple_app_is() {
        let home = tempdir();
        let safety = Safety::for_home(&home);
        let apps = home.join("Applications");
        let xcode = fake_app(&apps, "Xcode", "com.apple.dt.Xcode", 10);
        assert_eq!(safety.check(&xcode, Grant::AppBundle), Ok(xcode.clone()));
        assert_eq!(safety.check_app(&xcode, Grant::AppBundle, Some("com.apple.dt.Xcode")), Ok(xcode.clone()));
        // Another copy, as developers keep several, under a different name but the same identifier.
        let beta = fake_app(&apps, "Xcode-beta", "com.apple.dt.Xcode", 10);
        assert!(safety.check(&beta, Grant::AppBundle).is_ok());
        // Everything else from Apple stays protected, including things that merely start the same way.
        for (name, id) in [
            ("Pages", "com.apple.iWork.Pages"),
            ("Safari", "com.apple.Safari"),
            ("Helper", "com.apple.dt.Xcode.helper"),
            ("Almost", "com.apple.dt.XcodeX"),
            ("Bare", "com.apple"),
            ("Dt", "com.apple.dt"),
        ] {
            let bundle = fake_app(&apps, name, id, 10);
            assert!(matches!(safety.check(&bundle, Grant::AppBundle), Err(Refusal::Denied(_))), "{id} must stay protected");
        }
        // The identifier is what counts, not the folder's name: a third party's app called Xcode is simply a third party's app.
        let mine = fake_app(&apps, "Xcode Clone", "com.vendor.xcode", 10);
        assert!(safety.check(&mine, Grant::AppBundle).is_ok());
        assert!(
            is_removable_apple_app("COM.APPLE.DT.XCODE")
                && !is_removable_apple_app("com.apple.dt.Xcode.x")
                && REMOVABLE_APPLE_APPS.len() == 1
        );
    }

    #[test]
    fn xcodes_caches_and_settings_are_still_never_admitted_as_app_data() {
        let home = tempdir();
        let safety = Safety::for_home(&home);
        let lib = home.join("Library");
        for (path, folder) in [
            (lib.join("Caches/com.apple.dt.Xcode"), true),
            (lib.join("Preferences/com.apple.dt.Xcode.plist"), false),
            (lib.join("Saved Application State/com.apple.dt.Xcode.savedState"), true),
        ] {
            leave(&path, 10, folder);
            assert!(
                matches!(safety.check_app(&path, Grant::AppData, Some("com.apple.dt.Xcode")), Err(Refusal::NotAppData(_))),
                "{}",
                path.display()
            );
        }
    }

    #[test]
    fn a_bundle_macos_protects_is_refused_whoever_made_it() {
        assert!(has_restricted_flag(0x0008_0000) && has_restricted_flag(0x0008_0002));
        assert!(!has_restricted_flag(0) && !has_restricted_flag(0x0000_0002), "the user-immutable flag is not System Integrity Protection");
        // An ordinary bundle is not protected.
        let home = tempdir();
        let bundle = fake_app(&home.join("Applications"), "Plain", "com.vendor.plain", 10);
        assert!(!protected_by_macos(&bundle.symlink_metadata().unwrap()));
    }

    // ------------------------------------------------------------------------------ kept places

    #[test]
    fn a_kept_place_still_passes_the_gate_so_it_can_be_shown_but_is_refused_when_it_would_move() {
        let home = tempdir();
        let safety = Safety::for_home(&home);
        let (one, two) = (home.join("Library/Caches/com.example.one"), home.join("Library/Caches/com.example.two"));
        leave(&one, 10, true);
        leave(&two, 10, true);
        assert_eq!(safety.check_move(&one, Grant::Allowlisted, None), Ok(one.clone()), "nothing is kept yet");
        safety.set_kept(vec![one.clone()]);
        assert!(safety.check(&one, Grant::Allowlisted).is_ok(), "the gate answers what may be cleaned, not what you want kept");
        assert_eq!(safety.check_move(&one, Grant::Allowlisted, None), Err(Refusal::Kept));
        assert!(safety.check_move(&two, Grant::Allowlisted, None).is_ok(), "only that place");
        // Inside a kept place, and a place that holds one: moving a folder moves what is in it.
        safety.set_kept(vec![one.join("data")]);
        assert_eq!(safety.check_move(&one, Grant::Allowlisted, None), Err(Refusal::Kept));
        safety.set_kept(vec![home.join("Library/Caches")]);
        assert_eq!(safety.check_move(&two, Grant::Allowlisted, None), Err(Refusal::Kept));
        // A different spelling of the same place is the same place.
        let link = home.join("link");
        std::os::unix::fs::symlink(&one, &link).unwrap();
        safety.set_kept(vec![link]);
        assert_eq!(safety.check_move(&one, Grant::Allowlisted, None), Err(Refusal::Kept));
        // A neighbour with a similar name is not inside it.
        safety.set_kept(vec![home.join("Library/Caches/com.example.o")]);
        assert!(safety.check_move(&one, Grant::Allowlisted, None).is_ok());
        // The Trash is already thrown away: keeping does not apply, and nothing kept means nothing is asked.
        let thrown = home.join(".Trash/old");
        leave(&thrown, 10, true);
        safety.set_kept(vec![home.join(".Trash")]);
        assert!(safety.check_move(&thrown, Grant::InTrash, None).is_ok());
        safety.set_kept(Vec::new());
        assert!(!safety.is_kept(&one));
        // An app bundle is kept like anything else.
        let bundle = fake_app(&home.join("Applications"), "Keeper", "com.vendor.keeper", 10);
        safety.set_kept(vec![bundle.clone()]);
        assert_eq!(safety.check_move(&bundle, Grant::AppBundle, Some("com.vendor.keeper")), Err(Refusal::Kept));
    }

    #[test]
    fn the_keep_list_is_shared_by_every_copy_of_the_safety_layer() {
        let home = tempdir();
        let safety = Safety::for_home(&home);
        let copy = safety.clone();
        safety.set_kept(vec![home.join("a/../b")]);
        assert_eq!(copy.kept(), [home.join("b")], "written the way it is compared");
        assert!(copy.is_kept(&home.join("b/c")));
    }

    // ------------------------------------------------------------------------------ files you choose yourself

    const BACKUPS: &str = "Library/Application Support/MobileSync/Backup";

    fn make_backup(home: &Path, udid: &str) -> PathBuf {
        let dir = home.join(BACKUPS).join(udid);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("Info.plist"),
            "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>Device Name</key><string>Abhi's iPhone</string><key>Product Type</key><string>iPhone15,2</string></dict></plist>",
        )
        .unwrap();
        fs::write(dir.join("Manifest.db"), vec![1u8; 100]).unwrap();
        dir
    }

    #[test]
    fn an_installer_is_admitted_only_as_a_file_with_the_right_extension_directly_in_downloads() {
        let home = tempdir();
        let safety = Safety::for_home(&home);
        for name in ["Slack.dmg", "Tool.PKG", "ubuntu.iso", "Xcode_16.xip", "multi.mpkg"] {
            let path = home.join("Downloads").join(name);
            leave(&path, 10, false);
            assert_eq!(safety.check(&path, Grant::Installer), Ok(path.clone()), "{name}");
        }
        for rel in [
            "Downloads/notes.txt",
            "Downloads/thesis.pdf",
            "Downloads/Archive.zip",
            "Downloads/sub/Slack.dmg",
            "Documents/Slack.dmg",
            "Desktop/Slack.dmg",
            "Slack.dmg",
        ] {
            let path = home.join(rel);
            leave(&path, 10, false);
            assert!(matches!(safety.check(&path, Grant::Installer), Err(Refusal::NotAPickedFile(_))), "{rel} must not be admitted");
        }
        // A folder with an installer's name, a link, the folder itself, and what is gone.
        fs::create_dir_all(home.join("Downloads/Folder.dmg")).unwrap();
        assert!(matches!(safety.check(&home.join("Downloads/Folder.dmg"), Grant::Installer), Err(Refusal::NotAPickedFile(_))));
        std::os::unix::fs::symlink(home.join("Documents/Slack.dmg"), home.join("Downloads/link.dmg")).unwrap();
        assert_eq!(safety.check(&home.join("Downloads/link.dmg"), Grant::Installer), Err(Refusal::Symlink));
        assert!(safety.check(&home.join("Downloads"), Grant::Installer).is_err());
        assert_eq!(safety.check(&home.join("Downloads/gone.dmg"), Grant::Installer), Err(Refusal::Missing));
        // The other grants still keep Downloads out of reach.
        assert!(safety.check(&home.join("Downloads/Slack.dmg"), Grant::Allowlisted).is_err());
        // A Downloads that is a link to somewhere else is not the folder this is about.
        let other = tempdir();
        let moved = other.join("Downloads");
        leave(&moved.join("Slack.dmg"), 10, false);
        std::os::unix::fs::symlink(&moved, other.join("redirected")).unwrap();
        let elsewhere = Safety::for_home(&other);
        assert_eq!(elsewhere.check(&moved.join("Slack.dmg"), Grant::Installer), Ok(moved.join("Slack.dmg")), "an ordinary Downloads");
        let linked = tempdir();
        std::os::unix::fs::symlink(&moved, linked.join("Downloads")).unwrap();
        assert!(Safety::for_home(&linked).check(&linked.join("Downloads/Slack.dmg"), Grant::Installer).is_err());
    }

    fn old(path: &Path) {
        assert!(std::process::Command::new("touch").args(["-t", "202501010000"]).arg(path).status().unwrap().success());
    }

    #[test]
    fn a_big_old_file_is_admitted_as_a_finished_file_in_a_staging_folder_with_the_size_and_age_checked_now() {
        let home = tempdir();
        let safety = Safety::for_home(&home).with_big_download(1_000, Duration::from_secs(3_600));
        let big = |rel: &str| {
            let path = home.join(rel);
            leave(&path, 5_000, false);
            old(&path);
            path
        };
        // Admitted: a finished file in Downloads, on the Desktop, or loose at the top of the home folder.
        for rel in ["Downloads/movie.mov", "Downloads/archive.ZIP", "Downloads/no-extension", "Desktop/movie.mov", "loose.mov"] {
            let path = big(rel);
            assert_eq!(safety.check(&path, Grant::BigDownload), Ok(path.clone()), "{rel}");
        }
        // Not admitted: nested inside a staging folder, in a library kept whole, hidden, unfinished, a folder, a link, gone.
        for rel in [
            "Downloads/sub/movie.mov",
            "Desktop/sub/movie.mov",
            "Documents/movie.mov",
            "Movies/movie.mov",
            "Pictures/movie.mov",
            "Music/movie.mov",
            "Downloads/.hidden",
            "Downloads/x.crdownload",
            "Downloads/x.part",
        ] {
            let path = big(rel);
            assert!(matches!(safety.check(&path, Grant::BigDownload), Err(Refusal::NotAPickedFile(_))), "{rel} must not be admitted");
        }
        fs::create_dir_all(home.join("Downloads/Folder")).unwrap();
        assert!(matches!(safety.check(&home.join("Downloads/Folder"), Grant::BigDownload), Err(Refusal::NotAPickedFile(_))));
        std::os::unix::fs::symlink(home.join("Downloads/movie.mov"), home.join("Downloads/link.mov")).unwrap();
        assert_eq!(safety.check(&home.join("Downloads/link.mov"), Grant::BigDownload), Err(Refusal::Symlink));
        assert_eq!(safety.check(&home.join("Downloads/gone.mov"), Grant::BigDownload), Err(Refusal::Missing));
        // Size and age are asked again at the moment it moves: a file that shrank, or changed since, is refused.
        let moved = home.join("Downloads/movie.mov");
        fs::write(&moved, vec![1u8; 10]).unwrap();
        old(&moved);
        assert!(matches!(safety.check(&moved, Grant::BigDownload), Err(Refusal::NotAPickedFile(w)) if w.contains("smaller")));
        let changed = home.join("Downloads/report.pdf");
        fs::write(&changed, vec![1u8; 5_000]).unwrap();
        assert!(matches!(safety.check(&changed, Grant::BigDownload), Err(Refusal::NotAPickedFile(w)) if w.contains("too recently")));
        // The real thresholds are the constants, and no other gate lets Downloads in.
        let default = Safety::for_home(&home);
        assert_eq!(default.big_download_rules(), (BIG_DOWNLOAD_MIN_BYTES, BIG_DOWNLOAD_MIN_AGE));
        assert_eq!((BIG_DOWNLOAD_MIN_BYTES, BIG_DOWNLOAD_MIN_AGE.as_secs()), (100_000_000, 90 * 86_400));
        assert!(
            matches!(default.check(&home.join("Downloads/archive.ZIP"), Grant::BigDownload), Err(Refusal::NotAPickedFile(w)) if w.contains("smaller"))
        );
        for grant in [Grant::Allowlisted, Grant::ProvenArtifact, Grant::ToolCache, Grant::Installer, Grant::DeviceBackup] {
            assert!(default.check(&home.join("Downloads/archive.ZIP"), grant).is_err(), "{grant:?}");
        }
    }

    #[test]
    fn a_device_backup_is_admitted_only_as_a_folder_with_a_devices_info_directly_in_the_backup_folder() {
        let home = tempdir();
        let safety = Safety::for_home(&home);
        let backup = make_backup(&home, "00008110-0001A2B3C4D5E6F7");
        assert_eq!(safety.check(&backup, Grant::DeviceBackup), Ok(backup.clone()));
        let folder = home.join(BACKUPS);
        // Not backups: no Info.plist, one that names no device, nonsense, a file, something nested, a link, the folder itself.
        leave(&folder.join("empty"), 10, true);
        fs::create_dir_all(folder.join("anonymous")).unwrap();
        fs::write(
            folder.join("anonymous/Info.plist"),
            "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>X</key><string>y</string></dict></plist>",
        )
        .unwrap();
        fs::create_dir_all(folder.join("broken")).unwrap();
        fs::write(folder.join("broken/Info.plist"), "not a plist").unwrap();
        leave(&folder.join("loose-file"), 10, false);
        std::os::unix::fs::symlink(&backup, folder.join("link")).unwrap();
        for name in ["empty", "anonymous", "broken", "loose-file"] {
            assert!(matches!(safety.check(&folder.join(name), Grant::DeviceBackup), Err(Refusal::NotAPickedFile(_))), "{name}");
        }
        assert_eq!(safety.check(&folder.join("link"), Grant::DeviceBackup), Err(Refusal::Symlink));
        assert!(matches!(safety.check(&backup.join("Manifest.db"), Grant::DeviceBackup), Err(Refusal::NotAPickedFile(_))));
        assert!(safety.check(&folder, Grant::DeviceBackup).is_err());
        let nested = backup.join("inner");
        fs::create_dir_all(&nested).unwrap();
        fs::copy(backup.join("Info.plist"), nested.join("Info.plist")).unwrap();
        assert!(safety.check(&nested, Grant::DeviceBackup).is_err(), "only a direct child");
        // The same folder under any other gate is refused: the never-delete list still guards Application Support.
        for grant in [Grant::Allowlisted, Grant::ProvenArtifact, Grant::ToolCache, Grant::Installer] {
            assert!(safety.check(&backup, grant).is_err(), "{grant:?}");
        }
        assert!(safety.check(&home.join("Downloads/Slack.dmg"), Grant::DeviceBackup).is_err());
    }

    // ------------------------------------------------------------------------------ tool caches

    fn make_tool_caches(home: &Path) {
        for cache in TOOL_CACHES {
            leave(&home.join(cache.path), 10, true);
        }
    }

    #[test]
    fn every_tool_cache_on_the_list_is_admitted_by_its_exact_place() {
        let home = tempdir();
        make_tool_caches(&home);
        let safety = Safety::for_home(&home);
        for cache in TOOL_CACHES {
            let path = home.join(cache.path);
            assert_eq!(safety.check(&path, Grant::ToolCache), Ok(path.clone()), "{}", cache.path);
        }
        let xdg = home.join(".cache/uv");
        leave(&xdg, 10, true);
        assert_eq!(safety.check(&xdg, Grant::ToolCache), Ok(xdg.clone()), "a folder directly inside ~/.cache");
    }

    #[test]
    fn nothing_beside_the_listed_tool_caches_is_admitted_least_of_all_what_sits_next_to_them() {
        let home = tempdir();
        make_tool_caches(&home);
        let safety = Safety::for_home(&home);
        // The things that share a parent with a listed cache, and the parents themselves.
        for rel in [
            ".cargo",
            ".cargo/bin",
            ".cargo/registry",
            ".cargo/registry/index",
            ".cargo/git",
            ".npm",
            ".npm/_logs",
            ".gradle",
            ".gradle/wrapper",
            ".gradle/init.d",
            ".bun",
            ".bun/install",
            "Library/pnpm",
            "Library/pnpm/global",
            "Library/pnpm/store/v3",
            ".cache",
            ".cache/.hidden",
            ".cache/uv/nested",
            "Library/Caches",
            "code",
        ] {
            let path = home.join(rel);
            leave(&path, 10, true);
            assert!(safety.check(&path, Grant::ToolCache).is_err(), "{rel} must not be admitted");
        }
        // Files that matter are not folders, and are not on the list either.
        for rel in [".cargo/credentials.toml", ".cargo/config.toml", ".npmrc", ".gradle/gradle.properties"] {
            let path = home.join(rel);
            leave(&path, 10, false);
            assert!(safety.check(&path, Grant::ToolCache).is_err(), "{rel} must not be admitted");
        }
        assert!(!is_tool_cache(Path::new(".cache")) && !is_tool_cache(Path::new(".cache/a/b")) && !is_tool_cache(Path::new("")));
        assert!(!is_tool_cache(Path::new(".cache/begone")) && !is_tool_cache(Path::new(".cache/.x")));
    }

    #[test]
    fn a_tool_cache_must_be_the_real_folder_in_the_real_place() {
        let home = tempdir();
        let safety = Safety::for_home(&home);
        // A listed place that is a symlink is refused before anything is followed.
        let elsewhere = home.join("elsewhere");
        leave(&elsewhere, 10, true);
        fs::create_dir_all(home.join(".npm")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, home.join(".npm/_cacache")).unwrap();
        assert_eq!(safety.check(&home.join(".npm/_cacache"), Grant::ToolCache), Err(Refusal::Symlink));
        // So is one whose parent was redirected: this is not the `.cargo` the list is about.
        let moved = home.join("on-another-disk");
        leave(&moved.join("registry/cache"), 10, true);
        std::os::unix::fs::symlink(&moved, home.join(".cargo")).unwrap();
        assert_eq!(safety.check(&home.join(".cargo/registry/cache"), Grant::ToolCache), Err(Refusal::OutsideAllowlist));
        // A listed place that has become a file is not a folder of cache.
        fs::create_dir_all(home.join(".gradle")).unwrap();
        fs::write(home.join(".gradle/caches"), b"x").unwrap();
        assert!(matches!(safety.check(&home.join(".gradle/caches"), Grant::ToolCache), Err(Refusal::NotAToolCache(_))));
        // Gone is gone.
        assert_eq!(safety.check(&home.join(".bun/install/cache"), Grant::ToolCache), Err(Refusal::Missing));
        // The same place under a different home is not this home's.
        let other = tempdir();
        leave(&other.join(".npm/_cacache"), 10, true);
        assert!(safety.check(&other.join(".npm/_cacache"), Grant::ToolCache).is_err());
        // And the home folder and the system are never a tool cache.
        assert!(safety.check(&home, Grant::ToolCache).is_err() && safety.check(Path::new("/usr"), Grant::ToolCache).is_err());
    }

    #[test]
    fn the_list_of_tool_caches_is_what_it_says_it_is() {
        let mut seen = std::collections::HashSet::new();
        for cache in TOOL_CACHES {
            assert!(seen.insert(cache.path), "{} is listed twice", cache.path);
            let path = Path::new(cache.path);
            assert!(path.is_relative() && path.components().all(|c| matches!(c, Component::Normal(_))), "{}", cache.path);
            assert!(path.components().count() >= 2, "{} is a folder inside a tool's own folder, never the tool's folder", cache.path);
            assert!(!cache.title.is_empty() && !cache.what.is_empty() && !cache.comes_back.is_empty(), "{}", cache.path);
            for parent in path.ancestors().skip(1).filter(|p| !p.as_os_str().is_empty()) {
                assert!(!is_tool_cache(parent), "{} would admit {}", parent.display(), cache.path);
            }
        }
        for never in
            [".cargo/bin", ".cargo/credentials.toml", ".ssh", ".aws", ".gnupg", ".config", "Documents", "Library/Application Support"]
        {
            assert!(!TOOL_CACHES.iter().any(|c| Path::new(c.path).starts_with(never)), "{never}");
        }
    }

    #[test]
    fn a_developer_store_is_admitted_only_where_it_sits_and_never_its_parent_or_a_neighbour() {
        let home = tempdir();
        leave(&home.join(".rustup/toolchains/stable-aarch64-apple-darwin"), 10, true);
        leave(&home.join(".rustup/toolchains/nightly-aarch64-apple-darwin"), 10, true);
        leave(&home.join(".rustup/downloads"), 10, true);
        leave(&home.join(".nvm/versions/node/v20.11.1"), 10, true);
        leave(&home.join(".pyenv/versions/3.12.1"), 10, true);
        leave(&home.join(".ollama/models"), 10, true);
        let safety = Safety::for_home(&home);
        // Each child of a per-child store, and each whole-folder store, is admitted.
        for ok in [
            ".rustup/toolchains/stable-aarch64-apple-darwin",
            ".rustup/toolchains/nightly-aarch64-apple-darwin",
            ".rustup/downloads",
            ".nvm/versions/node/v20.11.1",
            ".pyenv/versions/3.12.1",
            ".ollama/models",
        ] {
            assert!(safety.check(&home.join(ok), Grant::DevStore).is_ok(), "{ok} should pass");
        }
        // The parent of a per-child store is never itself removable, nor the tool's own folder, nor a neighbour.
        for refused in
            [".rustup", ".rustup/toolchains", ".nvm/versions/node", ".nvm", ".pyenv/versions", ".ollama", ".cargo/bin", ".config"]
        {
            leave(&home.join(refused), 10, true);
            assert!(safety.check(&home.join(refused), Grant::DevStore).is_err(), "{refused} must be refused");
        }
        // A file, not a folder, is not a store; a symlinked toolchain is refused before it is followed.
        leave(&home.join(".rustup/toolchains/a-file"), 10, false);
        assert!(safety.check(&home.join(".rustup/toolchains/a-file"), Grant::DevStore).is_err());
        let elsewhere = home.join("elsewhere");
        leave(&elsewhere, 10, true);
        std::os::unix::fs::symlink(&elsewhere, home.join(".nvm/versions/node/linked")).unwrap();
        assert!(matches!(safety.check(&home.join(".nvm/versions/node/linked"), Grant::DevStore), Err(Refusal::Symlink)));
    }

    #[test]
    fn a_virtual_machine_is_admitted_only_at_the_place_its_app_keeps_it() {
        let home = tempdir();
        let utm = "Library/Containers/com.utmapp.UTM/Data/Documents";
        leave(&home.join(format!("{utm}/Ubuntu.utm")), 100, true);
        leave(&home.join("Parallels/Windows 11.pvm"), 100, true);
        leave(&home.join("Virtual Machines.localized/macOS.vmwarevm"), 100, true);
        leave(&home.join("Library/Containers/com.docker.docker/Data/vms/0/data/Docker.raw"), 100, false);
        leave(&home.join(".orbstack/data"), 100, true);
        let safety = Safety::for_home(&home);
        for ok in [
            format!("{utm}/Ubuntu.utm"),
            "Parallels/Windows 11.pvm".to_string(),
            "Virtual Machines.localized/macOS.vmwarevm".to_string(),
            "Library/Containers/com.docker.docker/Data/vms/0/data/Docker.raw".to_string(),
            ".orbstack/data".to_string(),
        ] {
            assert!(safety.check(&home.join(&ok), Grant::VmImage).is_ok(), "{ok} should pass");
        }
        // Not a VM: a plain file beside the bundles, a wrong extension, the parent folder itself, a tool's folder.
        leave(&home.join(format!("{utm}/notes.txt")), 100, false);
        leave(&home.join(format!("{utm}/thing.other")), 100, true);
        for refused in
            [format!("{utm}/notes.txt"), format!("{utm}/thing.other"), utm.to_string(), "Parallels".to_string(), ".orbstack".to_string()]
        {
            assert!(safety.check(&home.join(&refused), Grant::VmImage).is_err(), "{refused} must be refused");
        }
        // A .utm that is a file, not a bundle, is refused; a symlink is refused before it is followed.
        leave(&home.join(format!("{utm}/File.utm")), 100, false);
        assert!(matches!(safety.check(&home.join(format!("{utm}/File.utm")), Grant::VmImage), Err(Refusal::NotAVm(_))));
        let elsewhere = home.join("elsewhere");
        leave(&elsewhere, 100, true);
        std::os::unix::fs::symlink(&elsewhere, home.join(format!("{utm}/Link.utm"))).unwrap();
        assert!(matches!(safety.check(&home.join(format!("{utm}/Link.utm")), Grant::VmImage), Err(Refusal::Symlink)));
    }

    #[test]
    fn an_external_volume_is_operable_but_other_volumes_and_system_paths_stay_off_limits() {
        let vol = Safety::for_home(Path::new("/Volumes/MyDrive"));
        assert!(vol.is_external());
        // Inside the chosen volume, the /Volumes guard no longer blocks.
        assert_eq!(vol.system_block(Path::new("/Volumes/MyDrive/big.iso")), None);
        assert_eq!(vol.system_block(Path::new("/Volumes/MyDrive/projects/app/target")), None);
        // Another drive, and anything else under /Volumes, stays blocked.
        assert_eq!(vol.system_block(Path::new("/Volumes/OtherDrive/x")), Some("/Volumes"));
        assert_eq!(vol.system_block(Path::new("/Volumes/MyDriveTwo/x")), Some("/Volumes"), "a prefix match is not a path match");
        // System territory is never relaxed, whatever the target.
        for p in ["/System/Library", "/usr/bin/x", "/Applications/Thing.app", "/private/var/db/x"] {
            assert!(vol.system_block(Path::new(p)).is_some(), "{p} must stay blocked");
        }
        // On a volume, duplicates and big files are looked for across the whole drive.
        assert_eq!(vol.big_file_roots(), vec![PathBuf::from("/Volumes/MyDrive")]);
        // The home folder is not external, and blocks all of /Volumes exactly as before.
        let home = Safety::for_home(Path::new("/Users/test"));
        assert!(!home.is_external());
        assert_eq!(home.system_block(Path::new("/Volumes/Any/x")), Some("/Volumes"));
        assert_eq!(home.big_file_roots().len(), 3);
    }

    #[test]
    fn a_duplicate_is_removable_only_while_an_identical_original_is_really_there() {
        let home = tempdir();
        let original = home.join("keep/report.pdf");
        let copy = home.join("Downloads/report copy.pdf");
        let different = home.join("Downloads/other.pdf");
        fs::create_dir_all(home.join("keep")).unwrap();
        fs::create_dir_all(home.join("Downloads")).unwrap();
        fs::write(&original, vec![7u8; 40_000]).unwrap();
        fs::write(&copy, vec![7u8; 40_000]).unwrap();
        fs::write(&different, vec![8u8; 40_000]).unwrap();
        let safety = Safety::for_home(&home);
        let orig = original.to_str();
        // The copy goes, against the named original that stays.
        assert!(safety.check_move(&copy, Grant::DuplicateCopy, orig).is_ok());
        // Without the original named, or against a file that does not match, it is refused.
        assert!(matches!(safety.check(&copy, Grant::DuplicateCopy), Err(Refusal::NotADuplicate(_))));
        assert!(matches!(safety.check_move(&different, Grant::DuplicateCopy, orig), Err(Refusal::NotADuplicate(_))));
        // The same file is not its own copy; a copy that has since changed no longer matches; a vanished original refuses.
        assert!(matches!(safety.check_move(&original, Grant::DuplicateCopy, orig), Err(Refusal::NotADuplicate(_))));
        fs::write(&copy, vec![9u8; 40_000]).unwrap();
        assert!(matches!(safety.check_move(&copy, Grant::DuplicateCopy, orig), Err(Refusal::NotADuplicate(_))));
        fs::write(&copy, vec![7u8; 40_000]).unwrap();
        fs::remove_file(&original).unwrap();
        assert!(matches!(safety.check_move(&copy, Grant::DuplicateCopy, orig), Err(Refusal::NotADuplicate(_))));
    }
}
