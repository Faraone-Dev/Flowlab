#pragma once

#include <cstdint>
#include <array>
#include <algorithm>
#include <unordered_map>

namespace flowlab {

/// Fixed-layout event - must match Rust `#[repr(C)] Event` exactly (40 bytes).
struct Event {
    uint64_t ts;
    uint64_t price;
    uint64_t qty;
    uint64_t order_id;
    uint32_t instrument_id;
    uint8_t  event_type;
    uint8_t  side;
    uint8_t  _pad[2];
};

static_assert(sizeof(Event) == 40, "Event must be 40 bytes");
static_assert(alignof(Event) == 8, "Event must be 8-byte aligned");

struct Level {
    uint64_t price;
    uint64_t total_qty;
    uint32_t order_count;
};

/// Per-order metadata keyed by order_id. Mirrors Rust `OrderMeta` so
/// ITCH 'D' (price=0) and 'X' (partial cancel) semantics match exactly.
struct OrderMeta {
    uint64_t price;
    uint64_t qty;
    uint8_t  side;
};

template<size_t MaxLevels = 256>
class OrderBook {
public:
    OrderBook() : bid_count_(0), ask_count_(0) {}

    void apply(const Event& event) noexcept;

    uint64_t best_bid() const noexcept { return bid_count_ > 0 ? bids_[0].price : 0; }
    uint64_t best_ask() const noexcept { return ask_count_ > 0 ? asks_[0].price : 0; }

    uint64_t spread() const noexcept {
        auto bb = best_bid();
        auto ba = best_ask();
        return (bb > 0 && ba > 0 && ba > bb) ? (ba - bb) : 0;
    }

    uint64_t bid_depth() const noexcept {
        uint64_t d = 0;
        for (size_t i = 0; i < bid_count_; ++i) d += bids_[i].total_qty;
        return d;
    }

    uint64_t ask_depth() const noexcept {
        uint64_t d = 0;
        for (size_t i = 0; i < ask_count_; ++i) d += asks_[i].total_qty;
        return d;
    }

    size_t bid_levels() const noexcept { return bid_count_; }
    size_t ask_levels() const noexcept { return ask_count_; }

    const Level* bids() const noexcept { return bids_.data(); }
    const Level* asks() const noexcept { return asks_.data(); }

private:
    void add_order(const Event& e) noexcept;
    void cancel_order(const Event& e) noexcept;
    void trade(const Event& e) noexcept;

    std::array<Level, MaxLevels> bids_;
    std::array<Level, MaxLevels> asks_;
    size_t bid_count_;
    size_t ask_count_;
    std::unordered_map<uint64_t, OrderMeta> orders_;
};

} // namespace flowlab