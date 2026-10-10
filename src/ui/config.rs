use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    widgets::Paragraph,
};

use crate::app::{CONGESTION, ConfigState, DIRECTIONS, Field, MTUS, PROTOCOLS, TOS, WINDOWS};
use crate::ui::fields::{
    checkbox, edit_or, field, render_action_button, render_cli_preview, select_value, slider_value,
};
use crate::ui::titled_block;

pub fn draw(frame: &mut Frame, area: Rect, cfg: &ConfigState) {
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(area);

    let [target_area, params_area] =
        Layout::vertical([Constraint::Length(6), Constraint::Min(10)]).areas(left);
    draw_target(frame, target_area, cfg);
    draw_params(frame, params_area, cfg);

    let [advanced_area, cli_area, buttons_area] = Layout::vertical([
        Constraint::Length(11),
        // Takes whatever height is left: the preview grows a line for every
        // option set, and clipped flags would make it a misleading copy.
        Constraint::Min(10),
        Constraint::Length(3),
    ])
    .areas(right);
    draw_advanced(frame, advanced_area, cfg);
    render_cli_preview(frame, cli_area, cfg.cli_preview());
    draw_buttons(frame, buttons_area, cfg);
}

fn draw_target(frame: &mut Frame, area: Rect, cfg: &ConfigState) {
    let f = Field::ALL[cfg.focus];
    let lines = vec![
        field(
            "Host / IP",
            edit_or(cfg.editing, &cfg.edit_buffer, f == Field::Host, &cfg.host),
            f == Field::Host,
            f == Field::Host && cfg.editing,
        ),
        field(
            "Port",
            edit_or(cfg.editing, &cfg.edit_buffer, f == Field::Port, &cfg.port),
            f == Field::Port,
            f == Field::Port && cfg.editing,
        ),
        field("Protocol", select_value(&PROTOCOLS, cfg.protocol), f == Field::Protocol, false),
    ];
    frame.render_widget(Paragraph::new(lines).block(titled_block("Target", None)), area);
}

fn draw_params(frame: &mut Frame, area: Rect, cfg: &ConfigState) {
    let f = Field::ALL[cfg.focus];
    let lines = vec![
        field(
            "Duration",
            slider_value(cfg.duration as u32, 5, 300, 16, "s"),
            f == Field::Duration,
            false,
        ),
        field(
            "Parallel streams",
            slider_value(cfg.parallel as u32, 1, 32, 16, ""),
            f == Field::Parallel,
            false,
        ),
        field("Window size", select_value(&WINDOWS, cfg.window), f == Field::Window, false),
        field("MTU", select_value(&MTUS, cfg.mtu), f == Field::Mtu, false),
        field("Direction", select_value(&DIRECTIONS, cfg.direction), f == Field::Direction, false),
        field(
            "Interval (s)",
            edit_or(cfg.editing, &cfg.edit_buffer, f == Field::Interval, &cfg.interval),
            f == Field::Interval,
            f == Field::Interval && cfg.editing,
        ),
    ];
    frame.render_widget(Paragraph::new(lines).block(titled_block("Test Parameters", None)), area);
}

fn draw_advanced(frame: &mut Frame, area: Rect, cfg: &ConfigState) {
    let f = Field::ALL[cfg.focus];
    // Pacing only applies to UDP — TCP has its own congestion control and
    // ignores this entirely (see ClientParams::bandwidth_bps only ever
    // being set for Proto::Udp). Still editable either way (so switching
    // to UDP later doesn't lose what you typed), just labeled honestly.
    let bandwidth_label =
        if cfg.protocol == 1 { "Bandwidth (0=∞)" } else { "Bandwidth (UDP only)" };
    // Same reasoning as bandwidth_label above, just flipped — the
    // write/read chunk size only means anything for TCP (UDP already has
    // its own packet-size knob, MTU).
    let buffer_label = if cfg.protocol == 1 { "Buffer (TCP/QUIC)" } else { "Buffer size (bytes)" };
    // TCP's per-socket congestion control exists only on Linux (elsewhere the
    // field is kept so a shared config file round-trips, but labeled, and
    // starting a TCP test with one selected is refused); QUIC's runs in user
    // space and works everywhere; UDP has none.
    let congestion_label = match cfg.protocol {
        1 => "Congestion (not UDP)",
        0 if !crate::net::CONGESTION_SETTABLE => "Congestion (Linux)",
        2 => "Congestion",
        _ => "TCP congestion",
    };
    let lines = vec![
        field(
            bandwidth_label,
            edit_or(cfg.editing, &cfg.edit_buffer, f == Field::Bandwidth, &cfg.bandwidth),
            f == Field::Bandwidth,
            f == Field::Bandwidth && cfg.editing,
        ),
        field("TOS / DSCP", select_value(&TOS, cfg.tos), f == Field::Tos, false),
        field(
            congestion_label,
            select_value(&CONGESTION, cfg.congestion),
            f == Field::Congestion,
            false,
        ),
        field(
            buffer_label,
            edit_or(cfg.editing, &cfg.edit_buffer, f == Field::Buffer, &cfg.buffer),
            f == Field::Buffer,
            f == Field::Buffer && cfg.editing,
        ),
        field("JSON output", checkbox(cfg.json_output, "(--json)"), f == Field::Json, false),
        // A real, second view onto `cfg.direction` (see its doc comment in
        // app.rs) — not an independent, disconnected bool.
        field(
            "Reverse mode",
            checkbox(cfg.direction == 2, "server sends"),
            f == Field::Reverse,
            false,
        ),
        field("Omit first 3s", checkbox(cfg.omit, "TCP ramp-up"), f == Field::Omit, false),
    ];
    frame.render_widget(Paragraph::new(lines).block(titled_block("Advanced", None)), area);
}

fn draw_buttons(frame: &mut Frame, area: Rect, _cfg: &ConfigState) {
    let [start, save] =
        Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(area);
    render_action_button(frame, start, "▶ Start Test  (f5)");
    frame.render_widget(
        Paragraph::new(ratatui::text::Line::from("Save Profile  (ctrl+s)").centered())
            .style(ratatui::style::Style::new().fg(crate::theme::SUBTEXT))
            .block(
                ratatui::widgets::Block::bordered()
                    .border_style(ratatui::style::Style::new().fg(crate::theme::SURFACE1)),
            ),
        save,
    );
}
