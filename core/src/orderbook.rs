// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

use std::collections::BTreeMap;

use crate::event::{Event, EventType, Side};
use crate::types::{OrderId, Price, Qty};

/// Single order resting on the book.
#[derive(Debug, Clone, Copy)]
pub struct Order {
    pub order_id: OrderId,
    pub price: Price,
    pub qty: Qty,
    pub side: Side,
}

/// Price level: aggregated quantity at a given price.
#[derive(Debug, Clone, Default)]
pub struct Level {
    pub price: Price,
    pub total_qty: Qty,
    pub order_count: u32,
}

/// L2 order book — price-level aggregated, deterministic state.
#[derive(Debug, Clone)]
pub struct OrderBook {
    pub instrument_id: u32,
    /// BTreeMap for sorted price levels (ascending)
    pub bids: BTreeMap<Price, Level>,
    pub asks: BTreeMap<Price, Level>,
    /// Individual orders for L3 reconstruction
    orders: std::collections::HashMap<OrderId, Order>,
}

impl OrderBook {
    pub fn new(instrument_id: u32) -> Self {
        Self {
            instrument_id,
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            orders: std::collections::HashMap::new(),
        }
    }

    /// Apply a single event to the book. Returns true if state changed.
    pub fn apply(&mut self, event: &Event) -> bool {
        let Some(event_type) = EventType::from_u8(event.event_type) else {
            return false;
        };

        match event_type {
            EventType::OrderAdd => self.add_order(event),
            EventType::OrderCancel => self.cancel_order(event),
            EventType::OrderModify => self.modify_order(event),
            EventType::Trade => self.execute_trade(event),
            EventType::BookSnapshot => false, // handled externally
        }
    }

    fn add_order(&mut self, event: &Event) -> bool {
        let Some(side) = Side::from_u8(event.side) else {
            return false;
        };

        let order = Order {
            order_id: event.order_id,
            price: event.price,
            qty: event.qty,
            side,
        };

        let levels = match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        };

        let level = levels.entry(event.price).or_insert_with(|| Level {
            price: event.price,
            total_qty: 0,
            order_count: 0,
        });
        level.total_qty += event.qty;
        level.order_count += 1;

        self.orders.insert(event.order_id, order);
        true
    }

    fn cancel_order(&mut self, event: &Event) -> bool {
        let oid = event.order_id;
        // Mirror HotOrderBook semantics: ITCH 'D' arrives with qty=0
        // (full delete using stored order qty), ITCH 'X' arrives with
        // qty=cancelled_shares (partial -- keep order alive if residual
        // remains).
        let Some(order) = self.orders.get_mut(&oid) else {
            return false;
        };

        let (price, deduct_qty, side, release_slot) =
            if event.qty == 0 || event.qty >= order.qty {
                let q = order.qty;
                let p = order.price;
                let s = order.side;
                self.orders.remove(&oid);
                (p, q, s, true)
            } else {
                order.qty -= event.qty;
                (order.price, event.qty, order.side, false)
            };

        let levels = match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        };

        if let Some(level) = levels.get_mut(&price) {
            level.total_qty = level.total_qty.saturating_sub(deduct_qty);
            if release_slot {
                level.order_count = level.order_count.saturating_sub(1);
                if level.order_count == 0 {
                    levels.remove(&price);
                }
            }
        }

        true
    }

    fn modify_order(&mut self, event: &Event) -> bool {
        // Cancel + re-add with new qty/price
        self.cancel_order(event);
        self.add_order(event)
    }

    fn execute_trade(&mut self, event: &Event) -> bool {
        let oid = event.order_id;
        if let Some(order) = self.orders.get_mut(&oid) {
            let old_qty = order.qty;
            order.qty = order.qty.saturating_sub(event.qty);

            let levels = match order.side {
                Side::Bid => &mut self.bids,
                Side::Ask => &mut self.asks,
            };

            if let Some(level) = levels.get_mut(&order.price) {
                level.total_qty = level.total_qty.saturating_sub(event.qty.min(old_qty));
                if order.qty == 0 {
                    level.order_count = level.order_count.saturating_sub(1);
                    if level.order_count == 0 {
                        levels.remove(&order.price);
                    }
                }
            }

            if order.qty == 0 {
                self.orders.remove(&oid);
            }

            true
        } else {
            false
        }
    }

    /// Best bid price (highest)
    pub fn best_bid(&self) -> Option<Price> {
        self.bids.keys().next_back().copied()
    }

    /// Best ask price (lowest)
    pub fn best_ask(&self) -> Option<Price> {
        self.asks.keys().next().copied()
    }

    /// Spread in ticks
    pub fn spread(&self) -> Option<u64> {
        match (self.best_bid(), self.best_ask()) {
            (Some(bid), Some(ask)) if ask > bid => Some(ask - bid),
            _ => None,
        }
    }

    /// Total bid depth across all levels
    pub fn bid_depth(&self) -> Qty {
        self.bids.values().map(|l| l.total_qty).sum()
    }

    /// Total ask depth across all levels
    pub fn ask_depth(&self) -> Qty {
        self.asks.values().map(|l| l.total_qty).sum()
    }

    /// **Canonical L2 hash** — same scheme as [`crate::hot_book::HotOrderBook::canonical_l2_hash`].
    /// Iterates bids from best (descending) and asks from best (ascending),
    /// hashing `(price, total_qty)` pairs. Implementations agree iff observable L2 state agrees.
    pub fn canonical_l2_hash(&self) -> u64 {
        let mut buf = [0u8; 16];
        let mut h = xxhash_rust::xxh3::xxh3_64(b"FLOWLAB-L2-v1");

        for (price, level) in self.bids.iter().rev() {
            buf[0..8].copy_from_slice(&price.to_le_bytes());
            buf[8..16].copy_from_slice(&level.total_qty.to_le_bytes());
            h ^= xxhash_rust::xxh3::xxh3_64_with_seed(&buf, h);
        }
        h = xxhash_rust::xxh3::xxh3_64_with_seed(b"|", h);
        for (price, level) in self.asks.iter() {
            buf[0..8].copy_from_slice(&price.to_le_bytes());
            buf[8..16].copy_from_slice(&level.total_qty.to_le_bytes());
            h ^= xxhash_rust::xxh3::xxh3_64_with_seed(&buf, h);
        }
        h
    }
}
