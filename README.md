<div align="center">

# gauge

**A network throughput tester in Rust — an `iperf`-style client/server with a full terminal UI.**

TCP · UDP · QUIC &nbsp;|&nbsp; TX / RX / bidirectional &nbsp;|&nbsp; real loss, jitter and retransmit measurements

![Rust 1.88+](https://img.shields.io/badge/rust-1.88%2B-orange)
![License: MIT](https://img.shields.io/badge/license-MIT-green)
![Platforms](https://img.shields.io/badge/platforms-Linux%20%7C%20macOS-blue)
![Status](https://img.shields.io/badge/status-0.1.0-lightgrey)

</div>

```text
┌─────────────────────────────────── gauge — rust network performance tool ────────────────────────────────────┐
│ ≋ gauge │ Live Test │ Config │ History │ Server Mode │                           ● running  → 127.0.0.1:5201 │
│┌ ↑ TX Throughput ────── TCP · real ┐┌ ↓ RX Throughput ────── TCP · real ┐┌ Latency ─ connect RTT (one-time) ┐│
││20.88 Gbit/s                       ││21.20 Gbit/s                       ││0.48 ms                           ││
││ ──────────────────────────────────││───────────────────────────────────││ ─────────────────────────────────││
││0                       target 10G ││0                       target 10G ││0                        p99: n/a ││
││▇▆▆█                               ││▇▅▆█                               ││                                  ││
││████                               ││████                               ││                                  ││
││████                               ││████                               ││                                  ││
││████                               ││████                               ││                                  ││
│└───────────────────────────────────┘└───────────────────────────────────┘└──────────────────────────────────┘│
│┌ Per-Stream Breakdown ─────────────────────────────────── 1 stream(s) ┐┌ Test Stats ────────────────────────┐│
││Stream  TX Gb/s RX Gb/s RTT ms  Retr  Cwnd KB Loss    Jitter   State  ││Protocol            TCP             ││
││#1      20.88   21.20   0.48    0     511     0.00%   –        ok     ││Duration            00:00:04        ││
││                                                                      ││Total TX            10.19 GB        ││
││                                                                      ││Total RX            10.24 GB        ││
││                                                                      ││Retransmits         0               ││
││                                                                      ││Streams             1               ││
││                                                                      ││MTU                 9000            ││
│└──────────────────────────────────────────────────────────────────────┘└────────────────────────────────────┘│
│┌ Event Log ──────────────────────────────────────────────────────────────────────────────── live · last 60s ┐│
││18:50:16 [DATA] interval 3-4s  RX 21.20 Gbit/s aggregate (1 stream(s))                                      ││
││18:50:16 [DATA] interval 3-4s  TX 20.88 Gbit/s aggregate (1 stream(s))                                      ││
││18:50:15 [DATA] interval 2-3s  RX 20.06 Gbit/s aggregate (1 stream(s))                                      ││
││18:50:15 [DATA] interval 2-3s  TX 19.97 Gbit/s aggregate (1 stream(s))                                      ││
││18:50:14 [DATA] interval 1-2s  RX 19.58 Gbit/s aggregate (1 stream(s))                                      ││
││18:50:14 [DATA] interval 1-2s  TX 19.86 Gbit/s aggregate (1 stream(s))                                      ││
││18:50:13 [DATA] interval 0-1s  RX 21.04 Gbit/s aggregate (1 stream(s))                                      ││
││18:50:13 [DATA] interval 0-1s  TX 20.78 Gbit/s aggregate (1 stream(s))                                      ││
││18:50:12 [OK  ] connected — TCP handshake 0.48ms                                                            ││
│└────────────────────────────────────────────────────────────────────────────────────────────────────────────┘│
│ target 127.0.0.1:5201 │ proto TCP │ streams 1 │ window 256K │ mtu 9000 │                 ● test running — 4s │
│ s  Stop   r  Restart   e  Export   1-4  Switch view   q  Quit                                                │
└──────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

<sub>The Live Test screen during a bidirectional TCP test (rendered from a real run).</sub>

> [!IMPORTANT]
> **gauge is not compatible with iperf3.** It has its own wire protocol, so a gauge client cannot talk to an iperf3 server or the other way round. Run `gauge server` on the far end.

---

## Table of contents

- [Why gauge?](#why-gauge)
- [Quick start](#quick-start)
- [Installation](#installation)
- [Usage](#usage)
  - [The terminal UI](#the-terminal-ui)
  - [The command line](#the-command-line)
  - [Recipes](#recipes)
- [Reference](#reference)
  - [Which option applies to which protocol](#which-option-applies-to-which-protocol)
  - [How the numbers are measured](#how-the-numbers-are-measured)
  - [Configuration file](#configuration-file)
  - [Files gauge writes](#files-gauge-writes)
- [Security notes](#security-notes)
- [Troubleshooting](#troubleshooting)
- [Architecture](#architecture)
- [Development](#development)
- [Limitations](#limitations)
- [Contributing](#contributing) · [Changelog](CHANGELOG.md)
- [License](#license)

## Why gauge?

- **One tool, two front ends.** The same engine drives an interactive TUI and a scriptable CLI (`gauge client` / `gauge server`). Output composes with pipes, `tee`, cron and CI; `--json` gives machine-readable results.
- **Three transports.** TCP, UDP and QUIC, each in TX, RX (`--reverse`) and bidirectional mode, with parallel streams.
- **Real measurements, nothing simulated.** UDP loss and jitter (RFC 3550) are measured by whichever side receives. TCP retransmits and the congestion window come straight from the kernel; QUIC's from its own connection statistics.
- **Everything on screen does something.** Each setting in the UI maps to a real socket option or engine behavior, and a setting that does not apply to the chosen protocol says so in its label.
- **Remembers.** Every run is saved to a history (with the options it used, TX/RX charts and JSON export); every event is also written to a log file; defaults live in a TOML config shared by CLI and TUI.
- **Tested end to end.** Real sockets on loopback, rendered screens, and the actual binary driven through a pseudo-terminal — see [Development](#development).

| | gauge | iperf3 |
|---|---|---|
| Interactive TUI with live charts | ✅ | ❌ |
| QUIC | ✅ | ❌ |
| Persistent run history | ✅ | ❌ |
| Wire-compatible with the other tool | ❌ | — |
| Long-established, widely deployed | ❌ (new) | ✅ |

## Quick start

```bash
# 1. build and install
cargo install --path .

# 2. on the far end (or another terminal): start a server
gauge server

# 3. run a 10 second test against it
gauge client --host 127.0.0.1
```

Prefer the interactive UI? Just run `gauge` with no arguments:

```bash
gauge          # F5 on the Config screen starts a test
```

## Installation

**Requirements:** Linux or macOS, and Rust **1.88 or newer** (the crate uses the 2024 edition). Windows is not supported.

```bash
git clone <this repository>
cd gauge

cargo install --path .        # installs the `gauge` binary into ~/.cargo/bin
# or, without installing:
cargo build --release         # binary at target/release/gauge
```

Use a **release build** for real measurements — the debug build is far slower, especially for QUIC. The TUI needs a real terminal (TTY) of at least about 80×24; larger is nicer.

## Usage

### The terminal UI

Run `gauge` without a subcommand. Switch screens with `1`–`4`, quit with `q`.

| Screen | What it does | Keys |
|---|---|---|
| **1 Live Test** | Throughput gauges and sparklines for TX/RX, latency or jitter, a per-stream table (RTT, retransmits, congestion window, loss, jitter), test stats and an event log | `s` stop · `r` restart · `e` export JSON |
| **2 Config** | Set up a test: target, protocol, duration, streams, direction, window, MTU, buffer, interval, bandwidth, TOS, congestion, omit. A live **CLI preview** shows the equivalent `gauge client` command | `Tab`/`↑↓` next field · `←→`/`Space` change · `Enter` edit a text field · `Esc` cancel · `F5` start · `Ctrl+S` save as defaults |
| **3 History** | Every completed run with its summary, the options it used and a TX/RX chart | `↑↓` select · `e` export JSON · `d` delete |
| **4 Server Mode** | Run a server from inside the UI: watch connected clients and their live stats | `Tab`/`↑↓` next field · `F5` start · `Ctrl+S` save as defaults · `s` stop · `k` kill client · `c` clear log |

### The command line

```bash
gauge server [OPTIONS]    # listen for tests
gauge client [OPTIONS]    # run a test
```

```text
$ gauge client --help
Usage: gauge client [OPTIONS]

Options:
      --host <HOST>              Server host or IP to connect to. [default: 127.0.0.1]
  -p, --port <PORT>              Server port. [default: 5201]
  -t, --duration <DURATION>      Test duration in seconds. [default: 10]
  -P, --parallel <PARALLEL>      Number of parallel streams. [default: 1]
  -u, --udp                      Use UDP instead of TCP
      --quic                     Use QUIC (encrypted, over UDP) instead of TCP
  -R, --reverse                  Server sends, client receives
      --bidir                    Both directions at once, over the same connection
  -b, --bandwidth <BANDWIDTH>    UDP target bitrate, e.g. "100M" or "1G" (unthrottled if omitted)
      --window <WINDOW>          TCP socket buffer / QUIC flow-control window / UDP socket buffers: "256K", "1M" or "auto" [default: 256K]
      --mtu <MTU>                UDP datagram size: payload = mtu - 28 [default: 9000]
      --buffer <BUFFER>          Size in bytes of each write()/read() call (TCP, QUIC) [default: 131072]
      --omit                     Exclude the first 3 seconds from the average/peak rate
  -i, --interval <INTERVAL>      Seconds between live reports, 1-60 [default: 1]
      --tos <TOS>                IP TOS/DSCP byte, decimal or hex, e.g. 0xB8 (EF)
      --congestion <CONGESTION>  Congestion control: cubic, bbr, reno (TCP: Linux only; QUIC: everywhere)
  -J, --json                     Print the final result as JSON

$ gauge server --help
Usage: gauge server [OPTIONS]

Options:
      --bind <BIND>      Address to bind to [default: 0.0.0.0]
  -p, --port <PORT>      Port to listen on [default: 5201]
  -u, --udp              Accept UDP tests instead of TCP
      --quic             Accept QUIC tests instead of TCP (self-signed certificate)
      --window <WINDOW>  TCP socket buffer / QUIC flow-control window / UDP socket buffers [default: 256K]
  -J, --json             Print each event as one JSON line
  -1, --one-off          Stop after the first completed test
```

`gauge <command> --help` prints the full text. Every option also has a [config-file default](#configuration-file).

A client prints one line per interval and a summary, and records the run to the [history](#files-gauge-writes):

```text
$ gauge client --bidir -t 5
gauge client → 127.0.0.1:5201  (TCP · 1 stream(s) · bidir · 5s)
connected — handshake 0.63ms
[sum]   0-  1s  TX   19.25 Gbit/s
[sum]   0-  1s  RX   19.21 Gbit/s
[sum]   1-  2s  TX   17.99 Gbit/s
[sum]   1-  2s  RX   18.00 Gbit/s
...
────────────────────────────────────────
TX avg 16.95 / RX avg 16.99 Gbit/s over 5s · 10.60 / 10.62 GB total
retransmits: 0
```

With `--json` the final result is one JSON document (target, summary, peak/average rates, RTT, retransmits, the TX/RX charts and the options used) — the same shape the history stores.

The process **exit code** is `0` on success and non-zero if the test could not be run (bad arguments, connection failure, rejected option).

### Recipes

```bash
# Throughput with 4 parallel TCP streams for 30 s
gauge client --host 10.0.0.5 -P 4 -t 30

# Download direction (server sends)
gauge client --host 10.0.0.5 --reverse

# How lossy is this path? UDP at a fixed 100 Mbit/s: loss and jitter are measured
gauge server --udp                                   # far end
gauge client --host 10.0.0.5 --udp -b 100M -t 30

# Both directions at once over UDP, with a larger datagram size
gauge client --host 10.0.0.5 --udp --bidir -b 200M --mtu 1500

# QUIC, with BBR congestion control (works on macOS too, unlike TCP)
gauge server --quic                                  # far end
gauge client --host 10.0.0.5 --quic --congestion bbr

# Does my QoS marking survive? Mark packets as EF (voice) and watch the path
gauge client --host 10.0.0.5 --tos 0xB8

# Skip TCP slow start in the reported average, report every 2 s
gauge client --host 10.0.0.5 -t 20 --omit -i 2

# Machine-readable result
gauge client --host 10.0.0.5 -J | jq '.avg_gbps'

# A server that handles one test and exits (handy in scripts)
gauge server --one-off
```

## Reference

### Which option applies to which protocol

| Option | TCP | UDP | QUIC | Notes |
|---|:-:|:-:|:-:|---|
| `--reverse`, `--bidir` | ✅ | ✅ | ✅ | |
| `--parallel`, `--duration`, `--interval`, `--omit` | ✅ | ✅ | ✅ | |
| `--window` | ✅ | ✅ | ✅ | TCP: `SO_SNDBUF`/`SO_RCVBUF`. QUIC: flow-control windows. UDP: enlarges the socket buffers (never below the OS default); the kernel may cap it (Linux: `net.core.rmem_max`) |
| `--buffer` | ✅ | — | ✅ | Size of each `write()`/`read()` call (iperf3's `-l`), not the kernel buffer |
| `--bandwidth` | — | ✅ | — | Pacing for UDP; TCP and QUIC rely on congestion control |
| `--mtu` | — | ✅ | — | UDP payload = `mtu − 28`. TCP has no portable socket option to force an MTU |
| `--tos` | ✅ | ✅ | ✅ | `IP_TOS` (IPv6: `IPV6_TCLASS`); also applied to the server's sends |
| `--congestion` | Linux only | — | ✅ | Refused with an error where it can't be honored — never silently ignored |

### How the numbers are measured

- **Throughput** is bytes moved per interval, as seen by the sender or receiver of that flow. The final average uses exact byte totals, so the live samples (including the short last one) add up to the true total.
- **`--omit`** excludes the first 3 seconds from the *average and peak* only (TCP slow start); total bytes are never adjusted. A test not longer than 3 s ignores `--omit` and says so.
- **UDP loss** comes from sequence-number gaps and **jitter** is the RFC 3550 interarrival jitter, both measured by the receiving side: the server for TX tests, the client for RX tests, both for bidirectional ones.
- **TCP retransmits and congestion window** are read from the kernel each interval (`TCP_INFO` on Linux, `TCP_CONNECTION_INFO` on macOS). TCP has no direct loss counter, so **loss %** is an estimate: retransmitted ÷ sent segments, with the segment count derived from bytes ÷ MSS.
- **QUIC retransmits, congestion window and loss** come from the connection statistics of [quinn](https://github.com/quinn-rs/quinn); loss is exact (lost ÷ sent packets).
- **RTT** is the time of the application-level handshake round trip, measured once at connect.

### Configuration file

An optional TOML file holds defaults for the flags above. No file, a missing section or a missing field all just mean "use the built-in default."

```toml
[client]
host = "10.0.0.5"
port = 5555
duration_secs = 30
parallel = 4
window = "1M"
mtu = 1500
buffer = "65536"
omit = true
interval_secs = 2
tos = 184            # 0xB8 (EF)
congestion = "bbr"   # Linux only for TCP
# udp = true         # or: quic = true

[server]
bind = "0.0.0.0"
port = 5555
window = "512K"
```

Precedence for every setting: **command-line flag > config file > built-in default.** The on/off flags (`--udp`, `--quic`, `--reverse`, `--bidir`, `--omit`, `--json`, `--one-off`) are the one exception: they OR together with the file, since there is no `--no-udp`.

`Ctrl+S` on the Config or Server Mode screen writes the screen's current values into the matching section (keeping the other one intact), so the TUI and the CLI share one set of defaults.

### Files gauge writes

| File | Location | Content |
|---|---|---|
| `config.toml` | OS config dir | defaults (see above) |
| `history.json` | OS data dir | the last 50 completed runs |
| `gauge.log` | OS data dir | every Live Test / Server Mode event-log line, append-only, UTC-timestamped, tagged `[live]` / `[server]` |
| `gauge-history-*.json` | current directory | only when you press `e` (export) |

The OS config and data dirs are `~/.config/gauge/` and `~/.local/share/gauge/` on Linux, and both `~/Library/Application Support/gauge/` on macOS.

## Security notes

gauge is a measurement tool, not a hardened service. Know what it does and does not do:

- **No authentication.** Any host that can reach a running `gauge server` can start a test against it and use its bandwidth. Bind to a specific interface (`--bind`) or firewall the port on untrusted networks, and stop the server when you are done.
- **TCP and UDP tests are unencrypted.**
- **QUIC is encrypted but the server is not authenticated.** The server generates a throw-away self-signed certificate on every start, and the client deliberately does not verify it. A QUIC test measures the transport; it does not prove you reached the right server.
- Input from the network (handshake fields such as the report interval) is validated and clamped.

## Troubleshooting

| Symptom | Cause and fix |
|---|---|
| `connect to …: Permission denied (os error 13)` | The OS refused to even try the connection. Common causes: a mistyped target such as a subnet's network or broadcast address (ending in `.0` / `.255`), a firewall or VPN policy, or macOS's *Local Network* privacy setting for your terminal. Double-check `--host`, and that the port is reachable (`nc -vz host port`). The **default port is 5201**. |
| `bind to … failed: Address already in use` | The port is taken, perhaps by another gauge or an iperf3 server (also 5201). Pick another with `--port`. |
| `connection refused` / handshake times out | No server on that port, wrong protocol (a TCP client cannot reach a `--udp` or `--quic` server), or a firewall dropping UDP. The client and server **must use the same protocol flag**. |
| `--congestion` fails on macOS | TCP congestion control cannot be chosen per socket on macOS. Use Linux, or `--quic`, whose controller is selectable everywhere. |
| QUIC is much slower than TCP on loopback | QUIC runs in user space and encrypts every packet; without UDP segmentation offload (e.g. on macOS) one core caps out around 1 Gbit/s. Use a release build and measure on the real path. |
| High UDP loss without `-b` (most with `--bidir` and several streams) | Unthrottled senders use every CPU cycle; once the machine's cores are saturated the receivers can't keep up and the OS drops the excess. The loss is real, but a property of the test machine, not of the network. Set a target rate with `-b` (e.g. `-b 500M`), which gives 0 % while the receivers keep up. A larger `--window` (UDP socket buffers) only helps with short bursts. |
| Average looks too low with `--omit` | Omit is ignored for tests not longer than 3 s and the summary says so; use longer tests. |
| The TUI looks broken or garbled | Use a real terminal with 256-color support and at least ~80×24. Without a real terminal the TUI exits with an error — use `gauge client` / `gauge server` in scripts. |

## Architecture

```text
src/
  main.rs        CLI-vs-TUI dispatch, terminal setup/teardown, TUI event loop
  cli.rs         non-interactive `client` / `server` subcommands (clap)
  app.rs         TUI application state, input handling, real-event-driven updates
  store.rs       history persistence (JSON) + JSON export
  config.rs      TOML config file (defaults for the CLI and the Config screen)
  filelog.rs     appends every Event Log line to gauge.log
  util.rs  theme.rs
  net/
    mod.rs       background Tokio runtime + command/event channel to the sync UI
    protocol.rs  handshake framing (length-prefixed JSON for TCP/QUIC, datagrams for UDP)
    duplex.rs    shared stream read/write loops, interval reporting, per-direction driver
    client.rs    connects and runs a test; TCP and UDP clients
    server.rs    accepts tests; TCP server and UDP server (one socket + task per stream)
    quic.rs      QUIC transport: TLS setup, one connection + stream per test stream
    udp.rs       UDP sender loop, loss/jitter bookkeeping, client receive loop
    tcpinfo.rs   live TCP stats via getsockopt (TCP_INFO / TCP_CONNECTION_INFO)
    sockopt.rs   TOS / traffic class and TCP congestion control
    bandwidth.rs window.rs   "100M" / "256K" parsing
  ui/            Ratatui screens: live, config, history, server (+ shared widgets)
tests/
  tui_pty.rs     end-to-end tests driving the real binary through a PTY
```

The network engine runs on its own OS thread hosting a Tokio runtime, fully decoupled from the synchronous Ratatui render loop: `NetCommand`s go in, `NetEvent`s come out over plain channels. The CLI drives the same channels with blocking receives. TCP and QUIC share one set of read/write loops (`duplex.rs`); only the handshake and socket setup differ.

Built on [Ratatui](https://ratatui.rs), [Crossterm](https://github.com/crossterm-rs/crossterm), [Tokio](https://tokio.rs), [quinn](https://github.com/quinn-rs/quinn) (QUIC), [rustls](https://github.com/rustls/rustls) and [clap](https://docs.rs/clap). A little `libc` is used for socket options and live TCP state.

## Development

```bash
cargo build                          # debug build
cargo test                           # all tests (about 20 s)
just check                           # fmt, clippy -D warnings, cargo audit, cargo deny, tests
just linux                           # the same lints + tests in a Linux container
```

CI (`.github/workflows/ci.yml`) checks formatting (`cargo fmt --check`) and the dependency policy (`cargo deny`: licenses, sources, RustSec advisories), then runs clippy with `-D warnings`, the full test suite and a release build on **Linux and macOS**. Dependabot proposes weekly dependency and action updates. See [CONTRIBUTING.md](CONTRIBUTING.md).

**139 tests**, in four layers:

| Layer | What it checks |
|---|---|
| State machine | key handling, field navigation, config merge and save, omit/jitter/interval logic, history navigation |
| Engine integration | real sockets on `127.0.0.1:0`: TCP, UDP and QUIC in every direction, parallel streams, loss/jitter, kernel stats, window/buffer/MTU/TOS/congestion effects, report intervals (client and server), kill-client, exact byte accounting |
| Rendered screens | every screen drawn into ratatui's in-memory `TestBackend` and checked for labels, panels, footer keys, protocol-specific wording and small-terminal safety |
| End to end | the real binary in a pseudo-terminal, driven with real key bytes and read back through a terminal emulator: screen switching, editing a field, a full test landing in History and the log, an unreachable server, Server Mode with a CLI client, `Ctrl+S` writing the config. Each runs in a throw-away `HOME`, so your own config and history are never touched |

Tests never touch your real config, history or log: those paths are stubbed in unit tests and redirected in the end-to-end tests.

## Limitations

- **Not wire-compatible with iperf3** (see the note at the top).
- **Linux and macOS only.** The code uses Unix file descriptors and socket options directly.
- **TCP congestion control per socket is Linux-only**; QUIC offers it on every platform.
- **No MTU control for TCP** — there is no portable socket option for it.
- **QUIC is CPU-bound** without UDP segmentation offload; expect far lower numbers than TCP on fast links.
- **A QUIC server cannot mirror the client's congestion algorithm** (it is fixed before the handshake), and its TOS marking is per socket, so with several clients the latest request wins.
- **UDP has no flow control:** at high `--bandwidth` the receiver may drop packets; that is what the loss figure shows.
- **The UDP server relies on `SO_REUSEPORT`** to give each stream its own socket on the listening port (Linux and macOS both route a connected peer's datagrams to its dedicated socket).
- **No authentication or access control** (see [Security notes](#security-notes)).
- Measurements were developed and verified on loopback and in a Linux container; behavior across real networks is yours to validate for your path.

## Contributing

Issues and pull requests are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) for the development setup, the checklist for adding an option, where each kind of test belongs, and the PR guidelines. In short: `just check` (format, clippy, dependency checks, tests) must pass, and new behavior needs a test and a README/[CHANGELOG](CHANGELOG.md) update.

## License

gauge is released under the [MIT License](LICENSE).
