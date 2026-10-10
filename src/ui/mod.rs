mod config;
pub(crate) mod fields;
mod history;
mod live;
mod server;
#[cfg(test)]
mod tests;

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders},
};

use crate::app::{App, Tab};
use crate::theme;

pub fn draw(frame: &mut Frame, app: &App) {
    let outer = Block::new()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(theme::SURFACE0))
        .title(
            Line::from(" gauge — rust network performance tool ")
                .centered()
                .style(Style::new().fg(theme::OVERLAY0)),
        )
        .style(Style::new().bg(theme::BASE));
    let inner = outer.inner(frame.area());
    frame.render_widget(outer, frame.area());

    let [topbar_area, content_area, status_area, keybinds_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(6),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    draw_topbar(frame, topbar_area, app);

    match app.tab {
        Tab::Live => live::draw(frame, content_area, &app.live),
        Tab::Config => config::draw(frame, content_area, &app.config),
        Tab::History => history::draw(frame, content_area, &app.history),
        Tab::Server => server::draw(frame, content_area, &app.server),
    }

    draw_statusbar(frame, status_area, app);
    draw_keybinds(frame, keybinds_area, app);
}

fn draw_topbar(frame: &mut Frame, area: Rect, app: &App) {
    let mut spans = vec![
        Span::styled(" ≋ gauge ", Style::new().fg(theme::MAUVE).bold()),
        Span::styled("│", Style::new().fg(theme::SURFACE0)),
    ];
    for tab in Tab::ALL {
        let active = tab == app.tab;
        let style = if active {
            Style::new().fg(theme::BLUE).bg(theme::BASE).bold()
        } else {
            Style::new().fg(theme::OVERLAY0)
        };
        spans.push(Span::styled(format!(" {} ", tab.title()), style));
        spans.push(Span::styled("│", Style::new().fg(theme::SURFACE0)));
    }

    let right = match app.tab {
        Tab::Live => {
            let (label, color) = if app.live.connecting {
                ("connecting", theme::YELLOW)
            } else if app.live.running {
                ("running", theme::GREEN)
            } else if app.live.error.is_some() {
                ("error", theme::RED)
            } else if app.live.target.is_empty() {
                ("idle", theme::OVERLAY0)
            } else {
                ("finished", theme::OVERLAY0)
            };
            let mut spans = vec![
                Span::styled(" ● ", Style::new().fg(color)),
                Span::styled(label, Style::new().fg(color)),
            ];
            if !app.live.target.is_empty() {
                spans.push(Span::styled(
                    format!("  → {} ", app.live.target),
                    Style::new().fg(theme::SUBTEXT),
                ));
            }
            spans
        }
        Tab::Config => vec![Span::styled(" idle ", Style::new().fg(theme::OVERLAY0))],
        Tab::History => vec![Span::styled(
            format!(" {} runs saved ", app.history.runs.len()),
            Style::new().fg(theme::OVERLAY0),
        )],
        Tab::Server => {
            if app.server.listening {
                vec![
                    Span::styled(" ● ", Style::new().fg(theme::GREEN)),
                    Span::styled("listening", Style::new().fg(theme::GREEN)),
                    Span::styled(
                        format!("  {} ", app.server.bind),
                        Style::new().fg(theme::SUBTEXT),
                    ),
                ]
            } else {
                vec![Span::styled(" configuring ", Style::new().fg(theme::OVERLAY0))]
            }
        }
    };

    let line_left = Line::from(spans).left_aligned();
    let line_right = Line::from(right).right_aligned();
    frame.render_widget(
        ratatui::widgets::Paragraph::new(line_left).style(Style::new().bg(theme::MANTLE)),
        area,
    );
    frame.render_widget(
        ratatui::widgets::Paragraph::new(line_right).style(Style::new().bg(theme::MANTLE)),
        area,
    );
}

fn draw_statusbar(frame: &mut Frame, area: Rect, app: &App) {
    let segs: Vec<(String, String)> = match app.tab {
        Tab::Live => vec![
            ("target".into(), app.live.target.clone()),
            ("proto".into(), app.live.proto.clone()),
            ("streams".into(), app.live.streams.to_string()),
            ("window".into(), app.live.window.clone()),
            ("mtu".into(), app.live.mtu.to_string()),
        ],
        Tab::Config => vec![
            ("host".into(), app.config.host.clone()),
            ("proto".into(), crate::app::PROTOCOLS[app.config.protocol].into()),
            ("streams".into(), app.config.parallel.to_string()),
        ],
        Tab::History => vec![(
            "selected".into(),
            format!("{}/{}", app.history.selected + 1, app.history.runs.len().max(1)),
        )],
        Tab::Server if app.server.listening => vec![
            ("bind".into(), app.server.bind.clone()),
            ("clients".into(), format!("{} active", app.server.active_clients)),
            ("sessions".into(), format!("{} total", app.server.total_sessions)),
        ],
        Tab::Server => vec![
            ("bind".into(), format!("{}:{}", app.server.config.bind, app.server.config.port)),
            ("proto".into(), crate::app::PROTOCOLS[app.server.config.protocol].into()),
        ],
    };

    let mut spans = Vec::new();
    for (k, v) in &segs {
        spans.push(Span::styled(format!(" {k} "), Style::new().fg(theme::SURFACE1)));
        spans.push(Span::styled(format!("{v} "), Style::new().fg(theme::SUBTEXT)));
        spans.push(Span::styled("│", Style::new().fg(theme::SURFACE0)));
    }
    let left_width: u16 = spans.iter().map(|s| s.content.chars().count() as u16).sum();
    // Leaves the left segments untouched — the flag gets whatever's left,
    // never more, so long messages truncate instead of overlapping them.
    let flag_budget = area.width.saturating_sub(left_width + 4) as usize;

    let flag = match app.tab {
        Tab::Live => {
            let elapsed = app.live.started_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);
            if app.live.connecting {
                Span::styled(" ● connecting… ", Style::new().fg(theme::YELLOW))
            } else if app.live.running {
                Span::styled(
                    format!(" ● test running — {elapsed}s "),
                    Style::new().fg(theme::GREEN),
                )
            } else if let Some(err) = &app.live.error {
                Span::styled(
                    format!(" ✕ {} ", truncate(err, flag_budget.saturating_sub(4))),
                    Style::new().fg(theme::RED),
                )
            } else {
                Span::styled(" ○ test stopped ", Style::new().fg(theme::OVERLAY0))
            }
        }
        Tab::Config => {
            Span::styled(" tab/↑↓ navigate · enter edit ", Style::new().fg(theme::OVERLAY0))
        }
        Tab::History => Span::styled(" ↑↓ select run ", Style::new().fg(theme::OVERLAY0)),
        Tab::Server => {
            if app.server.listening {
                Span::styled(" ● server running ", Style::new().fg(theme::GREEN))
            } else {
                Span::styled(
                    " ○ not started — press f5 to start ",
                    Style::new().fg(theme::OVERLAY0),
                )
            }
        }
    };

    let line = Line::from(spans).left_aligned();
    let flag_line = Line::from(vec![flag]).right_aligned();
    frame.render_widget(
        ratatui::widgets::Paragraph::new(line).style(Style::new().bg(theme::CRUST)),
        area,
    );
    frame.render_widget(
        ratatui::widgets::Paragraph::new(flag_line).style(Style::new().bg(theme::CRUST)),
        area,
    );
}

fn draw_keybinds(frame: &mut Frame, area: Rect, app: &App) {
    let binds: Vec<(&str, &str)> = match app.tab {
        Tab::Live => vec![
            ("s", "Stop"),
            ("r", "Restart"),
            ("e", "Export"),
            ("1-4", "Switch view"),
            ("q", "Quit"),
        ],
        Tab::Config => vec![
            ("ctrl+s", "Save"),
            ("f5", "Start test"),
            ("tab/↑↓", "Next field"),
            ("←→/space", "Change value"),
            ("esc", "Cancel edit"),
        ],
        Tab::History => vec![
            ("↑↓", "Select"),
            ("e", "Export JSON"),
            ("d", "Delete"),
            ("1-4", "Switch view"),
            ("q", "Quit"),
        ],
        Tab::Server if app.server.listening => vec![
            ("s", "Stop server"),
            ("k", "Kill client"),
            ("c", "Clear log"),
            ("1-4", "Switch view"),
            ("q", "Quit"),
        ],
        Tab::Server => vec![
            ("ctrl+s", "Save"),
            ("f5", "Start server"),
            ("tab/↑↓", "Next field"),
            ("←→/space", "Change value"),
            ("esc", "Cancel edit"),
        ],
    };

    let mut spans = Vec::new();
    for (k, label) in binds {
        spans.push(Span::styled(
            format!(" {k} "),
            Style::new().fg(theme::SUBTEXT).bg(theme::SURFACE0).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(format!(" {label}  "), Style::new().fg(theme::OVERLAY0)));
    }

    let toast = match app.tab {
        Tab::Live => app.live.toast.as_deref(),
        Tab::Config => app.config.toast.as_deref(),
        Tab::Server => app.server.toast.as_deref(),
        Tab::History => app.history.toast.as_deref(),
    };
    if let Some(t) = toast {
        let left_width: u16 = spans.iter().map(|s| s.content.chars().count() as u16).sum();
        let budget = area.width.saturating_sub(left_width + 4) as usize;
        let toast_line = Line::from(Span::styled(
            format!(" {} ", truncate(t, budget)),
            Style::new().fg(theme::YELLOW),
        ))
        .right_aligned();
        frame.render_widget(
            ratatui::widgets::Paragraph::new(toast_line).style(Style::new().bg(theme::CRUST)),
            area,
        );
    }

    frame.render_widget(
        ratatui::widgets::Paragraph::new(Line::from(spans))
            .style(Style::new().bg(theme::CRUST))
            .alignment(Alignment::Left),
        area,
    );
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{truncated}…")
    }
}

pub(crate) fn titled_block(title: &str, accent: Option<&str>) -> Block<'static> {
    let mut block = Block::bordered().border_style(Style::new().fg(theme::SURFACE0)).title_top(
        Line::from(Span::styled(format!(" {title} "), Style::new().fg(theme::OVERLAY1).bold())),
    );
    if let Some(a) = accent {
        block = block.title_top(
            Line::from(Span::styled(format!(" {a} "), Style::new().fg(theme::OVERLAY0)))
                .right_aligned(),
        );
    }
    block
}
