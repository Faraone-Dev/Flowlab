#include "flowlab/hasher.h"
#include <cstring>

// Minimal xxh3-64 implementation for cross-language hash verification.
// Production: link xxHash library. This is a simplified version for bootstrapping.

namespace flowlab {

static uint64_t xxh3_simple(const uint8_t* data, size_t len) noexcept {
    // FNV-1a as placeholder until xxHash is linked
    // TODO: replace with real xxh3_64 for cross-language verification
    uint64_t hash = 14695981039346656037ULL;
    for (size_t i = 0; i < len; ++i) {
        hash ^= static_cast<uint64_t>(data[i]);
        hash *= 1099511628211ULL;
    }
    return hash;
}

void StateHasher::update(const uint8_t* data, size_t len) noexcept {
    uint8_t buf[8];
    std::memcpy(buf, &state_, 8);

    // H(prev_hash || data)
    size_t total = 8 + len;
    auto* combined = new uint8_t[total]; // TODO: stack alloc for small sizes
    std::memcpy(combined, buf, 8);
    std::memcpy(combined + 8, data, len);
    state_ = xxh3_simple(combined, total);
    delete[] combined;
}

uint64_t xxh3_hash(const uint8_t* data, size_t len) noexcept {
    return xxh3_simple(data, len);
}

} // namespace flowlab
