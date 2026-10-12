//! QUIC transport (via `quinn`). One QUIC connection per test stream, each
//! carrying a single bidirectional stream that is handled exactly like a
//! TCP connection: the same JSON handshake first, then the shared
//! `duplex` read/write loops — so Tx/Rx/Bidir, buffer size, interval and
//! omit all work unchanged.
//!
//! QUIC is always encrypted (TLS 1.3), but gauge is a measurement tool, not
//! a secure channel: the server uses a throw-away self-signed certificate
//! and the client deliberately does not verify it (there is nothing to
//! pin it to), so a QUIC test measures the transport and does not
//! authenticate the server.

use std::net::SocketAddr;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use quinn::congestion::{BbrConfig, ControllerFactory, CubicConfig, NewRenoConfig};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::DigitallySignedStruct;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use tokio::sync::mpsc::UnboundedSender;

use super::client::{
    err, handshake_for, report_interval, require_server_version, send_finished as client_finished,
    send_interval as client_interval,
};
use super::protocol::{Handshake, HandshakeAck, check_client_version, read_message, write_message};
use super::server::{send_finished as server_finished, send_interval as server_interval};
use super::{ClientParams, NetEvent, ServerParams, duplex, sockopt};

const ALPN: &[u8] = b"gauge";
/// The (unverified) name the client asks for.
const SERVER_NAME: &str = "gauge";
/// How long to wait for the peer to confirm it received our last bytes
/// before closing the connection anyway.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(3);

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// A congestion controller by the same names the TCP setting uses. Unlike
/// TCP, QUIC runs in user space, so this works on every platform.
fn congestion_factory(name: &str) -> Result<Arc<dyn ControllerFactory + Send + Sync>, String> {
    match name {
        "cubic" => Ok(Arc::new(CubicConfig::default())),
        "bbr" => Ok(Arc::new(BbrConfig::default())),
        "reno" => Ok(Arc::new(NewRenoConfig::default())),
        other => {
            Err(format!("unknown congestion algorithm '{other}' (QUIC supports cubic, bbr, reno)"))
        }
    }
}

/// `window_bytes` becomes QUIC's flow-control windows (the closest thing
/// QUIC has to a TCP socket buffer); `None` keeps quinn's defaults.
fn transport_config(
    window_bytes: Option<u32>,
    congestion: Option<&str>,
) -> Result<Arc<quinn::TransportConfig>, String> {
    let mut t = quinn::TransportConfig::default();
    if let Some(w) = window_bytes {
        t.stream_receive_window(quinn::VarInt::from_u32(w));
        t.receive_window(quinn::VarInt::from_u32(w));
        t.send_window(w as u64);
    }
    if let Some(name) = congestion {
        t.congestion_controller_factory(congestion_factory(name)?);
    }
    t.max_idle_timeout(Some(Duration::from_secs(15).try_into().expect("15s fits an idle timeout")));
    Ok(Arc::new(t))
}

fn server_config(window_bytes: Option<u32>) -> Result<quinn::ServerConfig, String> {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec![SERVER_NAME.to_string()])
            .map_err(|e| format!("generating a certificate failed: {e}"))?;
    let key = PrivatePkcs8KeyDer::from(signing_key.serialize_der());
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(vec![cert.der().clone()], key.into())
        .map_err(|e| format!("TLS setup failed: {e}"))?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let quic = QuicServerConfig::try_from(tls).map_err(|e| e.to_string())?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(quic));
    config.transport_config(transport_config(window_bytes, None)?);
    Ok(config)
}

/// Accepts any server certificate — see the module docs for why.
#[derive(Debug)]
struct AcceptAnyCert(Arc<CryptoProvider>);

impl ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn client_config(params: &ClientParams) -> Result<quinn::ClientConfig, String> {
    let provider = provider();
    let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCert(provider)))
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let quic = QuicClientConfig::try_from(tls).map_err(|e| e.to_string())?;
    let mut config = quinn::ClientConfig::new(Arc::new(quic));
    config.transport_config(transport_config(params.window_bytes, params.congestion.as_deref())?);
    Ok(config)
}

async fn resolve(addr: &str) -> Result<SocketAddr, String> {
    tokio::net::lookup_host(addr)
        .await
        .map_err(|e| e.to_string())?
        .next()
        .ok_or_else(|| "no addresses resolved".to_string())
}

fn endpoint(
    socket: std::net::UdpSocket,
    server: Option<quinn::ServerConfig>,
) -> Result<quinn::Endpoint, String> {
    socket.set_nonblocking(true).map_err(|e| e.to_string())?;
    let runtime = quinn::default_runtime().ok_or("no async runtime available for QUIC")?;
    quinn::Endpoint::new(quinn::EndpointConfig::default(), server, socket, runtime)
        .map_err(|e| e.to_string())
}

/// After our side is done: wait (bounded) until the peer has read
/// everything we sent — closing earlier would drop in-flight data — then
/// close the connection.
async fn finish_and_close(send: &mut quinn::SendStream, conn: &quinn::Connection, wrote: bool) {
    if wrote {
        let _ = tokio::time::timeout(DRAIN_TIMEOUT, send.stopped()).await;
    }
    conn.close(0u32.into(), b"done");
}

// ───────────────────────────── client ─────────────────────────────

pub async fn run_client_stream(
    params: ClientParams,
    stream_index: u8,
    stop: Arc<AtomicBool>,
    tx: UnboundedSender<NetEvent>,
) {
    let addr = format!("{}:{}", params.host, params.port);
    let fail = |msg: String| {
        let _ = tx.send(err(stream_index, msg));
    };

    let sockaddr = match resolve(&addr).await {
        Ok(a) => a,
        Err(e) => return fail(format!("connect to {addr} failed: {e}")),
    };
    let local: SocketAddr =
        if sockaddr.is_ipv4() { ([0, 0, 0, 0], 0).into() } else { ([0u16; 8], 0).into() };
    let socket = match std::net::UdpSocket::bind(local) {
        Ok(s) => s,
        Err(e) => return fail(format!("udp bind failed: {e}")),
    };
    if let Some(tos) = params.tos.filter(|t| *t != 0)
        && let Err(e) = sockopt::set_tos(socket.as_raw_fd(), tos, sockaddr.is_ipv6())
    {
        return fail(format!("setting TOS {tos:#04x} failed: {e}"));
    }
    let mut ep = match endpoint(socket, None) {
        Ok(ep) => ep,
        Err(e) => return fail(format!("QUIC setup failed: {e}")),
    };
    match client_config(&params) {
        Ok(cfg) => ep.set_default_client_config(cfg),
        Err(e) => return fail(e),
    }

    let conn = match ep.connect(sockaddr, SERVER_NAME) {
        Ok(connecting) => match connecting.await {
            Ok(c) => c,
            Err(e) => return fail(format!("connect to {addr} failed: {e}")),
        },
        Err(e) => return fail(format!("connect to {addr} failed: {e}")),
    };
    let (mut send, mut recv) = match conn.open_bi().await {
        Ok(s) => s,
        Err(e) => return fail(format!("opening a QUIC stream failed: {e}")),
    };

    let hs_start = Instant::now();
    if let Err(e) = write_message(&mut send, &handshake_for(&params, stream_index)).await {
        return fail(format!("handshake failed: {e}"));
    }
    match read_message::<HandshakeAck, _>(&mut recv).await {
        Ok(a) if a.ok => {
            if let Err(reason) = require_server_version(&params, a.protocol_version) {
                return fail(reason);
            }
        }
        Ok(a) => return fail(a.error.unwrap_or_else(|| "server rejected handshake".into())),
        Err(e) => return fail(format!("handshake ack failed: {e}")),
    }
    let rtt_ms = hs_start.elapsed().as_secs_f64() * 1000.0;
    let _ = tx.send(NetEvent::ClientStreamConnected { stream_index, rtt_ms });

    let start = Instant::now();
    let duration = Duration::from_secs(params.duration_secs.max(1));
    let ctx = duplex::LoopContext {
        start,
        stop: &stop,
        stats: duplex::StatsSource::Quic(conn.clone()),
        chunk_bytes: params.buffer_bytes.map(|b| b as usize).unwrap_or(duplex::CHUNK),
        report_every: report_interval(&params),
    };
    duplex::run_direction(
        params.direction,
        &mut recv,
        &mut send,
        ctx,
        duration,
        |flow, report| client_interval(&tx, stream_index, flow, report),
        |flow, total, secs, avg| client_finished(&tx, stream_index, flow, total, secs, avg),
    )
    .await;
    finish_and_close(&mut send, &conn, params.direction != super::Direction::Rx).await;
}

// ───────────────────────────── server ─────────────────────────────

pub async fn run_server(params: ServerParams, tx: UnboundedSender<NetEvent>) {
    let bind_addr = format!("{}:{}", params.bind, params.port);
    let listen_error = |msg: String| {
        let _ = tx.send(NetEvent::ServerListenError(msg));
    };

    let sockaddr = match resolve(&bind_addr).await {
        Ok(a) => a,
        Err(e) => return listen_error(format!("bind to {bind_addr} failed: {e}")),
    };
    let socket = match std::net::UdpSocket::bind(sockaddr) {
        Ok(s) => s,
        Err(e) => return listen_error(format!("bind to {bind_addr} failed: {e}")),
    };
    // A second handle on the same socket, so a client's TOS request can be
    // applied to the server's sends after the handshake (see `serve_conn`).
    let socket_for_tos = socket.try_clone().ok().map(Arc::new);
    let config = match server_config(params.window_bytes) {
        Ok(c) => c,
        Err(e) => return listen_error(e),
    };
    let ep = match endpoint(socket, Some(config)) {
        Ok(ep) => ep,
        Err(e) => return listen_error(format!("bind to {bind_addr} failed: {e}")),
    };
    let actual = ep.local_addr().map(|a| a.to_string()).unwrap_or(bind_addr);
    let _ = tx.send(NetEvent::ServerListening { bind: actual });

    while let Some(incoming) = ep.accept().await {
        let tx = tx.clone();
        let socket_for_tos = socket_for_tos.clone();
        tokio::spawn(async move {
            if let Ok(conn) = incoming.await {
                serve_conn(conn, socket_for_tos, tx).await;
            }
        });
    }
}

async fn serve_conn(
    conn: quinn::Connection,
    socket: Option<Arc<std::net::UdpSocket>>,
    tx: UnboundedSender<NetEvent>,
) {
    let Ok((mut send, mut recv)) = conn.accept_bi().await else { return };
    let handshake: Handshake = match read_message(&mut recv).await {
        Ok(h) => h,
        Err(_) => return,
    };
    if let Err(reason) = check_client_version(handshake.protocol_version) {
        let _ = write_message(&mut send, &HandshakeAck::refused(reason)).await;
        return;
    }
    if write_message(&mut send, &HandshakeAck::ok()).await.is_err() {
        return;
    }
    // Best effort, and endpoint-wide: all QUIC clients share the server's
    // one UDP socket, so the most recent request wins. (A per-connection
    // congestion algorithm can't be mirrored at all — it is fixed before
    // the handshake — so the server's own sends use quinn's default.)
    if let (Some(tos), Some(socket)) = (handshake.tos.filter(|t| *t != 0), socket) {
        let ipv6 = conn.local_ip().is_some_and(|ip| ip.is_ipv6());
        let _ = sockopt::set_tos(socket.as_raw_fd(), tos, ipv6);
    }

    let stop = Arc::new(AtomicBool::new(false));
    let (session_id, stream_index) = (handshake.session_id, handshake.stream_index);
    let _ = tx.send(NetEvent::ServerStreamConnected {
        session_id,
        stream_index,
        stream_count: handshake.stream_count,
        addr: conn.remote_address().to_string(),
        stop: stop.clone(),
        duration_secs: handshake.duration_secs,
    });

    let direction = handshake.direction.mirrored();
    let ctx = duplex::LoopContext {
        start: Instant::now(),
        stop: &stop,
        stats: duplex::StatsSource::Quic(conn.clone()),
        chunk_bytes: handshake.chunk_bytes.map(|b| b as usize).unwrap_or(duplex::CHUNK),
        report_every: handshake.report_every(),
    };
    duplex::run_direction(
        direction,
        &mut recv,
        &mut send,
        ctx,
        Duration::from_secs(handshake.duration_secs.max(1)),
        |flow, report| server_interval(&tx, session_id, stream_index, flow, report),
        |flow, _total, _secs, avg| server_finished(&tx, session_id, stream_index, flow, avg),
    )
    .await;
    finish_and_close(&mut send, &conn, direction != super::Direction::Rx).await;

    let _ = tx.send(NetEvent::ServerStreamDisconnected { session_id, stream_index });
}
