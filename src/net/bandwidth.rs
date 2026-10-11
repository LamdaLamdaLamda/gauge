/// Parses a bandwidth target like the Config screen's "0 = unlimited" field
/// into bits/sec. Accepts a bare number or one with a `K`/`M`/`G` suffix
/// (e.g. "100M", "1G"). Returns `None` for "0", empty, or unparseable input
/// — all treated as "unlimited" (only meaningful for UDP pacing; TCP always
/// sends unthrottled and relies on congestion control).
pub fn parse_bps(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num_part, mult) = if let Some(p) = s.strip_suffix(['G', 'g']) {
        (p, 1_000_000_000.0)
    } else if let Some(p) = s.strip_suffix(['M', 'm']) {
        (p, 1_000_000.0)
    } else if let Some(p) = s.strip_suffix(['K', 'k']) {
        (p, 1_000.0)
    } else {
        (s, 1.0)
    };
    let n: f64 = num_part.trim().parse().ok()?;
    if n <= 0.0 {
        return None;
    }
    Some((n * mult) as u64)
}

/// `-b auto` asks for a rate search instead of a fixed rate.
pub fn is_auto_bandwidth(s: &str) -> bool {
    s.trim().eq_ignore_ascii_case("auto")
}

#[cfg(test)]
mod tests {
    use super::{is_auto_bandwidth, parse_bps};

    #[test]
    fn auto_is_recognised_and_is_not_a_rate() {
        assert!(is_auto_bandwidth("auto"));
        assert!(is_auto_bandwidth("  AUTO "));
        assert!(!is_auto_bandwidth("100M"));
        assert!(!is_auto_bandwidth("0"));
        assert_eq!(parse_bps("auto"), None, "auto must never be mistaken for a fixed rate");
    }

    #[test]
    fn parses_suffixes() {
        assert_eq!(parse_bps("0"), None);
        assert_eq!(parse_bps(""), None);
        assert_eq!(parse_bps("100M"), Some(100_000_000));
        assert_eq!(parse_bps("1G"), Some(1_000_000_000));
        assert_eq!(parse_bps("512k"), Some(512_000));
        assert_eq!(parse_bps("2500"), Some(2500));
    }
}
