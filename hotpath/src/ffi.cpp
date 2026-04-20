#include "flowlab/ffi.h"
#include "flowlab/orderbook.h"
#include "flowlab/hasher.h"

#include <cstring>

using Book = flowlab::OrderBook<256>;

extern "C" {

void* flowlab_orderbook_new() {
    return new Book();
}

void flowlab_orderbook_free(void* book) {
    delete static_cast<Book*>(book);
}

void flowlab_orderbook_apply(void* book, const uint8_t* event_data) {
    flowlab::Event event;
    std::memcpy(&event, event_data, sizeof(flowlab::Event));
    static_cast<Book*>(book)->apply(event);
}

uint64_t flowlab_orderbook_best_bid(const void* book) {
    return static_cast<const Book*>(book)->best_bid();
}

uint64_t flowlab_orderbook_best_ask(const void* book) {
    return static_cast<const Book*>(book)->best_ask();
}

uint64_t flowlab_orderbook_spread(const void* book) {
    return static_cast<const Book*>(book)->spread();
}

uint64_t flowlab_orderbook_hash(const void* book) {
    // Hash all bid/ask levels
    flowlab::StateHasher hasher;
    auto* b = static_cast<const Book*>(book);

    for (size_t i = 0; i < b->bid_levels(); ++i) {
        hasher.update(reinterpret_cast<const uint8_t*>(&b->bids()[i]), sizeof(flowlab::Level));
    }
    for (size_t i = 0; i < b->ask_levels(); ++i) {
        hasher.update(reinterpret_cast<const uint8_t*>(&b->asks()[i]), sizeof(flowlab::Level));
    }

    return hasher.digest();
}

void* flowlab_hasher_new() {
    return new flowlab::StateHasher();
}

void flowlab_hasher_free(void* hasher) {
    delete static_cast<flowlab::StateHasher*>(hasher);
}

void flowlab_hasher_update(void* hasher, const uint8_t* data, uint64_t len) {
    static_cast<flowlab::StateHasher*>(hasher)->update(data, static_cast<size_t>(len));
}

uint64_t flowlab_hasher_digest(const void* hasher) {
    return static_cast<const flowlab::StateHasher*>(hasher)->digest();
}

} // extern "C"
