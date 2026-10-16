//! `--older-than` / `--newer-than`: filtering files by modification time.
//!
//! An age is a whole number followed by a unit: `30s`, `15min`, `12h`, `7d`,
//! `2w`, `6mo` (30 days), `1y` (365 days). A bare `m` is rejected because it
//! could mean minutes or months.
//!
//! Both filters compare a file's mtime with one reference time, taken once
//! when the tool starts:
//!
//! * `--older-than A` keeps files modified at or before `now - A`;
//! * `--newer-than B` keeps files modified after `now - B`.
//!
//! So for the same age, the two options split all files exactly in two.
//! Given together, a file must satisfy both, which only works when `A < B`.

use std::time::{Duration, SystemTime};

const MINUTE: u64 = 60;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;

/// Longest accepted age. Keeps `now - age` representable everywhere.
const MAX_SECS: u64 = 1000 * 365 * DAY;

/// An age as given on the command line, with its meaning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Age {
    /// The text as typed (trimmed), echoed in JSON.
    pub text: String,
    pub duration: Duration,
}

/// Parses an age such as `7d` or `1y`. See the module docs for the units.
pub fn parse_age(s: &str) -> Result<Age, String> {
    let text = s.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| format!("missing unit in '{text}'; use e.g. 30d, 12h or 1y"))?;
    let (number, unit) = text.split_at(split);
    if number.is_empty() {
        return Err(format!(
            "invalid age '{text}': expected a whole number followed by a unit, e.g. 7d"
        ));
    }
    let value: u64 = number
        .parse()
        .map_err(|_| format!("number too large in '{text}'"))?;
    let unit_secs = match unit.trim().to_ascii_lowercase().as_str() {
        "s" | "sec" => 1,
        "min" => MINUTE,
        "h" => HOUR,
        "d" => DAY,
        "w" => 7 * DAY,
        "mo" => 30 * DAY,
        "y" => 365 * DAY,
        "m" => {
            return Err(format!(
                "ambiguous unit 'm' in '{text}': use 'min' for minutes or 'mo' for months"
            ));
        }
        other => {
            return Err(format!(
                "unknown unit '{other}' in '{text}'; use s, min, h, d, w, mo or y"
            ));
        }
    };
    let secs = value
        .checked_mul(unit_secs)
        .filter(|&s| s <= MAX_SECS)
        .ok_or_else(|| format!("age '{text}' is too large (at most 1000y)"))?;
    Ok(Age {
        text: text.to_string(),
        duration: Duration::from_secs(secs),
    })
}

/// `--older-than` / `--newer-than`, resolved against one reference time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MtimeFilter {
    /// `--older-than`: keep files modified at or before this instant.
    pub modified_at_or_before: Option<SystemTime>,
    /// `--newer-than`: keep files modified strictly after this instant.
    pub modified_after: Option<SystemTime>,
}

impl MtimeFilter {
    /// Resolves the ages against `now`.
    pub fn new(
        now: SystemTime,
        older_than: Option<Duration>,
        newer_than: Option<Duration>,
    ) -> Self {
        let back = |d: Duration| now.checked_sub(d).unwrap_or(SystemTime::UNIX_EPOCH);
        MtimeFilter {
            modified_at_or_before: older_than.map(back),
            modified_after: newer_than.map(back),
        }
    }

    pub fn is_active(&self) -> bool {
        self.modified_at_or_before.is_some() || self.modified_after.is_some()
    }

    /// Whether a file with modification time `mtime` passes. A file whose
    /// mtime cannot be read never passes an active filter.
    pub fn matches(&self, mtime: Option<SystemTime>) -> bool {
        if !self.is_active() {
            return true;
        }
        let Some(t) = mtime else { return false };
        self.modified_at_or_before.is_none_or(|limit| t <= limit)
            && self.modified_after.is_none_or(|limit| t > limit)
    }
}

/// Checks that `--older-than` and `--newer-than` given together leave a
/// non-empty window.
pub fn check_window(older_than: Option<&Age>, newer_than: Option<&Age>) -> Result<(), String> {
    match (older_than, newer_than) {
        (Some(o), Some(n)) if o.duration >= n.duration => Err(format!(
            "no file can be older than {} and newer than {} at the same time; \
             --older-than must be shorter than --newer-than (e.g. --newer-than 1y --older-than 30d)",
            o.text, n.text
        )),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: &str) -> u64 {
        parse_age(s).unwrap().duration.as_secs()
    }

    #[test]
    fn units() {
        assert_eq!(secs("30s"), 30);
        assert_eq!(secs("15min"), 15 * 60);
        assert_eq!(secs("12h"), 12 * 3600);
        assert_eq!(secs("7d"), 7 * 86_400);
        assert_eq!(secs("2w"), 14 * 86_400);
        assert_eq!(secs("6mo"), 180 * 86_400);
        assert_eq!(secs("1y"), 365 * 86_400);
        assert_eq!(secs(" 1Y "), 365 * 86_400);
        assert_eq!(secs("0d"), 0);
        assert_eq!(secs("1000y"), MAX_SECS);
        assert_eq!(parse_age(" 7d ").unwrap().text, "7d");
    }

    #[test]
    fn rejects_bad_input() {
        for bad in [
            "",
            "7",
            "d",
            "-1d",
            "1.5d",
            "7 days",
            "1m",
            "1M",
            "7x",
            "1001y",
            "99999999999999999999d",
        ] {
            assert!(parse_age(bad).is_err(), "{bad:?} should be rejected");
        }
        assert!(parse_age("1m").unwrap_err().contains("'min'"));
    }

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn older_and_newer_split_files_exactly_in_two() {
        let now = at(1_000_000);
        let age = Duration::from_secs(100);
        let older = MtimeFilter::new(now, Some(age), None);
        let newer = MtimeFilter::new(now, None, Some(age));
        for t in [999_800, 999_899, 999_900, 999_901, 1_000_000, 1_000_500] {
            let m = Some(at(t));
            assert_ne!(older.matches(m), newer.matches(m), "t={t}");
        }
        // The boundary itself is "older".
        assert!(older.matches(Some(at(999_900))));
    }

    #[test]
    fn window_and_unreadable_mtime() {
        let now = at(1_000_000);
        let f = MtimeFilter::new(
            now,
            Some(Duration::from_secs(10)),
            Some(Duration::from_secs(100)),
        );
        assert!(f.matches(Some(at(999_950))));
        assert!(!f.matches(Some(at(999_995))));
        assert!(!f.matches(Some(at(999_000))));
        assert!(!f.matches(None));
        assert!(
            MtimeFilter::default().matches(None),
            "inactive filter passes all"
        );
    }

    #[test]
    fn future_mtimes_are_new() {
        let now = at(1_000);
        let newer = MtimeFilter::new(now, None, Some(Duration::from_secs(1)));
        assert!(newer.matches(Some(at(5_000))));
    }

    #[test]
    fn window_check() {
        let a = |s| parse_age(s).unwrap();
        assert!(check_window(Some(&a("30d")), Some(&a("1y"))).is_ok());
        assert!(check_window(Some(&a("1y")), Some(&a("30d"))).is_err());
        assert!(check_window(Some(&a("7d")), Some(&a("1w"))).is_err());
        assert!(check_window(Some(&a("7d")), None).is_ok());
        assert!(check_window(None, None).is_ok());
    }
}
