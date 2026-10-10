use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::{Axis, Chart, Dataset, GraphType, List, ListItem, Paragraph},
};

use crate::app::HistoryState;
use crate::theme;
use crate::ui::titled_block;

fn tag_color(idx: usize) -> Color {
    match idx {
        0 => theme::BLUE,
        1 => theme::TEAL,
        2 => theme::MAUVE,
        _ => theme::OVERLAY1,
    }
}

pub fn draw(frame: &mut Frame, area: Rect, hist: &HistoryState) {
    let [list_area, detail_area] =
        Layout::horizontal([Constraint::Length(36), Constraint::Min(20)]).areas(area);

    draw_list(frame, list_area, hist);
    draw_detail(frame, detail_area, hist);
}

fn draw_list(frame: &mut Frame, area: Rect, hist: &HistoryState) {
    let items: Vec<ListItem> = hist
        .runs
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let selected = i == hist.selected;
            let bg = if selected { theme::SURFACE0 } else { theme::BASE };
            let text = Text::from(vec![
                Line::from(vec![
                    Span::styled(r.target.clone(), Style::new().fg(theme::TEXT).bold().bg(bg)),
                    Span::raw(" "),
                    Span::styled(
                        format!("[{}]", r.tag),
                        Style::new().fg(tag_color(r.tag_color_idx)).bg(bg),
                    ),
                ]),
                Line::from(vec![
                    Span::styled(r.summary.clone(), Style::new().fg(theme::GREEN).bg(bg)),
                    Span::raw(" "),
                    Span::styled(r.time.clone(), Style::new().fg(theme::OVERLAY0).bg(bg)),
                ]),
            ]);
            ListItem::new(text)
        })
        .collect();

    let block = titled_block("RECENT RUNS", None);
    if items.is_empty() {
        let inner = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "no runs yet",
                Style::new().fg(theme::OVERLAY0),
            ))),
            inner,
        );
        return;
    }
    let list = List::new(items).block(block);
    frame.render_widget(list, area);
}

fn draw_detail(frame: &mut Frame, area: Rect, hist: &HistoryState) {
    let Some(run) = hist.runs.get(hist.selected) else {
        let msg = "completed tests from the Config screen show up here — press F5 there to run one";
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(msg, Style::new().fg(theme::OVERLAY0)))),
            area,
        );
        return;
    };

    let [header_area, tiles_area, chart_area] =
        Layout::vertical([Constraint::Length(3), Constraint::Length(4), Constraint::Min(8)])
            .areas(area);

    let header = vec![
        Line::from(vec![
            Span::styled(format!("{} — ", run.target), Style::new().fg(theme::TEXT).bold()),
            Span::styled(run.tag.clone(), Style::new().fg(tag_color(run.tag_color_idx)).bold()),
        ]),
        Line::from(Span::styled(
            format!(
                "{} · {}s · {} stream(s) · window {} · MTU {} · {} retransmit(s)",
                run.time, run.duration_secs, run.streams, run.window, run.mtu, run.retransmits
            ),
            Style::new().fg(theme::OVERLAY0),
        )),
        // Runs recorded before settings were saved have none to show.
        Line::from(Span::styled(
            run.settings.as_ref().map(|s| s.describe()).unwrap_or_default(),
            Style::new().fg(theme::OVERLAY0),
        )),
    ];
    frame.render_widget(Paragraph::new(header), header_area);

    let tiles = [
        ("Peak TX", format!("{:.2}", run.peak_gbps), "Gbit/s", theme::BLUE),
        ("Avg TX", format!("{:.2}", run.avg_gbps), "Gbit/s", theme::GREEN),
        ("Connect RTT", format!("{:.2}", run.rtt_ms), "ms", theme::TEAL),
        (
            "Total",
            format!("{:.2}", run.avg_gbps * run.duration_secs as f64 / 8.0),
            "GB",
            theme::YELLOW,
        ),
    ];
    let tile_areas = Layout::horizontal([Constraint::Percentage(25); 4]).split(tiles_area);
    for (area, (label, val, unit, color)) in tile_areas.iter().zip(tiles.iter()) {
        let lines = vec![
            Line::from(Span::styled(*label, Style::new().fg(theme::OVERLAY0))),
            Line::from(vec![
                Span::styled(format!("{val} "), Style::new().fg(*color).bold()),
                Span::styled(*unit, Style::new().fg(theme::SUBTEXT)),
            ]),
        ];
        frame.render_widget(
            Paragraph::new(lines).block(
                ratatui::widgets::Block::bordered().border_style(Style::new().fg(theme::SURFACE0)),
            ),
            *area,
        );
    }

    draw_chart(frame, chart_area, run);
}

fn draw_chart(frame: &mut Frame, area: Rect, run: &crate::store::HistoryRun) {
    if run.chart.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "no throughput samples recorded for this run",
                Style::new().fg(theme::OVERLAY0),
            )))
            .block(titled_block("Throughput over time", None)),
            area,
        );
        return;
    }

    let peak_rx = run.peak_rx_gbps.unwrap_or(0.0);
    let max_y = (run.peak_gbps.max(peak_rx) * 1.2).max(0.1);
    let max_x = run.duration_secs.max(1) as f64;
    // The chart's Braille line only connects points that are adjacent in the
    // dot grid, so a handful of samples (one per second) leave visible gaps
    // between them. Resampling along the straight lines between samples
    // fills them in without changing the shape.
    let steps = (area.width as usize).saturating_mul(4).max(2);
    let tx_dense = densify(&run.chart, steps);
    let rx_dense = densify(&run.chart_rx, steps);
    let mut datasets = vec![
        Dataset::default()
            .name("TX")
            .marker(ratatui::symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::new().fg(theme::BLUE))
            .data(&tx_dense),
    ];
    if !run.chart_rx.is_empty() {
        datasets.push(
            Dataset::default()
                .name("RX")
                .marker(ratatui::symbols::Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::new().fg(theme::GREEN))
                .data(&rx_dense),
        );
    }
    let subtitle = if run.chart_rx.is_empty() { "aggregate TX" } else { "aggregate TX/RX" };

    let chart = Chart::new(datasets)
        .block(titled_block("Throughput over time", Some(subtitle)))
        .legend_position(if run.chart_rx.is_empty() {
            None
        } else {
            Some(ratatui::widgets::LegendPosition::TopRight)
        })
        .x_axis(
            Axis::default().style(Style::new().fg(theme::SURFACE1)).bounds([0.0, max_x]).labels(
                vec!["0s".to_string(), format!("{:.0}s", max_x / 2.0), format!("{max_x:.0}s")],
            ),
        )
        .y_axis(
            Axis::default()
                .style(Style::new().fg(theme::SURFACE1))
                .bounds([0.0, max_y])
                .labels(vec!["0".into(), format!("{:.1}G", max_y / 2.0), format!("{max_y:.1}G")]),
        );
    frame.render_widget(chart, area);
}

/// Linearly interpolates `points` (sorted by x) so consecutive output points
/// are at most `(last_x - first_x) / steps` apart. The original samples are
/// kept exactly; fewer than two points are returned unchanged.
fn densify(points: &[(f64, f64)], steps: usize) -> Vec<(f64, f64)> {
    let (Some(first), Some(last)) = (points.first(), points.last()) else {
        return Vec::new();
    };
    let max_gap = (last.0 - first.0) / steps.max(1) as f64;
    if points.len() < 2 || max_gap <= 0.0 {
        return points.to_vec();
    }
    let mut out = Vec::with_capacity(steps + points.len());
    for pair in points.windows(2) {
        let ((x0, y0), (x1, y1)) = (pair[0], pair[1]);
        out.push((x0, y0));
        let n = ((x1 - x0) / max_gap).ceil() as usize;
        for i in 1..n {
            let t = i as f64 / n as f64;
            out.push((x0 + (x1 - x0) * t, y0 + (y1 - y0) * t));
        }
    }
    out.push(*last);
    out
}

#[cfg(test)]
mod densify_tests {
    use super::densify;

    #[test]
    fn keeps_original_samples_and_fills_gaps_linearly() {
        let pts = [(1.0, 0.0), (2.0, 10.0), (5.0, 4.0)];
        let d = densify(&pts, 8); // max gap = 4/8 = 0.5
        for p in pts {
            assert!(d.contains(&p), "{p:?} missing");
        }
        assert!(d.windows(2).all(|w| w[1].0 - w[0].0 <= 0.5 + 1e-9));
        assert!(d.windows(2).all(|w| w[1].0 > w[0].0));
        let mid = d.iter().find(|p| (p.0 - 1.5).abs() < 1e-9).unwrap();
        assert!((mid.1 - 5.0).abs() < 1e-9);
    }

    #[test]
    fn degenerate_inputs_pass_through() {
        assert!(densify(&[], 10).is_empty());
        assert_eq!(densify(&[(1.0, 2.0)], 10), vec![(1.0, 2.0)]);
    }
}
