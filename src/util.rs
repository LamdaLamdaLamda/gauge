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

/// A bit rate for people: `2.41 Gbit/s`, `480 Mbit/s`, `12 kbit/s`.
pub fn fmt_rate(bps: u64) -> String {
    let b = bps as f64;
    if b >= 1e9 {
        format!("{:.2} Gbit/s", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.0} Mbit/s", b / 1e6)
    } else {
        format!("{:.0} kbit/s", b / 1e3)
    }
}

#[cfg(test)]
mod rate_tests {
    use super::fmt_rate;

    #[test]
    fn formats_rates_in_the_natural_unit() {
        assert_eq!(fmt_rate(2_410_000_000), "2.41 Gbit/s");
        assert_eq!(fmt_rate(480_000_000), "480 Mbit/s");
        assert_eq!(fmt_rate(12_000), "12 kbit/s");
    }
}
