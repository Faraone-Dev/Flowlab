// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

#pragma once

#include <cstdint>

/// C ABI exports for Rust FFI — no C++ types exposed.
extern "C" {

/// Create a new order book. Returns opaque handle.
void* flowlab_orderbook_new();

/// Destroy an order book.
void flowlab_orderbook_free(void* book);

/// Apply a raw 40-byte event to the book.
void flowlab_orderbook_apply(void* book, const uint8_t* event_data);

/// Get best bid price.
uint64_t flowlab_orderbook_best_bid(const void* book);

/// Get best ask price.
uint64_t flowlab_orderbook_best_ask(const void* book);

/// Get spread.
uint64_t flowlab_orderbook_spread(const void* book);

/// Compute state hash of current book (for cross-language verification).
uint64_t flowlab_orderbook_hash(const void* book);

/// Create a new state hasher.
void* flowlab_hasher_new();

/// Free a state hasher.
void flowlab_hasher_free(void* hasher);

/// Update hasher with data.
void flowlab_hasher_update(void* hasher, const uint8_t* data, uint64_t len);

/// Get current hash digest.
uint64_t flowlab_hasher_digest(const void* hasher);

} // extern "C"
