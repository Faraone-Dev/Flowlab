//! Flash-crash detector.
//!
//! ## Definition
//!
//! A *flash crash* (or flash spike) is a **discontinuous** dislocation
//! of the top-of-book: midprice jumps by more than `min_gap_bps` over
//! at most `gap_window_events` consecutive events, accompanied by a
//! **liquidity vacuum** at the top (aggregate depth within
//! `depth_band_ticks` of the new best collapsed by at least
//! `depth_drop_pct` percent vs the pre-event snapshot), and confirmed
//! by **aggressive trades** in the same direction (>= `min_aggr_trades`
//! within the same micro-window).
//!
//! ## Why this is *not* MomentumIgnition
//!
//! MomentumIgnition fires on a *monotone drift* sustained for several
//! mid samples. FlashCrash fires on a *single sharp jump* (1-2 events)
//! that simultaneously evaporates the visible book around the new
//! best. The two detectors are designed to be mutually exclusive by
//! construction:
//!
//!   * Ignition needs `min_consecutive >= 3` mid samples.
//!   * FlashCrash needs the move to land in `<= gap_window_events`
//!     events (default 2).
//!
//! On the same recording, an event that triggers FlashCrash is too
//! short to satisfy Ignition's persistence; an event that triggers
//! Ignition is too gradual to satisfy FlashCrash's gap-window.
//!
//! ## Self-contained mini-book
//!
//! Same shape as `momentum_ignition::MomentumIgnitionDetector`: a
//! `BTreeMap<price, qty>` per side, deterministic eviction of the
//! levels farthest from the best when above `max_book_levels`.
//!
//! ## Hot-path optimisation: incremental band depth
//!
//! `depth_in_band()` is called on every event (snapshot creation).
//! A naive implementation (BTreeMap range scan, O(K + log n)) makes
//! the snapshot stage the dominant cost of the whole chain. Instead
//! we maintain `band_depth` as a single `u64` updated **incrementally**
//! on every mutation:
//!
//!   * If the mutated price falls inside the current band and best
//!     did not move -> `band_depth ±= delta_qty` in O(1).
//!   * If best moved -> we **lazily** invalidate (`band_depth_valid`
//!     becomes `false`) and the next `depth_in_band()` triggers a
//!     single O(K) recompute via the BTreeMap range.
//!
//! The `depth_in_band_around(bb, ba)` path used by the gap detector
//! to measure the *original* band's collapse is NOT cached because
//! it is only called once per fired flag (rare).
//!
//! ## Pre-event depth snapshot
//!
//! On every successful event we cache the **prior** depth-in-band so
//! the next event can compute `depth_drop_pct` against the snapshot
//! that existed *before* the gap. The ring is a fixed-size array of
//! `SNAP_CAP` slots with a circular head index — no `VecDeque`, no
//! per-event allocation, no front-pop branch.

use std::collections::BTreeMap;

use flowlab_core::event::{EventType, SequencedEvent};

use crate::order_tracker::{AggressorSide, OrderTracker};
use crate::{ChaosEvent, ChaosFeatures, ChaosKind};

/// Maximum supported `gap_window_events`. Detectors built with a
/// larger window will hit a `debug_assert!` in `new()`.
const SNAP_CAP: usize = 16;

/// Snapshot of the top-of-book just *before* an event was applied.
#[derive(Debug, Clone, Copy, Default)]
struct TopSnapshot {
    seq: u64,
    ts: u64,
    mid: u64,
    /// Best bid at snap time. Used to anchor the depth band when we
    /// later compute the "vacuum left behind" by a crash.
    best_bid: u64,
    /// Best ask at snap time, anchor for the upper depth band.
    best_ask: u64,
    /// Aggregate qty in [best_bid - band, best_bid] +
    ///                  [best_ask, best_ask + band] (in tick units),
    /// computed at snap time around the snap's best.
    depth_in_band: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CrashDir {
    Down, // mid jumped down
    Up,   // mid jumped up
}

pub struct FlashCrashDetector {
    bids: BTreeMap<u64, u64>,
    asks: BTreeMap<u64, u64>,
    order_tracker: OrderTracker,
    max_book_levels: usize,

    /// Cached best bid / best ask. Updated *incrementally* on every
    /// mutation so the hot path never walks the BTreeMap. Kept in
    /// sync via `add_qty` / `sub_qty`; recomputed from scratch only
    /// when the current best is fully removed.
    cached_bb: Option<u64>,
    cached_ba: Option<u64>,

    /// Fixed-capacity circular buffer of pre-event snapshots. We keep
    /// at most `gap_window_events + 1` valid entries so we can also
    /// evaluate gaps that span the full window.
    snaps: [TopSnapshot; SNAP_CAP],
    /// Index of the next write slot.
    snap_head: u8,
    /// Number of valid entries (<= `gap_window_events + 1`).
    snap_len: u8,

    /// Cached current `depth_in_band()` value. Valid only when
    /// `band_depth_valid` is true; otherwise the next read recomputes.
    band_depth: u64,
    band_depth_valid: bool,

    /// Minimum gap on midprice in basis points to qualify.
    min_gap_bps: u32,
    /// Maximum number of events the gap is allowed to span. A "true"
    /// flash event lands in 1-2 events; defaults to 2.
    gap_window_events: u32,
    /// Half-band in price ticks around best for depth aggregation.
    depth_band_ticks: u64,
    /// Minimum % collapse of depth-in-band vs the snapshot at the
    /// start of the gap window. `0.0` disables the depth check (which
    /// would make FlashCrash overlap with Ignition - not recommended).
    depth_drop_pct: f64,
    /// Minimum number of trades in the same direction inside the gap
    /// window. The aggressor side must align with the crash direction.
    min_aggr_trades: u32,
    /// Refractory period (in seq numbers) after a flag.
    cooldown_seq: u64,

    /// Trade-aggression accumulators inside the current gap window.
    buy_aggr: u32,
    sell_aggr: u32,

    refractory_until: u64,
}

impl FlashCrashDetector {
    pub fn new(
        min_gap_bps: u32,
        gap_window_events: u32,
        max_book_levels: usize,
        depth_band_ticks: u64,
        depth_drop_pct: f64,
        min_aggr_trades: u32,
        cooldown_seq: u64,
    ) -> Self {
        debug_assert!(min_gap_bps >= 1, "min_gap_bps must be >= 1");
        debug_assert!(
            gap_window_events >= 1,
            "gap_window_events must be >= 1"
        );
        debug_assert!(max_book_levels >= 1, "max_book_levels must be >= 1");
        debug_assert!(
            (0.0..=1.0).contains(&depth_drop_pct),
            "depth_drop_pct must be in [0, 1]"
        );
        debug_assert!(
            (gap_window_events as usize) < SNAP_CAP,
            "gap_window_events must be < SNAP_CAP ({SNAP_CAP})"
        );

        Self {
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            order_tracker: OrderTracker::new(),
            max_book_levels,
            cached_bb: None,
            cached_ba: None,
            snaps: [TopSnapshot::default(); SNAP_CAP],
            snap_head: 0,
            snap_len: 0,
            band_depth: 0,
            band_depth_valid: true,
            min_gap_bps,
            gap_window_events,
            depth_band_ticks,
            depth_drop_pct,
            min_aggr_trades,
            cooldown_seq,
            buy_aggr: 0,
            sell_aggr: 0,
            refractory_until: 0,
        }
    }

    pub fn best_bid(&self) -> Option<u64> {
        self.cached_bb
    }
    pub fn best_ask(&self) -> Option<u64> {
        self.cached_ba
    }
    pub fn midprice(&self) -> Option<u64> {
        match (self.cached_bb, self.cached_ba) {
            (Some(b), Some(a)) if a >= b => Some((a + b) / 2),
            _ => None,
        }
    }

    /// Aggregate qty in the depth-band around current best on both
    /// sides. Returns 0 when either side is empty.
    ///
    /// Hot path: serves the per-event snapshot. Returns the cached
    /// `band_depth` in O(1) when valid, otherwise triggers a single
    /// O(K) recompute against the BTreeMap and caches the result.
    pub fn depth_in_band(&mut self) -> u64 {
        if self.band_depth_valid {
            return self.band_depth;
        }
        let d = match (self.cached_bb, self.cached_ba) {
            (Some(bb), Some(ba)) => self.depth_in_band_around(bb, ba),
            _ => 0,
        };
        self.band_depth = d;
        self.band_depth_valid = true;
        d
    }

    /// Aggregate qty in the depth-band around a *specific* anchor pair
    /// (`bb`, `ba`). Used by the gap detector to measure depth-collapse
    /// in the *original* band, not in a band that drifts with the new
    /// best. Without anchoring, a crash that wipes the old top would
    /// be masked because the band slides down with the new best.
    fn depth_in_band_around(&self, bb: u64, ba: u64) -> u64 {
        let bid_lo = bb.saturating_sub(self.depth_band_ticks);
        let ask_hi = ba.saturating_add(self.depth_band_ticks);
        let bid_sum: u64 = self.bids.range(bid_lo..=bb).map(|(_, &q)| q).sum();
        let ask_sum: u64 = self.asks.range(ba..=ask_hi).map(|(_, &q)| q).sum();
        bid_sum.saturating_add(ask_sum)
    }

    pub fn process(&mut self, seq_event: &SequencedEvent) -> Option<ChaosEvent> {
        let event = &seq_event.event;
        let etype = EventType::from_u8(event.event_type)?;

        // 1. Snapshot the current top BEFORE mutating. Skip on the very
        //    first event when mid is undefined.
        let mid_before = self.midprice();
        if let Some(m) = mid_before {
            let bb = self.best_bid().unwrap_or(0);
            let ba = self.best_ask().unwrap_or(0);
            let depth_now = self.depth_in_band();
            let snap = TopSnapshot {
                seq: seq_event.seq,
                ts: event.ts,
                mid: m,
                best_bid: bb,
                best_ask: ba,
                depth_in_band: depth_now,
            };
            self.push_snap(snap);
        }

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
                    match trade.aggressor {
                        AggressorSide::Buy => self.buy_aggr = self.buy_aggr.saturating_add(1),
                        AggressorSide::Sell => {
                            self.sell_aggr = self.sell_aggr.saturating_add(1)
                        }
                    }
                    self.sub_qty(trade.update.side, trade.update.price, trade.update.qty);
                }
            }
            EventType::BookSnapshot => return None,
        }

        // 4. Try to detect a gap.
        let mid_after = self.midprice()?;
        let in_refractory = seq_event.seq < self.refractory_until;
        if in_refractory {
            // Bleed aggression while suppressed so a subsequent valid
            // window doesn't carry stale pressure.
            self.buy_aggr = 0;
            self.sell_aggr = 0;
            return None;
        }

        // Iterate snapshots from oldest to newest, looking for the
        // first one within `gap_window_events` events that satisfies
        // the gap. The earliest qualifying snapshot inside the window
        // gives the largest measured gap.
        let mut hit: Option<(TopSnapshot, CrashDir, u32)> = None;
        for i in 0..self.snap_len as usize {
            // snap_head points to the next *write* slot, so the oldest
            // valid entry is at `snap_head - snap_len` modulo CAP.
            let idx = (self.snap_head as usize + SNAP_CAP - self.snap_len as usize + i) % SNAP_CAP;
            let snap = self.snaps[idx];
            let span = seq_event.seq.saturating_sub(snap.seq);
            if span == 0 || span > self.gap_window_events as u64 {
                continue;
            }
            let bps = bps_move(snap.mid, mid_after);
            if bps < self.min_gap_bps {
                continue;
            }
            let dir = if mid_after < snap.mid {
                CrashDir::Down
            } else {
                CrashDir::Up
            };
            hit = Some((snap, dir, bps));
            break;
        }
        let (snap, dir, gap_bps) = hit?;

        // 5. Depth-collapse check. Compare depth in the SAME price
        //    band that existed at snap time (anchored to snap's best).
        //    Without anchoring, the band slides with the moving best
        //    and the collapse becomes invisible.
        let depth_now = self.depth_in_band_around(snap.best_bid, snap.best_ask);
        let depth_then = snap.depth_in_band;
        let drop_ok = if self.depth_drop_pct == 0.0 {
            true
        } else if depth_then == 0 {
            // No prior depth -> cannot prove collapse; skip to keep
            // the detector conservative.
            false
        } else {
            let drop = (depth_then - depth_now.min(depth_then)) as f64
                / depth_then as f64;
            drop >= self.depth_drop_pct
        };
        if !drop_ok {
            return None;
        }

        // 6. Aggression confirmation aligned with crash direction.
        let aligned = match dir {
            CrashDir::Down => self.sell_aggr >= self.min_aggr_trades,
            CrashDir::Up => self.buy_aggr >= self.min_aggr_trades,
        };
        if !aligned {
            return None;
        }

        // 7. Build the flag.
        let total_aggr = (self.buy_aggr + self.sell_aggr).max(1);
        let aligned_count = match dir {
            CrashDir::Down => self.sell_aggr,
            CrashDir::Up => self.buy_aggr,
        };
        let imbalance = aligned_count as f64 / total_aggr as f64;
        let move_score = (gap_bps as f64 / (self.min_gap_bps as f64 * 4.0)).clamp(0.0, 1.0);
        let depth_score = if depth_then > 0 {
            ((depth_then - depth_now.min(depth_then)) as f64 / depth_then as f64).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let severity = (0.5 * move_score + 0.3 * depth_score + 0.2 * imbalance).clamp(0.0, 1.0);

        let displacement = mid_after as i64 - snap.mid as i64;
        let flag = ChaosEvent {
            kind: ChaosKind::FlashCrash,
            start_seq: snap.seq,
            end_seq: seq_event.seq,
            severity,
            initiator: None,
            features: ChaosFeatures {
                event_count: (seq_event.seq - snap.seq) as u64,
                duration_ns: event.ts.saturating_sub(snap.ts),
                cancel_trade_ratio: 0.0,
                price_displacement: displacement,
                depth_removed: depth_then.saturating_sub(depth_now),
            },
        };

        // 8. Arm refractory and bleed counters.
        self.refractory_until = seq_event.seq.saturating_add(self.cooldown_seq);
        self.buy_aggr = 0;
        self.sell_aggr = 0;
        self.snap_len = 0;

        Some(flag)
    }

    /// Push a new snapshot into the circular ring, evicting the oldest
    /// when full. The ring keeps at most `gap_window_events + 1`
    /// entries so reads never need to skip stale snaps.
    #[inline]
    fn push_snap(&mut self, snap: TopSnapshot) {
        let cap = self.gap_window_events as usize + 1;
        let idx = self.snap_head as usize;
        self.snaps[idx] = snap;
        self.snap_head = ((idx + 1) % SNAP_CAP) as u8;
        if (self.snap_len as usize) < cap {
            self.snap_len += 1;
        }
        // When `snap_len` is already at `cap`, the next write
        // overwrites the oldest slot — no shifting required.
    }

    fn add_qty(&mut self, side: u8, price: u64, qty: u64) {
        let cap = self.max_book_levels;
        let book = self.book_mut(side);
        *book.entry(price).or_insert(0) += qty;

        // Pruning may evict the farthest-from-best level. Because
        // pruning is by definition NOT at best, it never invalidates
        // the cached best. It CAN evict a level inside the band
        // though, which means our `band_depth` cache could be stale.
        // Detect that by observing the BTreeMap len change.
        let pre_len = book.len();
        Self::prune_levels(book, side, cap);
        let pruned = pre_len != book.len();

        // Maintain best cache on the mutated side. An ADD can only
        // *improve* (never worsen) the best on its own side.
        let mut best_moved = false;
        match side {
            0 => match self.cached_bb {
                Some(bb) if price > bb => {
                    self.cached_bb = Some(price);
                    best_moved = true;
                }
                None => {
                    self.cached_bb = Some(price);
                    best_moved = true;
                }
                _ => {}
            },
            _ => match self.cached_ba {
                Some(ba) if price < ba => {
                    self.cached_ba = Some(price);
                    best_moved = true;
                }
                None => {
                    self.cached_ba = Some(price);
                    best_moved = true;
                }
                _ => {}
            },
        }

        // Cache update for band_depth.
        if best_moved || pruned {
            self.band_depth_valid = false;
        } else if self.band_depth_valid && self.in_band_same_side(side, price) {
            self.band_depth = self.band_depth.saturating_add(qty);
        }
    }

    fn sub_qty(&mut self, side: u8, price: u64, qty: u64) {
        let mut removed: u64 = 0;
        let mut level_emptied = false;
        let book = self.book_mut(side);
        if let Some(entry) = book.get_mut(&price) {
            let take = if qty == 0 { *entry } else { qty.min(*entry) };
            *entry -= take;
            removed = take;
            if *entry == 0 {
                book.remove(&price);
                level_emptied = true;
            }
        }

        // Maintain best cache on the mutated side. Best can only
        // change if THIS sub emptied the current best level.
        let mut best_moved = false;
        if level_emptied {
            match side {
                0 if self.cached_bb == Some(price) => {
                    self.cached_bb = self.bids.iter().next_back().map(|(&p, _)| p);
                    best_moved = true;
                }
                1 if self.cached_ba == Some(price) => {
                    self.cached_ba = self.asks.iter().next().map(|(&p, _)| p);
                    best_moved = true;
                }
                _ => {}
            }
        }

        if best_moved {
            self.band_depth_valid = false;
        } else if self.band_depth_valid
            && removed > 0
            && self.in_band_same_side(side, price)
        {
            self.band_depth = self.band_depth.saturating_sub(removed);
        }
    }

    /// Same-side band check used by the incremental cache update.
    /// `add_qty(side=X)` and `sub_qty(side=X)` only affect the
    /// `X`-side slice of the band, so we don't need to look at the
    /// other side here.
    #[inline]
    fn in_band_same_side(&self, side: u8, price: u64) -> bool {
        match side {
            0 => match self.cached_bb {
                Some(bb) => price >= bb.saturating_sub(self.depth_band_ticks) && price <= bb,
                None => false,
            },
            _ => match self.cached_ba {
                Some(ba) => price >= ba && price <= ba.saturating_add(self.depth_band_ticks),
                None => false,
            },
        }
    }

    fn book_mut(&mut self, side: u8) -> &mut BTreeMap<u64, u64> {
        if side == 0 {
            &mut self.bids
        } else {
            &mut self.asks
        }
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

#[inline]
fn bps_move(from: u64, to: u64) -> u32 {
    if from == 0 {
        return 0;
    }
    let diff = if to >= from { to - from } else { from - to };
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

    /// Seed a balanced book: `levels` levels each side, 1-tick apart,
    /// `qty_per` per level.
    fn seed_book(
        det: &mut FlashCrashDetector,
        mid: u64,
        levels: u64,
        qty_per: u64,
    ) -> u64 {
        let mut seq = 1u64;
        for i in 1..=levels {
            det.process(&ev(seq, seq * 1_000, EventType::OrderAdd, Side::Bid, mid - i, qty_per));
            seq += 1;
            det.process(&ev(seq, seq * 1_000, EventType::OrderAdd, Side::Ask, mid + i, qty_per));
            seq += 1;
        }
        seq
    }

    /// A single big trade that sweeps multiple bid levels (mid jumps
    /// down sharply) + depth collapses near top + sell aggression
    /// present -> FlashCrash flag.
    #[test]
    fn flags_downward_flash_crash() {
        // Realistic ITCH calibration: a sweep prints one trade message
        // per consumed level, so the gap is measured across ~5-8 raw
        // events. The semantic "1-2 events" applies to mid-moving
        // events; on a deep book those map to a tight raw-event burst.
        let mut det = FlashCrashDetector::new(
            /* min_gap_bps */ 2,
            /* gap_window_events */ 8,
            /* max_book_levels */ 32,
            /* depth_band_ticks */ 5,
            /* depth_drop_pct */ 0.5,
            /* min_aggr_trades */ 1,
            /* cooldown_seq */ 200,
        );

        let mut seq = seed_book(&mut det, 10_000, 10, 100);
        // Wipe out the top 6 bid levels in 6 cancels (large depth
        // collapse near top), then a sell-aggressive trade prints at
        // the new lower mid.
        for px in (9_995u64..=9_999).rev() {
            let s = seq;
            det.process(&ev(s, s * 1_000, EventType::OrderCancel, Side::Bid, px, 100));
            seq += 1;
        }
        // One synthetic aggressive sell trade at the new low to confirm
        // direction. Use the (now) best bid.
        let s = seq;
        let flag = det.process(&ev(s, s * 1_000, EventType::Trade, Side::Bid, 9_994, 100));
        // The flag may have already fired on one of the cancels (depth
        // collapse + gap, but no aggression yet); we must see at least
        // one flag by the time the trade lands.
        let triggered = flag.is_some();
        let mut total = if triggered { 1 } else { 0 };
        // Continue a few more events to give the detector a chance.
        seq += 1;
        for _ in 0..3 {
            let s = seq;
            if det.process(&ev(s, s * 1_000, EventType::Trade, Side::Bid, 9_994, 50)).is_some() {
                total += 1;
            }
            seq += 1;
        }
        assert!(total >= 1, "downward flash crash must flag");
    }

    /// A slow monotone drift over 8 events does NOT satisfy the
    /// gap-window-events=2 filter. FlashCrash must NOT flag (this is
    /// the territory of MomentumIgnition).
    #[test]
    fn no_flag_on_slow_drift() {
        let mut det = FlashCrashDetector::new(5, 2, 32, 5, 0.3, 1, 200);
        let mut seq = seed_book(&mut det, 10_000, 16, 100);
        for k in 0..8u64 {
            // One small cancel per event chips away at the top bid;
            // mid drifts by 1 tick at a time over many events.
            let px = 9_999 - k;
            let s = seq;
            let flag = det.process(&ev(s, s * 1_000, EventType::OrderCancel, Side::Bid, px, 100));
            assert!(flag.is_none(), "slow drift flagged at k={k}");
            seq += 1;
        }
    }

    /// A sharp gap with no depth collapse (because we widened the
    /// `depth_band_ticks` so far that the new mid still has plenty of
    /// depth nearby) must NOT flag.
    #[test]
    fn no_flag_when_depth_does_not_collapse() {
        let mut det = FlashCrashDetector::new(
            5, 2, 64,
            /* depth_band_ticks */ 200, // huge band -> always full
            0.9, // demand 90% drop, impossible
            1, 200,
        );
        let mut seq = seed_book(&mut det, 10_000, 32, 100);
        for px in (9_995u64..=9_999).rev() {
            let s = seq;
            det.process(&ev(s, s * 1_000, EventType::OrderCancel, Side::Bid, px, 100));
            seq += 1;
        }
        let s = seq;
        let flag = det.process(&ev(s, s * 1_000, EventType::Trade, Side::Bid, 9_994, 100));
        assert!(flag.is_none(), "huge band must defeat depth check");
    }

    /// Sharp gap + depth collapse but **no aggressive trades** -> no
    /// flag (aggression is required).
    #[test]
    fn no_flag_without_aggression() {
        let mut det = FlashCrashDetector::new(
            5, 2, 32, 5, 0.3,
            /* min_aggr_trades */ 2, 200,
        );
        let mut seq = seed_book(&mut det, 10_000, 16, 100);
        for px in (9_995u64..=9_999).rev() {
            let s = seq;
            let flag = det.process(&ev(s, s * 1_000, EventType::OrderCancel, Side::Bid, px, 100));
            assert!(flag.is_none(), "no-aggression cancel flagged at {px}");
            seq += 1;
        }
    }

    /// Two crashes back-to-back inside cooldown -> only the first
    /// flags.
    #[test]
    fn cooldown_suppresses_second_crash() {
        let mut det = FlashCrashDetector::new(2, 8, 64, 5, 0.3, 1, 1_000);
        let mut seq = seed_book(&mut det, 10_000, 32, 100);
        let mut hits = 0;

        // Crash #1
        for px in (9_995u64..=9_999).rev() {
            let s = seq;
            det.process(&ev(s, s * 1_000, EventType::OrderCancel, Side::Bid, px, 100));
            seq += 1;
        }
        let s = seq;
        if det.process(&ev(s, s * 1_000, EventType::Trade, Side::Bid, 9_994, 100)).is_some() {
            hits += 1;
        }
        seq += 1;

        // Crash #2 immediately
        for px in (9_990u64..=9_993).rev() {
            let s = seq;
            det.process(&ev(s, s * 1_000, EventType::OrderCancel, Side::Bid, px, 100));
            seq += 1;
        }
        let s = seq;
        if det.process(&ev(s, s * 1_000, EventType::Trade, Side::Bid, 9_989, 100)).is_some() {
            hits += 1;
        }
        assert_eq!(hits, 1, "cooldown must suppress second crash (got {hits})");
    }

    #[test]
    fn raw_delete_resolves_order_id_and_qty_zero() {
        let mut det = FlashCrashDetector::new(5, 2, 16, 5, 0.3, 1, 200);
        det.process(&ev_oid(1, 1_000, EventType::OrderAdd, Side::Bid, 9_999, 100, 11));
        det.process(&ev_oid(2, 2_000, EventType::OrderAdd, Side::Ask, 10_001, 100, 22));

        assert_eq!(det.best_bid(), Some(9_999));

        det.process(&ev_oid(3, 3_000, EventType::OrderCancel, Side::Ask, 0, 0, 11));

        assert_eq!(det.best_bid(), None);
    }

    // ─── Property tests ──────────────────────────────────────────────

    proptest! {
        /// On a deep, balanced, slowly-rotating book (only ADDs, no
        /// trades, no cancels) FlashCrash must never flag.
        #[test]
        fn no_flags_on_pure_add_stream(
            n in 50u64..400,
        ) {
            let mut det = FlashCrashDetector::new(5, 2, 64, 5, 0.3, 1, 200);
            let mut seq = seed_book(&mut det, 10_000, 32, 100);
            for k in 0..n {
                let s = seq;
                let side = if k % 2 == 0 { Side::Bid } else { Side::Ask };
                let px = if side == Side::Bid {
                    9_990_u64.saturating_sub(k % 8)
                } else {
                    10_010_u64.saturating_add(k % 8)
                };
                let flag = det.process(&ev(s, s * 1_000, EventType::OrderAdd, side, px, 100));
                prop_assert!(flag.is_none(),
                    "pure-add stream must not flag (k={k})");
                seq += 1;
            }
        }

        /// Random alternating tiny trades on a deep book never produce
        /// a sharp gap -> no flag.
        #[test]
        fn no_flags_on_alternating_micro_trades(
            n in 20u64..200,
        ) {
            let mut det = FlashCrashDetector::new(5, 2, 64, 5, 0.3, 1, 200);
            let mut seq = seed_book(&mut det, 10_000, 64, 1_000);
            for k in 0..n {
                let (side, price) = if k % 2 == 0 {
                    (Side::Ask, 10_001)
                } else {
                    (Side::Bid, 9_999)
                };
                let s = seq;
                let flag = det.process(&ev(s, s * 1_000, EventType::Trade, side, price, 1));
                prop_assert!(flag.is_none(),
                    "tiny alternating trades must not flag (k={k})");
                seq += 1;
                // Replenish the consumed level
                det.process(&ev(seq, seq * 1_000, EventType::OrderAdd, side, price, 1));
                seq += 1;
            }
        }
    }
}
