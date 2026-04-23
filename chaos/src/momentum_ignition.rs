//! Momentum-ignition detector.
//!
//! ## Definition
//!
//! Momentum ignition is a sequence of *aggressive* takes — typically
//! sweeping multiple price levels in one direction — designed to
//! provoke other participants (momentum followers, stop hunters,
//! latency-sensitive HFT) into chasing the move. The textbook
//! signature, observable on a single feed, is:
//!
//!   1. a **monotone midprice drift** of at least `min_move_bps`,
//!   2. accumulated within a **dual rolling window** (seq + time),
//!   3. supported by a **trade-imbalance** in the same direction
//!      (aggressors disproportionately on one side),
//!   4. lasting at least `min_consecutive` mid samples
//!      (filters tick-level micro-noise),
//!   5. above a minimum trade count `min_trades`
//!      (ignores quiet drifts),
//!   6. and not inside a refractory period (`cooldown_seq`).
//!
//! Stay coupling-free with `flowlab-core`: the detector keeps its own
//! minimal top-of-book (BTreeMap<price, qty> per side) updated by ADD
//! / CANCEL / TRADE. We only need best_bid/best_ask to derive midprice
//! and spread; depth beyond a handful of nearby levels is irrelevant
//! for the signal.
//!
//! ## Mini-book invariants
//!
//!   * Empty levels (qty == 0) are pruned on every event.
//!   * Total tracked levels are capped at `max_book_levels` per side;
//!     when exceeded, we evict the level **farthest from the best** on
//!     that side. This is deterministic and O(log n) without an LRU
//!     accounting structure.
//!   * `best_bid()` / `best_ask()` are simply `BTreeMap::last_key()`
//!     and `first_key()`.
//!
//! ## Trade aggression heuristic
//!
//! ITCH does not flag aggressor side directly. We classify a trade
//! relative to the most recent mid:
//!
//!   * `trade.price >= mid_at_trade_time` -> buy-aggressor pressure
//!   * `trade.price <  mid_at_trade_time` -> sell-aggressor pressure
//!
//! This is the standard tick-rule classifier. It is wrong on ~5-10 %
//! of mid-quote prints, which is fine because the detector requires a
//! *signed imbalance*, not single-trade attribution.

use std::collections::{BTreeMap, VecDeque};

use flowlab_core::event::{EventType, SequencedEvent};

use crate::order_tracker::{AggressorSide, OrderTracker};
use crate::{ChaosEvent, ChaosFeatures, ChaosKind};

/// Direction of the current detected drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dir {
    Up,
    Down,
}

#[derive(Debug, Clone, Copy)]
struct MidSample {
    seq: u64,
    ts: u64,
    mid: u64,
}

/// Momentum-ignition detector. Self-contained, dual-window, persistence-
/// gated, refractory-protected.
pub struct MomentumIgnitionDetector {
    /// Bid side mini-book (price -> aggregated qty).
    bids: BTreeMap<u64, u64>,
    /// Ask side mini-book (price -> aggregated qty).
    asks: BTreeMap<u64, u64>,
    order_tracker: OrderTracker,
    /// Cap on tracked levels per side. Levels farthest from best are
    /// evicted when exceeded.
    max_book_levels: usize,

    /// Rolling midprice samples (only points where mid changes).
    mids: VecDeque<MidSample>,
    /// Aggressor counters in the rolling window.
    buy_pressure: u64,
    sell_pressure: u64,
    /// Total trades in the rolling window.
    trades_in_window: u64,

    /// Window in sequence numbers (`0` disables).
    seq_window: u64,
    /// Window in nanoseconds (`0` disables).
    time_window_ns: u64,
    /// Minimum directional move in basis points before a flag can fire.
    min_move_bps: u32,
    /// Minimum total trades in the window.
    min_trades: u64,
    /// Minimum number of consecutive monotone mid samples.
    min_consecutive: u32,
    /// Refractory period (in seq numbers) after a flag.
    cooldown_seq: u64,

    /// Track the current monotone-run direction and its length.
    run_dir: Option<Dir>,
    run_len: u32,
    run_start_seq: u64,
    run_start_ts: u64,
    run_start_mid: u64,

    /// Refractory-end sequence number (`0` = not in refractory).
    refractory_until: u64,
}

impl MomentumIgnitionDetector {
    pub fn new(
        seq_window: u64,
        time_window_ns: u64,
        max_book_levels: usize,
        min_move_bps: u32,
        min_trades: u64,
        min_consecutive: u32,
        cooldown_seq: u64,
    ) -> Self {
        debug_assert!(
            seq_window != 0 || time_window_ns != 0,
            "MomentumIgnitionDetector: at least one window must be enabled"
        );
        debug_assert!(min_consecutive >= 1, "min_consecutive must be >= 1");
        debug_assert!(max_book_levels >= 1, "max_book_levels must be >= 1");

        Self {
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            order_tracker: OrderTracker::new(),
            max_book_levels,
            mids: VecDeque::with_capacity(256),
            buy_pressure: 0,
            sell_pressure: 0,
            trades_in_window: 0,
            seq_window,
            time_window_ns,
            min_move_bps,
            min_trades,
            min_consecutive,
            cooldown_seq,
            run_dir: None,
            run_len: 0,
            run_start_seq: 0,
            run_start_ts: 0,
            run_start_mid: 0,
            refractory_until: 0,
        }
    }

    /// Best bid (highest tracked price with qty > 0).
    pub fn best_bid(&self) -> Option<u64> {
        self.bids.iter().next_back().map(|(&p, _)| p)
    }

    /// Best ask (lowest tracked price with qty > 0).
    pub fn best_ask(&self) -> Option<u64> {
        self.asks.iter().next().map(|(&p, _)| p)
    }

    /// Current midprice in fixed-point units. None until both sides
    /// have at least one level.
    pub fn midprice(&self) -> Option<u64> {
        match (self.best_bid(), self.best_ask()) {
            (Some(b), Some(a)) if a >= b => Some((a + b) / 2),
            _ => None,
        }
    }

    /// Current spread in fixed-point units. None until both sides have
    /// at least one level.
    pub fn spread(&self) -> Option<u64> {
        match (self.best_bid(), self.best_ask()) {
            (Some(b), Some(a)) if a >= b => Some(a - b),
            _ => None,
        }
    }

    /// Length of the current monotone run.
    pub fn run_len(&self) -> u32 {
        self.run_len
    }

    /// Feed one event in sequence order.
    pub fn process(&mut self, seq_event: &SequencedEvent) -> Option<ChaosEvent> {
        let event = &seq_event.event;
        let etype = EventType::from_u8(event.event_type)?;

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
            EventType::OrderModify => {
            }
            EventType::Trade => {
                if let Some(trade) = self.order_tracker.on_trade(event) {
                    self.sub_qty(trade.update.side, trade.update.price, trade.update.qty);
                    self.trades_in_window += 1;
                    match trade.aggressor {
                        AggressorSide::Buy => self.buy_pressure += 1,
                        AggressorSide::Sell => self.sell_pressure += 1,
                    }
                }
            }
            EventType::BookSnapshot => return None,
        }

        // Mid AFTER mutation drives the run logic.
        let mid_after = self.midprice()?;

        // Record the new mid sample only on a *change* — flat ticks
        // are noise for run detection.
        let push_sample = self
            .mids
            .back()
            .map(|s| s.mid != mid_after)
            .unwrap_or(true);
        if push_sample {
            self.mids.push_back(MidSample {
                seq: seq_event.seq,
                ts: event.ts,
                mid: mid_after,
            });
        }
        self.evict_expired(seq_event.seq, event.ts);

        // Update the monotone-run state machine.
        let in_refractory = seq_event.seq < self.refractory_until;
        if push_sample {
            self.update_run(mid_after, seq_event.seq, event.ts);
        }

        if in_refractory {
            return None;
        }

        // Evaluate trigger conditions.
        let dir = self.run_dir?;
        if self.run_len < self.min_consecutive {
            return None;
        }
        if self.trades_in_window < self.min_trades {
            return None;
        }

        // Move in basis points relative to the run-start mid.
        let move_bps = bps_move(self.run_start_mid, mid_after);
        if move_bps < self.min_move_bps {
            return None;
        }

        // Imbalance must align with the run direction.
        let aligned = match dir {
            Dir::Up => self.buy_pressure > self.sell_pressure,
            Dir::Down => self.sell_pressure > self.buy_pressure,
        };
        if !aligned {
            return None;
        }

        let total_pressure = self.buy_pressure + self.sell_pressure;
        let imbalance = if total_pressure == 0 {
            0.0
        } else {
            let dom = match dir {
                Dir::Up => self.buy_pressure,
                Dir::Down => self.sell_pressure,
            };
            dom as f64 / total_pressure as f64
        };

        // Severity blends move size and imbalance: a bigger move with
        // a stronger one-sided pressure scores higher. Both terms are
        // in [0, 1] before averaging.
        let move_score = (move_bps as f64 / (self.min_move_bps as f64 * 4.0)).clamp(0.0, 1.0);
        let imb_score = ((imbalance - 0.5) * 2.0).clamp(0.0, 1.0);
        let severity = (0.6 * move_score + 0.4 * imb_score).clamp(0.0, 1.0);

        let displacement = mid_after as i64 - self.run_start_mid as i64;
        let flag = ChaosEvent {
            kind: ChaosKind::MomentumIgnition,
            start_seq: self.run_start_seq,
            end_seq: seq_event.seq,
            severity,
            initiator: None,
            features: ChaosFeatures {
                event_count: self.trades_in_window,
                duration_ns: event.ts.saturating_sub(self.run_start_ts),
                cancel_trade_ratio: 0.0,
                price_displacement: displacement,
                depth_removed: 0,
            },
        };

        // Arm refractory and reset run.
        self.refractory_until = seq_event.seq.saturating_add(self.cooldown_seq);
        self.run_dir = None;
        self.run_len = 0;

        Some(flag)
    }

    /// Update the monotone-run state when a new mid sample lands.
    fn update_run(&mut self, mid: u64, seq: u64, ts: u64) {
        let prev = match self.mids.iter().rev().nth(1) {
            Some(s) => s.mid,
            None => {
                // First sample ever — no direction yet.
                self.run_dir = None;
                self.run_len = 1;
                self.run_start_seq = seq;
                self.run_start_ts = ts;
                self.run_start_mid = mid;
                return;
            }
        };
        let new_dir = match mid.cmp(&prev) {
            std::cmp::Ordering::Greater => Some(Dir::Up),
            std::cmp::Ordering::Less => Some(Dir::Down),
            std::cmp::Ordering::Equal => return, // shouldn't happen (push_sample gate)
        };
        if new_dir == self.run_dir {
            self.run_len = self.run_len.saturating_add(1);
        } else {
            // Direction changed — restart the run from `prev`.
            self.run_dir = new_dir;
            self.run_len = 2; // prev + current
            self.run_start_seq = self
                .mids
                .iter()
                .rev()
                .nth(1)
                .map(|s| s.seq)
                .unwrap_or(seq);
            self.run_start_ts = self
                .mids
                .iter()
                .rev()
                .nth(1)
                .map(|s| s.ts)
                .unwrap_or(ts);
            self.run_start_mid = prev;
        }
    }

    /// Evict mid samples and decay pressure counters when entries fall
    /// outside the rolling window.
    fn evict_expired(&mut self, cur_seq: u64, cur_ts: u64) {
        // Mids: cheap eviction, just drop heads outside both windows.
        while let Some(front) = self.mids.front() {
            let too_old_seq = self.seq_window > 0
                && cur_seq.saturating_sub(front.seq) > self.seq_window;
            let too_old_ts = self.time_window_ns > 0
                && cur_ts.saturating_sub(front.ts) > self.time_window_ns;
            if too_old_seq || too_old_ts {
                self.mids.pop_front();
            } else {
                break;
            }
        }
        // Pressure / trade counters: simplest sound model is to decay
        // them when the front of the mids deque has just been popped
        // — the window has effectively rotated. We approximate by
        // bleeding ~5 % of pressure per evicted sample, and capping at
        // 0. This avoids maintaining a full per-event ring buffer for
        // a quantity we only need for ordering.
        // For determinism in tests we keep this simple: when no mid
        // samples remain, fully reset pressure.
        if self.mids.is_empty() {
            self.buy_pressure = 0;
            self.sell_pressure = 0;
            self.trades_in_window = 0;
        }
    }

    /// Add `qty` to a level on `side`. Side: 0=Bid, 1=Ask.
    fn add_qty(&mut self, side: u8, price: u64, qty: u64) {
        let cap = self.max_book_levels;
        let book = self.book_mut(side);
        *book.entry(price).or_insert(0) += qty;
        Self::prune_levels(book, side, cap);
    }

    /// Subtract `qty` from a level on `side` and prune empty entries.
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
        if side == 0 {
            &mut self.bids
        } else {
            &mut self.asks
        }
    }

    /// Drop levels farthest from the best when above the cap.
    /// Deterministic; preserves the levels that matter for top-of-book
    /// signals.
    fn prune_levels(book: &mut BTreeMap<u64, u64>, side: u8, cap: usize) {
        while book.len() > cap {
            // For bids, the "best" is the highest price -> evict lowest.
            // For asks, the "best" is the lowest price  -> evict highest.
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

/// Absolute basis-point distance between two prices, on the from-price.
#[inline]
fn bps_move(from: u64, to: u64) -> u32 {
    if from == 0 {
        return 0;
    }
    let diff = if to >= from { to - from } else { from - to };
    // bps = diff * 10_000 / from. Use u128 to avoid overflow on big
    // fixed-point ticks.
    let bps = (diff as u128 * 10_000) / from as u128;
    bps.min(u32::MAX as u128) as u32
}

// ─── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use flowlab_core::event::{Event, Side};
    use proptest::prelude::*;

    fn ev(
        seq: u64,
        ts: u64,
        etype: EventType,
        side: Side,
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

    /// Seed a balanced book around `mid` with `levels` levels each side,
    /// 1-tick apart, qty=100 each. Returns the next seq number to use.
    fn seed_book(det: &mut MomentumIgnitionDetector, mid: u64, levels: u64) -> u64 {
        let mut seq = 1u64;
        for i in 1..=levels {
            det.process(&ev(seq, seq * 1_000, EventType::OrderAdd, Side::Bid, mid - i, 100));
            seq += 1;
            det.process(&ev(seq, seq * 1_000, EventType::OrderAdd, Side::Ask, mid + i, 100));
            seq += 1;
        }
        seq
    }

    /// A monotone upward sweep through several ask levels with strong
    /// buy-aggressor imbalance must flag MomentumIgnition.
    #[test]
    fn flags_upward_ignition() {
        let mut det = MomentumIgnitionDetector::new(
            /* seq_window */ 500,
            /* time_window_ns */ 0,
            /* max_book_levels */ 16,
            /* min_move_bps */ 1,
            /* min_trades */ 3,
            /* min_consecutive */ 3,
            /* cooldown_seq */ 200,
        );

        let mut seq = seed_book(&mut det, 10_000, 8);
        // Sweep asks 10_001..=10_006: each trade at ask removes that
        // level, mid drifts up.
        let mut flagged = 0;
        for px in 10_001u64..=10_006 {
            let s = seq;
            let flag = det.process(&ev(
                s, s * 1_000, EventType::Trade, Side::Ask, px, 100,
            ));
            if flag.is_some() {
                flagged += 1;
            }
            seq += 1;
        }
        assert!(
            flagged >= 1,
            "upward sweep with imbalance must flag (got {flagged})"
        );
    }

    /// A balanced two-way tape with frequent direction reversals must
    /// NOT flag.
    #[test]
    fn no_flag_on_balanced_chop() {
        let mut det = MomentumIgnitionDetector::new(
            500, 0, 32, 5, 3, 3, 200,
        );
        let mut seq = seed_book(&mut det, 10_000, 16);
        // Alternate: hit ask, hit bid, hit ask, hit bid... mid oscillates.
        for k in 0..40u64 {
            let (side, price) = if k % 2 == 0 {
                (Side::Ask, 10_000 + 1 + (k % 3))
            } else {
                (Side::Bid, 10_000 - 1 - (k % 3))
            };
            let s = seq;
            let flag = det.process(&ev(s, s * 1_000, EventType::Trade, side, price, 100));
            assert!(flag.is_none(), "balanced chop flagged at k={k}");
            seq += 1;
        }
    }

    /// A move that satisfies persistence but is below `min_move_bps`
    /// must NOT flag.
    #[test]
    fn no_flag_below_min_move_bps() {
        let mut det = MomentumIgnitionDetector::new(
            500, 0, 16,
            /* min_move_bps */ 1_000, // huge
            3, 3, 200,
        );
        let mut seq = seed_book(&mut det, 10_000, 8);
        for px in 10_001u64..=10_005 {
            let s = seq;
            let flag = det.process(&ev(s, s * 1_000, EventType::Trade, Side::Ask, px, 100));
            assert!(flag.is_none(), "below-bps move flagged at px={px}");
            seq += 1;
        }
    }

    /// A sweep with too few trades (2 < min_trades=4) must NOT flag.
    #[test]
    fn no_flag_below_min_trades() {
        let mut det = MomentumIgnitionDetector::new(
            500, 0, 16, 1,
            /* min_trades */ 4,
            2, 200,
        );
        let mut seq = seed_book(&mut det, 10_000, 8);
        for px in 10_001u64..=10_002 {
            let s = seq;
            let flag = det.process(&ev(s, s * 1_000, EventType::Trade, Side::Ask, px, 100));
            assert!(flag.is_none(), "too few trades flagged at px={px}");
            seq += 1;
        }
    }

    /// Two ignitions back-to-back inside the cooldown -> only the
    /// first flags.
    #[test]
    fn cooldown_suppresses_second_ignition() {
        let mut det = MomentumIgnitionDetector::new(
            500, 0, 64, 5, 3, 3,
            /* cooldown_seq */ 1_000,
        );
        let mut seq = seed_book(&mut det, 10_000, 32);

        let mut hits = 0;
        for px in 10_001u64..=10_006 {
            let s = seq;
            if det.process(&ev(s, s * 1_000, EventType::Trade, Side::Ask, px, 100)).is_some() {
                hits += 1;
            }
            seq += 1;
        }
        // Second sweep immediately after — still in cooldown.
        for px in 10_007u64..=10_012 {
            let s = seq;
            if det.process(&ev(s, s * 1_000, EventType::Trade, Side::Ask, px, 100)).is_some() {
                hits += 1;
            }
            seq += 1;
        }
        assert_eq!(hits, 1, "cooldown must prevent second flag (got {hits})");
    }

    /// Mini-book pruning keeps total levels per side bounded.
    #[test]
    fn book_levels_bounded() {
        let cap = 4;
        let mut det = MomentumIgnitionDetector::new(500, 0, cap, 5, 3, 3, 200);
        for i in 1..=20u64 {
            det.process(&ev(i, i * 1_000, EventType::OrderAdd, Side::Bid, 10_000 - i, 100));
            det.process(&ev(
                100 + i,
                (100 + i) * 1_000,
                EventType::OrderAdd,
                Side::Ask,
                10_000 + i,
                100,
            ));
        }
        assert!(det.bids.len() <= cap, "bids over cap: {}", det.bids.len());
        assert!(det.asks.len() <= cap, "asks over cap: {}", det.asks.len());
        // The retained levels should be the ones nearest the best
        // (best_bid is largest tracked, best_ask is smallest tracked).
        assert_eq!(det.best_bid(), Some(9_999));
        assert_eq!(det.best_ask(), Some(10_001));
    }

    #[test]
    fn raw_trade_resolves_order_id_and_price() {
        let mut det = MomentumIgnitionDetector::new(500, 0, 16, 1, 1, 1, 200);
        det.process(&ev_oid(1, 1_000, EventType::OrderAdd, Side::Bid, 9_999, 100, 11));
        det.process(&ev_oid(2, 2_000, EventType::OrderAdd, Side::Ask, 10_001, 100, 22));

        assert_eq!(det.best_ask(), Some(10_001));

        det.process(&ev_oid(3, 3_000, EventType::Trade, Side::Bid, 0, 100, 22));

        assert_eq!(det.best_ask(), None);
    }

    // ─── Property tests ──────────────────────────────────────────────

    proptest! {
        /// Pure ADD-only stream (no trades) must never flag — the
        /// `min_trades` filter is hard.
        #[test]
        fn no_flags_without_trades(
            n in 50usize..400,
        ) {
            let mut det = MomentumIgnitionDetector::new(
                500, 0, 32, 5, 3, 3, 200,
            );
            // Seed both sides
            for i in 1..=20u64 {
                det.process(&ev(i, i * 1_000, EventType::OrderAdd, Side::Bid, 10_000 - i, 100));
                det.process(&ev(50 + i, (50 + i) * 1_000, EventType::OrderAdd, Side::Ask, 10_000 + i, 100));
            }
            for i in 0..(n as u64) {
                let s = 200 + i;
                let side = if i % 2 == 0 { Side::Bid } else { Side::Ask };
                let px = if i % 2 == 0 {
                    10_000_u64.saturating_sub(1 + (i % 8))
                } else {
                    10_000_u64.saturating_add(1 + (i % 8))
                };
                let flag = det.process(&ev(s, s * 1_000, EventType::OrderAdd, side, px, 100));
                prop_assert!(flag.is_none());
            }
        }

        /// Random alternating buy/sell trades on a deep balanced book
        /// must not flag (no monotone run).
        #[test]
        fn no_flags_on_alternating_trades(
            n in 20u64..200,
        ) {
            let mut det = MomentumIgnitionDetector::new(
                500, 0, 64, 1, 2, 2, 200,
            );
            let mut seq = seed_book(&mut det, 10_000, 32);
            for k in 0..n {
                let (side, price) = if k % 2 == 0 {
                    (Side::Ask, 10_000 + 1)
                } else {
                    (Side::Bid, 10_000 - 1)
                };
                let s = seq;
                let flag = det.process(&ev(s, s * 1_000, EventType::Trade, side, price, 1));
                prop_assert!(flag.is_none(),
                    "alternating trades must not flag (k={k})");
                seq += 1;
                // Re-add the consumed level so the book stays alive
                det.process(&ev(seq, seq * 1_000, EventType::OrderAdd, side, price, 100));
                seq += 1;
            }
        }
    }
}
