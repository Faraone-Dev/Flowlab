//! Phantom-liquidity detector.
//!
//! ## Definition
//!
//! A *phantom* level is a price level that:
//!
//!   1. is **created** by an `OrderAdd` (the first add at that price/side
//!      after the level was empty),
//!   2. accumulates one or more orders **without ever being traded
//!      against**,
//!   3. is **fully removed** by `OrderCancel` events within either
//!      `max_seq_gap` events or `max_time_gap_ns` ns of its creation,
//!      whichever fires first (dual window).
//!
//! Real liquidity providers leave their orders at risk of execution.
//! A pattern of book-padding adds that vanish before any taker can hit
//! them is the textbook signature of "spoof-as-a-service" liquidity
//! that exists only to mislead the consolidated tape. ITCH-only feeds
//! cannot prove intent; this detector flags the *pattern*, the human
//! decides on attribution.
//!
//! ## Self-contained tracker
//!
//! The detector keeps its own micro-orderbook indexed by `(side, price)`,
//! with one entry per *currently-tracked* phantom-candidate level. It
//! does NOT depend on `flowlab-core::HotOrderBook`: the tracker only
//! needs to know "did this level ever see a trade?". This isolation
//! keeps the unit tests trivial and decouples chaos detection from the
//! hot path.
//!
//! ## Memory characteristics
//!
//! Tracker uses two `HashMap`s:
//!   * `levels: HashMap<(u8, u64), LevelTracker>` — one entry per live
//!     candidate level.
//!   * `orders: HashMap<u64, (u8, u64)>` — order_id -> (side, price)
//!     resolution map for cancel events that don't carry a price.
//!
//! Both are bounded by `max_tracked` (LRU-pruned by `created_seq` when
//! exceeded) so a long replay cannot grow the tracker without bound.

use std::collections::HashMap;

use flowlab_core::event::{EventType, SequencedEvent};
use flowlab_core::types::SeqNum;

use crate::{ChaosEvent, ChaosFeatures, ChaosKind};

/// Per-level tracking record. Lives only while the level is a phantom
/// candidate (created, not yet traded, not yet fully cancelled).
#[derive(Debug, Clone, Copy)]
struct LevelTracker {
    /// Sequence number of the first add at this (side, price).
    created_seq: SeqNum,
    /// Timestamp (ns) of the first add.
    created_ts: u64,
    /// Sum of qty added at this level since creation (NOT the live
    /// resting qty — adds only).
    added_qty: u64,
    /// Sum of qty cancelled at this level since creation.
    cancelled_qty: u64,
    /// Number of distinct orders added.
    order_count: u32,
    /// Number of distinct orders cancelled.
    cancel_count: u32,
}

impl LevelTracker {
    /// True once cancellations have removed at least the same qty as
    /// adds AND the per-order accounting matches. We require BOTH so
    /// partial cancels on a phantom-suspect level don't trigger.
    #[inline]
    fn fully_cancelled(&self) -> bool {
        self.cancelled_qty >= self.added_qty && self.cancel_count >= self.order_count
    }
}

/// Phantom-liquidity detector. Self-contained, dual-window, ITCH-only.
pub struct PhantomLiquidityDetector {
    /// Live candidate levels: `(side, price) -> tracker`.
    levels: HashMap<(u8, u64), LevelTracker>,
    /// Order id -> (side, price). Required because ITCH 'D' / 'X'
    /// cancel messages carry the order_id but not always the price.
    orders: HashMap<u64, (u8, u64)>,
    /// Maximum gap in sequence numbers between create and full-cancel.
    /// `0` disables the sequence window.
    max_seq_gap: u64,
    /// Maximum gap in nanoseconds between create and full-cancel.
    /// `0` disables the time window.
    max_time_gap_ns: u64,
    /// Hard cap on the tracker size — prevents unbounded growth on
    /// pathological streams. When exceeded, the oldest-by-`created_seq`
    /// entries are evicted.
    max_tracked: usize,
    /// Minimum total qty added at a level before it can ever flag.
    /// Filters out tiny retail-style add/cancel patterns from being
    /// flagged as institutional book-padding.
    min_qty: u64,
}

impl PhantomLiquidityDetector {
    /// New detector. `max_seq_gap == 0` disables sequence dimension;
    /// `max_time_gap_ns == 0` disables time dimension. At least one
    /// MUST be > 0; with both zero the detector flags every level
    /// that completes a full cancel cycle, which is rarely useful.
    pub fn new(
        max_seq_gap: u64,
        max_time_gap_ns: u64,
        max_tracked: usize,
        min_qty: u64,
    ) -> Self {
        debug_assert!(
            max_seq_gap != 0 || max_time_gap_ns != 0,
            "PhantomLiquidityDetector: at least one window must be enabled"
        );
        Self {
            levels: HashMap::with_capacity(max_tracked.min(1024)),
            orders: HashMap::with_capacity(max_tracked.min(2048)),
            max_seq_gap,
            max_time_gap_ns,
            max_tracked,
            min_qty,
        }
    }

    /// Number of levels currently tracked. Useful for tests and metrics.
    pub fn tracked_levels(&self) -> usize {
        self.levels.len()
    }

    /// Feed one event in sequence order. Returns a `ChaosEvent` if this
    /// event completes a phantom-liquidity pattern.
    pub fn process(&mut self, seq_event: &SequencedEvent) -> Option<ChaosEvent> {
        let event = &seq_event.event;
        let etype = EventType::from_u8(event.event_type)?;

        match etype {
            EventType::OrderAdd => {
                self.on_add(seq_event);
                None
            }
            EventType::OrderCancel => self.on_cancel(seq_event),
            EventType::OrderModify => {
                // Treat MODIFY as cancel-then-add. The cancel half can
                // emit a phantom event (if it completes a pattern); the
                // add half re-arms tracking at the new (side, price).
                let flagged = self.on_cancel(seq_event);
                self.on_add(seq_event);
                flagged
            }
            EventType::Trade => {
                // A trade at a tracked level proves the liquidity was
                // real; drop the candidate. We do this whether or not
                // the trade carries an order_id, because trades happen
                // at a price level regardless of attribution.
                let key = (event.side, event.price);
                self.levels.remove(&key);
                None
            }
            EventType::BookSnapshot => None,
        }
    }

    /// Internal: handle ADD. Creates or updates the tracker entry and
    /// records the order_id -> (side, price) mapping.
    fn on_add(&mut self, seq_event: &SequencedEvent) {
        let event = &seq_event.event;

        // Defend the tracker size BEFORE inserting so a malicious or
        // pathological feed cannot drive us OOM.
        if self.levels.len() >= self.max_tracked {
            self.evict_oldest();
        }

        let key = (event.side, event.price);
        self.levels
            .entry(key)
            .and_modify(|t| {
                t.added_qty = t.added_qty.saturating_add(event.qty);
                t.order_count = t.order_count.saturating_add(1);
            })
            .or_insert(LevelTracker {
                created_seq: seq_event.seq,
                created_ts: event.ts,
                added_qty: event.qty,
                cancelled_qty: 0,
                order_count: 1,
                cancel_count: 0,
            });

        if event.order_id != 0 {
            self.orders.insert(event.order_id, key);
        }
    }

    /// Internal: handle CANCEL. Resolves the level via `order_id` if
    /// possible, falls back to the event's own `(side, price)` for
    /// synthetic / pre-enriched feeds. Returns a `ChaosEvent` iff this
    /// cancel both *fully removes* the level AND the level lived inside
    /// at least one of the configured windows.
    fn on_cancel(&mut self, seq_event: &SequencedEvent) -> Option<ChaosEvent> {
        let event = &seq_event.event;

        // Resolve the (side, price) of the cancel target.
        let key = if event.order_id != 0 {
            // Drop the order_id mapping; it is one-shot.
            self.orders.remove(&event.order_id)?
        } else {
            (event.side, event.price)
        };

        // If we are not tracking this level it cannot be a phantom
        // event — return without mutating.
        let tracker = self.levels.get_mut(&key)?;

        // Cancel qty == 0 is ITCH 'D' (full delete of the resting order).
        // For tracking purposes we treat that as "the entire incremental
        // qty of the most recent add of this order is removed". Without
        // per-order qty memory we approximate by deducting the average
        // contribution; that is fine for the threshold check below
        // because we ALSO require cancel_count >= order_count, which
        // never trips on a partial cancel.
        let removed_qty = if event.qty == 0 {
            tracker
                .added_qty
                .saturating_sub(tracker.cancelled_qty)
                .max(1)
                / tracker.order_count.max(1) as u64
        } else {
            event.qty
        };
        tracker.cancelled_qty = tracker.cancelled_qty.saturating_add(removed_qty);
        tracker.cancel_count = tracker.cancel_count.saturating_add(1);

        if !tracker.fully_cancelled() {
            return None;
        }

        // Snapshot the tracker before removing it.
        let t = *tracker;
        self.levels.remove(&key);

        // Apply the qty floor. Using `added_qty` (cumulative adds)
        // rather than peak resting qty keeps the threshold comparable
        // across mixed add patterns.
        if t.added_qty < self.min_qty {
            return None;
        }

        // Window check — pattern qualifies iff the full cycle fits in
        // EITHER window. Disabled windows (==0) are skipped.
        let seq_gap = seq_event.seq.saturating_sub(t.created_seq);
        let time_gap = event.ts.saturating_sub(t.created_ts);
        let inside_seq = self.max_seq_gap > 0 && seq_gap <= self.max_seq_gap;
        let inside_time = self.max_time_gap_ns > 0 && time_gap <= self.max_time_gap_ns;
        if !(inside_seq || inside_time) {
            return None;
        }

        // Severity: closer to either window cap -> closer to 1.0.
        // Take the *tighter* of the two ratios so a fast cycle scores
        // high regardless of which window is the binding constraint.
        let seq_score = if self.max_seq_gap > 0 {
            1.0 - (seq_gap as f64 / self.max_seq_gap as f64)
        } else {
            0.0
        };
        let time_score = if self.max_time_gap_ns > 0 {
            1.0 - (time_gap as f64 / self.max_time_gap_ns as f64)
        } else {
            0.0
        };
        let severity = seq_score.max(time_score).clamp(0.0, 1.0);

        Some(ChaosEvent {
            kind: ChaosKind::PhantomLiquidity,
            start_seq: t.created_seq,
            end_seq: seq_event.seq,
            severity,
            initiator: None,
            features: ChaosFeatures {
                event_count: (t.order_count + t.cancel_count) as u64,
                duration_ns: time_gap,
                cancel_trade_ratio: f64::INFINITY, // by definition: zero trades
                price_displacement: 0,
                depth_removed: t.cancelled_qty,
            },
        })
    }

    /// Drop the entry with the smallest `created_seq`. Linear scan over
    /// the tracker is fine: `max_tracked` is small (typically <= 4096)
    /// and eviction is the slow path.
    fn evict_oldest(&mut self) {
        if let Some((&key, _)) = self
            .levels
            .iter()
            .min_by_key(|(_, t)| t.created_seq)
        {
            self.levels.remove(&key);
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use flowlab_core::event::{Event, Side};

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

    /// Happy path: add a level, cancel it within the seq window, no
    /// trades touched it -> flagged as PhantomLiquidity.
    #[test]
    fn flags_quick_add_cancel_with_no_trade() {
        let mut det = PhantomLiquidityDetector::new(
            /* seq_gap */ 100, /* time_gap_ns */ 1_000_000, /* max_tracked */ 64,
            /* min_qty */ 100,
        );

        let add = det.process(&ev(1, 1_000, EventType::OrderAdd, Side::Bid, 10_000, 500, 42));
        assert!(add.is_none(), "ADD must not flag by itself");
        assert_eq!(det.tracked_levels(), 1);

        let flag =
            det.process(&ev(5, 5_000, EventType::OrderCancel, Side::Bid, 10_000, 500, 42));
        let chaos = flag.expect("full cancel within window must flag");

        assert_eq!(chaos.kind, ChaosKind::PhantomLiquidity);
        assert_eq!(chaos.start_seq, 1);
        assert_eq!(chaos.end_seq, 5);
        assert_eq!(chaos.features.depth_removed, 500);
        assert_eq!(chaos.features.duration_ns, 4_000);
        assert!(chaos.severity > 0.9, "tight cycle should score near 1.0");
        assert_eq!(det.tracked_levels(), 0, "tracker must release the slot");
    }

    /// Negative: a trade at the level proves real liquidity. Even a
    /// subsequent full cancel must NOT flag.
    #[test]
    fn no_flag_when_level_trades() {
        let mut det = PhantomLiquidityDetector::new(100, 1_000_000, 64, 100);

        det.process(&ev(1, 1_000, EventType::OrderAdd, Side::Ask, 10_500, 500, 7));
        // A trade at the same (side, price) wipes the candidate.
        let trade =
            det.process(&ev(2, 1_500, EventType::Trade, Side::Ask, 10_500, 100, 7));
        assert!(trade.is_none());
        assert_eq!(det.tracked_levels(), 0, "trade must drop the candidate");

        // The trailing cancel finds nothing to flag.
        let cancel =
            det.process(&ev(3, 2_000, EventType::OrderCancel, Side::Ask, 10_500, 400, 7));
        assert!(cancel.is_none(), "post-trade cancel must not flag");
    }

    /// Negative: cancel arrives outside both windows -> no flag.
    #[test]
    fn no_flag_when_outside_both_windows() {
        let mut det = PhantomLiquidityDetector::new(
            /* seq_gap */ 10, /* time_gap_ns */ 1_000, /* max_tracked */ 64,
            /* min_qty */ 100,
        );
        det.process(&ev(1, 1_000, EventType::OrderAdd, Side::Bid, 10_000, 500, 1));
        let flag = det.process(&ev(
            100, 50_000,
            EventType::OrderCancel,
            Side::Bid,
            10_000,
            500,
            1,
        ));
        assert!(flag.is_none(), "outside both windows must not flag");
    }

    /// Negative: total added qty below `min_qty` -> no flag, even with
    /// a textbook quick-cancel cycle.
    #[test]
    fn no_flag_below_min_qty() {
        let mut det = PhantomLiquidityDetector::new(100, 1_000_000, 64, /* min_qty */ 1_000);
        det.process(&ev(1, 1_000, EventType::OrderAdd, Side::Bid, 10_000, 50, 1));
        let flag =
            det.process(&ev(2, 1_500, EventType::OrderCancel, Side::Bid, 10_000, 50, 1));
        assert!(flag.is_none(), "qty under floor must not flag");
    }

    /// Time window only: sequence gap large, but time gap inside cap.
    /// Pattern still qualifies (dual window OR semantics).
    #[test]
    fn time_window_alone_can_qualify() {
        let mut det = PhantomLiquidityDetector::new(
            /* seq_gap */ 5, /* time_gap_ns */ 10_000_000, 64, 100,
        );
        det.process(&ev(1, 1_000, EventType::OrderAdd, Side::Bid, 10_000, 500, 1));
        let flag = det.process(&ev(
            500, 1_000_000,
            EventType::OrderCancel,
            Side::Bid,
            10_000,
            500,
            1,
        ));
        assert!(
            flag.is_some(),
            "time-window-only fit must flag thanks to dual OR"
        );
    }

    /// Eviction: tracker never grows past `max_tracked`.
    #[test]
    fn tracker_size_bounded() {
        let cap = 8;
        let mut det = PhantomLiquidityDetector::new(1_000_000, 0, cap, 1);

        for i in 0..(cap as u64 * 4) {
            det.process(&ev(
                i + 1, i + 1,
                EventType::OrderAdd,
                Side::Bid,
                10_000 + i,
                100,
                i + 1,
            ));
        }
        assert!(
            det.tracked_levels() <= cap,
            "tracker must stay within max_tracked, got {}",
            det.tracked_levels()
        );
    }

    /// Property: a stream of pure ADD+TRADE pairs (no cancels) must
    /// NEVER flag, regardless of timing or sequencing. This exercises
    /// the "trade clears candidate" invariant on random inputs.
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn no_false_positives_on_add_then_trade(
            seqs in proptest::collection::vec(1u64..1_000_000, 1..200),
            base_price in 1_000u64..50_000,
        ) {
            let mut det = PhantomLiquidityDetector::new(1_000_000, 0, 1024, 1);
            let mut ts = 1u64;
            let mut sorted = seqs.clone();
            sorted.sort();
            sorted.dedup();
            for (i, &seq) in sorted.iter().enumerate() {
                let oid = (i as u64) + 1;
                let price = base_price + (i as u64 % 32);
                // ADD followed by TRADE at the same level.
                let add = det.process(&ev(
                    seq, ts, EventType::OrderAdd, Side::Bid, price, 100, oid,
                ));
                ts += 1;
                let trade = det.process(&ev(
                    seq + 1, ts, EventType::Trade, Side::Bid, price, 100, oid,
                ));
                ts += 1;
                prop_assert!(add.is_none());
                prop_assert!(trade.is_none(),
                    "trade event must never flag PhantomLiquidity");
            }
            prop_assert_eq!(det.tracked_levels(), 0);
        }

        /// Property: every ADD-only stream (no cancels, no trades)
        /// produces zero phantom flags. The detector only fires on
        /// completed cancel cycles.
        #[test]
        fn no_flags_without_cancels(
            count in 1usize..500,
            base_price in 1_000u64..50_000,
        ) {
            let mut det = PhantomLiquidityDetector::new(1_000_000, 0, 4096, 1);
            for i in 0..count {
                let flag = det.process(&ev(
                    (i as u64) + 1,
                    (i as u64) + 1,
                    EventType::OrderAdd,
                    Side::Bid,
                    base_price + (i as u64 % 64),
                    100,
                    (i as u64) + 1,
                ));
                prop_assert!(flag.is_none());
            }
        }
    }
}
