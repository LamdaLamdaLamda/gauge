use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::config::{self, ClientDefaults, GaugeConfig, ServerDefaults};
use crate::net::{self, ClientParams, Direction, Flow, NetCommand, NetEvent, Proto, ServerParams};
use crate::store::{HistoryRun, RunSettings};
use crate::util::{LogLevel, LogLine, now_hms};

/// How many interval samples the sparkline history buffers hold — needs to
/// comfortably exceed the widest realistic terminal width in columns
/// (~1 sample/column at render time, see `ui/live.rs::draw_metric`), or the
/// buffer runs out of history before the sparkline widget runs out of
/// space to draw it in.
const SPARK_LEN: usize = 300;
const LOG_MAX: usize = 60;
/// How many seconds of ramp-up the "Omit first 3s" checkbox / `--omit` flag
/// excludes from the average/peak throughput reported at test end — a
/// fixed value, not user-adjustable, matching the checkbox's own label.
/// Omit only applies to tests that run longer than this — see
/// `LiveState::rate_summary`.
pub const OMIT_SECS: u64 = 3;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Live,
    Config,
    History,
    Server,
}

impl Tab {
    pub const ALL: [Tab; 4] = [Tab::Live, Tab::Config, Tab::History, Tab::Server];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Live => "Live Test",
            Tab::Config => "Config",
            Tab::History => "History",
            Tab::Server => "Server Mode",
        }
    }

    pub fn index(self) -> usize {
        Tab::ALL.iter().position(|t| *t == self).unwrap()
    }

    pub fn from_index(i: usize) -> Tab {
        Tab::ALL[i % Tab::ALL.len()]
    }
}

pub struct StreamRow {
    pub id: u8,
    pub tx: f64,
    pub rx: f64,
    pub rtt: f64,
    pub retrans: u32,
    pub cwnd: u32,
    pub loss: f64,
    /// Receiver-measured UDP jitter (RFC 3550) — only streams whose Rx
    /// side this client measures (UDP reverse/bidir) ever have one.
    pub jitter_ms: Option<f64>,
    pub degraded: bool,
}

/// Live Test screen state. Idle until a test is started (from Config via F5,
/// or `r` to restart); from then on it is driven entirely by `NetEvent`s
/// coming off the real network engine — no more simulated jitter.
pub struct LiveState {
    pub running: bool,
    pub connecting: bool,
    pub error: Option<String>,
    pub target: String,
    pub proto: String,
    pub direction: Direction,
    pub streams: u8,
    pub window: String,
    pub mtu: u32,
    pub tx: f64,
    pub rx: f64,
    pub lat: f64,
    pub tx_hist: VecDeque<u64>,
    pub rx_hist: VecDeque<u64>,
    pub lat_hist: VecDeque<u64>,
    /// Mean receiver-measured jitter across streams, latest sample and its
    /// history (0.01 ms units) — `None` until a stream reports one.
    pub jitter_ms: Option<f64>,
    pub jitter_hist: VecDeque<u64>,
    pub rows: Vec<StreamRow>,
    /// How many `NetEvent::ClientStream*Finished` events (one per flow —
    /// Bidir means two, Tx/Rx mean one) are expected before the test as a
    /// whole is done.
    expected_finishes: u8,
    streams_finished: u8,
    /// Exact byte totals from each flow's `ClientStreamFinished` event,
    /// accumulated as they arrive. `tx_total_gb`/`rx_total_gb` are the
    /// live, interval-sampled approximation shown *during* the test; once
    /// every flow has finished we overwrite them with these exact sums —
    /// interval sampling misses the last fractional second of data between
    /// the final periodic report and the stream actually ending.
    tx_finished_bytes: u64,
    rx_finished_bytes: u64,
    pub started_at: Option<Instant>,
    pub tx_total_gb: f64,
    pub rx_total_gb: f64,
    pub peak_tx_gbps: f64,
    pub peak_rx_gbps: f64,
    /// The same peaks, but only over samples after the omit window — used
    /// instead of the above when omit applies (`rate_summary`).
    peak_tx_after_omit: f64,
    peak_rx_after_omit: f64,
    /// Set at test start from the Config screen's "Omit first 3s" checkbox
    /// (or `--omit`). When true, `peak_tx_gbps`/`peak_rx_gbps` stop
    /// updating during the first `OMIT_SECS`, and `avg_tx_gbps`/
    /// `avg_rx_gbps` (below) are computed from bytes seen *after* that
    /// window instead of the full total — `tx_total_gb`/`rx_total_gb`
    /// themselves are never adjusted, so "Total TX/RX" always shows the
    /// true, complete byte count regardless of this setting.
    omit_enabled: bool,
    /// End of the previous aggregate sample per flow (TX, RX) — the start
    /// of the next log line's span, which differs from `elapsed - interval`
    /// for the shorter final sample.
    last_sample_secs: [u64; 2],
    tx_bytes_after_omit: u64,
    rx_bytes_after_omit: u64,
    /// The tuning options this test started with, stored in its History
    /// record. `None` before any test has started.
    settings: Option<RunSettings>,
    /// Aggregate throughput samples (elapsed_secs, gbps), gated to stream 0
    /// like the sparkline/log line — feeds the History screen's chart once
    /// the test completes.
    pub tx_chart: Vec<(f64, f64)>,
    pub rx_chart: Vec<(f64, f64)>,
    pub log: VecDeque<LogLine>,
    pub toast: Option<String>,
    pub stop_flag: Option<Arc<AtomicBool>>,
}

impl Default for LiveState {
    fn default() -> Self {
        Self {
            running: false,
            connecting: false,
            error: None,
            target: String::new(),
            proto: "TCP".into(),
            direction: Direction::Tx,
            streams: 0,
            window: String::new(),
            mtu: 0,
            tx: 0.0,
            rx: 0.0,
            lat: 0.0,
            // Deliberately empty, not pre-filled with zeros — the sparkline
            // should visibly grow from nothing at the left edge as real
            // samples arrive, not appear to have (fake) history already.
            // See `push_hist`'s doc comment for how it fills up over time.
            tx_hist: VecDeque::new(),
            rx_hist: VecDeque::new(),
            lat_hist: VecDeque::new(),
            jitter_ms: None,
            jitter_hist: VecDeque::new(),
            rows: Vec::new(),
            expected_finishes: 0,
            streams_finished: 0,
            tx_finished_bytes: 0,
            rx_finished_bytes: 0,
            started_at: None,
            tx_total_gb: 0.0,
            rx_total_gb: 0.0,
            peak_tx_gbps: 0.0,
            peak_rx_gbps: 0.0,
            peak_tx_after_omit: 0.0,
            peak_rx_after_omit: 0.0,
            omit_enabled: false,
            last_sample_secs: [0; 2],
            settings: None,
            tx_bytes_after_omit: 0,
            rx_bytes_after_omit: 0,
            tx_chart: Vec::new(),
            rx_chart: Vec::new(),
            log: VecDeque::new(),
            toast: None,
            stop_flag: None,
        }
    }
}

pub enum LiveAction {
    None,
    Stop,
    Restart,
}

/// Everything `LiveState::connecting` needs to set up a fresh "dialing"
/// state — bundled so the constructor's parameter list doesn't grow every
/// time a new per-test setting (like `omit_enabled`) is added.
pub struct ConnectingParams {
    pub target: String,
    pub proto: String,
    pub direction: Direction,
    pub stream_count: u8,
    pub window: String,
    pub mtu: u32,
    pub omit_enabled: bool,
    pub settings: RunSettings,
}

impl LiveState {
    /// Resets to a fresh "dialing" state for a new, possibly multi-stream,
    /// possibly bidirectional test.
    fn connecting(p: ConnectingParams, stop: Arc<AtomicBool>) -> Self {
        let ConnectingParams {
            target,
            proto,
            direction,
            stream_count,
            window,
            mtu,
            omit_enabled,
            settings,
        } = p;
        let rows = (0..stream_count)
            .map(|i| StreamRow {
                id: i + 1,
                tx: 0.0,
                rx: 0.0,
                rtt: 0.0,
                retrans: 0,
                cwnd: 0,
                loss: 0.0,
                jitter_ms: None,
                degraded: false,
            })
            .collect();
        let mut s = Self {
            running: false,
            connecting: true,
            target: target.clone(),
            proto,
            direction,
            streams: stream_count,
            window,
            mtu,
            rows,
            expected_finishes: stream_count * direction.flow_count(),
            started_at: Some(Instant::now()),
            omit_enabled,
            settings: Some(settings),
            stop_flag: Some(stop),
            ..Self::default()
        };
        let dir_label = match direction {
            Direction::Tx => "TX",
            Direction::Rx => "RX",
            Direction::Bidir => "bidir",
        };
        s.push_log(
            LogLevel::Info,
            format!("connecting to {target} — {stream_count} stream(s), {dir_label} …"),
        );
        s
    }

    fn push_log(&mut self, level: LogLevel, text: impl Into<String>) {
        let line = LogLine::new(now_hms(), level, text);
        crate::filelog::append("live", &line);
        self.log.push_front(line);
        while self.log.len() > LOG_MAX {
            self.log.pop_back();
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> LiveAction {
        match key.code {
            KeyCode::Char('s') => {
                if self.running || self.connecting {
                    LiveAction::Stop
                } else {
                    self.toast = Some("no test running".into());
                    LiveAction::None
                }
            }
            KeyCode::Char('r') => LiveAction::Restart,
            KeyCode::Char('e') => {
                if self.target.is_empty() {
                    self.toast = Some("no test to export yet".into());
                } else {
                    self.toast = Some(
                        match crate::store::export_json("gauge-export.json", &self.snapshot()) {
                            Ok(path) => format!("exported to {}", path.display()),
                            Err(e) => format!("export failed: {e}"),
                        },
                    );
                }
                LiveAction::None
            }
            _ => LiveAction::None,
        }
    }

    /// Whether the "Omit first 3s" setting actually takes effect for a run
    /// that lasted `elapsed` seconds: a test that ends within the omit
    /// window (short duration, or stopped early) would otherwise be left
    /// with no data to average, so it is measured in full instead.
    fn omit_applies(&self, elapsed: f64) -> bool {
        self.omit_enabled && elapsed > OMIT_SECS as f64
    }

    /// `(avg_tx, avg_rx, peak_tx, peak_rx)` in Gbit/s. With omit in effect,
    /// the averages cover only the bytes/time after the ramp-up window and
    /// the peaks only the samples after it; the true totals
    /// (`tx_total_gb`/`rx_total_gb`) are never adjusted.
    fn rate_summary(&self, elapsed: f64) -> (f64, f64, f64, f64) {
        if self.omit_applies(elapsed) {
            let effective_elapsed = (elapsed - OMIT_SECS as f64).max(0.001);
            (
                self.tx_bytes_after_omit as f64 * 8.0 / 1e9 / effective_elapsed,
                self.rx_bytes_after_omit as f64 * 8.0 / 1e9 / effective_elapsed,
                self.peak_tx_after_omit,
                self.peak_rx_after_omit,
            )
        } else {
            (
                self.tx_total_gb * 8.0 / elapsed,
                self.rx_total_gb * 8.0 / elapsed,
                self.peak_tx_gbps,
                self.peak_rx_gbps,
            )
        }
    }

    /// Builds a `HistoryRun`-shaped snapshot of the current state — used for
    /// the manual JSON export and, on completion, for the History record.
    /// Safe to call mid-test: it just reflects the numbers so far.
    fn snapshot(&self) -> HistoryRun {
        let elapsed = self.started_at.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0).max(0.001);
        let (avg_tx_gbps, avg_rx_gbps, peak_tx_gbps, peak_rx_gbps) = self.rate_summary(elapsed);
        let dir_label = match self.direction {
            Direction::Tx => "TX",
            Direction::Rx => "RX",
            Direction::Bidir => "bidir",
        };
        let summary = match self.direction {
            Direction::Bidir => format!("↑{avg_tx_gbps:.2} / ↓{avg_rx_gbps:.2} Gbit/s avg"),
            Direction::Rx => format!("↓{avg_rx_gbps:.2} Gbit/s avg · peak {peak_rx_gbps:.2}"),
            Direction::Tx => format!("↑{avg_tx_gbps:.2} Gbit/s avg · peak {peak_tx_gbps:.2}"),
        };
        HistoryRun {
            target: self.target.clone(),
            tag: format!("{} {dir_label} ×{}", self.proto, self.streams),
            tag_color_idx: match self.proto.as_str() {
                "UDP" => 1,
                "QUIC" => 2,
                _ => 0,
            },
            summary,
            time: now_hms().chars().take(5).collect(),
            peak_gbps: peak_tx_gbps,
            avg_gbps: avg_tx_gbps,
            peak_rx_gbps: (self.direction != Direction::Tx).then_some(peak_rx_gbps),
            avg_rx_gbps: (self.direction != Direction::Tx).then_some(avg_rx_gbps),
            rtt_ms: self.lat,
            duration_secs: elapsed.round() as u64,
            streams: self.streams,
            window: self.window.clone(),
            mtu: self.mtu,
            retransmits: self.rows.iter().map(|r| r.retrans as u64).sum(),
            chart: self.tx_chart.clone(),
            chart_rx: self.rx_chart.clone(),
            // Record what was actually applied: a too-short run ignores omit.
            settings: self.settings.clone().map(|mut s| {
                s.omit = self.omit_applies(elapsed);
                s
            }),
        }
    }
}

/// Appends one sample to a sparkline history buffer. Buffers start empty
/// (see `LiveState::default()`), so the sparkline genuinely grows from
/// nothing at its left edge as real samples arrive — this only starts
/// evicting the oldest sample once there are more than `SPARK_LEN` of
/// them, at which point the widget's render-time windowing (see
/// `ui/live.rs::draw_metric`) naturally shows it scrolling instead.
fn push_hist(hist: &mut VecDeque<u64>, v: u64) {
    hist.push_back(v);
    while hist.len() > SPARK_LEN {
        hist.pop_front();
    }
}

pub const PROTOCOLS: [&str; 3] = ["TCP", "UDP", "QUIC"];
pub const WINDOWS: [&str; 5] = ["64K", "128K", "256K", "512K", "auto"];
pub const MTUS: [&str; 3] = ["1500", "4096", "9000"];
pub const DIRECTIONS: [&str; 3] = ["TX + RX (bidir)", "TX only", "RX only"];
pub const TOS: [&str; 4] =
    ["Best Effort (0x00)", "AF41 — Video (0x88)", "EF — Voice (0xB8)", "CS6 — Routing (0xC0)"];
/// The TOS byte each `TOS` entry stands for (same order).
pub const TOS_BYTES: [u8; 4] = [0x00, 0x88, 0xB8, 0xC0];
/// "default" leaves the OS's own algorithm alone; the rest are passed to
/// `TCP_CONGESTION` verbatim (Linux only).
pub const CONGESTION: [&str; 4] = ["default", "cubic", "bbr", "reno"];

/// The longest interval we accept — anything bigger leaves a short test
/// with no live samples at all.
pub const MAX_INTERVAL_SECS: u64 = 60;

/// Parses the "Interval (s)" field: a whole number of seconds, 1..=60.
pub fn parse_interval(s: &str) -> Option<u64> {
    s.trim().parse().ok().filter(|v| (1..=MAX_INTERVAL_SECS).contains(v))
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Host,
    Port,
    Protocol,
    Duration,
    Parallel,
    Window,
    Mtu,
    Direction,
    Interval,
    Bandwidth,
    Tos,
    Congestion,
    Buffer,
    Json,
    Reverse,
    Omit,
}

impl Field {
    pub const ALL: [Field; 16] = [
        Field::Host,
        Field::Port,
        Field::Protocol,
        Field::Duration,
        Field::Parallel,
        Field::Window,
        Field::Mtu,
        Field::Direction,
        Field::Interval,
        Field::Bandwidth,
        Field::Tos,
        Field::Congestion,
        Field::Buffer,
        Field::Json,
        Field::Reverse,
        Field::Omit,
    ];

    fn is_text(self) -> bool {
        matches!(
            self,
            Field::Host | Field::Port | Field::Interval | Field::Bandwidth | Field::Buffer
        )
    }
}

pub struct ConfigState {
    pub host: String,
    pub port: String,
    pub protocol: usize,
    pub duration: u16,
    pub parallel: u8,
    pub window: usize,
    pub mtu: usize,
    /// Index into `DIRECTIONS`. The Advanced panel's "Reverse mode"
    /// checkbox is a second, real (not a dead duplicate) way to reach the
    /// same state — checking it sets this to `2` (Rx), unchecking it sets
    /// it back to `1` (Tx); it always wins over `0` (bidir), matching
    /// `-R`/`--bidir` being mutually exclusive on the CLI. There's no
    /// separate `reverse: bool` field to keep in sync — this is the only
    /// source of truth.
    pub direction: usize,
    pub interval: String,
    pub bandwidth: String,
    pub tos: usize,
    pub congestion: usize,
    pub buffer: String,
    pub json_output: bool,
    pub omit: bool,
    pub focus: usize,
    pub editing: bool,
    pub edit_buffer: String,
    pub toast: Option<String>,
}

impl Default for ConfigState {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: "5201".into(),
            protocol: 0,
            duration: 10,
            parallel: 1,
            window: 2,
            mtu: 2,
            direction: 0,
            interval: "1".into(),
            bandwidth: "0".into(),
            tos: 0,
            congestion: 0,
            // Matches `duplex::CHUNK` (the engine's own default write/read
            // chunk size) so the field's starting value is what's actually
            // used before anyone touches it, not an arbitrary placeholder.
            buffer: "131072".into(),
            json_output: false,
            omit: true,
            focus: 0,
            editing: false,
            edit_buffer: String::new(),
            toast: None,
        }
    }
}

pub enum ConfigAction {
    None,
    StartTest,
}

impl ConfigState {
    /// Applies saved/config-file defaults on top of the built-in ones —
    /// called once at startup with whatever `config::load().client`
    /// returns (an all-`None` default if there's no config file yet, so
    /// this is always safe to call). Only overrides the fields the engine
    /// actually acts on; see `cli_preview`'s doc comment for the ones that
    /// don't have a config-file counterpart on purpose.
    pub fn apply_defaults(&mut self, d: &ClientDefaults) {
        if let Some(v) = &d.host {
            self.host = v.clone();
        }
        if let Some(v) = d.port {
            self.port = v.to_string();
        }
        if let Some(v) = d.duration_secs {
            self.duration = v.min(u16::MAX as u64) as u16;
        }
        if let Some(v) = d.parallel {
            self.parallel = v;
        }
        if d.udp == Some(true) {
            self.protocol = 1;
        }
        if d.quic == Some(true) {
            self.protocol = 2;
        }
        self.direction = match (d.bidir, d.reverse) {
            (Some(true), _) => 0,
            (_, Some(true)) => 2,
            _ => self.direction,
        };
        if let Some(v) = &d.bandwidth {
            self.bandwidth = v.clone();
        }
        if let Some(v) = &d.window
            && let Some(idx) = WINDOWS.iter().position(|w| w == v)
        {
            self.window = idx;
        }
        if let Some(v) = d.mtu
            && let Some(idx) = MTUS.iter().position(|m| m.parse() == Ok(v))
        {
            self.mtu = idx;
        }
        if let Some(v) = &d.buffer {
            self.buffer = v.clone();
        }
        if let Some(v) = d.omit {
            self.omit = v;
        }
        if let Some(v) = d.interval_secs {
            self.interval = v.to_string();
        }
        if let Some(v) = d.tos
            && let Some(idx) = TOS_BYTES.iter().position(|t| *t == v)
        {
            self.tos = idx;
        }
        if let Some(v) = &d.congestion
            && let Some(idx) = CONGESTION.iter().position(|c| c == v)
        {
            self.congestion = idx;
        }
        if let Some(v) = d.json {
            self.json_output = v;
        }
    }

    /// The inverse of `apply_defaults` — builds what "Save Profile" (Ctrl+S)
    /// actually writes to disk, mirroring the current field values.
    pub fn to_defaults(&self) -> ClientDefaults {
        ClientDefaults {
            host: Some(self.host.clone()),
            port: self.port.parse().ok(),
            duration_secs: Some(self.duration as u64),
            parallel: Some(self.parallel),
            udp: Some(self.protocol == 1),
            quic: Some(self.protocol == 2),
            reverse: Some(self.direction == 2),
            bidir: Some(self.direction == 0),
            bandwidth: Some(self.bandwidth.clone()),
            window: Some(WINDOWS[self.window].to_string()),
            mtu: MTUS[self.mtu].parse().ok(),
            buffer: Some(self.buffer.clone()),
            omit: Some(self.omit),
            interval_secs: parse_interval(&self.interval),
            tos: Some(TOS_BYTES[self.tos]),
            congestion: Some(CONGESTION[self.congestion].to_string()),
            json: Some(self.json_output),
        }
    }

    /// Mirrors exactly what `gauge client` (the real CLI, see `crate::cli`)
    /// accepts — so copying this line out of the TUI always works. Every
    /// field the screen shows is real, but defaults are left out to keep the
    /// line short.
    pub fn cli_preview(&self) -> Vec<String> {
        let mut flags = vec![
            format!("--host {}", self.host),
            format!("--port {}", self.port),
            format!("--duration {}", self.duration),
            format!("--parallel {}", self.parallel),
        ];
        if self.protocol == 2 {
            flags.push("--quic".into());
        }
        if self.protocol == 1 {
            flags.push("--udp".into());
            let bw = self.bandwidth.trim();
            if !bw.is_empty() && bw != "0" {
                flags.push(format!("--bandwidth {bw}"));
            }
        }
        match self.direction {
            0 => flags.push("--bidir".into()),
            2 => flags.push("--reverse".into()),
            _ => {}
        }
        flags.push(format!("--window {}", WINDOWS[self.window]));
        flags.push(format!("--mtu {}", MTUS[self.mtu]));
        flags.push(format!("--buffer {}", self.buffer));
        if self.omit {
            flags.push("--omit".into());
        }
        if let Some(secs) = parse_interval(&self.interval).filter(|s| *s != 1) {
            flags.push(format!("--interval {secs}"));
        }
        if self.tos != 0 {
            flags.push(format!("--tos {:#04x}", TOS_BYTES[self.tos]));
        }
        if self.congestion != 0 {
            flags.push(format!("--congestion {}", CONGESTION[self.congestion]));
        }
        if self.json_output {
            flags.push("--json".into());
        }
        let mut lines = vec!["$ gauge client \\".to_string()];
        for (i, f) in flags.iter().enumerate() {
            let suffix = if i + 1 == flags.len() { "" } else { " \\" };
            lines.push(format!("  {f}{suffix}"));
        }
        lines
    }

    fn current_field(&self) -> Field {
        Field::ALL[self.focus]
    }

    pub fn on_key(&mut self, key: KeyEvent) -> ConfigAction {
        if self.editing {
            match key.code {
                KeyCode::Enter => {
                    self.apply_edit();
                    self.editing = false;
                }
                KeyCode::Esc => {
                    self.editing = false;
                    self.edit_buffer.clear();
                }
                KeyCode::Backspace => {
                    self.edit_buffer.pop();
                }
                KeyCode::Char(c) => {
                    self.edit_buffer.push(c);
                }
                _ => {}
            }
            return ConfigAction::None;
        }

        match key.code {
            // Ctrl+Enter is indistinguishable from plain Enter on most terminals
            // without kitty-protocol negotiation, so F5 is the reliable trigger.
            KeyCode::F(5) => return ConfigAction::StartTest,
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return ConfigAction::StartTest;
            }
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                // Re-load first rather than saving a blank `GaugeConfig` —
                // otherwise saving the client side here would silently wipe
                // out a `[server]` section the user (or a hand-edit) had
                // already put in the file.
                let mut on_disk = config::load();
                on_disk.client = self.to_defaults();
                self.toast = Some(match config::save(&on_disk) {
                    Ok(path) => format!("config saved to {}", path.display()),
                    Err(e) => format!("failed to save config: {e}"),
                });
            }
            KeyCode::Tab | KeyCode::Down => {
                self.focus = (self.focus + 1) % Field::ALL.len();
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.focus = (self.focus + Field::ALL.len() - 1) % Field::ALL.len();
            }
            KeyCode::Enter => {
                let f = self.current_field();
                if f.is_text() {
                    self.edit_buffer = self.field_text(f);
                    self.editing = true;
                } else {
                    self.toggle_or_cycle(1);
                }
            }
            KeyCode::Left => self.step(-1),
            KeyCode::Right => self.step(1),
            KeyCode::Char(' ') => self.toggle_or_cycle(1),
            _ => {}
        }
        ConfigAction::None
    }

    fn field_text(&self, f: Field) -> String {
        match f {
            Field::Host => self.host.clone(),
            Field::Port => self.port.clone(),
            Field::Interval => self.interval.clone(),
            Field::Bandwidth => self.bandwidth.clone(),
            Field::Buffer => self.buffer.clone(),
            _ => String::new(),
        }
    }

    fn apply_edit(&mut self) {
        let f = self.current_field();
        match f {
            Field::Host => self.host = self.edit_buffer.clone(),
            Field::Port => self.port = self.edit_buffer.clone(),
            Field::Interval => self.interval = self.edit_buffer.clone(),
            Field::Bandwidth => self.bandwidth = self.edit_buffer.clone(),
            Field::Buffer => self.buffer = self.edit_buffer.clone(),
            _ => {}
        }
        self.edit_buffer.clear();
    }

    fn toggle_or_cycle(&mut self, dir: i32) {
        match self.current_field() {
            Field::Protocol => self.protocol = cycle(self.protocol, PROTOCOLS.len(), dir),
            Field::Window => self.window = cycle(self.window, WINDOWS.len(), dir),
            Field::Mtu => self.mtu = cycle(self.mtu, MTUS.len(), dir),
            Field::Direction => self.direction = cycle(self.direction, DIRECTIONS.len(), dir),
            Field::Tos => self.tos = cycle(self.tos, TOS.len(), dir),
            Field::Congestion => self.congestion = cycle(self.congestion, CONGESTION.len(), dir),
            Field::Json => self.json_output = !self.json_output,
            // A boolean view onto the same `direction` the dropdown above
            // controls — see its doc comment. Always lands on Tx or Rx,
            // never Bidir, matching -R/--bidir's mutual exclusivity.
            Field::Reverse => self.direction = if self.direction == 2 { 1 } else { 2 },
            Field::Omit => self.omit = !self.omit,
            _ => {}
        }
    }

    fn step(&mut self, dir: i32) {
        match self.current_field() {
            Field::Duration => {
                let d = self.duration as i32 + dir * 5;
                self.duration = d.clamp(5, 300) as u16;
            }
            Field::Parallel => {
                let p = self.parallel as i32 + dir;
                self.parallel = p.clamp(1, 32) as u8;
            }
            _ => self.toggle_or_cycle(dir),
        }
    }
}

fn cycle(cur: usize, len: usize, dir: i32) -> usize {
    let len = len as i32;
    ((cur as i32 + dir).rem_euclid(len)) as usize
}

/// Maps a `PROTOCOLS` index to a real transport.
fn proto_from_index(idx: usize) -> Option<Proto> {
    match idx {
        0 => Some(Proto::Tcp),
        1 => Some(Proto::Udp),
        2 => Some(Proto::Quic),
        _ => None,
    }
}

/// Maps a `DIRECTIONS` index to the engine's `Direction`. Order matches the
/// TUI's own dropdown: "TX + RX (bidir)" / "TX only" / "RX only".
fn direction_from_index(idx: usize) -> Direction {
    match idx {
        0 => Direction::Bidir,
        2 => Direction::Rx,
        _ => Direction::Tx,
    }
}

/// A cheap, good-enough-for-a-demo-tool unique id correlating a client
/// test's parallel streams into one session on the receiving side.
fn new_session_id() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

pub struct HistoryState {
    pub runs: Vec<HistoryRun>,
    pub selected: usize,
    pub toast: Option<String>,
}

impl Default for HistoryState {
    fn default() -> Self {
        Self { runs: crate::store::load(), selected: 0, toast: None }
    }
}

impl HistoryState {
    pub fn on_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up => {
                if self.selected > 0 {
                    self.selected -= 1;
                }
            }
            KeyCode::Down => {
                if self.selected + 1 < self.runs.len() {
                    self.selected += 1;
                }
            }
            KeyCode::Char('d') if !self.runs.is_empty() => {
                self.runs.remove(self.selected);
                if self.selected >= self.runs.len() && self.selected > 0 {
                    self.selected -= 1;
                }
                crate::store::save(&self.runs);
            }
            KeyCode::Char('e') if !self.runs.is_empty() => {
                let run = &self.runs[self.selected];
                let filename = format!("gauge-history-{}.json", now_hms().replace(':', ""));
                self.toast = Some(match crate::store::export_json(&filename, run) {
                    Ok(path) => format!("exported to {}", path.display()),
                    Err(e) => format!("export failed: {e}"),
                });
            }
            _ => {}
        }
    }

    /// Records a just-finished test as the newest entry, selects it, and
    /// persists the updated list to disk.
    fn push_run(&mut self, run: HistoryRun) {
        self.runs.insert(0, run);
        self.selected = 0;
        crate::store::save(&self.runs);
    }
}

pub struct ServerStreamRow {
    pub stream_index: u8,
    pub addr: String,
    pub tx: f64,
    pub rx: f64,
    pub loss_pct: f64,
    pub jitter_ms: f64,
    pub avg_gbps: f64,
    pub connected_at: Instant,
    pub stop: Arc<AtomicBool>,
    pub finished: bool,
}

pub struct ServerSession {
    pub session_id: u64,
    pub stream_count: u8,
    pub total_secs: u64,
    pub streams: HashMap<u8, ServerStreamRow>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ServerField {
    Bind,
    Port,
    Protocol,
    Window,
    Json,
    OneOff,
}

impl ServerField {
    pub const ALL: [ServerField; 6] = [
        ServerField::Bind,
        ServerField::Port,
        ServerField::Protocol,
        ServerField::Window,
        ServerField::Json,
        ServerField::OneOff,
    ];

    fn is_text(self) -> bool {
        matches!(self, ServerField::Bind | ServerField::Port)
    }
}

pub struct ServerConfigState {
    pub bind: String,
    pub port: String,
    pub protocol: usize,
    pub window: usize,
    pub json_output: bool,
    pub one_off: bool,
    pub focus: usize,
    pub editing: bool,
    pub edit_buffer: String,
}

impl Default for ServerConfigState {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0".into(),
            port: "5201".into(),
            protocol: 0,
            window: 2,
            json_output: false,
            one_off: false,
            focus: 0,
            editing: false,
            edit_buffer: String::new(),
        }
    }
}

pub enum ServerConfigAction {
    None,
    Start,
    /// Ctrl+S — write the current values to the `[server]` config section.
    Save,
}

impl ServerConfigState {
    /// Applies config-file defaults on top of the built-in ones — called
    /// once at startup with `config::load().server` (an all-`None` default
    /// if there's no config file yet, so always safe to call). Ctrl+S on
    /// the Server Mode screen writes them back (see `to_defaults`).
    pub fn apply_defaults(&mut self, d: &ServerDefaults) {
        if let Some(v) = &d.bind {
            self.bind = v.clone();
        }
        if let Some(v) = d.port {
            self.port = v.to_string();
        }
        if d.udp == Some(true) {
            self.protocol = 1;
        }
        if d.quic == Some(true) {
            self.protocol = 2;
        }
        if let Some(v) = &d.window
            && let Some(idx) = WINDOWS.iter().position(|w| w == v)
        {
            self.window = idx;
        }
        if let Some(v) = d.json {
            self.json_output = v;
        }
        if let Some(v) = d.one_off {
            self.one_off = v;
        }
    }

    /// The inverse of `apply_defaults` — what Ctrl+S writes to `[server]`.
    pub fn to_defaults(&self) -> ServerDefaults {
        ServerDefaults {
            bind: Some(self.bind.clone()),
            port: self.port.parse().ok(),
            udp: Some(self.protocol == 1),
            quic: Some(self.protocol == 2),
            window: Some(WINDOWS[self.window].to_string()),
            json: Some(self.json_output),
            one_off: Some(self.one_off),
        }
    }

    /// Mirrors exactly what `gauge server` (the real CLI, see `crate::cli`)
    /// accepts. `mtu` isn't included — `ServerArgs` has no `--mtu` flag
    /// (MTU only applies to UDP, and only on the sending side).
    pub fn cli_preview(&self) -> Vec<String> {
        let mut flags = vec![format!("--bind {}", self.bind), format!("--port {}", self.port)];
        if self.protocol == 1 {
            flags.push("--udp".into());
        }
        if self.protocol == 2 {
            flags.push("--quic".into());
        }
        flags.push(format!("--window {}", WINDOWS[self.window]));
        if self.json_output {
            flags.push("--json".into());
        }
        if self.one_off {
            flags.push("--one-off".into());
        }
        let mut lines = vec!["$ gauge server \\".to_string()];
        for (i, f) in flags.iter().enumerate() {
            let suffix = if i + 1 == flags.len() { "" } else { " \\" };
            lines.push(format!("  {f}{suffix}"));
        }
        lines
    }

    fn current_field(&self) -> ServerField {
        ServerField::ALL[self.focus]
    }

    pub fn on_key(&mut self, key: KeyEvent) -> ServerConfigAction {
        if self.editing {
            match key.code {
                KeyCode::Enter => {
                    self.apply_edit();
                    self.editing = false;
                }
                KeyCode::Esc => {
                    self.editing = false;
                    self.edit_buffer.clear();
                }
                KeyCode::Backspace => {
                    self.edit_buffer.pop();
                }
                KeyCode::Char(c) => {
                    self.edit_buffer.push(c);
                }
                _ => {}
            }
            return ServerConfigAction::None;
        }

        match key.code {
            KeyCode::F(5) => return ServerConfigAction::Start,
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return ServerConfigAction::Start;
            }
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return ServerConfigAction::Save;
            }
            KeyCode::Tab | KeyCode::Down => {
                self.focus = (self.focus + 1) % ServerField::ALL.len();
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.focus = (self.focus + ServerField::ALL.len() - 1) % ServerField::ALL.len();
            }
            KeyCode::Enter => {
                let f = self.current_field();
                if f.is_text() {
                    self.edit_buffer = self.field_text(f);
                    self.editing = true;
                } else {
                    self.toggle_or_cycle(1);
                }
            }
            KeyCode::Left => self.step(-1),
            KeyCode::Right => self.step(1),
            KeyCode::Char(' ') => self.toggle_or_cycle(1),
            _ => {}
        }
        ServerConfigAction::None
    }

    fn field_text(&self, f: ServerField) -> String {
        match f {
            ServerField::Bind => self.bind.clone(),
            ServerField::Port => self.port.clone(),
            _ => String::new(),
        }
    }

    fn apply_edit(&mut self) {
        match self.current_field() {
            ServerField::Bind => self.bind = self.edit_buffer.clone(),
            ServerField::Port => self.port = self.edit_buffer.clone(),
            _ => {}
        }
        self.edit_buffer.clear();
    }

    fn toggle_or_cycle(&mut self, dir: i32) {
        match self.current_field() {
            ServerField::Protocol => self.protocol = cycle(self.protocol, PROTOCOLS.len(), dir),
            ServerField::Window => self.window = cycle(self.window, WINDOWS.len(), dir),
            ServerField::Json => self.json_output = !self.json_output,
            ServerField::OneOff => self.one_off = !self.one_off,
            _ => {}
        }
    }

    fn step(&mut self, dir: i32) {
        self.toggle_or_cycle(dir);
    }
}

/// Server Mode screen state. Idle (config view) until started via F5; from
/// then on it is driven by `NetEvent`s from the real TCP server engine.
pub struct ServerState {
    pub listening: bool,
    pub bind: String,
    pub active_proto: String,
    pub active_clients: u32,
    pub total_sessions: u32,
    pub listening_since: Option<Instant>,
    pub session: Option<ServerSession>,
    pub config: ServerConfigState,
    pub log: VecDeque<LogLine>,
    pub toast: Option<String>,
}

impl Default for ServerState {
    fn default() -> Self {
        Self {
            listening: false,
            bind: "0.0.0.0:5201".into(),
            active_proto: "TCP".into(),
            active_clients: 0,
            total_sessions: 0,
            listening_since: None,
            session: None,
            config: ServerConfigState::default(),
            log: VecDeque::new(),
            toast: None,
        }
    }
}

pub enum ServerRunAction {
    None,
    Stop,
    KillClient,
}

impl ServerState {
    fn push_log(&mut self, level: LogLevel, text: impl Into<String>) {
        let line = LogLine::new(now_hms(), level, text);
        crate::filelog::append("server", &line);
        self.log.push_front(line);
        while self.log.len() > LOG_MAX {
            self.log.pop_back();
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> ServerRunAction {
        match key.code {
            KeyCode::Char('s') => ServerRunAction::Stop,
            KeyCode::Char('k') => ServerRunAction::KillClient,
            KeyCode::Char('c') => {
                self.log.clear();
                ServerRunAction::None
            }
            _ => ServerRunAction::None,
        }
    }
}

pub struct App {
    pub tab: Tab,
    pub should_quit: bool,
    pub live: LiveState,
    pub config: ConfigState,
    pub history: HistoryState,
    pub server: ServerState,
    net_tx: UnboundedSender<NetCommand>,
    net_rx: UnboundedReceiver<NetEvent>,
}

impl Default for App {
    fn default() -> Self {
        let (net_tx, net_rx) = net::spawn_runtime();
        let GaugeConfig { client, server } = config::load();
        let mut config = ConfigState::default();
        config.apply_defaults(&client);
        let mut server_state = ServerState::default();
        server_state.config.apply_defaults(&server);
        Self {
            tab: Tab::Live,
            should_quit: false,
            live: LiveState::default(),
            config,
            history: HistoryState::default(),
            server: server_state,
            net_tx,
            net_rx,
        }
    }
}

impl App {
    pub fn on_key(&mut self, key: KeyEvent) {
        let editing_text = (self.tab == Tab::Config && self.config.editing)
            || (self.tab == Tab::Server && !self.server.listening && self.server.config.editing);

        if key.code == KeyCode::Char('q') && !editing_text {
            self.should_quit = true;
            return;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.should_quit = true;
            return;
        }

        if let KeyCode::Char(c @ '1'..='4') = key.code
            && !editing_text
        {
            self.tab = Tab::from_index(c as usize - '1' as usize);
            return;
        }

        match self.tab {
            Tab::Live => {
                if key.code == KeyCode::Tab {
                    self.tab = Tab::from_index(self.tab.index() + 1);
                } else {
                    match self.live.on_key(key) {
                        LiveAction::Stop => {
                            if let Some(flag) = &self.live.stop_flag {
                                flag.store(true, Ordering::Relaxed);
                            }
                            self.live.running = false;
                            self.live.connecting = false;
                            self.live.push_log(LogLevel::Warn, "test stopped by user");
                            self.live.toast = Some("test stopped".into());
                        }
                        LiveAction::Restart => {
                            self.start_client_test();
                        }
                        LiveAction::None => {}
                    }
                }
            }
            Tab::Config => match self.config.on_key(key) {
                ConfigAction::StartTest => {
                    if self.start_client_test() {
                        self.tab = Tab::Live;
                    }
                }
                ConfigAction::None => {}
            },
            Tab::History => {
                if key.code == KeyCode::Tab {
                    self.tab = Tab::from_index(self.tab.index() + 1);
                } else {
                    self.history.on_key(key);
                }
            }
            Tab::Server => {
                if self.server.listening {
                    if key.code == KeyCode::Tab {
                        self.tab = Tab::from_index(self.tab.index() + 1);
                    } else {
                        match self.server.on_key(key) {
                            ServerRunAction::Stop => {
                                let _ = self.net_tx.send(NetCommand::StopServer);
                                if let Some(session) = self.server.session.take() {
                                    for row in session.streams.into_values() {
                                        row.stop.store(true, Ordering::Relaxed);
                                    }
                                }
                                self.server.listening = false;
                                self.server.listening_since = None;
                                self.server.active_clients = 0;
                                self.server.push_log(LogLevel::Warn, "server stopped by user");
                                self.server.toast = Some("server stopped".into());
                            }
                            ServerRunAction::KillClient => {
                                if let Some(session) = &self.server.session {
                                    for row in session.streams.values() {
                                        row.stop.store(true, Ordering::Relaxed);
                                    }
                                    self.server.push_log(
                                        LogLevel::Warn,
                                        "active client connection(s) killed by operator",
                                    );
                                } else {
                                    self.server.toast = Some("no active client".into());
                                }
                            }
                            ServerRunAction::None => {}
                        }
                    }
                } else {
                    match self.server.config.on_key(key) {
                        ServerConfigAction::Start => self.start_server(),
                        ServerConfigAction::Save => self.save_server_config(),
                        ServerConfigAction::None => {}
                    }
                }
            }
        }
    }

    /// Dispatches a real, possibly multi-stream TCP, UDP or QUIC test using
    /// the current `ConfigState`, and resets `LiveState` to a fresh
    /// "connecting" view.
    /// Returns `false` if the test couldn't be started (e.g. an invalid
    /// interval) — callers must not switch to the Live Test tab in that
    /// case, or the message ends up on a toast nobody's looking at.
    fn start_client_test(&mut self) -> bool {
        let proto = proto_from_index(self.config.protocol).unwrap_or(Proto::Tcp);
        let direction = direction_from_index(self.config.direction);
        // QUIC's congestion control runs in user space, so only TCP needs
        // OS support for choosing one.
        if proto == Proto::Tcp && self.config.congestion != 0 && !net::CONGESTION_SETTABLE {
            let msg =
                "TCP congestion control can only be chosen on Linux — set it back to \"default\"";
            self.config.toast = Some(msg.into());
            self.live.toast = Some(msg.into());
            return false;
        }
        let Some(interval_secs) = parse_interval(&self.config.interval) else {
            let msg = format!(
                "Interval must be a whole number of seconds between 1 and {MAX_INTERVAL_SECS}"
            );
            self.config.toast = Some(msg.clone());
            self.live.toast = Some(msg);
            return false;
        };
        let stream_count = self.config.parallel.max(1);
        let mtu: u32 = MTUS[self.config.mtu].parse().unwrap_or(1500);
        let window_bytes = net::parse_window_bytes(WINDOWS[self.config.window]);
        let stop = Arc::new(AtomicBool::new(false));
        let target = format!("{}:{}", self.config.host, self.config.port);
        self.live = LiveState::connecting(
            ConnectingParams {
                target,
                proto: PROTOCOLS[self.config.protocol].into(),
                direction,
                stream_count,
                window: WINDOWS[self.config.window].into(),
                mtu,
                omit_enabled: self.config.omit,
                settings: RunSettings {
                    buffer_bytes: (proto != Proto::Udp)
                        .then(|| net::parse_window_bytes(&self.config.buffer))
                        .flatten(),
                    bandwidth_bps: (proto == Proto::Udp)
                        .then(|| net::parse_bps(&self.config.bandwidth))
                        .flatten(),
                    omit: self.config.omit,
                    interval_secs,
                    tos: Some(TOS_BYTES[self.config.tos]).filter(|t| *t != 0),
                    congestion: (proto != Proto::Udp && self.config.congestion != 0)
                        .then(|| CONGESTION[self.config.congestion].to_string()),
                },
            },
            stop.clone(),
        );
        let bandwidth_bps =
            if proto == Proto::Udp { net::parse_bps(&self.config.bandwidth) } else { None };
        let buffer_bytes = net::parse_window_bytes(&self.config.buffer);
        let params = ClientParams {
            host: self.config.host.clone(),
            port: self.config.port.parse().unwrap_or(5201),
            duration_secs: self.config.duration as u64,
            proto,
            stream_count,
            direction,
            bandwidth_bps,
            window_bytes,
            mtu,
            buffer_bytes,
            interval_secs,
            tos: Some(TOS_BYTES[self.config.tos]).filter(|t| *t != 0),
            congestion: (self.config.congestion != 0)
                .then(|| CONGESTION[self.config.congestion].to_string()),
            session_id: new_session_id(),
        };
        let _ = self.net_tx.send(NetCommand::StartClient(params, stop));
        true
    }

    /// Re-loads the file first so saving the `[server]` side never wipes
    /// out a `[client]` section (same reasoning as the Config screen's save).
    fn save_server_config(&mut self) {
        let mut on_disk = config::load();
        on_disk.server = self.server.config.to_defaults();
        self.server.toast = Some(match config::save(&on_disk) {
            Ok(path) => format!("config saved to {}", path.display()),
            Err(e) => format!("failed to save config: {e}"),
        });
    }

    fn start_server(&mut self) {
        let proto = proto_from_index(self.server.config.protocol).unwrap_or(Proto::Tcp);
        let params = ServerParams {
            bind: self.server.config.bind.clone(),
            port: self.server.config.port.parse().unwrap_or(5201),
            proto,
            window_bytes: net::parse_window_bytes(WINDOWS[self.server.config.window]),
        };
        self.server.active_proto = PROTOCOLS[self.server.config.protocol].into();
        self.server.toast = Some("starting server…".into());
        let _ = self.net_tx.send(NetCommand::StartServer(params));
    }

    /// Turns the just-finished `LiveState` into a `HistoryRun`. Called only
    /// on a clean finish (no `live.error`) once every stream is done.
    fn record_history_run(&mut self) {
        self.history.push_run(self.live.snapshot());
    }

    fn handle_net_event(&mut self, evt: NetEvent) {
        match evt {
            NetEvent::ClientStreamConnected { stream_index, rtt_ms } => {
                if stream_index == 0 {
                    self.live.connecting = false;
                    self.live.running = true;
                    self.live.lat = rtt_ms;
                    push_hist(&mut self.live.lat_hist, rtt_ms.max(0.0) as u64);
                    self.live.push_log(
                        LogLevel::Ok,
                        format!("connected — {} handshake {rtt_ms:.2}ms", self.live.proto),
                    );
                }
                if let Some(row) = self.live.rows.get_mut(stream_index as usize) {
                    row.rtt = rtt_ms;
                }
            }
            NetEvent::ClientStreamInterval { stream_index, flow, interval_bytes, stats } => {
                if let Some(row) = self.live.rows.get_mut(stream_index as usize) {
                    match flow {
                        Flow::Tx => row.tx = stats.gbps,
                        Flow::Rx => row.rx = stats.gbps,
                    }
                    if let Some(r) = stats.retransmits {
                        // "Degraded" means retransmits happened *since the
                        // last sample* (an active problem right now), not
                        // just that the cumulative count is nonzero (a
                        // handful of retransmits early in a long test
                        // shouldn't paint the whole run red forever after).
                        row.degraded = r > row.retrans as u64;
                        row.retrans = u32::try_from(r).unwrap_or(u32::MAX);
                    }
                    if let Some(c) = stats.cwnd_bytes {
                        row.cwnd = c / 1024;
                    }
                    if let Some(l) = stats.loss_pct {
                        row.loss = l / 100.0;
                    }
                    if flow == Flow::Rx && stats.jitter_ms.is_some() {
                        row.jitter_ms = stats.jitter_ms;
                    }
                }
                match flow {
                    Flow::Tx => self.live.tx_total_gb += interval_bytes as f64 / 1e9,
                    Flow::Rx => self.live.rx_total_gb += interval_bytes as f64 / 1e9,
                }
                // "Omit first 3s": bytes within the ramp-up window are still
                // counted in tx_total_gb/rx_total_gb above (the true total
                // transferred never changes), but excluded from these
                // separate accumulators — used only for the *rate* summary
                // at test end (see the ClientStreamFinished handler).
                if !self.live.omit_enabled || stats.elapsed_secs > OMIT_SECS {
                    match flow {
                        Flow::Tx => self.live.tx_bytes_after_omit += interval_bytes,
                        Flow::Rx => self.live.rx_bytes_after_omit += interval_bytes,
                    }
                }
                self.live.tx = self.live.rows.iter().map(|r| r.tx).sum();
                self.live.rx = self.live.rows.iter().map(|r| r.rx).sum();
                // Only stream 0 drives the aggregate sparkline/log line —
                // otherwise N streams would each push their own ~1s tick,
                // making the aggregate update N times too fast.
                if stream_index == 0 {
                    let (hist, peak, chart, aggregate, label) = match flow {
                        Flow::Tx => (
                            &mut self.live.tx_hist,
                            &mut self.live.peak_tx_gbps,
                            &mut self.live.tx_chart,
                            self.live.tx,
                            "TX",
                        ),
                        Flow::Rx => (
                            &mut self.live.rx_hist,
                            &mut self.live.peak_rx_gbps,
                            &mut self.live.rx_chart,
                            self.live.rx,
                            "RX",
                        ),
                    };
                    push_hist(hist, (aggregate * 100.0) as u64);
                    if flow == Flow::Rx {
                        let measured: Vec<f64> =
                            self.live.rows.iter().filter_map(|r| r.jitter_ms).collect();
                        if !measured.is_empty() {
                            let mean = measured.iter().sum::<f64>() / measured.len() as f64;
                            self.live.jitter_ms = Some(mean);
                            push_hist(&mut self.live.jitter_hist, (mean * 100.0) as u64);
                        }
                    }
                    let from_secs = std::mem::replace(
                        &mut self.live.last_sample_secs[flow as usize],
                        stats.elapsed_secs,
                    );
                    // The peak is tracked over every sample and, separately,
                    // over the post-ramp-up ones (see `rate_summary`); the
                    // chart/sparkline always show every sample.
                    *peak = peak.max(aggregate);
                    if !self.live.omit_enabled || stats.elapsed_secs > OMIT_SECS {
                        let after = match flow {
                            Flow::Tx => &mut self.live.peak_tx_after_omit,
                            Flow::Rx => &mut self.live.peak_rx_after_omit,
                        };
                        *after = after.max(aggregate);
                    }
                    chart.push((stats.elapsed_secs as f64, aggregate));
                    self.live.push_log(
                        LogLevel::Data,
                        format!(
                            "interval {}-{}s  {label} {aggregate:.2} Gbit/s aggregate ({} stream(s))",
                            from_secs,
                            stats.elapsed_secs,
                            self.live.rows.len()
                        ),
                    );
                }
            }
            NetEvent::ClientStreamFinished {
                stream_index, flow, total_bytes, avg_gbps, ..
            } => {
                self.live.streams_finished += 1;
                match flow {
                    Flow::Tx => self.live.tx_finished_bytes += total_bytes,
                    Flow::Rx => self.live.rx_finished_bytes += total_bytes,
                }
                let flow_label = match flow {
                    Flow::Tx => "TX",
                    Flow::Rx => "RX",
                };
                self.live.push_log(
                    LogLevel::Info,
                    format!(
                        "stream #{} {flow_label} finished — avg {avg_gbps:.2} Gbit/s",
                        stream_index + 1
                    ),
                );
                if self.live.streams_finished >= self.live.expected_finishes {
                    self.live.running = false;
                    // Now that every flow has reported its exact byte count,
                    // replace the interval-sampled approximation with it —
                    // see the field doc comment on `tx_finished_bytes`.
                    self.live.tx_total_gb = self.live.tx_finished_bytes as f64 / 1e9;
                    self.live.rx_total_gb = self.live.rx_finished_bytes as f64 / 1e9;
                    let elapsed = self
                        .live
                        .started_at
                        .map(|t| t.elapsed().as_secs_f64())
                        .unwrap_or(1.0)
                        .max(0.001);
                    let (avg_tx, avg_rx, _, _) = self.live.rate_summary(elapsed);
                    let omit_note = if self.live.omit_applies(elapsed) {
                        format!(" (first {OMIT_SECS}s omitted)")
                    } else if self.live.omit_enabled {
                        format!(" (omit ignored: test not longer than {OMIT_SECS}s)")
                    } else {
                        String::new()
                    };
                    let summary = match self.live.direction {
                        Direction::Bidir => {
                            format!(
                                "test complete — TX avg {avg_tx:.2} / RX avg {avg_rx:.2} Gbit/s over {elapsed:.0}s{omit_note}"
                            )
                        }
                        Direction::Rx => format!(
                            "test complete — RX avg {avg_rx:.2} Gbit/s over {elapsed:.0}s{omit_note}"
                        ),
                        Direction::Tx => format!(
                            "test complete — avg {avg_tx:.2} Gbit/s over {elapsed:.0}s{omit_note}"
                        ),
                    };
                    self.live.push_log(LogLevel::Ok, summary);
                    self.live.toast = Some("test complete".into());
                    if self.live.error.is_none() {
                        self.record_history_run();
                    }
                }
            }
            NetEvent::ClientError { stream_index, message } => {
                self.live.running = false;
                self.live.connecting = false;
                let prefixed = match stream_index {
                    Some(i) => format!("stream #{}: {message}", i + 1),
                    None => message.clone(),
                };
                self.live.push_log(LogLevel::Err, prefixed);
                // Shown persistently in the status bar instead of a toast —
                // it's a lasting state, not a one-off notification, and the
                // two overlapping was a layout bug on narrower terminals.
                self.live.error = Some(message);
            }

            NetEvent::ServerListening { bind } => {
                self.server.bind = bind;
                self.server.listening = true;
                self.server.listening_since = Some(Instant::now());
                self.server.push_log(
                    LogLevel::Ok,
                    format!("gauge server started — listening on {}", self.server.bind),
                );
                self.server.toast = Some("server started".into());
            }
            NetEvent::ServerListenError(msg) => {
                self.server.listening = false;
                self.server.push_log(LogLevel::Err, msg.clone());
                self.server.toast = Some(msg);
            }
            NetEvent::ServerStreamConnected {
                session_id,
                stream_index,
                stream_count,
                addr,
                stop,
                duration_secs,
            } => {
                let is_new_session =
                    self.server.session.as_ref().map(|s| s.session_id) != Some(session_id);
                if is_new_session {
                    self.server.total_sessions += 1;
                    self.server.session = Some(ServerSession {
                        session_id,
                        stream_count,
                        total_secs: duration_secs,
                        streams: HashMap::new(),
                    });
                }
                if let Some(session) = self.server.session.as_mut() {
                    session.streams.insert(
                        stream_index,
                        ServerStreamRow {
                            stream_index,
                            addr: addr.clone(),
                            tx: 0.0,
                            rx: 0.0,
                            loss_pct: 0.0,
                            jitter_ms: 0.0,
                            avg_gbps: 0.0,
                            connected_at: Instant::now(),
                            stop,
                            finished: false,
                        },
                    );
                }
                self.server.active_clients = 1;
                self.server.push_log(
                    LogLevel::Info,
                    format!("{addr} connected — stream {}/{}", stream_index + 1, stream_count),
                );
            }
            NetEvent::ServerStreamInterval { session_id, stream_index, flow, stats } => {
                let mut log_line = None;
                if let Some(session) = self.server.session.as_mut()
                    && session.session_id == session_id
                    && let Some(row) = session.streams.get_mut(&stream_index)
                {
                    let label = match flow {
                        Flow::Tx => {
                            row.tx = stats.gbps;
                            "TX"
                        }
                        Flow::Rx => {
                            row.rx = stats.gbps;
                            row.loss_pct = stats.loss_pct.unwrap_or(0.0);
                            row.jitter_ms = stats.jitter_ms.unwrap_or(0.0);
                            "RX"
                        }
                    };
                    log_line = Some(format!("{} — {label} {:.2} Gbit/s", row.addr, stats.gbps));
                }
                if let Some(line) = log_line {
                    self.server.push_log(LogLevel::Data, line);
                }
            }
            NetEvent::ServerStreamFinished { session_id, stream_index, flow, avg_gbps } => {
                let mut log_line = None;
                if let Some(session) = self.server.session.as_mut()
                    && session.session_id == session_id
                    && let Some(row) = session.streams.get_mut(&stream_index)
                {
                    row.finished = true;
                    row.avg_gbps = avg_gbps;
                    let label = match flow {
                        Flow::Tx => "TX",
                        Flow::Rx => "RX",
                    };
                    log_line = Some(format!(
                        "{} — {label} stream complete, avg {avg_gbps:.2} Gbit/s",
                        row.addr
                    ));
                }
                if let Some(line) = log_line {
                    self.server.push_log(LogLevel::Ok, line);
                }
            }
            NetEvent::ServerStreamDisconnected { session_id, stream_index } => {
                self.server
                    .push_log(LogLevel::Info, format!("stream #{} disconnected", stream_index + 1));
                if let Some(session) = self.server.session.as_mut()
                    && session.session_id == session_id
                {
                    session.streams.remove(&stream_index);
                    if session.streams.is_empty() {
                        self.server.session = None;
                        self.server.active_clients = 0;
                    }
                }
            }
        }
    }

    pub fn tick(&mut self) {
        while let Ok(evt) = self.net_rx.try_recv() {
            self.handle_net_event(evt);
        }
    }
}

#[cfg(test)]
// Building `T::default()` then setting one or two fields reads more clearly
// in test setup than a struct-update-syntax literal with a long `.focus:
// Field::ALL.iter().position(...).unwrap()` expression inline.
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    // ── cycle() ─────────────────────────────────────────────────────────

    #[test]
    fn cycle_wraps_forward_and_backward() {
        assert_eq!(cycle(0, 3, 1), 1);
        assert_eq!(cycle(2, 3, 1), 0, "should wrap forward past the end");
        assert_eq!(cycle(0, 3, -1), 2, "should wrap backward past the start");
        assert_eq!(cycle(1, 3, -1), 0);
    }

    // ── push_hist() ─────────────────────────────────────────────────────

    #[test]
    fn push_hist_caps_at_spark_len() {
        let mut hist = VecDeque::new();
        for v in 0..(SPARK_LEN as u64 + 10) {
            push_hist(&mut hist, v);
        }
        assert_eq!(hist.len(), SPARK_LEN);
        // Oldest values should have been evicted from the front.
        assert_eq!(*hist.front().unwrap(), 10);
        assert_eq!(*hist.back().unwrap(), SPARK_LEN as u64 + 9);
    }

    // ── proto_from_index() ──────────────────────────────────────────────

    #[test]
    fn proto_from_index_maps_every_protocol_entry() {
        assert_eq!(proto_from_index(0), Some(Proto::Tcp));
        assert_eq!(proto_from_index(1), Some(Proto::Udp));
        assert_eq!(proto_from_index(2), Some(Proto::Quic));
        assert_eq!(proto_from_index(99), None);
    }

    #[test]
    fn direction_from_index_matches_the_dropdown_order() {
        assert_eq!(direction_from_index(0), Direction::Bidir);
        assert_eq!(direction_from_index(1), Direction::Tx);
        assert_eq!(direction_from_index(2), Direction::Rx);
        assert_eq!(
            direction_from_index(99),
            Direction::Tx,
            "unknown index should fall back to the safe default"
        );
    }

    // ── ConfigState ─────────────────────────────────────────────────────

    #[test]
    fn interval_accepts_whole_seconds_in_range_only() {
        assert_eq!(parse_interval("1"), Some(1));
        assert_eq!(parse_interval(" 60 "), Some(60));
        for bad in ["0", "61", "-1", "1.5", "", "abc"] {
            assert_eq!(parse_interval(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn cli_preview_shows_interval_tos_and_congestion_only_when_set() {
        let mut cfg = ConfigState::default();
        let preview = cfg.cli_preview().join("\n");
        assert!(
            !preview.contains("--interval")
                && !preview.contains("--tos")
                && !preview.contains("--congestion")
        );

        cfg.interval = "5".into();
        cfg.tos = 2;
        cfg.congestion = 2;
        let preview = cfg.cli_preview().join("\n");
        assert!(preview.contains("--interval 5"), "{preview}");
        assert!(preview.contains("--tos 0xb8"), "{preview}");
        assert!(preview.contains("--congestion bbr"), "{preview}");
    }

    #[test]
    fn quic_config_selects_the_quic_entry_and_round_trips() {
        let mut cfg = ConfigState::default();
        cfg.apply_defaults(&ClientDefaults { quic: Some(true), ..Default::default() });
        assert_eq!(PROTOCOLS[cfg.protocol], "QUIC");
        let d = cfg.to_defaults();
        assert_eq!((d.udp, d.quic), (Some(false), Some(true)));
        assert!(cfg.cli_preview().iter().any(|l| l.contains("--quic")), "{:?}", cfg.cli_preview());

        let mut server = ServerConfigState::default();
        server.apply_defaults(&ServerDefaults { quic: Some(true), ..Default::default() });
        assert_eq!(PROTOCOLS[server.protocol], "QUIC");
        assert!(server.cli_preview().iter().any(|l| l.contains("--quic")));
    }

    #[test]
    fn config_apply_defaults_overrides_only_the_fields_the_config_sets() {
        let mut cfg = ConfigState::default();
        let d = ClientDefaults {
            host: Some("10.0.0.9".into()),
            port: Some(6001),
            duration_secs: Some(42),
            parallel: Some(8),
            udp: Some(true),
            quic: None,
            reverse: None,
            bidir: Some(true),
            bandwidth: Some("100M".into()),
            window: Some("512K".into()),
            mtu: Some(1500),
            buffer: Some("65536".into()),
            omit: Some(true),
            interval_secs: Some(5),
            tos: Some(0xB8),
            congestion: Some("bbr".into()),
            json: Some(true),
        };
        cfg.apply_defaults(&d);

        assert_eq!(cfg.interval, "5");
        assert_eq!(TOS_BYTES[cfg.tos], 0xB8);
        assert_eq!(CONGESTION[cfg.congestion], "bbr");
        let round_trip = cfg.to_defaults();
        assert_eq!((round_trip.interval_secs, round_trip.tos), (Some(5), Some(0xB8)));
        assert_eq!(round_trip.congestion.as_deref(), Some("bbr"));

        assert_eq!(cfg.host, "10.0.0.9");
        assert_eq!(cfg.port, "6001");
        assert_eq!(cfg.duration, 42);
        assert_eq!(cfg.parallel, 8);
        assert_eq!(cfg.protocol, 1, "udp: true should select the UDP entry");
        assert_eq!(cfg.direction, 0, "bidir: true should select the bidir entry");
        assert_eq!(cfg.bandwidth, "100M");
        assert_eq!(WINDOWS[cfg.window], "512K");
        assert_eq!(MTUS[cfg.mtu], "1500");
        assert_eq!(cfg.buffer, "65536");
        assert!(cfg.omit);
        assert!(cfg.json_output);
    }

    #[test]
    fn config_apply_defaults_with_an_empty_config_changes_nothing() {
        let mut cfg = ConfigState::default();
        let before = cfg.to_defaults();
        cfg.apply_defaults(&ClientDefaults::default());
        assert_eq!(cfg.to_defaults(), before);
    }

    #[test]
    fn config_apply_defaults_ignores_a_window_or_mtu_not_in_the_dropdown_list() {
        // A hand-edited config file could contain e.g. "1000K" or 3000,
        // neither of which is one of the TUI's fixed WINDOWS/MTUS options —
        // that must leave the dropdown at its current value, not panic or
        // silently pick something wrong.
        let mut cfg = ConfigState::default();
        let before_window = cfg.window;
        let before_mtu = cfg.mtu;
        cfg.apply_defaults(&ClientDefaults {
            window: Some("1000K".into()),
            mtu: Some(3000),
            ..Default::default()
        });
        assert_eq!(cfg.window, before_window);
        assert_eq!(cfg.mtu, before_mtu);
    }

    #[test]
    fn config_to_defaults_round_trips_through_apply_defaults() {
        let mut cfg = ConfigState::default();
        cfg.host = "192.168.1.1".into();
        cfg.port = "9000".into();
        cfg.direction = 2; // RX only
        cfg.protocol = 1; // UDP

        let saved = cfg.to_defaults();
        let mut restored = ConfigState::default();
        restored.apply_defaults(&saved);

        assert_eq!(restored.host, cfg.host);
        assert_eq!(restored.port, cfg.port);
        assert_eq!(restored.direction, cfg.direction);
        assert_eq!(restored.protocol, cfg.protocol);
    }

    #[test]
    fn config_tab_cycles_focus_through_all_fields_and_wraps() {
        let mut cfg = ConfigState::default();
        assert_eq!(cfg.focus, 0);
        for expected in 1..Field::ALL.len() {
            cfg.on_key(key(KeyCode::Tab));
            assert_eq!(cfg.focus, expected);
        }
        cfg.on_key(key(KeyCode::Tab));
        assert_eq!(cfg.focus, 0, "Tab past the last field should wrap to the first");

        cfg.on_key(key(KeyCode::BackTab));
        assert_eq!(
            cfg.focus,
            Field::ALL.len() - 1,
            "BackTab from the first field should wrap to the last"
        );
    }

    #[test]
    fn config_duration_slider_clamps_to_its_range() {
        let mut cfg = ConfigState::default();
        cfg.duration = 5;
        cfg.focus = Field::ALL.iter().position(|f| *f == Field::Duration).unwrap();

        cfg.on_key(key(KeyCode::Left));
        assert_eq!(cfg.duration, 5, "must not clamp below the minimum");

        for _ in 0..100 {
            cfg.on_key(key(KeyCode::Right));
        }
        assert_eq!(cfg.duration, 300, "must not clamp above the maximum");
    }

    #[test]
    fn config_parallel_slider_clamps_to_its_range() {
        let mut cfg = ConfigState::default();
        cfg.focus = Field::ALL.iter().position(|f| *f == Field::Parallel).unwrap();

        cfg.on_key(key(KeyCode::Left));
        assert_eq!(cfg.parallel, 1, "must not clamp below the minimum");

        for _ in 0..100 {
            cfg.on_key(key(KeyCode::Right));
        }
        assert_eq!(cfg.parallel, 32, "must not clamp above the maximum");
    }

    #[test]
    fn config_protocol_cycles_and_wraps() {
        let mut cfg = ConfigState::default();
        cfg.focus = Field::ALL.iter().position(|f| *f == Field::Protocol).unwrap();
        assert_eq!(cfg.protocol, 0);

        cfg.on_key(key(KeyCode::Right));
        assert_eq!(cfg.protocol, 1);
        cfg.on_key(key(KeyCode::Right));
        assert_eq!(cfg.protocol, 2);
        cfg.on_key(key(KeyCode::Right));
        assert_eq!(cfg.protocol, 0, "should wrap back to TCP after QUIC");

        cfg.on_key(key(KeyCode::Left));
        assert_eq!(cfg.protocol, 2, "should wrap backward to QUIC");
    }

    #[test]
    fn config_checkbox_toggles_with_space() {
        let mut cfg = ConfigState::default();
        assert!(!cfg.json_output);
        cfg.focus = Field::ALL.iter().position(|f| *f == Field::Json).unwrap();

        cfg.on_key(key(KeyCode::Char(' ')));
        assert!(cfg.json_output);
        cfg.on_key(key(KeyCode::Char(' ')));
        assert!(!cfg.json_output);
    }

    #[test]
    fn config_reverse_mode_checkbox_is_a_real_view_onto_direction_not_a_dead_toggle() {
        let mut cfg = ConfigState::default();
        cfg.focus = Field::ALL.iter().position(|f| *f == Field::Reverse).unwrap();
        assert_eq!(cfg.direction, 0, "starts on bidir");
        assert!(!(cfg.direction == 2), "checkbox should read unchecked while direction isn't Rx");

        cfg.on_key(key(KeyCode::Char(' ')));
        assert_eq!(
            cfg.direction, 2,
            "checking it should switch direction to RX only (server sends)"
        );

        cfg.on_key(key(KeyCode::Char(' ')));
        assert_eq!(
            cfg.direction, 1,
            "unchecking it should switch direction back to TX only, never back to bidir"
        );
    }

    #[test]
    fn config_reverse_mode_checkbox_and_direction_dropdown_share_one_source_of_truth() {
        // Changing the Direction dropdown directly must be reflected by the
        // checkbox's real (not independently-stored) state, and vice versa
        // — there's no separate `reverse: bool` left to drift out of sync.
        let mut cfg = ConfigState::default();
        cfg.focus = Field::ALL.iter().position(|f| *f == Field::Direction).unwrap();
        cfg.on_key(key(KeyCode::Right)); // bidir(0) -> TX only(1)
        cfg.on_key(key(KeyCode::Right)); // TX only(1) -> RX only(2)
        assert_eq!(cfg.direction, 2);

        cfg.focus = Field::ALL.iter().position(|f| *f == Field::Reverse).unwrap();
        cfg.on_key(key(KeyCode::Char(' '))); // uncheck "Reverse mode"
        assert_eq!(
            cfg.direction, 1,
            "the dropdown's RX-only selection should be exactly what the checkbox toggles off"
        );
    }

    #[test]
    fn config_text_field_edit_commit_and_cancel() {
        let mut cfg = ConfigState::default();
        cfg.focus = Field::ALL.iter().position(|f| *f == Field::Host).unwrap();
        let original = cfg.host.clone();

        // Enter edit mode, replace the content, cancel — original is kept.
        cfg.on_key(key(KeyCode::Enter));
        assert!(cfg.editing);
        for _ in 0..original.len() {
            cfg.on_key(key(KeyCode::Backspace));
        }
        cfg.on_key(key(KeyCode::Char('1')));
        cfg.on_key(key(KeyCode::Char('0')));
        cfg.on_key(key(KeyCode::Esc));
        assert!(!cfg.editing);
        assert_eq!(cfg.host, original, "Esc must discard the edit");

        // Enter edit mode again, this time commit with Enter.
        cfg.on_key(key(KeyCode::Enter));
        for _ in 0..cfg.edit_buffer.len() {
            cfg.on_key(key(KeyCode::Backspace));
        }
        cfg.on_key(key(KeyCode::Char('x')));
        cfg.on_key(key(KeyCode::Char('y')));
        cfg.on_key(key(KeyCode::Char('z')));
        cfg.on_key(key(KeyCode::Enter));
        assert!(!cfg.editing);
        assert_eq!(cfg.host, "xyz", "Enter must commit the edit");
    }

    #[test]
    fn config_f5_starts_test_from_any_field_and_ctrl_enter_is_equivalent() {
        let mut cfg = ConfigState::default();
        cfg.focus = 3;
        assert!(matches!(cfg.on_key(key(KeyCode::F(5))), ConfigAction::StartTest));

        let mut cfg2 = ConfigState::default();
        assert!(matches!(cfg2.on_key(ctrl_key(KeyCode::Enter)), ConfigAction::StartTest));
    }

    #[test]
    fn config_ctrl_s_saves_and_shows_a_success_toast() {
        // `config::save` is a no-op under `cfg(test)` (see its doc comment)
        // — this exercises the real Ctrl+S code path in `on_key` without
        // touching the real user's config file.
        let mut cfg = ConfigState::default();
        cfg.host = "10.0.0.9".into();
        assert!(cfg.toast.is_none());
        cfg.on_key(ctrl_key(KeyCode::Char('s')));
        let toast = cfg.toast.expect("Ctrl+S should always set a toast, success or failure");
        assert!(toast.starts_with("config saved to"), "unexpected toast: {toast}");
    }

    #[test]
    fn server_config_ctrl_s_requests_a_save_and_round_trips_through_defaults() {
        let mut cfg = ServerConfigState::default();
        assert!(matches!(cfg.on_key(ctrl_key(KeyCode::Char('s'))), ServerConfigAction::Save));

        cfg.bind = "127.0.0.1".into();
        cfg.port = "6001".into();
        cfg.protocol = 1;
        cfg.one_off = true;
        let mut restored = ServerConfigState::default();
        restored.apply_defaults(&cfg.to_defaults());
        assert_eq!(
            (restored.bind.as_str(), restored.port.as_str(), restored.protocol, restored.one_off),
            ("127.0.0.1", "6001", 1, true)
        );
    }

    #[test]
    fn config_plain_enter_on_non_text_field_does_not_start_test() {
        let mut cfg = ConfigState::default();
        cfg.focus = Field::ALL.iter().position(|f| *f == Field::Protocol).unwrap();
        assert!(matches!(cfg.on_key(key(KeyCode::Enter)), ConfigAction::None));
    }

    #[test]
    fn config_f5_while_editing_is_swallowed_by_the_text_buffer() {
        // While editing, every key including F5 must go into the text
        // buffer rather than triggering navigation/actions — otherwise
        // there'd be no way to type an 'F' followed by other characters,
        // and F5 specifically must not accidentally abort an in-progress edit.
        let mut cfg = ConfigState::default();
        cfg.focus = Field::ALL.iter().position(|f| *f == Field::Host).unwrap();
        cfg.on_key(key(KeyCode::Enter));
        assert!(cfg.editing);
        assert!(matches!(cfg.on_key(key(KeyCode::F(5))), ConfigAction::None));
        assert!(cfg.editing, "F5 must not fall through to StartTest while editing");
    }

    // ── ServerConfigState ───────────────────────────────────────────────

    #[test]
    fn server_config_apply_defaults_overrides_only_the_fields_the_config_sets() {
        let mut cfg = ServerConfigState::default();
        let d = ServerDefaults {
            bind: Some("127.0.0.1".into()),
            port: Some(6001),
            udp: Some(true),
            quic: None,
            window: Some("512K".into()),
            json: Some(true),
            one_off: Some(true),
        };
        cfg.apply_defaults(&d);

        assert_eq!(cfg.bind, "127.0.0.1");
        assert_eq!(cfg.port, "6001");
        assert_eq!(cfg.protocol, 1, "udp: true should select the UDP entry");
        assert_eq!(WINDOWS[cfg.window], "512K");
        assert!(cfg.json_output);
        assert!(cfg.one_off);
    }

    #[test]
    fn server_config_apply_defaults_with_an_empty_config_changes_nothing() {
        let mut cfg = ServerConfigState::default();
        cfg.apply_defaults(&ServerDefaults::default());
        let default = ServerConfigState::default();
        assert_eq!(cfg.bind, default.bind);
        assert_eq!(cfg.port, default.port);
        assert_eq!(cfg.protocol, default.protocol);
        assert_eq!(cfg.window, default.window);
        assert_eq!(cfg.json_output, default.json_output);
        assert_eq!(cfg.one_off, default.one_off);
    }

    #[test]
    fn server_config_tab_wraps_and_f5_starts() {
        let mut cfg = ServerConfigState::default();
        for expected in 1..ServerField::ALL.len() {
            cfg.on_key(key(KeyCode::Tab));
            assert_eq!(cfg.focus, expected);
        }
        cfg.on_key(key(KeyCode::Tab));
        assert_eq!(cfg.focus, 0);

        assert!(matches!(cfg.on_key(key(KeyCode::F(5))), ServerConfigAction::Start));
    }

    #[test]
    fn server_config_one_off_checkbox_toggles() {
        let mut cfg = ServerConfigState::default();
        assert!(!cfg.one_off);
        cfg.focus = ServerField::ALL.iter().position(|f| *f == ServerField::OneOff).unwrap();
        cfg.on_key(key(KeyCode::Char(' ')));
        assert!(cfg.one_off);
    }

    // ── HistoryState ────────────────────────────────────────────────────

    fn dummy_run(target: &str) -> HistoryRun {
        HistoryRun {
            target: target.into(),
            tag: "TCP TX ×1".into(),
            tag_color_idx: 0,
            summary: String::new(),
            time: "00:00".into(),
            peak_gbps: 1.0,
            avg_gbps: 1.0,
            peak_rx_gbps: None,
            avg_rx_gbps: None,
            rtt_ms: 0.1,
            duration_secs: 1,
            streams: 1,
            window: "256K".into(),
            mtu: 1500,
            retransmits: 0,
            chart: Vec::new(),
            chart_rx: Vec::new(),
            settings: None,
        }
    }

    #[test]
    fn history_navigation_does_not_go_out_of_bounds() {
        let mut hist =
            HistoryState { runs: vec![dummy_run("a"), dummy_run("b")], selected: 0, toast: None };
        hist.on_key(key(KeyCode::Up));
        assert_eq!(hist.selected, 0, "must not go below zero");

        hist.on_key(key(KeyCode::Down));
        assert_eq!(hist.selected, 1);
        hist.on_key(key(KeyCode::Down));
        assert_eq!(hist.selected, 1, "must not go past the last run");
    }

    #[test]
    fn history_delete_removes_run_and_clamps_selection() {
        let mut hist =
            HistoryState { runs: vec![dummy_run("a"), dummy_run("b")], selected: 1, toast: None };
        hist.on_key(key(KeyCode::Char('d')));
        assert_eq!(hist.runs.len(), 1);
        assert_eq!(hist.runs[0].target, "a");
        assert_eq!(hist.selected, 0, "deleting the last row should move selection back");

        hist.on_key(key(KeyCode::Char('d')));
        assert!(hist.runs.is_empty());
        assert_eq!(hist.selected, 0);

        // Deleting from an empty list must be a no-op, not a panic.
        hist.on_key(key(KeyCode::Char('d')));
        assert!(hist.runs.is_empty());
    }

    // ── LiveState ───────────────────────────────────────────────────────

    #[test]
    fn live_stop_key_is_a_noop_toast_when_idle() {
        let mut live = LiveState::default();
        assert!(!live.running && !live.connecting);
        assert!(matches!(live.on_key(key(KeyCode::Char('s'))), LiveAction::None));
        assert_eq!(live.toast.as_deref(), Some("no test running"));
    }

    #[test]
    fn live_stop_key_requests_stop_when_running() {
        let stop = Arc::new(AtomicBool::new(false));
        let mut live = LiveState::connecting(
            ConnectingParams {
                target: "127.0.0.1:5201".into(),
                proto: "TCP".into(),
                direction: Direction::Tx,
                stream_count: 1,
                window: "256K".into(),
                mtu: 1500,
                omit_enabled: false,
                settings: Default::default(),
            },
            stop,
        );
        live.running = true;
        assert!(matches!(live.on_key(key(KeyCode::Char('s'))), LiveAction::Stop));
    }

    #[test]
    fn live_restart_key_always_requests_restart() {
        let mut live = LiveState::default();
        assert!(matches!(live.on_key(key(KeyCode::Char('r'))), LiveAction::Restart));
    }

    #[test]
    fn live_export_without_a_test_shows_a_toast_and_touches_no_disk() {
        let mut live = LiveState::default();
        assert!(live.target.is_empty());
        live.on_key(key(KeyCode::Char('e')));
        assert_eq!(live.toast.as_deref(), Some("no test to export yet"));
    }

    #[test]
    fn live_snapshot_reflects_current_totals() {
        let stop = Arc::new(AtomicBool::new(false));
        let mut live = LiveState::connecting(
            ConnectingParams {
                target: "10.0.0.1:5201".into(),
                proto: "UDP".into(),
                direction: Direction::Tx,
                stream_count: 2,
                window: "auto".into(),
                mtu: 9000,
                omit_enabled: false,
                settings: Default::default(),
            },
            stop,
        );
        live.peak_tx_gbps = 3.5;
        live.peak_tx_after_omit = 3.5;
        live.tx_total_gb = 1.0;
        live.lat = 0.42;
        live.rows[0].retrans = 3;
        live.rows[1].retrans = 4;

        let snap = live.snapshot();
        assert_eq!(snap.target, "10.0.0.1:5201");
        assert_eq!(snap.tag, "UDP TX ×2");
        assert_eq!(snap.tag_color_idx, 1, "UDP should map to the teal tag color");
        assert_eq!(snap.peak_gbps, 3.5);
        assert_eq!(snap.rtt_ms, 0.42);
        assert_eq!(snap.window, "auto");
        assert_eq!(snap.mtu, 9000);
        assert_eq!(snap.retransmits, 7, "should sum retransmits across all per-stream rows");
    }

    #[test]
    fn live_snapshot_with_omit_enabled_averages_only_bytes_after_the_ramp_up() {
        let stop = Arc::new(AtomicBool::new(false));
        let mut live = LiveState::connecting(
            ConnectingParams {
                target: "10.0.0.1:5201".into(),
                proto: "TCP".into(),
                direction: Direction::Tx,
                stream_count: 1,
                window: "256K".into(),
                mtu: 9000,
                omit_enabled: true,
                settings: Default::default(),
            },
            stop,
        );
        // Simulate having run for 10s: total bytes include a much faster
        // ramp-up-free steady state, plus a slower first few seconds that
        // should be excluded from the reported average.
        live.started_at = Some(Instant::now() - std::time::Duration::from_secs(10));
        live.tx_total_gb = 10.0; // the true, complete total — must stay as-is
        live.tx_bytes_after_omit = 9_000_000_000; // 9 GB counted after the omit window

        let snap = live.snapshot();
        // avg should be ~9GB*8/7s (10s elapsed - 3s omitted), not 10GB*8/10s.
        let expected_avg = 9_000_000_000.0_f64 * 8.0 / 1e9 / 7.0;
        assert!(
            (snap.avg_gbps - expected_avg).abs() < 0.01,
            "expected avg around {expected_avg:.3}, got {}",
            snap.avg_gbps
        );
    }

    #[test]
    fn omit_is_ignored_for_a_run_that_ends_within_the_omit_window() {
        let stop = Arc::new(AtomicBool::new(false));
        let mut live = LiveState::connecting(
            ConnectingParams {
                target: "10.0.0.1:5201".into(),
                proto: "TCP".into(),
                direction: Direction::Tx,
                stream_count: 1,
                window: "256K".into(),
                mtu: 9000,
                omit_enabled: true,
                settings: crate::store::RunSettings {
                    omit: true,
                    interval_secs: 1,
                    ..Default::default()
                },
            },
            stop,
        );
        // Stopped after 2s: everything fell inside the 3s omit window, so
        // nothing is counted "after omit" — yet the run must still report
        // its real average and peak, not 0.
        live.started_at = Some(Instant::now() - std::time::Duration::from_secs(2));
        live.tx_total_gb = 5.0;
        live.peak_tx_gbps = 3.0;

        let snap = live.snapshot();
        assert!((snap.avg_gbps - 5.0 * 8.0 / 2.0).abs() < 0.05, "avg {}", snap.avg_gbps);
        assert_eq!(snap.peak_gbps, 3.0);
        assert!(!snap.settings.unwrap().omit, "history must record that omit was not applied");
    }

    #[test]
    fn jitter_is_averaged_over_streams_that_measure_it_and_charted() {
        let mut app = App::default();
        app.live = LiveState::connecting(
            ConnectingParams {
                target: "x:1".into(),
                proto: "UDP".into(),
                direction: Direction::Rx,
                stream_count: 2,
                window: "256K".into(),
                mtu: 1500,
                omit_enabled: false,
                settings: Default::default(),
            },
            Arc::new(AtomicBool::new(false)),
        );
        assert!(app.live.jitter_ms.is_none(), "no jitter before any stream reports one");
        for (idx, jitter) in [(1u8, 0.30), (0u8, 0.10)] {
            app.handle_net_event(NetEvent::ClientStreamInterval {
                stream_index: idx,
                flow: Flow::Rx,
                interval_bytes: 1000,
                stats: crate::net::StreamStats {
                    gbps: 0.1,
                    elapsed_secs: 1,
                    loss_pct: Some(0.0),
                    jitter_ms: Some(jitter),
                    ..Default::default()
                },
            });
        }
        assert!((app.live.jitter_ms.unwrap() - 0.20).abs() < 1e-9);
        assert_eq!(
            app.live.jitter_hist.len(),
            1,
            "stream 0 drives the history, one point per sample"
        );
        assert_eq!(app.live.rows[1].jitter_ms, Some(0.30));
    }

    #[test]
    fn live_snapshot_without_omit_averages_the_full_total() {
        let stop = Arc::new(AtomicBool::new(false));
        let mut live = LiveState::connecting(
            ConnectingParams {
                target: "10.0.0.1:5201".into(),
                proto: "TCP".into(),
                direction: Direction::Tx,
                stream_count: 1,
                window: "256K".into(),
                mtu: 9000,
                omit_enabled: false,
                settings: Default::default(),
            },
            stop,
        );
        live.started_at = Some(Instant::now() - std::time::Duration::from_secs(10));
        live.tx_total_gb = 10.0;
        live.tx_bytes_after_omit = 9_000_000_000; // should be ignored entirely when omit is off

        let snap = live.snapshot();
        let expected_avg = 10.0 * 8.0 / 10.0;
        assert!(
            (snap.avg_gbps - expected_avg).abs() < 0.01,
            "expected avg around {expected_avg:.3}, got {}",
            snap.avg_gbps
        );
    }

    #[test]
    fn live_snapshot_carries_the_rx_chart_for_bidir_runs() {
        // Regression guard: History used to only ever persist the TX
        // chart, silently dropping RX samples even for bidir/reverse runs.
        let stop = Arc::new(AtomicBool::new(false));
        let mut live = LiveState::connecting(
            ConnectingParams {
                target: "10.0.0.1:5201".into(),
                proto: "TCP".into(),
                direction: Direction::Bidir,
                stream_count: 1,
                window: "256K".into(),
                mtu: 9000,
                omit_enabled: false,
                settings: Default::default(),
            },
            stop,
        );
        live.tx_chart = vec![(0.0, 1.0), (1.0, 1.5)];
        live.rx_chart = vec![(0.0, 0.8), (1.0, 1.2)];

        let snap = live.snapshot();
        assert_eq!(snap.chart, vec![(0.0, 1.0), (1.0, 1.5)]);
        assert_eq!(snap.chart_rx, vec![(0.0, 0.8), (1.0, 1.2)]);
    }
}
