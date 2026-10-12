use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The version of gauge's wire protocol this build speaks. It is bumped only
/// when a change would stop older peers from working; adding an optional
/// handshake field does not count (see `docs/protocol.md`). Peers that
/// predate the field (gauge 0.1.x) count as version 0.
pub const PROTOCOL_VERSION: u32 = 1;

/// The largest handshake message accepted over TCP/QUIC. Real ones are a few
/// hundred bytes; the cap stops a peer from making us allocate gigabytes by
/// announcing a huge length.
pub const MAX_MESSAGE_LEN: usize = 64 * 1024;

/// Whether this build can serve a client speaking `client_version`: anything
/// up to our own version is fine (we never drop support within a major
/// release), newer is not.
pub fn check_client_version(client_version: u32) -> Result<(), String> {
    if client_version > PROTOCOL_VERSION {
        Err(format!(
            "this client speaks gauge protocol {client_version}, but this server only understands \
             up to {PROTOCOL_VERSION} — please upgrade the server"
        ))
    } else {
        Ok(())
    }
}

/// Which way bulk data flows for a test. Chosen by the client and carried
/// in the handshake so the server knows whether to read, write, or both.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Client sends, server receives — the original, still-default mode.
    Tx,
    /// Reversed: server sends, client receives.
    Rx,
    /// Both directions at once, over the same connection.
    Bidir,
}

/// Sent by the client immediately after connecting (TCP) or as the first
/// datagram (UDP), before any bulk data flows. `session_id` correlates the
/// parallel streams of one logical test into a single session on the
/// server side; `stream_index`/`stream_count` identify this stream within it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Handshake {
    /// The protocol version the client speaks (absent = 0, a 0.1.x client).
    #[serde(default)]
    pub protocol_version: u32,
    pub duration_secs: u64,
    pub session_id: u64,
    pub stream_index: u8,
    pub stream_count: u8,
    pub direction: Direction,
    /// TCP write/read buffer size in bytes (the "Buffer size" setting) —
    /// carried over so the server applies the *client's* chosen chunk size
    /// too when it's the one doing the writing/reading (Rx/Bidir). `None`
    /// means "use the engine default" (`duplex::CHUNK`).
    pub chunk_bytes: Option<u32>,
    /// TOS/DSCP byte the client marks its packets with — the server marks
    /// its own sends the same way. `None`/absent: leave the OS default.
    #[serde(default)]
    pub tos: Option<u8>,
    /// TCP congestion-control algorithm the client chose; the server applies
    /// it (best effort) to its own sending side. Absent: OS default.
    #[serde(default)]
    pub congestion: Option<String>,
    /// UDP only: the client's per-stream rate target and datagram size, so
    /// the server paces and sizes its own sends the same way when it is the
    /// sender (Rx/Bidir). Absent: unthrottled / default size.
    #[serde(default)]
    pub bandwidth_bps: Option<u64>,
    #[serde(default)]
    pub mtu: Option<u32>,
    /// Seconds between the server's own interval reports — follows the
    /// client's "Interval (s)" so both sides sample alike. Absent: 1s.
    #[serde(default)]
    pub interval_secs: Option<u64>,
}

impl Handshake {
    /// The report interval to use, clamped — a handshake is untrusted input.
    pub fn report_every(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.interval_secs.unwrap_or(1).clamp(1, 60))
    }
}

/// What a UDP receiver tells the sender once a stream has ended — the sender
/// of a UDP flow has no other way to learn how much of it arrived. The server
/// sends this (a few times, since UDP may drop it) after the client's FIN.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub struct UdpSummary {
    pub received: u64,
    pub lost: u64,
    pub bytes: u64,
    pub jitter_ms: f64,
}

impl UdpSummary {
    pub fn loss_pct(&self) -> f64 {
        let expected = self.received + self.lost;
        if expected > 0 { self.lost as f64 / expected as f64 * 100.0 } else { 0.0 }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct HandshakeAck {
    pub ok: bool,
    /// Set when `ok` is false, so the client can report *why* instead of
    /// just failing generically (e.g. the client is newer than the server).
    pub error: Option<String>,
    /// The protocol version the server speaks (absent = 0, a 0.1.x server).
    #[serde(default)]
    pub protocol_version: u32,
}

impl HandshakeAck {
    pub fn ok() -> Self {
        HandshakeAck { ok: true, error: None, protocol_version: PROTOCOL_VERSION }
    }

    pub fn refused(reason: String) -> Self {
        HandshakeAck { ok: false, error: Some(reason), protocol_version: PROTOCOL_VERSION }
    }
}

/// Length-prefixed (u32 BE) JSON framing for the TCP control/data stream.
pub async fn write_message<T, S>(stream: &mut S, msg: &T) -> std::io::Result<()>
where
    T: Serialize,
    S: AsyncWrite + Unpin,
{
    let bytes = serde_json::to_vec(msg)?;
    stream.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
    stream.write_all(&bytes).await?;
    stream.flush().await
}

pub async fn read_message<T, S>(stream: &mut S) -> std::io::Result<T>
where
    T: for<'de> Deserialize<'de>,
    S: AsyncRead + Unpin,
{
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_MESSAGE_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("message of {len} bytes exceeds the {MAX_MESSAGE_LEN} byte limit"),
        ));
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    serde_json::from_slice(&buf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// UDP is datagram-oriented — no length prefix needed, the whole packet is
/// the message. Used only for the handshake/ack exchange; bulk data packets
/// use their own compact binary header (see `net::client`/`net::server`).
pub fn encode_datagram<T: Serialize>(msg: &T) -> serde_json::Result<Vec<u8>> {
    serde_json::to_vec(msg)
}

pub fn decode_datagram<T: for<'de> Deserialize<'de>>(buf: &[u8]) -> serde_json::Result<T> {
    serde_json::from_slice(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_handshake() -> Handshake {
        Handshake {
            protocol_version: PROTOCOL_VERSION,
            duration_secs: 10,
            session_id: 0xC0FFEE,
            stream_index: 2,
            stream_count: 4,
            direction: Direction::Bidir,
            chunk_bytes: Some(65536),
            tos: Some(0xB8),
            congestion: Some("reno".into()),
            bandwidth_bps: Some(1_000_000),
            mtu: Some(1500),
            interval_secs: Some(2),
        }
    }

    #[tokio::test]
    async fn tcp_framing_round_trips() {
        let mut buf: Vec<u8> = Vec::new();
        write_message(&mut buf, &sample_handshake()).await.unwrap();

        // The length prefix should match exactly the JSON payload that follows.
        let declared_len = u32::from_be_bytes(buf[0..4].try_into().unwrap()) as usize;
        assert_eq!(declared_len, buf.len() - 4);

        let mut cursor = std::io::Cursor::new(buf);
        let decoded: Handshake = read_message(&mut cursor).await.unwrap();
        assert_eq!(decoded, sample_handshake());
    }

    #[tokio::test]
    async fn tcp_framing_reads_two_consecutive_messages() {
        // The server reads a Handshake then writes back a HandshakeAck on
        // the same stream — make sure the framing doesn't leave stray bytes
        // behind that would corrupt whatever comes next.
        let mut buf: Vec<u8> = Vec::new();
        write_message(&mut buf, &sample_handshake()).await.unwrap();
        write_message(&mut buf, &HandshakeAck::ok()).await.unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        let first: Handshake = read_message(&mut cursor).await.unwrap();
        let second: HandshakeAck = read_message(&mut cursor).await.unwrap();
        assert_eq!(first, sample_handshake());
        assert_eq!(second, HandshakeAck::ok());
    }

    #[tokio::test]
    async fn read_message_errors_on_truncated_stream() {
        let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
        let result: std::io::Result<HandshakeAck> = read_message(&mut cursor).await;
        assert!(result.is_err());
    }

    #[test]
    fn udp_datagram_round_trips() {
        let bytes = encode_datagram(&sample_handshake()).unwrap();
        let decoded: Handshake = decode_datagram(&bytes).unwrap();
        assert_eq!(decoded, sample_handshake());
    }

    #[test]
    fn decode_datagram_rejects_non_json_bytes() {
        // This is exactly how net::server tells a handshake apart from a
        // bulk data/FIN packet — it must reliably fail on raw binary.
        let garbage = [0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
        let result: serde_json::Result<Handshake> = decode_datagram(&garbage);
        assert!(result.is_err());
    }

    #[test]
    fn handshake_report_every_defaults_and_clamps() {
        let mut hs = sample_handshake();
        assert_eq!(hs.report_every().as_secs(), 2);
        hs.interval_secs = None;
        assert_eq!(hs.report_every().as_secs(), 1);
        hs.interval_secs = Some(0);
        assert_eq!(hs.report_every().as_secs(), 1);
        hs.interval_secs = Some(100_000);
        assert_eq!(hs.report_every().as_secs(), 60);
    }

    #[test]
    fn a_handshake_without_a_version_is_a_legacy_peer_at_version_zero() {
        // What a gauge 0.1.x client sends: no `protocol_version` at all.
        let json = r#"{"duration_secs":10,"session_id":7,"stream_index":0,"stream_count":1,
                       "direction":"Tx","chunk_bytes":null}"#;
        let hs: Handshake = serde_json::from_str(json).unwrap();
        assert_eq!(hs.protocol_version, 0);
        assert!(check_client_version(hs.protocol_version).is_ok(), "legacy clients are served");

        let ack: HandshakeAck = serde_json::from_str(r#"{"ok":true,"error":null}"#).unwrap();
        assert_eq!(ack.protocol_version, 0, "a 0.1.x server's ack carries no version");
    }

    #[test]
    fn newer_clients_are_refused_with_a_message_that_says_what_to_do() {
        assert!(check_client_version(PROTOCOL_VERSION).is_ok());
        let reason = check_client_version(PROTOCOL_VERSION + 1).unwrap_err();
        assert!(reason.contains("protocol") && reason.contains("upgrade the server"), "{reason}");
    }

    #[test]
    fn our_own_acks_carry_our_version() {
        assert_eq!(HandshakeAck::ok().protocol_version, PROTOCOL_VERSION);
        let refused = HandshakeAck::refused("nope".into());
        assert!(!refused.ok);
        assert_eq!(
            (refused.error.as_deref(), refused.protocol_version),
            (Some("nope"), PROTOCOL_VERSION)
        );
    }

    #[tokio::test]
    async fn an_oversized_message_is_refused_before_anything_is_allocated() {
        // A peer announcing a 4 GiB message must not make us allocate it.
        let mut input: &[u8] = &u32::MAX.to_be_bytes();
        let err = read_message::<Handshake, _>(&mut input).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("limit"), "{err}");

        // Just over the cap is refused, exactly at it is read (and then fails to parse).
        let mut over: &[u8] = &((MAX_MESSAGE_LEN + 1) as u32).to_be_bytes();
        assert!(
            read_message::<Handshake, _>(&mut over)
                .await
                .unwrap_err()
                .to_string()
                .contains("limit")
        );
    }

    // ── the protocol document must describe what the code really does ────

    const PROTOCOL_DOC: &str = include_str!("../../docs/protocol.md");

    #[test]
    fn the_protocol_doc_names_the_current_protocol_version() {
        assert!(
            PROTOCOL_DOC.contains(&format!("(currently **{PROTOCOL_VERSION}**)")),
            "docs/protocol.md must say the current protocol version is {PROTOCOL_VERSION}"
        );
        assert!(PROTOCOL_DOC.contains(&format!("for protocol **version {PROTOCOL_VERSION}**")));
    }

    #[test]
    fn every_json_example_in_the_protocol_doc_parses_as_the_message_it_claims_to_be() {
        let mut examples = 0;
        let mut rest = PROTOCOL_DOC;
        while let Some(start) = rest.find("```json\n") {
            let after = &rest[start + "```json\n".len()..];
            let end = after.find("```").expect("unterminated code block");
            let block = after[..end].trim();
            rest = &after[end..];

            let parsed = serde_json::from_str::<Handshake>(block).is_ok()
                || block.lines().all(|line| {
                    serde_json::from_str::<HandshakeAck>(line).is_ok()
                        || serde_json::from_str::<UdpSummary>(line).is_ok()
                });
            assert!(parsed, "an example in docs/protocol.md no longer parses:\n{block}");
            examples += 1;
        }
        assert!(
            examples >= 3,
            "expected the handshake, ack and summary examples, found {examples}"
        );
    }

    #[test]
    fn the_documented_limits_match_the_constants() {
        assert!(
            PROTOCOL_DOC.contains(&format!("{}", MAX_MESSAGE_LEN))
                || PROTOCOL_DOC.contains("65 536")
        );
        assert_eq!(MAX_MESSAGE_LEN, 65_536, "docs/protocol.md says 65 536 bytes");
    }
}
