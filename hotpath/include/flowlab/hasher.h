// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

#pragma once

#include <cstdint>
#include <cstddef>

namespace flowlab {

/// xxHash3-compatible incremental state hasher.
/// Must produce identical results to Rust StateHasher.
class StateHasher {
public:
    StateHasher() : state_(0) {}

    void update(const uint8_t* data, size_t len) noexcept;
    uint64_t digest() const noexcept { return state_; }
    void reset() noexcept { state_ = 0; }

private:
    uint64_t state_;
};

/// Standalone xxh3_64 hash.
uint64_t xxh3_hash(const uint8_t* data, size_t len) noexcept;

} // namespace flowlab
