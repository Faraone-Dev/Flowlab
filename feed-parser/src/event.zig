// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

/// Canonical event format — must match Rust `#[repr(C)] Event` exactly.
/// 40 bytes, 8-byte aligned, little-endian, zero-padding.
///
/// Field order is chosen so natural C alignment lands on exactly 40 bytes
/// (all u64 fields first, then u32, then u8 — no internal padding).
pub const Event = extern struct {
    ts: u64,
    price: u64,
    qty: u64,
    order_id: u64,
    instrument_id: u32,
    event_type: u8,
    side: u8,
    _pad: [2]u8 = .{ 0, 0 },
};

comptime {
    if (@sizeOf(Event) != 40) @compileError("Event must be exactly 40 bytes");
    if (@alignOf(Event) != 8) @compileError("Event must be 8-byte aligned");
}

pub const EventType = enum(u8) {
    order_add = 0x01,
    order_cancel = 0x02,
    order_modify = 0x03,
    trade = 0x04,
    book_snapshot = 0x05,
};

pub const Side = enum(u8) {
    bid = 0,
    ask = 1,
};
