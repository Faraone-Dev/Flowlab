#include "flowlab/stats.h"

// RollingStats is header-only (Welford's algorithm).
// This file exists for future SIMD-optimized batch statistics.

namespace flowlab {

// TODO: SIMD batch update for rolling volatility
// void batch_update_avx2(RollingStats& stats, const double* values, size_t count);

} // namespace flowlab
