use std::collections::VecDeque;

use flowlab_core::hot_book::HotOrderBook;

/// Rolling spread tracker — ring buffer, zero reallocation in steady state.
pub struct SpreadTracker {
    history: VecDeque<u64>,
    window: usize,
    running_sum: u64,
}

impl SpreadTracker {
    pub fn new(window: usize) -> Self {
        Self {
            history: VecDeque::with_capacity(window),
            window,
            running_sum: 0,
        }
    }

    /// Record current spread. O(1) amortized — no shifts, no reallocs.
    pub fn record<const N: usize>(&mut self, book: &HotOrderBook<N>) -> Option<SpreadMetrics> {
        let spread = book.spread()?;

        if self.history.len() >= self.window {
            self.running_sum -= self.history.pop_front().unwrap_or(0);
        }
        self.history.push_back(spread);
        self.running_sum += spread;

        let mean = self.running_sum as f64 / self.history.len() as f64;
        let blowout_ratio = if mean > 0.0 {
            spread as f64 / mean
        } else {
            1.0
        };

        Some(SpreadMetrics {
            current: spread,
            mean,
            blowout_ratio,
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SpreadMetrics {
    pub current: u64,
    pub mean: f64,
    /// current / mean — >1.0 means spread is wider than average
    pub blowout_ratio: f64,
}
