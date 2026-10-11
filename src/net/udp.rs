//! Shared UDP building blocks. Like the TCP loops in `duplex.rs`, both the
//! client and the server need *both* directions (Tx/Rx/Bidir), so the
//! sender loop, the receive-side loss/jitter bookkeeping and the client's
//! receive loop live here once.
//!
//! Wire format of a data datagram: `seq: u64 BE` + `send_timestamp_ns: u64
//! BE` + zero padding up to the packet size. `seq == FIN_SEQ` marks the end
//! of a stream (no timestamp meaning). Handshake datagrams are JSON and are
//! told apart from data simply by not starting with a sequence number's
//! zero high byte (see `looks_like_json`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use std::net::SocketAddr;

use tokio::net::UdpSocket;

use super::duplex::{IntervalReport, MIN_PARTIAL_INTERVAL};
use super::protocol::{UdpSummary, decode_datagram};

pub const UDP_HEADER: usize = 16;
pub const IP_UDP_HEADER_OVERHEAD: u32 = 28; // 20-byte IPv4 + 8-byte UDP header
pub const FIN_SEQ: u64 = u64::MAX;
/// How long a receiver waits without a single packet before giving up.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(3);

/// UDP payload size for the configured MTU, header included — clamped to a
/// sane range so a bogus/tiny MTU can't produce a zero-or-negative size.
pub fn packet_size_for_mtu(mtu: u32) -> usize {
    mtu.saturating_sub(IP_UDP_HEADER_OVERHEAD).clamp(64, 65_000) as usize
}

/// True for datagrams that are (stray) JSON control messages, e.g. a
/// duplicate handshake ack — never a data packet, whose first byte is the
/// high byte of a small sequence number (0) or of `FIN_SEQ` (0xFF).
pub fn looks_like_json(data: &[u8]) -> bool {
    data.first() == Some(&b'{')
}

pub fn is_fin(data: &[u8]) -> bool {
    data.len() >= UDP_HEADER && u64::from_be_bytes(data[0..8].try_into().unwrap()) == FIN_SEQ
}

/// Receive-side bookkeeping for one UDP stream: byte counts, sequence-gap
/// loss and RFC 3550 interarrival jitter.
#[derive(Default)]
pub struct RecvStats {
    next_expected_seq: u64,
    received: u64,
    lost: u64,
    pub bytes_total: u64,
    pub bytes_interval: u64,
    prev_transit_ns: Option<i64>,
    jitter_ns: f64,
}

impl RecvStats {
    /// Accounts one data datagram (caller has checked it is at least
    /// `UDP_HEADER` long and not a FIN).
    pub fn on_packet(&mut self, data: &[u8]) {
        let seq = u64::from_be_bytes(data[0..8].try_into().unwrap());
        self.received += 1;
        self.bytes_total += data.len() as u64;
        self.bytes_interval += data.len() as u64;
        if seq >= self.next_expected_seq {
            self.lost += seq - self.next_expected_seq;
            self.next_expected_seq = seq + 1;
        }

        let send_ts_ns = i64::from_be_bytes(data[8..16].try_into().unwrap_or_default());
        let arrival_ns =
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos() as i64;
        let transit = arrival_ns - send_ts_ns;
        if let Some(prev) = self.prev_transit_ns {
            let d = (transit - prev).unsigned_abs() as f64;
            self.jitter_ns += (d - self.jitter_ns) / 16.0;
        }
        self.prev_transit_ns = Some(transit);
    }

    /// The final tally, as sent back to the sender of this stream.
    pub fn summary(&self) -> UdpSummary {
        UdpSummary {
            received: self.received,
            lost: self.lost,
            bytes: self.bytes_total,
            jitter_ms: self.jitter_ms(),
        }
    }

    pub fn loss_pct(&self) -> f64 {
        if self.received + self.lost > 0 {
            self.lost as f64 / (self.received + self.lost) as f64 * 100.0
        } else {
            0.0
        }
    }

    pub fn jitter_ms(&self) -> f64 {
        self.jitter_ns / 1_000_000.0
    }
}

/// A UDP socket bound to `local` with `SO_REUSEPORT`, connected to `peer` if
/// given. The server uses this to give every stream its **own** socket on the
/// listening port: once connected, the kernel delivers that peer's datagrams
/// to its dedicated socket (an exact 4-tuple match beats the wildcard
/// listener), so each stream is received by its own task on its own core.
pub fn reuseport_socket(local: SocketAddr, peer: Option<SocketAddr>) -> std::io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::for_address(local), Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_port(true)?;
    socket.bind(&local.into())?;
    if let Some(peer) = peer {
        socket.connect(&peer.into())?;
    }
    socket.set_nonblocking(true)?;
    UdpSocket::from_std(socket.into())
}

pub struct TxConfig {
    pub packet_size: usize,
    pub duration: Duration,
    /// Per-stream target rate; `None` sends as fast as the socket allows.
    pub bandwidth_bps: Option<u64>,
    pub report_every: Duration,
}

/// Sends numbered, timestamped datagrams on `sock` — to `target` if given
/// (the server's unconnected socket), else to its connected peer — for up to
/// `cfg.duration` (or until `stop`), paced to `cfg.bandwidth_bps` if set,
/// calling `on_interval` every `cfg.report_every`, then a best-effort FIN
/// burst. Returns `(total_bytes, elapsed_secs_of_last_report)`.
/// Waits until the next packet is due, then schedules the one after it.
///
/// Tokio's timers have ~1 ms resolution, so sleeping per packet makes a fast
/// sender wake up late and then fire everything that fell due in a burst —
/// at several Gbit/s that is hundreds of KB back to back, which a receiver
/// (or the kernel queue) drops. So: sleep only for the long part of a wait and
/// yield in a loop for the last stretch (only when packets are that close
/// together), and never owe more than 50 ms of
/// catch-up after a long stall (shorter stalls are made up for, so the
/// average rate stays exact even on a busy machine).
async fn pace(next_send: &mut Instant, delay: Duration) {
    const MAX_BACKLOG: Duration = Duration::from_millis(50);
    // Below this spacing a sleep (which rounds up to ~1 ms) would make a
    // burst of several packets; only then is spinning worth its CPU cost.
    const SPIN_BELOW: Duration = Duration::from_micros(500);
    loop {
        let now = Instant::now();
        if now >= *next_send {
            if now - *next_send > MAX_BACKLOG {
                *next_send = now - MAX_BACKLOG;
            }
            break;
        }
        let wait = *next_send - now;
        if wait >= Duration::from_micros(1500) {
            tokio::time::sleep(wait - Duration::from_millis(1)).await;
        } else if delay >= SPIN_BELOW {
            tokio::time::sleep(wait).await;
        } else {
            tokio::task::yield_now().await;
        }
    }
    *next_send += delay;
}

async fn send_datagram(
    sock: &UdpSocket,
    target: Option<SocketAddr>,
    data: &[u8],
) -> std::io::Result<usize> {
    match target {
        Some(addr) => sock.send_to(data, addr).await,
        None => sock.send(data).await,
    }
}

pub async fn tx_loop(
    cfg: TxConfig,
    start: Instant,
    stop: &AtomicBool,
    sock: &UdpSocket,
    target: Option<SocketAddr>,
    mut on_interval: impl FnMut(IntervalReport),
) -> (u64, u64) {
    let mut packet = vec![0u8; cfg.packet_size.max(UDP_HEADER)];
    let packet_len = packet.len() as u64;
    let mut seq: u64 = 0;
    let mut total_bytes = 0u64;
    let mut interval_bytes = 0u64;
    let mut last_report = Instant::now();
    let mut last_elapsed = 0u64;

    // If a bandwidth target is set, pace packets to hit it instead of
    // blasting at line rate (real iperf3 does the same for UDP).
    let pace_delay = cfg
        .bandwidth_bps
        .map(|bps| Duration::from_secs_f64((packet_len as f64 * 8.0) / bps as f64));
    let mut next_send = Instant::now();

    while start.elapsed() < cfg.duration && !stop.load(Ordering::Relaxed) {
        if let Some(delay) = pace_delay {
            pace(&mut next_send, delay).await;
        }

        let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos() as u64;
        packet[0..8].copy_from_slice(&seq.to_be_bytes());
        packet[8..16].copy_from_slice(&ts.to_be_bytes());
        if send_datagram(sock, target, &packet).await.is_err() {
            break; // best-effort; stop rather than error out on a transient send failure
        }
        seq += 1;
        total_bytes += packet_len;
        interval_bytes += packet_len;

        if last_report.elapsed() >= cfg.report_every {
            let secs = last_report.elapsed().as_secs_f64();
            let gbps = (interval_bytes as f64 * 8.0) / secs / 1e9;
            last_elapsed = start.elapsed().as_secs();
            on_interval(IntervalReport {
                interval_bytes,
                gbps,
                elapsed_secs: last_elapsed,
                total_bytes,
                kstats: None,
            });
            interval_bytes = 0;
            last_report = Instant::now();
        }
    }

    // Flush the leftover bytes as a final, shorter sample (see duplex.rs).
    if interval_bytes > 0 && last_report.elapsed() >= MIN_PARTIAL_INTERVAL {
        let secs = last_report.elapsed().as_secs_f64();
        let gbps = (interval_bytes as f64 * 8.0) / secs / 1e9;
        last_elapsed = start.elapsed().as_secs_f64().round() as u64;
        on_interval(IntervalReport {
            interval_bytes,
            gbps,
            elapsed_secs: last_elapsed,
            total_bytes,
            kstats: None,
        });
    }

    // Best-effort FIN so the peer can finalize promptly instead of waiting
    // for its idle timeout; fine if some are dropped.
    let mut fin = vec![0u8; UDP_HEADER];
    fin[0..8].copy_from_slice(&FIN_SEQ.to_be_bytes());
    for _ in 0..3 {
        let _ = send_datagram(sock, target, &fin).await;
    }
    (total_bytes, last_elapsed)
}

/// What a client-side receive loop hands `on_interval` besides the common
/// report: the receiver-measured loss and jitter.
#[derive(Debug, Clone, Copy)]
pub struct RxQuality {
    pub loss_pct: f64,
    pub jitter_ms: f64,
}

/// How a receive loop runs.
pub struct RxConfig<'a> {
    /// Give up after this long overall (so a vanished peer can't hang a test).
    pub max_wait: Duration,
    pub report_every: Duration,
    /// Sent back whenever a repeated handshake arrives (our ack got lost).
    pub reack: Option<&'a [u8]>,
    /// The peer is expected to send back a `UdpSummary` of *our* flow; keep
    /// listening briefly after the stream ends so it isn't missed.
    pub expect_peer_summary: bool,
}

/// What a receive loop returns.
#[derive(Debug, Clone, Copy)]
pub struct RxOutcome {
    pub total_bytes: u64,
    pub last_elapsed: u64,
    /// What this side received (loss and jitter measured here).
    pub own: UdpSummary,
    /// What the peer says it received of our flow, if it told us.
    pub peer: Option<UdpSummary>,
}

/// How long to wait for the peer's summary after a stream ends.
pub const SUMMARY_WAIT: Duration = Duration::from_millis(1500);

/// Receives on a connected socket until the peer's FIN, `stop`, an idle
/// timeout, or `cfg.max_wait`.
pub async fn rx_loop(
    sock: &UdpSocket,
    start: Instant,
    stop: &AtomicBool,
    cfg: RxConfig<'_>,
    mut on_interval: impl FnMut(IntervalReport, RxQuality),
) -> RxOutcome {
    let mut buf = vec![0u8; 65536];
    let mut stats = RecvStats::default();
    let mut peer: Option<UdpSummary> = None;
    let mut last_report = Instant::now();
    let mut last_packet = Instant::now();
    let mut last_elapsed = 0u64;
    let (mut fin, mut got_data) = (false, false);

    while !stop.load(Ordering::Relaxed)
        && start.elapsed() < cfg.max_wait
        && last_packet.elapsed() < IDLE_TIMEOUT
    {
        // Wait (bounded) for the first datagram, then drain whatever else is
        // already queued without awaiting again: one timer and one set of
        // clock reads per burst instead of per packet. At line rate that
        // per-packet overhead is what makes a receiver fall behind.
        if let Ok(Ok(mut n)) =
            tokio::time::timeout(Duration::from_millis(200), sock.recv(&mut buf)).await
        {
            let mut drained = 0;
            loop {
                let data = &buf[..n];
                if looks_like_json(data) {
                    if let Ok(summary) = decode_datagram::<UdpSummary>(data) {
                        peer = Some(summary);
                    } else if let Some(ack) = cfg.reack {
                        // A repeated handshake: our ack got lost, say it again.
                        let _ = sock.send(ack).await;
                    }
                } else if data.len() >= UDP_HEADER {
                    if is_fin(data) {
                        fin = true;
                        break;
                    }
                    stats.on_packet(data);
                    got_data = true;
                }
                drained += 1;
                match sock.try_recv(&mut buf) {
                    Ok(next) if drained < 256 => n = next,
                    _ => break,
                }
            }
            if got_data {
                last_packet = Instant::now();
                got_data = false;
            }
            if fin {
                break;
            }
        }

        if last_report.elapsed() >= cfg.report_every {
            let secs = last_report.elapsed().as_secs_f64();
            let gbps = (stats.bytes_interval as f64 * 8.0) / secs / 1e9;
            last_elapsed = start.elapsed().as_secs();
            on_interval(
                IntervalReport {
                    interval_bytes: stats.bytes_interval,
                    gbps,
                    elapsed_secs: last_elapsed,
                    total_bytes: stats.bytes_total,
                    kstats: None,
                },
                RxQuality { loss_pct: stats.loss_pct(), jitter_ms: stats.jitter_ms() },
            );
            stats.bytes_interval = 0;
            last_report = Instant::now();
        }
    }
    if stats.bytes_interval > 0 && last_report.elapsed() >= MIN_PARTIAL_INTERVAL {
        let secs = last_report.elapsed().as_secs_f64();
        let gbps = (stats.bytes_interval as f64 * 8.0) / secs / 1e9;
        last_elapsed = start.elapsed().as_secs_f64().round() as u64;
        on_interval(
            IntervalReport {
                interval_bytes: stats.bytes_interval,
                gbps,
                elapsed_secs: last_elapsed,
                total_bytes: stats.bytes_total,
                kstats: None,
            },
            RxQuality { loss_pct: stats.loss_pct(), jitter_ms: stats.jitter_ms() },
        );
    }
    if cfg.expect_peer_summary && peer.is_none() {
        peer = await_peer_summary(sock, SUMMARY_WAIT).await;
    }
    RxOutcome { total_bytes: stats.bytes_total, last_elapsed, own: stats.summary(), peer }
}

/// Waits up to `timeout` for the peer's `UdpSummary`, ignoring anything else.
pub async fn await_peer_summary(sock: &UdpSocket, timeout: Duration) -> Option<UdpSummary> {
    let mut buf = [0u8; 512];
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.checked_duration_since(Instant::now())?;
        match tokio::time::timeout(left, sock.recv(&mut buf)).await {
            Ok(Ok(n)) => {
                if let Ok(summary) = decode_datagram::<UdpSummary>(&buf[..n]) {
                    return Some(summary);
                }
            }
            _ => return None,
        }
    }
}

/// Sends a summary to the peer a few times — it is a single UDP datagram and
/// the sender is waiting for it.
pub async fn send_summary(sock: &UdpSocket, summary: &UdpSummary) {
    let Ok(bytes) = super::protocol::encode_datagram(summary) else { return };
    for _ in 0..3 {
        let _ = sock.send(&bytes).await;
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(seq: u64, ts_ns: u64) -> Vec<u8> {
        let mut p = vec![0u8; 64];
        p[0..8].copy_from_slice(&seq.to_be_bytes());
        p[8..16].copy_from_slice(&ts_ns.to_be_bytes());
        p
    }

    #[test]
    fn sequence_gaps_count_as_loss() {
        let mut s = RecvStats::default();
        for seq in [0, 1, 4, 5] {
            s.on_packet(&packet(seq, 0));
        }
        // seqs 2 and 3 never arrived: 2 lost out of 6 expected.
        assert!((s.loss_pct() - 100.0 / 3.0).abs() < 1e-9, "{}", s.loss_pct());
        assert_eq!(s.bytes_total, 4 * 64);
    }

    #[test]
    fn late_duplicate_does_not_count_as_loss() {
        let mut s = RecvStats::default();
        for seq in [0, 1, 2, 1] {
            s.on_packet(&packet(seq, 0));
        }
        assert_eq!(s.loss_pct(), 0.0);
    }

    #[test]
    fn classifies_fin_and_json_control_datagrams() {
        assert!(is_fin(&packet(FIN_SEQ, 0)));
        assert!(!is_fin(&packet(7, 0)));
        assert!(looks_like_json(br#"{"ok":true,"error":null}"#));
        assert!(!looks_like_json(&packet(0, 0)));
        assert!(!looks_like_json(&packet(FIN_SEQ, 0)));
    }
}
