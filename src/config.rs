//! A small, optional config file for the thresholds that are otherwise constants, so a power user can tune what begone offers
//! without rebuilding. It lives beside the keep list at `<home>/.config/begone/config.txt`, one `key = value` per line, with
//! `#` comments. Everything is optional: an absent file, an unknown key, or an unparseable value just leaves the built-in
//! default in place. Nothing here can make begone offer *more kinds* of things — only change the size and age cutoffs of the
//! finds it already has.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::model::parse_size;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    /// How big a file in Downloads/Desktop/home must be to be offered as a big old file.
    pub big_file_min_bytes: Option<u64>,
    /// How long it must have sat untouched.
    pub big_file_min_age: Option<Duration>,
    /// The smallest file the duplicate finder bothers comparing.
    pub duplicate_min_size: Option<u64>,
    /// How much a deleted app must have left behind to be worth a row.
    pub leftover_min: Option<u64>,
    /// A simulator smaller than this is the empty one Xcode makes itself.
    pub simulator_min: Option<u64>,
}

/// Where the config lives for a given home folder.
pub fn file(home: &Path) -> PathBuf {
    home.join(".config/begone/config.txt")
}

/// Read and parse the config for a home folder. A missing or unreadable file is simply the defaults.
pub fn load(home: &Path) -> Config {
    match std::fs::read_to_string(file(home)) {
        Ok(text) => parse(&text),
        Err(_) => Config::default(),
    }
}

/// `90d`, `12h`, `2w`, or a bare number of days.
fn parse_age(text: &str) -> Option<Duration> {
    let text = text.trim().to_lowercase();
    let split = text.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let number: f64 = number.parse().ok()?;
    let secs = match unit.trim() {
        "" | "d" | "day" | "days" => number * 86_400.0,
        "h" | "hr" | "hour" | "hours" => number * 3_600.0,
        "w" | "week" | "weeks" => number * 7.0 * 86_400.0,
        _ => return None,
    };
    (secs.is_finite() && (0.0..1e12).contains(&secs)).then(|| Duration::from_secs(secs as u64))
}

fn parse(text: &str) -> Config {
    let mut config = Config::default();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let Some((key, value)) = line.split_once('=') else { continue };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "big-file-size" => config.big_file_min_bytes = parse_size(value),
            "big-file-age" => config.big_file_min_age = parse_age(value),
            "duplicate-min-size" => config.duplicate_min_size = parse_size(value),
            "app-leftover-min" => config.leftover_min = parse_size(value),
            "simulator-min" => config.simulator_min = parse_size(value),
            _ => {}
        }
    }
    config
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_config_reads_the_knobs_it_knows_and_ignores_the_rest() {
        let text = "
            # begone config
            big-file-size = 250 MB
            big-file-age = 30d
            duplicate-min-size = 5mb
            app-leftover-min = 2 MB
            simulator-min = 100MB
            unknown-key = whatever   # ignored
            malformed line without equals
            big-file-size-typo = 1GB # unknown key, ignored
        ";
        let c = parse(text);
        assert_eq!(c.big_file_min_bytes, Some(250_000_000));
        assert_eq!(c.big_file_min_age, Some(Duration::from_secs(30 * 86_400)));
        assert_eq!(c.duplicate_min_size, Some(5_000_000));
        assert_eq!(c.leftover_min, Some(2_000_000));
        assert_eq!(c.simulator_min, Some(100_000_000));
    }

    #[test]
    fn an_empty_or_bad_config_is_just_the_defaults() {
        assert_eq!(parse(""), Config::default());
        assert_eq!(parse("big-file-size = not-a-size\nbig-file-age = 9 parsecs"), Config::default());
        // Ages in days, hours and weeks.
        assert_eq!(parse("big-file-age = 12h").big_file_min_age, Some(Duration::from_secs(12 * 3_600)));
        assert_eq!(parse("big-file-age = 2w").big_file_min_age, Some(Duration::from_secs(14 * 86_400)));
        assert_eq!(parse("big-file-age = 45").big_file_min_age, Some(Duration::from_secs(45 * 86_400)), "a bare number is days");
    }
}
