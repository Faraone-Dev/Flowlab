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
    pub fn process_trade(&mut self, event: &Event) -> Option<f64> {
        if EventType::from_u8(event.event_type) != Some(EventType::Trade) {
            return None;
        }

        // Classify by aggressor side
        match Side::from_u8(event.side) {
            Some(Side::Bid) => self.current_buy_vol += event.qty,
            Some(Side::Ask) => self.current_sell_vol += event.qty,
            None => return None,
        }

        self.current_bucket_vol += event.qty;

        if self.current_bucket_vol >= self.bucket_size {
            let new_diff = (self.current_buy_vol as f64 - self.current_sell_vol as f64).abs();

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
