//! What begone looks for. Scanners only *discover* candidates; the engine checks each one
//! against the safety layer, measures it, and turns it into a row. Nothing a scanner says
//! can widen what the deleter will touch.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use crate::model::{Evidence, Kind, SafetyClass, Tag};
use crate::safety::Grant;

pub mod caches;
pub mod developer;
pub mod logs;
pub mod procs;
pub mod tools;
pub mod trash;
pub mod xcode;

/// A thing worth offering, before it is sized.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub path: PathBuf,
    pub kind: Kind,
    pub grant: Grant,
    pub safety: SafetyClass,
    pub tag: Tag,
    pub title: String,
    pub source: String,
    pub what: String,
    pub comes_back: String,
    pub evidence: Vec<Evidence>,
    pub pre_selected: bool,
    /// Drop it unless nothing inside changed for at least this long.
    pub min_idle: Option<Duration>,
    /// If something inside changed more recently than this, call it in use and leave it unchecked.
    pub in_use_window: Option<Duration>,
}

impl Candidate {
    pub fn new(path: PathBuf, kind: Kind, grant: Grant, title: impl Into<String>) -> Self {
        Self {
            path,
            kind,
            grant,
            safety: SafetyClass::Safe,
            tag: Tag::Safe,
            title: title.into(),
            source: String::new(),
            what: String::new(),
            comes_back: String::new(),
            evidence: Vec::new(),
            pre_selected: true,
            min_idle: None,
            in_use_window: None,
        }
    }

    pub fn src(mut self, source: impl Into<String>) -> Self {
        self.source = source.into();
        self
    }

    pub fn explain(mut self, what: impl Into<String>, comes_back: impl Into<String>) -> Self {
        self.what = what.into();
        self.comes_back = comes_back.into();
        self
    }

    pub fn ev(mut self, evidence: Evidence) -> Self {
        self.evidence.push(evidence);
        self
    }

    pub fn class(mut self, safety: SafetyClass, tag: Tag) -> Self {
        self.safety = safety;
        self.tag = tag;
        self
    }

    pub fn unchecked(mut self) -> Self {
        self.pre_selected = false;
        self
    }

    pub fn idle_at_least(mut self, d: Duration) -> Self {
        self.min_idle = Some(d);
        self
    }

    pub fn in_use_within(mut self, d: Duration) -> Self {
        self.in_use_window = Some(d);
        self
    }
}

/// What a scanner gets to look at.
pub struct Ctx<'a> {
    pub home: &'a Path,
    pub procs: &'a procs::Procs,
    pub cancel: &'a AtomicBool,
}

impl Ctx<'_> {
    pub fn cancelled(&self) -> bool {
        self.cancel.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// `(what the UI says it is reading, how much of the progress bar it owns, discovery)`
pub type Discover = fn(&Ctx<'_>) -> Vec<Candidate>;

pub const STAGES: [(&str, f32, Discover); 5] = [
    ("app caches", 0.20, caches::discover),
    ("tool caches", 0.10, tools::discover),
    ("old logs", 0.10, logs::discover),
    ("Xcode", 0.15, xcode::discover),
    ("build artifacts", 0.45, developer::discover),
];
