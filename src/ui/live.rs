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
        Layout::vertical([Constraint::Length(10), Constraint::Length(9), Constraint::Min(4)])
            .areas(area);

    let [tx_area, rx_area, lat_area] = Layout::horizontal([
        Constraint::Percentage(34),
        Constraint::Percentage(33),
        Constraint::Percentage(33),
    ])
    .areas(metrics_area);

    // One history sample per report interval; the time axis is in seconds.
    let sample_secs = live.settings.as_ref().map_or(1, |s| s.interval_secs.max(1));
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
        hist_per_unit: 100.0,
        sample_secs,
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
        hist_per_unit: 100.0,
        sample_secs,
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
            hist_per_unit: 100.0,
            sample_secs,
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
            hist_per_unit: 100.0,
            sample_secs,
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
    /// How many `hist` units make one `unit` (the buffers hold hundredths).
    hist_per_unit: f64,
    /// Seconds between two `hist` samples — the time axis' scale.
    sample_secs: u64,
    unit: &'a str,
    scale_label: &'a str,
}

fn draw_metric(frame: &mut Frame, area: Rect, m: &Metric) {
    let Metric {
        title,
        accent,
        value,
        target,
        color,
        hist,
        hist_per_unit,
        sample_secs,
        unit,
        scale_label,
    } = *m;
    let block = titled_block(title, Some(accent));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [value_area, gauge_area, chart_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(2)])
            .areas(inner);

    let value_line = Line::from(vec![
        Span::styled(format!("{value:.2} "), Style::new().fg(color).bold()),
        Span::styled(unit, Style::new().fg(theme::SUBTEXT)),
    ]);
    frame.render_widget(Paragraph::new(value_line), value_area);

    // `0 ━━━━━━━━━━──────── target 10G`: the scale label sits on the gauge's own row.
    let label_width = scale_label.chars().count() as u16 + 1;
    let [zero_area, bar_area, label_area] = Layout::horizontal([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(label_width),
    ])
    .areas(gauge_area);
    frame.render_widget(
        Paragraph::new(Span::styled("0", Style::new().fg(theme::OVERLAY0))),
        zero_area,
    );
    let ratio = (value / target).clamp(0.0, 1.0);
    let gauge = LineGauge::default()
        .filled_style(Style::new().fg(color))
        .unfilled_style(Style::new().fg(theme::SURFACE0))
        .ratio(ratio)
        .label("");
    frame.render_widget(gauge, bar_area);
    frame.render_widget(
        Paragraph::new(Span::styled(format!(" {scale_label}"), Style::new().fg(color))),
        label_area,
    );

    draw_chart(frame, chart_area, hist, hist_per_unit, sample_secs, color);
}

/// The sparkline framed by axes:
///
/// ```text
/// 22.0┤▇▇▇█▇▇▅        value axis: ticks on the top and middle rows,
///     │███████        labelled with the value that row's upper edge stands for
/// 11.0┤███████
///     │███████
///    0┼┬────┬───┬      baseline, with a tick for each time label
///      0s  3s  7s     time axis
/// ```
///
/// The top label is the largest *visible* sample — the sparkline scales to
/// exactly that, so the bars and the labels agree.
fn draw_chart(
    frame: &mut Frame,
    area: Rect,
    hist: &std::collections::VecDeque<u64>,
    hist_per_unit: f64,
    sample_secs: u64,
    color: ratatui::style::Color,
) {
    // Axes need room; in a very small panel just draw the bare sparkline.
    let rows = area.height.saturating_sub(2);
    if rows < 1 || area.width < 16 {
        let skip = spark_window_skip(hist.len(), area.width as usize);
        let data: Vec<u64> = hist.iter().skip(skip).copied().collect();
        frame.render_widget(Sparkline::default().data(&data).style(Style::new().fg(color)), area);
        return;
    }

    // Fixed per history (not per visible window) so the plot doesn't shift
    // sideways when old samples scroll out.
    let label_width = axis_value(hist.iter().copied().max().unwrap_or(0) as f64 / hist_per_unit)
        .chars()
        .count()
        .max(4);
    let y_width = label_width as u16 + 1; // labels plus the axis character
    let plot_width = area.width - y_width;

    // `hist` starts empty and can grow well past the render width (see
    // `SPARK_LEN`'s doc comment) — `spark_window_skip` picks the tail that fits
    // it. Early on (fewer samples than the box is wide) that's the whole
    // history, so `Sparkline` — which always draws starting at its area's
    // left edge — draws a curve that visibly *grows* from x=0 as real
    // samples arrive. Only once there's more history than the box can show
    // does this start showing a scrolling, right-anchored window instead.
    let skip = spark_window_skip(hist.len(), plot_width as usize);
    let data: Vec<u64> = hist.iter().skip(skip).copied().collect();
    let visible_max = data.iter().copied().max().unwrap_or(0) as f64 / hist_per_unit;

    let plot = Rect { x: area.x + y_width, y: area.y, width: plot_width, height: rows };
    frame.render_widget(Sparkline::default().data(&data).style(Style::new().fg(color)), plot);

    let axis = Style::new().fg(theme::SURFACE1);
    let text = Style::new().fg(theme::OVERLAY0);
    let tick_rows: &[u16] = if rows >= 4 { &[0, rows / 2] } else { &[0] };
    let mut y_lines = Vec::with_capacity(rows as usize + 1);
    for row in 0..rows {
        // The value a row's upper edge stands for: the top row is the maximum.
        let line = if tick_rows.contains(&row) {
            let edge = visible_max * f64::from(rows - row) / f64::from(rows);
            vec![
                Span::styled(format!("{:>label_width$}", axis_value(edge)), text),
                Span::styled("┤", axis),
            ]
        } else {
            vec![Span::raw(" ".repeat(label_width)), Span::styled("│", axis)]
        };
        y_lines.push(Line::from(line));
    }
    let (baseline, labels) = x_axis(hist.len(), skip, plot_width as usize, sample_secs);
    y_lines.push(Line::from(vec![
        Span::styled(format!("{:>label_width$}", "0"), text),
        Span::styled("┼", axis),
        Span::styled(baseline, axis),
    ]));
    y_lines.push(Line::from(vec![
        Span::raw(" ".repeat(y_width as usize)),
        Span::styled(labels, text),
    ]));
    let column = Rect { height: rows + 2, ..area };
    frame.render_widget(Paragraph::new(y_lines), column);
}

/// A value-axis label with a sensible number of digits: `21.2`, `0.48`, `130`.
fn axis_value(v: f64) -> String {
    if v >= 100.0 {
        format!("{v:.0}")
    } else if v >= 10.0 {
        format!("{v:.1}")
    } else {
        format!("{v:.2}")
    }
}

/// `45s`, `2m05s`, `1h02m` — compact, since axis labels are short on room.
fn time_label(secs: u64) -> String {
    match secs {
        0..=119 => format!("{secs}s"),
        120..=3599 => format!("{}m{:02}s", secs / 60, secs % 60),
        _ => format!("{}h{:02}m", secs / 3600, secs % 3600 / 60),
    }
}

/// The time axis as two lines of exactly `width` characters: the baseline
/// (with a `┬` tick under each label) and the labels.
///
/// Sample `k` covers the interval ending at `(k + 1) * sample_secs`, so the
/// plot starts at `skip * sample_secs`. While the whole history fits, the
/// right label sits under the newest sample and gives the elapsed time; once
/// it scrolls, the right edge is `now` and the left label counts up. A middle
/// label is added when there is room.
pub(crate) fn x_axis(len: usize, skip: usize, width: usize, sample_secs: u64) -> (String, String) {
    let secs = sample_secs.max(1);
    let mut baseline = vec!['─'; width];
    let mut labels = vec![' '; width];
    if width == 0 {
        return (String::new(), String::new());
    }
    let mut put = |text: &str, start: usize, tick: usize| {
        for (i, c) in text.chars().enumerate() {
            if let Some(slot) = labels.get_mut(start + i) {
                *slot = c;
            }
        }
        baseline[tick] = '┬';
    };

    let left = time_label(skip as u64 * secs);
    let left_len = left.chars().count();
    put(&left, 0, 0);

    let visible = len - skip;
    // Column of the newest sample (the right edge once the plot scrolls).
    let newest = if skip > 0 { width - 1 } else { visible.saturating_sub(1).min(width - 1) };
    let right = if skip > 0 { "now".to_string() } else { time_label(len as u64 * secs) };
    let right_len = right.chars().count();
    let right_start = (newest + 1).saturating_sub(right_len);
    let right_fits = visible > 0 && right_start > left_len;
    if right_fits {
        put(&right, right_start, newest);
    }

    // A middle label, centred under its tick, if it clears both neighbours.
    let mid_col = newest / 2;
    let mid = time_label((skip + mid_col) as u64 * secs);
    let mid_len = mid.chars().count();
    let mid_start = mid_col.saturating_sub(mid_len / 2);
    let right_edge = if right_fits { right_start } else { width };
    if visible > 0 && mid_start > left_len && mid_start + mid_len < right_edge {
        put(&mid, mid_start, mid_col);
    }
    if skip == 0 {
        // The origin: the `┼` where the two axes meet already marks it.
        baseline[0] = '─';
    }
    (baseline.into_iter().collect(), labels.into_iter().collect())
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

    #[test]
    fn x_axis_runs_from_zero_to_the_newest_sample_while_history_fits() {
        let (baseline, labels) = x_axis(12, 0, 30, 1);
        assert_eq!((baseline.chars().count(), labels.chars().count()), (30, 30));
        assert!(labels.starts_with("0s"), "{labels:?}");
        // The right label ends under the newest sample (column 11), not at the box edge.
        assert_eq!(labels.trim_end().chars().count(), 12, "{labels:?}");
        assert!(labels.trim_end().ends_with("12s"), "{labels:?}");
        // …with a tick on the baseline under the right label; the origin is
        // marked by the axes' own `┼`, so there is no tick under `0s`.
        assert_eq!(baseline.chars().next(), Some('─'));
        assert_eq!(baseline.chars().nth(11), Some('┬'));
        // Samples are `interval` seconds apart.
        assert!(x_axis(12, 0, 30, 2).1.trim_end().ends_with("24s"));
    }

    #[test]
    fn x_axis_adds_a_middle_label_when_there_is_room() {
        let (baseline, labels) = x_axis(40, 0, 60, 1);
        // 40 samples: the middle tick is under column 19, i.e. 19 s in.
        assert!(labels.contains("19s"), "{labels:?}");
        assert_eq!(
            baseline.chars().filter(|c| *c == '┬').count(),
            2,
            "mid and right; the origin has none"
        );
    }

    #[test]
    fn x_axis_drops_labels_that_would_collide_or_have_no_data() {
        assert_eq!(x_axis(0, 0, 20, 1).1.trim_end(), "0s");
        assert_eq!(x_axis(3, 0, 20, 1).1.trim_end(), "0s", "3 columns cannot hold both labels");
        // No label may overwrite another.
        for len in 1..70 {
            let labels = x_axis(len, 0, 60, 1).1;
            let words: Vec<&str> = labels.split_whitespace().collect();
            assert!(words.iter().all(|w| w.ends_with('s')), "{len}: {labels:?}");
        }
    }

    #[test]
    fn x_axis_counts_up_and_ends_in_now_once_the_history_scrolls() {
        let (baseline, labels) = x_axis(100, 60, 40, 2);
        assert_eq!((baseline.chars().count(), labels.chars().count()), (40, 40));
        assert!(labels.starts_with("2m00s"), "{labels:?}"); // 60 samples × 2 s
        assert!(labels.ends_with("now"), "{labels:?}");
        assert_eq!(baseline.chars().last(), Some('┬'));
        assert_eq!(
            baseline.chars().next(),
            Some('┬'),
            "a scrolled plot starts at a real time, not the origin"
        );
    }

    #[test]
    fn x_axis_is_always_exactly_as_wide_as_the_plot() {
        for width in [0, 1, 5, 17, 80] {
            for (len, skip) in [(0, 0), (1, 0), (30, 0), (500, 470)] {
                let (baseline, labels) = x_axis(len, skip, width, 1);
                assert_eq!((baseline.chars().count(), labels.chars().count()), (width, width));
            }
        }
    }

    #[test]
    fn time_labels_are_compact() {
        assert_eq!(time_label(0), "0s");
        assert_eq!(time_label(119), "119s");
        assert_eq!(time_label(120), "2m00s");
        assert_eq!(time_label(305), "5m05s");
        assert_eq!(time_label(3720), "1h02m");
    }

    #[test]
    fn value_axis_labels_use_a_sensible_number_of_digits() {
        assert_eq!(axis_value(0.0), "0.00");
        assert_eq!(axis_value(0.48), "0.48");
        assert_eq!(axis_value(21.25), "21.2");
        assert_eq!(axis_value(130.4), "130");
    }

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
