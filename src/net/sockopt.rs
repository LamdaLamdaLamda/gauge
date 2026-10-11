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

fn buffer_size(fd: RawFd, option: libc::c_int) -> io::Result<usize> {
    let mut value: libc::c_int = 0;
    let mut len = size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: `value`/`len` are valid for writes and `len` matches `value`.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            option,
            (&mut value as *mut libc::c_int).cast(),
            &mut len,
        )
    };
    if rc == 0 { Ok(value.max(0) as usize) } else { Err(io::Error::last_os_error()) }
}

fn set_buffer_size(fd: RawFd, option: libc::c_int, bytes: u32) -> io::Result<()> {
    let value = bytes.min(i32::MAX as u32) as libc::c_int;
    // SAFETY: `value` outlives the call and the length matches its type.
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            option,
            (&value as *const libc::c_int).cast(),
            size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// The socket's current receive-buffer size, as the kernel reports it.
#[cfg(test)]
pub fn recv_buffer(fd: RawFd) -> io::Result<usize> {
    buffer_size(fd, libc::SO_RCVBUF)
}

/// Makes the socket's send and receive buffers at least `bytes`. Never
/// shrinks them: a UDP receiver that is handed a smaller buffer than the OS
/// default would only lose more packets. The kernel may still clamp the
/// request (on Linux to `net.core.rmem_max` / `wmem_max`).
pub fn grow_socket_buffers(fd: RawFd, bytes: u32) -> io::Result<()> {
    for option in [libc::SO_RCVBUF, libc::SO_SNDBUF] {
        if buffer_size(fd, option)? < bytes as usize {
            set_buffer_size(fd, option, bytes)?;
        }
    }
    Ok(())
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
    fn grow_socket_buffers_enlarges_but_never_shrinks() {
        let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let fd = sock.as_raw_fd();
        let before = recv_buffer(fd).unwrap();

        grow_socket_buffers(fd, 4096).unwrap(); // far below any OS default
        assert_eq!(
            recv_buffer(fd).unwrap(),
            before,
            "a smaller request must not shrink the buffer"
        );

        grow_socket_buffers(fd, 8 * 1024 * 1024).unwrap();
        assert!(recv_buffer(fd).unwrap() >= before, "a larger request must not shrink it either");
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
