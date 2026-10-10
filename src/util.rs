#[derive(Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Err is reserved for future real-network error reporting
pub enum LogLevel {
    Ok,
    Info,
    Warn,
    Err,
    Data,
}

impl LogLevel {
    pub fn label(self) -> &'static str {
        match self {
            LogLevel::Ok => "OK",
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Err => "ERR",
            LogLevel::Data => "DATA",
        }
    }

    pub fn color(self) -> ratatui::style::Color {
        use crate::theme;
        match self {
            LogLevel::Ok => theme::GREEN,
            LogLevel::Info => theme::BLUE,
            LogLevel::Warn => theme::YELLOW,
            LogLevel::Err => theme::RED,
            LogLevel::Data => theme::TEAL,
        }
    }
}

#[derive(Clone)]
pub struct LogLine {
    pub ts: String,
    pub level: LogLevel,
    pub text: String,
}

impl LogLine {
    pub fn new(ts: impl Into<String>, level: LogLevel, text: impl Into<String>) -> Self {
        Self { ts: ts.into(), level, text: text.into() }
    }
}

pub fn now_hms() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let h = (secs / 3600) % 24;
    let m = (secs / 60) % 60;
    let s = secs % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

pub fn fmt_hms(total_secs: u64) -> String {
    let h = total_secs / 3600;
    let m = (total_secs / 60) % 60;
    let s = total_secs % 60;
    format!("{h:02}:{m:02}:{s:02}")
}
