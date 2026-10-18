//! A congestion-controller wrapper for a QUIC connection that rides TCP (a door, a relay's TCP
//! carrier): it holds our queue in front of the TCP pipe to a delay budget.
//!
//! Over TCP the QUIC controller never sees a loss — TCP repairs it underneath — so a loss-driven
//! window only grows, and BBR's grew to hundreds of MB: the door's RTT under load was 1.6-1.9 s,
//! every message and call on the link waiting behind the file (live 2026-10-16,
//! docs/bugs/door-throughput.md). What the pipe can carry is decided by the OS's TCP below us, not by
//! us; the one thing left to decide is how much waits in front of it.
//!
//! The rate-based cap (`bdp_cap`) was tried there first and is the wrong tool for it: it builds the
//! window from a measured delivery rate, and over TCP that rate is whatever TCP's own retransmits,
//! head-of-line stalls and CUBIC cut-backs let through in the sample. One bad stretch, a small
//! estimate, a small window that cannot show the path is faster — live 2026-10-17 it sat at its
//! 8-packet floor, 0.5 MB/s on a door that did 4.
//!
//! This one needs no rate at all: the window grows while the RTT stays under `min_rtt + budget` and
//! is cut back in proportion when it goes over, at most once per RTT. It follows whatever TCP drains
//! and keeps about `budget` of queue on top of the path, which keeps TCP's own pipe full.

use std::any::Any;
use std::sync::Arc;
use std::time::{Duration, Instant};

use quinn::congestion::{Controller, ControllerFactory, ControllerMetrics};
use quinn_proto::RttEstimator;

/// Never below this many packets: loss recovery needs packets behind a lost one.
const MIN_WINDOW_PACKETS: u64 = 8;

/// The deepest one cut goes: the window keeps at least this share of itself.
const MAX_CUT: f64 = 0.7;

/// The base RTT is the smallest seen over this long, not over the connection's life. Over TCP the
/// path's own delay moves — TCP's recoveries on a lossy path held it at ~120 ms for minutes — and a
/// base learned once at 37 ms then made the target unreachable: the window sat at its floor, 0.5 MB/s
/// (live 2026-10-17). Our own queue cannot fake a higher base: the cuts drain it within this window.
const BASE_RTT_WINDOW: Duration = Duration::from_secs(10);

/// The smallest RTT over the last `BASE_RTT_WINDOW`, kept as one minimum per second.
#[derive(Clone, Default)]
struct BaseRtt {
    /// (second it opened, smallest RTT in it), oldest first.
    seconds: std::collections::VecDeque<(Instant, Duration)>,
}

impl BaseRtt {
    fn observe(&mut self, now: Instant, rtt: Duration) {
        while self.seconds.front().is_some_and(|(at, _)| now.saturating_duration_since(*at) > BASE_RTT_WINDOW) {
            self.seconds.pop_front();
        }
        match self.seconds.back_mut() {
            Some((at, min)) if now.saturating_duration_since(*at) < Duration::from_secs(1) => *min = (*min).min(rtt),
            _ => self.seconds.push_back((now, rtt)),
        }
    }

    fn get(&self) -> Option<Duration> {
        self.seconds.iter().map(|(_, min)| *min).min()
    }
}

/// The window's next value. `latest` — the RTT just measured; `in_use` — the window was being used
/// (a window nobody fills says nothing about the path and must not grow: a transfer held by flow
/// control would otherwise inflate it for a burst later); `cut_due` — an RTT has passed since the
/// last cut. Returns the window and whether it was cut.
fn next_window(
    window: u64,
    latest: Duration,
    min_rtt: Duration,
    budget: Duration,
    acked: u64,
    in_use: bool,
    cut_due: bool,
    floor: u64,
) -> (u64, bool) {
    let target = min_rtt.saturating_add(budget);
    if latest > target {
        if !cut_due {
            return (window, false);
        }
        // In proportion to how far over — one cut brings the queue back to about the budget — but
        // never below `MAX_CUT` of it at once: TCP's own recoveries spike the RTT for a moment, and
        // a full proportional cut on such a spike left TCP with nothing to send when it was back.
        let ratio = (target.as_secs_f64() / latest.as_secs_f64()).max(MAX_CUT);
        let cut = (window as f64 * ratio) as u64;
        return (cut.max(floor), true);
    }
    if !in_use {
        return (window, false);
    }
    // Under the target: grow by what was acked — doubling per RTT, as a fresh window should, so a
    // fast pipe is reached in a few RTTs. Overshoot is what the cut above is for.
    (window.saturating_add(acked), false)
}

pub struct DelayCappedController {
    inner: Box<dyn Controller>,
    budget: Duration,
    mtu: u64,
    window: u64,
    base_rtt: BaseRtt,
    last_cut: Option<Instant>,
    /// Bytes in flight at the last ack batch: whether the window is being used.
    in_flight: u64,
}

impl DelayCappedController {
    fn new(inner: Box<dyn Controller>, budget: Duration, current_mtu: u16) -> Self {
        let window = inner.initial_window();
        Self { inner, budget, mtu: u64::from(current_mtu), window, base_rtt: BaseRtt::default(), last_cut: None, in_flight: 0 }
    }

    fn floor(&self) -> u64 {
        self.mtu.saturating_mul(MIN_WINDOW_PACKETS)
    }

    /// For the `quic/stats` line: the window held, what the inner controller wanted, the base RTT.
    pub fn snapshot(&self) -> crate::bdp_cap::BdpCapSnapshot {
        crate::bdp_cap::BdpCapSnapshot {
            uncapped_window: self.inner.window(),
            cap: Some(self.window),
            min_rtt: self.base_rtt.get(),
            delivery_rate_bytes_per_sec: 0.0,
        }
    }
}

impl Controller for DelayCappedController {
    fn on_sent(&mut self, now: Instant, bytes: u64, last_packet_number: u64) {
        self.inner.on_sent(now, bytes, last_packet_number);
    }

    fn on_ack(&mut self, now: Instant, sent: Instant, bytes: u64, app_limited: bool, rtt: &RttEstimator) {
        self.inner.on_ack(now, sent, bytes, app_limited, rtt);
        // This packet's own RTT (sent to acked) feeds the base: quinn's `min` is for the whole life.
        self.base_rtt.observe(now, now.saturating_duration_since(sent));
        let Some(min_rtt) = self.base_rtt.get() else {
            return;
        };
        let smoothed = rtt.get();
        let cut_due = self.last_cut.is_none_or(|at| now.saturating_duration_since(at) >= smoothed);
        // In use: what is in flight fills at least half the window (not app-limited by flow control).
        let in_use = !app_limited || self.in_flight.saturating_mul(2) >= self.window;
        // The smoothed RTT decides, not this packet's: over TCP one packet held behind a TCP repair
        // reads hundreds of ms on a pipe that is otherwise fine.
        let (window, cut) =
            next_window(self.window, smoothed, min_rtt, self.budget, bytes, in_use, cut_due, self.floor());
        self.window = window;
        if cut {
            self.last_cut = Some(now);
        }
    }

    fn on_end_acks(&mut self, now: Instant, in_flight: u64, app_limited: bool, largest_packet_num_acked: Option<u64>) {
        self.in_flight = in_flight;
        self.inner.on_end_acks(now, in_flight, app_limited, largest_packet_num_acked);
    }

    fn on_congestion_event(&mut self, now: Instant, sent: Instant, is_persistent_congestion: bool, lost_bytes: u64) {
        // Over TCP a "loss" is a packet late behind a TCP repair, not a full queue: the delay rule
        // above already answers the queue. The inner controller still hears it.
        self.inner.on_congestion_event(now, sent, is_persistent_congestion, lost_bytes);
    }

    fn on_mtu_update(&mut self, new_mtu: u16) {
        self.mtu = u64::from(new_mtu);
        self.window = self.window.max(self.floor());
        self.inner.on_mtu_update(new_mtu);
    }

    fn window(&self) -> u64 {
        self.inner.window().min(self.window)
    }

    fn metrics(&self) -> ControllerMetrics {
        let mut metrics = self.inner.metrics();
        metrics.congestion_window = self.window();
        metrics
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(Self {
            inner: self.inner.clone_box(),
            budget: self.budget,
            mtu: self.mtu,
            window: self.window,
            base_rtt: self.base_rtt.clone(),
            last_cut: self.last_cut,
            in_flight: self.in_flight,
        })
    }

    fn initial_window(&self) -> u64 {
        self.inner.initial_window()
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

/// Builds [`DelayCappedController`]s around whatever the inner factory makes.
pub struct DelayCappedControllerFactory {
    inner: Arc<dyn ControllerFactory + Send + Sync>,
    budget: Duration,
}

impl DelayCappedControllerFactory {
    pub fn new(inner: Arc<dyn ControllerFactory + Send + Sync>, budget: Duration) -> Self {
        Self { inner, budget }
    }
}

impl ControllerFactory for DelayCappedControllerFactory {
    fn build(self: Arc<Self>, now: Instant, current_mtu: u16) -> Box<dyn Controller> {
        Box::new(DelayCappedController::new(Arc::clone(&self.inner).build(now, current_mtu), self.budget, current_mtu))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: fn(u64) -> Duration = Duration::from_millis;
    const FLOOR: u64 = 8 * 1200;

    #[test]
    fn under_the_target_a_used_window_grows_by_what_was_acked() {
        let (window, cut) = next_window(100_000, MS(60), MS(40), MS(50), 20_000, true, true, FLOOR);
        assert_eq!((window, cut), (120_000, false));
    }

    #[test]
    fn a_window_nobody_fills_does_not_grow() {
        let (window, _) = next_window(100_000, MS(45), MS(40), MS(50), 20_000, false, true, FLOOR);
        assert_eq!(window, 100_000, "held by flow control: no growth to burst out later");
    }

    #[test]
    fn over_the_target_it_is_cut_in_proportion_once_per_rtt() {
        // Target 90 ms, measured 108: cut to 90/108 of the window.
        let (window, cut) = next_window(1_200_000, MS(108), MS(40), MS(50), 20_000, true, true, FLOOR);
        assert!(cut && window.abs_diff(1_000_000) <= 1, "{window}");
        let (again, cut) = next_window(window, MS(180), MS(40), MS(50), 20_000, true, false, FLOOR);
        assert_eq!((again, cut), (window, false), "not twice inside one RTT: the first cut is still draining");
    }

    #[test]
    fn one_cut_keeps_at_least_seventy_percent_of_the_window() {
        let (window, _) = next_window(1_000_000, MS(900), MS(40), MS(50), 20_000, true, true, FLOOR);
        assert_eq!(window, 700_000, "a spike ten times over the target still cuts by 30% only");
    }

    /// The base follows the path: a lower RTT older than the window no longer counts.
    #[test]
    fn the_base_rtt_is_the_smallest_of_the_last_ten_seconds() {
        let mut base = BaseRtt::default();
        let start = Instant::now();
        base.observe(start, MS(37));
        for second in 1..=12 {
            base.observe(start + Duration::from_secs(second), MS(120));
        }
        assert_eq!(base.get(), Some(MS(120)), "37 ms is twelve seconds old");
        base.observe(start + Duration::from_millis(12_500), MS(90));
        assert_eq!(base.get(), Some(MS(90)));
    }

    #[test]
    fn a_cut_never_goes_below_the_packet_floor() {
        let (window, _) = next_window(12_000, MS(5_000), MS(40), MS(50), 1_000, true, true, FLOOR);
        assert_eq!(window, FLOOR);
    }
}
