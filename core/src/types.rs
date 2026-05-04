// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

/// Price in raw ticks (integer representation, no floating point)
pub type Price = u64;
/// Quantity in fixed-point units
pub type Qty = u64;
/// Order identifier from exchange
pub type OrderId = u64;
/// Instrument identifier
pub type InstrumentId = u32;
/// Global monotonic sequence number
pub type SeqNum = u64;
/// Channel identifier for multi-feed
pub type ChannelId = u32;
