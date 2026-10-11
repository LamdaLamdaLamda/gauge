use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

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
    /// just failing generically (e.g. UDP doesn't support Rx/Bidir yet).
    pub error: Option<String>,
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
        write_message(&mut buf, &HandshakeAck { ok: true, error: None }).await.unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        let first: Handshake = read_message(&mut cursor).await.unwrap();
        let second: HandshakeAck = read_message(&mut cursor).await.unwrap();
        assert_eq!(first, sample_handshake());
        assert_eq!(second, HandshakeAck { ok: true, error: None });
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
}
