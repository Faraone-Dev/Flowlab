// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

//! SyntheticSource — deterministic event generator for bringup and tests.
//!
//! NOT a market simulator. Coherent ADD/CANCEL/TRADE mix that keeps a
//! HotOrderBook populated, exercises every flow+risk path, and is
//! bit-reproducible from a seed.

use crate::source::Source;
use flowlab_core::event::{Event, EventType, Side};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct SyntheticSource {
    rng: StdRng,
    next_oid: u64,
    seq: u64,
    /// Pool of resting order_ids we can cancel/trade against.
    live_orders: Vec<(u64, u64, u8)>, // (oid, price_ticks, side)
    mid_ticks: i64,
    /// Episode regime — drives dashboard through CALM → VOLATILE →
    /// AGGRESSIVE → CRISIS so regime classifier and chaos strip move.
    phase: Phase,
    /// Events left in the current phase before transitioning.
    phase_remaining: u32,
}

/// Synthetic episode phase. Each emits ADD/CANCEL/TRADE primitives with
/// a skewed mix to drive downstream metrics:
///   * `Calm`           — symmetric flow, baseline VPIN/velocity.
///   * `ImbalanceBurst` — one-sided ADDs, imbalance → ±1.0.
///   * `TradeBurst`     — ~80% Trades on chosen side, drives velocity+VPIN.
///   * `Flash`          — cancel storm + one-sided trades, fires
///                        flash_crash + momentum_ignition past Crisis (6.0).
#[derive(Copy, Clone)]
enum Phase {
    Calm,
    ImbalanceBurst { side: u8 },
    TradeBurst { side: u8 },
    Flash { side: u8 },
}

impl SyntheticSource {
    pub fn new(seed: u64) -> Self {
        Self {
            rng: StdRng::seed_from_u64(seed),
            next_oid: 1,
            seq: 0,
            live_orders: Vec::with_capacity(8192),
            mid_ticks: 100_000_000, // 10000.0000 USD in 1/10000 ticks
            phase: Phase::Calm,
            phase_remaining: 4_000,
        }
    }

    fn now_ns() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    }

    /// Choose the next phase + how many events it should run for.
    fn pick_next_phase(&mut self) -> (Phase, u32) {
        // Bias toward Calm so the dashboard "rests" between events,
        // exactly like a real session: long calm stretches punctuated
        // by short bursts. Lengths are tuned so each phase is wide
        // enough to dominate the rolling baselines (ewma α ≈ 0.125).
        let r: u8 = self.rng.gen_range(0..100);
        let side = (self.rng.gen_range(0..2)) as u8;
        if r < 50 {
            (Phase::Calm, self.rng.gen_range(2_800..=5_500))
        } else if r < 73 {
            (Phase::ImbalanceBurst { side }, self.rng.gen_range(800..=1_500))
        } else if r < 88 {
            (Phase::TradeBurst { side }, self.rng.gen_range(450..=950))
        } else {
            // Slightly more common + slightly longer than before so the
            // demo hits regime=3 often enough to visibly exercise the
            // CRITICAL state, without turning the whole feed into chaos.
            (Phase::Flash { side }, self.rng.gen_range(260..=520))
        }
    }
}

impl Source for SyntheticSource {
    fn name(&self) -> &str {
        "synthetic"
    }

    fn is_live(&self) -> bool {
        // Conceptually finite, but for the engine it behaves as an infinite stream.
        true
    }

    fn next(&mut self) -> Option<Event> {
        self.seq = self.seq.wrapping_add(1);

        // ── episode scheduler. Drives the regime through CALM → VOLATILE
        //    → AGGRESSIVE → CRISIS so the dashboard's regime strip and
        //    chaos pattern panel actually move on synthetic data.
        if self.phase_remaining == 0 {
            let (p, n) = self.pick_next_phase();
            self.phase = p;
            self.phase_remaining = n;
        }
        self.phase_remaining -= 1;

        // Mid stays fixed by design. The synthetic source is for bringup
        // and dashboard demos; a free random-walking mid would let new
        // orders land "below the old best ask / above the old best bid"
        // and turn the cumulative book crossed within a few hundred
        // events (synthetic Trades don't actually match against the
        // book, so stale crossed levels never get cleared). Leaving the
        // mid fixed keeps the book strictly two-sided forever.
        // Real movement comes from the (asymmetric) flow of cancels and
        // partial fills exercising the best levels.

        // Per-phase mix. The split numbers (`add_pct`, `cancel_pct`)
        // and the optional `forced_side` are the only knobs the phases
        // touch — the actual event construction below stays uniform.
        let flash_mode = matches!(self.phase, Phase::Flash { .. });
        let (add_pct, cancel_pct, forced_side) = match self.phase {
            // 60 / 25 / 15 — symmetric baseline.
            Phase::Calm => (60u8, 85u8, None),
            // Heavy one-sided ADD, no cancels — drives book imbalance
            // and depth on the chosen side without churning trades.
            Phase::ImbalanceBurst { side } => (95, 100, Some(side)),
            // Trade-heavy, single direction — pumps trade velocity ratio
            // and VPIN. Falls back to add when the live pool is empty.
            Phase::TradeBurst { side } => (15, 20, Some(side)),
            // Flash: materially deplete one side of the top-of-book, then
            // repeatedly trade through the opposite best levels. Compared
            // with TradeBurst this is intentionally more violent so the
            // composite score crosses into CRITICAL during demos.
            Phase::Flash { side } => (2, 35, Some(side)),
        };

        let r: u8 = self.rng.gen_range(0..100);
        let (event_type, oid, price, qty, side) = if r < add_pct || self.live_orders.is_empty() {
            // ADD — pick a side, then pick a price on the CORRECT side of
            // the mid so the book never goes crossed by construction.
            // bid → strictly below mid, ask → strictly above mid; spread
            // distance is geometric-ish via gen_range so most orders land
            // near the top of the book.
            let side = forced_side.unwrap_or_else(|| if self.rng.gen_bool(0.5) { 0u8 } else { 1u8 });
            // Min offset 5 keeps us safe against the ±2 mid drift above
            // (worst-case need is min_offset > 2*max_drift = 4). During
            // imbalance / flash phases we still hug the top of the book
            // (offset 5..=7) so the imbalance metric — which only sees the
            // top 10 levels — reflects the asymmetric pressure. Calm
            // phase keeps the wide 5..=50 spread to populate depth.
            let offset: i64 = if forced_side.is_some() {
                self.rng.gen_range(5..=7)
            } else {
                self.rng.gen_range(5..=50)
            };
            let price = if side == 0 {
                (self.mid_ticks - offset).max(1) as u64
            } else {
                (self.mid_ticks + offset) as u64
            };
            let qty: u64 = self.rng.gen_range(10..=500);
            let oid = self.next_oid;
            self.next_oid += 1;
            self.live_orders.push((oid, price, side));
            // Soft cap on live set so the slab doesn't grow without bound.
            if self.live_orders.len() > 8192 {
                self.live_orders.swap_remove(0);
            }
            (EventType::OrderAdd, oid, price, qty, side)
        } else if r < cancel_pct {
            // CANCEL — prefer an order on the forced side when we have
            // one; otherwise take any. This keeps cancel storms one-sided
            // during Flash phases.
            let i = if let Some(target) = forced_side {
                if flash_mode {
                    self.live_orders
                        .iter()
                        .enumerate()
                        .filter(|(_, (_, _, s))| *s == target)
                        .max_by_key(|(_, (_, price, side))| {
                            if *side == 0 { *price } else { u64::MAX - *price }
                        })
                        .map(|(idx, _)| idx)
                        .unwrap_or_else(|| self.rng.gen_range(0..self.live_orders.len()))
                } else {
                    self.live_orders
                        .iter()
                        .position(|(_, _, s)| *s == target)
                        .unwrap_or_else(|| self.rng.gen_range(0..self.live_orders.len()))
                }
            } else {
                self.rng.gen_range(0..self.live_orders.len())
            };
            let (oid, price, side) = self.live_orders.swap_remove(i);
            (EventType::OrderCancel, oid, price, 0, side)
        } else {
            // TRADE — partial fill. Forced-side phases execute against
            // the opposite side of the book (a buy-side burst lifts asks,
            // a sell-side burst hits bids), which is what drives the
            // VPIN imbalance signal upward.
            let i = if let Some(target) = forced_side {
                let opposite = 1 - target;
                if flash_mode {
                    self.live_orders
                        .iter()
                        .enumerate()
                        .filter(|(_, (_, _, s))| *s == opposite)
                        .max_by_key(|(_, (_, price, side))| {
                            if *side == 0 { *price } else { u64::MAX - *price }
                        })
                        .map(|(idx, _)| idx)
                        .unwrap_or_else(|| self.rng.gen_range(0..self.live_orders.len()))
                } else {
                    self.live_orders
                        .iter()
                        .position(|(_, _, s)| *s == opposite)
                        .unwrap_or_else(|| self.rng.gen_range(0..self.live_orders.len()))
                }
            } else {
                self.rng.gen_range(0..self.live_orders.len())
            };
            let (oid, price, side) = self.live_orders[i];
            let fill: u64 = if flash_mode {
                self.rng.gen_range(35..=140)
            } else {
                self.rng.gen_range(1..=50)
            };
            (EventType::Trade, oid, price, fill, side)
        };

        Some(Event {
            ts: Self::now_ns(),
            price,
            qty,
            order_id: oid,
            instrument_id: 1,
            event_type: event_type as u8,
            side: match side {
                0 => Side::Bid as u8,
                _ => Side::Ask as u8,
            },
            _pad: [0; 2],
        })
    }
}
