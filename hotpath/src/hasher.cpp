// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

#include "flowlab/hasher.h"

// Use the official xxHash single-header implementation, inlined into this
// translation unit. We keep XXH_INLINE_ALL so no extra link step is needed
// and only this .cpp pays the compilation cost.
#define XXH_INLINE_ALL
#include "xxhash.h"

#include <cstring>

namespace flowlab {

// Mirror of Rust `flowlab-verify::StateHasher::update`:
//
//     state = xxh3_64( state.to_le_bytes() || data )
//
// Stack-buffered for the common small-input path, falling back to streaming
// only for inputs larger than 4 KiB. No allocations, no exceptions.
void StateHasher::update(const uint8_t* data, size_t len) noexcept {
    constexpr size_t kStackBuf = 4096;
    uint8_t stack[kStackBuf];
    uint8_t prev_le[8];

    // Little-endian serialize prev state (matches Rust to_le_bytes()).
    for (int i = 0; i < 8; ++i) {
        prev_le[i] = static_cast<uint8_t>((state_ >> (i * 8)) & 0xFF);
    }

    const size_t total = 8 + len;

    if (total <= kStackBuf) {
        std::memcpy(stack, prev_le, 8);
        if (len) std::memcpy(stack + 8, data, len);
        state_ = XXH3_64bits(stack, total);
        return;
    }

    // Streaming path: avoids any heap allocation regardless of input size.
    XXH3_state_t st;
    XXH3_64bits_reset(&st);
    XXH3_64bits_update(&st, prev_le, 8);
    XXH3_64bits_update(&st, data, len);
    state_ = XXH3_64bits_digest(&st);
}

uint64_t xxh3_hash(const uint8_t* data, size_t len) noexcept {
    return XXH3_64bits(data, len);
}

} // namespace flowlab
