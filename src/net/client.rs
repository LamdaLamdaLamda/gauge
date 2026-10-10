use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use tokio::net::{TcpSocket, TcpStream, UdpSocket};
use tokio::sync::mpsc::UnboundedSender;

use super::protocol::{
    Direction, Handshake, HandshakeAck, decode_datagram, encode_datagram, read_message,
    write_message,
};
use super::{ClientParams, Flow, NetEvent, Proto, StreamStats};
use super::{duplex, sockopt, udp};

/// Resolves `addr` and connects, applying the window (SO_SNDBUF/SO_RCVBUF),
/// TOS and congestion-control settings before connecting. `TcpStream::
/// connect` can't do this — pre-connection socket options need the
/// `TcpSocket` builder, which only takes a resolved `SocketAddr`, not a
/// hostname string. The error is a ready-to-show message.
async fn connect_tcp(addr: &str, params: &ClientParams) -> Result<TcpStream, String> {
    let sockaddr = tokio::net::lookup_host(addr)
        .await
        .map_err(|e| format!("connect to {addr} failed: {e}"))?
        .next()
        .ok_or_else(|| format!("connect to {addr} failed: no addresses resolved"))?;
    let socket = if sockaddr.is_ipv4() { TcpSocket::new_v4() } else { TcpSocket::new_v6() }
        .map_err(|e| format!("connect to {addr} failed: {e}"))?;
    if let Some(bytes) = params.window_bytes {
        let _ = socket.set_send_buffer_size(bytes);
        let _ = socket.set_recv_buffer_size(bytes);
    }
    let fd = socket.as_raw_fd();
    if let Some(tos) = params.tos.filter(|t| *t != 0) {
        sockopt::set_tos(fd, tos, sockaddr.is_ipv6())
            .map_err(|e| format!("setting TOS {tos:#04x} failed: {e}"))?;
    }
    // Unlike the window (a hint the OS may clamp), an explicitly requested
    // congestion algorithm that can't be applied is an error — silently
    // measuring with a different one would be a misleading result.
    if let Some(name) = &params.congestion {
        sockopt::set_congestion(fd, name)
            .map_err(|e| format!("setting TCP congestion control '{name}' failed: {e}"))?;
    }
    socket.connect(sockaddr).await.map_err(|e| format!("connect to {addr} failed: {e}"))
}

pub(crate) use udp::packet_size_for_mtu as udp_packet_size;

pub(super) fn report_interval(params: &ClientParams) -> Duration {
    Duration::from_secs(params.interval_secs.max(1))
}

pub async fn run_stream(
    params: ClientParams,
    stream_index: u8,
    stop: Arc<AtomicBool>,
    tx: UnboundedSender<NetEvent>,
) {
    match params.proto {
        Proto::Tcp => run_tcp_stream(params, stream_index, stop, tx).await,
        Proto::Udp => run_udp_stream(params, stream_index, stop, tx).await,
        Proto::Quic => super::quic::run_client_stream(params, stream_index, stop, tx).await,
    }
}

pub(super) fn handshake_for(params: &ClientParams, stream_index: u8) -> Handshake {
    Handshake {
        duration_secs: params.duration_secs,
        session_id: params.session_id,
        stream_index,
        stream_count: params.stream_count,
        direction: params.direction,
        chunk_bytes: params.buffer_bytes,
        tos: params.tos,
        congestion: params.congestion.clone(),
        bandwidth_bps: params.bandwidth_bps,
        mtu: Some(params.mtu),
        interval_secs: Some(params.interval_secs),
    }
}

async fn run_tcp_stream(
    params: ClientParams,
    stream_index: u8,
    stop: Arc<AtomicBool>,
    tx: UnboundedSender<NetEvent>,
) {
    let addr = format!("{}:{}", params.host, params.port);
    let mut stream = match connect_tcp(&addr, &params).await {
        Ok(s) => s,
        Err(msg) => {
            let _ = tx.send(err(stream_index, msg));
            return;
        }
    };
    let _ = stream.set_nodelay(true);

    let hs_start = Instant::now();
    if let Err(e) = write_message(&mut stream, &handshake_for(&params, stream_index)).await {
        let _ = tx.send(err(stream_index, format!("handshake failed: {e}")));
        return;
    }
    match read_message::<HandshakeAck, _>(&mut stream).await {
        Ok(a) if a.ok => {}
        Ok(a) => {
            let reason = a.error.unwrap_or_else(|| "server rejected handshake".into());
            let _ = tx.send(err(stream_index, reason));
            return;
        }
        Err(e) => {
            let _ = tx.send(err(stream_index, format!("handshake ack failed: {e}")));
            return;
        }
    }
    let rtt_ms = hs_start.elapsed().as_secs_f64() * 1000.0;
    let _ = tx.send(NetEvent::ClientStreamConnected { stream_index, rtt_ms });

    let start = Instant::now();
    let duration = Duration::from_secs(params.duration_secs.max(1));
    let fd = stream.as_raw_fd();
    let chunk_bytes = params.buffer_bytes.map(|b| b as usize).unwrap_or(duplex::CHUNK);
    let report_every = report_interval(&params);
    let (mut rh, mut wh) = stream.split();
    duplex::run_direction(
        params.direction,
        &mut rh,
        &mut wh,
        duplex::LoopContext {
            start,
            stop: &stop,
            stats: duplex::StatsSource::Tcp(fd),
            chunk_bytes,
            report_every,
        },
        duration,
        |flow, report| send_interval(&tx, stream_index, flow, report),
        |flow, total, secs, avg| send_finished(&tx, stream_index, flow, total, secs, avg),
    )
    .await;
}

async fn run_udp_stream(
    params: ClientParams,
    stream_index: u8,
    stop: Arc<AtomicBool>,
    tx: UnboundedSender<NetEvent>,
) {
    let addr = format!("{}:{}", params.host, params.port);
    let sock = match UdpSocket::bind("0.0.0.0:0").await {
        Ok(s) => s,
        Err(e) => {
            let _ = tx.send(err(stream_index, format!("udp bind failed: {e}")));
            return;
        }
    };
    if let Err(e) = sock.connect(&addr).await {
        let _ = tx.send(err(stream_index, format!("udp connect to {addr} failed: {e}")));
        return;
    }
    if let Some(tos) = params.tos.filter(|t| *t != 0) {
        let ipv6 = sock.peer_addr().is_ok_and(|a| a.is_ipv6());
        if let Err(e) = sockopt::set_tos(sock.as_raw_fd(), tos, ipv6) {
            let _ = tx.send(err(stream_index, format!("setting TOS {tos:#04x} failed: {e}")));
            return;
        }
    }

    let hs_bytes = match encode_datagram(&handshake_for(&params, stream_index)) {
        Ok(b) => b,
        Err(e) => {
            let _ = tx.send(err(stream_index, format!("encode handshake failed: {e}")));
            return;
        }
    };

    let hs_start = Instant::now();
    let mut acked = false;
    let mut ack_error: Option<String> = None;
    let mut ack_buf = [0u8; 512];
    for _ in 0..8 {
        if sock.send(&hs_bytes).await.is_err() {
            continue;
        }
        if let Ok(Ok(n)) =
            tokio::time::timeout(Duration::from_millis(250), sock.recv(&mut ack_buf)).await
            && let Ok(ack) = decode_datagram::<HandshakeAck>(&ack_buf[..n])
        {
            if ack.ok {
                acked = true;
            } else {
                ack_error = ack.error;
            }
            break;
        }
    }
    if !acked {
        let reason = ack_error.unwrap_or_else(|| "udp handshake: no response from server".into());
        let _ = tx.send(err(stream_index, reason));
        return;
    }
    let rtt_ms = hs_start.elapsed().as_secs_f64() * 1000.0;
    let _ = tx.send(NetEvent::ClientStreamConnected { stream_index, rtt_ms });

    let start = Instant::now();
    let duration = Duration::from_secs(params.duration_secs.max(1));
    let report_every = report_interval(&params);
    let sock = &sock;
    let stop = &*stop;
    let tx = &tx;

    // Sending (Tx/Bidir) and receiving (Rx/Bidir) run concurrently on the
    // one connected socket, just like the TCP duplex loops.
    let send_part = async {
        if params.direction == Direction::Rx {
            return;
        }
        let cfg = udp::TxConfig {
            packet_size: udp_packet_size(params.mtu),
            duration,
            bandwidth_bps: params.bandwidth_bps,
            report_every,
        };
        let (total, last_elapsed) = udp::tx_loop(cfg, start, stop, sock, None, |report| {
            send_interval(tx, stream_index, Flow::Tx, report)
        })
        .await;
        let avg = (total as f64 * 8.0) / start.elapsed().as_secs_f64().max(0.001) / 1e9;
        send_finished(tx, stream_index, Flow::Tx, total, last_elapsed.max(1), avg);
    };
    let recv_part = async {
        if params.direction == Direction::Tx {
            return;
        }
        // The server stops sending at `duration`; leave it a few seconds of
        // slack for the FIN / in-flight packets before giving up.
        let max_wait = duration + udp::IDLE_TIMEOUT;
        let (total, last_elapsed) =
            udp::rx_loop(sock, start, stop, max_wait, report_every, |report, quality| {
                send_udp_rx_interval(tx, stream_index, report, quality)
            })
            .await;
        let avg = (total as f64 * 8.0) / start.elapsed().as_secs_f64().max(0.001) / 1e9;
        send_finished(tx, stream_index, Flow::Rx, total, last_elapsed.max(1), avg);
    };
    tokio::join!(send_part, recv_part);
}

pub(super) fn err(stream_index: u8, message: String) -> NetEvent {
    NetEvent::ClientError { stream_index: Some(stream_index), message }
}

pub(super) fn send_interval(
    tx: &UnboundedSender<NetEvent>,
    stream_index: u8,
    flow: Flow,
    report: duplex::IntervalReport,
) {
    // loss_pct is a send-side estimate (retransmitted / sent segments) —
    // only meaningful when `report.total_bytes` is bytes *we* sent, i.e.
    // Flow::Tx.
    let loss_pct = if flow == Flow::Tx {
        report.kstats.and_then(|k| k.loss_pct(report.total_bytes))
    } else {
        None
    };
    let _ = tx.send(NetEvent::ClientStreamInterval {
        stream_index,
        flow,
        interval_bytes: report.interval_bytes,
        stats: StreamStats {
            gbps: report.gbps,
            elapsed_secs: report.elapsed_secs,
            retransmits: report.kstats.map(|k| k.retransmits),
            cwnd_bytes: report.kstats.map(|k| k.cwnd_bytes),
            loss_pct,
            ..Default::default()
        },
    });
}

fn send_udp_rx_interval(
    tx: &UnboundedSender<NetEvent>,
    stream_index: u8,
    report: duplex::IntervalReport,
    quality: udp::RxQuality,
) {
    let _ = tx.send(NetEvent::ClientStreamInterval {
        stream_index,
        flow: Flow::Rx,
        interval_bytes: report.interval_bytes,
        stats: StreamStats {
            gbps: report.gbps,
            elapsed_secs: report.elapsed_secs,
            loss_pct: Some(quality.loss_pct),
            jitter_ms: Some(quality.jitter_ms),
            ..Default::default()
        },
    });
}

pub(super) fn send_finished(
    tx: &UnboundedSender<NetEvent>,
    stream_index: u8,
    flow: Flow,
    total_bytes: u64,
    elapsed_secs: u64,
    avg_gbps: f64,
) {
    let _ = tx.send(NetEvent::ClientStreamFinished {
        stream_index,
        flow,
        total_bytes,
        elapsed_secs,
        avg_gbps,
    });
}
