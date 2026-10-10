use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Cell, Paragraph, Row, Table},
};

use crate::app::{PROTOCOLS, ServerConfigState, ServerField, ServerState, WINDOWS};
use crate::theme;
use crate::ui::fields::{
    checkbox, edit_or, field, render_action_button, render_cli_preview, select_value,
};
use crate::ui::titled_block;
use crate::util::fmt_hms;

pub fn draw(frame: &mut Frame, area: Rect, srv: &ServerState) {
    if srv.listening {
        draw_running(frame, area, srv);
    } else {
        draw_config(frame, area, &srv.config);
    }
}

fn draw_running(frame: &mut Frame, area: Rect, srv: &ServerState) {
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(area);

    let [status_area, session_area] =
        Layout::vertical([Constraint::Length(9), Constraint::Min(7)]).areas(left);
    draw_status(frame, status_area, srv);
    draw_session(frame, session_area, srv);
    draw_log(frame, right, srv);
}

fn draw_status(frame: &mut Frame, area: Rect, srv: &ServerState) {
    let (label, color) = if srv.listening {
        ("● listening", theme::GREEN)
    } else {
        ("○ stopped", theme::OVERLAY0)
    };
    let uptime = srv.listening_since.map(|t| t.elapsed().as_secs()).unwrap_or(0);
    let rows = vec![
        Row::new(vec!["Status".to_string(), label.to_string()]).style(Style::new().fg(color)),
        Row::new(vec!["Bind address".to_string(), format!("{} ({})", srv.bind, srv.active_proto)])
            .style(Style::new().fg(theme::SUBTEXT)),
        Row::new(vec!["Active clients".to_string(), srv.active_clients.to_string()])
            .style(Style::new().fg(theme::BLUE)),
        Row::new(vec!["Total sessions".to_string(), srv.total_sessions.to_string()])
            .style(Style::new().fg(theme::SUBTEXT)),
        Row::new(vec!["Uptime".to_string(), fmt_hms(uptime)])
            .style(Style::new().fg(theme::SUBTEXT)),
    ];
    let widths = [Constraint::Percentage(55), Constraint::Percentage(45)];
    frame.render_widget(Table::new(rows, widths).block(titled_block("Server Status", None)), area);
}

fn draw_session(frame: &mut Frame, area: Rect, srv: &ServerState) {
    match &srv.session {
        Some(session) => {
            let accent =
                format!("{} stream(s) · {}s target", session.stream_count, session.total_secs);
            let block = titled_block("Active Session", Some(&accent));
            let mut rows: Vec<_> = session.streams.values().collect();
            rows.sort_by_key(|r| r.stream_index);

            let header = Row::new(vec![
                "Stream",
                "Client",
                "TX Gb/s",
                "RX Gb/s",
                "Loss",
                "Jitter ms",
                "Dur",
                "State",
            ])
            .style(Style::new().fg(theme::OVERLAY0));
            let table_rows = rows.into_iter().map(|r| {
                let (state_color, state_txt) = if r.finished {
                    (theme::OVERLAY0, "done")
                } else if r.loss_pct > 1.0 || r.jitter_ms > 5.0 {
                    (theme::YELLOW, "degraded")
                } else {
                    (theme::GREEN, "ok")
                };
                Row::new(vec![
                    Cell::from(format!("#{}", r.stream_index + 1))
                        .style(Style::new().fg(theme::BLUE).bold()),
                    Cell::from(r.addr.clone()).style(Style::new().fg(theme::SUBTEXT)),
                    Cell::from(format!("{:.2}", r.tx)).style(Style::new().fg(theme::BLUE)),
                    Cell::from(format!("{:.2}", r.rx)).style(Style::new().fg(theme::GREEN)),
                    Cell::from(format!("{:.2}%", r.loss_pct)).style(Style::new().fg(theme::YELLOW)),
                    Cell::from(format!("{:.2}", r.jitter_ms)).style(Style::new().fg(theme::TEAL)),
                    Cell::from(fmt_hms(r.connected_at.elapsed().as_secs()))
                        .style(Style::new().fg(theme::SUBTEXT)),
                    Cell::from(state_txt).style(Style::new().fg(state_color)),
                ])
            });
            let widths = [
                Constraint::Length(7),
                Constraint::Length(21),
                Constraint::Length(8),
                Constraint::Length(8),
                Constraint::Length(7),
                Constraint::Length(9),
                Constraint::Length(8),
                Constraint::Length(9),
            ];
            frame.render_widget(Table::new(table_rows, widths).header(header).block(block), area);
        }
        None => {
            let block = titled_block("Active Session", None);
            let msg = "waiting for connections…";
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(msg, Style::new().fg(theme::OVERLAY0))))
                    .block(block),
                area,
            );
        }
    }
}

fn draw_log(frame: &mut Frame, area: Rect, srv: &ServerState) {
    let block = titled_block("Server Log", Some("live"));
    let inner_height = area.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = srv
        .log
        .iter()
        .take(inner_height)
        .map(|l| {
            Line::from(vec![
                Span::styled(format!("{} ", l.ts), Style::new().fg(theme::OVERLAY0)),
                Span::styled(
                    format!("[{:<4}] ", l.level.label()),
                    Style::new().fg(l.level.color()),
                ),
                Span::styled(l.text.clone(), Style::new().fg(theme::TEXT)),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_config(frame: &mut Frame, area: Rect, cfg: &ServerConfigState) {
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(area);

    let [bind_area, _spacer] =
        Layout::vertical([Constraint::Length(6), Constraint::Min(0)]).areas(left);
    draw_bind(frame, bind_area, cfg);

    let [options_area, cli_area, buttons_area] = Layout::vertical([
        Constraint::Length(8),
        Constraint::Length(cfg.cli_preview().len() as u16 + 2),
        Constraint::Length(3),
    ])
    .areas(right);
    draw_options(frame, options_area, cfg);
    render_cli_preview(frame, cli_area, cfg.cli_preview());
    render_action_button(frame, buttons_area, "▶ Start Server  (f5)");
}

fn draw_bind(frame: &mut Frame, area: Rect, cfg: &ServerConfigState) {
    let f = ServerField::ALL[cfg.focus];
    let lines = vec![
        field(
            "Bind address",
            edit_or(cfg.editing, &cfg.edit_buffer, f == ServerField::Bind, &cfg.bind),
            f == ServerField::Bind,
            f == ServerField::Bind && cfg.editing,
        ),
        field(
            "Port",
            edit_or(cfg.editing, &cfg.edit_buffer, f == ServerField::Port, &cfg.port),
            f == ServerField::Port,
            f == ServerField::Port && cfg.editing,
        ),
        field(
            "Protocol",
            select_value(&PROTOCOLS, cfg.protocol),
            f == ServerField::Protocol,
            false,
        ),
    ];
    frame.render_widget(Paragraph::new(lines).block(titled_block("Bind Target", None)), area);
}

fn draw_options(frame: &mut Frame, area: Rect, cfg: &ServerConfigState) {
    let f = ServerField::ALL[cfg.focus];
    let lines = vec![
        field("Window size", select_value(&WINDOWS, cfg.window), f == ServerField::Window, false),
        field("JSON output", checkbox(cfg.json_output, "(--json)"), f == ServerField::Json, false),
        field(
            "One-off test",
            checkbox(cfg.one_off, "stop after first test"),
            f == ServerField::OneOff,
            false,
        ),
    ];
    frame.render_widget(Paragraph::new(lines).block(titled_block("Server Options", None)), area);
}
