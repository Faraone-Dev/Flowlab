// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

//! Hot-path order book — fixed-depth sorted arrays + slab-backed order index.
//!
//! Mirrors the C++ `OrderBook<MaxLevels>` template exactly.
//!
//! Optimizations:
//!   L1: sorted arrays, no heap, no trees — cache-contiguous price grid
//!   L2: 64-byte aligned struct (no false sharing), prefetch on depth scan
//!   L3: SIMD depth sum (portable_simd), branch-free insertion
//!
//! Order index split for cache footprint:
//!   * `slab: Vec<OrderMeta>` — dense payload storage, reused via free_list.
//!   * `index: HashMap<u64, u32>` — order_id → slab slot. 12 B/entry instead
//!     of 32 B → ~5× more entries per cache line on lookup.
//!
//! aHash with **fixed seed** for deterministic iteration across runs (not
//! used in canonical L2 hash, but enables bit-exact replays).
//!
//! Hot-path α (frozen 2026-04-20, 3.79 GHz x86_64, 60/25/15 mix, 256 depth,
//! 500k events). See [`../../docs/latency-alpha.md`].
//!
//! | Metric                              | Baseline | α      |
//! |-------------------------------------|----------|--------|
//! | TRADE p50                           |  72 ns   |  30 ns |
//! | TRADE p99                           | 352 ns   | 128 ns |
//! | TOTAL p99                           | 160 ns   |  88 ns |
//! | Wall apply-only (500k events)       | 22.5 ms  | 17.1 ms|
//!
//! Wins: (1) `trade()` hot/cold split — partial-fill straight-line,
//! full-fill + anonymous-trade out-of-line `#[cold]`. (2) `prefetch_event`
//! `_mm_prefetch T0` on slab slot + bids/asks top, called one event ahead.
//!
//! `apply_lanes` (3-pass lane-separated apply) is **rejected** for realtime:
//! correct (canonical hash bit-exact) but +8.8% wall. Retained for batch.

use crate::event::{Event, EventType, Side};
use crate::types::Price;
use hashbrown::HashMap;
use std::hash::BuildHasherDefault;

/// Fixed-seed aHash builder — deterministic across runs and processes.
type FxBuild = BuildHasherDefault<ahash::AHasher>;

/// Single price level — 20 bytes + 4 pad = 24 bytes.
/// Aligned to 8 bytes so arrays of Level have predictable stride.
#[derive(Debug, Clone, Copy, Default)]
#[repr(C, align(8))]
pub struct Level {
    pub price: u64,
    pub total_qty: u64,
    pub order_count: u32,
    _pad: u32,
}

/// Fixed-depth L2 order book. No heap. No trees. No HashMap.
///
/// **L2**: `align(64)` ensures the struct starts on a cache-line boundary,
/// preventing false sharing when multiple books live in adjacent memory.
///
/// **L3**: Binary search + `ptr::copy` for insertion (memmove, not bubble swap),
/// SIMD-friendly depth summation via manual unrolling + compiler auto-vectorization.
///
/// Generic over `MAX_LEVELS` — compile-time depth like the C++ template.
/// Bids sorted descending, asks ascending — best prices always at index 0.
#[derive(Debug, Clone)]
#[repr(C, align(64))]
pub struct HotOrderBook<const MAX_LEVELS: usize = 256> {
    pub instrument_id: u32,
    bid_count: u32,
    ask_count: u32,
    _pad: u32,
    bids: [Level; MAX_LEVELS],
    /// Cache-line gap between bids and asks — prevents false sharing
    /// when the same book is read concurrently or when SIMD loads
    /// accidentally straddle the boundary.
    _gap: [u8; 64],
    asks: [Level; MAX_LEVELS],
    /// Off-hot-path: dense slab + sparse index for `order_id` resolution
    /// of ITCH cancel/trade messages.
    ///
    /// Heap-allocated and **always sits AFTER the cache-line gap** so the
    /// bid/ask grids stay isolated.
    orders: OrderSlab,
}

/// Per-order metadata: price it sits at, remaining qty, and side.
/// Stored in a dense `Vec` (the slab); the hash index points to it by `u32`.
///
/// Layout pinned: 8 + 8 + 1 + 7 pad = 24 B, align 8. Adding fields here
/// requires updating the compile-time `size_of` assert at the bottom of
/// the module.
#[derive(Debug, Clone, Copy)]
#[repr(C, align(8))]
pub struct OrderMeta {
    pub price: u64,
    pub qty: u64,
    pub side: u8,
    _pad: [u8; 7],
}

impl OrderMeta {
    #[inline]
    fn new(price: u64, qty: u64, side: u8) -> Self {
        Self { price, qty, side, _pad: [0; 7] }
    }
}

/// Slab-backed order index. Two layers:
///   * `data`: contiguous `Vec<OrderMeta>` — hot data lives here.
///   * `index`: `HashMap<order_id, u32>` — cold lookup, 12 B/entry.
#[derive(Debug, Clone)]
struct OrderSlab {
    data: Vec<OrderMeta>,
    free_list: Vec<u32>,
    index: HashMap<u64, u32, FxBuild>,
}

impl OrderSlab {
    fn with_capacity(cap: usize) -> Self {
        Self {
            data: Vec::with_capacity(cap),
            free_list: Vec::with_capacity(cap / 4),
            index: HashMap::with_capacity_and_hasher(cap, FxBuild::default()),
        }
    }

    /// Insert (or overwrite) the metadata for `order_id`. O(1) amortised.
    #[inline]
    fn insert(&mut self, order_id: u64, meta: OrderMeta) {
        match self.index.get(&order_id).copied() {
            Some(slot) => {
                self.data[slot as usize] = meta;
            }
            None => {
                let slot = if let Some(s) = self.free_list.pop() {
                    self.data[s as usize] = meta;
                    s
                } else {
                    let s = self.data.len() as u32;
                    self.data.push(meta);
                    s
                };
                self.index.insert(order_id, slot);
            }
        }
    }

    /// Borrow the metadata for `order_id`, mutably.
    #[inline]
    fn get_mut(&mut self, order_id: u64) -> Option<&mut OrderMeta> {
        let slot = *self.index.get(&order_id)? as usize;
        debug_assert!(slot < self.data.len(), "slab index points to stale slot {slot}");
        // SAFETY: index entries always reference live slab slots (debug-asserted).
        Some(unsafe { self.data.get_unchecked_mut(slot) })
    }

    /// Borrow the metadata for `order_id`, immutably.
    #[inline]
    fn get(&self, order_id: u64) -> Option<&OrderMeta> {
        let slot = *self.index.get(&order_id)? as usize;
        debug_assert!(slot < self.data.len(), "slab index points to stale slot {slot}");
        // SAFETY: index entries always reference live slab slots (debug-asserted).
        Some(unsafe { self.data.get_unchecked(slot) })
    }

    /// Remove the metadata for `order_id`, returning the previous value.
    /// Frees the slab slot for reuse.
    #[inline]
    fn remove(&mut self, order_id: u64) -> Option<OrderMeta> {
        let slot = self.index.remove(&order_id)?;
        let meta = self.data[slot as usize];
        self.free_list.push(slot);
        Some(meta)
    }
}

impl<const MAX_LEVELS: usize> HotOrderBook<MAX_LEVELS> {
    pub fn new(instrument_id: u32) -> Self {
        Self {
            instrument_id,
            bid_count: 0,
            ask_count: 0,
            _pad: 0,
            bids: [Level::default(); MAX_LEVELS],
            _gap: [0; 64],
            asks: [Level::default(); MAX_LEVELS],
            orders: OrderSlab::with_capacity(1024),
        }
    }

    /// Apply a single event. Returns true if state changed.
    #[inline]
    pub fn apply(&mut self, event: &Event) -> bool {
        let Some(event_type) = EventType::from_u8(event.event_type) else {
            return false;
        };
        match event_type {
            EventType::OrderAdd => self.add_order(event),
            EventType::OrderCancel => self.cancel_order(event),
            EventType::OrderModify => {
                self.cancel_order(event);
                self.add_order(event)
            }
            EventType::Trade => self.trade(event),
            EventType::BookSnapshot => false,
        }
    }

    /// **α prefetch hint** — `_mm_prefetch T0` on the slab slot for the
    /// referenced oid (TRADE / CANCEL / MODIFY) plus the top cache line
    /// of both bid and ask grids. Call one event ahead of `apply()`.
    ///
    /// Both grid sides are prefetched unconditionally: at the prefetch
    /// point the side is hidden behind the slab slot we're about to
    /// fetch, and 2× 64 B prefetches cost ~2 cycles vs branching on
    /// `is_bid`. ADD skips the slab prefetch (writes a fresh slot) but
    /// still hits the grid (`add_order` does a binary-search insert).
    ///
    /// No-op on non-x86_64 and when `order_id == 0`.
    #[inline]
    pub fn prefetch_event(&self, event: &Event) {
        #[cfg(target_arch = "x86_64")]
        {
            use core::arch::x86_64::{_MM_HINT_T0, _mm_prefetch};

            let t = event.event_type;
            let needs_lookup = t == EventType::Trade as u8
                || t == EventType::OrderCancel as u8
                || t == EventType::OrderModify as u8;

            // Slab slot prefetch — only for events that read an existing oid.
            if needs_lookup && event.order_id != 0 {
                if let Some(&slot) = self.orders.index.get(&event.order_id) {
                    let ptr = self.orders.data.as_ptr().wrapping_add(slot as usize);
                    unsafe { _mm_prefetch(ptr as *const i8, _MM_HINT_T0) };
                }
            }

            // Level-grid top-of-book prefetch — for ANY event that hits
            // `find_level` (ADD insertion, CANCEL, TRADE, MODIFY).
            let touches_grid = needs_lookup || t == EventType::OrderAdd as u8;
            if touches_grid {
                let bids_ptr = self.bids.as_ptr() as *const i8;
                let asks_ptr = self.asks.as_ptr() as *const i8;
                unsafe {
                    _mm_prefetch(bids_ptr, _MM_HINT_T0);
                    _mm_prefetch(asks_ptr, _MM_HINT_T0);
                }
            }
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            let _ = event;
        }
    }

    /// **Lane-separated batch apply** — three homogeneous passes
    /// (ADD, TRADE, CANCEL) over a window of events. Keeps each pass's
    /// working set L1-resident at the cost of triple loop overhead.
    ///
    /// Status: **rejected for realtime** (+8.8 % wall vs serial `apply`,
    /// see [`docs/latency-alpha.md`]). Retained for future offline /
    /// pre-bucketed batch contexts where the type-filter cost vanishes.
    ///
    /// ## Correctness
    ///
    /// Final post-batch state matches the interleaved `apply` loop on
    /// any ITCH-conformant input: unique oids per ADD, CANCEL/TRADE
    /// never precedes its ADD, MODIFY references a live oid. The lane
    /// order ADD → TRADE → CANCEL ensures that intra-batch TRADEs
    /// resolve against ADDs in pass 1 and intra-batch CANCELs see the
    /// post-fill residual. Mid-batch snapshots DIFFER from the serial
    /// loop — only the post-batch state is observable.
    ///
    /// Verified by `test_apply_lanes_matches_serial_canonical_hash`.
    pub fn apply_lanes(&mut self, events: &[Event]) -> usize {
        self.apply_lane_adds(events)
            + self.apply_lane_trades(events)
            + self.apply_lane_cancels(events)
    }

    /// ADD lane (and the ADD half of MODIFY). See `apply_lanes` for
    /// the correctness contract.
    #[inline]
    pub fn apply_lane_adds(&mut self, events: &[Event]) -> usize {
        let mut changes = 0usize;
        for e in events {
            if e.event_type == EventType::OrderAdd as u8
                || e.event_type == EventType::OrderModify as u8
            {
                if self.add_order(e) {
                    changes += 1;
                }
            }
        }
        changes
    }

    /// TRADE lane.
    #[inline]
    pub fn apply_lane_trades(&mut self, events: &[Event]) -> usize {
        let mut changes = 0usize;
        for e in events {
            if e.event_type == EventType::Trade as u8 {
                if self.trade(e) {
                    changes += 1;
                }
            }
        }
        changes
    }

    /// CANCEL lane.
    #[inline]
    pub fn apply_lane_cancels(&mut self, events: &[Event]) -> usize {
        let mut changes = 0usize;
        for e in events {
            if e.event_type == EventType::OrderCancel as u8 {
                if self.cancel_order(e) {
                    changes += 1;
                }
            }
        }
        changes
    }

    // ─── Accessors ───────────────────────────────────────────────────

    #[inline]
    pub fn best_bid(&self) -> Option<Price> {
        if self.bid_count > 0 {
            Some(self.bids[0].price)
        } else {
            None
        }
    }

    #[inline]
    pub fn best_ask(&self) -> Option<Price> {
        if self.ask_count > 0 {
            Some(self.asks[0].price)
        } else {
            None
        }
    }

    /// Size resting at the best bid level (0 if no bid).
    #[inline]
    pub fn best_bid_size(&self) -> u64 {
        if self.bid_count > 0 { self.bids[0].total_qty } else { 0 }
    }

    /// Size resting at the best ask level (0 if no ask).
    #[inline]
    pub fn best_ask_size(&self) -> u64 {
        if self.ask_count > 0 { self.asks[0].total_qty } else { 0 }
    }

    #[inline]
    pub fn spread(&self) -> Option<u64> {
        match (self.best_bid(), self.best_ask()) {
            (Some(bid), Some(ask)) if ask > bid => Some(ask - bid),
            _ => None,
        }
    }

    #[inline]
    pub fn bid_count(&self) -> usize {
        self.bid_count as usize
    }

    #[inline]
    pub fn ask_count(&self) -> usize {
        self.ask_count as usize
    }

    /// Read-only accessor into the order-id slab. Returns the live
    /// `OrderMeta` for `oid` if present. Useful for tests and tooling
    /// that need to introspect the L3 state (residual qty after a
    /// partial cancel, owning side, etc.).
    #[inline]
    pub fn order_meta(&self, oid: u64) -> Option<&OrderMeta> {
        self.orders.get(oid)
    }

    /// Bid depth across top `n` levels.
    #[inline]
    pub fn bid_depth(&self, n: usize) -> u64 {
        Self::depth_sum(&self.bids, self.bid_count as usize, n)
    }

    /// Ask depth across top `n` levels.
    #[inline]
    pub fn ask_depth(&self, n: usize) -> u64 {
        Self::depth_sum(&self.asks, self.ask_count as usize, n)
    }

    /// **L3**: 4-wide unrolled depth sum — 4 independent accumulators
    /// let the compiler emit `vpaddq` on AVX2 (or `add` chains on NEON).
    /// Tail handled with a `match` so each `dN` keeps an even share of
    /// the work (otherwise auto-vec collapses on the tail iteration).
    /// Prefetches the first 2 cache lines of `levels`.
    #[inline(always)]
    fn depth_sum(levels: &[Level; MAX_LEVELS], count: usize, n: usize) -> u64 {
        let n = n.min(count);

        if n > 0 {
            #[cfg(target_arch = "x86_64")]
            unsafe {
                let ptr = levels.as_ptr() as *const i8;
                core::arch::x86_64::_mm_prefetch(ptr,           core::arch::x86_64::_MM_HINT_T0);
                core::arch::x86_64::_mm_prefetch(ptr.add(64),   core::arch::x86_64::_MM_HINT_T0);
            }
        }

        let mut d0: u64 = 0;
        let mut d1: u64 = 0;
        let mut d2: u64 = 0;
        let mut d3: u64 = 0;

        let chunks = n / 4;
        let remainder = n % 4;

        for c in 0..chunks {
            let base = c * 4;
            d0 += levels[base].total_qty;
            d1 += levels[base + 1].total_qty;
            d2 += levels[base + 2].total_qty;
            d3 += levels[base + 3].total_qty;
        }

        // Tail: distribute across accumulators to preserve 4-wide symmetry.
        let tail = chunks * 4;
        match remainder {
            3 => {
                d0 += levels[tail].total_qty;
                d1 += levels[tail + 1].total_qty;
                d2 += levels[tail + 2].total_qty;
            }
            2 => {
                d0 += levels[tail].total_qty;
                d1 += levels[tail + 1].total_qty;
            }
            1 => {
                d0 += levels[tail].total_qty;
            }
            _ => {}
        }

        d0 + d1 + d2 + d3
    }

    /// Total bid depth (all levels).
    pub fn total_bid_depth(&self) -> u64 {
        self.bid_depth(self.bid_count as usize)
    }

    /// Total ask depth (all levels).
    pub fn total_ask_depth(&self) -> u64 {
        self.ask_depth(self.ask_count as usize)
    }

    /// Direct read-only access to bid levels slice.
    #[inline]
    pub fn bid_levels(&self) -> &[Level] {
        &self.bids[..self.bid_count as usize]
    }

    /// Direct read-only access to ask levels slice.
    #[inline]
    pub fn ask_levels(&self) -> &[Level] {
        &self.asks[..self.ask_count as usize]
    }

    /// **Canonical L2 hash** — cross-language verification primitive.
    ///
    /// Produces an xxh3 digest over (price, total_qty) pairs, bids first
    /// (descending) then asks (ascending). Identical scheme as C++ in
    /// [`hotpath/src/ffi.cpp`](../../hotpath/src/ffi.cpp) `flowlab_orderbook_hash`.
    ///
    /// Note the deliberate asymmetry between level fold and side
    /// separator: levels are **XOR-folded** into `h`, the `'|'`
    /// separator **replaces** `h`. The C++ implementation does the same
    /// — changing either side breaks cross-impl agreement.
    pub fn canonical_l2_hash(&self) -> u64 {
        let mut buf = [0u8; 16];
        // Domain tag so empty book still hashes to a non-zero, namespaced value.
        let mut h = xxhash_rust::xxh3::xxh3_64(b"FLOWLAB-L2-v1");

        for l in self.bid_levels() {
            buf[0..8].copy_from_slice(&l.price.to_le_bytes());
            buf[8..16].copy_from_slice(&l.total_qty.to_le_bytes());
            h ^= xxhash_rust::xxh3::xxh3_64_with_seed(&buf, h);
        }
        // Side separator REPLACES h (matches C++); levels XOR-fold into h.
        h = xxhash_rust::xxh3::xxh3_64_with_seed(b"|", h);
        for l in self.ask_levels() {
            buf[0..8].copy_from_slice(&l.price.to_le_bytes());
            buf[8..16].copy_from_slice(&l.total_qty.to_le_bytes());
            h ^= xxhash_rust::xxh3::xxh3_64_with_seed(&buf, h);
        }
        h
    }

    // ─── Mutations ───────────────────────────────────────────────────

    /// **L3**: Binary search for the level, then either update in place
    /// (existing) or `ptr::copy` (memmove) to open a slot for a new level.
    ///
    /// Also records the order in the `orders` lookup map (iff `order_id != 0`)
    /// so subsequent `OrderCancel` / `Trade` messages can resolve by order_id
    /// alone \u2014 matching real ITCH 5.0 semantics.
    fn add_order(&mut self, e: &Event) -> bool {
        let price = e.price;
        let qty = e.qty;
        let is_bid = e.side == Side::Bid as u8;

        let (levels, count) = if is_bid {
            (&mut self.bids, &mut self.bid_count)
        } else {
            (&mut self.asks, &mut self.ask_count)
        };
        let cnt = *count as usize;

        // Binary search for insertion point. `partition_point` is
        // branchless on monotonic predicates.
        let pos = if is_bid {
            // Bids sorted descending: find first index where price < levels[i].price
            levels[..cnt].partition_point(|l| l.price > price)
        } else {
            // Asks sorted ascending
            levels[..cnt].partition_point(|l| l.price < price)
        };

        // Level already exists?
        let level_ok = if pos < cnt && levels[pos].price == price {
            levels[pos].total_qty += qty;
            levels[pos].order_count += 1;
            true
        } else if cnt >= MAX_LEVELS {
            // New level — capacity exhausted
            false
        } else {
            // memmove to open slot at `pos`
            if pos < cnt {
                unsafe {
                    let src = levels.as_ptr().add(pos);
                    let dst = levels.as_mut_ptr().add(pos + 1);
                    core::ptr::copy(src, dst, cnt - pos);
                }
            }
            levels[pos] = Level {
                price,
                total_qty: qty,
                order_count: 1,
                _pad: 0,
            };
            *count = (cnt + 1) as u32;
            true
        };

        if level_ok && e.order_id != 0 {
            self.orders.insert(e.order_id, OrderMeta::new(price, qty, e.side));
        }
        level_ok
    }

    /// Find level index by price using binary search.
    /// Returns `Some(idx)` if found.
    #[inline]
    fn find_level(levels: &[Level], cnt: usize, price: Price, is_bid: bool) -> Option<usize> {
        let pos = if is_bid {
            levels[..cnt].partition_point(|l| l.price > price)
        } else {
            levels[..cnt].partition_point(|l| l.price < price)
        };
        if pos < cnt && levels[pos].price == price {
            Some(pos)
        } else {
            None
        }
    }

    /// Cancel an order. ITCH 5.0 carries two flavours that both arrive
    /// as `EventType::OrderCancel`:
    ///
    ///   * `D` Order Delete  -> full removal. `e.qty == 0`. We drop the
    ///     entire resting qty using the stored `meta.qty` and free the
    ///     slab slot.
    ///   * `X` Order Cancel  -> partial removal. `e.qty > 0` and equals
    ///     the `cancelled_shares` field. We deduct `e.qty` from both
    ///     the level total and the slab `meta.qty`. The slab slot is
    ///     released only when the residual `meta.qty` reaches zero;
    ///     in that case the level's `order_count` is decremented too.
    ///
    /// Resolution priority:
    ///   1. `order_id != 0` and present in the lookup map -> use stored
    ///      `(price, side)` and partial/full logic above.
    ///   2. `order_id != 0` and not present -> drop the message (return
    ///      `false`), matching `OrderBook` (BTreeMap) behaviour on
    ///      unknown cancel refs.
    ///   3. `order_id == 0` (legacy / synthetic pre-enriched events) ->
    ///      fall back to `(event.price, event.side, event.qty)` and
    ///      treat as a full delete (single anonymous order).
    fn cancel_order(&mut self, e: &Event) -> bool {
        // Resolve target level + cancelled qty + whether the slab slot
        // should be released after the deduction.
        let (price, qty, is_bid, release_slot) = if e.order_id != 0 {
            // ITCH 'X' (partial) carries cancelled_shares > 0; ITCH 'D'
            // (full delete) carries 0. Inspect the slab before deciding
            // to release the slot -- partial cancels keep the order alive.
            let Some(meta) = self.orders.get_mut(e.order_id) else {
                return false;
            };
            let is_bid = meta.side == Side::Bid as u8;
            let price = meta.price;
            if e.qty == 0 || e.qty >= meta.qty {
                // Full delete (or 'X' that wipes the residual).
                let removed_qty = meta.qty;
                self.orders.remove(e.order_id);
                (price, removed_qty, is_bid, true)
            } else {
                // True partial: deduct cancelled_shares from the slab,
                // keep the slot live for further cancels / fills.
                meta.qty -= e.qty;
                (price, e.qty, is_bid, false)
            }
        } else {
            (e.price, e.qty, e.side == Side::Bid as u8, true)
        };

        let (levels, count) = if is_bid {
            (&mut self.bids, &mut self.bid_count)
        } else {
            (&mut self.asks, &mut self.ask_count)
        };
        let cnt = *count as usize;

        if let Some(i) = Self::find_level(levels, cnt, price, is_bid) {
            levels[i].total_qty = levels[i].total_qty.saturating_sub(qty);
            // Only decrement order_count when the underlying order is
            // actually being released. Partial cancels leave the order
            // (and the level's order_count) intact.
            if release_slot {
                levels[i].order_count = levels[i].order_count.saturating_sub(1);
                if levels[i].order_count == 0 {
                    Self::remove_level(levels, count, i);
                }
            }
            return true;
        }
        false
    }

    /// Apply a trade. Uses the same `order_id`-first resolution as `cancel_order`.
    /// The traded qty is deducted from the level; if the order is fully
    /// consumed, its metadata entry is dropped and the level's order_count
    /// is decremented, possibly removing the level.
    ///
    /// When `order_id != 0` and the order is not in the lookup map, the
    /// trade is dropped (return `false`) \u2014 matching `OrderBook` (BTreeMap)
    /// semantics where `execute_trade` fails on unknown order refs (typical
    /// of ITCH `P` messages that reference trades outside the book).
    #[inline]
    fn trade(&mut self, e: &Event) -> bool {
        // Synthetic / pre-enriched path (no oid).
        if e.order_id == 0 {
            return self.trade_anon(e.price, e.qty, e.side == Side::Bid as u8);
        }

        let Some(meta) = self.orders.get_mut(e.order_id) else {
            return false;
        };
        let fill = e.qty.min(meta.qty);
        let new_qty = meta.qty - fill;
        let price = meta.price;
        let is_bid = meta.side == Side::Bid as u8;

        if new_qty != 0 {
            // HOT: partial fill. No slab eviction, no level removal,
            // no order_count change. Single straight-line update.
            meta.qty = new_qty;
            let (levels, count) = if is_bid {
                (&mut self.bids[..], self.bid_count as usize)
            } else {
                (&mut self.asks[..], self.ask_count as usize)
            };
            if let Some(i) = Self::find_level(levels, count, price, is_bid) {
                levels[i].total_qty = levels[i].total_qty.saturating_sub(fill);
                return true;
            }
            return false;
        }

        // COLD: order fully consumed.
        self.trade_full_fill(e.order_id, price, fill, is_bid)
    }

    /// Cold path: order fully consumed by the trade. Out-of-line so the
    /// hot partial-fill path stays branch-light and i-cache compact.
    #[cold]
    #[inline(never)]
    fn trade_full_fill(&mut self, order_id: u64, price: u64, fill: u64, is_bid: bool) -> bool {
        self.orders.remove(order_id);
        let (levels, count) = if is_bid {
            (&mut self.bids, &mut self.bid_count)
        } else {
            (&mut self.asks, &mut self.ask_count)
        };
        let cnt = *count as usize;
        if let Some(i) = Self::find_level(levels, cnt, price, is_bid) {
            levels[i].total_qty = levels[i].total_qty.saturating_sub(fill);
            levels[i].order_count = levels[i].order_count.saturating_sub(1);
            if levels[i].order_count == 0 {
                Self::remove_level(levels, count, i);
            }
            return true;
        }
        false
    }

    /// Anonymous TRADE (no order_id). Pre-enriched / synthetic stream.
    #[cold]
    #[inline(never)]
    fn trade_anon(&mut self, price: u64, qty: u64, is_bid: bool) -> bool {
        let (levels, count) = if is_bid {
            (&mut self.bids, &mut self.bid_count)
        } else {
            (&mut self.asks, &mut self.ask_count)
        };
        let cnt = *count as usize;
        if let Some(i) = Self::find_level(levels, cnt, price, is_bid) {
            levels[i].total_qty = levels[i].total_qty.saturating_sub(qty);
            if levels[i].total_qty == 0 {
                levels[i].order_count = levels[i].order_count.saturating_sub(1);
                if levels[i].order_count == 0 {
                    Self::remove_level(levels, count, i);
                }
            }
            return true;
        }
        false
    }

    /// Remove level at index `i` — memmove remaining left.
    #[inline]
    fn remove_level(levels: &mut [Level; MAX_LEVELS], count: &mut u32, i: usize) {
        let cnt = *count as usize;
        if i < cnt - 1 {
            unsafe {
                let src = levels.as_ptr().add(i + 1);
                let dst = levels.as_mut_ptr().add(i);
                core::ptr::copy(src, dst, cnt - i - 1);
            }
        }
        levels[cnt - 1] = Level::default();
        *count = (cnt - 1) as u32;
    }
}

// ─── Compile-time verification ───────────────────────────────────────

const _: () = {
    // Level is 24 bytes (8-byte aligned)
    assert!(core::mem::size_of::<Level>() == 24);
    // OrderMeta is 24 bytes (8-byte aligned) — keeps the slab dense
    // and predictable across slot reuses.
    assert!(core::mem::size_of::<OrderMeta>() == 24);
    assert!(core::mem::align_of::<OrderMeta>() == 8);
    // HotOrderBook<256> is 64-byte aligned
    assert!(core::mem::align_of::<HotOrderBook<256>>() == 64);
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EventType, Side};

    fn make_event(event_type: EventType, side: Side, price: u64, qty: u64) -> Event {
        Event {
            ts: 0,
            instrument_id: 1,
            event_type: event_type as u8,
            side: side as u8,
            price,
            qty,
            order_id: 0,
            _pad: [0; 2],
        }
    }

    fn make_event_oid(
        event_type: EventType,
        side: Side,
        price: u64,
        qty: u64,
        order_id: u64,
    ) -> Event {
        Event {
            ts: 0,
            instrument_id: 1,
            event_type: event_type as u8,
            side: side as u8,
            price,
            qty,
            order_id,
            _pad: [0; 2],
        }
    }

    #[test]
    fn test_cache_line_alignment() {
        let book = HotOrderBook::<64>::new(1);
        let addr = &book as *const _ as usize;
        assert_eq!(addr % 64, 0, "HotOrderBook must be 64-byte aligned");
    }

    #[test]
    fn test_add_and_spread() {
        let mut book = HotOrderBook::<64>::new(1);

        book.apply(&make_event(EventType::OrderAdd, Side::Bid, 100, 10));
        book.apply(&make_event(EventType::OrderAdd, Side::Ask, 105, 5));

        assert_eq!(book.best_bid(), Some(100));
        assert_eq!(book.best_ask(), Some(105));
        assert_eq!(book.spread(), Some(5));
        assert_eq!(book.bid_depth(1), 10);
        assert_eq!(book.ask_depth(1), 5);
    }

    #[test]
    fn test_cancel_removes_level() {
        let mut book = HotOrderBook::<64>::new(1);
        book.apply(&make_event(EventType::OrderAdd, Side::Bid, 100, 10));
        book.apply(&make_event(EventType::OrderCancel, Side::Bid, 100, 10));

        assert_eq!(book.bid_count(), 0);
        assert_eq!(book.best_bid(), None);
    }

    #[test]
    fn test_anon_trade_full_fill_removes_level() {
        let mut book = HotOrderBook::<64>::new(1);
        book.apply(&make_event(EventType::OrderAdd, Side::Bid, 100, 10));
        book.apply(&make_event(EventType::Trade, Side::Bid, 100, 10));

        assert_eq!(book.bid_count(), 0);
        assert_eq!(book.best_bid(), None);
    }

    #[test]
    fn test_partial_cancel_keeps_order_alive() {
        // ITCH 'X' Order Cancel: cancelled_shares < resting qty.
        // The order must stay in the slab with the residual qty, the
        // level total_qty must be deducted, but order_count must NOT
        // decrement (the level still hosts one live order).
        let mut book = HotOrderBook::<64>::new(1);

        // Two orders at the same level so we can detect a wrong
        // order_count decrement (would drop to 1 instead of staying 2).
        book.apply(&make_event_oid(EventType::OrderAdd, Side::Bid, 100, 10, 1));
        book.apply(&make_event_oid(EventType::OrderAdd, Side::Bid, 100, 5, 2));

        assert_eq!(book.bid_levels()[0].total_qty, 15);
        assert_eq!(book.bid_levels()[0].order_count, 2);

        // Partial cancel: oid=1 loses 3 shares, residual = 7.
        book.apply(&make_event_oid(EventType::OrderCancel, Side::Bid, 0, 3, 1));

        assert_eq!(
            book.bid_levels()[0].total_qty,
            12,
            "level total_qty must drop by exactly the cancelled qty"
        );
        assert_eq!(
            book.bid_levels()[0].order_count,
            2,
            "partial cancel must NOT decrement order_count"
        );
        let meta = book
            .order_meta(1)
            .expect("oid 1 must still be live in the slab after partial cancel");
        assert_eq!(meta.qty, 7, "slab qty must reflect the residual");

        // Wiping the residual (qty == residual) must promote to a full
        // delete and free the slab slot + decrement order_count.
        book.apply(&make_event_oid(EventType::OrderCancel, Side::Bid, 0, 7, 1));
        assert!(book.order_meta(1).is_none(), "oid 1 must be released");
        assert_eq!(book.bid_levels()[0].total_qty, 5);
        assert_eq!(book.bid_levels()[0].order_count, 1);

        // ITCH 'D' (qty=0) on remaining oid=2 must fully delete it.
        book.apply(&make_event_oid(EventType::OrderCancel, Side::Bid, 0, 0, 2));
        assert_eq!(book.bid_count(), 0, "level must be removed");
        assert!(book.order_meta(2).is_none());
    }

    #[test]
    fn test_sorted_insertion_binary_search() {
        let mut book = HotOrderBook::<64>::new(1);

        // Add bids out of order — binary search insertion
        book.apply(&make_event(EventType::OrderAdd, Side::Bid, 100, 10));
        book.apply(&make_event(EventType::OrderAdd, Side::Bid, 110, 5));
        book.apply(&make_event(EventType::OrderAdd, Side::Bid, 105, 3));

        // Bids descending: 110, 105, 100
        assert_eq!(book.bid_levels()[0].price, 110);
        assert_eq!(book.bid_levels()[1].price, 105);
        assert_eq!(book.bid_levels()[2].price, 100);

        // Add asks out of order
        book.apply(&make_event(EventType::OrderAdd, Side::Ask, 120, 2));
        book.apply(&make_event(EventType::OrderAdd, Side::Ask, 115, 7));

        // Asks ascending: 115, 120
        assert_eq!(book.ask_levels()[0].price, 115);
        assert_eq!(book.ask_levels()[1].price, 120);
    }

    #[test]
    fn test_trade_depletes() {
        let mut book = HotOrderBook::<64>::new(1);
        // Use a tracked order_id so `trade` resolves via the lookup map
        // (which decrements `order_count` on full fills, matching the
        // BTreeMap reference implementation).
        book.apply(&make_event_oid(EventType::OrderAdd, Side::Ask, 100, 20, 42));

        // Partial fill: 15 of 20
        book.apply(&make_event_oid(EventType::Trade, Side::Ask, 100, 15, 42));
        assert_eq!(book.ask_levels()[0].total_qty, 5);
        assert_eq!(book.ask_count(), 1);

        // Full fill of remainder: order_count -> 0 removes the level
        book.apply(&make_event_oid(EventType::Trade, Side::Ask, 100, 5, 42));
        assert_eq!(book.ask_count(), 0);
    }

    #[test]
    fn test_depth_sum_unrolled() {
        let mut book = HotOrderBook::<64>::new(1);

        // Add 7 bid levels to test 4-wide unroll + 3 remainder
        for p in 0..7u64 {
            book.apply(&make_event(EventType::OrderAdd, Side::Bid, 100 + p, 10));
        }

        assert_eq!(book.bid_depth(7), 70);
        assert_eq!(book.bid_depth(4), 40);
        assert_eq!(book.bid_depth(1), 10);
    }

    #[test]
    fn test_many_levels_stress() {
        let mut book = HotOrderBook::<256>::new(1);

        // Fill 200 bid levels
        for p in 0..200u64 {
            book.apply(&make_event(EventType::OrderAdd, Side::Bid, 1000 + p, 1));
        }
        assert_eq!(book.bid_count(), 200);
        // Best bid should be highest price (1199)
        assert_eq!(book.best_bid(), Some(1199));

        // Cancel middle level
        book.apply(&make_event(EventType::OrderCancel, Side::Bid, 1100, 1));
        assert_eq!(book.bid_count(), 199);

        // Levels should still be sorted
        let levels = book.bid_levels();
        for w in levels.windows(2) {
            assert!(w[0].price > w[1].price, "bids must be descending");
        }
    }

    #[test]
    fn test_itch_cancel_resolves_via_order_id() {
        // ITCH `OrderDelete` (`D`) carries only `order_id` — price/qty/side
        // are zero on the wire. The book must still locate the owning
        // level via the internal lookup map.
        let mut book = HotOrderBook::<64>::new(1);
        book.apply(&make_event_oid(EventType::OrderAdd, Side::Bid, 100, 10, 7));
        book.apply(&make_event_oid(EventType::OrderAdd, Side::Bid, 100, 15, 8));
        assert_eq!(book.bid_count(), 1);
        assert_eq!(book.bid_levels()[0].total_qty, 25);

        // ITCH-style cancel: price=0, qty=0, side=0 — resolved via order_id=7
        let cancel = make_event_oid(EventType::OrderCancel, Side::Bid, 0, 0, 7);
        assert!(book.apply(&cancel));
        assert_eq!(book.bid_count(), 1);
        assert_eq!(book.bid_levels()[0].total_qty, 15); // qty deducted by 10 (stored)

        // Unknown order_id -> dropped (matches BTreeMap OrderBook)
        let bogus = make_event_oid(EventType::OrderCancel, Side::Bid, 0, 0, 999);
        assert!(!book.apply(&bogus));
    }

    /// **Lane equivalence** — `apply_lanes` must produce the same
    /// canonical L2 hash as the serial `apply` loop on any
    /// ITCH-conformant input (unique oids, CANCEL/TRADE strictly after
    /// ADD, no oid reuse). Final state bit-exact identical.
    #[test]
    fn test_apply_lanes_matches_serial_canonical_hash() {
        let mut events: Vec<Event> = Vec::with_capacity(2_000);
        let mut next_oid: u64 = 1;
        let mut live: Vec<(u64, u64, u8)> = Vec::new(); // (oid, price, side)

        // Deterministic xorshift inline to avoid cross-crate deps.
        let mut s: u64 = 0xA17E_F10A_BABE_2026;
        let mut r = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };

        for i in 0..2_000u64 {
            let pick = r() % 100;
            let evt = if pick < 60 || live.is_empty() {
                let side = (r() & 1) as u8;
                let price = 10_000u64.saturating_sub(r() % 50) + (side as u64) * 100;
                let qty = 10 + (r() % 90);
                let oid = next_oid;
                next_oid += 1;
                live.push((oid, price, side));
                make_event_oid(EventType::OrderAdd,
                    if side == 0 { Side::Bid } else { Side::Ask },
                    price, qty, oid)
            } else if pick < 85 {
                let idx = (r() as usize) % live.len();
                let (oid, _, side) = live.swap_remove(idx);
                make_event_oid(EventType::OrderCancel,
                    if side == 0 { Side::Bid } else { Side::Ask },
                    0, 0, oid)
            } else {
                let idx = (r() as usize) % live.len();
                let (oid, _, side) = live[idx];
                make_event_oid(EventType::Trade,
                    if side == 0 { Side::Bid } else { Side::Ask },
                    0, 1, oid)
            };
            let mut e = evt;
            e.ts = i * 1_000;
            events.push(e);
        }

        let mut serial = HotOrderBook::<256>::new(1);
        for e in &events {
            serial.apply(e);
        }

        let mut lanes = HotOrderBook::<256>::new(1);
        // Process in batches of 64 to reflect realistic poll-cadence usage.
        for chunk in events.chunks(64) {
            lanes.apply_lanes(chunk);
        }

        assert_eq!(
            serial.canonical_l2_hash(),
            lanes.canonical_l2_hash(),
            "apply_lanes must produce identical post-batch state to serial apply"
        );
    }
}
