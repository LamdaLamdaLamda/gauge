//! Non-interactive CLI mode: `gauge client ...` / `gauge server ...`.
//!
//! Runs the exact same real network engine (`crate::net`) the TUI drives —
//! same protocol, same events, same History file — just printed to stdout
//! instead of rendered. No Ratatui, no alternate screen; plain text (or
//! JSON with `--json`) so it composes with `|`, `tee`, cron, CI, etc.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use clap::{Args, Parser, Subcommand};
use serde::Serialize;

use crate::app::OMIT_SECS;
use crate::config;
use crate::net::{
    self, ClientParams, Direction, Flow, NetCommand, NetEvent, ProbeVerdict, Proto, ServerParams,
};
use crate::store::{self, HistoryRun, RateSearchSummary};
use crate::util::{fmt_rate, now_hms};

/// gauge — a network performance tester (TCP/UDP/QUIC, client & server).
///
/// Run with no arguments to launch the interactive TUI instead.
#[derive(Parser)]
#[command(name = "gauge", version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run a throughput test against a gauge server.
    Client(ClientArgs),
    /// Listen for and run tests from gauge clients.
    Server(ServerArgs),
}

/// Every field here that isn't a plain on/off flag is `Option` and has no
/// `default_value` — an absent flag falls through to the config file
/// (`~/.config/gauge/config.toml`'s `[client]` section, or wherever
/// `dirs::config_dir()` points; see `crate::config`) and only then to the
/// hardcoded fallback noted in each doc comment. The on/off flags
/// (`--udp`/`--reverse`/`--bidir`/`--json`) OR together with the config
/// file's value — the config file can turn one on by default, but there's
/// no `--no-udp` to force it back off from the command line.
#[derive(Args)]
pub struct ClientArgs {
    /// Server host or IP to connect to. [config default: client.host,
    /// otherwise "127.0.0.1"]
    #[arg(long)]
    host: Option<String>,
    /// Server port. [config default: client.port, otherwise 5201]
    #[arg(short, long)]
    port: Option<u16>,
    /// Test duration in seconds. [config default: client.duration_secs,
    /// otherwise 10]
    #[arg(short = 't', long = "duration")]
    duration: Option<u64>,
    /// Number of parallel streams. [config default: client.parallel,
    /// otherwise 1]
    #[arg(short = 'P', long)]
    parallel: Option<u8>,
    /// Use UDP instead of TCP.
    #[arg(short, long, conflicts_with = "quic")]
    udp: bool,
    /// Use QUIC (encrypted, over UDP) instead of TCP. The server's
    /// certificate is not verified. Supports --reverse/--bidir, --window
    /// (flow-control windows), --buffer, --congestion (cubic/bbr/reno, on
    /// every platform) and --tos; --bandwidth and --mtu don't apply.
    #[arg(long)]
    quic: bool,
    /// Reverse the direction: server sends, client receives.
    #[arg(short = 'R', long, conflicts_with = "bidir")]
    reverse: bool,
    /// Both directions at once, over the same connection.
    #[arg(long, conflicts_with = "reverse")]
    bidir: bool,
    /// Target bitrate for UDP, e.g. "100M" or "1G" — unthrottled if omitted
    /// (and not set in the config file). Ignored for TCP, which relies on
    /// congestion control instead. "auto" searches for the highest rate that
    /// arrives without loss (probes of 2 s that double until packets are
    /// lost, then narrow in); -t is not used then.
    #[arg(short, long)]
    bandwidth: Option<String>,
    /// TCP window/buffer size applied to the connecting socket (SO_SNDBUF/
    /// SO_RCVBUF), e.g. "256K", "1M", or "auto" to leave the OS default.
    /// For QUIC it sets the flow-control windows; for UDP it enlarges the
    /// socket send/receive buffers (never below the OS default).
    /// [config default: client.window,
    /// otherwise "256K"]
    #[arg(long)]
    window: Option<String>,
    /// MTU used to size UDP datagram payloads (mtu - 28 for the IPv4+UDP
    /// header). UDP only — TCP has no portable socket option to force it.
    /// [config default: client.mtu, otherwise 9000]
    #[arg(long)]
    mtu: Option<u32>,
    /// TCP write/read buffer size in bytes — the size of each write()/
    /// read() call (like iperf3's -l), not the kernel socket buffer (see
    /// --window for that). TCP and QUIC. [config default: client.buffer,
    /// otherwise 131072]
    #[arg(long)]
    buffer: Option<String>,
    /// Exclude the first 3 seconds (TCP ramp-up) from the average/peak
    /// throughput reported at test end. Total bytes transferred are always
    /// exact regardless of this flag — only the rate summary is affected.
    #[arg(long)]
    omit: bool,
    /// Seconds between live interval reports, 1-60. [config default:
    /// client.interval_secs, otherwise 1]
    #[arg(short = 'i', long)]
    interval: Option<u64>,
    /// Marks outgoing packets with this IP TOS/DSCP byte, decimal or hex —
    /// e.g. "0xB8" (EF, voice) or "0x88" (AF41, video). Also applied to
    /// the server's sends in --reverse/--bidir. [config default:
    /// client.tos, otherwise unset = OS default]
    #[arg(long)]
    tos: Option<String>,
    /// Congestion-control algorithm, e.g. "cubic", "bbr", "reno". For TCP
    /// this is Linux only (macOS has no per-socket choice); QUIC does it in
    /// user space on every platform. The test fails rather than silently
    /// using a different algorithm. [config default:
    /// client.congestion, otherwise the OS default]
    #[arg(long)]
    congestion: Option<String>,
    /// Print the final result as JSON instead of human-readable text.
    #[arg(short = 'J', long)]
    json: bool,
}

#[derive(Args)]
pub struct ServerArgs {
    /// Address to bind to. [config default: server.bind, otherwise
    /// "0.0.0.0"]
    #[arg(long)]
    bind: Option<String>,
    /// Port to listen on. [config default: server.port, otherwise 5201]
    #[arg(short, long)]
    port: Option<u16>,
    /// Accept UDP tests instead of TCP.
    #[arg(short, long, conflicts_with = "quic")]
    udp: bool,
    /// Accept QUIC tests instead of TCP (self-signed certificate).
    #[arg(long)]
    quic: bool,
    /// TCP window/buffer size applied to the listening socket (SO_SNDBUF/
    /// SO_RCVBUF), e.g. "256K", "1M", or "auto" to leave the OS default.
    /// For QUIC it sets the flow-control windows; for UDP it enlarges the
    /// socket send/receive buffers (never below the OS default).
    /// [config default: server.window,
    /// otherwise "256K"]
    #[arg(long)]
    window: Option<String>,
    /// Print each event as one JSON line instead of human-readable text.
    #[arg(short = 'J', long)]
    json: bool,
    /// Stop after the first completed test.
    #[arg(short = '1', long = "one-off")]
    one_off: bool,
}

fn new_session_id() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

/// Sets `stop` on Ctrl+C so an in-flight client test finishes gracefully
/// (and still prints its summary) rather than being killed outright.
fn watch_ctrlc_soft(stop: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
            return;
        };
        rt.block_on(async {
            let _ = tokio::signal::ctrl_c().await;
        });
        stop.store(true, Ordering::Relaxed);
    });
}

/// The server has no "final summary" to wait for — Ctrl+C just stops it now.
fn watch_ctrlc_hard() {
    std::thread::spawn(|| {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
            return;
        };
        rt.block_on(async {
            let _ = tokio::signal::ctrl_c().await;
        });
        eprintln!("\ninterrupted");
        std::process::exit(130);
    });
}

pub fn run_client(args: ClientArgs) -> i32 {
    // Merge order: explicit CLI flag > config file > hardcoded fallback.
    // See `ClientArgs`'s doc comment for the on/off-flag OR-merge caveat.
    let cfg = config::load().client;
    let host = args.host.or(cfg.host).unwrap_or_else(|| "127.0.0.1".into());
    let port = args.port.or(cfg.port).unwrap_or(5201);
    let duration = args.duration.or(cfg.duration_secs).unwrap_or(10);
    let parallel = args.parallel.or(cfg.parallel).unwrap_or(1);
    let udp = args.udp || cfg.udp.unwrap_or(false);
    let quic = args.quic || cfg.quic.unwrap_or(false);
    let reverse = args.reverse || cfg.reverse.unwrap_or(false);
    let bidir = args.bidir || cfg.bidir.unwrap_or(false);
    let bandwidth = args.bandwidth.or(cfg.bandwidth);
    let window = args.window.or(cfg.window).unwrap_or_else(|| "256K".into());
    let mtu = args.mtu.or(cfg.mtu).unwrap_or(9000);
    let buffer = args.buffer.or(cfg.buffer).unwrap_or_else(|| "131072".into());
    let omit = args.omit || cfg.omit.unwrap_or(false);
    let json = args.json || cfg.json.unwrap_or(false);
    let interval = args.interval.or(cfg.interval_secs).unwrap_or(1);
    if !(1..=crate::app::MAX_INTERVAL_SECS).contains(&interval) {
        eprintln!(
            "error: --interval must be a whole number of seconds between 1 and {}",
            crate::app::MAX_INTERVAL_SECS
        );
        return 1;
    }
    let tos = match args.tos.as_deref() {
        Some(s) => match net::parse_tos(s) {
            Some(v) => Some(v),
            None => {
                eprintln!(
                    "error: --tos must be a number 0-255, decimal or 0x-prefixed hex (got \"{s}\")"
                );
                return 1;
            }
        },
        None => cfg.tos,
    };
    let congestion = args.congestion.or(cfg.congestion).filter(|c| c != "default");
    if !udp && !quic && congestion.is_some() && !net::CONGESTION_SETTABLE {
        eprintln!(
            "error: --congestion is only supported on Linux; this platform picks the algorithm system-wide"
        );
        return 1;
    }

    let proto = if quic {
        Proto::Quic
    } else if udp {
        Proto::Udp
    } else {
        Proto::Tcp
    };
    let proto_label = match proto {
        Proto::Tcp => "TCP",
        Proto::Udp => "UDP",
        Proto::Quic => "QUIC",
    };
    let direction = if bidir {
        Direction::Bidir
    } else if reverse {
        Direction::Rx
    } else {
        Direction::Tx
    };
    let dir_label = match direction {
        Direction::Tx => "TX",
        Direction::Rx => "RX",
        Direction::Bidir => "bidir",
    };
    let rate_search = bandwidth.as_deref().is_some_and(net::is_auto_bandwidth);
    if rate_search && proto != Proto::Udp {
        eprintln!("error: -b auto (rate search) only works with --udp");
        return 1;
    }
    let bandwidth_bps =
        if proto == Proto::Udp { bandwidth.as_deref().and_then(net::parse_bps) } else { None };
    let stream_count = parallel.max(1);

    let (cmd_tx, mut evt_rx) = net::spawn_runtime();
    let stop = Arc::new(AtomicBool::new(false));
    watch_ctrlc_soft(stop.clone());

    let congestion_name = congestion.clone();
    let params = ClientParams {
        host: host.clone(),
        port,
        duration_secs: duration,
        proto,
        stream_count,
        direction,
        bandwidth_bps,
        window_bytes: net::parse_window_bytes(&window),
        mtu,
        buffer_bytes: net::parse_window_bytes(&buffer),
        interval_secs: interval,
        tos,
        congestion,
        rate_search,
        session_id: new_session_id(),
    };
    let _ = cmd_tx.send(NetCommand::StartClient(params, stop));

    if !json {
        println!(
            "gauge client → {host}:{port}  ({proto_label} · {stream_count} stream(s) · {dir_label} · {duration}s)"
        );
        if rate_search {
            println!("note: {}", crate::app::RATE_SEARCH_NOTE);
        } else if proto == Proto::Udp && bandwidth_bps.is_none() {
            println!("note: {}", crate::app::UNTHROTTLED_UDP_NOTE);
        }
    }

    // Set when a rate search (-b auto) finishes.
    let mut search_summary: Option<RateSearchSummary> = None;
    let mut per_stream_tx = vec![0.0f64; stream_count as usize];
    let mut per_stream_rx = vec![0.0f64; stream_count as usize];
    // Cumulative per the kernel, so the latest value per stream is already
    // the running total — summed across streams for the overall count.
    let mut per_stream_retrans = vec![0u64; stream_count as usize];
    let mut peak_tx_gbps = 0.0f64;
    let mut peak_rx_gbps = 0.0f64;
    // The same peaks over only the post-ramp-up samples, used when omit applies.
    let mut peak_tx_after_omit = 0.0f64;
    let mut peak_rx_after_omit = 0.0f64;
    // Exact byte totals, accumulated from each flow's `ClientStreamFinished`
    // (not from interval sampling, which misses the last fractional second
    // of data between the final periodic report and the stream ending).
    let mut tx_finished_bytes = 0u64;
    let mut rx_finished_bytes = 0u64;
    // "--omit": bytes/peak seen after the first OMIT_SECS, tracked
    // separately from the totals above (which always stay exact/complete)
    // — see app.rs's copy of this same constant and logic for the TUI side.
    let mut tx_bytes_after_omit = 0u64;
    let mut rx_bytes_after_omit = 0u64;
    // End of the previous aggregate sample per flow, for the printed span.
    let mut last_sample_secs = [0u64; 2];
    let mut tx_chart: Vec<(f64, f64)> = Vec::new();
    let mut rx_chart: Vec<(f64, f64)> = Vec::new();
    let mut rtt_ms = 0.0f64;
    let mut finished = 0u8;
    let expected_finishes = stream_count * direction.flow_count();
    let start = Instant::now();

    while let Some(evt) = evt_rx.blocking_recv() {
        match evt {
            NetEvent::ClientStreamConnected { stream_index, rtt_ms: rtt } => {
                if stream_index == 0 {
                    rtt_ms = rtt;
                    if !json {
                        println!("connected — handshake {rtt:.2}ms");
                    }
                }
            }
            NetEvent::ClientStreamInterval { stream_index, flow, interval_bytes, stats } => {
                let per_stream =
                    if flow == Flow::Tx { &mut per_stream_tx } else { &mut per_stream_rx };
                if let Some(slot) = per_stream.get_mut(stream_index as usize) {
                    *slot = stats.gbps;
                }
                if let (Some(r), Some(slot)) =
                    (stats.retransmits, per_stream_retrans.get_mut(stream_index as usize))
                {
                    *slot = r;
                }
                let past_omit_window = !omit || stats.elapsed_secs > OMIT_SECS;
                if past_omit_window {
                    match flow {
                        Flow::Tx => tx_bytes_after_omit += interval_bytes,
                        Flow::Rx => rx_bytes_after_omit += interval_bytes,
                    }
                }
                // Stream 0 drives the printed/aggregate line — otherwise N
                // streams would each print their own ~1s tick.
                if stream_index == 0 {
                    match flow {
                        Flow::Tx => {
                            let aggregate: f64 = per_stream_tx.iter().sum();
                            peak_tx_gbps = peak_tx_gbps.max(aggregate);
                            if past_omit_window {
                                peak_tx_after_omit = peak_tx_after_omit.max(aggregate);
                            }
                            tx_chart.push((stats.elapsed_secs as f64, aggregate));
                            let from_secs =
                                std::mem::replace(&mut last_sample_secs[0], stats.elapsed_secs);
                            if !json && !rate_search {
                                println!(
                                    "[sum] {:>3}-{:>3}s  TX {aggregate:>7.2} Gbit/s",
                                    from_secs, stats.elapsed_secs
                                );
                            }
                        }
                        Flow::Rx => {
                            let aggregate: f64 = per_stream_rx.iter().sum();
                            peak_rx_gbps = peak_rx_gbps.max(aggregate);
                            if past_omit_window {
                                peak_rx_after_omit = peak_rx_after_omit.max(aggregate);
                            }
                            rx_chart.push((stats.elapsed_secs as f64, aggregate));
                            let from_secs =
                                std::mem::replace(&mut last_sample_secs[1], stats.elapsed_secs);
                            if !json && !rate_search {
                                // UDP receivers measure these themselves (stream 0's, not an aggregate).
                                let quality = match (stats.loss_pct, stats.jitter_ms) {
                                    (Some(loss), Some(jitter)) => {
                                        format!("  loss {loss:.2}%  jitter {jitter:.2}ms")
                                    }
                                    _ => String::new(),
                                };
                                println!(
                                    "[sum] {:>3}-{:>3}s  RX {aggregate:>7.2} Gbit/s{quality}",
                                    from_secs, stats.elapsed_secs
                                );
                            }
                        }
                    }
                }
            }
            NetEvent::ClientStreamFinished { flow, total_bytes, .. } => {
                match flow {
                    Flow::Tx => tx_finished_bytes += total_bytes,
                    Flow::Rx => rx_finished_bytes += total_bytes,
                }
                finished += 1;
                if finished >= expected_finishes {
                    break;
                }
            }
            NetEvent::RateProbe(p) => {
                if !json {
                    let mark = match p.verdict {
                        ProbeVerdict::Pass => "ok",
                        ProbeVerdict::Fail => "LOSS",
                        ProbeVerdict::SenderLimited => "sender limit",
                    };
                    println!(
                        "[probe {:>2}] target {:>13} → delivered {:>13}  loss {:>6.2}%  jitter {:.2}ms  {mark}",
                        p.index,
                        fmt_rate(p.target_bps),
                        fmt_rate(p.delivered_bps),
                        p.loss_pct,
                        p.jitter_ms
                    );
                }
            }
            NetEvent::RateSearchDone { best, probes, reason } => {
                search_summary = Some(RateSearchSummary {
                    best_bps: best.map(|b| b.delivered_bps),
                    probes,
                    reason: crate::app::search_reason_text(reason).to_string(),
                });
            }
            NetEvent::ClientUdpReport {
                stream_index,
                flow,
                received,
                lost,
                loss_pct,
                jitter_ms,
            } => {
                if !json {
                    let who = match flow {
                        Flow::Tx => "TX (reported by the server)",
                        Flow::Rx => "RX",
                    };
                    println!(
                        "[#{} {who}] loss {loss_pct:.2}% ({lost} of {} packets), jitter {jitter_ms:.2}ms",
                        stream_index + 1,
                        received + lost
                    );
                }
            }
            NetEvent::ClientError { stream_index, message } => {
                let who = stream_index.map(|i| format!("stream #{}: ", i + 1)).unwrap_or_default();
                eprintln!("error: {who}{message}");
                return 1;
            }
            _ => {}
        }
    }

    let elapsed = start.elapsed().as_secs_f64().max(0.001);
    let tx_total_gb = tx_finished_bytes as f64 / 1e9;
    let rx_total_gb = rx_finished_bytes as f64 / 1e9;
    // "--omit": average the *rate* over only the bytes/time seen after the
    // ramp-up window — tx_total_gb/rx_total_gb (and the "GB total" printed
    // below) always stay the true, complete totals regardless.
    // A test that ends within the omit window (a short -t, or Ctrl-C) would
    // have nothing left to average, so omit is then ignored.
    let omit_applies = omit && elapsed > OMIT_SECS as f64;
    let (avg_tx_gbps, avg_rx_gbps, peak_tx_gbps, peak_rx_gbps) = if omit_applies {
        let effective_elapsed = (elapsed - OMIT_SECS as f64).max(0.001);
        (
            tx_bytes_after_omit as f64 * 8.0 / 1e9 / effective_elapsed,
            rx_bytes_after_omit as f64 * 8.0 / 1e9 / effective_elapsed,
            peak_tx_after_omit,
            peak_rx_after_omit,
        )
    } else {
        (tx_total_gb * 8.0 / elapsed, rx_total_gb * 8.0 / elapsed, peak_tx_gbps, peak_rx_gbps)
    };
    let summary = match direction {
        Direction::Bidir => format!("↑{avg_tx_gbps:.2} / ↓{avg_rx_gbps:.2} Gbit/s avg"),
        Direction::Rx => format!("↓{avg_rx_gbps:.2} Gbit/s avg · peak {peak_rx_gbps:.2}"),
        Direction::Tx => format!("↑{avg_tx_gbps:.2} Gbit/s avg · peak {peak_tx_gbps:.2}"),
    };
    let summary = match &search_summary {
        Some(found) => match found.best_bps {
            Some(b) => format!("max loss-free {} · {} probes", fmt_rate(b), found.probes),
            None => format!("no loss-free rate found · {} probes", found.probes),
        },
        None => summary,
    };
    let search_best_gbps = search_summary.as_ref().and_then(|f| f.best_bps).map(|b| b as f64 / 1e9);
    let (avg_tx_gbps, peak_tx_gbps) = match (&search_summary, search_best_gbps) {
        (Some(_), best) => (best.unwrap_or(0.0), best.unwrap_or(0.0)),
        _ => (avg_tx_gbps, peak_tx_gbps),
    };
    let run = HistoryRun {
        target: format!("{host}:{port}"),
        tag: format!("{proto_label} {dir_label} ×{stream_count}"),
        tag_color_idx: match proto {
            Proto::Tcp => 0,
            Proto::Udp => 1,
            Proto::Quic => 2,
        },
        summary,
        time: now_hms().chars().take(5).collect(),
        peak_gbps: peak_tx_gbps,
        avg_gbps: avg_tx_gbps,
        peak_rx_gbps: (direction != Direction::Tx).then_some(peak_rx_gbps),
        avg_rx_gbps: (direction != Direction::Tx).then_some(avg_rx_gbps),
        rtt_ms,
        duration_secs: elapsed.round() as u64,
        streams: stream_count,
        window,
        mtu,
        retransmits: per_stream_retrans.iter().sum(),
        chart: tx_chart,
        chart_rx: rx_chart,
        rate_search: search_summary.clone(),
        settings: Some(crate::store::RunSettings {
            buffer_bytes: if proto != Proto::Udp { net::parse_window_bytes(&buffer) } else { None },
            bandwidth_bps,
            omit: omit_applies,
            interval_secs: interval,
            tos,
            congestion: if proto != Proto::Udp { congestion_name } else { None },
            rate_search,
        }),
    };

    if json {
        match serde_json::to_string_pretty(&run) {
            Ok(s) => println!("{s}"),
            Err(e) => eprintln!("failed to encode result as JSON: {e}"),
        }
    } else if let Some(found) = &search_summary {
        println!("────────────────────────────────────────");
        match found.best_bps {
            Some(b) => println!(
                "highest loss-free rate: {} (loss ≤ {}%), after {} probes — {}",
                fmt_rate(b),
                net::LOSS_LIMIT_PCT,
                found.probes,
                found.reason
            ),
            None => {
                println!("no loss-free rate found after {} probes — {}", found.probes, found.reason)
            }
        }
    } else {
        println!("────────────────────────────────────────");
        match direction {
            Direction::Bidir => println!(
                "TX avg {avg_tx_gbps:.2} / RX avg {avg_rx_gbps:.2} Gbit/s over {}s · {tx_total_gb:.2} / {rx_total_gb:.2} GB total",
                run.duration_secs
            ),
            Direction::Rx => println!(
                "avg {avg_rx_gbps:.2} Gbit/s · peak {peak_rx_gbps:.2} Gbit/s over {}s · {rx_total_gb:.2} GB total",
                run.duration_secs
            ),
            Direction::Tx => println!(
                "avg {avg_tx_gbps:.2} Gbit/s · peak {peak_tx_gbps:.2} Gbit/s over {}s · {tx_total_gb:.2} GB total",
                run.duration_secs
            ),
        }
        if proto == Proto::Tcp {
            println!("retransmits: {}", run.retransmits);
        }
        if omit_applies {
            println!("(first {OMIT_SECS}s omitted from the average/peak above)");
        } else if omit {
            println!("(--omit ignored: the test was not longer than {OMIT_SECS}s)");
        }
    }

    store::record_run(run);
    0
}

#[derive(Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum CliEvent<'a> {
    Listening {
        bind: &'a str,
    },
    Connected {
        addr: &'a str,
        stream: u8,
        of: u8,
    },
    Interval {
        addr: &'a str,
        stream: u8,
        flow: &'a str,
        gbps: f64,
        loss_pct: Option<f64>,
        jitter_ms: Option<f64>,
    },
    Done {
        addr: &'a str,
        stream: u8,
        flow: &'a str,
        avg_gbps: f64,
    },
}

fn flow_label(flow: Flow) -> &'static str {
    match flow {
        Flow::Tx => "tx",
        Flow::Rx => "rx",
    }
}

fn print_json_line(evt: &CliEvent) {
    if let Ok(s) = serde_json::to_string(evt) {
        println!("{s}");
    }
}

pub fn run_server(args: ServerArgs) -> i32 {
    // Same merge order as run_client: explicit CLI flag > config file >
    // hardcoded fallback.
    let cfg = config::load().server;
    let bind = args.bind.or(cfg.bind).unwrap_or_else(|| "0.0.0.0".into());
    let port = args.port.or(cfg.port).unwrap_or(5201);
    let udp = args.udp || cfg.udp.unwrap_or(false);
    let quic = args.quic || cfg.quic.unwrap_or(false);
    let window = args.window.or(cfg.window).unwrap_or_else(|| "256K".into());
    let json = args.json || cfg.json.unwrap_or(false);
    let one_off = args.one_off || cfg.one_off.unwrap_or(false);

    let proto = if quic {
        Proto::Quic
    } else if udp {
        Proto::Udp
    } else {
        Proto::Tcp
    };
    let proto_label = match proto {
        Proto::Tcp => "TCP",
        Proto::Udp => "UDP",
        Proto::Quic => "QUIC",
    };

    let (cmd_tx, mut evt_rx) = net::spawn_runtime();
    watch_ctrlc_hard();

    let window_bytes = net::parse_window_bytes(&window);
    let _ = cmd_tx.send(NetCommand::StartServer(ServerParams {
        bind: bind.clone(),
        port,
        proto,
        window_bytes,
    }));

    // Later events only carry session_id/stream_index — this is purely so
    // they can still be printed with the address they belong to.
    let mut sessions: HashMap<u64, HashMap<u8, String>> = HashMap::new();
    let mut a_session_finished = false;

    while let Some(evt) = evt_rx.blocking_recv() {
        match evt {
            NetEvent::ServerListening { bind } => {
                if json {
                    print_json_line(&CliEvent::Listening { bind: &bind });
                } else {
                    println!("gauge server listening on {bind}  ({proto_label})");
                    println!("press Ctrl+C to stop\n");
                }
            }
            NetEvent::ServerListenError(msg) => {
                eprintln!("failed to start: {msg}");
                return 1;
            }
            NetEvent::ServerStreamConnected {
                session_id,
                stream_index,
                stream_count,
                addr,
                ..
            } => {
                sessions.entry(session_id).or_default().insert(stream_index, addr.clone());
                if json {
                    print_json_line(&CliEvent::Connected {
                        addr: &addr,
                        stream: stream_index,
                        of: stream_count,
                    });
                } else {
                    println!("[connect] {addr} — stream {}/{stream_count}", stream_index + 1);
                }
            }
            NetEvent::ServerStreamInterval { session_id, stream_index, flow, stats } => {
                let addr = sessions
                    .get(&session_id)
                    .and_then(|m| m.get(&stream_index))
                    .cloned()
                    .unwrap_or_default();
                if json {
                    print_json_line(&CliEvent::Interval {
                        addr: &addr,
                        stream: stream_index,
                        flow: flow_label(flow),
                        gbps: stats.gbps,
                        loss_pct: stats.loss_pct,
                        jitter_ms: stats.jitter_ms,
                    });
                } else {
                    let extra = match (stats.loss_pct, stats.jitter_ms) {
                        (Some(loss), Some(jitter)) => {
                            format!("  loss {loss:.2}%  jitter {jitter:.2}ms")
                        }
                        _ => String::new(),
                    };
                    println!(
                        "[#{} {}] {addr}  {:.2} Gbit/s{extra}",
                        stream_index + 1,
                        flow_label(flow),
                        stats.gbps
                    );
                }
            }
            NetEvent::ServerStreamFinished { session_id, stream_index, flow, avg_gbps } => {
                let addr = sessions
                    .get(&session_id)
                    .and_then(|m| m.get(&stream_index))
                    .cloned()
                    .unwrap_or_default();
                if json {
                    print_json_line(&CliEvent::Done {
                        addr: &addr,
                        stream: stream_index,
                        flow: flow_label(flow),
                        avg_gbps,
                    });
                } else {
                    println!("[done {}] {addr} — avg {avg_gbps:.2} Gbit/s", flow_label(flow));
                }
            }
            NetEvent::ServerStreamDisconnected { session_id, stream_index } => {
                if let Some(streams) = sessions.get_mut(&session_id) {
                    streams.remove(&stream_index);
                    if streams.is_empty() {
                        sessions.remove(&session_id);
                        a_session_finished = true;
                    }
                }
                if one_off && a_session_finished {
                    if !json {
                        println!("\none-off: test complete, shutting down");
                    }
                    break;
                }
            }
            NetEvent::ClientStreamConnected { .. }
            | NetEvent::ClientStreamInterval { .. }
            | NetEvent::ClientStreamFinished { .. }
            | NetEvent::ClientUdpReport { .. }
            | NetEvent::RateProbe(_)
            | NetEvent::RateSearchDone { .. }
            | NetEvent::ClientError { .. } => {} // this process never starts a client
        }
    }
    0
}
