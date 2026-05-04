// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

const std = @import("std");
const event = @import("event.zig");

// ─── ITCH 5.0 — wire format ─────────────────────────────────────────
//
// ITCH messages are framed by:
//   [length: u16 BE][type: u8][payload...]
// We accept either framed or unframed buffers via parse_buffer_framed /
// parse_buffer. The framed form is what NASDAQ transmits over
// SoupBinTCP / MoldUDP after the transport header is stripped.
//
// Reference: NASDAQ TotalView-ITCH 5.0 spec, section 4.

pub const MsgType = enum(u8) {
    system_event = 'S',
    add_order = 'A',
    add_order_mpid = 'F',
    order_executed = 'E',
    order_executed_price = 'C',
    order_cancel = 'X',
    order_delete = 'D',
    order_replace = 'U',
    trade_non_cross = 'P',
    cross_trade = 'Q',
    broken_trade = 'B',
    _,
};

/// Per-spec ITCH 5.0 message lengths (including the 1-byte type tag).
pub fn msg_length(t: u8) u16 {
    return switch (t) {
        'S' => 12,
        'R' => 39,
        'H' => 25,
        'Y' => 20,
        'L' => 26,
        'V' => 35,
        'W' => 12,
        'K' => 28,
        'J' => 35,
        'h' => 21,
        'A' => 36,
        'F' => 40,
        'E' => 31,
        'C' => 36,
        'X' => 23,
        'D' => 19,
        'U' => 35,
        'P' => 44,
        'Q' => 40,
        'B' => 19,
        'I' => 50,
        'N' => 20,
        'O' => 48,
        else => 0,
    };
}

// ─── Big-endian readers (ITCH wire format is BE) ─────────────────────

inline fn rd_u16(p: [*]const u8) u16 {
    return std.mem.readInt(u16, p[0..2], .big);
}
inline fn rd_u32(p: [*]const u8) u32 {
    return std.mem.readInt(u32, p[0..4], .big);
}
inline fn rd_u64(p: [*]const u8) u64 {
    return std.mem.readInt(u64, p[0..8], .big);
}

/// Combine 48-bit ITCH timestamp (u16 hi + u32 lo) into a u64 ns.
inline fn rd_ts48(p: [*]const u8) u64 {
    const hi: u64 = rd_u16(p);
    const lo: u64 = rd_u32(p + 2);
    return (hi << 32) | lo;
}

// ─── Message parsers (per-type field offsets after the 1-byte tag) ──

fn parse_add_order(p: [*]const u8) event.Event {
    const stock_locate = rd_u16(p + 1);
    const ts = rd_ts48(p + 5);
    const order_ref = rd_u64(p + 11);
    const side_raw = p[19];
    const shares = rd_u32(p + 20);
    const price = rd_u32(p + 32);
    return .{
        .ts = ts,
        .instrument_id = stock_locate,
        .event_type = @intFromEnum(event.EventType.order_add),
        .side = if (side_raw == 'B') @intFromEnum(event.Side.bid) else @intFromEnum(event.Side.ask),
        .price = price,
        .qty = shares,
        .order_id = order_ref,
    };
}

/// Order Delete (type 'D') — full cancel; qty=0 because the message
/// carries no shares field. Side resolution is the consumer's job.
fn parse_order_delete(p: [*]const u8) event.Event {
    const stock_locate = rd_u16(p + 1);
    const ts = rd_ts48(p + 5);
    const order_ref = rd_u64(p + 11);
    return .{
        .ts = ts,
        .instrument_id = stock_locate,
        .event_type = @intFromEnum(event.EventType.order_cancel),
        .side = 0,
        .price = 0,
        .qty = 0,
        .order_id = order_ref,
    };
}

/// Order Cancel (type 'X') — partial cancel of `cancelled_shares`.
fn parse_order_cancel(p: [*]const u8) event.Event {
    const stock_locate = rd_u16(p + 1);
    const ts = rd_ts48(p + 5);
    const order_ref = rd_u64(p + 11);
    const cancelled_shares = rd_u32(p + 19);
    return .{
        .ts = ts,
        .instrument_id = stock_locate,
        .event_type = @intFromEnum(event.EventType.order_cancel),
        .side = 0,
        .price = 0,
        .qty = cancelled_shares,
        .order_id = order_ref,
    };
}

/// Order Executed (type 'E') — full or partial fill of an existing order.
fn parse_order_executed(p: [*]const u8) event.Event {
    const stock_locate = rd_u16(p + 1);
    const ts = rd_ts48(p + 5);
    const order_ref = rd_u64(p + 11);
    const executed_shares = rd_u32(p + 19);
    return .{
        .ts = ts,
        .instrument_id = stock_locate,
        .event_type = @intFromEnum(event.EventType.trade),
        .side = 0,
        .price = 0,
        .qty = executed_shares,
        .order_id = order_ref,
    };
}

fn parse_trade(p: [*]const u8) event.Event {
    const stock_locate = rd_u16(p + 1);
    const ts = rd_ts48(p + 5);
    const order_ref = rd_u64(p + 11);
    const side_raw = p[19];
    const shares = rd_u32(p + 20);
    const price = rd_u32(p + 32);
    return .{
        .ts = ts,
        .instrument_id = stock_locate,
        .event_type = @intFromEnum(event.EventType.trade),
        .side = if (side_raw == 'B') @intFromEnum(event.Side.bid) else @intFromEnum(event.Side.ask),
        .price = price,
        .qty = shares,
        .order_id = order_ref,
    };
}

fn dispatch(p: [*]const u8) ?event.Event {
    return switch (p[0]) {
        'A', 'F' => parse_add_order(p),
        'D' => parse_order_delete(p),
        'X' => parse_order_cancel(p),
        'E', 'C' => parse_order_executed(p),
        'P' => parse_trade(p),
        else => null,
    };
}

/// Parse an UNFRAMED ITCH stream — back-to-back messages with no length
/// prefix. Uses the spec lookup table to advance `offset` correctly.
pub fn parse_buffer(
    raw: [*]const u8,
    raw_len: usize,
    out: [*]event.Event,
    out_cap: usize,
) i64 {
    var offset: usize = 0;
    var count: usize = 0;

    while (offset < raw_len and count < out_cap) {
        const t = raw[offset];
        const len = msg_length(t);
        if (len == 0) return -1; // unknown — cannot safely advance
        if (offset + len > raw_len) break; // truncated tail

        if (dispatch(raw + offset)) |ev| {
            out[count] = ev;
            count += 1;
        }
        offset += len;
    }

    return @intCast(count);
}

/// Parse a FRAMED ITCH stream — each message preceded by a u16 BE length.
pub fn parse_buffer_framed(
    raw: [*]const u8,
    raw_len: usize,
    out: [*]event.Event,
    out_cap: usize,
) i64 {
    var offset: usize = 0;
    var count: usize = 0;

    while (offset + 2 <= raw_len and count < out_cap) {
        const frame_len = rd_u16(raw + offset);
        offset += 2;
        if (frame_len == 0 or offset + frame_len > raw_len) break;

        if (dispatch(raw + offset)) |ev| {
            out[count] = ev;
            count += 1;
        }
        offset += frame_len;
    }

    return @intCast(count);
}

// ─── Tests ───────────────────────────────────────────────────────────

const testing = std.testing;

test "msg_length covers all major types" {
    try testing.expectEqual(@as(u16, 36), msg_length('A'));
    try testing.expectEqual(@as(u16, 40), msg_length('F'));
    try testing.expectEqual(@as(u16, 19), msg_length('D'));
    try testing.expectEqual(@as(u16, 23), msg_length('X'));
    try testing.expectEqual(@as(u16, 31), msg_length('E'));
    try testing.expectEqual(@as(u16, 44), msg_length('P'));
    try testing.expectEqual(@as(u16, 0), msg_length(0xff));
}

fn write_be_u16(buf: []u8, v: u16) void {
    std.mem.writeInt(u16, buf[0..2], v, .big);
}
fn write_be_u32(buf: []u8, v: u32) void {
    std.mem.writeInt(u32, buf[0..4], v, .big);
}
fn write_be_u48(buf: []u8, hi: u16, lo: u32) void {
    write_be_u16(buf, hi);
    write_be_u32(buf[2..], lo);
}
fn write_be_u64(buf: []u8, v: u64) void {
    std.mem.writeInt(u64, buf[0..8], v, .big);
}

fn build_add_order(buf: []u8, locate: u16, ts_hi: u16, ts_lo: u32, oref: u64, side: u8, shares: u32, price: u32) void {
    @memset(buf, 0);
    buf[0] = 'A';
    write_be_u16(buf[1..], locate);
    write_be_u16(buf[3..], 0);
    write_be_u48(buf[5..], ts_hi, ts_lo);
    write_be_u64(buf[11..], oref);
    buf[19] = side;
    write_be_u32(buf[20..], shares);
    write_be_u32(buf[32..], price);
}

test "parse single AddOrder big-endian" {
    var raw: [36]u8 = undefined;
    build_add_order(&raw, 7, 0x0001, 0xDEADBEEF, 0xCAFEBABE_DEADBEEF, 'B', 12345, 99_500_000);

    var out: [4]event.Event = undefined;
    const n = parse_buffer(&raw, raw.len, &out, out.len);
    try testing.expectEqual(@as(i64, 1), n);

    const ev = out[0];
    try testing.expectEqual(@as(u32, 7), ev.instrument_id);
    try testing.expectEqual(@as(u8, @intFromEnum(event.EventType.order_add)), ev.event_type);
    try testing.expectEqual(@as(u8, @intFromEnum(event.Side.bid)), ev.side);
    try testing.expectEqual(@as(u64, 12345), ev.qty);
    try testing.expectEqual(@as(u64, 99_500_000), ev.price);
    try testing.expectEqual(@as(u64, 0xCAFEBABE_DEADBEEF), ev.order_id);
    try testing.expectEqual(@as(u64, (@as(u64, 0x0001) << 32) | 0xDEADBEEF), ev.ts);
}

test "parse mixed stream advances by per-spec lengths" {
    // 'A' (36) + 'D' (19) + 'X' (23) = 78 bytes
    var raw: [78]u8 = undefined;
    @memset(&raw, 0);

    build_add_order(raw[0..36], 1, 0, 100, 42, 'S', 200, 50_000_000);

    raw[36] = 'D';
    write_be_u16(raw[37..], 1);
    write_be_u16(raw[39..], 0);
    write_be_u48(raw[41..], 0, 200);
    write_be_u64(raw[47..], 42);

    raw[55] = 'X';
    write_be_u16(raw[56..], 1);
    write_be_u16(raw[58..], 0);
    write_be_u48(raw[60..], 0, 300);
    write_be_u64(raw[66..], 99);
    write_be_u32(raw[74..], 7);

    var out: [8]event.Event = undefined;
    const n = parse_buffer(&raw, raw.len, &out, out.len);
    try testing.expectEqual(@as(i64, 3), n);

    const e0 = out[0];
    const e1 = out[1];
    const e2 = out[2];
    try testing.expectEqual(@as(u8, @intFromEnum(event.EventType.order_add)), e0.event_type);
    try testing.expectEqual(@as(u8, @intFromEnum(event.Side.ask)), e0.side);
    try testing.expectEqual(@as(u8, @intFromEnum(event.EventType.order_cancel)), e1.event_type);
    try testing.expectEqual(@as(u64, 42), e1.order_id);
    try testing.expectEqual(@as(u8, @intFromEnum(event.EventType.order_cancel)), e2.event_type);
    try testing.expectEqual(@as(u64, 7), e2.qty);
    try testing.expectEqual(@as(u64, 99), e2.order_id);
}

test "framed parser respects u16 BE length prefix" {
    var raw: [38]u8 = undefined;
    write_be_u16(raw[0..2], 36);
    build_add_order(raw[2..38], 1, 0, 1, 1, 'B', 10, 100);

    var out: [2]event.Event = undefined;
    const n = parse_buffer_framed(&raw, raw.len, &out, out.len);
    try testing.expectEqual(@as(i64, 1), n);
    const ev = out[0];
    try testing.expectEqual(@as(u64, 10), ev.qty);
}

test "unknown message type returns -1" {
    var raw = [_]u8{ 0xFF, 0xFF };
    var out: [1]event.Event = undefined;
    const n = parse_buffer(&raw, raw.len, &out, out.len);
    try testing.expectEqual(@as(i64, -1), n);
}

test "truncated tail stops cleanly without error" {
    // Full 'A' message (36 B) followed by 5 stray bytes not enough
    // for any complete frame. Parser must return 1, not -1, not crash.
    var raw: [41]u8 = undefined;
    @memset(&raw, 0);
    build_add_order(raw[0..36], 1, 0, 1, 7, 'B', 100, 500);
    raw[36] = 'D'; // valid type tag
    // bytes 37..41 are zero — 'D' needs 19 bytes total, only 5 available
    var out: [4]event.Event = undefined;
    const n = parse_buffer(&raw, raw.len, &out, out.len);
    try testing.expectEqual(@as(i64, 1), n);
    try testing.expectEqual(@as(u64, 7), out[0].order_id);
}

test "parse Order Executed ('E') carries shares in qty" {
    var raw: [31]u8 = undefined;
    @memset(&raw, 0);
    raw[0] = 'E';
    write_be_u16(raw[1..], 1);
    write_be_u16(raw[3..], 0);
    write_be_u48(raw[5..], 0, 42);
    write_be_u64(raw[11..], 777); // order_ref
    write_be_u32(raw[19..], 55);  // executed_shares

    var out: [2]event.Event = undefined;
    const n = parse_buffer(&raw, raw.len, &out, out.len);
    try testing.expectEqual(@as(i64, 1), n);
    try testing.expectEqual(@as(u8, @intFromEnum(event.EventType.trade)), out[0].event_type);
    try testing.expectEqual(@as(u64, 777), out[0].order_id);
    try testing.expectEqual(@as(u64, 55), out[0].qty);
}

test "parse Trade Non-Cross ('P') carries price and side" {
    var raw: [44]u8 = undefined;
    @memset(&raw, 0);
    raw[0] = 'P';
    write_be_u16(raw[1..], 2);
    write_be_u16(raw[3..], 0);
    write_be_u48(raw[5..], 0, 999);
    write_be_u64(raw[11..], 1234);
    raw[19] = 'S';
    write_be_u32(raw[20..], 300);
    // bytes 24..31: stock symbol (ignored)
    write_be_u32(raw[32..], 123_456);

    var out: [2]event.Event = undefined;
    const n = parse_buffer(&raw, raw.len, &out, out.len);
    try testing.expectEqual(@as(i64, 1), n);
    try testing.expectEqual(@as(u8, @intFromEnum(event.EventType.trade)), out[0].event_type);
    try testing.expectEqual(@as(u8, @intFromEnum(event.Side.ask)), out[0].side);
    try testing.expectEqual(@as(u64, 300), out[0].qty);
    try testing.expectEqual(@as(u64, 123_456), out[0].price);
}

test "out_cap bounds the number of parsed events" {
    // Three back-to-back 'A' messages, but out_cap = 2.
    var raw: [108]u8 = undefined;
    @memset(&raw, 0);
    build_add_order(raw[0..36], 1, 0, 1, 1, 'B', 1, 100);
    build_add_order(raw[36..72], 1, 0, 2, 2, 'B', 2, 200);
    build_add_order(raw[72..108], 1, 0, 3, 3, 'B', 3, 300);
    var out: [2]event.Event = undefined;
    const n = parse_buffer(&raw, raw.len, &out, out.len);
    try testing.expectEqual(@as(i64, 2), n);
    try testing.expectEqual(@as(u64, 1), out[0].order_id);
    try testing.expectEqual(@as(u64, 2), out[1].order_id);
}

test "system event ('S') is skipped but stream keeps advancing" {
    // 'S' (12 B, no orderbook event) followed by 'A' (36 B).
    var raw: [48]u8 = undefined;
    @memset(&raw, 0);
    raw[0] = 'S';
    // rest of 'S' can be zero — we only rely on msg_length('S') = 12
    build_add_order(raw[12..48], 1, 0, 5, 500, 'B', 99, 4200);
    var out: [4]event.Event = undefined;
    const n = parse_buffer(&raw, raw.len, &out, out.len);
    try testing.expectEqual(@as(i64, 1), n);
    try testing.expectEqual(@as(u64, 500), out[0].order_id);
    try testing.expectEqual(@as(u64, 99), out[0].qty);
}

test "timestamp composition from u16 hi + u32 lo is monotone BE" {
    var raw: [36]u8 = undefined;
    @memset(&raw, 0);
    // ts_hi = 0xAAAA, ts_lo = 0xBBBBCCCC → 0xAAAABBBBCCCC
    build_add_order(&raw, 1, 0xAAAA, 0xBBBBCCCC, 1, 'B', 1, 1);
    var out: [1]event.Event = undefined;
    const n = parse_buffer(&raw, raw.len, &out, out.len);
    try testing.expectEqual(@as(i64, 1), n);
    try testing.expectEqual(@as(u64, 0x0000AAAA_BBBBCCCC), out[0].ts);
}
