//! Persists user-configurable defaults to a TOML file on disk
//! (`~/.config/gauge/config.toml` on Linux, `~/Library/Application
//! Support/gauge/config.toml` on macOS — wherever `dirs::config_dir()`
//! points). Two roles:
//!
//! - It's what the Config screen's "Save Profile" (`Ctrl+S`) actually
//!   writes to, and what the TUI pre-fills its Config/Server Mode screens
//!   from on startup.
//! - It's what `gauge client`/`gauge server` on the CLI fall back to for
//!   any flag not given explicitly, before finally falling back to the
//!   same built-in defaults the flag's `--help` text describes.
//!
//! Two independent, entirely optional sections, `[client]` and `[server]`
//! — a missing file, a missing section, or a missing field all just mean
//! "use the next fallback," never an error. Only fields the engine
//! actually acts on are here; see the doc comments on `app.rs`'s
//! `ConfigState`/`ServerConfigState` for which display fields are still
//! cosmetic-only and deliberately left out of both the CLI and this file.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
pub struct ClientDefaults {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub duration_secs: Option<u64>,
    pub parallel: Option<u8>,
    pub udp: Option<bool>,
    /// Use QUIC instead of TCP/UDP (takes precedence over `udp`).
    pub quic: Option<bool>,
    pub reverse: Option<bool>,
    pub bidir: Option<bool>,
    pub bandwidth: Option<String>,
    pub window: Option<String>,
    pub mtu: Option<u32>,
    /// TCP write/read buffer size in bytes (iperf3's `-l`) — distinct from
    /// `window` (the kernel socket buffer).
    pub buffer: Option<String>,
    /// Exclude the first few seconds (ramp-up) from the average/peak
    /// throughput reported at test end. A plain on/off switch, not a
    /// configurable number of seconds — matches the TUI checkbox.
    pub omit: Option<bool>,
    /// Seconds between live interval reports (default 1).
    pub interval_secs: Option<u64>,
    /// IP TOS/DSCP byte to mark packets with, e.g. `184` (= 0xB8, EF).
    pub tos: Option<u8>,
    /// TCP congestion-control algorithm name — Linux only.
    pub congestion: Option<String>,
    pub json: Option<bool>,
}

#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
pub struct ServerDefaults {
    pub bind: Option<String>,
    pub port: Option<u16>,
    pub udp: Option<bool>,
    pub quic: Option<bool>,
    pub window: Option<String>,
    pub json: Option<bool>,
    pub one_off: Option<bool>,
}

#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
pub struct GaugeConfig {
    #[serde(default)]
    pub client: ClientDefaults,
    #[serde(default)]
    pub server: ServerDefaults,
}

#[cfg(not(test))]
fn config_path() -> Option<PathBuf> {
    let mut dir = dirs::config_dir()?;
    dir.push("gauge");
    Some(dir.join("config.toml"))
}

/// Returns the default (empty) config on any failure — no file yet,
/// unreadable, or invalid TOML. A fresh install with no config file is the
/// overwhelmingly common case and must never be treated as an error.
///
/// A no-op (always returns the empty default) under `cfg(test)` — this is
/// called from `App::default()`, which plenty of unit tests construct, and
/// none of them should have their behavior depend on whatever happens to
/// be in the *real* user's config file. `load_from`/`save_to` (below) are
/// what's actually under test, called directly with a temp path by this
/// module's own tests.
#[cfg(not(test))]
pub fn load() -> GaugeConfig {
    match config_path() {
        Some(path) => load_from(&path),
        None => GaugeConfig::default(),
    }
}

#[cfg(test)]
pub fn load() -> GaugeConfig {
    GaugeConfig::default()
}

fn load_from(path: &Path) -> GaugeConfig {
    let Ok(text) = std::fs::read_to_string(path) else { return GaugeConfig::default() };
    toml::from_str(&text).unwrap_or_default()
}

/// Writes the full config (both sections) to disk, creating the parent
/// directory if needed. Returns the path written to, so a caller (e.g. the
/// Config screen's save toast) can tell the user exactly where it went —
/// unlike `store.rs`'s History saves, a config save is a deliberate,
/// user-initiated action, so a failure here is worth surfacing rather than
/// silently swallowing.
///
/// A no-op under `cfg(test)` for the same reason as `load` — see its doc
/// comment. Returns a fixed placeholder path rather than touching disk.
#[cfg(not(test))]
pub fn save(cfg: &GaugeConfig) -> std::io::Result<PathBuf> {
    let path = config_path().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no config directory available on this platform",
        )
    })?;
    save_to(&path, cfg)?;
    Ok(path)
}

#[cfg(test)]
pub fn save(_cfg: &GaugeConfig) -> std::io::Result<PathBuf> {
    Ok(PathBuf::from("<config save is a no-op under cfg(test)>"))
}

fn save_to(path: &Path, cfg: &GaugeConfig) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = toml::to_string_pretty(cfg).map_err(std::io::Error::other)?;
    std::fs::write(path, text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A path under the OS temp dir that's unique per test *and* per call
    /// within a test — same pattern as `store.rs`'s `temp_path`.
    fn temp_path(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!("gauge-config-test-{label}-{}-{n}.toml", std::process::id()))
    }

    #[test]
    fn load_from_missing_file_is_the_empty_default_not_an_error() {
        let cfg = load_from(&temp_path("missing"));
        assert_eq!(cfg, GaugeConfig::default());
    }

    #[test]
    fn load_from_corrupt_file_is_the_empty_default_not_a_panic() {
        let path = temp_path("corrupt");
        std::fs::write(&path, "this is not valid toml {{{").unwrap();

        let cfg = load_from(&path);
        assert_eq!(cfg, GaugeConfig::default());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_then_load_round_trips_both_sections() {
        let path = temp_path("roundtrip");
        let cfg = GaugeConfig {
            client: ClientDefaults {
                host: Some("10.0.0.5".into()),
                port: Some(6000),
                duration_secs: Some(30),
                parallel: Some(4),
                udp: Some(false),
                quic: Some(true),
                reverse: Some(false),
                bidir: Some(false),
                bandwidth: Some("100M".into()),
                window: Some("1M".into()),
                mtu: Some(1500),
                buffer: Some("65536".into()),
                omit: Some(true),
                interval_secs: Some(2),
                tos: Some(0xB8),
                congestion: Some("bbr".into()),
                json: Some(true),
            },
            server: ServerDefaults {
                bind: Some("127.0.0.1".into()),
                port: Some(6001),
                udp: Some(false),
                quic: Some(false),
                window: Some("512K".into()),
                json: Some(false),
                one_off: Some(true),
            },
        };

        save_to(&path, &cfg).unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded, cfg);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_creates_missing_parent_directories() {
        let path = temp_path("newdir")
            .parent()
            .unwrap()
            .join("gauge-config-test-nested")
            .join("config.toml");
        assert!(!path.parent().unwrap().exists());

        save_to(&path, &GaugeConfig::default()).unwrap();
        assert!(path.exists());

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_config_with_only_the_client_section_leaves_server_fields_none() {
        // Hand-written partial config — every field a user would omit by
        // hand should just come back `None`, not fail to parse.
        let path = temp_path("partial");
        std::fs::write(&path, "[client]\nhost = \"10.0.0.9\"\nport = 5555\n").unwrap();

        let cfg = load_from(&path);
        assert_eq!(cfg.client.host.as_deref(), Some("10.0.0.9"));
        assert_eq!(cfg.client.port, Some(5555));
        assert_eq!(cfg.client.duration_secs, None);
        assert_eq!(cfg.server, ServerDefaults::default());

        let _ = std::fs::remove_file(&path);
    }
}
