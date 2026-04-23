//! Latency-arbitrage *reaction proxy*.
//!
//! ## Honest naming
//!
//! On a single ITCH feed you cannot prove cross-venue arbitrage; you
//! lack the second feed needed to show one venue *moved before* the
//! other. What you can measure is a *behavioural proxy*: a market
//! taker prints at T₀, and within a tiny window, a burst of `OrderAdd`
//! / `OrderCancel` events lands on the **opposite side** in the same
//! micro price-band. That is the observable shadow of someone reacting
//! to (or anticipating) the print — possibly because they saw the same
//! information on a faster venue, possibly for unrelated reasons.
//! The detector flags the *pattern*; the analyst decides on cause.
//!
//! ## Trigger
//!
//!   1. A `Trade` event of qty ≥ `min_trade_qty` lands at price `p₀`
//!      and direction `dir` (deduced from tick rule against the mid
//!      *before* the trade).
//!   2. Within a dual rolling window
//!      (`reaction_seq` events OR `reaction_time_ns` ns, whichever
//!      fires first) we observe ≥ `min_burst` `OrderAdd`/`OrderCancel`
//!      events on the **opposite side** with `|p - p₀| ≤ band_ticks`.
//!   3. Optional reversion gate: if `require_reversion` is set, the
//!      mid must snap back by ≥ `reversion_bps` toward `p₀` within
//!      `reaction_seq` of the burst end, so we don't catch
//!      trend-following follow-through.
//!
//! ## Why "opposite side"
//!
//! A genuine reaction-to-print pattern moves the *quoted* opposite
//! side (someone repricing their resting orders to defend against the
//! tape). Bursts on the same side as the trade are normally "queue
//! recovery" and look qualitatively similar to phantom-liquidity, so
//! we keep them out of this detector to avoid double-flagging.
//!
//! ## Self-contained
//!
//! Like the other detectors in this crate, no dependency on a hot
//! orderbook. We track only the rolling mid (top-of-book) and a tiny
//! deque of pending trade-triggers waiting for their reaction window
//! to close.

use std::collections::{BTreeMap, VecDeque};

use flowlab_core::event::{EventType, SequencedEvent};

use crate::order_tracker::{AggressorSide, OrderTracker, ResolvedTrade};
use crate::{ChaosEvent, ChaosFeatures, ChaosKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TakerDir {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy)]
struct PendingTrigger {
    seq: u64,
    ts: u64,
    price: u64,
    qty: u64,
    dir: TakerDir,
    /// Mid at trigger time, used for the optional reversion gate.
    mid_at_trade: u64,
    /// Number of qualifying opposite-side events accumulated so far.
    burst_count: u32,
    /// Last opposite-side event in the burst — anchor for the
    /// reversion gate.
    last_burst_seq: u64,
    last_burst_ts: u64,
    /// Set true once `min_burst` has been reached. Then the trigger
    /// transitions into "waiting for reversion / window close".
    burst_qualified: bool,
}

pub struct LatencyArbProxyDetector {
    bids: BTreeMap<u64, u64>,
    asks: BTreeMap<u64, u64>,
    order_tracker: OrderTracker,
    max_book_levels: usize,

    pending: VecDeque<PendingTrigger>,

    /// Reaction window in seq numbers (`0` disables).
    reaction_seq: u64,
    /// Reaction window in nanoseconds (`0` disables).
    reaction_time_ns: u64,
    /// Half-band in price ticks for "same micro price-band as p₀".
    band_ticks: u64,
    /// Minimum trade qty to seed a trigger. Filters retail prints.
    min_trade_qty: u64,
    /// Minimum number of opposite-side ADD/CANCEL events in the
    /// reaction window to qualify the burst.
    min_burst: u32,
    /// If true, after the burst qualifies we require the mid to
    /// snap back by ≥ `reversion_bps` toward the trade price within
    /// `reaction_seq` of the burst end. Distinguishes reaction from
    /// trend-following follow-through.
    require_reversion: bool,
    reversion_bps: u32,
    /// Refractory in seq numbers after a flag.
    cooldown_seq: u64,

    refractory_until: u64,
}

impl LatencyArbProxyDetector {
    pub fn new(
        reaction_seq: u64,
        reaction_time_ns: u64,
        max_book_levels: usize,
        band_ticks: u64,
        min_trade_qty: u64,
        min_burst: u32,
        require_reversion: bool,
        reversion_bps: u32,
        cooldown_seq: u64,
    ) -> Self {
        debug_assert!(
            reaction_seq != 0 || reaction_time_ns != 0,
            "LatencyArbProxyDetector: at least one window must be enabled"
        );
        debug_assert!(min_burst >= 1, "min_burst must be >= 1");
        debug_assert!(max_book_levels >= 1);

        Self {
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            order_tracker: OrderTracker::new(),
            max_book_levels,
            pending: VecDeque::with_capacity(16),
            reaction_seq,
            reaction_time_ns,
            band_ticks,
            min_trade_qty,
            min_burst,
            require_reversion,
            reversion_bps,
            cooldown_seq,
            refractory_until: 0,
        }
    }

    pub fn best_bid(&self) -> Option<u64> {
        self.bids.iter().next_back().map(|(&p, _)| p)
    }
    pub fn best_ask(&self) -> Option<u64> {
        self.asks.iter().next().map(|(&p, _)| p)
    }
    pub fn midprice(&self) -> Option<u64> {
        match (self.best_bid(), self.best_ask()) {
            (Some(b), Some(a)) if a >= b => Some((a + b) / 2),
            _ => None,
        }
    }
    pub fn pending_triggers(&self) -> usize {
        self.pending.len()
    }

    pub fn process(&mut self, seq_event: &SequencedEvent) -> Option<ChaosEvent> {
        let event = &seq_event.event;
        let etype = EventType::from_u8(event.event_type)?;

        let mid_before = self.midprice();
        let mut resolved_trade: Option<ResolvedTrade> = None;

        match etype {
            EventType::OrderAdd => {
                if let Some(update) = self.order_tracker.on_add(event) {
                    self.add_qty(update.side, update.price, update.qty);
                }
            }
            EventType::OrderCancel => {
                if let Some(update) = self.order_tracker.on_cancel(event) {
                    self.sub_qty(update.side, update.price, update.qty);
                }
            }
            EventType::OrderModify => {}
            EventType::Trade => {
                if let Some(trade) = self.order_tracker.on_trade(event) {
                    self.sub_qty(trade.update.side, trade.update.price, trade.update.qty);
                    resolved_trade = Some(trade);
                }
            }
            EventType::BookSnapshot => return None,
        }

        let in_refractory = seq_event.seq < self.refractory_until;

        if !in_refractory {
            if let (Some(trade), Some(m)) = (resolved_trade, mid_before) {
                if trade.update.qty >= self.min_trade_qty {
                    let dir = match trade.aggressor {
                        AggressorSide::Buy => TakerDir::Buy,
                        AggressorSide::Sell => TakerDir::Sell,
                    };
                    self.pending.push_back(PendingTrigger {
                        seq: seq_event.seq,
                        ts: event.ts,
                        price: trade.update.price,
                        qty: trade.update.qty,
                        dir,
                        mid_at_trade: m,
                        burst_count: 0,
                        last_burst_seq: seq_event.seq,
                        last_burst_ts: event.ts,
                        burst_qualified: false,
                    });
                    if self.pending.len() > 64 {
                        self.pending.pop_front();
                    }
                }
            }
        }

        let mut flag: Option<ChaosEvent> = None;
        let mut keep: VecDeque<PendingTrigger> =
            VecDeque::with_capacity(self.pending.len());

        // We must drain to `keep` so we can mutate each trigger and
        // emit at most one flag per call. Take the deque by value.
        let pending = std::mem::take(&mut self.pending);

        for mut trig in pending {
            let inside_seq = self.reaction_seq > 0
                && seq_event.seq.saturating_sub(trig.seq) <= self.reaction_seq;
            let inside_time = self.reaction_time_ns > 0
                && event.ts.saturating_sub(trig.ts) <= self.reaction_time_ns;
            let inside_window = inside_seq || inside_time;

            // Contribute to the burst if this event is on the OPPOSITE
            // side and inside the price band.
            if inside_window && !trig.burst_qualified {
                // Side mapping: a buy-taker print hits the ASK resting
                // side. The "opposite" reaction we look for is
                // re-pricing on the BID side (sellers pulling). So:
                //   TakerDir::Buy  -> opposite resting side = BID = 0
                //   TakerDir::Sell -> opposite resting side = ASK = 1
                let want_side = match trig.dir {
                    TakerDir::Buy => 0u8,
                    TakerDir::Sell => 1u8,
                };
                let in_band = (event.price as i128 - trig.price as i128).unsigned_abs()
                    <= self.band_ticks as u128;
                let qualifies = matches!(
                    etype,
                    EventType::OrderAdd | EventType::OrderCancel
                ) && event.side == want_side
                    && in_band;
                if qualifies {
                    trig.burst_count = trig.burst_count.saturating_add(1);
                    trig.last_burst_seq = seq_event.seq;
                    trig.last_burst_ts = event.ts;
                    if trig.burst_count >= self.min_burst {
                        trig.burst_qualified = true;
                    }
                }
            }

            // Decide trigger fate.
            if !inside_window {
                // Window expired without flagging: drop.
                continue;
            }

            if trig.burst_qualified {
                // If reversion is required, wait for either reversion
                // success (flag) or burst-end window expiry (drop).
                if self.require_reversion {
                    let post_burst_seq = seq_event.seq.saturating_sub(trig.last_burst_seq);
                    if post_burst_seq <= self.reaction_seq {
                        if let Some(mid_now) = self.midprice() {
                            // Reversion: mid moves back TOWARD trig.price
                            // by at least `reversion_bps` from
                            // mid_at_trade.
                            let signed_back = match trig.dir {
                                // Buy print -> ask side typically lifts;
                                // reversion = mid coming back DOWN.
                                TakerDir::Buy => trig.mid_at_trade as i128 - mid_now as i128,
                                // Sell print -> bid side typically drops;
                                // reversion = mid coming back UP.
                                TakerDir::Sell => mid_now as i128 - trig.mid_at_trade as i128,
                            };
                            if signed_back > 0 {
                                let bps = ((signed_back as u128 * 10_000)
                                    / trig.mid_at_trade.max(1) as u128)
                                    as u32;
                                if bps >= self.reversion_bps && flag.is_none() {
                                    flag = Some(self.build_flag(&trig, seq_event.seq, event.ts));
                                    self.refractory_until =
                                        seq_event.seq.saturating_add(self.cooldown_seq);
                                    continue; // drop trigger after flag
                                }
                            }
                        }
                        keep.push_back(trig); // still waiting for reversion
                    }
                    // else: burst window closed -> drop trigger
                } else if flag.is_none() {
                    // Reversion not required: emit immediately on
                    // qualification.
                    flag = Some(self.build_flag(&trig, seq_event.seq, event.ts));
                    self.refractory_until =
                        seq_event.seq.saturating_add(self.cooldown_seq);
                    // drop trigger
                }
            } else {
                keep.push_back(trig);
            }
        }
        self.pending = keep;

        flag
    }

    fn build_flag(&self, trig: &PendingTrigger, end_seq: u64, end_ts: u64) -> ChaosEvent {
        let reaction_events = end_seq.saturating_sub(trig.seq);
        let reaction_ns = end_ts.saturating_sub(trig.ts);
        // Severity: tighter reaction + bigger burst -> higher.
        let speed_score = if self.reaction_seq > 0 {
            1.0 - (reaction_events as f64 / self.reaction_seq as f64).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let burst_score = (trig.burst_count as f64 / (self.min_burst as f64 * 4.0))
            .clamp(0.0, 1.0);
        let severity = (0.6 * speed_score + 0.4 * burst_score).clamp(0.0, 1.0);

        ChaosEvent {
            kind: ChaosKind::LatencyArbitrage,
            start_seq: trig.seq,
            end_seq,
            severity,
            initiator: None,
            features: ChaosFeatures {
                event_count: trig.burst_count as u64,
                duration_ns: reaction_ns,
                cancel_trade_ratio: 0.0,
                price_displacement: 0,
                depth_removed: trig.qty,
            },
        }
    }

    fn add_qty(&mut self, side: u8, price: u64, qty: u64) {
        let cap = self.max_book_levels;
        let book = self.book_mut(side);
        *book.entry(price).or_insert(0) += qty;
        Self::prune_levels(book, side, cap);
    }
    fn sub_qty(&mut self, side: u8, price: u64, qty: u64) {
        let book = self.book_mut(side);
        if let Some(entry) = book.get_mut(&price) {
            let take = if qty == 0 { *entry } else { qty.min(*entry) };
            *entry -= take;
            if *entry == 0 {
                book.remove(&price);
            }
        }
    }
    fn book_mut(&mut self, side: u8) -> &mut BTreeMap<u64, u64> {
        if side == 0 { &mut self.bids } else { &mut self.asks }
    }
    fn prune_levels(book: &mut BTreeMap<u64, u64>, side: u8, cap: usize) {
        while book.len() > cap {
            let evict = if side == 0 {
                book.iter().next().map(|(&p, _)| p)
            } else {
                book.iter().next_back().map(|(&p, _)| p)
            };
            if let Some(p) = evict {
                book.remove(&p);
            } else {
                break;
            }
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use flowlab_core::event::{Event, Side as ESide};
    use proptest::prelude::*;

    fn ev(
        seq: u64,
        ts: u64,
        etype: EventType,
        side: ESide,
        price: u64,
        qty: u64,
    ) -> SequencedEvent {
        SequencedEvent {
            seq,
            channel_id: 0,
            event: Event {
                ts,
                price,
                qty,
                order_id: seq,
                instrument_id: 1,
                event_type: etype as u8,
                side: side as u8,
                _pad: [0; 2],
            },
        }
    }

    fn ev_oid(
        seq: u64,
        ts: u64,
        etype: EventType,
        side: ESide,
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

    fn seed_book(det: &mut LatencyArbProxyDetector, mid: u64, levels: u64) -> u64 {
        let mut seq = 1u64;
        for i in 1..=levels {
            det.process(&ev(seq, seq * 1_000, EventType::OrderAdd, ESide::Bid, mid - i, 100));
            seq += 1;
            det.process(&ev(seq, seq * 1_000, EventType::OrderAdd, ESide::Ask, mid + i, 100));
            seq += 1;
        }
        seq
    }

    /// Buy-taker print at ask + burst of bid-side cancels in band ->
    /// flag (no-reversion mode).
    #[test]
    fn flags_buy_print_with_bid_burst() {
        let mut det = LatencyArbProxyDetector::new(
            /* reaction_seq */ 50,
            /* reaction_time_ns */ 0,
            /* max_book_levels */ 32,
            /* band_ticks */ 5,
            /* min_trade_qty */ 50,
            /* min_burst */ 3,
            /* require_reversion */ false,
            /* reversion_bps */ 0,
            /* cooldown_seq */ 200,
        );

        let mut seq = seed_book(&mut det, 10_000, 16);
        // Big buy print at the ask.
        let s = seq;
        det.process(&ev(s, s * 1_000, EventType::Trade, ESide::Ask, 10_001, 100));
        seq += 1;
        // Burst of bid-side cancels in band.
        let mut hits = 0;
        for k in 0..6u64 {
            let s = seq;
            if det.process(&ev(
                s, s * 1_000, EventType::OrderCancel, ESide::Bid,
                9_998 - (k % 3), 50,
            )).is_some() {
                hits += 1;
            }
            seq += 1;
        }
        assert!(hits >= 1, "buy-print + bid burst must flag");
    }

    /// Trade qty below `min_trade_qty` does NOT seed a trigger.
    #[test]
    fn no_flag_when_trade_below_min_qty() {
        let mut det = LatencyArbProxyDetector::new(
            50, 0, 32, 5,
            /* min_trade_qty */ 1_000,
            3, false, 0, 200,
        );
        let mut seq = seed_book(&mut det, 10_000, 16);
        let s = seq;
        det.process(&ev(s, s * 1_000, EventType::Trade, ESide::Ask, 10_001, 10));
        seq += 1;
        for _ in 0..6 {
            let s = seq;
            let f = det.process(&ev(
                s, s * 1_000, EventType::OrderCancel, ESide::Bid, 9_999, 50,
            ));
            assert!(f.is_none(), "no trigger -> no flag");
            seq += 1;
        }
        assert_eq!(det.pending_triggers(), 0);
    }

    /// Burst on the SAME side as the trade (queue recovery) must NOT
    /// flag — that pattern belongs to PhantomLiquidity, not arb proxy.
    #[test]
    fn no_flag_when_burst_on_same_side() {
        let mut det = LatencyArbProxyDetector::new(50, 0, 32, 5, 50, 3, false, 0, 200);
        let mut seq = seed_book(&mut det, 10_000, 16);
        // Buy print -> opposite side is BID. We send ASK-side burst -> ignore.
        let s = seq;
        det.process(&ev(s, s * 1_000, EventType::Trade, ESide::Ask, 10_001, 100));
        seq += 1;
        for _ in 0..6 {
            let s = seq;
            let f = det.process(&ev(
                s, s * 1_000, EventType::OrderCancel, ESide::Ask, 10_002, 50,
            ));
            assert!(f.is_none(), "same-side burst must not flag");
            seq += 1;
        }
    }

    /// Burst outside the price band must NOT contribute.
    #[test]
    fn no_flag_when_burst_out_of_band() {
        let mut det = LatencyArbProxyDetector::new(
            50, 0, 32, /* band_ticks */ 2, 50, 3, false, 0, 200,
        );
        let mut seq = seed_book(&mut det, 10_000, 32);
        let s = seq;
        det.process(&ev(s, s * 1_000, EventType::Trade, ESide::Ask, 10_001, 100));
        seq += 1;
        // Bid burst far away (>2 ticks from 10_001).
        for _ in 0..6 {
            let s = seq;
            let f = det.process(&ev(
                s, s * 1_000, EventType::OrderCancel, ESide::Bid, 9_990, 50,
            ));
            assert!(f.is_none(), "out-of-band burst must not flag");
            seq += 1;
        }
    }

    /// Burst arrives AFTER the reaction window closes -> no flag.
    #[test]
    fn no_flag_when_reaction_window_closed() {
        let mut det = LatencyArbProxyDetector::new(
            /* reaction_seq */ 5, 0, 32, 5, 50, 3, false, 0, 200,
        );
        let mut seq = seed_book(&mut det, 10_000, 16);
        let s = seq;
        det.process(&ev(s, s * 1_000, EventType::Trade, ESide::Ask, 10_001, 100));
        seq += 1;
        // Idle for 50 events on neutral side (no cancels).
        for _ in 0..50 {
            let s = seq;
            det.process(&ev(s, s * 1_000, EventType::OrderAdd, ESide::Ask, 10_020, 100));
            seq += 1;
        }
        // Late burst.
        for _ in 0..10 {
            let s = seq;
            let f = det.process(&ev(
                s, s * 1_000, EventType::OrderCancel, ESide::Bid, 9_999, 50,
            ));
            assert!(f.is_none(), "late burst must not flag");
            seq += 1;
        }
        assert_eq!(det.pending_triggers(), 0, "stale trigger must be evicted");
    }

    /// Cooldown: two qualifying patterns back-to-back -> only first.
    #[test]
    fn cooldown_suppresses_second_proxy() {
        let mut det = LatencyArbProxyDetector::new(
            50, 0, 32, 5, 50, 3, false, 0, /* cooldown_seq */ 1_000,
        );
        let mut seq = seed_book(&mut det, 10_000, 64);
        let mut hits = 0;
        for _ in 0..2 {
            let s = seq;
            det.process(&ev(s, s * 1_000, EventType::Trade, ESide::Ask, 10_001, 100));
            seq += 1;
            for k in 0..6u64 {
                let s = seq;
                if det.process(&ev(
                    s, s * 1_000, EventType::OrderCancel, ESide::Bid,
                    9_998 - (k % 3), 50,
                )).is_some() {
                    hits += 1;
                }
                seq += 1;
            }
        }
        assert_eq!(hits, 1, "cooldown must suppress second proxy (got {hits})");
    }

    #[test]
    fn raw_trade_seeds_trigger_from_order_id() {
        let mut det = LatencyArbProxyDetector::new(50, 0, 32, 5, 50, 3, false, 0, 200);
        det.process(&ev_oid(1, 1_000, EventType::OrderAdd, ESide::Bid, 9_999, 100, 11));
        det.process(&ev_oid(2, 2_000, EventType::OrderAdd, ESide::Bid, 9_998, 100, 12));
        det.process(&ev_oid(3, 3_000, EventType::OrderAdd, ESide::Bid, 9_997, 100, 13));
        det.process(&ev_oid(4, 4_000, EventType::OrderAdd, ESide::Ask, 10_001, 100, 21));

        let mut hits = 0;
        det.process(&ev_oid(5, 5_000, EventType::Trade, ESide::Bid, 0, 100, 21));
        for (seq, price) in [(6, 9_999), (7, 9_998), (8, 9_997)] {
            if det.process(&ev(seq, seq * 1_000, EventType::OrderCancel, ESide::Bid, price, 50)).is_some() {
                hits += 1;
            }
        }

        assert_eq!(hits, 1);
    }

    // ─── Property tests ──────────────────────────────────────────────

    proptest! {
        /// On a stream with NO trades, no proxy can ever flag — there
        /// are no triggers to seed.
        #[test]
        fn no_flags_without_trades(
            n in 50u64..400,
        ) {
            let mut det = LatencyArbProxyDetector::new(50, 0, 64, 5, 1, 1, false, 0, 200);
            let mut seq = seed_book(&mut det, 10_000, 32);
            for _ in 0..n {
                let s = seq;
                let side = if s % 2 == 0 { ESide::Bid } else { ESide::Ask };
                let px = if side == ESide::Bid { 9_999 } else { 10_001 };
                let f = det.process(&ev(s, s * 1_000, EventType::OrderCancel, side, px, 50));
                prop_assert!(f.is_none());
                seq += 1;
            }
        }

        /// Steady stream of small trades alternating sides on a deep
        /// book, with NO opposite-side bursts: never flag.
        #[test]
        fn no_flags_on_alternating_quiet_trades(
            n in 20u64..200,
        ) {
            let mut det = LatencyArbProxyDetector::new(20, 0, 64, 3, 1, 5, false, 0, 200);
            let mut seq = seed_book(&mut det, 10_000, 64);
            for k in 0..n {
                let (side, price) = if k % 2 == 0 {
                    (ESide::Ask, 10_001)
                } else {
                    (ESide::Bid, 9_999)
                };
                let s = seq;
                let f = det.process(&ev(s, s * 1_000, EventType::Trade, side, price, 1));
                prop_assert!(f.is_none(),
                    "alternating quiet trades must not flag (k={k})");
                seq += 1;
                // Replenish the consumed level.
                det.process(&ev(seq, seq * 1_000, EventType::OrderAdd, side, price, 1));
                seq += 1;
            }
        }
    }
}
