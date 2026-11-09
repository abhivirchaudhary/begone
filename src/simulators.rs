//! Xcode's simulators: the devices people create in it and the runtimes those devices run on. They are big, and the way Xcode
//! keeps its own lists right is `simctl`, so begone never touches their folders. It lists them from `simctl`, and when asked has
//! `simctl` delete them. What `simctl` deletes does not go to the Trash, so this is the one step besides emptying the Trash that
//! cannot be undone, and it is asked about on its own, never together with files that can be put back.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use crate::apps::{next_id, parse_mdls_date};
use crate::engine::{EmptyEvent, EmptyReport, Env};
use crate::model::{Evidence, Kind, SafetyClass, ScanItem, Tag, ago, human, idle_since};
use crate::safety::{Grant, Safety};

/// How begone talks to `simctl`: one call, its answer or why it failed. The real one runs `xcrun simctl`; tests stand in for it.
pub trait Simctl: Send + Sync + std::fmt::Debug {
    fn run(&self, args: &[&str], timeout: Duration) -> Result<String, String>;
}

/// `xcrun simctl ...`, as a child process with a time limit, never through a shell.
#[derive(Debug)]
pub struct Xcrun {
    program: PathBuf,
}

impl Xcrun {
    /// Xcode's own tool, on a real machine.
    pub fn system() -> Self {
        Self { program: PathBuf::from("/usr/bin/xcrun") }
    }

    /// Another program that answers as `xcrun` does, for trying begone on a scratch home.
    pub fn with_program(program: impl Into<PathBuf>) -> Self {
        Self { program: program.into() }
    }
}

fn drain(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = pipe.read_to_string(&mut text);
        text
    })
}

impl Simctl for Xcrun {
    fn run(&self, args: &[&str], timeout: Duration) -> Result<String, String> {
        let mut child = Command::new(&self.program)
            .arg("simctl")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| format!("could not run {} ({err})", self.program.display()))?;
        // Read both pipes while waiting, or a long answer fills the pipe and the child never ends.
        let out = child.stdout.take().map(drain);
        let err = child.stderr.take().map(drain);
        let started = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if started.elapsed() > timeout => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("simctl did not answer in time".to_string());
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(40)),
                Err(err) => return Err(err.to_string()),
            }
        };
        let stdout = out.and_then(|h| h.join().ok()).unwrap_or_default();
        let stderr = err.and_then(|h| h.join().ok()).unwrap_or_default();
        if status.success() {
            Ok(stdout)
        } else {
            let said = stderr.lines().chain(stdout.lines()).map(str::trim).find(|l| !l.is_empty()).unwrap_or("it failed");
            Err(said.to_string())
        }
    }
}

/// A simulator device or runtime, as `simctl` describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sim {
    /// The device's UDID, or the runtime's identifier: both are UUIDs, and the only thing ever handed back to `simctl`.
    pub id: String,
    pub runtime: bool,
    pub name: String,
    /// `iOS 26.5`: the runtime a device runs on, or the runtime itself.
    pub platform: String,
    pub bytes: u64,
    pub last_used: Option<SystemTime>,
    pub running: bool,
    /// A device whose runtime is gone cannot be used.
    pub available: bool,
    /// For a runtime: how many devices run on it.
    pub uses: usize,
    pub path: PathBuf,
}

/// Is this written as a UUID, which is all that is ever passed to `simctl delete`?
pub fn is_uuid(text: &str) -> bool {
    text.len() == 36 && text.chars().enumerate().all(|(i, c)| if [8, 13, 18, 23].contains(&i) { c == '-' } else { c.is_ascii_hexdigit() })
}

/// `com.apple.CoreSimulator.SimRuntime.iOS-26-5` as `iOS 26.5`.
pub fn runtime_label(identifier: &str) -> String {
    let last = identifier.rsplit('.').next().unwrap_or(identifier);
    match last.split_once('-') {
        Some((platform, version)) if !version.is_empty() => format!("{platform} {}", version.replace('-', ".")),
        _ => last.to_string(),
    }
}

/// `2026-10-24T07:25:24Z`, as `simctl` writes a date.
fn iso_date(text: &str) -> Option<SystemTime> {
    let text = text.trim().trim_end_matches('Z');
    let text = text.split('.').next()?;
    parse_mdls_date(&format!("{} +0000", text.replace('T', " ")))
}

/// What `simctl` says is there: devices with their size and when they were last used, and runtimes that can be deleted.
pub fn list(simctl: &dyn Simctl) -> Result<Vec<Sim>, String> {
    let devices = simctl.run(&["list", "devices", "-j"], Duration::from_secs(90))?;
    let runtimes = simctl.run(&["runtime", "list", "-j"], Duration::from_secs(90)).unwrap_or_default();
    parse(&devices, &runtimes)
}

pub fn parse(devices_json: &str, runtimes_json: &str) -> Result<Vec<Sim>, String> {
    let devices: serde_json::Value =
        serde_json::from_str(devices_json).map_err(|err| format!("simctl's list could not be read ({err})"))?;
    let mut out = Vec::new();
    let mut uses = std::collections::HashMap::<String, usize>::new();
    for (runtime, list) in devices.get("devices").and_then(|d| d.as_object()).into_iter().flatten() {
        let platform = runtime_label(runtime);
        for device in list.as_array().into_iter().flatten() {
            let text = |key: &str| device.get(key).and_then(|v| v.as_str());
            let Some(udid) = text("udid").filter(|u| is_uuid(u)) else { continue };
            *uses.entry(runtime.clone()).or_insert(0) += 1;
            out.push(Sim {
                id: udid.to_string(),
                runtime: false,
                name: text("name").unwrap_or("Simulator").to_string(),
                platform: platform.clone(),
                bytes: device.get("dataPathSize").and_then(|v| v.as_u64()).unwrap_or(0),
                last_used: text("lastUsedAt").or_else(|| text("lastBootedAt")).and_then(iso_date),
                running: matches!(text("state"), Some("Booted" | "Booting" | "Shutting Down")),
                available: device.get("isAvailable").and_then(|v| v.as_bool()).unwrap_or(true),
                uses: 0,
                path: PathBuf::from(text("dataPath").unwrap_or_default()),
            });
        }
    }
    if let Ok(runtimes) = serde_json::from_str::<serde_json::Value>(runtimes_json) {
        for (id, runtime) in runtimes.as_object().into_iter().flatten() {
            let text = |key: &str| runtime.get(key).and_then(|v| v.as_str());
            // One that cannot be deleted (it came with Xcode) is not offered.
            if !is_uuid(id) || runtime.get("deletable").and_then(|v| v.as_bool()) != Some(true) {
                continue;
            }
            let identifier = text("runtimeIdentifier").unwrap_or_default();
            out.push(Sim {
                id: id.clone(),
                runtime: true,
                name: runtime_label(identifier),
                platform: runtime_label(identifier),
                bytes: runtime.get("sizeBytes").and_then(|v| v.as_u64()).unwrap_or(0),
                last_used: text("lastUsedAt").and_then(iso_date),
                running: false,
                available: true,
                uses: uses.get(identifier).copied().unwrap_or(0),
                path: PathBuf::from(text("path").unwrap_or_default()),
            });
        }
    }
    Ok(out)
}

/// A simulator as a row of the Files tab. Its grant admits nothing to the Trash: a simulator is only ever deleted by `simctl`.
pub fn scan_item(sim: &Sim, safety: &Safety) -> ScanItem {
    let mut evidence = vec![Evidence::good("Xcode's own tool, simctl, deletes it, so Xcode's lists stay right")];
    evidence.push(Evidence::warn("Deleted for good: it does not go to the Trash, so begone cannot bring it back"));
    let (title, source, what, comes_back) = if sim.runtime {
        evidence.push(if sim.uses > 0 {
            Evidence::warn(format!("{} {} use it and would stop working", sim.uses, if sim.uses == 1 { "simulator" } else { "simulators" }))
        } else {
            Evidence::good("No simulator uses it")
        });
        (
            format!("{} simulator runtime", sim.platform),
            "sim runtime",
            format!("The {} runtime that simulators run on, {}.", sim.platform, human(sim.bytes)),
            "Download it again in Xcode, in Settings under Components.".to_string(),
        )
    } else {
        if !sim.available {
            evidence.push(Evidence::good("Its runtime is gone, so it cannot be used"));
        }
        if sim.running {
            evidence.push(Evidence::warn("It is running now: shut it down first"));
        }
        (
            format!("{} ({})", sim.name, sim.platform),
            "simulator",
            format!("A simulator, {} on {}, with its apps and data.", sim.name, sim.platform),
            "Make a new one in Xcode. This one's apps and data are gone for good.".to_string(),
        )
    };
    evidence.push(match idle_since(sim.last_used) {
        Some(idle) => Evidence::good(format!("Last used {}", ago(idle))),
        None => Evidence::good("It has never been used"),
    });
    ScanItem {
        id: next_id(),
        kept: safety.is_kept(&sim.path),
        path: sim.path.clone(),
        size: sim.bytes,
        files: 0,
        kind: Kind::Files,
        safety: SafetyClass::Yours,
        tag: if sim.running { Tag::Running } else { Tag::Yours },
        grant: Grant::Simulator,
        title,
        source: source.to_string(),
        what,
        comes_back,
        evidence,
        modified: sim.last_used,
        pre_selected: false,
        owner: Some(sim.id.clone()),
    }
}

/// The `simctl` words that delete this row, or why it must not be asked at all.
fn delete_args(item: &ScanItem) -> Result<Vec<String>, String> {
    if item.grant != Grant::Simulator {
        return Err("it is not a simulator".to_string());
    }
    let id = item.owner.as_deref().filter(|id| is_uuid(id)).ok_or_else(|| "it has no valid identifier".to_string())?;
    Ok(if item.source == "sim runtime" {
        vec!["runtime".to_string(), "delete".to_string(), id.to_string()]
    } else {
        vec!["delete".to_string(), id.to_string()]
    })
}

/// Ask `simctl` to delete the chosen simulators, one at a time, reporting as the emptying of the Trash does: what went, what
/// stayed and why. Each is refused first if it is kept, and the history records what was deleted for good.
pub fn delete(
    env: &Env,
    simctl: &dyn Simctl,
    items: &[ScanItem],
    cancel: &AtomicBool,
    on_event: &mut dyn FnMut(EmptyEvent),
) -> EmptyReport {
    on_event(EmptyEvent::Started { items: items.len(), bytes: items.iter().map(|i| i.size).sum() });
    let mut report = EmptyReport::default();
    let (mut devices, mut runtimes) = (0usize, 0usize);
    for item in items {
        let mut gone = 0;
        let outcome = if cancel.load(Ordering::Relaxed) {
            Err("stopped before this one".to_string())
        } else if env.safety.is_kept(&item.path) {
            Err("you asked begone to keep it".to_string())
        } else {
            delete_args(item).and_then(|args| {
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                let long = if args.first() == Some(&"runtime") { Duration::from_secs(900) } else { Duration::from_secs(180) };
                simctl.run(&args, long).map(|_| ())
            })
        };
        match outcome {
            Ok(()) => {
                report.removed += 1;
                report.bytes += item.size;
                report.gone.push(item.path.clone());
                gone = item.size;
                if item.source == "sim runtime" {
                    runtimes += 1;
                } else {
                    devices += 1;
                }
            }
            Err(why) => report.kept.push((item.path.clone(), why)),
        }
        on_event(EmptyEvent::Item { bytes: gone });
    }
    if report.removed > 0
        && let Ok(mut app) = env.journal.appender()
    {
        let plural = |n: usize, one: &str| format!("{n} {one}{}", if n == 1 { "" } else { "s" });
        let what = match (devices, runtimes) {
            (d, 0) => plural(d, "simulator"),
            (0, r) => plural(r, "simulator runtime"),
            (d, r) => format!("{} and {}", plural(d, "simulator"), plural(r, "runtime")),
        };
        let _ = app.deleted(&what, report.bytes);
        let _ = app.sync();
    }
    on_event(EmptyEvent::Finished(report.clone()));
    report
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::safety::tests::tempdir;
    use std::sync::Mutex;

    const DEVICES: &str = r#"{"devices":{
        "com.apple.CoreSimulator.SimRuntime.iOS-26-5":[
          {"dataPath":"/Sims/Devices/064A5F07-ADB2-4AA2-95E1-FEEB341A883B/data","dataPathSize":3743993856,"isAvailable":true,"lastUsedAt":"2026-10-24T07:25:24Z","name":"iPhone 17 Pro","state":"Shutdown","udid":"064A5F07-ADB2-4AA2-95E1-FEEB341A883B"},
          {"dataPath":"/Sims/Devices/D5D087F9-4272-458C-909C-562333775904/data","dataPathSize":18337792,"isAvailable":true,"name":"iPhone 17 Pro Max","state":"Booted","udid":"D5D087F9-4272-458C-909C-562333775904"}],
        "com.apple.CoreSimulator.SimRuntime.iOS-18-0":[
          {"dataPath":"/Sims/Devices/9A9A77B9-37CC-48C5-8927-A426EB5A407D/data","dataPathSize":9000000,"isAvailable":false,"name":"iPhone 15","state":"Shutdown","udid":"9A9A77B9-37CC-48C5-8927-A426EB5A407D"},
          {"dataPath":"/x","dataPathSize":1,"isAvailable":true,"name":"Bad id","state":"Shutdown","udid":"not-a-uuid; rm -rf /"}]}}"#;
    const RUNTIMES: &str = r#"{
        "A1AFF4E9-7CA7-4098-BC41-0180A54B43A4":{"build":"23F77","deletable":true,"kind":"Patchable Cryptex Disk Image","lastUsedAt":"2026-10-24T07:25:24Z","path":"/System/Library/AssetsV2/x.dmg","runtimeIdentifier":"com.apple.CoreSimulator.SimRuntime.iOS-26-5","sizeBytes":8494282293,"state":"Ready","version":"26.5"},
        "B1AFF4E9-7CA7-4098-BC41-0180A54B43A4":{"deletable":false,"runtimeIdentifier":"com.apple.CoreSimulator.SimRuntime.iOS-18-0","sizeBytes":1,"version":"18.0"},
        "not-an-id":{"deletable":true,"runtimeIdentifier":"x","sizeBytes":1}}"#;

    /// Stands in for `simctl`: canned lists, and a record of what it was asked to delete.
    #[derive(Debug, Default)]
    pub struct Fake {
        pub devices: String,
        pub runtimes: String,
        pub calls: Mutex<Vec<Vec<String>>>,
        /// Identifiers it refuses to delete, with its reason.
        pub fail: Vec<(String, String)>,
    }

    impl Simctl for Fake {
        fn run(&self, args: &[&str], _timeout: Duration) -> Result<String, String> {
            self.calls.lock().unwrap().push(args.iter().map(|a| (*a).to_string()).collect());
            match args {
                ["list", "devices", "-j"] => Ok(self.devices.clone()),
                ["runtime", "list", "-j"] => Ok(self.runtimes.clone()),
                [.., "delete", id] => match self.fail.iter().find(|(f, _)| f == id) {
                    Some((_, why)) => Err(why.clone()),
                    None => Ok(String::new()),
                },
                other => Err(format!("unexpected {other:?}")),
            }
        }
    }

    pub fn fake() -> Fake {
        Fake { devices: DEVICES.to_string(), runtimes: RUNTIMES.to_string(), ..Fake::default() }
    }

    #[test]
    fn simctls_lists_become_devices_and_runtimes_with_their_size_age_and_state() {
        let sims = list(&fake()).unwrap();
        let by = |name: &str| sims.iter().find(|s| s.name == name).unwrap_or_else(|| panic!("{name}"));
        assert_eq!(sims.len(), 4, "three devices with an identifier, and the one runtime that can be deleted: {sims:?}");
        let pro = by("iPhone 17 Pro");
        assert_eq!(
            (pro.bytes, pro.running, pro.available, pro.runtime, pro.platform.as_str()),
            (3_743_993_856, false, true, false, "iOS 26.5")
        );
        assert!(
            pro.last_used.is_some_and(|t| t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs() == 1_792_826_724),
            "{:?}",
            pro.last_used
        );
        assert!(by("iPhone 17 Pro Max").running && by("iPhone 17 Pro Max").last_used.is_none());
        assert!(!by("iPhone 15").available, "its runtime is gone");
        let runtime = by("iOS 26.5");
        assert_eq!(
            (runtime.runtime, runtime.bytes, runtime.uses, runtime.id.as_str()),
            (true, 8_494_282_293, 2, "A1AFF4E9-7CA7-4098-BC41-0180A54B43A4")
        );
        assert!(sims.iter().all(|s| is_uuid(&s.id)), "nothing that is not a UUID is ever kept to be handed back");
        // No runtime list at all still gives the devices.
        let devices_only = Fake { devices: DEVICES.to_string(), ..Fake::default() };
        assert_eq!(list(&devices_only).unwrap().iter().filter(|s| s.runtime).count(), 0);
        assert!(parse("not json", "{}").is_err());
    }

    #[test]
    fn identifiers_and_labels_are_read_the_way_xcode_writes_them() {
        assert!(is_uuid("064A5F07-ADB2-4AA2-95E1-FEEB341A883B") && is_uuid("064a5f07-adb2-4aa2-95e1-feeb341a883b"));
        for bad in [
            "",
            "064A5F07ADB24AA295E1FEEB341A883B",
            "064A5F07-ADB2-4AA2-95E1-FEEB341A883",
            "064A5F07-ADB2-4AA2-95E1-FEEB341A883G",
            "x; rm -rf /",
            "--all",
        ] {
            assert!(!is_uuid(bad), "{bad:?}");
        }
        assert_eq!(runtime_label("com.apple.CoreSimulator.SimRuntime.iOS-26-5"), "iOS 26.5");
        assert_eq!(runtime_label("com.apple.CoreSimulator.SimRuntime.watchOS-11-0"), "watchOS 11.0");
        assert_eq!(runtime_label("odd"), "odd");
        assert!(iso_date("2026-10-24T07:25:24.123Z").is_some() && iso_date("yesterday").is_none());
    }

    #[test]
    fn a_simulator_row_says_it_is_deleted_by_xcodes_tool_and_cannot_come_back_and_a_booted_one_says_to_shut_it_down() {
        let home = tempdir();
        let safety = Safety::for_home(&home);
        let sims = list(&fake()).unwrap();
        let rows: Vec<ScanItem> = sims.iter().map(|s| scan_item(s, &safety)).collect();
        for row in &rows {
            assert_eq!((row.kind, row.grant, row.pre_selected, row.safety), (Kind::Files, Grant::Simulator, false, SafetyClass::Yours));
            assert!(row.owner.as_deref().is_some_and(is_uuid) && row.files == 0);
            assert!(row.evidence.iter().any(|e| e.ok && e.text.contains("simctl")));
            assert!(row.evidence.iter().any(|e| !e.ok && e.text.contains("does not go to the Trash")));
        }
        let by = |t: &str| rows.iter().find(|r| r.title.starts_with(t)).unwrap();
        assert_eq!((by("iPhone 17 Pro (").source.as_str(), by("iPhone 17 Pro (").tag), ("simulator", Tag::Yours));
        assert_eq!(by("iPhone 17 Pro Max").tag, Tag::Running);
        assert!(by("iPhone 17 Pro Max").evidence.iter().any(|e| !e.ok && e.text.contains("shut it down")));
        assert!(by("iPhone 15").evidence.iter().any(|e| e.ok && e.text.contains("cannot be used")));
        let runtime = by("iOS 26.5 simulator runtime");
        assert_eq!(runtime.source, "sim runtime");
        assert!(runtime.evidence.iter().any(|e| !e.ok && e.text.contains("2 simulators use it")), "{:?}", runtime.evidence);
        assert!(runtime.comes_back.contains("Components"));
        // The gate admits none of them to the Trash: a simulator is only ever deleted by simctl.
        for row in &rows {
            assert!(safety.check(&row.path, row.grant).is_err(), "{}", row.title);
        }
        // A path you keep marks its row, and only its row.
        safety.set_kept(vec![PathBuf::from("/Sims/Devices/064A5F07-ADB2-4AA2-95E1-FEEB341A883B")]);
        let kept: Vec<&str> =
            sims.iter().map(|s| (s, scan_item(s, &safety))).filter(|(_, r)| r.kept).map(|(s, _)| s.name.as_str()).collect();
        assert_eq!(kept, ["iPhone 17 Pro"]);
    }

    fn rows_of(fake: &Fake, safety: &Safety) -> Vec<ScanItem> {
        list(fake).unwrap().iter().map(|s| scan_item(s, safety)).collect()
    }

    /// What it was asked to delete, leaving out the reading.
    fn deletes(fake: &Fake) -> Vec<Vec<String>> {
        fake.calls.lock().unwrap().iter().filter(|c| c.iter().any(|a| a == "delete")).cloned().collect()
    }

    #[test]
    fn deleting_asks_simctl_for_each_by_identifier_only_and_reports_what_went_and_what_stayed_and_why() {
        let home = tempdir();
        let env = Env::new(home.to_path_buf());
        let mut sim = fake();
        sim.fail = vec![("9A9A77B9-37CC-48C5-8927-A426EB5A407D".to_string(), "Unable to delete device: it is in use".to_string())];
        let rows = rows_of(&sim, &env.safety);
        let chosen: Vec<ScanItem> = rows.into_iter().filter(|r| !r.title.starts_with("iPhone 17 Pro Max")).collect();
        assert_eq!(chosen.len(), 3);
        let mut events = Vec::new();
        let report = delete(&env, &sim, &chosen, &AtomicBool::new(false), &mut |e| events.push(e));
        let calls = deletes(&sim);
        let want: Vec<Vec<String>> = chosen
            .iter()
            .map(|r| {
                let id = r.owner.clone().unwrap();
                if r.source == "sim runtime" {
                    vec!["runtime".to_string(), "delete".to_string(), id]
                } else {
                    vec!["delete".to_string(), id]
                }
            })
            .collect();
        assert_eq!(calls, want, "only `delete <uuid>` and `runtime delete <uuid>`, in the order chosen");
        assert_eq!(calls.len(), 3);
        assert_eq!(report.removed, 2);
        assert_eq!(report.bytes, 3_743_993_856 + 8_494_282_293);
        assert_eq!(report.kept.len(), 1);
        assert!(report.kept[0].1.contains("it is in use"), "{:?}", report.kept);
        let counted: Vec<u64> =
            events.iter().filter_map(|e| if let EmptyEvent::Item { bytes } = e { Some(*bytes) } else { None }).collect();
        let stayed = chosen.iter().position(|r| r.owner.as_deref() == Some("9A9A77B9-37CC-48C5-8927-A426EB5A407D")).unwrap();
        assert_eq!(counted.iter().filter(|b| **b == 0).count(), 1, "what actually went, and 0 for the one that stayed");
        assert_eq!(counted[stayed], 0);
        assert_eq!(counted.iter().sum::<u64>(), report.bytes);
        assert!(
            matches!(events.first(), Some(EmptyEvent::Started { items: 3, .. })) && matches!(events.last(), Some(EmptyEvent::Finished(_)))
        );
        let history = env.journal.history(env.safety.trash_dir());
        assert!(
            matches!(&history[0], crate::journal::HistoryEntry::Deleted { what, bytes, .. } if what == "1 simulator and 1 runtime" && *bytes == report.bytes),
            "{history:?}"
        );
    }

    #[test]
    fn nothing_that_is_kept_stopped_or_not_a_simulator_is_ever_passed_to_simctl() {
        let home = tempdir();
        let env = Env::new(home.to_path_buf());
        let sim = fake();
        let mut rows = rows_of(&sim, &env.safety);
        env.safety.set_kept(vec![rows[0].path.clone()]);
        // Forged: a file dressed as a simulator, one with a hostile identifier, one with none.
        let mut forged = rows[1].clone();
        forged.grant = Grant::Allowlisted;
        let mut hostile = rows[1].clone();
        hostile.owner = Some("--all".to_string());
        let mut nameless = rows[1].clone();
        nameless.owner = None;
        rows.truncate(1);
        rows.extend([forged, hostile, nameless]);
        let report = delete(&env, &sim, &rows, &AtomicBool::new(false), &mut |_| {});
        assert!(deletes(&sim).is_empty(), "{:?}", deletes(&sim));
        assert_eq!(report.removed, 0);
        assert_eq!(report.kept.len(), 4);
        assert!(
            report.kept[0].1.contains("keep it")
                && report.kept[1].1.contains("not a simulator")
                && report.kept[2].1.contains("valid identifier")
        );
        // A stop request is honoured before each one.
        env.safety.set_kept(Vec::new());
        let fresh = rows_of(&sim, &env.safety);
        let stopped = delete(&env, &sim, &fresh, &AtomicBool::new(true), &mut |_| {});
        assert_eq!((stopped.removed, stopped.kept.len()), (0, 4));
        assert!(deletes(&sim).is_empty());
        assert!(env.journal.history(env.safety.trash_dir()).is_empty(), "nothing deleted, nothing recorded");
    }

    #[test]
    fn the_real_runner_runs_a_program_with_simctl_first_and_reports_its_failure_and_its_silence() {
        let dir = tempdir();
        let script = dir.join("xcrun");
        std::fs::write(&script, "#!/bin/sh\nif [ \"$1\" != simctl ]; then echo wrong >&2; exit 9; fi\nshift\ncase \"$1\" in\n  hang) sleep 5;;\n  fail) echo 'Unable to delete: busy' >&2; exit 3;;\n  *) echo \"ok $*\";;\nesac\n").unwrap();
        std::fs::set_permissions(&script, <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755)).unwrap();
        let xcrun = Xcrun::with_program(&script);
        assert_eq!(xcrun.run(&["list", "devices", "-j"], Duration::from_secs(5)).unwrap().trim(), "ok list devices -j");
        assert_eq!(xcrun.run(&["fail"], Duration::from_secs(5)), Err("Unable to delete: busy".to_string()));
        let started = Instant::now();
        assert_eq!(xcrun.run(&["hang"], Duration::from_millis(300)), Err("simctl did not answer in time".to_string()));
        assert!(started.elapsed() < Duration::from_secs(3), "it gave up at its time limit, not when the child ended");
        assert!(Xcrun::with_program("/nonexistent/xcrun").run(&["list"], Duration::from_secs(1)).unwrap_err().contains("could not run"));
    }
}
