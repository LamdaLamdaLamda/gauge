//! Shared TCP read/write loops. Both the client and the server now need
//! *both* directions (Tx/Rx/Bidir), so the actual byte-shoveling + interval
//! reporting lives here once, driven via callbacks so each caller can still
//! send its own flavor of `NetEvent` (client vs. server events differ in
//! shape — the server's carry a `session_id`, the client's don't).

use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::Flow;
use super::protocol::Direction;
use super::tcpinfo;

pub const CHUNK: usize = 128 * 1024;
/// A final partial interval shorter than this is not reported on its own.
pub const MIN_PARTIAL_INTERVAL: Duration = Duration::from_millis(100);

/// Everything a periodic `on_interval` callback gets, bundled so the
/// callback signature doesn't grow a new positional parameter every time a
/// new live stat is added.
#[derive(Debug, Clone, Copy)]
pub struct IntervalReport {
    pub interval_bytes: u64,
    pub gbps: f64,
    pub elapsed_secs: u64,
    /// Running total of bytes written (`write_loop`) or read (`read_loop`)
    /// so far — the caller needs this to turn `kstats` into an estimated
    /// loss percentage without duplicating a byte counter. Only meaningful
    /// as a *sent*-bytes count on the `write_loop` side; see its doc.
    pub total_bytes: u64,
    pub kstats: Option<tcpinfo::TcpKernelStats>,
}

/// Where a loop reads its live transport statistics from each interval.
#[derive(Clone)]
pub enum StatsSource {
    /// The kernel's `TCP_INFO` for this socket.
    Tcp(RawFd),
    /// quinn's own connection statistics (QUIC has no kernel state).
    Quic(quinn::Connection),
}

impl StatsSource {
    pub fn query(&self) -> Option<tcpinfo::TcpKernelStats> {
        match self {
            StatsSource::Tcp(fd) => tcpinfo::query(*fd),
            StatsSource::Quic(conn) => {
                let path = conn.stats().path;
                Some(tcpinfo::TcpKernelStats {
                    retransmits: path.lost_packets,
                    mss: path.current_mtu as u32,
                    cwnd_bytes: u32::try_from(path.cwnd).unwrap_or(u32::MAX),
                    segments_sent: Some(path.sent_packets),
                })
            }
        }
    }
}

/// The parts of a `write_loop`/`read_loop` call that don't change once the
/// stream's underlying socket is chosen — bundled so adding one doesn't
/// grow the functions' own parameter lists (they're already long once the
/// generic stream + both callbacks are counted).
#[derive(Clone)]
pub struct LoopContext<'a> {
    pub start: Instant,
    pub stop: &'a AtomicBool,
    /// Source of the live transport stats read once per interval. Passed in
    /// rather than derived from the stream because it may be a split half
    /// (bidir mode), which doesn't expose the socket itself.
    pub stats: StatsSource,
    /// Size of each `write_all`/`read` call (the "Buffer size" setting,
    /// iperf3's `-l`) — defaults to `CHUNK`.
    pub chunk_bytes: usize,
    /// How often `on_interval` fires (the "Interval (s)" setting).
    pub report_every: Duration,
}

/// Writes zero-filled chunks for up to `duration` (or until `ctx.stop`),
/// calling `on_interval` roughly once per `ctx.report_every` and `on_finished` once done.
/// Shared by the TX sender (whichever side that is for this test).
pub async fn write_loop<W>(
    mut w: W,
    ctx: LoopContext<'_>,
    duration: Duration,
    mut on_interval: impl FnMut(IntervalReport),
    on_finished: impl FnOnce(u64, u64, f64),
) where
    W: AsyncWrite + Unpin,
{
    let LoopContext { start, stop, stats, chunk_bytes, report_every } = ctx;
    let buf = vec![0u8; chunk_bytes.max(1)];
    let mut total_bytes = 0u64;
    let mut interval_bytes = 0u64;
    let mut last_report = Instant::now();
    let mut last_elapsed = 0u64;

    while start.elapsed() < duration && !stop.load(Ordering::Relaxed) {
        if w.write_all(&buf).await.is_err() {
            break;
        }
        total_bytes += buf.len() as u64;
        interval_bytes += buf.len() as u64;

        if last_report.elapsed() >= report_every {
            let secs = last_report.elapsed().as_secs_f64();
            let gbps = (interval_bytes as f64 * 8.0) / secs / 1e9;
            last_elapsed = start.elapsed().as_secs();
            on_interval(IntervalReport {
                interval_bytes,
                gbps,
                elapsed_secs: last_elapsed,
                total_bytes,
                kstats: stats.query(),
            });
            interval_bytes = 0;
            last_report = Instant::now();
        }
    }

    // The test (almost never) ends exactly on a report boundary — flush the
    // leftover bytes as one last, shorter sample so the live numbers add up
    // to the exact total. Skipped when it is only a sliver after a regular
    // report, whose rate would be meaningless.
    if interval_bytes > 0 && last_report.elapsed() >= MIN_PARTIAL_INTERVAL {
        let secs = last_report.elapsed().as_secs_f64();
        let gbps = (interval_bytes as f64 * 8.0) / secs / 1e9;
        last_elapsed = start.elapsed().as_secs_f64().round() as u64;
        on_interval(IntervalReport {
            interval_bytes,
            gbps,
            elapsed_secs: last_elapsed,
            total_bytes,
            kstats: stats.query(),
        });
    }
    let _ = w.shutdown().await;

    let elapsed = start.elapsed().as_secs_f64().max(0.001);
    let avg_gbps = (total_bytes as f64 * 8.0) / elapsed / 1e9;
    on_finished(total_bytes, last_elapsed.max(1), avg_gbps);
}

/// Reads until EOF (the peer's `write_loop` shutting down, which is the
/// normal end-of-test signal) or `ctx.stop`, calling `on_interval` roughly
/// once a second and `on_finished` once done. Shared by the RX receiver.
/// The running total passed to `on_interval` is bytes *received*, not sent
/// — callers computing a send-side loss estimate should ignore it here
/// (see `write_loop`'s doc comment).
pub async fn read_loop<R>(
    mut r: R,
    ctx: LoopContext<'_>,
    mut on_interval: impl FnMut(IntervalReport),
    on_finished: impl FnOnce(u64, u64, f64),
) where
    R: AsyncRead + Unpin,
{
    let LoopContext { start, stop, stats, chunk_bytes, report_every } = ctx;
    let mut buf = vec![0u8; chunk_bytes.max(1)];
    let mut total_bytes = 0u64;
    let mut interval_bytes = 0u64;
    let mut last_report = Instant::now();
    let mut last_elapsed = 0u64;

    while !stop.load(Ordering::Relaxed) {
        match r.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                total_bytes += n as u64;
                interval_bytes += n as u64;
            }
            Err(_) => break,
        }
        if last_report.elapsed() >= report_every {
            let secs = last_report.elapsed().as_secs_f64();
            let gbps = (interval_bytes as f64 * 8.0) / secs / 1e9;
            last_elapsed = start.elapsed().as_secs();
            on_interval(IntervalReport {
                interval_bytes,
                gbps,
                elapsed_secs: last_elapsed,
                total_bytes,
                kstats: stats.query(),
            });
            interval_bytes = 0;
            last_report = Instant::now();
        }
    }

    // The test (almost never) ends exactly on a report boundary — flush the
    // leftover bytes as one last, shorter sample so the live numbers add up
    // to the exact total. Skipped when it is only a sliver after a regular
    // report, whose rate would be meaningless.
    if interval_bytes > 0 && last_report.elapsed() >= MIN_PARTIAL_INTERVAL {
        let secs = last_report.elapsed().as_secs_f64();
        let gbps = (interval_bytes as f64 * 8.0) / secs / 1e9;
        last_elapsed = start.elapsed().as_secs_f64().round() as u64;
        on_interval(IntervalReport {
            interval_bytes,
            gbps,
            elapsed_secs: last_elapsed,
            total_bytes,
            kstats: stats.query(),
        });
    }

    let elapsed = start.elapsed().as_secs_f64().max(0.001);
    let avg_gbps = (total_bytes as f64 * 8.0) / elapsed / 1e9;
    on_finished(total_bytes, last_elapsed.max(1), avg_gbps);
}

/// Runs the test's direction over an already-connected, handshaken stream
/// pair: Tx writes, Rx reads, Bidir does both concurrently. Shared by every
/// stream transport (TCP halves, QUIC send/recv streams) and both ends of
/// the connection — only the callbacks differ.
pub async fn run_direction<R, W>(
    direction: Direction,
    r: &mut R,
    w: &mut W,
    ctx: LoopContext<'_>,
    duration: Duration,
    on_interval: impl Fn(Flow, IntervalReport),
    on_finished: impl Fn(Flow, u64, u64, f64),
) where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    match direction {
        Direction::Tx => {
            write_loop(
                &mut *w,
                ctx,
                duration,
                |rp| on_interval(Flow::Tx, rp),
                |t, s, a| on_finished(Flow::Tx, t, s, a),
            )
            .await;
        }
        Direction::Rx => {
            read_loop(
                &mut *r,
                ctx,
                |rp| on_interval(Flow::Rx, rp),
                |t, s, a| on_finished(Flow::Rx, t, s, a),
            )
            .await;
        }
        Direction::Bidir => {
            tokio::join!(
                write_loop(
                    &mut *w,
                    ctx.clone(),
                    duration,
                    |rp| on_interval(Flow::Tx, rp),
                    |t, s, a| on_finished(Flow::Tx, t, s, a)
                ),
                read_loop(
                    &mut *r,
                    ctx,
                    |rp| on_interval(Flow::Rx, rp),
                    |t, s, a| on_finished(Flow::Rx, t, s, a)
                ),
            );
        }
    }
}
