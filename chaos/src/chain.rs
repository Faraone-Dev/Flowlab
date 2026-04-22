//! Detector chain: feeds one event into all five chaos detectors in
//! deterministic order and collects the union of their flags.
//!
//! ## Why a single struct
//!
//! Consumers (`lab::executor`, `bench/chaos_throughput`, future
//! replay tooling) need a *single* call site that runs the full chaos
//! analysis with predictable cost. Wiring five detectors by hand at
//! every call site is brittle and makes ordering implicit. A chain
//! type fixes the order, exposes default-tuned constructors for each
//! detector, and gives a single hook for future additions.
//!
//! ## Ordering rationale
//!
//! Detectors are independent — each owns its mini-book and counters —
//! so the order does not affect *correctness*. It does, however,
//! affect the *order in which flags appear in the output vector* when
//! a single event triggers more than one detector. We keep the order
//! stable so test assertions and reports are reproducible:
//!
//!   1. PhantomLiquidity         — purely cancel-cycle topology
//!   2. CancellationStorm        — adaptive rate baseline
//!   3. MomentumIgnition         — sustained directional drift
//!   4. FlashCrash               — instantaneous gap + vacuum
//!   5. LatencyArbProxy          — reaction-to-print burst
//!
//! ## Default tuning
//!
//! `ChaosChain::default_itch()` builds a chain calibrated for raw ITCH
//! 5.0 (US equities 100 µs cadence, 8-decimal price scale). The
//! parameters are conservative: each detector aims for < 0.5 % flag
//! rate on a clean trading day. They will be re-tuned (and pinned in
//! `chaos/README.md`) once we have benchmark numbers from a real
//! recording.

use flowlab_core::event::SequencedEvent;

use crate::cancellation_storm::CancellationStormDetector;
use crate::flash_crash::FlashCrashDetector;
use crate::latency_arb_proxy::LatencyArbProxyDetector;
use crate::momentum_ignition::MomentumIgnitionDetector;
use crate::phantom_liquidity::PhantomLiquidityDetector;
use crate::{ChaosEvent, ChaosKind};

pub struct ChaosChain {
    phantom: PhantomLiquidityDetector,
    storm: CancellationStormDetector,
    ignition: MomentumIgnitionDetector,
    crash: FlashCrashDetector,
    proxy: LatencyArbProxyDetector,

    /// Cumulative per-kind flag counts. Useful for reports without
    /// having to rescan the full event vector.
    counts: [u64; 5],
}

impl ChaosChain {
    /// Build a chain from explicit, tuned detectors. Use this when you
    /// want full control of every parameter.
    pub fn new(
        phantom: PhantomLiquidityDetector,
        storm: CancellationStormDetector,
        ignition: MomentumIgnitionDetector,
        crash: FlashCrashDetector,
        proxy: LatencyArbProxyDetector,
    ) -> Self {
        Self {
            phantom,
            storm,
            ignition,
            crash,
            proxy,
            counts: [0; 5],
        }
    }

    /// Conservative default tuning for raw ITCH 5.0 streams. Numbers
    /// are placeholders until benched on real recording (April 2026:
    /// see `chaos/README.md` once the calibration table lands).
    pub fn default_itch() -> Self {
        Self::new(
            PhantomLiquidityDetector::new(
                /* max_seq_gap */ 256,
                /* max_time_gap_ns */ 5_000_000,
                /* max_tracked */ 4096,
                /* min_qty */ 100,
            ),
            CancellationStormDetector::new(
                /* seq_window */ 2_048,
                /* time_window_ns */ 0,
                /* min_window */ 128,
                /* warmup_samples */ 1_024,
                /* sigma_floor */ 0.05,
                /* k_sigma */ 4.0,
                /* min_consecutive */ 8,
                /* cooldown_seq */ 4_096,
            ),
            MomentumIgnitionDetector::new(
                /* seq_window */ 1_024,
                /* time_window_ns */ 0,
                /* max_book_levels */ 64,
                /* min_move_bps */ 5,
                /* min_trades */ 8,
                /* min_consecutive */ 4,
                /* cooldown_seq */ 2_048,
            ),
            FlashCrashDetector::new(
                /* min_gap_bps */ 10,
                /* gap_window_events */ 8,
                /* max_book_levels */ 64,
                /* depth_band_ticks */ 5,
                /* depth_drop_pct */ 0.6,
                /* min_aggr_trades */ 3,
                /* cooldown_seq */ 2_048,
            ),
            LatencyArbProxyDetector::new(
                /* reaction_seq */ 64,
                /* reaction_time_ns */ 0,
                /* max_book_levels */ 64,
                /* band_ticks */ 4,
                /* min_trade_qty */ 200,
                /* min_burst */ 4,
                /* require_reversion */ false,
                /* reversion_bps */ 0,
                /* cooldown_seq */ 1_024,
            ),
        )
    }

    /// Feed one event into every detector and collect all emitted
    /// flags in deterministic order. `Vec` is allocated only when at
    /// least one detector fires; the empty case allocates nothing
    /// because we use a stack-sized check first.
    #[inline]
    pub fn process(&mut self, seq_event: &SequencedEvent) -> Vec<ChaosEvent> {
        let mut out: Vec<ChaosEvent> = Vec::new();
        if let Some(e) = self.phantom.process(seq_event) {
            self.bump(&e);
            out.push(e);
        }
        if let Some(e) = self.storm.process(seq_event) {
            self.bump(&e);
            out.push(e);
        }
        if let Some(e) = self.ignition.process(seq_event) {
            self.bump(&e);
            out.push(e);
        }
        if let Some(e) = self.crash.process(seq_event) {
            self.bump(&e);
            out.push(e);
        }
        if let Some(e) = self.proxy.process(seq_event) {
            self.bump(&e);
            out.push(e);
        }
        out
    }

    /// Cumulative flag count for a given chaos kind. `0` for kinds
    /// not produced by this chain (`QuoteStuff`, `Spoof`).
    pub fn count(&self, kind: ChaosKind) -> u64 {
        match Self::kind_index(kind) {
            Some(i) => self.counts[i],
            None => 0,
        }
    }

    /// Total flags emitted across all detectors.
    pub fn total_flags(&self) -> u64 {
        self.counts.iter().sum()
    }

    fn bump(&mut self, e: &ChaosEvent) {
        if let Some(i) = Self::kind_index(e.kind) {
            self.counts[i] = self.counts[i].saturating_add(1);
        }
    }

    /// Compact mapping from `ChaosKind` to the dense `counts` array.
    #[inline]
    fn kind_index(kind: ChaosKind) -> Option<usize> {
        match kind {
            ChaosKind::PhantomLiquidity => Some(0),
            ChaosKind::CancellationStorm => Some(1),
            ChaosKind::MomentumIgnition => Some(2),
            ChaosKind::FlashCrash => Some(3),
            ChaosKind::LatencyArbitrage => Some(4),
            // Unmapped variants — not produced by this chain.
            ChaosKind::QuoteStuff | ChaosKind::Spoof => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flowlab_core::event::{Event, EventType, Side};

    fn ev(
        seq: u64,
        ts: u64,
        etype: EventType,
        side: Side,
        price: u64,
        qty: u64,
        order_id: u64,
    ) -> SequencedEvent {
        SequencedEvent {
            seq,
            channel_id: 0,
            event: Event {
                ts,
                price,
                qty,
                order_id,
                instrument_id: 1,
                event_type: etype as u8,
                side: side as u8,
                _pad: [0; 2],
            },
        }
    }

    /// A pure ADD-only stream produces zero flags from any detector.
    /// This is the baseline noise-floor smoke test for the chain.
    #[test]
    fn no_flags_on_idle_stream() {
        let mut chain = ChaosChain::default_itch();
        for i in 1u64..=2_000 {
            let side = if i % 2 == 0 { Side::Bid } else { Side::Ask };
            let px = if side == Side::Bid {
                10_000 - (i % 16)
            } else {
                10_000 + (i % 16)
            };
            let flags = chain.process(&ev(i, i * 1_000, EventType::OrderAdd, side, px, 100, i));
            assert!(flags.is_empty(), "idle stream flagged at i={i}");
        }
        assert_eq!(chain.total_flags(), 0);
    }

    /// A textbook phantom-liquidity cycle (large add, full cancel
    /// before any trade) at the start of the stream still flags via
    /// the chain wrapper, proving wiring is correct.
    #[test]
    fn chain_propagates_phantom_flag() {
        let mut chain = ChaosChain::default_itch();
        // Big add then quick full cancel. Same order_id so the phantom
        // detector links the cancel back to the original add.
        let f1 = chain.process(&ev(1, 1_000, EventType::OrderAdd, Side::Bid, 10_000, 1_000, 42));
        assert!(f1.is_empty());
        let f2 = chain.process(&ev(5, 5_000, EventType::OrderCancel, Side::Bid, 10_000, 1_000, 42));
        assert!(
            f2.iter().any(|e| e.kind == ChaosKind::PhantomLiquidity),
            "chain failed to surface phantom flag: {f2:?}"
        );
        assert!(chain.count(ChaosKind::PhantomLiquidity) >= 1);
    }
}
