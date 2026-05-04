// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

#include "flowlab/orderbook.h"

#include <algorithm>
#include <cstring>

namespace flowlab {

// ─── Internal helpers ─────────────────────────────────────────────────
//
// Mirrors flowlab-core::hot_book::HotOrderBook private helpers
// (`find_level`, `remove_level`) and uses the same primitives:
//   * `std::partition_point`  -> branchless binary search on monotonic
//                                predicate (bids descending, asks asc).
//   * `std::memmove`          -> O(k) shift to open / close a slot,
//                                instead of O(k) bubble swaps.
//
// The C++ template was previously linear-scan + bubble-sort. The
// canonical hash and ITCH semantics are byte-for-byte identical; only
// the algorithmic complexity per insertion / removal changes
// (O(N) compares + O(N) swaps  ->  O(log N) compares + O(N) memmove).

namespace {

// Binary-search the insertion point for `price` in a sorted half-book.
// Bids:  descending  -> partition predicate is `l.price > price`.
// Asks:  ascending   -> partition predicate is `l.price < price`.
template <bool IsBid>
inline size_t partition_point(const Level* levels, size_t cnt, uint64_t price) noexcept {
    size_t lo = 0;
    size_t hi = cnt;
    while (lo < hi) {
        size_t mid = lo + ((hi - lo) >> 1);
        bool ordered = IsBid ? (levels[mid].price > price)
                             : (levels[mid].price < price);
        if (ordered) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    return lo;
}

// Locate an existing level by price — `partition_point` + equality
// check on the boundary slot. Returns SIZE_MAX when absent.
template <bool IsBid>
inline size_t find_level(const Level* levels, size_t cnt, uint64_t price) noexcept {
    size_t pos = partition_point<IsBid>(levels, cnt, price);
    if (pos < cnt && levels[pos].price == price) {
        return pos;
    }
    return static_cast<size_t>(-1);
}

// Drop level at `i`, shifting the tail left with a single memmove.
inline void remove_level(Level* levels, size_t& count, size_t i) noexcept {
    size_t cnt = count;
    if (i + 1 < cnt) {
        std::memmove(&levels[i], &levels[i + 1], (cnt - i - 1) * sizeof(Level));
    }
    levels[cnt - 1] = Level{0, 0, 0};
    count = cnt - 1;
}

} // namespace

template<size_t MaxLevels>
void OrderBook<MaxLevels>::apply(const Event& event) noexcept {
    switch (event.event_type) {
        case 0x01: add_order(event); break;    // OrderAdd
        case 0x02: cancel_order(event); break; // OrderCancel
        case 0x03:                             // OrderModify
            cancel_order(event);
            add_order(event);
            break;
        case 0x04: trade(event); break;        // Trade
        default: break;
    }
}

template<size_t MaxLevels>
void OrderBook<MaxLevels>::add_order(const Event& e) noexcept {
    const bool is_bid = (e.side == 0);
    Level* levels = is_bid ? bids_.data() : asks_.data();
    size_t& count = is_bid ? bid_count_ : ask_count_;
    const size_t cnt = count;

    // Branchless binary-search insertion point. `partition_point` is
    // dispatched at compile-time on the side template parameter so the
    // hot path has no `is_bid` branch inside the loop.
    const size_t pos = is_bid
        ? partition_point<true>(levels, cnt, e.price)
        : partition_point<false>(levels, cnt, e.price);

    bool placed = false;

    if (pos < cnt && levels[pos].price == e.price) {
        // Existing level — straight-line update.
        levels[pos].total_qty  += e.qty;
        levels[pos].order_count++;
        placed = true;
    } else if (cnt < MaxLevels) {
        // New level — open a slot at `pos` with one memmove instead of
        // O(N) bubble swaps. memmove on trivially-copyable Level is
        // safe even for overlapping ranges.
        if (pos < cnt) {
            std::memmove(&levels[pos + 1], &levels[pos], (cnt - pos) * sizeof(Level));
        }
        levels[pos] = Level{e.price, e.qty, 1};
        count = cnt + 1;
        placed = true;
    }
    // else: capacity exhausted — drop the order silently (Rust does the same).

    // Track order metadata for cancel/trade resolution by order_id.
    // ITCH 'A'/'F' always carry order_id != 0; defend against synthetic
    // events with order_id == 0 by skipping the lookup map.
    if (placed && e.order_id != 0) {
        orders_[e.order_id] = OrderMeta{e.price, e.qty, e.side};
    }
}

template<size_t MaxLevels>
void OrderBook<MaxLevels>::cancel_order(const Event& e) noexcept {
    // Mirror Rust HotOrderBook::cancel_order:
    //   * order_id != 0 + present  -> use stored (price, side); ITCH 'X'
    //                                  partial cancel keeps the slot live
    //                                  when 0 < e.qty < meta.qty.
    //   * order_id != 0 + absent   -> drop (return).
    //   * order_id == 0            -> fall back to event (price, side, qty),
    //                                  treat as a single-order full delete.
    uint64_t price;
    uint64_t deduct_qty;
    uint8_t  side;
    bool     release_slot;

    if (e.order_id != 0) {
        auto it = orders_.find(e.order_id);
        if (it == orders_.end()) {
            return;
        }
        OrderMeta& meta = it->second;
        price = meta.price;
        side  = meta.side;
        if (e.qty == 0 || e.qty >= meta.qty) {
            deduct_qty   = meta.qty;
            release_slot = true;
            orders_.erase(it);
        } else {
            meta.qty   -= e.qty;
            deduct_qty  = e.qty;
            release_slot = false;
        }
    } else {
        price        = e.price;
        deduct_qty   = e.qty;
        side         = e.side;
        release_slot = true;
    }

    const bool is_bid = (side == 0);
    Level* levels = is_bid ? bids_.data() : asks_.data();
    size_t& count = is_bid ? bid_count_ : ask_count_;

    const size_t i = is_bid
        ? find_level<true>(levels, count, price)
        : find_level<false>(levels, count, price);
    if (i == static_cast<size_t>(-1)) {
        return;
    }

    uint64_t cur = levels[i].total_qty;
    levels[i].total_qty = (cur > deduct_qty) ? (cur - deduct_qty) : 0;
    if (release_slot) {
        if (levels[i].order_count > 0) levels[i].order_count--;
        if (levels[i].order_count == 0) {
            remove_level(levels, count, i);
        }
    }
}

template<size_t MaxLevels>
void OrderBook<MaxLevels>::trade(const Event& e) noexcept {
    // Mirror Rust HotOrderBook::trade with the same hot/cold split:
    //   * partial fill (`new_qty != 0`) is straight-line, single update.
    //   * full fill and anonymous trade are routed to out-of-line cold
    //     helpers so the i-cache footprint of the hot path stays small.
    //
    // Anonymous (synthetic) path: no order_id, fall back to a price-keyed
    // deduction with no order_count delta unless the level empties.
    if (e.order_id == 0) {
        const bool is_bid = (e.side == 0);
        Level* levels = is_bid ? bids_.data() : asks_.data();
        size_t& count = is_bid ? bid_count_ : ask_count_;
        const size_t i = is_bid
            ? find_level<true>(levels, count, e.price)
            : find_level<false>(levels, count, e.price);
        if (i == static_cast<size_t>(-1)) {
            return;
        }
        uint64_t cur = levels[i].total_qty;
        levels[i].total_qty = (cur > e.qty) ? (cur - e.qty) : 0;
        if (levels[i].total_qty == 0) {
            if (levels[i].order_count > 0) levels[i].order_count--;
            if (levels[i].order_count == 0) {
                remove_level(levels, count, i);
            }
        }
        return;
    }

    auto it = orders_.find(e.order_id);
    if (it == orders_.end()) {
        return;
    }
    OrderMeta& meta = it->second;
    const uint64_t price     = meta.price;
    const uint8_t  side      = meta.side;
    const uint64_t fill_qty  = (e.qty >= meta.qty) ? meta.qty : e.qty;
    const bool     full_fill = (fill_qty == meta.qty);
    if (full_fill) {
        orders_.erase(it);
    } else {
        meta.qty -= fill_qty;
    }

    const bool is_bid = (side == 0);
    Level* levels = is_bid ? bids_.data() : asks_.data();
    size_t& count = is_bid ? bid_count_ : ask_count_;

    const size_t i = is_bid
        ? find_level<true>(levels, count, price)
        : find_level<false>(levels, count, price);
    if (i == static_cast<size_t>(-1)) {
        return;
    }

    uint64_t cur = levels[i].total_qty;
    levels[i].total_qty = (cur > fill_qty) ? (cur - fill_qty) : 0;
    if (full_fill) {
        if (levels[i].order_count > 0) levels[i].order_count--;
        if (levels[i].order_count == 0) {
            remove_level(levels, count, i);
        }
    }
}

// Explicit instantiation
template class OrderBook<256>;
template class OrderBook<1024>;

} // namespace flowlab

