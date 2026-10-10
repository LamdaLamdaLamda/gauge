//! Appends every Event Log line (Live Test and Server Mode screens) to a
//! plain-text file on disk, so a session's log survives after the TUI
//! closes — handy for looking at what happened after the fact. Best-effort
//! and silent on failure, same convention as `store.rs`'s History saves:
//! losing a log line to a read-only disk or a permissions issue is never
//! worth interrupting the user over.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::util::LogLine;

#[cfg(not(test))]
fn log_path() -> Option<PathBuf> {
    let mut dir = dirs::data_dir()?;
    dir.push("gauge");
    Some(dir.join("gauge.log"))
}

/// Appends one Event Log line to the on-disk log file, tagged with
/// `source` (`"live"` or `"server"`) so entries from both screens can be
/// told apart when both ran interleaved (e.g. testing against your own
/// in-app server in the same session).
///
/// A no-op under `cfg(test)` — `push_log` in `app.rs` is exercised by many
/// unit tests via `LiveState`/`ServerState`, and none of them should be
/// spamming the real user's on-disk log file. `append_to` (below) is what's
/// actually under test, called directly with a temp path by this module's
/// own tests.
#[cfg(not(test))]
pub fn append(source: &str, line: &LogLine) {
    if let Some(path) = log_path() {
        append_to(&path, source, line);
    }
}

#[cfg(test)]
pub fn append(_source: &str, _line: &LogLine) {}

fn append_to(path: &Path, source: &str, line: &LogLine) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else { return };
    let _ = writeln!(file, "{} [{source}] [{}] {}", now_date_hms(), line.level.label(), line.text);
}

/// `YYYY-MM-DD HH:MM:SS`, UTC — matches the (also UTC, unlabeled) clock
/// already shown in the Event Log itself (`util::now_hms`); the date is
/// added here because unlike the in-app log, this file is meant to
/// accumulate across multiple days of runs.
fn now_date_hms() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (y, m, d) = civil_from_days((secs / 86400) as i64);
    let secs_of_day = secs % 86400;
    let (h, mi, s) = (secs_of_day / 3600, (secs_of_day / 60) % 60, secs_of_day % 60);
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

/// Days-since-Unix-epoch → (year, month, day) in the proleptic Gregorian
/// calendar. A small, well-known public-domain algorithm (Howard Hinnant's
/// `civil_from_days`) — used instead of pulling in a datetime crate just to
/// stamp log lines with a date.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::LogLevel;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A path under the OS temp dir that's unique per test *and* per call
    /// within a test — same pattern as `store.rs`'s `temp_path`.
    fn temp_path(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!("gauge-filelog-test-{label}-{}-{n}.log", std::process::id()))
    }

    #[test]
    fn civil_from_days_matches_known_reference_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1)); // the Unix epoch itself
        assert_eq!(civil_from_days(19_723), (2024, 1, 1)); // a known leap year boundary
        assert_eq!(civil_from_days(19_723 + 31 + 28), (2024, 2, 29)); // 2024 is a leap year
    }

    #[test]
    fn append_creates_missing_parent_directories() {
        let path = temp_path("newdir")
            .parent()
            .unwrap()
            .join("gauge-filelog-test-nested")
            .join("gauge.log");
        assert!(!path.parent().unwrap().exists());

        append_to(&path, "live", &LogLine::new("12:00:00", LogLevel::Info, "hello"));

        assert!(path.exists());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn append_writes_one_readable_line_with_source_level_and_text() {
        let path = temp_path("oneline");

        append_to(&path, "server", &LogLine::new("12:00:00", LogLevel::Ok, "gauge server started"));

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("[server]"), "line should be tagged with its source: {content}");
        assert!(content.contains("[OK]"), "line should carry the log level: {content}");
        assert!(content.contains("gauge server started"));
        assert!(content.ends_with('\n'));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn append_is_append_only_across_multiple_calls() {
        let path = temp_path("multi");

        append_to(&path, "live", &LogLine::new("12:00:00", LogLevel::Info, "first"));
        append_to(&path, "live", &LogLine::new("12:00:01", LogLevel::Info, "second"));

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("first"));
        assert!(lines[1].contains("second"));

        let _ = std::fs::remove_file(&path);
    }
}
