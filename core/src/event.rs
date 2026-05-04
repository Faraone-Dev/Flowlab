// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

use bytemuck::{Pod, Zeroable};

/// Fixed-layout canonical event format — cross-language ABI.
///
/// **40 bytes, naturally 8-byte aligned, little-endian.**
///
/// Field order is chosen so natural C alignment lands on exactly 40 bytes
/// with zero internal padding, so the same struct layout is reproduced
/// verbatim by:
///   - Zig  `extern struct` in `feed-parser/src/event.zig`
///   - C++  `struct`         in `hotpath/include/flowlab/orderbook.h`
/// No `packed` attribute is needed anywhere — every u64 load is aligned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct Event {
    /// Nanosecond timestamp (informational only — never used for ordering)
    pub ts: u64,
    /// Price in fixed-point (raw ticks)
    pub price: u64,
    /// Quantity in fixed-point units
    pub qty: u64,
    /// Exchange order ID
    pub order_id: u64,
    /// Instrument identifier
    pub instrument_id: u32,
    /// Event type (see `EventType`)
    pub event_type: u8,
    /// Side: 0 = bid, 1 = ask
    pub side: u8,
    /// Padding to 40 bytes
    pub _pad: [u8; 2],
}

const _: () = assert!(size_of::<Event>() == 40);
const _: () = assert!(align_of::<Event>() == 8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum EventType {
    OrderAdd = 0x01,
    OrderCancel = 0x02,
    OrderModify = 0x03,
    Trade = 0x04,
    BookSnapshot = 0x05,
}

impl EventType {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0x01 => Some(Self::OrderAdd),
            0x02 => Some(Self::OrderCancel),
            0x03 => Some(Self::OrderModify),
            0x04 => Some(Self::Trade),
            0x05 => Some(Self::BookSnapshot),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Side {
    Bid = 0,
    Ask = 1,
}

impl Side {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Bid),
            1 => Some(Self::Ask),
            _ => None,
        }
    }
}

/// Sequenced event wrapper — execution order is strictly by sequence ID.
#[derive(Debug, Clone, Copy)]
pub struct SequencedEvent {
    /// Global monotonic sequence ID (source of ordering truth)
    pub seq: u64,
    /// Channel ID for multi-feed scenarios
    pub channel_id: u32,
    /// The raw event
    pub event: Event,
}
