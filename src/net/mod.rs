//! The real network engine: TCP or UDP, N parallel streams per test.
//!
//! Runs on its own OS thread hosting a Tokio runtime, kept fully separate
//! from the synchronous Ratatui render loop. Communication is a pair of
//! unbounded channels: `NetCommand`s flow from the UI into the runtime,
//! `NetEvent`s flow measurements/log-worthy happenings back out. Both ends
//! of `tokio::sync::mpsc` are plain, non-blocking to use from sync code
//! (`send`, `try_recv`), so the UI thread never awaits anything.
//!
//! Parallel streams are just N independent connections (TCP) or N
//! independently-bound sockets (UDP) sharing one `session_id`, generated
//! once per test and carried in the handshake so the receiving side can
//! group them back into a single logical session.

mod bandwidth;
mod client;
mod duplex;
mod protocol;
mod quic;
mod server;
mod sockopt;
mod tcpinfo;
mod udp;
mod window;

pub use bandwidth::parse_bps;
pub use protocol::Direction;
pub use sockopt::{CONGESTION_SETTABLE, parse_tos};
pub use window::parse_window_bytes;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proto {
    Tcp,
    Udp,
    Quic,
}

/// Which direction a single reported measurement belongs to — independent
/// of the test's overall `Direction`, since Bidir produces both a Tx and
/// an Rx stream of events concurrently for the same stream index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Flow {
    Tx,
    Rx,
}

impl Direction {
    /// The same test seen from the other end: the client's Tx is the
    /// server's Rx and vice versa; Bidir is Bidir from both sides.
    pub fn mirrored(self) -> Direction {
        match self {
            Direction::Tx => Direction::Rx,
            Direction::Rx => Direction::Tx,
            Direction::Bidir => Direction::Bidir,
        }
    }

    /// How many separate `Flow`s of events a stream in this direction
    /// produces — 2 for Bidir (Tx and Rx each finish independently), 1
    /// otherwise. Used to know when a whole test has actually finished.
    pub fn flow_count(self) -> u8 {
        if self == Direction::Bidir { 2 } else { 1 }
    }
}

#[derive(Debug, Clone)]
pub struct ClientParams {
    pub host: String,
    pub port: u16,
    pub duration_secs: u64,
    pub proto: Proto,
    pub stream_count: u8,
    pub direction: Direction,
    /// Per-stream UDP send-rate target; ignored for TCP, which relies on
    /// congestion control instead. `None` means unthrottled.
    pub bandwidth_bps: Option<u64>,
    /// Applied as SO_SNDBUF/SO_RCVBUF on the TCP socket before connecting.
    /// `None` ("auto") leaves the OS default alone. UDP has no equivalent
    /// of a TCP window, so this is ignored for UDP.
    pub window_bytes: Option<u32>,
    /// Only meaningful for UDP: sizes each datagram's payload
    /// (mtu - 28 bytes for IP+UDP headers). Ignored for TCP, which has no
    /// portable, cross-platform socket option to force a link MTU.
    pub mtu: u32,
    /// TCP only ("Buffer size" in the UI, iperf3's `-l`): the size of each
    /// `write`/`read` call's application-level buffer — distinct from
    /// `window_bytes` (the kernel's SO_SNDBUF/SO_RCVBUF). `None` means use
    /// the engine default (`duplex::CHUNK`). Carried to the server too (via
    /// the handshake) so it applies when the server is the one writing or
    /// reading (Rx/Bidir).
    pub buffer_bytes: Option<u32>,
    /// How often the client reports a live interval, in whole seconds (the
    /// "Interval (s)" setting). Client-side only: the server keeps its own
    /// fixed 1s cadence.
    pub interval_secs: u64,
    /// IP TOS / DSCP byte to mark outgoing packets with (`None` = leave the
    /// OS default). Applied to the client's socket and carried to the
    /// server so it marks its own sends too (Rx/Bidir).
    pub tos: Option<u8>,
    /// TCP congestion-control algorithm name (`None` = OS default). Linux
    /// only — see `sockopt::CONGESTION_SETTABLE`.
    pub congestion: Option<String>,
    pub session_id: u64,
}

#[derive(Debug, Clone)]
pub struct ServerParams {
    pub bind: String,
    pub port: u16,
    pub proto: Proto,
    /// Applied to the *listening* TCP socket before `listen()` — on POSIX
    /// systems accepted connections inherit SO_SNDBUF/SO_RCVBUF from it.
    /// `None` leaves the OS default alone.
    pub window_bytes: Option<u32>,
}

pub enum NetCommand {
    StartClient(ClientParams, Arc<AtomicBool>),
    StartServer(ServerParams),
    StopServer,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct StreamStats {
    pub gbps: f64,
    pub elapsed_secs: u64,
    /// For UDP, `Some` only on the receiving side — the sender has no way
    /// to know what was lost. For TCP, a retransmission-rate estimate
    /// (retransmitted segments / segments sent) read live from the kernel —
    /// TCP exposes no direct loss counter, so this is the closest proxy.
    pub loss_pct: Option<f64>,
    pub jitter_ms: Option<f64>,
    /// TCP only — cumulative retransmit count read live from the kernel via
    /// `tcpinfo::query`. `None` for UDP (the concept doesn't apply) or on
    /// platforms without a known accessor.
    pub retransmits: Option<u64>,
    /// TCP only — current congestion window in bytes, read live from the
    /// kernel via `tcpinfo::query`. Same `None` cases as `retransmits`.
    pub cwnd_bytes: Option<u32>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // some fields (raw byte totals) are reserved for history/export
pub enum NetEvent {
    ClientStreamConnected {
        stream_index: u8,
        rtt_ms: f64,
    },
    ClientStreamInterval {
        stream_index: u8,
        flow: Flow,
        interval_bytes: u64,
        stats: StreamStats,
    },
    ClientStreamFinished {
        stream_index: u8,
        flow: Flow,
        total_bytes: u64,
        elapsed_secs: u64,
        avg_gbps: f64,
    },
    ClientError {
        stream_index: Option<u8>,
        message: String,
    },

    ServerListening {
        bind: String,
    },
    ServerListenError(String),
    ServerStreamConnected {
        session_id: u64,
        stream_index: u8,
        stream_count: u8,
        addr: String,
        stop: Arc<AtomicBool>,
        duration_secs: u64,
    },
    ServerStreamInterval {
        session_id: u64,
        stream_index: u8,
        flow: Flow,
        stats: StreamStats,
    },
    ServerStreamFinished {
        session_id: u64,
        stream_index: u8,
        flow: Flow,
        avg_gbps: f64,
    },
    ServerStreamDisconnected {
        session_id: u64,
        stream_index: u8,
    },
}

/// Spawns the background network thread and returns the two ends the UI
/// thread keeps: a command sender and an event receiver.
pub fn spawn_runtime() -> (UnboundedSender<NetCommand>, UnboundedReceiver<NetEvent>) {
    let (cmd_tx, mut cmd_rx) = unbounded_channel::<NetCommand>();
    let (evt_tx, evt_rx) = unbounded_channel::<NetEvent>();

    std::thread::spawn(move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                let _ = evt_tx.send(NetEvent::ServerListenError(format!(
                    "network runtime failed to start: {e}"
                )));
                return;
            }
        };
        rt.block_on(async move {
            let mut server_handle: Option<tokio::task::JoinHandle<()>> = None;
            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    NetCommand::StartClient(params, stop) => {
                        for stream_index in 0..params.stream_count {
                            tokio::spawn(client::run_stream(
                                params.clone(),
                                stream_index,
                                stop.clone(),
                                evt_tx.clone(),
                            ));
                        }
                    }
                    NetCommand::StartServer(params) => {
                        if let Some(h) = server_handle.take() {
                            h.abort();
                        }
                        let evt_tx = evt_tx.clone();
                        server_handle = Some(match params.proto {
                            Proto::Tcp => tokio::spawn(server::run_tcp(params, evt_tx)),
                            Proto::Udp => tokio::spawn(server::run_udp(params, evt_tx)),
                            Proto::Quic => tokio::spawn(quic::run_server(params, evt_tx)),
                        });
                    }
                    NetCommand::StopServer => {
                        if let Some(h) = server_handle.take() {
                            h.abort();
                        }
                    }
                }
            }
        });
    });

    (cmd_tx, evt_rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};
    use tokio::sync::mpsc::UnboundedReceiver;

    /// Binds a real server on an OS-assigned port (127.0.0.1:0) and returns
    /// its event receiver plus the port actually bound, so tests never
    /// fight over a fixed port.
    async fn spawn_server(proto: Proto) -> (UnboundedReceiver<NetEvent>, u16) {
        let (tx, mut rx) = unbounded_channel::<NetEvent>();
        let params = ServerParams { bind: "127.0.0.1".into(), port: 0, proto, window_bytes: None };
        match proto {
            Proto::Tcp => {
                tokio::spawn(server::run_tcp(params, tx));
            }
            Proto::Udp => {
                tokio::spawn(server::run_udp(params, tx));
            }
            Proto::Quic => {
                tokio::spawn(quic::run_server(params, tx));
            }
        }
        let bind = match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
            Ok(Some(NetEvent::ServerListening { bind })) => bind,
            other => panic!("expected ServerListening, got {other:?}"),
        };
        let port: u16 = bind.rsplit(':').next().unwrap().parse().unwrap();
        (rx, port)
    }

    async fn recv(rx: &mut UnboundedReceiver<NetEvent>) -> NetEvent {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("timed out waiting for a NetEvent")
            .expect("event channel closed unexpectedly")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tcp_single_stream_transfers_real_data() {
        let (mut server_rx, port) = spawn_server(Proto::Tcp).await;

        let (client_tx, mut client_rx) = unbounded_channel::<NetEvent>();
        let stop = Arc::new(AtomicBool::new(false));
        let params = ClientParams {
            host: "127.0.0.1".into(),
            port,
            duration_secs: 1,
            proto: Proto::Tcp,
            stream_count: 1,
            direction: Direction::Tx,
            bandwidth_bps: None,
            window_bytes: None,
            mtu: 1500,
            buffer_bytes: None,
            interval_secs: 1,
            tos: None,
            congestion: None,
            session_id: 1,
        };
        tokio::spawn(client::run_stream(params, 0, stop, client_tx));

        assert!(matches!(
            recv(&mut client_rx).await,
            NetEvent::ClientStreamConnected { stream_index: 0, .. }
        ));

        // First event after connecting is either an interval (data flowed)
        // or straight to finished for a very short run — both prove bytes
        // actually moved once we see `total_bytes > 0` on Finished.
        loop {
            match recv(&mut client_rx).await {
                NetEvent::ClientStreamInterval { stream_index: 0, .. } => continue,
                NetEvent::ClientStreamFinished { stream_index: 0, total_bytes, .. } => {
                    assert!(total_bytes > 0, "expected the client to have sent some bytes");
                    break;
                }
                other => panic!("unexpected client event: {other:?}"),
            }
        }

        // The server side should independently agree data arrived.
        let mut server_confirmed = false;
        loop {
            match recv(&mut server_rx).await {
                NetEvent::ServerStreamConnected { session_id: 1, stream_index: 0, .. } => {}
                NetEvent::ServerStreamInterval { .. } => server_confirmed = true,
                NetEvent::ServerStreamFinished { session_id: 1, stream_index: 0, .. } => {
                    server_confirmed = true
                }
                NetEvent::ServerStreamDisconnected { session_id: 1, stream_index: 0 } => break,
                other => panic!("unexpected server event: {other:?}"),
            }
        }
        assert!(server_confirmed, "server never reported any received data");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tcp_custom_window_size_still_transfers_data() {
        // Regression guard for the window/MTU wiring: a non-default
        // window_bytes on both the listening socket (server) and the
        // connecting socket (client) must still produce a working transfer,
        // not just compile. We don't assert the exact OS-level SO_SNDBUF
        // (the kernel is free to double/clamp it), only that the socket
        // option path doesn't break the connection.
        let (tx, mut rx) = unbounded_channel::<NetEvent>();
        let server_params = ServerParams {
            bind: "127.0.0.1".into(),
            port: 0,
            proto: Proto::Tcp,
            window_bytes: Some(16 * 1024),
        };
        tokio::spawn(server::run_tcp(server_params, tx));
        let bind = match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
            Ok(Some(NetEvent::ServerListening { bind })) => bind,
            other => panic!("expected ServerListening, got {other:?}"),
        };
        let port: u16 = bind.rsplit(':').next().unwrap().parse().unwrap();

        let (client_tx, mut client_rx) = unbounded_channel::<NetEvent>();
        let stop = Arc::new(AtomicBool::new(false));
        let params = ClientParams {
            host: "127.0.0.1".into(),
            port,
            duration_secs: 1,
            proto: Proto::Tcp,
            stream_count: 1,
            direction: Direction::Tx,
            bandwidth_bps: None,
            window_bytes: Some(16 * 1024),
            mtu: 1500,
            buffer_bytes: None,
            interval_secs: 1,
            tos: None,
            congestion: None,
            session_id: 1,
        };
        tokio::spawn(client::run_stream(params, 0, stop, client_tx));

        assert!(matches!(
            recv(&mut client_rx).await,
            NetEvent::ClientStreamConnected { stream_index: 0, .. }
        ));
        loop {
            match recv(&mut client_rx).await {
                NetEvent::ClientStreamInterval { stream_index: 0, .. } => continue,
                NetEvent::ClientStreamFinished { stream_index: 0, total_bytes, .. } => {
                    assert!(
                        total_bytes > 0,
                        "expected the client to have sent some bytes even with a small window"
                    );
                    break;
                }
                other => panic!("unexpected client event: {other:?}"),
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tcp_custom_buffer_size_changes_the_write_chunk_size() {
        // Regression guard for the "Buffer size" wiring: `write_loop` writes
        // exactly `buffer_bytes` per `write_all` call and only counts whole
        // chunks, so on the TX (writing) side `total_bytes` must be an
        // exact multiple of it — proving the value actually changed write
        // granularity, not just that it was accepted and ignored.
        let (mut server_rx, port) = spawn_server(Proto::Tcp).await;

        let (client_tx, mut client_rx) = unbounded_channel::<NetEvent>();
        let stop = Arc::new(AtomicBool::new(false));
        let buffer_bytes = 4096u32;
        let params = ClientParams {
            host: "127.0.0.1".into(),
            port,
            duration_secs: 1,
            proto: Proto::Tcp,
            stream_count: 1,
            direction: Direction::Tx,
            bandwidth_bps: None,
            window_bytes: None,
            mtu: 1500,
            interval_secs: 1,
            tos: None,
            congestion: None,
            buffer_bytes: Some(buffer_bytes),
            session_id: 1,
        };
        tokio::spawn(client::run_stream(params, 0, stop, client_tx));

        assert!(matches!(
            recv(&mut client_rx).await,
            NetEvent::ClientStreamConnected { stream_index: 0, .. }
        ));
        let total_bytes = loop {
            match recv(&mut client_rx).await {
                NetEvent::ClientStreamInterval { stream_index: 0, .. } => continue,
                NetEvent::ClientStreamFinished { stream_index: 0, total_bytes, .. } => {
                    break total_bytes;
                }
                other => panic!("unexpected client event: {other:?}"),
            }
        };
        assert!(total_bytes > 0, "expected some bytes sent");
        assert_eq!(
            total_bytes % buffer_bytes as u64,
            0,
            "total bytes sent should be an exact multiple of the configured buffer size"
        );

        // Drain the server side so the connection doesn't dangle past the test.
        loop {
            match recv(&mut server_rx).await {
                NetEvent::ServerStreamConnected { .. }
                | NetEvent::ServerStreamInterval { .. }
                | NetEvent::ServerStreamFinished { .. } => {}
                NetEvent::ServerStreamDisconnected { .. } => break,
                other => panic!("unexpected server event: {other:?}"),
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tcp_custom_buffer_size_applies_when_the_server_is_the_writer() {
        // The client's chosen buffer size is carried to the server via the
        // handshake (see `Handshake::chunk_bytes`) so it also applies when
        // the server is the one writing (Rx/Bidir) — this just proves that
        // path doesn't break the transfer, since the server's own exact
        // byte count isn't exposed on `ServerStreamFinished` to assert on
        // directly the way the TX-side test above does.
        let (mut server_rx, port) = spawn_server(Proto::Tcp).await;

        let (client_tx, mut client_rx) = unbounded_channel::<NetEvent>();
        let stop = Arc::new(AtomicBool::new(false));
        let params = ClientParams {
            host: "127.0.0.1".into(),
            port,
            duration_secs: 1,
            proto: Proto::Tcp,
            stream_count: 1,
            direction: Direction::Rx,
            bandwidth_bps: None,
            window_bytes: None,
            mtu: 1500,
            interval_secs: 1,
            tos: None,
            congestion: None,
            buffer_bytes: Some(4096),
            session_id: 1,
        };
        tokio::spawn(client::run_stream(params, 0, stop, client_tx));

        assert!(matches!(
            recv(&mut client_rx).await,
            NetEvent::ClientStreamConnected { stream_index: 0, .. }
        ));
        loop {
            match recv(&mut client_rx).await {
                NetEvent::ClientStreamInterval { stream_index: 0, .. } => continue,
                NetEvent::ClientStreamFinished { stream_index: 0, total_bytes, .. } => {
                    assert!(
                        total_bytes > 0,
                        "expected the client to have received some bytes from the server"
                    );
                    break;
                }
                other => panic!("unexpected client event: {other:?}"),
            }
        }

        loop {
            match recv(&mut server_rx).await {
                NetEvent::ServerStreamConnected { .. }
                | NetEvent::ServerStreamInterval { .. }
                | NetEvent::ServerStreamFinished { .. } => {}
                NetEvent::ServerStreamDisconnected { .. } => break,
                other => panic!("unexpected server event: {other:?}"),
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn udp_custom_mtu_changes_packet_size() {
        // Regression guard: a smaller MTU must shrink the actual UDP
        // payload size, verified by the loss-free total_bytes / seq count
        // matching udp_packet_size(mtu) rather than the old hardcoded 1200.
        let (mut server_rx, port) = spawn_server(Proto::Udp).await;

        let (client_tx, mut client_rx) = unbounded_channel::<NetEvent>();
        let stop = Arc::new(AtomicBool::new(false));
        let params = ClientParams {
            host: "127.0.0.1".into(),
            port,
            duration_secs: 1,
            proto: Proto::Udp,
            stream_count: 1,
            direction: Direction::Tx,
            bandwidth_bps: None,
            window_bytes: None,
            mtu: 576, // small MTU -> small packets, well under the old hardcoded 1200
            buffer_bytes: None,
            interval_secs: 1,
            tos: None,
            congestion: None,
            session_id: 1,
        };
        tokio::spawn(client::run_stream(params, 0, stop, client_tx));

        assert!(matches!(
            recv(&mut client_rx).await,
            NetEvent::ClientStreamConnected { stream_index: 0, .. }
        ));
        let total_bytes = loop {
            match recv(&mut client_rx).await {
                NetEvent::ClientStreamInterval { stream_index: 0, .. } => continue,
                NetEvent::ClientStreamFinished { stream_index: 0, total_bytes: t, .. } => break t,
                other => panic!("unexpected client event: {other:?}"),
            }
        };
        assert!(total_bytes > 0, "expected some bytes sent");
        let expected_packet_size = client::udp_packet_size(576) as u64;
        assert!(
            expected_packet_size < 1200,
            "sanity check: a 576 MTU should produce packets smaller than the old hardcoded 1200"
        );
        assert_eq!(
            total_bytes % expected_packet_size,
            0,
            "on loopback (no loss expected for a 1s burst) total_bytes should be an exact multiple of the MTU-derived packet size"
        );

        // Drain server events until we see the session finish.
        loop {
            match recv(&mut server_rx).await {
                NetEvent::ServerStreamConnected { .. }
                | NetEvent::ServerStreamInterval { .. }
                | NetEvent::ServerStreamFinished { .. } => {}
                NetEvent::ServerStreamDisconnected { .. } => break,
                other => panic!("unexpected server event: {other:?}"),
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tcp_parallel_streams_share_one_session() {
        let (mut server_rx, port) = spawn_server(Proto::Tcp).await;

        let stop = Arc::new(AtomicBool::new(false));
        let params = ClientParams {
            host: "127.0.0.1".into(),
            port,
            duration_secs: 1,
            proto: Proto::Tcp,
            stream_count: 3,
            direction: Direction::Tx,
            bandwidth_bps: None,
            window_bytes: None,
            mtu: 1500,
            buffer_bytes: None,
            interval_secs: 1,
            tos: None,
            congestion: None,
            session_id: 77,
        };
        let (client_tx, mut client_rx) = unbounded_channel::<NetEvent>();
        for i in 0..params.stream_count {
            tokio::spawn(client::run_stream(params.clone(), i, stop.clone(), client_tx.clone()));
        }
        drop(client_tx);

        let mut connected: std::collections::HashSet<u8> = std::collections::HashSet::new();
        while connected.len() < 3 {
            if let NetEvent::ClientStreamConnected { stream_index, .. } = recv(&mut client_rx).await
            {
                connected.insert(stream_index);
            }
        }
        assert_eq!(connected, std::collections::HashSet::from([0, 1, 2]));

        // The server must see all three streams land under the same
        // session_id — that correlation is the whole point of parallel
        // streams being useful to the UI.
        let mut server_streams: std::collections::HashSet<u8> = std::collections::HashSet::new();
        while server_streams.len() < 3 {
            if let NetEvent::ServerStreamConnected {
                session_id, stream_index, stream_count, ..
            } = recv(&mut server_rx).await
            {
                assert_eq!(session_id, 77);
                assert_eq!(stream_count, 3);
                server_streams.insert(stream_index);
            }
        }
        assert_eq!(server_streams, std::collections::HashSet::from([0, 1, 2]));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tcp_interval_reports_live_kernel_tcp_stats() {
        // Regression guard for the retransmit/cwnd/loss wiring: TCP
        // intervals must carry real, kernel-sourced values (Some(_) on the
        // platforms we support them on) instead of the old static `0`/`0.0`
        // placeholders. We don't assert retransmits/loss are *nonzero* — a
        // clean loopback run legitimately has zero of both, that's the
        // whole point — but cwnd should be a real, positive window size.
        let (mut server_rx, port) = spawn_server(Proto::Tcp).await;

        let (client_tx, mut client_rx) = unbounded_channel::<NetEvent>();
        let stop = Arc::new(AtomicBool::new(false));
        let params = ClientParams {
            host: "127.0.0.1".into(),
            port,
            duration_secs: 2, // long enough to guarantee at least one 1s report tick
            proto: Proto::Tcp,
            stream_count: 1,
            direction: Direction::Tx,
            bandwidth_bps: None,
            window_bytes: None,
            mtu: 1500,
            buffer_bytes: None,
            interval_secs: 1,
            tos: None,
            congestion: None,
            session_id: 9,
        };
        tokio::spawn(client::run_stream(params, 0, stop, client_tx));

        assert!(matches!(
            recv(&mut client_rx).await,
            NetEvent::ClientStreamConnected { stream_index: 0, .. }
        ));
        match recv(&mut client_rx).await {
            NetEvent::ClientStreamInterval { stats, .. } => {
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                {
                    assert!(
                        stats.retransmits.is_some(),
                        "TCP interval on a supported platform must report a retransmit count"
                    );
                    assert!(
                        stats.cwnd_bytes.is_some(),
                        "TCP interval on a supported platform must report a congestion window"
                    );
                    assert!(
                        stats.cwnd_bytes.unwrap() > 0,
                        "a connection that's actively sending data must have a nonzero cwnd"
                    );
                    // loss_pct is only `Some` once segments_out > 0 — by the
                    // time we've had a full second of data flow (this is a
                    // 2s test), that's guaranteed.
                    let loss = stats
                        .loss_pct
                        .expect("segments must have gone out by the first interval of a 2s test");
                    assert!((0.0..=100.0).contains(&loss), "loss_pct out of range: {loss}");
                }
            }
            other => panic!("expected an interval right after connecting on a 2s test: {other:?}"),
        }

        // Drain the server side too so the connection doesn't dangle past
        // the test — same pattern used elsewhere in this module.
        loop {
            match recv(&mut server_rx).await {
                NetEvent::ServerStreamConnected { .. }
                | NetEvent::ServerStreamInterval { .. }
                | NetEvent::ServerStreamFinished { .. } => {}
                NetEvent::ServerStreamDisconnected { .. } => break,
                other => panic!("unexpected server event: {other:?}"),
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn udp_stream_reports_loss_and_jitter() {
        let (mut server_rx, port) = spawn_server(Proto::Udp).await;

        let (client_tx, mut client_rx) = unbounded_channel::<NetEvent>();
        let stop = Arc::new(AtomicBool::new(false));
        let params = ClientParams {
            host: "127.0.0.1".into(),
            port,
            duration_secs: 2, // long enough to guarantee at least one 1s report tick
            proto: Proto::Udp,
            stream_count: 1,
            direction: Direction::Tx,
            bandwidth_bps: Some(5_000_000), // paced, so loopback loss should be ~0
            window_bytes: None,
            mtu: 1500,
            buffer_bytes: None,
            interval_secs: 1,
            tos: None,
            congestion: None,
            session_id: 5,
        };
        tokio::spawn(client::run_stream(params, 0, stop, client_tx));

        assert!(matches!(
            recv(&mut client_rx).await,
            NetEvent::ClientStreamConnected { stream_index: 0, .. }
        ));

        // UDP is the one path where the server measures loss/jitter itself
        // (the sender has no way to know) — assert those fields are `Some`
        // and, on a clean loopback with pacing, essentially lossless.
        loop {
            match recv(&mut server_rx).await {
                NetEvent::ServerStreamConnected { .. } => {}
                NetEvent::ServerStreamInterval { stats, .. } => {
                    assert!(stats.loss_pct.is_some(), "UDP interval must report a loss percentage");
                    assert!(stats.jitter_ms.is_some(), "UDP interval must report a jitter value");
                    assert!(
                        stats.loss_pct.unwrap() < 5.0,
                        "unexpectedly high loss on loopback: {:?}",
                        stats.loss_pct
                    );
                    break;
                }
                other => panic!("unexpected server event before an interval: {other:?}"),
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tcp_bidir_transfers_data_both_ways() {
        let (mut server_rx, port) = spawn_server(Proto::Tcp).await;

        let (client_tx, mut client_rx) = unbounded_channel::<NetEvent>();
        let stop = Arc::new(AtomicBool::new(false));
        let params = ClientParams {
            host: "127.0.0.1".into(),
            port,
            duration_secs: 1,
            proto: Proto::Tcp,
            stream_count: 1,
            direction: Direction::Bidir,
            bandwidth_bps: None,
            window_bytes: None,
            mtu: 1500,
            buffer_bytes: None,
            interval_secs: 1,
            tos: None,
            congestion: None,
            session_id: 42,
        };
        tokio::spawn(client::run_stream(params, 0, stop, client_tx));

        assert!(matches!(
            recv(&mut client_rx).await,
            NetEvent::ClientStreamConnected { stream_index: 0, .. }
        ));

        // Bidir means two independent flows finish for this one stream —
        // both must actually report bytes, not just one direction.
        let mut client_flows_finished: std::collections::HashSet<Flow> =
            std::collections::HashSet::new();
        while client_flows_finished.len() < 2 {
            if let NetEvent::ClientStreamFinished { flow, total_bytes, .. } =
                recv(&mut client_rx).await
            {
                assert!(total_bytes > 0, "flow {flow:?} reported zero bytes");
                client_flows_finished.insert(flow);
            }
        }
        assert_eq!(client_flows_finished, std::collections::HashSet::from([Flow::Tx, Flow::Rx]));

        // The server must independently agree: it saw both a Tx and an Rx
        // flow for the same stream, and eventually disconnects once both
        // are done (not after just the first one).
        let mut server_flows_seen: std::collections::HashSet<Flow> =
            std::collections::HashSet::new();
        loop {
            match recv(&mut server_rx).await {
                NetEvent::ServerStreamConnected { .. } => {}
                NetEvent::ServerStreamInterval { flow, .. }
                | NetEvent::ServerStreamFinished { flow, .. } => {
                    server_flows_seen.insert(flow);
                }
                NetEvent::ServerStreamDisconnected { .. } => break,
                other => panic!("unexpected server event: {other:?}"),
            }
        }
        assert_eq!(server_flows_seen, std::collections::HashSet::from([Flow::Tx, Flow::Rx]));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tcp_reverse_direction_server_sends_client_receives() {
        let (mut server_rx, port) = spawn_server(Proto::Tcp).await;

        let (client_tx, mut client_rx) = unbounded_channel::<NetEvent>();
        let stop = Arc::new(AtomicBool::new(false));
        let params = ClientParams {
            host: "127.0.0.1".into(),
            port,
            duration_secs: 1,
            proto: Proto::Tcp,
            stream_count: 1,
            direction: Direction::Rx,
            bandwidth_bps: None,
            window_bytes: None,
            mtu: 1500,
            buffer_bytes: None,
            interval_secs: 1,
            tos: None,
            congestion: None,
            session_id: 43,
        };
        tokio::spawn(client::run_stream(params, 0, stop, client_tx));

        assert!(matches!(recv(&mut client_rx).await, NetEvent::ClientStreamConnected { .. }));
        loop {
            match recv(&mut client_rx).await {
                NetEvent::ClientStreamInterval { flow: Flow::Rx, .. } => continue,
                NetEvent::ClientStreamFinished { flow: Flow::Rx, total_bytes, .. } => {
                    assert!(total_bytes > 0, "client should have received bytes the server sent");
                    break;
                }
                other => panic!("expected Rx interval/finish events on the client, got {other:?}"),
            }
        }

        loop {
            match recv(&mut server_rx).await {
                NetEvent::ServerStreamConnected { .. }
                | NetEvent::ServerStreamInterval { flow: Flow::Tx, .. } => continue,
                NetEvent::ServerStreamFinished { flow: Flow::Tx, .. } => break,
                other => panic!(
                    "expected Tx interval/finish events on the server (it was the sender), got {other:?}"
                ),
            }
        }
    }

    async fn run_udp_client(
        direction: Direction,
        port: u16,
        session_id: u64,
    ) -> (Vec<NetEvent>, UnboundedReceiver<NetEvent>) {
        let (client_tx, mut client_rx) = unbounded_channel::<NetEvent>();
        let stop = Arc::new(AtomicBool::new(false));
        let params = ClientParams {
            proto: Proto::Udp,
            direction,
            bandwidth_bps: Some(50_000_000),
            session_id,
            ..tcp_params(port, 2)
        };
        tokio::spawn(client::run_stream(params, 0, stop, client_tx));
        let flows = direction.flow_count() as usize;
        let mut events = Vec::new();
        let mut finished = 0;
        while finished < flows {
            let evt = recv(&mut client_rx).await;
            if let NetEvent::ClientError { message, .. } = &evt {
                panic!("UDP {direction:?} failed: {message}");
            }
            if matches!(evt, NetEvent::ClientStreamFinished { .. }) {
                finished += 1;
            }
            events.push(evt);
        }
        (events, client_rx)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn udp_reverse_server_sends_and_client_measures_loss_and_jitter() {
        let (mut server_rx, port) = spawn_server(Proto::Udp).await;
        let (events, _) = run_udp_client(Direction::Rx, port, 50).await;

        let total = events.iter().find_map(|e| match e {
            NetEvent::ClientStreamFinished { flow: Flow::Rx, total_bytes, .. } => {
                Some(*total_bytes)
            }
            _ => None,
        });
        assert!(
            total.unwrap_or(0) > 0,
            "client must have received UDP data from the server: {events:?}"
        );
        assert!(
            !events.iter().any(|e| matches!(
                e,
                NetEvent::ClientStreamInterval { flow: Flow::Tx, .. }
                    | NetEvent::ClientStreamFinished { flow: Flow::Tx, .. }
            )),
            "a reverse test must not send from the client"
        );
        let measured = events.iter().any(|e| {
            matches!(e, NetEvent::ClientStreamInterval { flow: Flow::Rx, stats, .. } if stats.loss_pct.is_some() && stats.jitter_ms.is_some())
        });
        assert!(measured, "client-side Rx intervals must carry loss and jitter: {events:?}");

        // The server saw itself as the sender and cleaned the peer up.
        let mut saw_tx_finish = false;
        loop {
            match recv(&mut server_rx).await {
                NetEvent::ServerStreamFinished { flow: Flow::Tx, .. } => saw_tx_finish = true,
                NetEvent::ServerStreamDisconnected { .. } => break,
                _ => {}
            }
        }
        assert!(saw_tx_finish);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn udp_bidir_moves_data_both_ways_and_both_sides_finish() {
        let (mut server_rx, port) = spawn_server(Proto::Udp).await;
        let (events, _) = run_udp_client(Direction::Bidir, port, 51).await;

        for flow in [Flow::Tx, Flow::Rx] {
            let total = events.iter().find_map(|e| match e {
                NetEvent::ClientStreamFinished { flow: f, total_bytes, .. } if *f == flow => {
                    Some(*total_bytes)
                }
                _ => None,
            });
            assert!(
                total.unwrap_or(0) > 0,
                "{flow:?} must have moved data in a bidir UDP test: {events:?}"
            );
        }

        let (mut rx_finish, mut tx_finish) = (false, false);
        loop {
            match recv(&mut server_rx).await {
                NetEvent::ServerStreamFinished { flow: Flow::Rx, avg_gbps, .. } => {
                    assert!(avg_gbps > 0.0);
                    rx_finish = true;
                }
                NetEvent::ServerStreamFinished { flow: Flow::Tx, .. } => tx_finish = true,
                NetEvent::ServerStreamDisconnected { .. } => break,
                _ => {}
            }
        }
        assert!(rx_finish && tx_finish, "server must finish both flows before the peer is dropped");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn udp_kill_flag_disconnects_the_peer_promptly() {
        // Regression test for the "Kill Client" action doing nothing for
        // UDP peers — the stop `Arc` handed out on `ServerStreamConnected`
        // must actually be checked by the receive loop.
        let (mut server_rx, port) = spawn_server(Proto::Udp).await;

        let (client_tx, mut client_rx) = unbounded_channel::<NetEvent>();
        let client_stop = Arc::new(AtomicBool::new(false));
        let params = ClientParams {
            host: "127.0.0.1".into(),
            port,
            duration_secs: 10, // long enough that natural completion can't be mistaken for a kill
            proto: Proto::Udp,
            stream_count: 1,
            direction: Direction::Tx,
            bandwidth_bps: Some(2_000_000),
            window_bytes: None,
            mtu: 1500,
            buffer_bytes: None,
            interval_secs: 1,
            tos: None,
            congestion: None,
            session_id: 9,
        };
        tokio::spawn(client::run_stream(params, 0, client_stop, client_tx));
        assert!(matches!(recv(&mut client_rx).await, NetEvent::ClientStreamConnected { .. }));

        let kill = match recv(&mut server_rx).await {
            NetEvent::ServerStreamConnected { stop, .. } => stop,
            other => panic!("expected ServerStreamConnected, got {other:?}"),
        };

        let killed_at = Instant::now();
        kill.store(true, Ordering::Relaxed);

        loop {
            match recv(&mut server_rx).await {
                NetEvent::ServerStreamInterval { .. } | NetEvent::ServerStreamFinished { .. } => {
                    continue;
                }
                NetEvent::ServerStreamDisconnected { .. } => break,
                other => {
                    panic!("unexpected event while waiting for the kill to take effect: {other:?}")
                }
            }
        }
        assert!(
            killed_at.elapsed() < Duration::from_secs(2),
            "should disconnect on the next ~1s sweep tick, not wait anywhere near the 10s test duration"
        );
    }

    fn tcp_params(port: u16, duration_secs: u64) -> ClientParams {
        ClientParams {
            host: "127.0.0.1".into(),
            port,
            duration_secs,
            proto: Proto::Tcp,
            stream_count: 1,
            direction: Direction::Tx,
            bandwidth_bps: None,
            window_bytes: None,
            mtu: 1500,
            buffer_bytes: None,
            interval_secs: 1,
            tos: None,
            congestion: None,
            session_id: 1,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_longer_report_interval_spaces_out_the_live_samples() {
        let (_server_rx, port) = spawn_server(Proto::Tcp).await;
        let (client_tx, mut client_rx) = unbounded_channel::<NetEvent>();
        let stop = Arc::new(AtomicBool::new(false));
        let params = ClientParams { interval_secs: 2, ..tcp_params(port, 5) };
        tokio::spawn(client::run_stream(params, 0, stop, client_tx));

        let mut sample_times = Vec::new();
        loop {
            match recv(&mut client_rx).await {
                NetEvent::ClientStreamInterval { stats, .. } => {
                    sample_times.push(stats.elapsed_secs)
                }
                NetEvent::ClientStreamFinished { .. } => break,
                _ => {}
            }
        }
        // 5s test at a 2s interval -> samples at ~2s and ~4s, never at 1s/3s,
        // plus the shorter leftover 4-5s sample flushed when the test ends.
        assert_eq!(sample_times, vec![2, 4, 5]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tcp_and_udp_tests_still_run_with_a_tos_marking_applied() {
        for proto in [Proto::Tcp, Proto::Udp] {
            let (_server_rx, port) = spawn_server(proto).await;
            let (client_tx, mut client_rx) = unbounded_channel::<NetEvent>();
            let stop = Arc::new(AtomicBool::new(false));
            let params = ClientParams { proto, tos: Some(0xB8), ..tcp_params(port, 1) };
            tokio::spawn(client::run_stream(params, 0, stop, client_tx));
            loop {
                match recv(&mut client_rx).await {
                    NetEvent::ClientStreamFinished { total_bytes, .. } => {
                        assert!(total_bytes > 0, "{proto:?} with TOS set must still transfer data");
                        break;
                    }
                    NetEvent::ClientError { message, .. } => {
                        panic!("{proto:?} with TOS set failed: {message}")
                    }
                    _ => {}
                }
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn requesting_a_congestion_algorithm_works_on_linux_and_fails_loudly_elsewhere() {
        let (_server_rx, port) = spawn_server(Proto::Tcp).await;
        let (client_tx, mut client_rx) = unbounded_channel::<NetEvent>();
        let stop = Arc::new(AtomicBool::new(false));
        let params = ClientParams { congestion: Some("reno".into()), ..tcp_params(port, 1) };
        tokio::spawn(client::run_stream(params, 0, stop, client_tx));
        loop {
            match recv(&mut client_rx).await {
                NetEvent::ClientStreamFinished { .. } => {
                    assert!(
                        std::hint::black_box(CONGESTION_SETTABLE),
                        "a non-Linux platform must not silently ignore the requested algorithm"
                    );
                    break;
                }
                NetEvent::ClientError { message, .. } => {
                    assert!(
                        !std::hint::black_box(CONGESTION_SETTABLE),
                        "reno must be accepted on Linux: {message}"
                    );
                    assert!(message.contains("congestion control 'reno'"), "{message}");
                    break;
                }
                _ => {}
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_final_partial_interval_makes_live_samples_add_up_to_the_exact_total() {
        let (_server_rx, port) = spawn_server(Proto::Tcp).await;
        let (client_tx, mut client_rx) = unbounded_channel::<NetEvent>();
        let stop = Arc::new(AtomicBool::new(false));
        // 2.5s of data can't land on a 2s boundary, so there is leftover.
        tokio::spawn(client::run_stream(
            ClientParams { interval_secs: 2, ..tcp_params(port, 3) },
            0,
            stop,
            client_tx,
        ));

        let mut summed = 0u64;
        let total = loop {
            match recv(&mut client_rx).await {
                NetEvent::ClientStreamInterval { interval_bytes, .. } => summed += interval_bytes,
                NetEvent::ClientStreamFinished { total_bytes, .. } => break total_bytes,
                _ => {}
            }
        };
        assert_eq!(
            summed, total,
            "interval samples must account for every byte, including the last partial one"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_server_reports_on_the_clients_interval_for_tcp_and_udp() {
        for proto in [Proto::Tcp, Proto::Udp] {
            let (mut server_rx, port) = spawn_server(proto).await;
            let (client_tx, _client_rx) = unbounded_channel::<NetEvent>();
            let stop = Arc::new(AtomicBool::new(false));
            let params = ClientParams {
                proto,
                interval_secs: 2,
                bandwidth_bps: Some(50_000_000),
                ..tcp_params(port, 5)
            };
            tokio::spawn(client::run_stream(params, 0, stop, client_tx));

            let mut times = Vec::new();
            loop {
                match recv(&mut server_rx).await {
                    NetEvent::ServerStreamInterval { stats, .. } => times.push(stats.elapsed_secs),
                    NetEvent::ServerStreamFinished { .. } => break,
                    _ => {}
                }
            }
            assert!(
                times.contains(&2) && times.contains(&4),
                "{proto:?} server samples at the 2s interval: {times:?}"
            );
            assert!(
                !times.contains(&1) && !times.contains(&3),
                "{proto:?} server must not sample every second: {times:?}"
            );
        }
    }

    /// Runs one QUIC stream to completion and returns its client events.
    async fn run_quic(
        direction: Direction,
        congestion: Option<&str>,
        buffer_bytes: Option<u32>,
    ) -> Vec<NetEvent> {
        let (mut server_rx, port) = spawn_server(Proto::Quic).await;
        let (client_tx, mut client_rx) = unbounded_channel::<NetEvent>();
        let stop = Arc::new(AtomicBool::new(false));
        let params = ClientParams {
            proto: Proto::Quic,
            direction,
            congestion: congestion.map(String::from),
            buffer_bytes,
            window_bytes: Some(1024 * 1024),
            ..tcp_params(port, 2)
        };
        tokio::spawn(client::run_stream(params, 0, stop, client_tx));
        let mut events = Vec::new();
        let mut finished = 0;
        while finished < direction.flow_count() {
            let evt = recv(&mut client_rx).await;
            match &evt {
                NetEvent::ClientError { message, .. } => {
                    panic!("QUIC {direction:?} failed: {message}")
                }
                NetEvent::ClientStreamFinished { .. } => finished += 1,
                _ => {}
            }
            events.push(evt);
        }
        // The server must wind the stream down too, not hang on it.
        loop {
            if let NetEvent::ServerStreamDisconnected { .. } = recv(&mut server_rx).await {
                break;
            }
        }
        events
    }

    fn finished_bytes(events: &[NetEvent], flow: Flow) -> u64 {
        events
            .iter()
            .find_map(|e| match e {
                NetEvent::ClientStreamFinished { flow: f, total_bytes, .. } if *f == flow => {
                    Some(*total_bytes)
                }
                _ => None,
            })
            .unwrap_or(0)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn quic_transfers_data_in_every_direction() {
        let tx = run_quic(Direction::Tx, None, None).await;
        assert!(finished_bytes(&tx, Flow::Tx) > 0, "QUIC Tx moved no data");

        let rx = run_quic(Direction::Rx, None, None).await;
        assert!(finished_bytes(&rx, Flow::Rx) > 0, "QUIC Rx (server sends) moved no data");

        let both = run_quic(Direction::Bidir, None, None).await;
        assert!(
            finished_bytes(&both, Flow::Tx) > 0 && finished_bytes(&both, Flow::Rx) > 0,
            "QUIC Bidir must move data both ways"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn quic_server_finishes_a_non_empty_stream_and_disconnects() {
        // The close must wait for the peer to drain the stream, or the tail
        // of the transfer is lost.
        let (mut server_rx, port) = spawn_server(Proto::Quic).await;
        let (client_tx, mut client_rx) = unbounded_channel::<NetEvent>();
        let stop = Arc::new(AtomicBool::new(false));
        let params = ClientParams { proto: Proto::Quic, ..tcp_params(port, 1) };
        tokio::spawn(client::run_stream(params, 0, stop, client_tx));
        let sent = loop {
            if let NetEvent::ClientStreamFinished { total_bytes, .. } = recv(&mut client_rx).await {
                break total_bytes;
            }
        };
        let mut server_bytes_seen = false;
        loop {
            match recv(&mut server_rx).await {
                NetEvent::ServerStreamFinished { flow: Flow::Rx, avg_gbps, .. } => {
                    server_bytes_seen = avg_gbps > 0.0
                }
                NetEvent::ServerStreamDisconnected { .. } => break,
                _ => {}
            }
        }
        assert!(
            sent > 0 && server_bytes_seen,
            "server must have finished reading a non-empty stream"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn quic_intervals_carry_live_connection_stats() {
        let events = run_quic(Direction::Tx, None, None).await;
        let stats: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                NetEvent::ClientStreamInterval { stats, .. } => Some(*stats),
                _ => None,
            })
            .collect();
        assert!(!stats.is_empty());
        assert!(
            stats.iter().all(|s| s.retransmits.is_some() && s.cwnd_bytes.is_some()),
            "QUIC stats must be filled in: {stats:?}"
        );
        assert!(
            stats.iter().any(|s| s.cwnd_bytes.unwrap_or(0) > 0),
            "the congestion window must be real"
        );
        assert!(
            stats.iter().all(|s| s.loss_pct.is_some()),
            "loss is exact from quinn's sent/lost packet counts"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn quic_congestion_algorithms_and_buffer_size_are_applied_on_any_platform() {
        for name in ["cubic", "bbr", "reno"] {
            let events = run_quic(Direction::Tx, Some(name), Some(16 * 1024)).await;
            assert!(finished_bytes(&events, Flow::Tx) > 0, "QUIC with {name} moved no data");
            let total = finished_bytes(&events, Flow::Tx);
            assert_eq!(
                total % (16 * 1024),
                0,
                "the write chunk size must be the configured buffer size"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn quic_rejects_an_unknown_congestion_algorithm_clearly() {
        let (_server_rx, port) = spawn_server(Proto::Quic).await;
        let (client_tx, mut client_rx) = unbounded_channel::<NetEvent>();
        let stop = Arc::new(AtomicBool::new(false));
        let params = ClientParams {
            proto: Proto::Quic,
            congestion: Some("vegas".into()),
            ..tcp_params(port, 1)
        };
        tokio::spawn(client::run_stream(params, 0, stop, client_tx));
        loop {
            match recv(&mut client_rx).await {
                NetEvent::ClientError { message, .. } => {
                    assert!(message.contains("vegas"), "{message}");
                    break;
                }
                NetEvent::ClientStreamFinished { .. } => {
                    panic!("an unknown algorithm must not run a test")
                }
                _ => {}
            }
        }
    }
}
