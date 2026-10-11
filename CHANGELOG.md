# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

First public version (`0.1.0`). When it is tagged, rename this heading to
`## [0.1.0] - YYYY-MM-DD` and start a fresh `[Unreleased]` section above it.

### Added

- **Three transports:** TCP, UDP and QUIC (via `quinn`, TLS 1.3 with a
  throw-away self-signed certificate; the client does not verify it).
- **Directions:** TX, RX (`--reverse`) and bidirectional (`--bidir`) for every
  transport, with parallel streams (`-P`) correlated by a session id.
- **Terminal UI** with four screens — Live Test, Config, History, Server
  Mode — including throughput gauges and sparklines, a per-stream table, a
  CLI preview that mirrors the Config screen, and an event log.
- **Command line:** `gauge client` and `gauge server` on the same engine as
  the TUI, with `--json` output and `--one-off` servers.
- **Measurements:** UDP loss and RFC 3550 jitter measured by the receiving
  side (server, client or both); TCP retransmits, congestion window and an
  estimated loss rate read from the kernel (`TCP_INFO` on Linux,
  `TCP_CONNECTION_INFO` on macOS); QUIC retransmits, congestion window and
  exact loss from quinn's connection statistics.
- **Test options:** `--window`, `--buffer`, `--mtu`, `--bandwidth` (UDP
  pacing), `--tos` (IP TOS / DSCP, also applied to the server's sends),
  `--congestion` (`cubic`, `bbr`, `reno`; TCP on Linux only, QUIC everywhere),
  `--omit` (exclude the first 3 s from the average and peak) and
  `--interval` (1–60 s; the server follows the client's choice).
- **Exact accounting:** the shorter last sample at the end of a test is
  reported, so the live samples add up to the true byte total.
- **History:** completed runs are saved (last 50) with the options they used,
  TX/RX charts and JSON export.
- **Event log file:** every Live Test and Server Mode log line is also
  appended to `gauge.log`.
- **Configuration file** (`config.toml`) with defaults for the CLI and the
  TUI; precedence is command-line flag > config file > built-in default.
  `Ctrl+S` on the Config and Server Mode screens saves the current values.
- **Tests:** 139 tests — state machine, engine integration over real
  loopback sockets, rendered screens (ratatui `TestBackend`) and end-to-end
  tests that drive the real binary through a pseudo-terminal.
- **CI** on Linux and macOS: clippy with `-D warnings`, the full test suite
  and a release build; plus a format check (`cargo fmt --check`) and a
  dependency policy check (`cargo deny`: licenses, sources, advisories).
- **Developer tooling:** `rustfmt.toml`, `.editorconfig`, a `justfile`
  (`just check`, `just linux`, `just demo` …), `deny.toml` and weekly
  Dependabot updates.
- **Project documentation and templates:** `README.md`, `CONTRIBUTING.md`
  (including commit-message conventions), `CHANGELOG.md`, `SECURITY.md`, the
  MIT `LICENSE`, issue forms and a pull-request template.

### Changed

- UDP clients run the sending and receiving halves of a bidirectional or
  reverse test as separate tasks, so a busy sender can no longer starve its own
  receiver (single-stream bidirectional UDP loss on loopback went from about
  2 % to 0 %).
- `--window` now also applies to UDP: it enlarges the socket send/receive
  buffers (never below the OS default; the kernel may cap it). The TUI's window
  list gained 1M, 2M, 4M and 8M.

- The UDP server now gives every stream its **own socket and task**
  (`SO_REUSEPORT`, connected to the client) instead of receiving all streams in
  one loop, and UDP receive loops drain queued datagrams in bursts. On
  loopback (release build) four unthrottled TX streams went from about 1 to
  about 27 Gbit/s received at the server with loss down from ~88 % to ~6 %, and
  a reverse test from ~4 to ~28 Gbit/s. The server also applies a client's TOS
  request per stream now.

### Notes

- gauge uses its own wire protocol and is **not compatible with iperf3**.
- Supported platforms are Linux and macOS; the minimum supported Rust version
  is 1.88.

[Unreleased]: #unreleased
