#include "flowlab/orderbook.h"

namespace flowlab {

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
    auto& levels = (e.side == 0) ? bids_ : asks_;
    auto& count = (e.side == 0) ? bid_count_ : ask_count_;

    // Find existing level or insertion point
    for (size_t i = 0; i < count; ++i) {
        if (levels[i].price == e.price) {
            levels[i].total_qty += e.qty;
            levels[i].order_count++;
            return;
        }
    }

    // New level
    if (count < MaxLevels) {
        levels[count] = Level{e.price, e.qty, 1};
        count++;

        // Sort: bids descending, asks ascending
        if (e.side == 0) {
            // Descending
            for (size_t i = count - 1; i > 0; --i) {
                if (levels[i].price > levels[i - 1].price) {
                    std::swap(levels[i], levels[i - 1]);
                } else {
                    break;
                }
            }
        } else {
            // Ascending
            for (size_t i = count - 1; i > 0; --i) {
                if (levels[i].price < levels[i - 1].price) {
                    std::swap(levels[i], levels[i - 1]);
                } else {
                    break;
                }
            }
        }
    }
}

template<size_t MaxLevels>
void OrderBook<MaxLevels>::cancel_order(const Event& e) noexcept {
    auto& levels = (e.side == 0) ? bids_ : asks_;
    auto& count = (e.side == 0) ? bid_count_ : ask_count_;

    for (size_t i = 0; i < count; ++i) {
        if (levels[i].price == e.price) {
            if (e.qty >= levels[i].total_qty || levels[i].order_count <= 1) {
                // Remove level
                for (size_t j = i; j + 1 < count; ++j) {
                    levels[j] = levels[j + 1];
                }
                count--;
            } else {
                levels[i].total_qty -= e.qty;
                levels[i].order_count--;
            }
            return;
        }
    }
}

template<size_t MaxLevels>
void OrderBook<MaxLevels>::trade(const Event& e) noexcept {
    cancel_order(e);
}

// Explicit instantiation
template class OrderBook<256>;
template class OrderBook<1024>;

} // namespace flowlab
