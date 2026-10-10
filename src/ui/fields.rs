use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

use crate::theme;
use crate::ui::titled_block;

pub const NAME_W: usize = 20;

pub fn field(name: &str, value: String, focused: bool, editing: bool) -> Line<'static> {
    let marker = if focused { "▸ " } else { "  " };
    let name_style = if focused {
        Style::new().fg(theme::BLUE).bold()
    } else {
        Style::new().fg(theme::OVERLAY0)
    };
    let value_style = if editing {
        Style::new().fg(theme::YELLOW).bold()
    } else if focused {
        Style::new().fg(theme::TEXT).bold()
    } else {
        Style::new().fg(theme::SUBTEXT)
    };
    let name_col = if name.chars().count() >= NAME_W {
        format!("{name} ")
    } else {
        format!("{name:<width$}", width = NAME_W)
    };
    Line::from(vec![
        Span::styled(marker, name_style),
        Span::styled(name_col, name_style),
        Span::styled(value, value_style),
    ])
}

pub fn select_value(options: &[&str], idx: usize) -> String {
    format!("‹ {} ›", options[idx])
}

pub fn slider_value(value: u32, min: u32, max: u32, width: usize, suffix: &str) -> String {
    let ratio = (value - min) as f64 / (max - min) as f64;
    let filled = ((width as f64) * ratio).round() as usize;
    let filled = filled.min(width);
    format!("[{}{}] {}{}", "█".repeat(filled), "░".repeat(width - filled), value, suffix)
}

pub fn checkbox(checked: bool, label: &str) -> String {
    format!("[{}] {}", if checked { "x" } else { " " }, label)
}

pub fn edit_or(editing: bool, edit_buffer: &str, is_current: bool, value: &str) -> String {
    if is_current && editing { format!("{edit_buffer}█") } else { value.to_string() }
}

/// Renders a `$ gauge ... --flag value \` style command preview, coloring
/// `--flag` tokens blue and the rest of each line green.
pub fn render_cli_preview(frame: &mut Frame, area: Rect, lines: Vec<String>) {
    let lines: Vec<Line> = lines
        .into_iter()
        .map(|l| {
            if let Some(rest) = l.strip_prefix("  --") {
                let (flag, tail) = rest.split_once(' ').unwrap_or((rest, ""));
                Line::from(vec![
                    Span::styled("  --", Style::new().fg(theme::BLUE)),
                    Span::styled(flag.to_string(), Style::new().fg(theme::BLUE)),
                    Span::styled(format!(" {tail}"), Style::new().fg(theme::GREEN)),
                ])
            } else {
                Line::styled(l, Style::new().fg(theme::GREEN))
            }
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines).block(titled_block("CLI Preview", None)).wrap(Wrap { trim: false }),
        area,
    );
}

pub fn render_action_button(frame: &mut Frame, area: Rect, label: &str) {
    frame.render_widget(
        Paragraph::new(Line::from(label).centered())
            .style(Style::new().fg(theme::CRUST).bg(theme::GREEN).bold()),
        area,
    );
}
