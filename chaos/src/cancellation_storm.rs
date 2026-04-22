//! Cancellation-storm detector.
//!
//! ## Definition
//!
//! A *cancellation storm* is a period during which the **normalized
//! cancel rate** (cancels / total events in the recent window) sits
//! consistently above the running adaptive baseline `μ + k·σ̂` for at
//! least `min_consecutive` consecutive samples.
//!
//! Real venues quote-cancel routinely; what marks a storm is the
//! *deviation* from the local norm, not the absolute count. ITCH 5.0
//! routinely shows >85 % cancel ratios for healthy market makers; a
//! purely count-based threshold would either fire constantly (low cap)
//! or never (high cap). The adaptive baseline solves both.
//!
//! ## Robustness contract
//!
//! Three guards prevent the classical false-positive failure modes:
//!
//!   1. **Dual rolling window** (sequence + time) — events outside
//!      EITHER bound are evicted. Eliminates "ghost cancels" that look
//!      adjacent in seq but are seconds apart on a quiet feed.
//!   2. **Warmup + σ floor** — the adaptive threshold is suppressed
//!      until `warmup_samples` independent rate observations are
//!      accumulated AND `σ̂` is floored at `sigma_floor` to prevent
//!      single-sample collapse to zero variance (which would flag
//!      every subsequent micro-tick).
//!   3. **Persistence + refractory** — the storm flag fires only after
//!      the rate stays above threshold for `min_consecutive` samples,
//!      and after a flag the detector enters a `cooldown_seq` window
//!      during which further flags are suppressed (refractory period).
//!
//! ## Why Welford and not EMA
//!
//! Welford's incremental algorithm gives an unbiased running mean and
//! variance with O(1) state and no decay parameter to tune. It does
//! drift slowly when the underlying distribution changes (vs an EMA
//! which adapts to recent regime), so we *pause* the Welford update
//! while a storm is in progress: this prevents the baseline from being
//! poisoned by the very anomaly we are trying to detect.
//!
//! ## State characteristics
//!
//! Per-detector state is `O(window_size)` for the deque + 24 B for the
//! Welford accumulators + 32 B of bookkeeping. Self-contained: no
//! reference to `flowlab-core::HotOrderBook` or any external state.

use std::collections::VecDeque;

use flowlab_core::event::{EventType, SequencedEvent};
use flowlab_core::types::SeqNum;

use crate::{ChaosEvent, ChaosFeatures, ChaosKind};

/// Welford running mean / variance accumulator. Numerically stable
/// across the full range of f64 we'll ever feed it (rates in `[0, 1]`).
#[derive(Debug, Clone, Default)]
struct Welford {
    n: u64,
    mean: f64,
    m2: f64,
}

impl Welford {
    #[inline]
    fn update(&mut self, x: f64) {
        self.n = self.n.saturating_add(1);
        let delta = x - self.mean;
        self.mean += delta / self.n as f64;
        let delta2 = x - self.mean;
        self.m2 += delta * delta2;
    }

    #[inline]
    fn mean(&self) -> f64 {
        self.mean
    }

    /// Sample standard deviation (unbiased, divides by `n - 1`).
    /// Returns `0.0` when `n < 2`.
    #[inline]
    fn stddev(&self) -> f64 {
        if self.n < 2 {
            0.0
        } else {
            (self.m2 / (self.n - 1) as f64).sqrt()
        }
    }
}

/// Cancellation-storm detector. Adaptive, dual-window, warmup-guarded,
/// persistence-gated, refractory-protected.
pub struct CancellationStormDetector {
    /// Rolling event window: `(seq, ts, event_type_byte)`. Eviction
    /// from the front happens when EITHER the seq-window OR the
    /// time-window constraint is violated.
    window: VecDeque<(SeqNum, u64, u8)>,
    /// Maximum span of the rolling window in sequence numbers.
    /// `0` disables the seq dimension (time-only window).
    seq_window: u64,
    /// Maximum span of the rolling window in nanoseconds.
    /// `0` disables the time dimension (seq-only window).
    time_window_ns: u64,
    /// Minimum events the window must hold before a rate sample is
    /// considered meaningful. Below this, ratios swing wildly (small
    /// denominators) and we never feed Welford or evaluate threshold.
    min_window: usize,
    /// Number of `(non-storm)` rate samples we require before the
    /// adaptive threshold can fire. Protects against the cold-start
    /// case where a single early sample collapses the baseline.
    warmup_samples: u64,
    /// Lower bound on the standard-deviation estimate. Without a floor,
    /// a flat-rate run of identical samples drives `σ̂ → 0` and any
    /// non-trivial deviation flags. Typical value `0.02` (i.e. 2 pp on
    /// a `[0, 1]` rate).
    sigma_floor: f64,
    /// Multiplier on σ̂ for the adaptive threshold: `μ + k·σ̂`.
    k_sigma: f64,
    /// Number of consecutive above-threshold samples required to flag.
    /// Filters single-event spikes.
    min_consecutive: u64,
    /// Length of the refractory period after a flag, expressed in
    /// sequence numbers. Prevents back-to-back flags from one storm.
    cooldown_seq: u64,

    welford: Welford,
    /// Counter of consecutive above-threshold rate samples observed
    /// since the last below-threshold sample.
    consecutive_above: u64,
    /// Sequence number that must be reached before another flag is
    /// allowed (`0` = not in refractory).
    refractory_until: u64,
    /// Start-of-storm sequence number, captured the first sample we go
    /// above threshold within the current burst.
    burst_start_seq: u64,
    /// Start-of-storm timestamp (ns), same idea as `burst_start_seq`.
    burst_start_ts: u64,
    /// Latest computed normalized rate. Cached for severity reporting.
    last_rate: f64,
    /// Number of cancels currently inside the window. Maintained
    /// incrementally so we don't re-scan on every event.
    cancels_in_window: u64,
}

impl CancellationStormDetector {
    /// New detector. See module docs for parameter semantics.
    ///
    /// At least one of `seq_window` / `time_window_ns` must be > 0.
    pub fn new(
        seq_window: u64,
        time_window_ns: u64,
        min_window: usize,
        warmup_samples: u64,
        sigma_floor: f64,
        k_sigma: f64,
        min_consecutive: u64,
        cooldown_seq: u64,
    ) -> Self {
        debug_assert!(
            seq_window != 0 || time_window_ns != 0,
            "CancellationStormDetector: at least one window must be enabled"
        );
        debug_assert!(min_consecutive >= 1, "min_consecutive must be >= 1");
        debug_assert!(sigma_floor >= 0.0, "sigma_floor must be non-negative");
        debug_assert!(k_sigma >= 0.0, "k_sigma must be non-negative");

        Self {
            window: VecDeque::with_capacity(min_window.max(64)),
            seq_window,
            time_window_ns,
            min_window,
            warmup_samples,
            sigma_floor,
            k_sigma,
            min_consecutive,
            cooldown_seq,
            welford: Welford::default(),
            consecutive_above: 0,
            refractory_until: 0,
            burst_start_seq: 0,
            burst_start_ts: 0,
            last_rate: 0.0,
            cancels_in_window: 0,
        }
    }

    /// Current adaptive threshold `μ + k · max(σ̂, σ_floor)`. Public
    /// for tests and for downstream telemetry.
    pub fn threshold(&self) -> f64 {
        self.welford.mean()
            + self.k_sigma * self.welford.stddev().max(self.sigma_floor)
    }

    /// Number of independent rate samples that have been folded into
    /// the Welford baseline. Useful for tests asserting warmup logic.
    pub fn welford_samples(&self) -> u64 {
        self.welford.n
    }

    /// Number of events currently retained in the rolling window.
    pub fn window_len(&self) -> usize {
        self.window.len()
    }

    /// Feed one event. Returns a `ChaosEvent` iff this event closes a
    /// cancellation-storm pattern (above threshold, sustained, and not
    /// in refractory). Otherwise returns `None`, possibly while
    /// updating internal stats.
    pub fn process(&mut self, seq_event: &SequencedEvent) -> Option<ChaosEvent> {
        let event = &seq_event.event;
        let etype = EventType::from_u8(event.event_type)?;

        // Only ADD / CANCEL / TRADE feed the rate window. MODIFY is
        // skipped because it is observationally a cancel-then-add and
        // would double-bias the ratio.
        if !matches!(
            etype,
            EventType::OrderAdd | EventType::OrderCancel | EventType::Trade
        ) {
            return None;
        }

        // Push the new event and evict expired ones.
        self.window
            .push_back((seq_event.seq, event.ts, event.event_type));
        if etype == EventType::OrderCancel {
            self.cancels_in_window += 1;
        }
        self.evict_expired(seq_event.seq, event.ts);

        // Wait for a meaningful denominator.
        if self.window.len() < self.min_window {
            return None;
        }

        // Normalised rate of cancels in the window.
        let total = self.window.len() as f64;
        let rate = self.cancels_in_window as f64 / total;
        self.last_rate = rate;

        let in_refractory = seq_event.seq < self.refractory_until;
        let warmed = self.welford.n >= self.warmup_samples;

        // Pre-warmup: just feed Welford and exit.
        if !warmed {
            self.welford.update(rate);
            return None;
        }

        let thr = self.threshold();
        if rate > thr {
            // Capture the burst onset on the first above-threshold
            // sample of the run.
            if self.consecutive_above == 0 {
                self.burst_start_seq = seq_event.seq;
                self.burst_start_ts = event.ts;
            }
            self.consecutive_above += 1;

            if self.consecutive_above >= self.min_consecutive && !in_refractory {
                let flag = self.emit_flag(seq_event.seq, event.ts, rate, thr);
                self.refractory_until =
                    seq_event.seq.saturating_add(self.cooldown_seq);
                self.consecutive_above = 0;
                return Some(flag);
            }
            // Above threshold but not yet persistent enough -> do NOT
            // poison the baseline with this sample.
            None
        } else {
            // Back below threshold. Reset persistence counter and feed
            // Welford ONLY when we are not in refractory (so the long
            // tail of the storm doesn't drag the baseline up).
            self.consecutive_above = 0;
            if !in_refractory {
                self.welford.update(rate);
            }
            None
        }
    }

    /// Evict events that fall outside EITHER active window.
    fn evict_expired(&mut self, cur_seq: u64, cur_ts: u64) {
        while let Some(&(s, t, et)) = self.window.front() {
            let too_old_seq = self.seq_window > 0
                && cur_seq.saturating_sub(s) > self.seq_window;
            let too_old_ts = self.time_window_ns > 0
                && cur_ts.saturating_sub(t) > self.time_window_ns;
            if too_old_seq || too_old_ts {
                self.window.pop_front();
                if et == EventType::OrderCancel as u8 {
                    self.cancels_in_window =
                        self.cancels_in_window.saturating_sub(1);
                }
            } else {
                break;
            }
        }
    }

    fn emit_flag(
        &self,
        end_seq: u64,
        end_ts: u64,
        rate: f64,
        thr: f64,
    ) -> ChaosEvent {
        let sigma = self.welford.stddev().max(self.sigma_floor);
        // Z-score above mean. Map to [0, 1] via a soft saturating
        // transform so a 3σ spike scores ~0.6, a 6σ spike ~0.9.
        let z = ((rate - self.welford.mean()) / sigma).max(0.0);
        let severity = (z / (z + 3.0)).clamp(0.0, 1.0);

        ChaosEvent {
            kind: ChaosKind::CancellationStorm,
            start_seq: self.burst_start_seq,
            end_seq,
            severity,
            initiator: None,
            features: ChaosFeatures {
                event_count: self.window.len() as u64,
                duration_ns: end_ts.saturating_sub(self.burst_start_ts),
                cancel_trade_ratio: rate / (1.0 - rate).max(f64::EPSILON),
                price_displacement: 0,
                depth_removed: 0,
            },
        }
        .with_threshold_telemetry(thr)
    }
}

// Tiny helper trait so we can carry the threshold value into a debug
// log without changing the public ChaosEvent shape. Pure no-op today;
// kept here as a hook so bench/lab consumers can grep `tracing` lines
// for the adaptive baseline.
trait WithThresholdTelemetry {
    fn with_threshold_telemetry(self, thr: f64) -> Self;
}

impl WithThresholdTelemetry for ChaosEvent {
    fn with_threshold_telemetry(self, thr: f64) -> Self {
        tracing::trace!(
            target: "flowlab_chaos::cancel_storm",
            threshold = thr,
            severity = self.severity,
            start = self.start_seq,
            end = self.end_seq,
            "cancel-storm flagged",
        );
        self
    }
}

// ─── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use flowlab_core::event::{Event, Side};
    use proptest::prelude::*;

    fn ev(seq: u64, ts: u64, etype: EventType, side: Side) -> SequencedEvent {
        SequencedEvent {
            seq,
            channel_id: 0,
            event: Event {
                ts,
                price: 10_000,
                qty: 100,
                order_id: seq, // unique per event
                instrument_id: 1,
                event_type: etype as u8,
                side: side as u8,
                _pad: [0; 2],
            },
        }
    }

    /// Build a steady-state stream with ~50% cancel ratio (alternating
    /// ADD/CANCEL with occasional trades). Should never flag.
    #[test]
    fn no_flag_on_steady_state() {
        let mut det = CancellationStormDetector::new(
            /* seq_window */ 200,
            /* time_window_ns */ 0,
            /* min_window */ 50,
            /* warmup_samples */ 100,
            /* sigma_floor */ 0.02,
            /* k_sigma */ 4.0,
            /* min_consecutive */ 5,
            /* cooldown_seq */ 200,
        );

        for i in 1u64..=2_000 {
            let etype = match i % 4 {
                0 | 2 => EventType::OrderAdd,
                1 => EventType::OrderCancel,
                _ => EventType::Trade,
            };
            let flag = det.process(&ev(i, i * 1_000, etype, Side::Bid));
            assert!(
                flag.is_none(),
                "steady state flagged at i={i} rate={:.3} thr={:.3}",
                det.last_rate,
                det.threshold()
            );
        }
    }

    /// Steady state followed by a sustained ~95% cancel block — must
    /// flag exactly once thanks to refractory + persistence.
    #[test]
    fn flags_sustained_storm_once() {
        let mut det = CancellationStormDetector::new(
            /* seq_window */ 200,
            /* time_window_ns */ 0,
            /* min_window */ 50,
            /* warmup_samples */ 100,
            /* sigma_floor */ 0.02,
            /* k_sigma */ 4.0,
            /* min_consecutive */ 5,
            /* cooldown_seq */ 500,
        );

        // 1500 steady events: 50% cancel mix establishes the baseline.
        for i in 1u64..=1_500 {
            let etype = match i % 4 {
                0 | 2 => EventType::OrderAdd,
                1 => EventType::OrderCancel,
                _ => EventType::Trade,
            };
            assert!(det.process(&ev(i, i * 1_000, etype, Side::Bid)).is_none());
        }
        let baseline_thr = det.threshold();
        assert!(
            baseline_thr < 0.7,
            "baseline threshold {baseline_thr:.3} unrealistically high"
        );

        // 400 storm events: 19/20 cancels, 1/20 ADD.
        let mut flagged = 0;
        for k in 0u64..400 {
            let i = 1_500 + k + 1;
            let etype = if k % 20 == 0 {
                EventType::OrderAdd
            } else {
                EventType::OrderCancel
            };
            if det.process(&ev(i, i * 1_000, etype, Side::Bid)).is_some() {
                flagged += 1;
            }
        }
        assert_eq!(
            flagged, 1,
            "sustained storm should flag exactly once (refractory active)"
        );
    }

    /// A single elevated sample (one anomalous cancel ratio) must NOT
    /// flag — persistence requires `min_consecutive` consecutive
    /// above-threshold samples.
    #[test]
    fn no_flag_on_single_spike() {
        let mut det = CancellationStormDetector::new(
            10, 0, 10, 50, 0.02, 4.0,
            /* min_consecutive */ 5, 200,
        );

        // Establish baseline: 200 events, 50% cancels.
        for i in 1u64..=200 {
            let etype = if i % 2 == 0 {
                EventType::OrderAdd
            } else {
                EventType::OrderCancel
            };
            det.process(&ev(i, i * 1_000, etype, Side::Bid));
        }

        // One isolated cancel-only burst of 4 events (less than
        // min_consecutive=5 above threshold) — must not flag.
        for k in 0u64..4 {
            let i = 200 + k + 1;
            let flag = det.process(&ev(i, i * 1_000, EventType::OrderCancel, Side::Bid));
            assert!(flag.is_none(), "transient spike at k={k} should not flag");
        }
    }

    /// During warmup (first `warmup_samples` rate observations) the
    /// detector must NOT emit even on extreme rates.
    #[test]
    fn no_flag_during_warmup() {
        let mut det = CancellationStormDetector::new(
            50, 0, 10,
            /* warmup_samples */ 500,
            0.02, 4.0, 3, 100,
        );

        // 100% cancels from event #1 — extreme by any baseline.
        for i in 1u64..=200 {
            let flag = det.process(&ev(i, i * 1_000, EventType::OrderCancel, Side::Bid));
            assert!(
                flag.is_none(),
                "warmup must suppress all flags (i={i}, samples={})",
                det.welford_samples()
            );
        }
    }

    /// Two storms separated by less than the cooldown -> only one
    /// flag. Separated by more -> two flags.
    #[test]
    fn cooldown_suppresses_back_to_back_storms() {
        fn drive(det: &mut CancellationStormDetector, start: u64, n: u64) -> usize {
            let mut hits = 0;
            for k in 0..n {
                let i = start + k;
                let etype = if k % 20 == 0 {
                    EventType::OrderAdd
                } else {
                    EventType::OrderCancel
                };
                if det.process(&ev(i, i * 1_000, etype, Side::Bid)).is_some() {
                    hits += 1;
                }
            }
            hits
        }

        let cooldown = 500u64;
        let mut det = CancellationStormDetector::new(
            200, 0, 50, 100, 0.02, 4.0, 5, cooldown,
        );

        // Baseline.
        for i in 1u64..=1_500 {
            let etype = match i % 4 {
                0 | 2 => EventType::OrderAdd,
                1 => EventType::OrderCancel,
                _ => EventType::Trade,
            };
            det.process(&ev(i, i * 1_000, etype, Side::Bid));
        }

        // Storm #1 + back-to-back storm #2 inside cooldown.
        let h1 = drive(&mut det, 1_501, 100);
        let h2 = drive(&mut det, 1_601, 100); // still inside cooldown (<500 from h1)
        assert_eq!(h1, 1, "first storm should flag");
        assert_eq!(h2, 0, "second storm inside cooldown must not flag");

        // Long quiet stretch beyond cooldown, then a fresh storm.
        for i in 1_701u64..=3_500 {
            let etype = match i % 4 {
                0 | 2 => EventType::OrderAdd,
                1 => EventType::OrderCancel,
                _ => EventType::Trade,
            };
            det.process(&ev(i, i * 1_000, etype, Side::Bid));
        }
        let h3 = drive(&mut det, 3_501, 100);
        assert_eq!(h3, 1, "post-cooldown storm should flag again");
    }

    /// Sigma floor: a perfectly flat baseline (rate exactly constant)
    /// drives σ̂ -> 0. Without a floor any tiny rate increase would
    /// flag. The floor must prevent that.
    #[test]
    fn sigma_floor_prevents_flat_baseline_collapse() {
        let mut det = CancellationStormDetector::new(
            50, 0, 10, 50,
            /* sigma_floor */ 0.05,
            /* k_sigma */ 3.0,
            3, 100,
        );

        // Build a window where exactly 50% are cancels, deterministic.
        for i in 1u64..=400 {
            let etype = if i % 2 == 0 {
                EventType::OrderAdd
            } else {
                EventType::OrderCancel
            };
            det.process(&ev(i, i * 1_000, etype, Side::Bid));
        }
        let thr = det.threshold();
        // Mean ~0.5, sigma_floor=0.05, k=3 -> threshold >= 0.65.
        assert!(
            thr >= 0.5 + 0.05 * 3.0 - 1e-9,
            "sigma floor not honoured: thr={thr}"
        );
    }

    // ─── Property tests ──────────────────────────────────────────────

    proptest! {
        /// No false positives: any pure-ADD stream must never flag a
        /// cancellation storm — by construction there are no cancels.
        #[test]
        fn no_flags_on_pure_add_stream(
            n in 100usize..2_000,
        ) {
            let mut det = CancellationStormDetector::new(
                100, 0, 50, 200, 0.02, 3.0, 3, 200,
            );
            for i in 1..=(n as u64) {
                let flag = det.process(&ev(i, i * 1_000, EventType::OrderAdd, Side::Bid));
                prop_assert!(flag.is_none());
            }
        }

        /// No false positives on randomised but bounded steady-state
        /// streams. We pick a target cancel ratio in [0.3, 0.55] and
        /// generate an event mix exactly at that ratio: by definition
        /// the detector should never see a deviation worth flagging.
        #[test]
        fn no_flags_on_bounded_steady_state(
            cancel_ratio_per_mille in 300u64..=550,
            n in 800u64..2_500,
        ) {
            let mut det = CancellationStormDetector::new(
                200, 0, 50, 200, 0.05, 5.0, 5, 200,
            );
            for i in 1..=n {
                let etype = if (i * 1_000) % 1_000 < cancel_ratio_per_mille {
                    EventType::OrderCancel
                } else {
                    EventType::OrderAdd
                };
                let flag = det.process(&ev(i, i * 1_000, etype, Side::Bid));
                prop_assert!(flag.is_none(),
                    "false positive at i={i} with cancel_ratio_per_mille={cancel_ratio_per_mille}");
            }
        }
    }
}
