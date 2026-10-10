use std::collections::HashMap;
use std::net::SocketAddr;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::net::{TcpListener, TcpSocket, TcpStream, UdpSocket};
use tokio::sync::mpsc::UnboundedSender;

use super::protocol::{
    Direction, Handshake, HandshakeAck, decode_datagram, encode_datagram, read_message,
    write_message,
};
use super::{Flow, NetEvent, ServerParams, StreamStats};
use super::{duplex, sockopt, udp};

/// How often UDP peers are checked for due reports / idleness. Each peer
/// reports on its own (client-chosen) schedule, at most this late.
const SWEEP_EVERY: Duration = Duration::from_millis(100);

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
    if write_message(&mut stream, &HandshakeAck { ok: true, error: None }).await.is_err() {
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
// One unconnected socket serves every peer. Receiving (Tx/Bidir) is
// handled inline in the select loop; sending (Rx/Bidir) runs as a spawned
// task per peer that reports back on a channel when it is done.

struct UdpPeer {
    session_id: u64,
    stream_index: u8,
    last_seen: Instant,
    /// When this peer's last interval report went out, and how often to
    /// send them (the client's chosen interval).
    last_report: Instant,
    report_every: Duration,
    recv: udp::RecvStats,
    start: Instant,
    /// The client-to-server flow is still open (cleared on FIN, idle
    /// timeout or kill; never set for Rx-direction tests, where the client
    /// sends nothing).
    rx_open: bool,
    /// The server-to-client sender task is still running (never set for
    /// Tx-direction tests).
    tx_open: bool,
    /// Set by the App's "kill client" action (via the `Arc` handed out on
    /// `ServerStreamConnected`) — checked on every packet, each sweep tick
    /// and by the sender task, since UDP has no socket to actually close.
    stop: Arc<AtomicBool>,
}

pub async fn run_udp(params: ServerParams, tx: UnboundedSender<NetEvent>) {
    let bind_addr = format!("{}:{}", params.bind, params.port);
    let socket = match UdpSocket::bind(&bind_addr).await {
        Ok(s) => Arc::new(s),
        Err(e) => {
            let _ =
                tx.send(NetEvent::ServerListenError(format!("bind to {bind_addr} failed: {e}")));
            return;
        }
    };
    let actual = socket.local_addr().map(|a| a.to_string()).unwrap_or(bind_addr);
    let _ = tx.send(NetEvent::ServerListening { bind: actual });

    let mut peers: HashMap<SocketAddr, UdpPeer> = HashMap::new();
    let (sender_done_tx, mut sender_done_rx) = tokio::sync::mpsc::unbounded_channel::<SocketAddr>();
    let mut buf = vec![0u8; 65536];
    let mut ticker = tokio::time::interval(SWEEP_EVERY);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            res = socket.recv_from(&mut buf) => {
                let Ok((n, peer_addr)) = res else { continue };
                handle_udp_packet(&socket, &buf[..n], peer_addr, &mut peers, &tx, &sender_done_tx).await;
            }
            Some(addr) = sender_done_rx.recv() => {
                if let Some(peer) = peers.get_mut(&addr) {
                    peer.tx_open = false;
                }
                remove_if_closed(addr, &mut peers, &tx);
            }
            _ = ticker.tick() => {
                report_and_sweep(&mut peers, &tx);
            }
        }
    }
}

async fn handle_udp_packet(
    socket: &Arc<UdpSocket>,
    data: &[u8],
    peer_addr: SocketAddr,
    peers: &mut HashMap<SocketAddr, UdpPeer>,
    tx: &UnboundedSender<NetEvent>,
    sender_done_tx: &tokio::sync::mpsc::UnboundedSender<SocketAddr>,
) {
    // A handshake is valid JSON; bulk data/FIN packets are raw binary and
    // will simply fail to parse as one — that's how the two are told apart.
    if let Ok(hs) = decode_datagram::<Handshake>(data) {
        let ack = encode_datagram(&HandshakeAck { ok: true, error: None }).unwrap_or_default();
        let _ = socket.send_to(&ack, peer_addr).await;

        if let std::collections::hash_map::Entry::Vacant(e) = peers.entry(peer_addr) {
            let stop = Arc::new(AtomicBool::new(false));
            let receives = hs.direction != Direction::Rx;
            let sends = hs.direction != Direction::Tx;
            e.insert(UdpPeer {
                session_id: hs.session_id,
                stream_index: hs.stream_index,
                last_seen: Instant::now(),
                last_report: Instant::now(),
                report_every: hs.report_every(),
                recv: udp::RecvStats::default(),
                start: Instant::now(),
                rx_open: receives,
                tx_open: sends,
                stop: stop.clone(),
            });
            let _ = tx.send(NetEvent::ServerStreamConnected {
                session_id: hs.session_id,
                stream_index: hs.stream_index,
                stream_count: hs.stream_count,
                addr: peer_addr.to_string(),
                stop: stop.clone(),
                duration_secs: hs.duration_secs,
            });
            if sends {
                tokio::spawn(udp_sender(
                    socket.clone(),
                    peer_addr,
                    hs,
                    stop,
                    tx.clone(),
                    sender_done_tx.clone(),
                ));
            }
        }
        return;
    }

    if data.len() < udp::UDP_HEADER {
        return; // too short to be a data/FIN packet — drop
    }
    let Some(peer) = peers.get_mut(&peer_addr) else {
        return; // data before/without a handshake — drop
    };
    if !peer.rx_open || peer.stop.load(Ordering::Relaxed) {
        return; // killed or already finished — stop counting; formally finalized on the next sweep
    }

    if udp::is_fin(data) {
        close_rx(peer_addr, peers, tx);
        return;
    }
    peer.last_seen = Instant::now();
    peer.recv.on_packet(data);
}

/// Server-to-client half of an Rx/Bidir UDP stream.
async fn udp_sender(
    socket: Arc<UdpSocket>,
    peer_addr: SocketAddr,
    hs: Handshake,
    stop: Arc<AtomicBool>,
    tx: UnboundedSender<NetEvent>,
    done: tokio::sync::mpsc::UnboundedSender<SocketAddr>,
) {
    let (session_id, stream_index) = (hs.session_id, hs.stream_index);
    let start = Instant::now();
    let cfg = udp::TxConfig {
        packet_size: udp::packet_size_for_mtu(hs.mtu.unwrap_or(1500)),
        duration: Duration::from_secs(hs.duration_secs.max(1)),
        bandwidth_bps: hs.bandwidth_bps,
        report_every: hs.report_every(),
    };
    let (total, _) = udp::tx_loop(cfg, start, &stop, &socket, Some(peer_addr), |report| {
        send_interval(&tx, session_id, stream_index, Flow::Tx, report)
    })
    .await;
    let avg_gbps = (total as f64 * 8.0) / start.elapsed().as_secs_f64().max(0.001) / 1e9;
    send_finished(&tx, session_id, stream_index, Flow::Tx, avg_gbps);
    let _ = done.send(peer_addr);
}

/// Ends the client-to-server flow: reports it finished, then drops the
/// peer entirely if the server-to-client flow is over too.
fn close_rx(
    peer_addr: SocketAddr,
    peers: &mut HashMap<SocketAddr, UdpPeer>,
    tx: &UnboundedSender<NetEvent>,
) {
    if let Some(peer) = peers.get_mut(&peer_addr)
        && peer.rx_open
    {
        peer.rx_open = false;
        // Flush the leftover bytes as one last, shorter sample.
        if peer.recv.bytes_interval > 0
            && peer.last_report.elapsed() >= duplex::MIN_PARTIAL_INTERVAL
        {
            let elapsed_secs = peer.start.elapsed().as_secs_f64().round() as u64;
            send_peer_interval(tx, peer, elapsed_secs);
        }
        let elapsed = peer.start.elapsed().as_secs_f64().max(0.001);
        let avg_gbps = (peer.recv.bytes_total as f64 * 8.0) / elapsed / 1e9;
        send_finished(tx, peer.session_id, peer.stream_index, Flow::Rx, avg_gbps);
    }
    remove_if_closed(peer_addr, peers, tx);
}

fn remove_if_closed(
    peer_addr: SocketAddr,
    peers: &mut HashMap<SocketAddr, UdpPeer>,
    tx: &UnboundedSender<NetEvent>,
) {
    if peers.get(&peer_addr).is_some_and(|p| !p.rx_open && !p.tx_open)
        && let Some(peer) = peers.remove(&peer_addr)
    {
        let _ = tx.send(NetEvent::ServerStreamDisconnected {
            session_id: peer.session_id,
            stream_index: peer.stream_index,
        });
    }
}

/// Reports the client-to-server flow's rate/loss/jitter since the peer's
/// previous report, then starts the next interval.
fn send_peer_interval(tx: &UnboundedSender<NetEvent>, peer: &mut UdpPeer, elapsed_secs: u64) {
    let secs = peer.last_report.elapsed().as_secs_f64().max(0.001);
    let gbps = (peer.recv.bytes_interval as f64 * 8.0) / secs / 1e9;
    let _ = tx.send(NetEvent::ServerStreamInterval {
        session_id: peer.session_id,
        stream_index: peer.stream_index,
        flow: Flow::Rx,
        stats: StreamStats {
            gbps,
            elapsed_secs,
            loss_pct: Some(peer.recv.loss_pct()),
            jitter_ms: Some(peer.recv.jitter_ms()),
            retransmits: None,
            cwnd_bytes: None,
        },
    });
    peer.recv.bytes_interval = 0;
    peer.last_report = Instant::now();
}

fn report_and_sweep(peers: &mut HashMap<SocketAddr, UdpPeer>, tx: &UnboundedSender<NetEvent>) {
    let now = Instant::now();
    let mut finished = Vec::new();

    for (addr, peer) in peers.iter_mut() {
        if !peer.rx_open {
            continue; // the sender task reports its own intervals
        }
        if peer.stop.load(Ordering::Relaxed)
            || now.duration_since(peer.last_seen) > udp::IDLE_TIMEOUT
        {
            finished.push(*addr);
            continue;
        }
        if peer.last_report.elapsed() >= peer.report_every {
            let elapsed_secs = peer.start.elapsed().as_secs();
            send_peer_interval(tx, peer, elapsed_secs);
        }
    }

    for addr in finished {
        close_rx(addr, peers, tx);
    }
}
