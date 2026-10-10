//! Small, raw `setsockopt`/`getsockopt` wrappers for the per-socket options
//! tokio doesn't expose portably: IP TOS / IPv6 traffic class (QoS marking)
//! and the TCP congestion-control algorithm (Linux only).

use std::io;
use std::os::fd::RawFd;

/// Whether the OS lets us pick the TCP congestion-control algorithm per
/// socket. Only Linux has `TCP_CONGESTION`; macOS/BSD pick it system-wide.
pub const CONGESTION_SETTABLE: bool = cfg!(target_os = "linux");

fn tos_opt(ipv6: bool) -> (libc::c_int, libc::c_int) {
    if ipv6 { (libc::IPPROTO_IPV6, libc::IPV6_TCLASS) } else { (libc::IPPROTO_IP, libc::IP_TOS) }
}

/// Marks outgoing packets with the given TOS/DSCP byte (IPv6: traffic class).
pub fn set_tos(fd: RawFd, tos: u8, ipv6: bool) -> io::Result<()> {
    let (level, name) = tos_opt(ipv6);
    let value = tos as libc::c_int;
    // SAFETY: `value` outlives the call and the length matches its type.
    let rc = unsafe {
        libc::setsockopt(
            fd,
            level,
            name,
            (&value as *const libc::c_int).cast(),
            size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

#[cfg(test)]
pub fn get_tos(fd: RawFd, ipv6: bool) -> io::Result<u8> {
    let (level, name) = tos_opt(ipv6);
    let mut value: libc::c_int = 0;
    let mut len = size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: `value`/`len` are valid for writes and `len` matches `value`.
    let rc = unsafe {
        libc::getsockopt(fd, level, name, (&mut value as *mut libc::c_int).cast(), &mut len)
    };
    if rc == 0 { Ok(value as u8) } else { Err(io::Error::last_os_error()) }
}

/// Selects the congestion-control algorithm (e.g. "cubic", "bbr", "reno").
/// Fails if the kernel doesn't know/allow it (e.g. the `bbr` module isn't
/// loaded) — and always on non-Linux platforms.
#[cfg(target_os = "linux")]
pub fn set_congestion(fd: RawFd, name: &str) -> io::Result<()> {
    // SAFETY: the pointer/length pair describes `name`'s bytes exactly.
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_CONGESTION,
            name.as_ptr().cast(),
            name.len() as libc::socklen_t,
        )
    };
    if rc == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

#[cfg(not(target_os = "linux"))]
pub fn set_congestion(_fd: RawFd, _name: &str) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "only supported on Linux"))
}

/// Parses a TOS value as given on the CLI/config: decimal ("184") or hex
/// ("0xB8").
pub fn parse_tos(s: &str) -> Option<u8> {
    let s = s.trim();
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u8::from_str_radix(hex, 16).ok(),
        None => s.parse().ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    #[test]
    fn set_tos_is_really_applied_to_a_tcp_socket() {
        let sock = tokio::net::TcpSocket::new_v4().unwrap();
        set_tos(sock.as_raw_fd(), 0xB8, false).unwrap();
        assert_eq!(get_tos(sock.as_raw_fd(), false).unwrap(), 0xB8);
    }

    #[test]
    fn set_tos_is_really_applied_to_a_udp_socket() {
        let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        set_tos(sock.as_raw_fd(), 0x88, false).unwrap();
        assert_eq!(get_tos(sock.as_raw_fd(), false).unwrap(), 0x88);
    }

    #[test]
    fn parses_decimal_and_hex_tos() {
        assert_eq!(parse_tos("184"), Some(184));
        assert_eq!(parse_tos("0xB8"), Some(0xB8));
        assert_eq!(parse_tos("0x1ff"), None);
        assert_eq!(parse_tos("voice"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_accepts_reno_and_rejects_nonsense_congestion_names() {
        let sock = tokio::net::TcpSocket::new_v4().unwrap();
        set_congestion(sock.as_raw_fd(), "reno").unwrap();
        assert!(set_congestion(sock.as_raw_fd(), "no-such-algo").is_err());
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn congestion_is_reported_unsupported_off_linux() {
        let sock = tokio::net::TcpSocket::new_v4().unwrap();
        assert!(set_congestion(sock.as_raw_fd(), "reno").is_err());
        assert!(!std::hint::black_box(CONGESTION_SETTABLE));
    }
}
