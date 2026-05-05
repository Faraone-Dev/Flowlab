// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

//! Detector chain with stable output ordering.
//!
//! Single call site for the full chaos pipeline. Detectors are
//! independent (each owns its mini-book + counters); order only
//! affects flag ordering in the output vec, kept stable for
//! reproducible tests/reports:
//!
//!   1. PhantomLiquidity   2. CancellationStorm   3. MomentumIgnition
//!   4. FlashCrash         5. LatencyArbProxy
//!
//! `ChaosChain::default_itch()` ships conservative ITCH 5.0 tunings
//! aiming for <0.5% flag rate on a clean session.

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

    /// Run the full chain into a caller-owned buffer; **no per-event allocation**.
    ///
    /// `out` is appended to (not cleared) so the caller controls reuse policy:
    /// either pass a fresh `Vec` per event for ergonomic use, or — in a hot
    /// loop — `out.clear()` and reuse the same buffer across millions of events
    /// to keep the chain at zero heap traffic.
    ///
    /// Output ordering is the documented stable order:
    /// QuoteStuff → Spoof → PhantomLiquidity → CancellationStorm →
    /// MomentumIgnition → FlashCrash → LatencyArbProxy.
    #[inline]
    pub fn process_into(&mut self, seq_event: &SequencedEvent, out: &mut Vec<ChaosEvent>) {
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
    }

    /// Ergonomic wrapper around [`Self::process_into`] that allocates a fresh
    /// `Vec` per event. Convenient at one-off call sites and tests; for hot
    /// loops prefer `process_into` with a reused buffer.
    #[inline]
    pub fn process(&mut self, seq_event: &SequencedEvent) -> Vec<ChaosEvent> {
        let mut out: Vec<ChaosEvent> = Vec::new();
        self.process_into(seq_event, &mut out);
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

    /// `process` and `process_into` must produce identical kind sequences
    /// over an identical event stream — they share the same body and the
    /// only diff is who owns the buffer. This test pins that contract so a
    /// future refactor cannot accidentally diverge the two APIs.
    #[test]
    fn process_into_matches_process() {
        // Same script as the phantom-liquidity test, plus a quote-stuff burst
        // so we exercise multiple detector branches.
        let stream: Vec<SequencedEvent> = {
            let mut s = Vec::new();
            s.push(ev(1, 1_000, EventType::OrderAdd, Side::Bid, 10_000, 1_000, 42));
            s.push(ev(5, 5_000, EventType::OrderCancel, Side::Bid, 10_000, 1_000, 42));
            for i in 0..4 {
                let seq = 100 + i;
                s.push(ev(seq, seq * 1_000, EventType::Trade, Side::Ask, 10_001, 100, seq));
            }
            for i in 0..36 {
                let seq = 200 + i;
                s.push(ev(seq, seq * 1_000, EventType::OrderCancel, Side::Bid, 9_999, 100, seq));
            }
            s
        };

        let mut a = ChaosChain::default_itch();
        let from_alloc: Vec<ChaosKind> = stream
            .iter()
            .flat_map(|e| a.process(e).into_iter().map(|f| f.kind))
            .collect();

        let mut b = ChaosChain::default_itch();
        let mut buf: Vec<ChaosEvent> = Vec::with_capacity(8);
        let mut from_into: Vec<ChaosKind> = Vec::new();
        for e in &stream {
            buf.clear();
            b.process_into(e, &mut buf);
            from_into.extend(buf.iter().map(|f| f.kind));
        }

        assert_eq!(
            from_alloc, from_into,
            "process and process_into diverged: alloc={from_alloc:?} into={from_into:?}"
        );
        // Counters must agree too — they're updated by the same `bump`.
        for k in [
            ChaosKind::QuoteStuff,
            ChaosKind::Spoof,
            ChaosKind::PhantomLiquidity,
            ChaosKind::CancellationStorm,
            ChaosKind::MomentumIgnition,
            ChaosKind::FlashCrash,
            ChaosKind::LatencyArbitrage,
        ] {
            assert_eq!(a.count(k), b.count(k), "counter mismatch for {k:?}");
        }
    }
}
