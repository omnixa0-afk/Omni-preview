//! A congestion-controller wrapper that stops us from filling the carrier's queue.
//!
//! Measured on the live mobile stand: sending one 20 MB file takes the path RTT from 44 ms idle to
//! 609-706 ms under load and loses 1904 packets, because the sender keeps ~2.1 MB in flight on a
//! path whose bandwidth-delay product is about 82 KB. Twenty-five times more data in flight than the
//! path can carry does not make the transfer faster -- the uplink is the ceiling either way -- it
//! parks the excess in the operator's queue, where it delays everything else the phone is doing and
//! is eventually dropped anyway. Capping in-flight to 256 KB on that same path held goodput
//! (14.1 -> 13.7 Mbit/s, inside the noise) while RTT under load fell to 68-101 ms and losses to 28.
//!
//! That experiment used a FIXED cap, which cannot ship: 256 KB is ~3x the BDP of that mobile uplink
//! but only ~0.2x the BDP of a 100 Mbit/s path at 100 ms, where it would throttle hard. So the cap
//! must be derived per path.
//!
//! The rule is a QUEUE BUDGET, not a multiple of the BDP:
//! `in_flight <= delivery_rate * (min_rtt + budget)` -- "carry what the path carries, plus at most
//! `budget` milliseconds of our own queue on top". A multiplier was tried first and is unsound,
//! measured on the LAN stand: `k * rate * min_rtt` feeds back on itself, because `rate` is measured
//! while the cap is already squeezing it. Smaller cap -> lower rate -> smaller cap, settling at a
//! stable fraction of the link. At k=3 with an honest rate estimator that fixed point was
//! 263 Mbit/s on a link doing 790. It only ever looked workable while the estimator was over-reading
//! by 3-20x, i.e. the bug was cancelling the design error.
//!
//! A budget does not have that loop on any path where the budget dominates min_rtt, and it says
//! something a person can hold an opinion about: how much added delay a transfer may inflict on
//! everything else sharing the link.
//!
//! Deliberately a WRAPPER, not a new controller. The controller `select_congestion_control` picked
//! still decides how the window grows and how it answers loss -- the wrapper is bolted on top of
//! THAT one, whichever it is (it used to be hard-coded to Cubic at the call site, which silently
//! cancelled the BBR/metered decision). This only refuses to let the
//! result exceed what the path can actually hold: it can lower the window, never raise it.
//!
//! **The hard part is not the multiplier, it is measuring the rate.** Three separate estimator bugs
//! have been found here on live links, and each of them decided the outcome more than the multiplier
//! did: a window of one min-RTT let an ack burst read as the path's steady rate (20x over on a LAN);
//! an idle interval recorded as zero switched the cap off entirely; the same interval recorded at
//! its true tiny rate pinned the next transfer near the packet floor with no way back up. All three
//! are pinned by tests below. Treat any change to the sampling with the same suspicion.

use std::any::Any;
use std::sync::Arc;
use std::time::{Duration, Instant};

use quinn::congestion::{Controller, ControllerFactory, ControllerMetrics};
use quinn_proto::RttEstimator;

/// Never clamp below this many packets, whatever the estimates say. A window of a few packets cannot
/// sustain loss recovery -- a loss needs packets behind it to trigger fast retransmit -- so an
/// over-eager cap would convert a slow path into a stalled one.
const MIN_WINDOW_PACKETS: u64 = 8;

/// Delivery rate is sampled per interval; the answer is the MAXIMUM over this many recent samples.
///
/// The maximum is not a bias to be cleaned up — it is what stops the cap eating itself, and that is
/// worth spelling out because trying a median instead is an obvious idea that fails spectacularly.
///
/// Any cap proportional to the MEASURED rate is a downward ratchet, because the rate is measured
/// while the cap is in force. Write the equilibrium out: throughput is `cap / rtt_under_load`, and
/// `cap = rate * (min_rtt + budget)` with `rate` tracking throughput, so `rtt_under_load` settles at
/// `min_rtt + budget` — for ANY throughput. The loop pins the latency target and says nothing about
/// the rate, so any dip lowers the cap, which lowers the rate, which lowers the cap. With a median
/// this ran all the way down on the live mobile stand: the cap sat at exactly `8 * MTU`, its floor,
/// latency hit its target beautifully (54-81 ms against 803-1642 uncapped, loss 0.01% against 8-13%)
/// and goodput collapsed to 1.5 Mbit/s against 7.7-11.0.
///
/// A maximum filter breaks the ratchet: the estimate is the best the path has recently shown, so the
/// cap converges on capacity instead of on wherever throughput happened to drift. BBR uses a
/// windowed max of delivery rate for exactly this reason.
///
/// What made the maximum look wrong earlier was short windows, not the maximum: at a 10 ms floor one
/// window held roughly one GSO batch, so "the best window" meant "the best burst" and read 2-5x over.
/// With [`RATE_WINDOW_MIN`] at 50 ms a window averages several batches, and the max over eight of
/// them is a defensible read on capacity.
const RATE_SAMPLES: usize = 8;

/// How many real samples must exist before the cap is allowed to bind at all.
///
/// Not the full ring: with a max filter a partly-filled ring is not dangerous the way a partly-filled
/// median was (the zeros lose the max, they do not sort into the middle), so waiting for all eight
/// would only delay the cap — 2.8 s on a path whose windows are RTT-driven. Two is enough to keep a
/// single unlucky first window from setting the cap for the whole transfer.
const RATE_SAMPLES_BEFORE_CAPPING: usize = 2;

/// A rate sample must cover at least this many min-RTTs.
///
/// It used to be exactly ONE, and that is what made the whole cap untrustworthy: acknowledgements do
/// not arrive smoothly, they arrive in bursts, and a burst filling a one-min-RTT window reads as the
/// path's steady rate. Measured over-read against the goodput of the very same run: 2x on loopback,
/// 3x on a LAN at 3x, and **20x** on a LAN at 6x -- `cap_rate_mbit=15634`, i.e. 1.95 GB/s on a link
/// that delivered 723 Mbit/s. The cap was never "3x BDP"; it was "3x an over-estimate of BDP", and
/// the shorter the path's RTT the worse the over-estimate. The mobile result survived only because
/// 44 ms of min RTT made its windows long by accident.
const RATE_WINDOW_RTTS: u32 = 8;

/// ...and never shorter than this in wall-clock terms. Eight min-RTTs of a 50 us path is still
/// 400 us, which one burst fills just as easily. This is the floor that makes the window immune to
/// aggregation on fast paths, where every earlier version of this estimator failed.
///
/// 50 ms rather than the 10 ms first tried, for a reason the 10 ms version made visible: one GSO
/// batch is roughly 8 ms of data on the LAN stand, so a 10 ms window held about ONE batch and
/// inherited all of its burstiness. Fifty holds several, which is what averaging needs. The cost is
/// that the cap engages later — [`RATE_SAMPLES_BEFORE_CAPPING`] windows must close first, i.e. about
/// 100 ms on a fast path and ~700 ms on the mobile one, where windows are RTT-driven. That is the
/// right trade: the risk being managed is a transfer that runs for seconds, and doing nothing for
/// the first fraction of a second is free. The trace says `bdp_cap=none` meanwhile.
const RATE_WINDOW_MIN: Duration = Duration::from_millis(50);

/// An app-limited interval still counts when it carried at least this much AND beat the best
/// sample so far — BBR's rule: what was delivered while not pushing is a lower bound on what the
/// path carries, so it can only raise the estimate.
///
/// Without it a transfer bounded by flow control was never measured at all: quinn calls that
/// app-limited (the window was not full), every interval was thrown away, the cap stayed off and
/// BBR's window grew to 274 MB on a door doing 20 MB/s, RTT 365 ms (live 2026-10-17). The size floor
/// is what keeps the messaging warm-up out — kilobytes, the trap pinned by
/// `an_interval_touched_by_app_limiting_is_discarded_not_recorded_low`.
const APP_LIMITED_SAMPLE_MIN_BYTES: u64 = 512 * 1024;

/// What the cap is doing on a live connection, for the `quic/stats` diagnostic.
///
/// The cap is accepted on two path types (a policed mobile uplink and a loopback fast path) and
/// UNMEASURED on a third: a high-BDP path, say 100 Mbit/s at 100 ms. Neither stand resembles one, and
/// that is exactly where an under-estimate of the delivery rate would cost the most. Rather than
/// guess, this makes the question answerable from any path's trace: `cap` next to `uncapped_window`
/// says whether the cap bound at all and by how much, and the two inputs say why.
#[derive(Debug, Clone, Copy)]
pub struct BdpCapSnapshot {
    /// Window the inner controller asked for, before the cap.
    pub uncapped_window: u64,
    /// The cap being applied, or `None` while the path is not measured well enough to judge.
    pub cap: Option<u64>,
    /// Propagation delay estimate the cap is built on.
    pub min_rtt: Option<Duration>,
    /// Delivery-rate estimate the cap is built on, bytes/sec.
    pub delivery_rate_bytes_per_sec: f64,
}

/// Reads the cap's state out of a controller taken from `quinn::Connection::congestion_state()`.
///
/// `None` means the cap is not installed on this connection (`OMNI_QUIC_QUEUE_BUDGET_MS=off`). The
/// diagnostic stays silent in that case instead of printing a field about a cap that is not there.
pub fn snapshot_from_controller(controller: Box<dyn Controller>) -> Option<BdpCapSnapshot> {
    let any = controller.into_any();
    match any.downcast::<BdpCappedController>() {
        Ok(capped) => Some(capped.snapshot()),
        Err(any) => any
            .downcast::<crate::delay_cap::DelayCappedController>()
            .ok()
            .map(|capped| capped.snapshot()),
    }
}

/// Wraps any controller and clamps its window to a measured bandwidth-delay product.
pub struct BdpCappedController {
    inner: Box<dyn Controller>,
    queue_budget: Duration,
    current_mtu: u64,
    /// Bytes acknowledged so far in the interval being sampled.
    interval_acked: u64,
    /// Whether anything in this interval was acked while the sender was app-limited. If so the
    /// interval measures how much we had to send, not how much the path could carry, and counts only
    /// as a lower bound that beats the best so far (`APP_LIMITED_SAMPLE_MIN_BYTES`).
    interval_app_limited: bool,
    interval_started: Instant,
    /// Recent delivery-rate samples, bytes/sec. The cap uses their maximum (see [`RATE_SAMPLES`]).
    rate_samples: [f64; RATE_SAMPLES],
    rate_cursor: usize,
    /// How many of `rate_samples` hold a real measurement. Saturates at the ring size.
    samples_filled: usize,
    /// Smallest RTT this path has shown: propagation delay, i.e. the RTT with no queue in it.
    min_rtt: Option<Duration>,
}

impl BdpCappedController {
    fn new(
        inner: Box<dyn Controller>,
        queue_budget: Duration,
        now: Instant,
        current_mtu: u16,
    ) -> Self {
        Self {
            inner,
            queue_budget,
            current_mtu: u64::from(current_mtu),
            interval_acked: 0,
            interval_app_limited: false,
            interval_started: now,
            rate_samples: [0.0; RATE_SAMPLES],
            rate_cursor: 0,
            samples_filled: 0,
            min_rtt: None,
        }
    }

    /// The cap, or `None` while the path has not been measured well enough to judge. `None` means
    /// "do not interfere", which is what an unmeasured path deserves.
    fn cap_bytes(&self) -> Option<u64> {
        let min_rtt = self.min_rtt?;
        let rate = self.delivery_rate();
        if rate <= 0.0 || min_rtt.is_zero() {
            return None;
        }
        let target = min_rtt.saturating_add(self.queue_budget);
        let cap = (rate * target.as_secs_f64()) as u64;
        Some(cap.max(self.current_mtu.saturating_mul(MIN_WINDOW_PACKETS)))
    }

    /// The delivery rate the cap is currently built on: the best of the recent samples, for the
    /// anti-ratchet reason given on [`RATE_SAMPLES`]. Zero until the ring holds at least a couple of
    /// real measurements, so a single early window cannot set the cap for the whole transfer.
    fn delivery_rate(&self) -> f64 {
        if self.samples_filled < RATE_SAMPLES_BEFORE_CAPPING {
            return 0.0;
        }
        self.rate_samples.iter().copied().fold(0.0_f64, f64::max)
    }

    /// Everything a trace needs to judge whether the cap is helping or in the way.
    pub fn snapshot(&self) -> BdpCapSnapshot {
        BdpCapSnapshot {
            uncapped_window: self.inner.window(),
            cap: self.cap_bytes(),
            min_rtt: self.min_rtt,
            delivery_rate_bytes_per_sec: self.delivery_rate(),
        }
    }

    /// How long one rate sample must cover before it means anything. See [`RATE_WINDOW_RTTS`] and
    /// [`RATE_WINDOW_MIN`] -- between them, long enough that ack aggregation averages out instead of
    /// deciding the answer.
    fn rate_window(&self, min_rtt: Duration) -> Duration {
        min_rtt
            .saturating_mul(RATE_WINDOW_RTTS)
            .max(RATE_WINDOW_MIN)
    }

    /// Close the sampling interval once it has covered a full rate window. Sampling over less than
    /// that measures a burst, not a rate.
    fn maybe_close_interval(&mut self, now: Instant) {
        let Some(min_rtt) = self.min_rtt else {
            return;
        };
        let elapsed = now.saturating_duration_since(self.interval_started);
        if elapsed < self.rate_window(min_rtt) || elapsed.is_zero() {
            return;
        }
        // An interval that was app-limited at any point is DISCARDED, not recorded low. Both halves
        // of that were learned the hard way on the LAN stand:
        //
        // - Recording such an interval as a zero (the original code excluded app-limited BYTES but
        //   still closed the interval) zeroed all four samples during idle, `cap_bytes` then read
        //   "path not measured" and the cap switched ITSELF OFF -- live: `bdp_cap=none
        //   cap_rate_mbit=0.00` at the tail of a transfer while the window ran back to 127 MB and
        //   RTT to 34 ms.
        // - Recording it at its true, tiny rate is worse still: the messaging warm-up left the ring
        //   full of kilobyte-per-second samples, the next transfer started capped near the packet
        //   floor, and a small window cannot ack enough bytes to raise the estimate that is holding
        //   it small. That run came out at 290 Mbit/s against 739 uncapped.
        //
        // Discarding keeps the last honest measurement instead, which is what "we had nothing to
        // send" actually tells us about the path: nothing.
        let rate = self.interval_acked as f64 / elapsed.as_secs_f64();
        let best = self.rate_samples.iter().copied().fold(0.0_f64, f64::max);
        let counts = if self.interval_app_limited {
            self.interval_acked >= APP_LIMITED_SAMPLE_MIN_BYTES && rate > best
        } else {
            self.interval_acked > 0
        };
        if counts {
            self.rate_samples[self.rate_cursor] = rate;
            self.rate_cursor = (self.rate_cursor + 1) % RATE_SAMPLES;
            self.samples_filled = self.samples_filled.saturating_add(1).min(RATE_SAMPLES);
        }
        self.interval_acked = 0;
        self.interval_app_limited = false;
        self.interval_started = now;
    }
}

impl Controller for BdpCappedController {
    fn on_sent(&mut self, now: Instant, bytes: u64, last_packet_number: u64) {
        self.inner.on_sent(now, bytes, last_packet_number);
    }

    fn on_ack(
        &mut self,
        now: Instant,
        sent: Instant,
        bytes: u64,
        app_limited: bool,
        rtt: &RttEstimator,
    ) {
        // An app-limited stretch says nothing about what the path can carry -- we simply had nothing
        // to send -- so it taints the whole interval, which `maybe_close_interval` then discards.
        if app_limited {
            self.interval_app_limited = true;
        }
        self.interval_acked = self.interval_acked.saturating_add(bytes);
        let min = rtt.min();
        if !min.is_zero() {
            self.min_rtt = Some(self.min_rtt.map_or(min, |current| current.min(min)));
        }
        self.maybe_close_interval(now);
        self.inner.on_ack(now, sent, bytes, app_limited, rtt);
    }

    fn on_end_acks(
        &mut self,
        now: Instant,
        in_flight: u64,
        app_limited: bool,
        largest_packet_num_acked: Option<u64>,
    ) {
        self.inner
            .on_end_acks(now, in_flight, app_limited, largest_packet_num_acked);
    }

    fn on_congestion_event(
        &mut self,
        now: Instant,
        sent: Instant,
        is_persistent_congestion: bool,
        lost_bytes: u64,
    ) {
        self.inner
            .on_congestion_event(now, sent, is_persistent_congestion, lost_bytes);
    }

    fn on_mtu_update(&mut self, new_mtu: u16) {
        self.current_mtu = u64::from(new_mtu);
        self.inner.on_mtu_update(new_mtu);
    }

    fn window(&self) -> u64 {
        let window = self.inner.window();
        match self.cap_bytes() {
            Some(cap) => window.min(cap),
            None => window,
        }
    }

    fn metrics(&self) -> ControllerMetrics {
        // `ControllerMetrics` is #[non_exhaustive], so it cannot be built with struct-update syntax
        // from outside quinn -- take the inner's and overwrite the one field we actually change.
        let mut metrics = self.inner.metrics();
        metrics.congestion_window = self.window();
        metrics
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(Self {
            inner: self.inner.clone_box(),
            queue_budget: self.queue_budget,
            current_mtu: self.current_mtu,
            interval_acked: self.interval_acked,
            interval_app_limited: self.interval_app_limited,
            interval_started: self.interval_started,
            rate_samples: self.rate_samples,
            rate_cursor: self.rate_cursor,
            samples_filled: self.samples_filled,
            min_rtt: self.min_rtt,
        })
    }

    fn initial_window(&self) -> u64 {
        self.inner.initial_window()
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

/// Builds [`BdpCappedController`]s around whatever the inner factory makes.
pub struct BdpCappedControllerFactory {
    inner: Arc<dyn ControllerFactory + Send + Sync>,
    queue_budget: Duration,
}

impl BdpCappedControllerFactory {
    pub fn new(inner: Arc<dyn ControllerFactory + Send + Sync>, queue_budget: Duration) -> Self {
        Self {
            inner,
            queue_budget,
        }
    }
}

impl ControllerFactory for BdpCappedControllerFactory {
    fn build(self: Arc<Self>, now: Instant, current_mtu: u16) -> Box<dyn Controller> {
        Box::new(BdpCappedController::new(
            Arc::clone(&self.inner).build(now, current_mtu),
            self.queue_budget,
            now,
            current_mtu,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quinn::congestion::CubicConfig;

    const TEST_MTU: u16 = 1200;

    /// `on_ack` cannot be driven from a test — `RttEstimator` has no public constructor — so the
    /// estimates are placed directly. That is fine for what is being pinned here: the estimator is
    /// two moving averages, the *decision* built on top of them is what can silently throttle a
    /// healthy path.
    fn capped(queue_budget_ms: u64) -> BdpCappedController {
        BdpCappedController::new(
            Arc::new(CubicConfig::default()).build(Instant::now(), TEST_MTU),
            Duration::from_millis(queue_budget_ms),
            Instant::now(),
            TEST_MTU,
        )
    }

    fn measured(
        queue_budget_ms: u64,
        rate_bytes_per_sec: f64,
        min_rtt: Duration,
    ) -> BdpCappedController {
        let mut controller = capped(queue_budget_ms);
        controller.rate_samples = [rate_bytes_per_sec; RATE_SAMPLES];
        controller.samples_filled = RATE_SAMPLES;
        controller.min_rtt = Some(min_rtt);
        controller
    }

    #[test]
    fn an_unmeasured_path_is_left_completely_alone() {
        let controller = capped(25);
        let snapshot = controller.snapshot();

        assert_eq!(snapshot.cap, None, "nothing measured yet, nothing to cap");
        assert_eq!(
            controller.window(),
            snapshot.uncapped_window,
            "an unmeasured path must get the inner controller's window untouched"
        );
    }

    #[test]
    fn the_window_is_clamped_to_the_measured_bandwidth_delay_product() {
        // 625 KB/s over an 8 ms path, allowed 8 ms of queue on top: 16 ms of data in flight, i.e. a
        // 10000-byte cap. That sits below Cubic's initial window and above the packet floor, so the
        // cap alone decides the answer.
        let controller = measured(8, 625_000.0, Duration::from_millis(8));
        let snapshot = controller.snapshot();

        let cap = snapshot.cap.expect("a measured path must produce a cap");
        assert!(
            cap.abs_diff(10_000) <= 1,
            "cap should be rate * (min_rtt + budget), got {cap}"
        );
        assert!(
            snapshot.uncapped_window > cap,
            "test needs Cubic to want more than the cap, wanted {} cap {cap}",
            snapshot.uncapped_window
        );
        assert_eq!(controller.window(), cap);
    }

    #[test]
    fn a_tiny_measurement_cannot_shrink_the_window_below_the_packet_floor() {
        // 10 KB/s over 11 ms is a 110-byte cap: a quarter of a packet in flight, which would stall
        // the path outright. The floor is what makes an over-eager estimate survivable.
        let controller = measured(1, 10_000.0, Duration::from_millis(10));

        let floor = u64::from(TEST_MTU) * MIN_WINDOW_PACKETS;
        assert_eq!(controller.snapshot().cap, Some(floor));
        assert_eq!(controller.window(), floor);
    }

    #[test]
    fn a_generous_cap_never_raises_the_inner_window() {
        // 100 MB/s over 100 ms with 200 ms of budget: 30 MB. Far above anything Cubic has earned
        // this early, and the wrapper must still not hand out more than Cubic asked for.
        let controller = measured(200, 100_000_000.0, Duration::from_millis(100));
        let snapshot = controller.snapshot();

        assert!(snapshot.cap.unwrap() > snapshot.uncapped_window);
        assert_eq!(controller.window(), snapshot.uncapped_window);
    }

    #[test]
    fn the_estimate_tracks_the_best_window_so_the_cap_cannot_eat_itself() {
        // The property this replaced a median to get. A cap proportional to the measured rate is a
        // ratchet: measure low once, cap lower, measure lower still. On the live mobile stand a
        // median rode that all the way to the packet floor -- goodput 1.5 Mbit/s against 7.7-11.0 --
        // while hitting its latency target perfectly. The max filter is what makes the cap converge
        // on capacity instead of on wherever throughput drifted.
        let mut controller = capped(25);
        controller.min_rtt = Some(Duration::from_millis(1));
        let mut now = controller.interval_started;

        let window = RATE_WINDOW_MIN.as_secs_f64();
        // One good window, then a run of bad ones -- a stall, a retransmit burst, a cell handover.
        for rate in [20_000_000.0, 2_000_000.0, 1_000_000.0, 900_000.0, 800_000.0] {
            controller.interval_acked = (rate * window) as u64;
            now += RATE_WINDOW_MIN;
            controller.maybe_close_interval(now);
        }

        let rate = controller.delivery_rate();
        assert!(
            (rate - 20_000_000.0).abs() < 500_000.0,
            "a dip must not become the new ceiling; expected the 20 MB/s window to hold, got {rate}"
        );
    }

    #[test]
    fn a_window_shorter_than_the_floor_is_not_a_sample_at_all() {
        let mut controller = capped(25);
        controller.min_rtt = Some(Duration::from_millis(1));
        let started = controller.interval_started;
        // 900 KB in one millisecond reads as 900 MB/s if a single min-RTT counts as a window. It
        // does not: this is the burst that once produced `cap_rate_mbit=15634`.
        controller.interval_acked = 900_000;

        controller.maybe_close_interval(started + Duration::from_millis(1));
        assert_eq!(controller.samples_filled, 0);

        controller.maybe_close_interval(started + RATE_WINDOW_MIN);
        assert_eq!(controller.samples_filled, 1);
    }

    #[test]
    fn one_lone_window_is_not_enough_to_start_capping() {
        // A single early window can catch a ramp-up and read far off. Two is the smallest number
        // that stops one unlucky sample from setting the cap for a whole transfer -- and with a max
        // filter that is enough, because an unfilled slot loses the max rather than dragging it down
        // the way an unfilled slot dragged down a median.
        let mut controller = capped(25);
        controller.min_rtt = Some(Duration::from_millis(1));
        let mut now = controller.interval_started;

        controller.interval_acked = (1_000_000.0 * RATE_WINDOW_MIN.as_secs_f64()) as u64;
        now += RATE_WINDOW_MIN;
        controller.maybe_close_interval(now);
        assert_eq!(controller.samples_filled, 1);
        assert_eq!(controller.delivery_rate(), 0.0);
        assert_eq!(controller.snapshot().cap, None);

        controller.interval_acked = (1_000_000.0 * RATE_WINDOW_MIN.as_secs_f64()) as u64;
        now += RATE_WINDOW_MIN;
        controller.maybe_close_interval(now);
        assert_eq!(controller.samples_filled, RATE_SAMPLES_BEFORE_CAPPING);
        assert!(controller.delivery_rate() > 0.0);
        assert!(controller.snapshot().cap.is_some());
    }

    #[test]
    fn an_idle_stretch_does_not_erase_what_the_path_was_measured_to_carry() {
        // Live regression: at the tail of a transfer the sender goes app-limited, four intervals
        // close with nothing acked, every rate sample reads zero, and the cap concludes the path is
        // unmeasured and switches off -- the window ran back to 127 MB and RTT to 34 ms. A
        // messenger is app-limited most of the time, so this hit every transfer after a pause.
        let mut controller = measured(15, 625_000.0, Duration::from_millis(8));
        let cap_before = controller.snapshot().cap.expect("measured path has a cap");

        let mut now = Instant::now();
        for _ in 0..(RATE_SAMPLES * 2) {
            now += Duration::from_millis(100);
            controller.maybe_close_interval(now);
        }

        assert_eq!(
            controller.snapshot().cap,
            Some(cap_before),
            "an interval with nothing to send is an absent measurement, not a slow one"
        );
    }

    #[test]
    fn an_interval_touched_by_app_limiting_is_discarded_not_recorded_low() {
        // The messaging warm-up before a transfer is exactly this shape: a little real traffic, and
        // we run out of things to send inside the interval. Recorded at face value it fills the ring
        // with kilobyte-per-second samples, and the next transfer starts capped near the packet
        // floor -- a window too small to ack enough bytes to lift the estimate that is holding it
        // small. Measured live at 290 Mbit/s against 739 uncapped.
        let mut controller = measured(15, 625_000.0, Duration::from_millis(8));
        let cap_before = controller.snapshot().cap.expect("measured path has a cap");

        let mut now = Instant::now();
        for _ in 0..(RATE_SAMPLES * 2) {
            controller.interval_acked = 512;
            controller.interval_app_limited = true;
            now += Duration::from_millis(100);
            controller.maybe_close_interval(now);
        }

        assert_eq!(
            controller.snapshot().cap,
            Some(cap_before),
            "a tainted interval must not become the estimate the cap is built on"
        );
    }

    /// A transfer held by flow control reads as app-limited to quinn, interval after interval; it
    /// still moves megabytes, and those measure the path — the cap must come up on it.
    #[test]
    fn a_transfer_held_by_flow_control_is_measured_all_the_same() {
        let mut controller = capped(50);
        controller.min_rtt = Some(Duration::from_millis(40));
        let mut now = Instant::now();
        for _ in 0..RATE_SAMPLES {
            controller.interval_acked = 6 * 1024 * 1024; // ~20 MB/s over a 320 ms window
            controller.interval_app_limited = true;
            now += Duration::from_millis(320);
            controller.maybe_close_interval(now);
        }
        let snapshot = controller.snapshot();
        assert!(snapshot.cap.is_some(), "measured, so capped");
        assert!(snapshot.delivery_rate_bytes_per_sec > 15.0 * 1024.0 * 1024.0);
    }

    #[test]
    fn the_snapshot_is_only_readable_where_the_cap_is_actually_installed() {
        let bare = Arc::new(CubicConfig::default()).build(Instant::now(), TEST_MTU);
        assert!(
            snapshot_from_controller(bare).is_none(),
            "a bare controller has no cap, and the trace must not claim otherwise"
        );

        let wrapped = Arc::new(BdpCappedControllerFactory::new(
            Arc::new(CubicConfig::default()),
            Duration::from_millis(25),
        ))
        .build(Instant::now(), TEST_MTU);
        assert!(snapshot_from_controller(wrapped).is_some());
    }
}
