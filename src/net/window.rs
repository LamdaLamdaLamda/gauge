/// Parses a TCP window/buffer-size string like the UI's own labels
/// ("64K", "256K", "512K", "1M") into bytes, applied as `SO_SNDBUF`/
/// `SO_RCVBUF` on the socket. `"auto"` (or anything unparseable) returns
/// `None`, meaning "don't touch it — let the OS pick", same as today's
/// default behavior.
///
/// Unlike `bandwidth::parse_bps` (bits/sec, SI/decimal), this is a byte
/// count, so it uses binary (KiB/MiB) multipliers — the natural unit for a
/// socket buffer size.
pub fn parse_window_bytes(s: &str) -> Option<u32> {
    let s = s.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("auto") {
        return None;
    }
    let (num_part, mult) = if let Some(p) = s.strip_suffix(['M', 'm']) {
        (p, 1024.0 * 1024.0)
    } else if let Some(p) = s.strip_suffix(['K', 'k']) {
        (p, 1024.0)
    } else {
        (s, 1.0)
    };
    let n: f64 = num_part.trim().parse().ok()?;
    if n <= 0.0 {
        return None;
    }
    Some((n * mult) as u32)
}

#[cfg(test)]
mod tests {
    use super::parse_window_bytes;

    #[test]
    fn parses_binary_suffixes() {
        assert_eq!(parse_window_bytes("256K"), Some(256 * 1024));
        assert_eq!(parse_window_bytes("64k"), Some(64 * 1024));
        assert_eq!(parse_window_bytes("1M"), Some(1024 * 1024));
        assert_eq!(parse_window_bytes("212992"), Some(212_992));
    }

    #[test]
    fn auto_and_garbage_mean_leave_it_alone() {
        assert_eq!(parse_window_bytes("auto"), None);
        assert_eq!(parse_window_bytes("Auto"), None);
        assert_eq!(parse_window_bytes(""), None);
        assert_eq!(parse_window_bytes("not a number"), None);
        assert_eq!(parse_window_bytes("0"), None);
        assert_eq!(parse_window_bytes("-5K"), None);
    }
}
