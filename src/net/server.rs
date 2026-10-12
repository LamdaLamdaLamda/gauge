use std::collections::HashSet;
use std::net::SocketAddr;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use tokio::net::{TcpListener, TcpSocket, TcpStream, UdpSocket};
use tokio::sync::mpsc::UnboundedSender;

use super::protocol::{
    Direction, Handshake, HandshakeAck, check_client_version, decode_datagram, encode_datagram,
    read_message, write_message,
};
use super::{Flow, NetEvent, ServerParams, StreamStats};
use super::{duplex, sockopt, udp};

// ───────────────────────── TCP ─────────────────────────

/// Resolves `bind_addr` and binds+listens, applying `window_bytes`
/// (SO_SNDBUF/SO_RCVBUF) to the *listening* socket before `listen()` if
/// given — on POSIX systems, sockets returned by `accept()` inherit these
/// from it, so every connection benefits without re-configuring each one
/// individually (which tokio's plain, already-accepted `TcpStream` doesn't
/// expose a way to do anyway).
async fn bind_with_window(
    bind_addr: &str,
    window_bytes: Option<u32>,
) -> std::io::Result<TcpListener> {
    let sockaddr = tokio::net::lookup_host(bind_addr).await?.next().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("no addresses resolved for {bind_addr}"),
        )
    })?;
    let socket = if sockaddr.is_ipv4() { TcpSocket::new_v4()? } else { TcpSocket::new_v6()? };
    socket.set_reuseaddr(true)?;
    if let Some(bytes) = window_bytes {
        let _ = socket.set_send_buffer_size(bytes);
        let _ = socket.set_recv_buffer_size(bytes);
    }
    socket.bind(sockaddr)?;
    socket.listen(1024)
}

pub async fn run_tcp(params: ServerParams, tx: UnboundedSender<NetEvent>) {
    let bind_addr = format!("{}:{}", params.bind, params.port);
    let listener = match bind_with_window(&bind_addr, params.window_bytes).await {
        Ok(l) => l,
        Err(e) => {
            let _ =
                tx.send(NetEvent::ServerListenError(format!("bind to {bind_addr} failed: {e}")));
            return;
        }
    };
    // Report the actually-bound address (not the requested one) — matters
    // when port 0 asks the OS to pick one, which the demo tool never does
    // itself but the test suite relies on.
    let actual = listener.local_addr().map(|a| a.to_string()).unwrap_or(bind_addr);
    let _ = tx.send(NetEvent::ServerListening { bind: actual });

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(_) => continue,
        };
        tokio::spawn(handle_tcp_client(stream, peer.to_string(), tx.clone()));
    }
}

pub(super) fn send_interval(
    tx: &UnboundedSender<NetEvent>,
    session_id: u64,
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
    let _ = tx.send(NetEvent::ServerStreamInterval {
        session_id,
        stream_index,
        flow,
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

pub(super) fn send_finished(
    tx: &UnboundedSender<NetEvent>,
    session_id: u64,
    stream_index: u8,
    flow: Flow,
    avg_gbps: f64,
) {
    let _ = tx.send(NetEvent::ServerStreamFinished { session_id, stream_index, flow, avg_gbps });
}

async fn handle_tcp_client(mut stream: TcpStream, addr: String, tx: UnboundedSender<NetEvent>) {
    let _ = stream.set_nodelay(true);

    let handshake: Handshake = match read_message(&mut stream).await {
        Ok(h) => h,
        Err(_) => return,
    };
    if let Err(reason) = check_client_version(handshake.protocol_version) {
        let _ = write_message(&mut stream, &HandshakeAck::refused(reason)).await;
        return;
    }
    if write_message(&mut stream, &HandshakeAck::ok()).await.is_err() {
        return;
    }
    // Mirror the client's QoS marking / congestion choice on our own
    // sending side. Best effort: the client already got its ack, and a
    // failure here (e.g. `bbr` not available on this host) only affects
    // what the server sends in Rx/Bidir.
    let fd = stream.as_raw_fd();
    if let Some(tos) = handshake.tos.filter(|t| *t != 0) {
        let ipv6 = stream.local_addr().is_ok_and(|a| a.is_ipv6());
        let _ = sockopt::set_tos(fd, tos, ipv6);
    }
    if let Some(name) = &handshake.congestion {
        let _ = sockopt::set_congestion(fd, name);
    }

    let stop = Arc::new(AtomicBool::new(false));
    let _ = tx.send(NetEvent::ServerStreamConnected {
        session_id: handshake.session_id,
        stream_index: handshake.stream_index,
        stream_count: handshake.stream_count,
        addr,
        stop: stop.clone(),
        duration_secs: handshake.duration_secs,
    });

    let session_id = handshake.session_id;
    let stream_index = handshake.stream_index;
    let start = Instant::now();
    let duration = Duration::from_secs(handshake.duration_secs.max(1));
    let fd = stream.as_raw_fd();
    let chunk_bytes = handshake.chunk_bytes.map(|b| b as usize).unwrap_or(duplex::CHUNK);
    let report_every = handshake.report_every();

    let (mut rh, mut wh) = stream.split();
    duplex::run_direction(
        handshake.direction.mirrored(),
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
        |flow, report| send_interval(&tx, session_id, stream_index, flow, report),
        |flow, _total, _secs, avg| send_finished(&tx, session_id, stream_index, flow, avg),
    )
    .await;

    let _ = tx.send(NetEvent::ServerStreamDisconnected { session_id, stream_index });
}

// ───────────────────────── UDP ─────────────────────────
// The listening socket only handles handshakes. Each accepted stream then
// gets its own `SO_REUSEPORT` socket on the same port, connected to the
// client (see `udp::reuseport_socket`), and its own task that receives,
// reports and sends for that stream alone. Receiving used to be one loop for
// every stream; with several streams that single thread was the bottleneck
// and most of the traffic was dropped by the kernel.

pub async fn run_udp(params: ServerParams, tx: UnboundedSender<NetEvent>) {
    let bind_addr = format!("{}:{}", params.bind, params.port);
    let listen_error = |msg: String| {
        let _ = tx.send(NetEvent::ServerListenError(msg));
    };
    let sockaddr = match tokio::net::lookup_host(&bind_addr).await.map(|mut a| a.next()) {
        Ok(Some(a)) => a,
        _ => return listen_error(format!("bind to {bind_addr} failed: no address resolved")),
    };
    let socket = match udp::reuseport_socket(sockaddr, None) {
        Ok(s) => s,
        Err(e) => return listen_error(format!("bind to {bind_addr} failed: {e}")),
    };
    // UDP's "window" is the socket buffer size.
    if let Some(bytes) = params.window_bytes {
        let _ = sockopt::grow_socket_buffers(socket.as_raw_fd(), bytes);
    }
    let Ok(local) = socket.local_addr() else {
        return listen_error(format!("bind to {bind_addr} failed: no local address"));
    };
    let _ = tx.send(NetEvent::ServerListening { bind: local.to_string() });

    let mut peers: HashSet<SocketAddr> = HashSet::new();
    let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel::<SocketAddr>();
    let mut buf = vec![0u8; 65536];

    loop {
        tokio::select! {
            res = socket.recv_from(&mut buf) => {
                let Ok((n, peer_addr)) = res else { continue };
                // Only handshakes arrive here; established peers' datagrams
                // go to their own sockets. Anything else is dropped.
                let Ok(hs) = decode_datagram::<Handshake>(&buf[..n]) else { continue };
                let reply = |ack: HandshakeAck| encode_datagram(&ack).unwrap_or_default();
                if let Err(reason) = check_client_version(hs.protocol_version) {
                    let _ = socket.send_to(&reply(HandshakeAck::refused(reason)), peer_addr).await;
                    continue;
                }
                if peers.contains(&peer_addr) {
                    // A retransmitted handshake that raced the peer socket's creation.
                    let _ = socket.send_to(&reply(HandshakeAck::ok()), peer_addr).await;
                    continue;
                }
                match start_udp_peer(local, peer_addr, hs, params.window_bytes, &tx, &done_tx) {
                    Ok(()) => {
                        peers.insert(peer_addr);
                        let _ = socket.send_to(&reply(HandshakeAck::ok()), peer_addr).await;
                    }
                    Err(e) => {
                        let msg = format!("server could not set up a socket for this stream: {e}");
                        let _ = socket.send_to(&reply(HandshakeAck::refused(msg)), peer_addr).await;
                    }
                }
            }
            Some(addr) = done_rx.recv() => {
                peers.remove(&addr);
            }
        }
    }
}

/// Creates the stream's dedicated socket and starts its task. The socket must
/// exist *before* the client is acked, so that nothing the client sends after
/// the ack can reach the listening socket instead.
fn start_udp_peer(
    local: SocketAddr,
    peer_addr: SocketAddr,
    hs: Handshake,
    window_bytes: Option<u32>,
    tx: &UnboundedSender<NetEvent>,
    done: &tokio::sync::mpsc::UnboundedSender<SocketAddr>,
) -> std::io::Result<()> {
    let sock = udp::reuseport_socket(local, Some(peer_addr))?;
    let fd = sock.as_raw_fd();
    if let Some(bytes) = window_bytes {
        let _ = sockopt::grow_socket_buffers(fd, bytes);
    }
    // Per stream now, so the client's TOS request is honored exactly.
    if let Some(tos) = hs.tos.filter(|t| *t != 0) {
        let _ = sockopt::set_tos(fd, tos, peer_addr.is_ipv6());
    }

    let stop = Arc::new(AtomicBool::new(false));
    let _ = tx.send(NetEvent::ServerStreamConnected {
        session_id: hs.session_id,
        stream_index: hs.stream_index,
        stream_count: hs.stream_count,
        addr: peer_addr.to_string(),
        stop: stop.clone(),
        duration_secs: hs.duration_secs,
    });
    tokio::spawn(udp_peer(Arc::new(sock), peer_addr, hs, stop, tx.clone(), done.clone()));
    Ok(())
}

/// Everything the server does for one UDP stream.
async fn udp_peer(
    sock: Arc<UdpSocket>,
    peer_addr: SocketAddr,
    hs: Handshake,
    stop: Arc<AtomicBool>,
    tx: UnboundedSender<NetEvent>,
    done: tokio::sync::mpsc::UnboundedSender<SocketAddr>,
) {
    let (session_id, stream_index) = (hs.session_id, hs.stream_index);
    let start = Instant::now();
    let duration = Duration::from_secs(hs.duration_secs.max(1));
    let report_every = hs.report_every();

    // Server-to-client half (Rx/Bidir): its own task, so it never competes
    // with the receive loop below for the same thread.
    let send_task = (hs.direction != Direction::Tx).then(|| {
        let (sock, stop, tx) = (sock.clone(), stop.clone(), tx.clone());
        let cfg = udp::TxConfig {
            packet_size: udp::packet_size_for_mtu(hs.mtu.unwrap_or(1500)),
            duration,
            bandwidth_bps: hs.bandwidth_bps,
            report_every,
        };
        tokio::spawn(async move {
            let (total, _) = udp::tx_loop(cfg, start, &stop, &sock, None, |report| {
                send_interval(&tx, session_id, stream_index, Flow::Tx, report)
            })
            .await;
            let avg_gbps = (total as f64 * 8.0) / start.elapsed().as_secs_f64().max(0.001) / 1e9;
            send_finished(&tx, session_id, stream_index, Flow::Tx, avg_gbps);
        })
    });

    // Client-to-server half (Tx/Bidir), here on the peer task itself.
    if hs.direction != Direction::Rx {
        let ack = encode_datagram(&HandshakeAck::ok()).unwrap_or_default();
        let max_wait = duration + udp::IDLE_TIMEOUT * 2;
        let cfg =
            udp::RxConfig { max_wait, report_every, reack: Some(&ack), expect_peer_summary: false };
        let outcome = udp::rx_loop(&sock, start, &stop, cfg, |report, q| {
            send_udp_rx_interval(&tx, session_id, stream_index, report, q)
        })
        .await;
        let avg_gbps =
            (outcome.total_bytes as f64 * 8.0) / start.elapsed().as_secs_f64().max(0.001) / 1e9;
        send_finished(&tx, session_id, stream_index, Flow::Rx, avg_gbps);
        // Tell the sender what arrived — it has no other way to know.
        udp::send_summary(&sock, &outcome.own).await;
    }

    if let Some(task) = send_task {
        let _ = task.await;
    }
    let _ = tx.send(NetEvent::ServerStreamDisconnected { session_id, stream_index });
    let _ = done.send(peer_addr);
}

/// A receiving UDP stream's interval: unlike TCP, the loss and jitter are
/// measured by this side.
fn send_udp_rx_interval(
    tx: &UnboundedSender<NetEvent>,
    session_id: u64,
    stream_index: u8,
    report: duplex::IntervalReport,
    quality: udp::RxQuality,
) {
    let _ = tx.send(NetEvent::ServerStreamInterval {
        session_id,
        stream_index,
        flow: Flow::Rx,
        stats: StreamStats {
            gbps: report.gbps,
            elapsed_secs: report.elapsed_secs,
            loss_pct: Some(quality.loss_pct),
            jitter_ms: Some(quality.jitter_ms),
            retransmits: None,
            cwnd_bytes: None,
        },
    });
}
