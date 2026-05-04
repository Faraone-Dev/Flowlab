// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

pub mod circuit_breaker;
pub mod imbalance;
pub mod spread;
pub mod vpin;
pub mod regime;

pub use circuit_breaker::{
    BreakerConfig, BreakerSnapshot, CircuitBreaker, Decision, Fill, HaltReason, Intent, Side,
};
pub use regime::{Regime, RegimeClassifier, RegimeInput};
pub use spread::{SpreadMetrics, SpreadTracker};
