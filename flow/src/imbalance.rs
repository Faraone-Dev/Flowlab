// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

use flowlab_core::hot_book::HotOrderBook;

/// Order book imbalance at top N levels.
/// Returns value in [-1.0, 1.0]: positive = bid-heavy, negative = ask-heavy.
/// Direct slice access — no tree traversal, no pointer-chasing.
pub fn book_imbalance<const M: usize>(book: &HotOrderBook<M>, levels: usize) -> f64 {
    let bid_qty = book.bid_depth(levels);
    let ask_qty = book.ask_depth(levels);

    let total = bid_qty + ask_qty;
    if total == 0 {
        return 0.0;
    }

    (bid_qty as f64 - ask_qty as f64) / total as f64
}

/// Depth at top N levels per side.
pub struct DepthSnapshot {
    pub bid_depth: u64,
    pub ask_depth: u64,
    pub bid_levels: usize,
    pub ask_levels: usize,
}

pub fn depth_snapshot<const M: usize>(book: &HotOrderBook<M>, levels: usize) -> DepthSnapshot {
    DepthSnapshot {
        bid_depth: book.bid_depth(levels),
        ask_depth: book.ask_depth(levels),
        bid_levels: book.bid_count().min(levels),
        ask_levels: book.ask_count().min(levels),
    }
}
