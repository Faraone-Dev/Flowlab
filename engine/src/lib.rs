// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

//! flowlab-engine — runtime that drives a Source through the analytics
//! pipeline and publishes telemetry over TCP.
//!
//! See `wire.rs` for the protocol contract and `engine.rs` for the pipeline.

pub mod backpressure;
pub mod engine;
pub mod ich;
pub mod server;
pub mod source;
pub mod synthetic;
pub mod tsc;
pub mod wire;
