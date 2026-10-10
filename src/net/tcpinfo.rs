//! Best-effort access to the kernel's live TCP connection state via
//! `getsockopt(TCP_INFO / TCP_CONNECTION_INFO)`. Read-only introspection —
//! used to show real, live retransmit count, congestion window, and an
//! approximate loss rate in the UI/CLI instead of the static placeholders
//! that used to sit there.

use std::os::fd::RawFd;

/// A snapshot of kernel-tracked TCP connection state, normalized across
/// platforms to the handful of fields gauge actually shows.
#[derive(Debug, Clone, Copy, Default)]
pub struct TcpKernelStats {
    /// Packets/segments sent so far, when the transport counts them exactly
    /// (QUIC does; TCP doesn't reliably — see `mss`). Lets `loss_pct` be exact.
    pub segments_sent: Option<u64>,
    /// Cumulative retransmitted segments since the connection was opened.
    pub retransmits: u64,
    /// Negotiated maximum segment size, in bytes. Used to turn a byte count
    /// into an estimated segment count for `loss_pct` — not `tcpi_segs_out`
    /// directly, because macOS's send/receive packet counters
    /// (`tcpi_txpackets`/`tcpi_rxpackets`) stay `0` for ordinary
    /// (non-entitled) processes in practice, even mid-transfer.
    pub mss: u32,
    /// Current congestion window, in bytes.
    pub cwnd_bytes: u32,
}

impl TcpKernelStats {
    /// Retransmitted segments as a percentage of segments sent, estimated
    /// as `bytes_sent / mss` — TCP exposes no direct "packets lost"
    /// counter (and macOS doesn't reliably expose a segment counter at
    /// all), so this retransmission rate is the loss-rate proxy shown in
    /// the UI. Only meaningful for the sending side of the connection;
    /// callers on the receiving side shouldn't call this. `None` if `mss`
    /// is unknown or no segments have gone out yet.
    pub fn loss_pct(&self, bytes_sent: u64) -> Option<f64> {
        if let Some(sent) = self.segments_sent {
            return (sent > 0).then(|| self.retransmits as f64 / sent as f64 * 100.0);
        }
        if self.mss == 0 {
            return None;
        }
        let segments_out = bytes_sent / self.mss as u64;
        (segments_out > 0).then(|| self.retransmits as f64 / segments_out as f64 * 100.0)
    }
}

/// Reads live TCP state for the connection behind `fd`. `None` if the
/// platform has no known accessor, or the syscall fails (e.g. the socket is
/// already closed).
#[cfg(target_os = "linux")]
pub fn query(fd: RawFd) -> Option<TcpKernelStats> {
    use std::mem;
    // SAFETY: `info` is a zeroed, correctly sized out-parameter and `len`
    // is its size; getsockopt writes at most `len` bytes into it.
    unsafe {
        let mut info: libc::tcp_info = mem::zeroed();
        let mut len = mem::size_of::<libc::tcp_info>() as libc::socklen_t;
        let rc = libc::getsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_INFO,
            &mut info as *mut _ as *mut libc::c_void,
            &mut len,
        );
        if rc != 0 {
            return None;
        }
        // tcpi_snd_cwnd is in units of MSS on Linux, not bytes.
        let cwnd_bytes = info.tcpi_snd_cwnd.saturating_mul(info.tcpi_snd_mss);
        Some(TcpKernelStats {
            retransmits: info.tcpi_total_retrans as u64,
            mss: info.tcpi_snd_mss,
            cwnd_bytes,
            segments_sent: None,
        })
    }
}

/// macOS has no direct "retransmitted segments sent" counter — `TCP_CONNECTION_INFO`
/// only exposes retransmitted *bytes* sent (`tcpi_txretransmitbytes`) and
/// retransmitted *packets* received (`tcpi_rxretransmitpackets`). We
/// approximate a segment count for the send side by dividing bytes by the
/// negotiated segment size and add the receive-side packet count on top —
/// close enough for a live indicator, not a byte-exact match to Linux's
/// counter. `tcpi_snd_cwnd` is already in bytes here, unlike Linux.
#[cfg(target_os = "macos")]
pub fn query(fd: RawFd) -> Option<TcpKernelStats> {
    use std::mem;
    // SAFETY: `info` is a zeroed, correctly sized out-parameter and `len`
    // is its size; getsockopt writes at most `len` bytes into it.
    unsafe {
        let mut info: libc::tcp_connection_info = mem::zeroed();
        let mut len = mem::size_of::<libc::tcp_connection_info>() as libc::socklen_t;
        let rc = libc::getsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_CONNECTION_INFO,
            &mut info as *mut _ as *mut libc::c_void,
            &mut len,
        );
        if rc != 0 {
            return None;
        }
        let maxseg = (info.tcpi_maxseg as u64).max(1);
        let retransmits = info.tcpi_txretransmitbytes / maxseg + info.tcpi_rxretransmitpackets;
        Some(TcpKernelStats {
            retransmits,
            mss: info.tcpi_maxseg,
            cwnd_bytes: info.tcpi_snd_cwnd,
            segments_sent: None,
        })
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn query(_fd: RawFd) -> Option<TcpKernelStats> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use std::os::fd::AsRawFd;

    #[test]
    fn query_on_a_fresh_loopback_connection_is_sane() {
        // Not much to assert about the exact *values* on loopback (usually
        // near zero, but never negative or absurd) — this is mainly a smoke
        // test that the getsockopt call works on a real, live fd without
        // panicking.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        let (server_stream, _) = listener.accept().unwrap();

        for fd in [client.as_raw_fd(), server_stream.as_raw_fd()] {
            if let Some(stats) = query(fd) {
                assert!(
                    stats.retransmits < 1_000_000,
                    "sane upper bound for a fresh connection, got {}",
                    stats.retransmits
                );
            }
        }
    }

    #[test]
    fn loss_pct_is_none_when_nothing_has_been_sent_yet() {
        let stats =
            TcpKernelStats { retransmits: 0, mss: 1460, cwnd_bytes: 0, segments_sent: None };
        assert_eq!(stats.loss_pct(0), None);
    }

    #[test]
    fn loss_pct_is_none_when_mss_is_unknown() {
        let stats = TcpKernelStats { retransmits: 0, mss: 0, cwnd_bytes: 0, segments_sent: None };
        assert_eq!(stats.loss_pct(1_000_000), None);
    }

    #[test]
    fn loss_pct_is_the_retransmit_rate_over_estimated_segments() {
        let stats =
            TcpKernelStats { retransmits: 5, mss: 1000, cwnd_bytes: 0, segments_sent: None };
        // 100_000 bytes / 1000 mss = 100 estimated segments; 5 retransmits -> 5%.
        assert_eq!(stats.loss_pct(100_000), Some(5.0));
    }
}
