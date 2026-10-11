//! UDP rate search (`-b auto`): finds the highest rate a path sustains
//! **without loss**.
//!
//! UDP has no feedback, so a sender that is not told a rate just floods and
//! the "loss" it then sees is its own overload. The search replaces that
//! guess with measurements: it runs a series of short probes at increasing
//! rates (doubling) until one loses packets, then bisects between the last
//! loss-free rate and the first lossy one. Loss is whatever the receiving
//! side counted — the server tells the sender in a `UdpSummary` — so the
//! search works in every direction.
//!
//! [`Searcher`] is the pure decision logic (no I/O, easy to test); [`run`]
//! drives real probes with it.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use super::{ClientParams, Direction, Flow, NetEvent, Proto, client};

/// A probe passes if the receiver counted at most this much loss.
pub const LOSS_LIMIT_PCT: f64 = 0.5;
/// A probe whose sender managed less than this share of the target rate
/// tells us about the sender, not the path.
const SENDER_SHORTFALL: f64 = 0.9;
/// Stop bisecting once the lossy rate is within this factor of the clean one.
const TOLERANCE: f64 = 0.05;
const MIN_BPS: u64 = 1_000_000;
const MAX_PROBES: u32 = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The target rate arrived without (meaningful) loss.
    Pass,
    /// The receiver lost more than `LOSS_LIMIT_PCT`.
    Fail,
    /// No loss, but the sender couldn't even reach the target — the machine,
    /// not the path, is the limit.
    SenderLimited,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// Narrowed to within `TOLERANCE` of the loss-free limit.
    Converged,
    /// This machine can't send faster; the path may well take more.
    SenderLimited,
    /// Reached the highest rate the search tries.
    MaxRate,
    /// Even the lowest rate lost packets.
    NothingFits,
    /// Ran out of probes before converging (noisy path).
    ProbeLimit,
    /// Stopped by the user.
    Stopped,
}

/// What a search starts from and bounds itself by. Tests shrink these.
#[derive(Debug, Clone, Copy)]
pub struct SearchSettings {
    pub start_bps: u64,
    pub max_bps: u64,
    pub probe_secs: u64,
}

impl Default for SearchSettings {
    fn default() -> Self {
        SearchSettings { start_bps: 50_000_000, max_bps: 100_000_000_000, probe_secs: 2 }
    }
}

/// Decides which rate to probe next. Pure state machine.
#[derive(Debug)]
pub struct Searcher {
    max_bps: u64,
    best_pass: Option<u64>,
    lowest_fail: Option<u64>,
    next: u64,
    probes: u32,
    stop: Option<StopReason>,
}

impl Searcher {
    pub fn new(start_bps: u64, max_bps: u64) -> Self {
        Searcher {
            max_bps,
            best_pass: None,
            lowest_fail: None,
            next: start_bps.clamp(MIN_BPS, max_bps),
            probes: 0,
            stop: None,
        }
    }

    /// The next rate to probe, or `None` once the search is over.
    pub fn next_rate(&self) -> Option<u64> {
        self.stop.is_none().then_some(self.next)
    }

    pub fn stop_reason(&self) -> Option<StopReason> {
        self.stop
    }

    /// Feeds back what the probe at `rate` found.
    pub fn record(&mut self, rate: u64, verdict: Verdict) {
        self.probes += 1;
        match verdict {
            Verdict::SenderLimited => {
                self.stop = Some(StopReason::SenderLimited);
                return;
            }
            Verdict::Pass => {
                self.best_pass = Some(self.best_pass.map_or(rate, |p| p.max(rate)));
                // A pass above an earlier failure means that failure was noise.
                if self.lowest_fail.is_some_and(|f| rate >= f) {
                    self.lowest_fail = None;
                }
                if rate >= self.max_bps {
                    self.stop = Some(StopReason::MaxRate);
                    return;
                }
                self.next = match self.lowest_fail {
                    None => rate.saturating_mul(2).min(self.max_bps),
                    Some(fail) => rate + (fail - rate) / 2,
                };
            }
            Verdict::Fail => {
                self.lowest_fail = Some(self.lowest_fail.map_or(rate, |f| f.min(rate)));
                // A failure at or below an earlier pass means that pass was luck.
                if self.best_pass.is_some_and(|p| rate <= p) {
                    self.best_pass = None;
                }
                match self.best_pass {
                    None => {
                        if rate / 2 < MIN_BPS {
                            self.stop = Some(StopReason::NothingFits);
                            return;
                        }
                        self.next = rate / 2;
                    }
                    Some(pass) => self.next = pass + (rate - pass) / 2,
                }
            }
        }
        if let (Some(pass), Some(fail)) = (self.best_pass, self.lowest_fail)
            && fail as f64 <= pass as f64 * (1.0 + TOLERANCE)
        {
            self.stop = Some(StopReason::Converged);
        } else if self.probes >= MAX_PROBES {
            self.stop = Some(StopReason::ProbeLimit);
        }
    }
}

/// The outcome of one probe, as reported to the UI/CLI.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProbeResult {
    pub index: u32,
    /// Aggregate target rate over all streams.
    pub target_bps: u64,
    /// What the senders actually put on the wire.
    pub offered_bps: u64,
    /// What the receivers counted.
    pub delivered_bps: u64,
    pub loss_pct: f64,
    pub jitter_ms: f64,
    pub verdict: Verdict,
}

/// Per-flow totals of one probe, summed over its streams.
#[derive(Default, Clone, Copy)]
struct FlowTally {
    received: u64,
    lost: u64,
    jitter_ms: f64,
    reported: bool,
}

/// Turns a probe's tallies into a result. `packet_bytes` converts packet
/// counts to bytes; `flows` holds one tally per active direction.
fn evaluate(
    index: u32,
    target_bps: u64,
    secs: u64,
    packet_bytes: u64,
    flows: &[FlowTally],
) -> ProbeResult {
    let to_bps = |packets: u64| packets * packet_bytes * 8 / secs.max(1);
    // Each direction on its own; the weakest one decides.
    let offered = flows.iter().map(|f| to_bps(f.received + f.lost)).min().unwrap_or(0);
    let delivered = flows.iter().map(|f| to_bps(f.received)).min().unwrap_or(0);
    let loss_pct = flows
        .iter()
        .map(|f| {
            let expected = f.received + f.lost;
            if expected > 0 { f.lost as f64 / expected as f64 * 100.0 } else { 100.0 }
        })
        .fold(0.0, f64::max);
    let jitter_ms = flows.iter().map(|f| f.jitter_ms).fold(0.0, f64::max);
    let verdict = if loss_pct > LOSS_LIMIT_PCT {
        Verdict::Fail
    } else if (offered as f64) < target_bps as f64 * SENDER_SHORTFALL {
        Verdict::SenderLimited
    } else {
        Verdict::Pass
    };
    ProbeResult {
        index,
        target_bps,
        offered_bps: offered,
        delivered_bps: delivered,
        loss_pct,
        jitter_ms,
        verdict,
    }
}

/// Runs the whole search with the default settings.
pub async fn run(params: ClientParams, stop: Arc<AtomicBool>, tx: UnboundedSender<NetEvent>) {
    run_with(SearchSettings::default(), params, stop, tx).await;
}

pub async fn run_with(
    settings: SearchSettings,
    params: ClientParams,
    stop: Arc<AtomicBool>,
    tx: UnboundedSender<NetEvent>,
) {
    let fail = |message: String| {
        let _ = tx.send(NetEvent::ClientError { stream_index: None, message });
    };
    if params.proto != Proto::Udp {
        return fail("rate search (-b auto) only works with UDP".into());
    }
    let streams = params.stream_count.max(1);
    let packet_bytes = client::udp_packet_size(params.mtu) as u64;
    let directions: &[Flow] = match params.direction {
        Direction::Tx => &[Flow::Tx],
        Direction::Rx => &[Flow::Rx],
        Direction::Bidir => &[Flow::Tx, Flow::Rx],
    };

    let mut searcher = Searcher::new(settings.start_bps, settings.max_bps);
    let mut best: Option<ProbeResult> = None;
    let mut probes = 0u32;
    let mut sent_bytes: HashMap<(u8, Flow), u64> = HashMap::new();

    while let Some(target) = searcher.next_rate() {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let mut probe = params.clone();
        probe.bandwidth_bps = Some((target / streams as u64).max(MIN_BPS / 10));
        probe.duration_secs = settings.probe_secs;
        probe.interval_secs = 1;
        probe.session_id = params.session_id.wrapping_add(probes as u64 + 1);

        let (ptx, mut prx) = unbounded_channel::<NetEvent>();
        for stream_index in 0..streams {
            tokio::spawn(client::run_stream(
                probe.clone(),
                stream_index,
                stop.clone(),
                ptx.clone(),
            ));
        }
        drop(ptx); // the channel closes when every stream task has finished

        let mut tallies: HashMap<Flow, FlowTally> = HashMap::new();
        let offset = probes as u64 * settings.probe_secs;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(settings.probe_secs + 10);
        loop {
            let event = match tokio::time::timeout_at(deadline, prx.recv()).await {
                Ok(Some(e)) => e,
                Ok(None) => break,
                Err(_) => return fail("a probe timed out waiting for its streams".into()),
            };
            match event {
                NetEvent::ClientStreamConnected { .. } if probes > 0 => {}
                e @ NetEvent::ClientStreamConnected { .. } => {
                    let _ = tx.send(e);
                }
                NetEvent::ClientStreamInterval {
                    stream_index,
                    flow,
                    interval_bytes,
                    mut stats,
                } => {
                    stats.elapsed_secs += offset;
                    let _ = tx.send(NetEvent::ClientStreamInterval {
                        stream_index,
                        flow,
                        interval_bytes,
                        stats,
                    });
                }
                NetEvent::ClientStreamFinished { stream_index, flow, total_bytes, .. } => {
                    *sent_bytes.entry((stream_index, flow)).or_default() += total_bytes;
                }
                NetEvent::ClientUdpReport { flow, received, lost, jitter_ms, .. } => {
                    let t = tallies.entry(flow).or_default();
                    t.received += received;
                    t.lost += lost;
                    t.jitter_ms = t.jitter_ms.max(jitter_ms);
                    t.reported = true;
                }
                e @ NetEvent::ClientError { .. } => {
                    let _ = tx.send(e);
                    return;
                }
                _ => {}
            }
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }

        let flows: Vec<FlowTally> =
            directions.iter().map(|f| tallies.get(f).copied().unwrap_or_default()).collect();
        if flows.iter().any(|f| !f.reported) {
            return fail("the server did not report what it received; rate search needs a gauge server that does (same version)".into());
        }
        let result = evaluate(probes + 1, target, settings.probe_secs, packet_bytes, &flows);
        probes += 1;
        searcher.record(target, result.verdict);
        if result.verdict != Verdict::Fail
            && best.is_none_or(|b| result.delivered_bps > b.delivered_bps)
        {
            best = Some(result);
        }
        let _ = tx.send(NetEvent::RateProbe(result));
    }

    let reason = if stop.load(Ordering::Relaxed) {
        StopReason::Stopped
    } else {
        searcher.stop_reason().unwrap_or(StopReason::ProbeLimit)
    };
    let _ = tx.send(NetEvent::RateSearchDone { best, probes, reason });

    // The UI/CLI wait for one "finished" per stream and flow; report each
    // with the bytes it sent over the whole search.
    let elapsed = probes as u64 * settings.probe_secs;
    for stream_index in 0..streams {
        for &flow in directions {
            let total_bytes = sent_bytes.get(&(stream_index, flow)).copied().unwrap_or(0);
            let avg_gbps = total_bytes as f64 * 8.0 / elapsed.max(1) as f64 / 1e9;
            let _ = tx.send(NetEvent::ClientStreamFinished {
                stream_index,
                flow,
                total_bytes,
                elapsed_secs: elapsed.max(1),
                avg_gbps,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs the decision logic against a fake path that carries up to
    /// `capacity` bits/s without loss and loses packets above it.
    fn search(capacity: u64, sender_max: u64) -> (Option<u64>, StopReason, u32) {
        let mut s = Searcher::new(50_000_000, 100_000_000_000);
        let mut best = None;
        let mut probes = 0;
        while let Some(rate) = s.next_rate() {
            probes += 1;
            let verdict = if rate > sender_max {
                Verdict::SenderLimited
            } else if rate <= capacity {
                best = best.max(Some(rate));
                Verdict::Pass
            } else {
                Verdict::Fail
            };
            s.record(rate, verdict);
            assert!(probes <= MAX_PROBES, "search must terminate");
        }
        (best, s.stop_reason().unwrap(), probes)
    }

    #[test]
    fn finds_the_capacity_within_tolerance() {
        for capacity in [120_000_000u64, 937_000_000, 3_700_000_000, 24_000_000_000] {
            let (best, reason, probes) = search(capacity, u64::MAX);
            let best = best.expect("some rate must pass");
            assert_eq!(reason, StopReason::Converged, "capacity {capacity}");
            assert!(best <= capacity, "never reports more than the path carries");
            assert!(
                best as f64 >= capacity as f64 / (1.0 + TOLERANCE) / (1.0 + TOLERANCE),
                "capacity {capacity}: found only {best}"
            );
            assert!(probes <= MAX_PROBES);
        }
    }

    #[test]
    fn stops_when_the_sender_cannot_go_faster() {
        let (best, reason, _) = search(50_000_000_000, 8_000_000_000);
        assert_eq!(reason, StopReason::SenderLimited);
        assert!(
            best.unwrap() >= 3_200_000_000,
            "the clean rates below the sender limit still count"
        );
    }

    #[test]
    fn stops_at_the_maximum_rate() {
        let mut s = Searcher::new(50_000_000, 400_000_000);
        while let Some(rate) = s.next_rate() {
            s.record(rate, Verdict::Pass);
        }
        assert_eq!(s.stop_reason(), Some(StopReason::MaxRate));
    }

    #[test]
    fn reports_when_even_the_lowest_rate_loses_packets() {
        let (best, reason, _) = search(500_000, u64::MAX);
        assert_eq!(best, None);
        assert_eq!(reason, StopReason::NothingFits);
    }

    #[test]
    fn an_earlier_failure_that_a_higher_rate_contradicts_is_treated_as_noise() {
        let mut s = Searcher::new(100_000_000, 100_000_000_000);
        s.record(100_000_000, Verdict::Pass); // -> 200M
        s.record(200_000_000, Verdict::Fail); // -> 150M
        s.record(150_000_000, Verdict::Pass); // converged soon
        // A pass above the earlier failure clears it, so the search resumes ramping.
        s.record(300_000_000, Verdict::Pass);
        assert_eq!(s.next_rate(), Some(600_000_000));
    }

    fn tally(received: u64, lost: u64) -> FlowTally {
        FlowTally { received, lost, jitter_ms: 0.1, reported: true }
    }

    #[test]
    fn evaluate_separates_path_loss_from_a_sender_that_fell_short() {
        // 1000-byte packets over 2 s: 250_000 packets = 1 Gbit/s.
        let clean = evaluate(1, 1_000_000_000, 2, 1000, &[tally(250_000, 0)]);
        assert_eq!(clean.verdict, Verdict::Pass);
        assert_eq!(clean.delivered_bps, 1_000_000_000);

        let lossy = evaluate(2, 1_000_000_000, 2, 1000, &[tally(200_000, 50_000)]);
        assert_eq!(lossy.verdict, Verdict::Fail);
        assert!((lossy.loss_pct - 20.0).abs() < 1e-9);
        assert_eq!(lossy.offered_bps, 1_000_000_000);

        let slow_sender = evaluate(3, 1_000_000_000, 2, 1000, &[tally(100_000, 0)]);
        assert_eq!(slow_sender.verdict, Verdict::SenderLimited);
    }

    #[test]
    fn evaluate_lets_the_weaker_direction_decide() {
        let r = evaluate(1, 1_000_000_000, 2, 1000, &[tally(250_000, 0), tally(200_000, 50_000)]);
        assert_eq!(r.verdict, Verdict::Fail);
        assert!((r.loss_pct - 20.0).abs() < 1e-9);
    }

    #[test]
    fn a_probe_that_received_nothing_counts_as_total_loss() {
        let r = evaluate(1, 1_000_000_000, 2, 1000, &[tally(0, 0)]);
        assert_eq!(r.verdict, Verdict::Fail);
        assert_eq!(r.loss_pct, 100.0);
    }
}
