// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

//! Rolling statistics translation unit.
//!
//! `RollingStats` is intentionally header-only — Welford's incremental
//! algorithm is a tight scalar loop the compiler inlines and vectorises
//! at the call site, so a separate .cpp would only inhibit inlining.
//!
//! A batched AVX2 kernel was prototyped but rejected: at the engine's
//! tick cadence (~50 Hz) batch size is 1, so the SIMD prologue
//! dominates over the scalar update. We measured no win on the hot
//! path. If a sub-millisecond burst statistic is ever needed, drop the
//! kernel into a new TU rather than this one.

#include "flowlab/stats.h"

namespace flowlab {
// Intentionally empty — keeps the symbol visible to the linker for
// future ABI-stable batch entry points without forcing recompilation.
} // namespace flowlab
