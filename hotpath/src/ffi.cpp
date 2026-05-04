// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

#include "flowlab/ffi.h"
#include "flowlab/orderbook.h"
#include "flowlab/hasher.h"

// Pull in xxHash directly so we can call XXH3_64bits / _withSeed from
// flowlab_orderbook_hash. XXH_INLINE_ALL must be defined exactly once
// per translation unit before the header is seen — define it locally
// (hasher.cpp does the same in its own TU).
#define XXH_INLINE_ALL
#include "xxhash.h"

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
    // CANONICAL L2 HASH — must agree byte-for-byte with the Rust
    // implementation in `flowlab-core::hot_book::HotOrderBook::canonical_l2_hash`.
    //
    // Scheme:
    //   * domain tag         : XXH3_64bits("FLOWLAB-L2-v1")  -> seed
    //   * per-level payload  : 16 raw bytes = (price_le, total_qty_le)
    //                          NOT the full Level struct (the Rust side
    //                          excludes order_count + tail padding).
    //   * fold               : h ^= XXH3_64bits_withSeed(buf16, 16, h)
    //   * side separator     : h = XXH3_64bits_withSeed("|", 1, h)
    //
    // Iterate bids (descending) then asks (ascending) — matches the
    // canonical traversal order in HotOrderBook.
    auto* b = static_cast<const Book*>(book);

    static constexpr char kTag[] = "FLOWLAB-L2-v1";
    uint64_t h = XXH3_64bits(kTag, sizeof(kTag) - 1);

    uint8_t buf[16];

    const flowlab::Level* bids = b->bids();
    for (size_t i = 0; i < b->bid_levels(); ++i) {
        std::memcpy(buf,     &bids[i].price,     8);
        std::memcpy(buf + 8, &bids[i].total_qty, 8);
        h ^= XXH3_64bits_withSeed(buf, 16, h);
    }

    static constexpr char kSep = '|';
    h = XXH3_64bits_withSeed(&kSep, 1, h);

    const flowlab::Level* asks = b->asks();
    for (size_t i = 0; i < b->ask_levels(); ++i) {
        std::memcpy(buf,     &asks[i].price,     8);
        std::memcpy(buf + 8, &asks[i].total_qty, 8);
        h ^= XXH3_64bits_withSeed(buf, 16, h);
    }

    return h;
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
