use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Cell, LineGauge, Paragraph, Row, Sparkline, Table},
};

use crate::app::LiveState;
use crate::net::Direction;
use crate::theme;
use crate::ui::titled_block;

pub fn draw(frame: &mut Frame, area: Rect, live: &LiveState) {
    let [metrics_area, mid_area, log_area] =
        Layout::vertical([Constraint::Length(9), Constraint::Length(9), Constraint::Min(4)])
            .areas(area);

    let [tx_area, rx_area, lat_area] = Layout::horizontal([
        Constraint::Percentage(34),
        Constraint::Percentage(33),
        Constraint::Percentage(33),
    ])
    .areas(metrics_area);

    let rx_measured = live.direction != Direction::Tx;
    let tx_accent = if live.direction == Direction::Rx {
        format!("{} · not sent", live.proto)
    } else {
        format!("{} · real", live.proto)
    };
    let rx_accent = if rx_measured {
        format!("{} · real", live.proto)
    } else {
        "not measured (TX-only)".to_string()
    };

    let tx_metric = Metric {
        title: "↑ TX Throughput",
        accent: &tx_accent,
        value: live.tx,
        target: 10.0,
        color: theme::BLUE,
        hist: &live.tx_hist,
        unit: "Gbit/s",
        scale_label: "target 10G",
    };
    let rx_metric = Metric {
        title: "↓ RX Throughput",
        accent: &rx_accent,
        value: live.rx,
        target: 10.0,
        color: theme::GREEN,
        hist: &live.rx_hist,
        unit: "Gbit/s",
        scale_label: "target 10G",
    };
    draw_metric(frame, tx_area, &tx_metric);
    draw_metric(frame, rx_area, &rx_metric);
    // Titled "Latency", not "Latency & Jitter" — jitter is a receiver-side
    // UDP metric: the Server Mode screen shows it for client-sent streams,
    // and the CLI prints it for reverse/bidir UDP, but this box has no
    // series for it (and TCP has no jitter concept here).
    // Jitter is receiver-side: this client only measures it for UDP streams
    // it receives (reverse/bidir) — then it replaces the one-time connect
    // RTT in this box (the RTT moves into the accent text). Otherwise the
    // box stays "Latency" and never claims a jitter it doesn't have.
    let jitter_accent = format!("{} · receiver-measured · RTT {:.2}ms", live.proto, live.lat);
    let lat_metric = match live.jitter_ms {
        Some(j) => Metric {
            title: "Jitter",
            accent: &jitter_accent,
            value: j,
            target: 5.0,
            color: theme::TEAL,
            hist: &live.jitter_hist,
            unit: "ms",
            scale_label: "scale 5ms",
        },
        None => Metric {
            title: "Latency",
            accent: "connect RTT (one-time)",
            value: live.lat,
            target: 5.0,
            color: theme::TEAL,
            hist: &live.lat_hist,
            unit: "ms",
            scale_label: "p99: n/a",
        },
    };
    draw_metric(frame, lat_area, &lat_metric);

    let [table_area, stats_area] =
        Layout::horizontal([Constraint::Percentage(65), Constraint::Percentage(35)])
            .areas(mid_area);
    draw_stream_table(frame, table_area, live);
    draw_stats(frame, stats_area, live);

    draw_log(frame, log_area, live);

    if live.target.is_empty() {
        draw_idle_hint(frame, area);
    }
}

fn draw_idle_hint(frame: &mut Frame, area: Rect) {
    let msg = "no test running — go to Config and press F5 to start one";
    let hint_area = Rect { x: area.x, y: area.y + area.height / 2, width: area.width, height: 1 };
    frame.render_widget(
        Paragraph::new(Line::from(msg).centered())
            .style(Style::new().fg(theme::OVERLAY0).bg(theme::BASE)),
        hint_area,
    );
}

/// One throughput/latency panel: a headline value, a gauge toward `target`
/// and a sparkline of `hist`.
struct Metric<'a> {
    title: &'a str,
    accent: &'a str,
    value: f64,
    target: f64,
    color: ratatui::style::Color,
    hist: &'a std::collections::VecDeque<u64>,
    unit: &'a str,
    scale_label: &'a str,
}

fn draw_metric(frame: &mut Frame, area: Rect, m: &Metric) {
    let Metric { title, accent, value, target, color, hist, unit, scale_label } = *m;
    let block = titled_block(title, Some(accent));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [value_area, gauge_area, scale_area, spark_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(2),
    ])
    .areas(inner);

    let value_line = Line::from(vec![
        Span::styled(format!("{value:.2} "), Style::new().fg(color).bold()),
        Span::styled(unit, Style::new().fg(theme::SUBTEXT)),
    ]);
    frame.render_widget(Paragraph::new(value_line), value_area);

    let ratio = (value / target).clamp(0.0, 1.0);
    let gauge = LineGauge::default()
        .filled_style(Style::new().fg(color))
        .unfilled_style(Style::new().fg(theme::SURFACE0))
        .ratio(ratio)
        .label("");
    frame.render_widget(gauge, gauge_area);

    let scale_line = Line::from(vec![
        Span::styled("0", Style::new().fg(theme::OVERLAY0)),
        Span::raw(" ".repeat((scale_area.width as usize).saturating_sub(2 + scale_label.len()))),
        Span::styled(scale_label, Style::new().fg(color)),
    ]);
    frame.render_widget(Paragraph::new(scale_line), scale_area);

    // `hist` starts empty and can grow well past the render width (see
    // `SPARK_LEN`'s doc comment) — `spark_window` picks the tail that fits
    // it. Early on (fewer samples than the box is wide) that's the whole
    // history, so `Sparkline` — which always draws starting at its area's
    // left edge — draws a curve that visibly *grows* from x=0 as real
    // samples arrive. Only once there's more history than the box can show
    // does this start showing a scrolling, right-anchored window instead.
    let skip = spark_window_skip(hist.len(), spark_area.width as usize);
    let data: Vec<u64> = hist.iter().skip(skip).copied().collect();
    let spark = Sparkline::default().data(&data).style(Style::new().fg(color));
    frame.render_widget(spark, spark_area);
}

/// How many of the oldest samples to skip so the remaining tail is exactly
/// what fits in `width` columns — `0` (skip nothing, show the whole
/// history) while it's shorter than the box, then grows to keep only the
/// most recent `width` samples once there's more history than that.
fn spark_window_skip(hist_len: usize, width: usize) -> usize {
    hist_len.saturating_sub(width)
}

fn draw_stream_table(frame: &mut Frame, area: Rect, live: &LiveState) {
    let block = titled_block("Per-Stream Breakdown", Some(&format!("{} stream(s)", live.streams)));
    let header = Row::new(vec![
        "Stream", "TX Gb/s", "RX Gb/s", "RTT ms", "Retr", "Cwnd KB", "Loss", "Jitter", "State",
    ])
    .style(Style::new().fg(theme::OVERLAY0));
    let rows = live.rows.iter().map(|r| {
        let (loss_style, state_style, state_txt) = if r.degraded {
            (theme::YELLOW, theme::YELLOW, "degraded")
        } else {
            (theme::GREEN, theme::GREEN, "ok")
        };
        Row::new(vec![
            Cell::from(format!("#{}", r.id)).style(Style::new().fg(theme::BLUE).bold()),
            Cell::from(format!("{:.2}", r.tx)).style(Style::new().fg(theme::BLUE)),
            Cell::from(format!("{:.2}", r.rx)).style(Style::new().fg(theme::GREEN)),
            Cell::from(format!("{:.2}", r.rtt)).style(Style::new().fg(theme::TEAL)),
            Cell::from(r.retrans.to_string()).style(Style::new().fg(theme::COMMENT)),
            Cell::from(r.cwnd.to_string()).style(Style::new().fg(theme::SUBTEXT)),
            Cell::from(format!("{:.2}%", r.loss * 100.0)).style(Style::new().fg(loss_style)),
            Cell::from(r.jitter_ms.map_or("–".to_string(), |j| format!("{j:.2}ms")))
                .style(Style::new().fg(theme::TEAL)),
            Cell::from(state_txt).style(Style::new().fg(state_style)),
        ])
    });
    let widths = [
        Constraint::Length(7),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(7),
        Constraint::Length(5),
        Constraint::Length(8),
        Constraint::Length(7),
        Constraint::Length(8),
        Constraint::Length(9),
    ];
    let table = Table::new(rows, widths).header(header).block(block);
    frame.render_widget(table, area);
}

fn draw_stats(frame: &mut Frame, area: Rect, live: &LiveState) {
    let block = titled_block("Test Stats", None);
    let elapsed = live.started_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);
    let retransmits: u32 = live.rows.iter().map(|r| r.retrans).sum();
    let mut rows = vec![
        Row::new(vec!["Protocol".to_string(), live.proto.clone()]),
        Row::new(vec!["Duration".to_string(), crate::util::fmt_hms(elapsed)]),
        Row::new(vec!["Total TX".to_string(), format!("{:.2} GB", live.tx_total_gb)]),
    ];
    if live.direction != Direction::Tx {
        rows.push(Row::new(vec!["Total RX".to_string(), format!("{:.2} GB", live.rx_total_gb)]));
    }
    rows.push(Row::new(vec!["Retransmits".to_string(), retransmits.to_string()]));
    rows.push(Row::new(vec!["Streams".to_string(), live.streams.to_string()]));
    rows.push(Row::new(vec!["MTU".to_string(), live.mtu.to_string()]));
    let rows = rows.into_iter().map(|r| r.style(Style::new().fg(theme::SUBTEXT)));
    let widths = [Constraint::Percentage(55), Constraint::Percentage(45)];
    let table = Table::new(rows, widths).block(block);
    frame.render_widget(table, area);
}

fn draw_log(frame: &mut Frame, area: Rect, live: &LiveState) {
    let block = titled_block("Event Log", Some("live · last 60s"));
    let inner_height = area.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = live
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

#[cfg(test)]
mod tests {
    use super::*;

    // Regression guard for the sparkline "grows from the middle towards
    // the left instead of from the left towards the right" bug: with
    // history shorter than the box, nothing should be skipped — the whole
    // (short) history renders left-aligned, growing rightward as more
    // arrives. Only once there's more history than the box is wide should
    // it start showing a scrolling, right-anchored window.

    #[test]
    fn spark_window_skip_shows_all_history_while_it_fits_the_box() {
        assert_eq!(spark_window_skip(0, 50), 0, "no samples yet — nothing to skip");
        assert_eq!(
            spark_window_skip(1, 50),
            0,
            "one sample in a wide box should render at the left edge, not skipped"
        );
        assert_eq!(spark_window_skip(49, 50), 0, "history shorter than the box — show all of it");
        assert_eq!(spark_window_skip(50, 50), 0, "history exactly fills the box — show all of it");
    }

    #[test]
    fn spark_window_skip_scrolls_once_history_exceeds_the_box() {
        assert_eq!(spark_window_skip(51, 50), 1, "one sample over — skip exactly the oldest one");
        assert_eq!(spark_window_skip(300, 50), 250, "keep only the most recent `width` samples");
    }
}
