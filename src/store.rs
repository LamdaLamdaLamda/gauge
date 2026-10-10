//! Persists the History screen's completed runs to disk as JSON, so they
//! survive an app restart. Small and synchronous by design — it's read
//! once at startup (before the render loop exists) and written after each
//! completed test or delete, both cheap for a file this size.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A completed real test run, recorded from the client side (the side that
/// initiated it) once all its streams finish. TX (`peak_gbps`/`avg_gbps`)
/// is always present; RX is `Some` only for Rx/Bidir tests — plain TX-only
/// runs (still the default) leave it `None` rather than a misleading 0.0.
///
/// Field names are part of the on-disk JSON format — rename with care (or
/// add a migration) if it ever needs to change. The RX fields use
/// `#[serde(default)]` so History files written before bidirectional
/// support existed still load fine (they just have no RX data).
#[derive(Serialize, Deserialize)]
pub struct HistoryRun {
    pub target: String,
    pub tag: String,
    pub tag_color_idx: usize,
    pub summary: String,
    pub time: String,
    pub peak_gbps: f64,
    pub avg_gbps: f64,
    #[serde(default)]
    pub peak_rx_gbps: Option<f64>,
    #[serde(default)]
    pub avg_rx_gbps: Option<f64>,
    pub rtt_ms: f64,
    pub duration_secs: u64,
    pub streams: u8,
    pub window: String,
    pub mtu: u32,
    /// Cumulative TCP retransmit count observed over the run (0 for UDP,
    /// or on platforms without a live retransmit accessor). Added after
    /// the initial release, so older history entries default to 0.
    #[serde(default)]
    pub retransmits: u64,
    /// TX throughput samples (elapsed_secs, aggregate Gbit/s), 1s intervals.
    pub chart: Vec<(f64, f64)>,
    /// RX throughput samples, same shape as `chart` — empty for TX-only
    /// runs. Added after the initial release (which only ever recorded
    /// TX), so older history entries default to empty rather than failing
    /// to load.
    #[serde(default)]
    pub chart_rx: Vec<(f64, f64)>,
    /// The test-tuning options the run was started with. `None` for runs
    /// recorded before these were saved.
    #[serde(default)]
    pub settings: Option<RunSettings>,
}

/// The options of a run that change its result but aren't visible in the
/// run's tag/header (protocol, direction, streams, window and MTU already
/// are). Only options that applied to the run's protocol are `Some`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RunSettings {
    /// TCP only: size of each write()/read() call. `None` = engine default.
    pub buffer_bytes: Option<u32>,
    /// UDP only: per-stream target rate. `None` = unthrottled.
    pub bandwidth_bps: Option<u64>,
    /// Whether the first seconds were excluded from avg/peak.
    pub omit: bool,
    pub interval_secs: u64,
    pub tos: Option<u8>,
    pub congestion: Option<String>,
}

fn human(value: u64, unit_step: u64, suffixes: [&str; 4]) -> String {
    let mut v = value;
    let mut i = 0;
    while v >= unit_step && v.is_multiple_of(unit_step) && i < suffixes.len() - 1 {
        v /= unit_step;
        i += 1;
    }
    format!("{v}{}", suffixes[i])
}

impl RunSettings {
    /// One-line summary for the History header, e.g.
    /// `buffer 128K · omit 3s · interval 1s · TOS 0xb8`.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(b) = self.buffer_bytes {
            parts.push(format!("buffer {}", human(b as u64, 1024, ["B", "K", "M", "G"])));
        }
        if let Some(bps) = self.bandwidth_bps {
            parts.push(format!("target {}bit/s", human(bps, 1000, ["", "K", "M", "G"])));
        }
        parts.push(if self.omit { "omit 3s".into() } else { "no omit".into() });
        parts.push(format!("interval {}s", self.interval_secs.max(1)));
        if let Some(t) = self.tos.filter(|t| *t != 0) {
            parts.push(format!("TOS {t:#04x}"));
        }
        if let Some(c) = &self.congestion {
            parts.push(format!("congestion {c}"));
        }
        parts.join(" · ")
    }
}

/// Keep the file from growing without bound over a long-lived install.
const MAX_RUNS: usize = 50;

fn history_path() -> Option<PathBuf> {
    let mut dir = dirs::data_dir()?;
    dir.push("gauge");
    Some(dir.join("history.json"))
}

/// Returns an empty list on any failure (no file yet, unreadable, corrupt
/// JSON) — a fresh, empty History screen is a fine fallback, never a crash.
pub fn load() -> Vec<HistoryRun> {
    match history_path() {
        Some(path) => load_from(&path),
        None => Vec::new(),
    }
}

fn load_from(path: &Path) -> Vec<HistoryRun> {
    let Ok(bytes) = std::fs::read(path) else { return Vec::new() };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

/// Best-effort save; a failure here (e.g. read-only disk) is silently
/// swallowed rather than surfaced mid-test — losing the history entry is
/// better than interrupting the user over it.
pub fn save(runs: &[HistoryRun]) {
    if let Some(path) = history_path() {
        save_to(&path, runs);
    }
}

fn save_to(path: &Path, runs: &[HistoryRun]) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let trimmed = &runs[..runs.len().min(MAX_RUNS)];
    if let Ok(bytes) = serde_json::to_vec_pretty(trimmed) {
        let _ = std::fs::write(path, bytes);
    }
}

/// Loads history, prepends `run`, and saves — for callers with no
/// long-lived in-memory `HistoryState` to append to (the CLI's `client`
/// subcommand records a run this way; the TUI's `HistoryState::push_run`
/// already holds the list in memory and just re-saves it directly).
pub fn record_run(run: HistoryRun) {
    let mut runs = load();
    runs.insert(0, run);
    save(&runs);
}

/// User-initiated export (the `e` key on Live Test / History) — writes to
/// the current working directory rather than the app's own data dir, so
/// it lands somewhere the user can find without hunting for it.
pub fn export_json<T: Serialize>(filename: &str, value: &T) -> std::io::Result<PathBuf> {
    let path = PathBuf::from(filename);
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&path, &bytes)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A path under the OS temp dir that's unique per test *and* per call
    /// within a test, so parallel `cargo test` runs and repeated calls in
    /// one test never collide.
    fn temp_path(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!("gauge-store-test-{label}-{}-{n}.json", std::process::id()))
    }

    fn sample_run(target: &str) -> HistoryRun {
        HistoryRun {
            target: target.into(),
            tag: "TCP TX ×1".into(),
            tag_color_idx: 0,
            summary: "↑1.23 Gbit/s avg · peak 2.34".into(),
            time: "12:00".into(),
            peak_gbps: 2.34,
            avg_gbps: 1.23,
            peak_rx_gbps: None,
            avg_rx_gbps: None,
            rtt_ms: 0.5,
            duration_secs: 10,
            streams: 1,
            window: "256K".into(),
            mtu: 9000,
            retransmits: 3,
            chart: vec![(0.0, 0.5), (1.0, 1.2)],
            chart_rx: Vec::new(),
            settings: Some(RunSettings {
                buffer_bytes: Some(131072),
                bandwidth_bps: None,
                omit: true,
                interval_secs: 2,
                tos: Some(0xB8),
                congestion: Some("bbr".into()),
            }),
        }
    }

    #[test]
    fn load_from_missing_file_is_empty_not_an_error() {
        let path = temp_path("missing");
        assert!(load_from(&path).is_empty());
    }

    #[test]
    fn load_from_corrupt_file_is_empty_not_a_panic() {
        let path = temp_path("corrupt");
        std::fs::write(&path, b"{ this is not valid json ]").unwrap();
        assert!(load_from(&path).is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_then_load_round_trips() {
        let path = temp_path("roundtrip");
        let runs = vec![sample_run("10.0.0.1:5201"), sample_run("10.0.0.2:5201")];
        save_to(&path, &runs);

        let loaded = load_from(&path);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].target, "10.0.0.1:5201");
        assert_eq!(loaded[1].target, "10.0.0.2:5201");
        assert_eq!(loaded[0].chart, vec![(0.0, 0.5), (1.0, 1.2)]);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_then_load_round_trips_the_rx_chart_for_bidir_runs() {
        let path = temp_path("rxchart");
        let mut run = sample_run("10.0.0.1:5201");
        run.chart_rx = vec![(0.0, 0.8), (1.0, 0.9)];
        save_to(&path, &[run]);

        let loaded = load_from(&path);
        assert_eq!(loaded[0].chart_rx, vec![(0.0, 0.8), (1.0, 0.9)]);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_creates_missing_parent_directories() {
        let mut path = std::env::temp_dir();
        path.push(format!("gauge-store-test-nested-{}", std::process::id()));
        path.push("subdir");
        path.push("history.json");
        assert!(!path.parent().unwrap().exists());

        save_to(&path, &[sample_run("192.168.1.1:5201")]);
        assert!(path.exists());

        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn save_trims_to_max_runs() {
        let path = temp_path("trim");
        let runs: Vec<HistoryRun> =
            (0..(MAX_RUNS + 10)).map(|i| sample_run(&format!("host-{i}"))).collect();
        save_to(&path, &runs);

        let loaded = load_from(&path);
        assert_eq!(loaded.len(), MAX_RUNS);
        assert_eq!(
            loaded[0].target, "host-0",
            "should keep the front of the list, not trim from there"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn export_json_writes_readable_pretty_json() {
        let path = temp_path("export");
        let filename = path.to_str().unwrap();
        let run = sample_run("export-target:5201");

        let written_path = export_json(filename, &run).unwrap();
        assert_eq!(written_path, path);

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("export-target:5201"));
        assert!(contents.contains('\n'), "expected pretty-printed (multi-line) JSON");

        let decoded: HistoryRun = serde_json::from_str(&contents).unwrap();
        assert_eq!(decoded.target, "export-target:5201");
        assert_eq!(decoded.peak_gbps, run.peak_gbps);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn old_history_json_without_rx_fields_still_loads() {
        // A file written before bidirectional support existed — no
        // peak_rx_gbps/avg_rx_gbps keys at all.
        let old_json = r#"[{
            "target": "10.0.0.1:5201",
            "tag": "TCP TX ×1",
            "tag_color_idx": 0,
            "summary": "↑1.00 Gbit/s avg · peak 1.10",
            "time": "12:00",
            "peak_gbps": 1.1,
            "avg_gbps": 1.0,
            "rtt_ms": 0.3,
            "duration_secs": 10,
            "streams": 1,
            "window": "256K",
            "mtu": 9000,
            "chart": []
        }]"#;
        let path = temp_path("legacy");
        std::fs::write(&path, old_json).unwrap();

        let loaded = load_from(&path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].peak_gbps, 1.1);
        assert_eq!(loaded[0].peak_rx_gbps, None);
        assert_eq!(loaded[0].avg_rx_gbps, None);
        assert_eq!(
            loaded[0].retransmits, 0,
            "retransmits didn't exist in old files — must default to 0, not error"
        );
        assert_eq!(
            loaded[0].chart_rx,
            Vec::<(f64, f64)>::new(),
            "chart_rx didn't exist in old files — must default to empty, not error"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn settings_survive_a_save_load_round_trip() {
        let path = temp_path("settings");
        save_to(&path, &[sample_run("a:1")]);
        let loaded = load_from(&path);
        assert_eq!(loaded[0].settings, sample_run("a:1").settings);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn old_history_json_without_settings_still_loads() {
        let mut v = serde_json::to_value(sample_run("a:1")).unwrap();
        v.as_object_mut().unwrap().remove("settings");
        let run: HistoryRun = serde_json::from_value(v).unwrap();
        assert!(run.settings.is_none());
    }

    #[test]
    fn describe_lists_only_what_applied() {
        let tcp = RunSettings {
            buffer_bytes: Some(131072),
            omit: true,
            interval_secs: 1,
            ..Default::default()
        };
        assert_eq!(tcp.describe(), "buffer 128K · omit 3s · interval 1s");

        let udp = RunSettings {
            bandwidth_bps: Some(100_000_000),
            interval_secs: 5,
            tos: Some(0xB8),
            congestion: Some("bbr".into()),
            ..Default::default()
        };
        assert_eq!(
            udp.describe(),
            "target 100Mbit/s · no omit · interval 5s · TOS 0xb8 · congestion bbr"
        );
    }
}
