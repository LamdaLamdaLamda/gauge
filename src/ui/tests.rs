//! Render tests: draw each screen into ratatui's in-memory `TestBackend`
//! and assert on the text that ends up on screen. No terminal needed, so
//! they run everywhere (CI included) and catch what the pure state-machine
//! tests can't — wrong labels, missing panels, layouts that panic on a
//! small terminal.

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use super::draw;
use crate::app::{App, CONGESTION, StreamRow, Tab};
use crate::net::Direction;
use crate::store::{HistoryRun, RunSettings};

/// Renders `app` at the given size and returns the screen as one string,
/// a line per terminal row.
fn render(app: &App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| draw(frame, app)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

const W: u16 = 170;
const H: u16 = 50;

/// An app on `tab` with a known, empty history (`App::default()` would
/// load whatever the developer's real history file holds).
fn app_on(tab: Tab) -> App {
    let mut app = App::default();
    app.tab = tab;
    app.history.runs = Vec::new();
    app
}

fn assert_has(screen: &str, needle: &str) {
    assert!(screen.contains(needle), "expected {needle:?} on screen:\n{screen}");
}

fn assert_lacks(screen: &str, needle: &str) {
    assert!(!screen.contains(needle), "did not expect {needle:?} on screen:\n{screen}");
}

fn row(id: u8, jitter_ms: Option<f64>) -> StreamRow {
    StreamRow {
        id,
        tx: 0.0,
        rx: 1.5,
        rtt: 0.3,
        retrans: 0,
        cwnd: 64,
        loss: 0.0,
        jitter_ms,
        degraded: false,
    }
}

fn run(settings: Option<RunSettings>, with_rx: bool) -> HistoryRun {
    HistoryRun {
        target: "10.0.0.5:5201".into(),
        tag: "TCP bidir ×1".into(),
        tag_color_idx: 0,
        summary: "↑1.00 / ↓2.00 Gbit/s avg".into(),
        time: "12:34".into(),
        peak_gbps: 1.5,
        avg_gbps: 1.0,
        peak_rx_gbps: with_rx.then_some(2.5),
        avg_rx_gbps: with_rx.then_some(2.0),
        rtt_ms: 0.4,
        duration_secs: 5,
        streams: 1,
        window: "256K".into(),
        mtu: 9000,
        retransmits: 2,
        chart: vec![(1.0, 1.0), (2.0, 1.4), (3.0, 1.2), (4.0, 1.5), (5.0, 1.1)],
        chart_rx: if with_rx {
            vec![(1.0, 2.0), (2.0, 2.4), (3.0, 2.2), (4.0, 2.5), (5.0, 2.1)]
        } else {
            Vec::new()
        },
        settings,
    }
}

// ── chrome ──────────────────────────────────────────────────────────────

#[test]
fn every_screen_shows_the_tab_bar_with_the_active_tab_content() {
    for (tab, marker) in [
        (Tab::Live, "no test running"),
        (Tab::Config, "Test Parameters"),
        (Tab::History, "RECENT RUNS"),
        (Tab::Server, "Bind Target"),
    ] {
        let screen = render(&app_on(tab), W, H);
        for title in ["Live Test", "Config", "History", "Server Mode"] {
            assert_has(&screen, title);
        }
        assert_has(&screen, marker);
    }
}

#[test]
fn the_footer_lists_the_keys_that_work_on_the_current_screen() {
    let config = render(&app_on(Tab::Config), W, H);
    assert_has(&config, "ctrl+s");
    assert_has(&config, "Start test");

    let server = render(&app_on(Tab::Server), W, H);
    assert_has(&server, "ctrl+s");
    assert_has(&server, "Start server");

    let mut listening = app_on(Tab::Server);
    listening.server.listening = true;
    let listening = render(&listening, W, H);
    assert_has(&listening, "Kill client");
    assert_lacks(&listening, "Start server");
}

#[test]
fn no_screen_panics_or_draws_nothing_on_small_terminals() {
    for tab in Tab::ALL {
        for (w, h) in [(80, 24), (60, 20), (40, 12), (20, 6), (1, 1)] {
            let screen = render(&app_on(tab), w, h);
            assert_eq!(screen.lines().count(), h as usize);
        }
    }
}

// ── Config screen ───────────────────────────────────────────────────────

#[test]
fn config_screen_shows_every_field_and_a_cli_preview() {
    let screen = render(&app_on(Tab::Config), W, H);
    for label in [
        "Host / IP",
        "Port",
        "Protocol",
        "Duration",
        "Parallel streams",
        "Window size",
        "MTU",
        "Direction",
        "Interval (s)",
        "Bandwidth",
        "TOS / DSCP",
        "JSON output",
        "Reverse mode",
        "Omit first 3s",
        "CLI Preview",
        "gauge client",
        "Start Test",
        "Save Profile",
    ] {
        assert_has(&screen, label);
    }
}

#[test]
fn config_labels_say_which_protocol_a_setting_applies_to() {
    let mut app = app_on(Tab::Config);

    app.config.protocol = 0; // TCP
    let tcp = render(&app, W, H);
    assert_has(&tcp, "Bandwidth (UDP only)");
    assert_has(&tcp, "Buffer size (bytes)");
    assert_has(
        &tcp,
        if crate::net::CONGESTION_SETTABLE { "TCP congestion" } else { "Congestion (Linux)" },
    );

    app.config.protocol = 1; // UDP
    let udp = render(&app, W, H);
    assert_has(&udp, "Bandwidth (0=∞)");
    assert_has(&udp, "Buffer (TCP/QUIC)");
    assert_has(&udp, "Congestion (not UDP)");

    app.config.protocol = 2; // QUIC
    let quic = render(&app, W, H);
    assert_has(&quic, "Bandwidth (UDP only)");
    assert_lacks(&quic, "(Linux)");
    assert_lacks(&quic, "(not UDP)");
    assert_has(&quic, "--quic");
}

#[test]
fn config_cli_preview_reflects_the_selected_options() {
    let mut app = app_on(Tab::Config);
    app.config.host = "10.1.2.3".into();
    app.config.interval = "5".into();
    app.config.tos = 2;
    app.config.congestion = CONGESTION.iter().position(|c| *c == "reno").unwrap();
    let screen = render(&app, W, H);
    assert_has(&screen, "--host 10.1.2.3");
    assert_has(&screen, "--interval 5");
    assert_has(&screen, "--tos 0xb8");
    assert_has(&screen, "--congestion reno");
}

#[test]
fn config_cli_preview_is_not_clipped_with_every_option_set() {
    let mut app = app_on(Tab::Config);
    app.config.protocol = 1;
    app.config.bandwidth = "100M".into();
    app.config.direction = 0;
    app.config.interval = "3".into();
    app.config.tos = 1;
    app.config.congestion = 1;
    app.config.json_output = true;
    app.config.omit = true;
    // A tall-but-ordinary terminal: the last flag must still be visible.
    let screen = render(&app, 170, 40);
    for flag in [
        "--udp",
        "--bandwidth 100M",
        "--bidir",
        "--window",
        "--mtu",
        "--buffer",
        "--omit",
        "--interval 3",
        "--tos 0x88",
        "--congestion cubic",
        "--json",
    ] {
        assert_has(&screen, flag);
    }
}

#[test]
fn config_reverse_checkbox_tracks_the_direction_dropdown() {
    let mut app = app_on(Tab::Config);
    app.config.direction = 2; // RX only
    let screen = render(&app, W, H);
    assert_has(&screen, "RX only");
    assert_has(&screen, "[x] server sends");

    app.config.direction = 1; // TX only
    assert_has(&render(&app, W, H), "[ ] server sends");
}

#[test]
fn config_toast_is_shown() {
    let mut app = app_on(Tab::Config);
    app.config.toast = Some("config saved to /tmp/x/config.toml".into());
    assert_has(&render(&app, W, H), "config saved to");
}

// ── Live screen ─────────────────────────────────────────────────────────

#[test]
fn idle_live_screen_points_to_the_config_screen() {
    assert_has(&render(&app_on(Tab::Live), W, H), "no test running — go to Config and press F5");
}

#[test]
fn live_screen_without_measured_jitter_shows_latency_and_a_dash() {
    let mut app = app_on(Tab::Live);
    app.live.target = "10.0.0.5:5201".into();
    app.live.proto = "TCP".into();
    app.live.running = true;
    app.live.rows = vec![row(1, None)];
    let screen = render(&app, W, H);
    assert_has(&screen, "Latency");
    assert_has(&screen, "connect RTT (one-time)");
    assert_lacks(&screen, "receiver-measured");
    assert_has(&screen, "10.0.0.5:5201");
    assert_has(&screen, "running");
}

#[test]
fn live_screen_with_measured_jitter_shows_the_jitter_panel_and_column() {
    let mut app = app_on(Tab::Live);
    app.live.target = "10.0.0.5:5201".into();
    app.live.proto = "UDP".into();
    app.live.direction = Direction::Rx;
    app.live.running = true;
    app.live.lat = 0.84;
    app.live.rows = vec![row(1, Some(0.04)), row(2, Some(0.36))];
    app.live.jitter_ms = Some(0.20);
    app.live.jitter_hist = [10, 20, 30].into_iter().collect();
    let screen = render(&app, W, H);
    assert_has(&screen, "Jitter");
    assert_has(&screen, "receiver-measured");
    assert_has(&screen, "RTT 0.84ms");
    assert_has(&screen, "0.20");
    assert_has(&screen, "0.04ms");
    assert_has(&screen, "0.36ms");
    assert_lacks(&screen, "connect RTT (one-time)");
}

#[test]
fn live_screen_labels_the_unused_direction() {
    let mut app = app_on(Tab::Live);
    app.live.target = "x:1".into();
    app.live.proto = "TCP".into();
    app.live.direction = Direction::Tx;
    assert_has(&render(&app, W, H), "not measured (TX-only)");

    app.live.direction = Direction::Rx;
    assert_has(&render(&app, W, H), "not sent");
}

#[test]
fn live_screen_shows_an_error_state() {
    let mut app = app_on(Tab::Live);
    app.live.target = "x:1".into();
    app.live.error = Some("connect to x:1 failed: Permission denied".into());
    assert_has(&render(&app, W, H), "error");
}

// ── History screen ──────────────────────────────────────────────────────

#[test]
fn empty_history_says_so() {
    let screen = render(&app_on(Tab::History), W, H);
    assert_has(&screen, "no runs yet");
    assert_has(&screen, "0 runs saved");
}

#[test]
fn history_detail_shows_summary_header_and_the_run_settings() {
    let mut app = app_on(Tab::History);
    app.history.runs = vec![run(
        Some(RunSettings {
            buffer_bytes: Some(65536),
            omit: true,
            interval_secs: 2,
            tos: Some(0xB8),
            ..Default::default()
        }),
        true,
    )];
    let screen = render(&app, W, H);
    assert_has(&screen, "10.0.0.5:5201");
    assert_has(&screen, "TCP bidir ×1");
    assert_has(&screen, "2 retransmit(s)");
    assert_has(&screen, "buffer 64K · omit 3s · interval 2s · TOS 0xb8");
    assert_has(&screen, "Peak TX");
    assert_has(&screen, "aggregate TX/RX");
    assert_has(&screen, "1 runs saved");
}

#[test]
fn history_chart_has_an_rx_legend_only_for_runs_that_received() {
    let mut app = app_on(Tab::History);
    app.history.runs = vec![run(None, true)];
    let with_rx = render(&app, W, H);
    assert_has(&with_rx, "RX");

    app.history.runs = vec![run(None, false)];
    let tx_only = render(&app, W, H);
    assert_has(&tx_only, "aggregate TX");
    assert_lacks(&tx_only, "aggregate TX/RX");
}

#[test]
fn history_run_from_before_settings_were_saved_still_renders() {
    let mut app = app_on(Tab::History);
    app.history.runs = vec![run(None, false)];
    let screen = render(&app, W, H);
    assert_has(&screen, "10.0.0.5:5201");
    assert_lacks(&screen, "omit");
}

#[test]
fn history_run_without_samples_says_so_instead_of_drawing_a_chart() {
    let mut app = app_on(Tab::History);
    let mut r = run(None, false);
    r.chart.clear();
    app.history.runs = vec![r];
    assert_has(&render(&app, W, H), "no throughput samples recorded for this run");
}

// ── Server screen ───────────────────────────────────────────────────────

#[test]
fn server_config_screen_shows_its_fields_and_no_dead_mtu_field() {
    let screen = render(&app_on(Tab::Server), W, H);
    for label in [
        "Bind address",
        "Port",
        "Protocol",
        "Window size",
        "JSON output",
        "One-off test",
        "configuring",
    ] {
        assert_has(&screen, label);
    }
    assert_lacks(&screen, "MTU");
}

#[test]
fn listening_server_shows_its_address_and_state() {
    let mut app = app_on(Tab::Server);
    app.server.listening = true;
    app.server.bind = "0.0.0.0:5201".into();
    let screen = render(&app, W, H);
    assert_has(&screen, "listening");
    assert_has(&screen, "0.0.0.0:5201");
}
