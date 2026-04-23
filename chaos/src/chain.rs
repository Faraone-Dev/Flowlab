//! Detector chain with stable output ordering.
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
use crate::detection::{QuoteStuffDetector, SpoofDetector};
use crate::flash_crash::FlashCrashDetector;
use crate::latency_arb_proxy::LatencyArbProxyDetector;
use crate::momentum_ignition::MomentumIgnitionDetector;
use crate::phantom_liquidity::PhantomLiquidityDetector;
use crate::{ChaosEvent, ChaosKind};

pub struct ChaosChain {
    quote: QuoteStuffDetector,
    spoof: SpoofDetector,
    phantom: PhantomLiquidityDetector,
    storm: CancellationStormDetector,
    ignition: MomentumIgnitionDetector,
    crash: FlashCrashDetector,
    proxy: LatencyArbProxyDetector,
    counts: [u64; 7],
}

impl ChaosChain {
    pub fn new(
        quote: QuoteStuffDetector,
        spoof: SpoofDetector,
        phantom: PhantomLiquidityDetector,
        storm: CancellationStormDetector,
        ignition: MomentumIgnitionDetector,
        crash: FlashCrashDetector,
        proxy: LatencyArbProxyDetector,
    ) -> Self {
        Self {
            quote,
            spoof,
            phantom,
            storm,
            ignition,
            crash,
            proxy,
            counts: [0; 7],
        }
    }

    /// Default tuning for order-referenced ITCH-like streams.
    pub fn default_itch() -> Self {
        Self::new(
            QuoteStuffDetector::new(48, 8.0),
            SpoofDetector::new(1_000, 64),
            PhantomLiquidityDetector::new(
                256,
                5_000_000,
                4096,
                100,
            ),
            CancellationStormDetector::new(
                2_048,
                0,
                128,
                1_024,
                0.05,
                4.0,
                8,
                4_096,
            ),
            MomentumIgnitionDetector::new(
                1_024,
                0,
                64,
                5,
                8,
                4,
                2_048,
            ),
            FlashCrashDetector::new(
                10,
                8,
                64,
                5,
                0.6,
                3,
                2_048,
            ),
            LatencyArbProxyDetector::new(
                64,
                0,
                64,
                4,
                200,
                4,
                false,
                0,
                1_024,
            ),
        )
    }

    #[inline]
    pub fn process(&mut self, seq_event: &SequencedEvent) -> Vec<ChaosEvent> {
        let mut out: Vec<ChaosEvent> = Vec::new();
        if let Some(e) = self.quote.process(seq_event) {
            self.bump(&e);
            out.push(e);
        }
        if let Some(e) = self.spoof.process(seq_event) {
            self.bump(&e);
            out.push(e);
        }
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

    pub fn count(&self, kind: ChaosKind) -> u64 {
        self.counts[Self::kind_index(kind)]
    }

    pub fn total_flags(&self) -> u64 {
        self.counts.iter().sum()
    }

    fn bump(&mut self, e: &ChaosEvent) {
        let i = Self::kind_index(e.kind);
        self.counts[i] = self.counts[i].saturating_add(1);
    }

    #[inline]
    fn kind_index(kind: ChaosKind) -> usize {
        match kind {
            ChaosKind::QuoteStuff => 0,
            ChaosKind::Spoof => 1,
            ChaosKind::PhantomLiquidity => 2,
            ChaosKind::CancellationStorm => 3,
            ChaosKind::MomentumIgnition => 4,
            ChaosKind::FlashCrash => 5,
            ChaosKind::LatencyArbitrage => 6,
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

    #[test]
    fn chain_propagates_spoof_flag() {
        let mut chain = ChaosChain::default_itch();
        let add = chain.process(&ev(1, 1_000, EventType::OrderAdd, Side::Bid, 10_000, 5_000, 77));
        assert!(add.is_empty());
        let cancel = chain.process(&ev(2, 2_000, EventType::OrderCancel, Side::Bid, 10_000, 5_000, 77));
        assert!(cancel.iter().any(|e| e.kind == ChaosKind::Spoof));
        assert!(chain.count(ChaosKind::Spoof) >= 1);
    }

    #[test]
    fn chain_propagates_quote_stuff_flag() {
        let mut chain = ChaosChain::default_itch();
        let mut seen = false;
        let mut seq = 1u64;

        for _ in 0..4 {
            let _ = chain.process(&ev(seq, seq * 1_000, EventType::Trade, Side::Ask, 10_001, 100, seq));
            seq += 1;
        }
        for _ in 0..36 {
            let flags = chain.process(&ev(seq, seq * 1_000, EventType::OrderCancel, Side::Bid, 9_999, 100, seq));
            if flags.iter().any(|e| e.kind == ChaosKind::QuoteStuff) {
                seen = true;
                break;
            }
            seq += 1;
        }

        assert!(seen);
        assert!(chain.count(ChaosKind::QuoteStuff) >= 1);
    }
}
