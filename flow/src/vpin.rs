// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

use std::collections::VecDeque;

use flowlab_core::event::{Event, EventType, Side};

/// Volume-Synchronized Probability of Informed Trading (VPIN).
/// Estimates toxic flow by classifying trade volume as buy/sell-initiated.
pub struct VpinCalculator {
    bucket_size: u64,
    num_buckets: usize,
    current_buy_vol: u64,
    current_sell_vol: u64,
    current_bucket_vol: u64,
    buckets: VecDeque<(u64, u64)>, // (buy_vol, sell_vol) per bucket
    running_abs_diff: f64,         // cached sum for O(1) recalc
}

impl VpinCalculator {
    pub fn new(bucket_size: u64, num_buckets: usize) -> Self {
        Self {
            bucket_size,
            num_buckets,
            current_buy_vol: 0,
            current_sell_vol: 0,
            current_bucket_vol: 0,
            buckets: VecDeque::with_capacity(num_buckets),
            running_abs_diff: 0.0,
        }
    }

    /// Process a trade event. Returns updated VPIN if a bucket completed.
    ///
    /// Uses the raw `event.side` field. For NASDAQ ITCH `'P'` messages
    /// this is the side of the *resting non-displayed order*, NOT the
    /// aggressor, so VPIN computed this way saturates near 1.0 in
    /// directional minutes. Prefer `process_trade_signed` from
    /// engine code that has already classified the aggressor with the
    /// quote rule (price vs best bid/ask).
    pub fn process_trade(&mut self, event: &Event) -> Option<f64> {
        if EventType::from_u8(event.event_type) != Some(EventType::Trade) {
            return None;
        }
        let signed = match Side::from_u8(event.side) {
            Some(Side::Bid) => 1i8,
            Some(Side::Ask) => -1i8,
            None => return None,
        };
        self.process_signed(event.qty, signed)
    }

    /// Process a trade with an externally-computed aggressor sign:
    ///   `+1` = buy-initiated (price >= best_ask)
    ///   `-1` = sell-initiated (price <= best_bid)
    ///    `0` = inside spread / unknown -> split 50/50
    pub fn process_trade_signed(&mut self, qty: u64, aggressor: i8) -> Option<f64> {
        self.process_signed(qty, aggressor)
    }

    fn process_signed(&mut self, qty: u64, aggressor: i8) -> Option<f64> {
        if qty == 0 {
            return None;
        }
        match aggressor {
            1 => self.current_buy_vol += qty,
            -1 => self.current_sell_vol += qty,
            _ => {
                // Inside spread: split the print evenly so the bucket
                // doesn't end up biased by an unsignable trade.
                let half = qty / 2;
                self.current_buy_vol += half;
                self.current_sell_vol += qty - half;
            }
        }

        self.current_bucket_vol += qty;

        if self.current_bucket_vol >= self.bucket_size {
            let new_diff =
                (self.current_buy_vol as f64 - self.current_sell_vol as f64).abs();

            // Evict oldest bucket — O(1) pop_front
            if self.buckets.len() >= self.num_buckets {
                if let Some((old_b, old_s)) = self.buckets.pop_front() {
                    self.running_abs_diff -= (old_b as f64 - old_s as f64).abs();
                }
            }

            self.buckets
                .push_back((self.current_buy_vol, self.current_sell_vol));
            self.running_abs_diff += new_diff;

            self.current_buy_vol = 0;
            self.current_sell_vol = 0;
            self.current_bucket_vol = 0;

            Some(self.calculate())
        } else {
            None
        }
    }

    /// VPIN = mean(|buy_vol - sell_vol|) / bucket_size
    /// O(1) — uses cached running sum.
    fn calculate(&self) -> f64 {
        if self.buckets.is_empty() {
            return 0.0;
        }
        self.running_abs_diff / (self.buckets.len() as f64 * self.bucket_size as f64)
    }

    pub fn vpin(&self) -> f64 {
        self.calculate()
    }
}
