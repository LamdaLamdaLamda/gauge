# The gauge wire protocol

This document specifies what `gauge client` and `gauge server` send each other,
for protocol **version 1** (gauge 0.2.0 and later). It is written so that a
compatible implementation can be built from it, and so that changes to the
protocol are deliberate rather than accidental.

> **gauge is not compatible with iperf3.** Different framing, different
> handshake, different everything. Do not point one at the other.

- [Overview](#overview)
- [Versioning and compatibility](#versioning-and-compatibility)
- [The handshake](#the-handshake)
- [TCP](#tcp)
- [QUIC](#quic)
- [UDP](#udp)
- [Directions and who sends what](#directions-and-who-sends-what)
- [Sessions and parallel streams](#sessions-and-parallel-streams)
- [Limits and untrusted input](#limits-and-untrusted-input)
- [What the protocol does not provide](#what-the-protocol-does-not-provide)

## Overview

A **test** consists of one or more **streams** (`-P`). Each stream is its own
transport connection:

| Transport | One stream is… |
|---|---|
| TCP | one TCP connection |
| QUIC | one QUIC connection carrying one bidirectional stream |
| UDP | one UDP "association": a client socket and the server-side socket it is paired with |

Every stream starts with a **handshake** — a small JSON message from the client
describing the test, answered by a JSON **ack** from the server. After that,
bulk data flows: zero-filled bytes (TCP, QUIC) or numbered, timestamped
datagrams (UDP). The payload carries no information beyond its size and, for
UDP, the sequence number and timestamp; the content is zeros.

All numbers in binary headers are big-endian. All JSON is UTF-8 and
unframed-extensible: **receivers ignore unknown fields** (that is what makes it
possible to add fields without a new protocol version).

## Versioning and compatibility

The protocol has an integer version, `PROTOCOL_VERSION` (currently **1**).

| Version | gauge releases | Notes |
|---|---|---|
| 0 | 0.1.x | Not a number the peer sends: **a peer that sends no `protocol_version` is version 0**. |
| 1 | 0.2.0 → | Adds `protocol_version` to the handshake and ack, the UDP receiver summary and the UDP per-stream server socket. |

**Rules**

1. The client puts its version in the handshake (`protocol_version`); the
   server puts its own in every ack, accepted or refused.
2. A server serves every client whose version is **less than or equal to**
   its own. It refuses a **newer** client with an ack of `ok: false` and an
   error telling the user to upgrade the server, then closes the stream.
3. A client works with an older server, except where a feature needs a newer
   protocol. Today that is only the **UDP rate search** (`-b auto`), which needs
   protocol 1 (the receiver summary); the client refuses an older server with a
   clear error. An older server simply never sends the UDP receiver summary,
   so a plain UDP TX test then shows no server-measured loss. The client knows
   this from the ack's `protocol_version` and does not wait for a summary.
4. **What bumps the version:** any change that would make an *older peer fail or
   misbehave*: removing or renaming a field, changing the meaning of a field or
   of the framing, changing the data format. **What does not:** adding an
   *optional* field that older peers ignore and whose absence has a defined
   meaning (like every field marked "absent =" below).
5. Within a major release of gauge (1.x) the protocol version is never lowered
   and support for earlier versions is not dropped.

## The handshake

Sent by the client first, as a JSON object. Fields:

| Field | Type | Required | Meaning |
|---|---|---|---|
| `protocol_version` | u32 | no (absent = 0) | The client's protocol version. |
| `duration_secs` | u64 | yes | How long the test runs. Whoever sends enforces it (see [Directions](#directions-and-who-sends-what)). Values below 1 are treated as 1. |
| `session_id` | u64 | yes | Random per test; ties parallel streams together. |
| `stream_index` | u8 | yes | This stream's position, `0 … stream_count − 1`. |
| `stream_count` | u8 | yes | How many streams the test has. |
| `direction` | `"Tx"` \| `"Rx"` \| `"Bidir"` | yes | **From the client's point of view**: `Tx` = the client sends, `Rx` = the server sends, `Bidir` = both. |
| `chunk_bytes` | u32 \| null | no (absent = null) | Size of each write/read call for TCP/QUIC (the "buffer size"). `null` = 128 KiB. A server caps it at 16 MiB. |
| `tos` | u8 \| null | no (absent = null) | IP TOS / DSCP byte the client marks its packets with; the server applies it to its own sends. `0`/`null` = leave the OS default. |
| `congestion` | string \| null | no | TCP congestion-control algorithm the client chose; the server applies it best-effort to its own sending side. |
| `bandwidth_bps` | u64 \| null | no | UDP: the per-stream target rate. The server paces its own sends (Rx/Bidir) the same way. `null` = unthrottled. |
| `mtu` | u32 \| null | no | UDP: the link MTU used to size datagrams (payload = `mtu − 28`, clamped to 64 … 65000 bytes). `null` = 1500. |
| `interval_secs` | u64 \| null | no | Seconds between the server's own interval reports. The server clamps it to 1 … 60; `null` = 1. |

Example:

```json
{"protocol_version":1,"duration_secs":10,"session_id":11640217725347829033,
 "stream_index":0,"stream_count":4,"direction":"Bidir","chunk_bytes":131072,
 "tos":184,"congestion":null,"bandwidth_bps":null,"mtu":9000,"interval_secs":1}
```

The server answers with the **ack**:

| Field | Type | Meaning |
|---|---|---|
| `ok` | bool | `true`: the stream is accepted and data may flow. |
| `error` | string \| null | When `ok` is `false`, a human-readable reason to show the user. |
| `protocol_version` | u32 (absent = 0) | The server's protocol version. |

```json
{"ok":true,"error":null,"protocol_version":1}
{"ok":false,"error":"this client speaks gauge protocol 2, but this server only understands up to 1 — please upgrade the server","protocol_version":1}
```

After a refusal the server closes the stream (TCP/QUIC) or creates no state
(UDP); the client reports `error` and ends that stream.

## TCP

**Connection.** The client opens a TCP connection to the server's port
(default 5201). It sets `TCP_NODELAY`, and — before connecting — the socket
buffers (`--window`), the TOS byte and the congestion algorithm. The server
listens with the same buffer size and sets `TCP_NODELAY` on accepted sockets.

**Framing of the handshake and ack.** A 4-byte unsigned big-endian length,
followed by that many bytes of JSON. The length must not exceed
**65 536** bytes; a larger announcement makes the receiver drop the connection
without reading it.

**Sequence**

```text
client                                         server
  │── connect ─────────────────────────────────▶│
  │── [len][Handshake JSON] ───────────────────▶│  (validated: version)
  │◀── [len][Ack JSON] ─────────────────────────│
  │       … bulk data, per `direction` …        │
  │── shutdown(write) after `duration` ────────▶│  (Tx / Bidir: client's side ends)
  │◀── shutdown(write) after `duration` ────────│  (Rx / Bidir: server's side ends)
```

**Bulk data.** Raw bytes, no framing: zero-filled writes of `chunk_bytes`
(default 128 KiB). The reader reads until EOF. There is no end-of-test message
other than the sender's `shutdown(write)` (TCP FIN). The round-trip time
reported as "connect RTT" is the time from sending the handshake to receiving
the ack.

**Statistics.** Each side reads retransmissions and congestion window from the
kernel locally; none of it goes over the wire.

## QUIC

QUIC (via the `quinn` implementation) carries the *same* stream protocol as
TCP:

- **Transport:** QUIC v1 over UDP, TLS 1.3. ALPN `gauge`, server name `gauge`.
- **Certificates:** the server generates a throw-away self-signed certificate
  at start. The client **does not verify it** — see
  [What the protocol does not provide](#what-the-protocol-does-not-provide).
- **One connection per stream**, on which the client opens **one
  bidirectional stream** and then sends the handshake in the TCP framing
  above (length + JSON). The server reads it, answers with the ack on the same
  stream, and the bulk data follows on that stream exactly as for TCP.
- **Ending:** a side that has written data finishes its send stream and waits
  (up to 3 s) until the peer has read it to completion; then it closes the
  connection with code 0 and reason `done`. The connection's idle timeout is
  15 s.
- `--window` becomes QUIC's flow-control windows; `--congestion` selects
  quinn's controller (cubic / bbr / reno) **on the client only** — the server's
  own sends use quinn's default. TOS is set on the UDP socket; on the server it
  is one socket shared by all connections, so the most recent request wins.

## UDP

UDP has two kinds of datagrams: **JSON control datagrams** (handshake, ack,
summary) and **binary data datagrams**. They are told apart by the first byte:
control datagrams are JSON objects and start with `{` (0x7B); a data
datagram's first byte is the top byte of its sequence number, which is never
`{` in practice (a sequence number would have to be about 9·10^18).

### Establishing a stream

```text
client (own socket, connect()ed to server)          server
  │── Handshake JSON datagram ─────────────────────▶│  listening socket
  │      (resent every 250 ms, up to 8 times, until acked)
  │                                                  │  creates a socket for this stream
  │◀── Ack JSON datagram ───────────────────────────│  (same local port as the listener)
  │       … data datagrams, per `direction` …        │
```

The server creates a **dedicated socket per stream before it sends the ack**:
bound to the listener's address and port with `SO_REUSEPORT` and `connect()`ed
to the client's address. From then on the kernel delivers that client's
datagrams to the dedicated socket, and each stream is served by its own task.
Consequences a compatible implementation must handle:

- A **repeated handshake** (the ack was lost) may arrive at the listener or at
  the stream's own socket; either way the server **answers with the ack again**
  and does not create a second stream.
- The ack's source address is the server's address and port, as for any reply.
- Datagrams from an unknown source that are not a valid handshake are
  discarded.

### Data datagrams

```text
 0               8              16
 ┌───────────────┬───────────────┬───────────── … ──┐
 │ seq  (u64 BE) │ send time (u64│  zero padding     │
 │               │ BE, ns since  │  up to the packet │
 │               │ UNIX epoch)   │  size             │
 └───────────────┴───────────────┴───────────── … ──┘
```

- `seq` starts at 0 and increases by 1 per datagram of one direction of one stream.
- The send time is the sender's wall clock. It is used only for jitter (below),
  where a constant offset between the two clocks cancels out, so the clocks do
  not need to be synchronised.
- The packet size is `mtu − 28` (IPv4 + UDP headers), clamped to 64 … 65000
  bytes. At least 16 bytes (the header) are always sent.
- **End of stream (FIN):** a 16-byte datagram whose `seq` is `0xFFFFFFFFFFFFFFFF`
  (`u64::MAX`), sent **3 times** by the sender when it is done.
- **Pacing:** with a target rate, packets are spaced evenly (not in bursts);
  without one the sender goes as fast as it can.

### What the receiver measures

- **Loss:** a datagram with `seq ≥ next_expected` adds `seq − next_expected`
  to the lost count and sets `next_expected = seq + 1`. A late or duplicate
  datagram (`seq < next_expected`) is counted as received but **not** as lost.
  Loss % = `lost / (received + lost)`.
- **Jitter:** RFC 3550 interarrival jitter. For each datagram,
  `transit = arrival_ns − send_time_ns`; with the previous datagram's transit,
  `J += (|transit − previous_transit| − J) / 16`. Reported in milliseconds.
- **Idle timeout:** a receiver that hears nothing for **3 s** stops.
  A receiver also stops after the test duration plus a grace period
  (3 s on the client, 6 s on the server) so a vanished peer cannot hold it.

### Receiver summary

When a UDP stream ends — on the FIN or on the idle timeout — the **receiving
server** sends the sender a summary so it can learn what arrived (UDP has no
other feedback channel). It is a JSON datagram, sent **3 times**, 15 ms apart:

```json
{"received":251380,"lost":120,"bytes":368103424,"jitter_ms":0.042}
```

A client that sent data waits up to **1.5 s** for it after its own FIN (only if the server's ack says it speaks protocol 1 or later). A
client receiving data (`Rx`) measures loss and jitter itself and needs no
summary. A server that predates protocol 1 never sends one.

## Directions and who sends what

`direction` is given from the **client's** point of view; the server mirrors it.

| `direction` | Client | Server | Duration enforced by |
|---|---|---|---|
| `Tx` | writes for `duration_secs`, then ends | reads until the client ends | the client |
| `Rx` | reads until the server ends | writes for `duration_secs`, then ends | the server |
| `Bidir` | does both at once | does both at once | each side for its own sending |

Each direction of a bidirectional stream is independent: its own sequence
numbers (UDP), its own FIN and summary. In `Bidir` over UDP the single
connection carries both directions' datagrams.

## Sessions and parallel streams

All streams of one test share a `session_id` and have different
`stream_index` values below `stream_count`. The server uses them only to group
streams for display (the server UI shows the most recent session).
Streams are otherwise independent: each has its own connection, its own
timing and its own end. The **UDP rate search** runs a series of short tests
back to back, each with a fresh `session_id`.

The server can end a stream early on the operator's request ("kill client");
that closes the connection (TCP/QUIC). For UDP the server stops serving the
stream; the client is not told and carries on until its own duration is up.

## Limits and untrusted input

Everything a peer sends is untrusted. What an implementation must bound:

| Input | Limit |
|---|---|
| Handshake / ack length (TCP, QUIC) | 65 536 bytes; larger is refused without allocating |
| `interval_secs` | clamped to 1 … 60 |
| `chunk_bytes` | the server uses 1 byte … 16 MiB, whatever the client asks for |
| `duration_secs` | at least 1; **no upper bound is enforced** — a client can ask a server to send or receive for as long as it stays connected |
| UDP datagram size | received buffers are 64 KiB; the sender's packet size is clamped to 64 … 65000 |
| Parallel streams | `stream_count` is a `u8`; the server places no further cap on how many streams or sessions it serves |

## What the protocol does not provide

- **No authentication or authorisation.** Anyone who can reach the port can
  start a test and use the server's bandwidth. Restrict reachability
  (`--bind`, a firewall).
- **No confidentiality for TCP and UDP.** Payload is zeros, but the handshake
  and all metadata travel in the clear.
- **QUIC is encrypted but not authenticated:** the client accepts any
  certificate. A QUIC test proves nothing about *who* the server is.
- **No integrity beyond the transport's own** (checksums; TLS for QUIC).
- **No congestion or rate negotiation.** The tester deliberately saturates the
  path; `--bandwidth` is the only brake.
