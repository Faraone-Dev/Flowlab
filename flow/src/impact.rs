use std::collections::VecDeque;

use flowlab_core::event::{Event, EventType};

/// Trade impact model — measures price movement per unit of executed volume.
pub struct ImpactTracker {
    last_trade_price: Option<u64>,
    impacts: VecDeque<f64>,
    window: usize,
    running_sum: f64,
}

impl ImpactTracker {
    pub fn new(window: usize) -> Self {
        Self {
            last_trade_price: None,
            impacts: VecDeque::with_capacity(window),
            window,
            running_sum: 0.0,
        }
    }

    /// Process a trade event. Returns per-unit impact if computable.
    pub fn process_trade(&mut self, event: &Event) -> Option<f64> {
        if EventType::from_u8(event.event_type) != Some(EventType::Trade) {
            return None;
        }

        let impact = if let Some(prev_price) = self.last_trade_price {
            let price_move = (event.price as f64 - prev_price as f64).abs();
            let per_unit = if event.qty > 0 {
                price_move / event.qty as f64
            } else {
                0.0
            };

            // O(1) eviction
            if self.impacts.len() >= self.window {
                self.running_sum -= self.impacts.pop_front().unwrap_or(0.0);
            }
            self.impacts.push_back(per_unit);
            self.running_sum += per_unit;

            Some(per_unit)
        } else {
            None
        };

        self.last_trade_price = Some(event.price);
        impact
    }

    /// Mean impact over rolling window. O(1).
    pub fn mean_impact(&self) -> f64 {
        if self.impacts.is_empty() {
            return 0.0;
        }
        self.running_sum / self.impacts.len() as f64
    }
}
