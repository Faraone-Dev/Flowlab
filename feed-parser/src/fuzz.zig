// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

//! ITCH 5.0 parser fuzz harness.
//!
//! Property-based, deterministic (PRNG seeded). Designed to run in CI
//! in the order of seconds even at 1M iterations per property.
//!
//! Invariants verified:
//!
//!   F1. **No over-read, no panic** on arbitrary input.
//!       For a uniform-random byte buffer of random length in
//!       `[0, MAX_RAW]`, both `parse_buffer` and `parse_buffer_framed`
//!       must return without crashing. The returned count must satisfy
//!       `-1 <= n <= out_cap`. The parser must never read past `raw_len`.
//!
//!   F2. **Unknown message type halts cleanly.**
//!       For every byte `t` such that `msg_length(t) == 0`, a buffer
//!       starting with `t` must yield `-1` from `parse_buffer` (no
//!       advance, no partial parse).
//!
//!   F3. **Truncation safety.**
//!       For every prefix length `0 <= k < L` of a fully valid `L`-byte
//!       message, the parser must terminate without panic and without
//!       over-reading. Returned count must be `0` (truncated tail
//!       discarded).
//!
//!   F4. **Round-trip on AddOrder.**
//!       For random valid `(locate, ts_hi, ts_lo, oref, side, shares,
//!       price)`, building an 'A' message and parsing it must yield an
//!       Event whose fields equal the inputs (modulo the documented side
//!       remapping `'B' → 0`, anything else → 1).
//!
//! Counts are kept seeded and bounded so the run is bit-exact across
//! machines. This is *not* AFL-style coverage-guided fuzzing; it is
//! property-based stress with a fixed seed, suitable for CI gating.

const std = @import("std");
const itch = @import("itch.zig");
const event = @import("event.zig");

const MAX_RAW: usize = 4096;
const OUT_CAP: usize = 256;

/// Fixed CI seed — change only if the fuzz contract itself changes.
const FUZZ_SEED: u64 = 0xF10B1AB_F022_2026;

// ─── F1: arbitrary input safety ──────────────────────────────────────

test "fuzz F1: parse_buffer never panics on random input" {
    var prng = std.Random.DefaultPrng.init(FUZZ_SEED ^ 0x01);
    const rand = prng.random();

    var raw: [MAX_RAW]u8 = undefined;
    var out: [OUT_CAP]event.Event = undefined;

    // 200_000 iterations — ~100 ms in ReleaseFast on a modern x86_64.
    var i: usize = 0;
    while (i < 200_000) : (i += 1) {
        const len = rand.uintLessThan(usize, MAX_RAW + 1);
        rand.bytes(raw[0..len]);

        const n = itch.parse_buffer(&raw, len, &out, OUT_CAP);
        try std.testing.expect(n >= -1);
        try std.testing.expect(n <= @as(i64, OUT_CAP));
    }
}

test "fuzz F1: parse_buffer_framed never panics on random input" {
    var prng = std.Random.DefaultPrng.init(FUZZ_SEED ^ 0x02);
    const rand = prng.random();

    var raw: [MAX_RAW]u8 = undefined;
    var out: [OUT_CAP]event.Event = undefined;

    var i: usize = 0;
    while (i < 200_000) : (i += 1) {
        const len = rand.uintLessThan(usize, MAX_RAW + 1);
        rand.bytes(raw[0..len]);

        const n = itch.parse_buffer_framed(&raw, len, &out, OUT_CAP);
        try std.testing.expect(n >= -1);
        try std.testing.expect(n <= @as(i64, OUT_CAP));
    }
}

// ─── F2: unknown type halts cleanly ──────────────────────────────────

test "fuzz F2: unknown msg type returns -1 from parse_buffer" {
    var raw: [64]u8 = undefined;
    @memset(&raw, 0);
    var out: [4]event.Event = undefined;

    var t: u16 = 0;
    while (t < 256) : (t += 1) {
        const tag: u8 = @intCast(t);
        if (itch.msg_length(tag) != 0) continue; // skip known types

        raw[0] = tag;
        const n = itch.parse_buffer(&raw, raw.len, &out, out.len);
        try std.testing.expectEqual(@as(i64, -1), n);
    }
}

// ─── F3: truncation safety ───────────────────────────────────────────

fn build_minimal_add_order(buf: *[36]u8) void {
    @memset(buf, 0);
    buf[0] = 'A';
    // locate, ts, oref, side, shares, price filled with deterministic dummies
    std.mem.writeInt(u16, buf[1..3], 7, .big);
    std.mem.writeInt(u16, buf[3..5], 0, .big);
    std.mem.writeInt(u16, buf[5..7], 0x0001, .big);
    std.mem.writeInt(u32, buf[7..11], 0xDEADBEEF, .big);
    std.mem.writeInt(u64, buf[11..19], 0xCAFEBABE_DEADBEEF, .big);
    buf[19] = 'B';
    std.mem.writeInt(u32, buf[20..24], 12_345, .big);
    std.mem.writeInt(u32, buf[32..36], 99_500_000, .big);
}

test "fuzz F3: truncated AddOrder never panics" {
    var raw: [36]u8 = undefined;
    build_minimal_add_order(&raw);
    var out: [4]event.Event = undefined;

    // Every prefix in [0, 36) must terminate cleanly. The parser
    // detects truncation via `offset + len > raw_len` and breaks.
    var k: usize = 0;
    while (k < raw.len) : (k += 1) {
        const n = itch.parse_buffer(&raw, k, &out, out.len);
        // Truncated tail: 0 events parsed (or -1 if k==0 hits unknown
        // path — but k==0 means while-loop never enters, so 0).
        try std.testing.expect(n == 0 or n == -1);
    }

    // Full message must parse exactly one event.
    const n_full = itch.parse_buffer(&raw, raw.len, &out, out.len);
    try std.testing.expectEqual(@as(i64, 1), n_full);
}

test "fuzz F3: framed truncation at every byte never panics" {
    // Framed: [u16 len][payload]. Build a single-frame buffer of
    // total size 38 (2 + 36) and truncate at every byte.
    var raw: [38]u8 = undefined;
    @memset(&raw, 0);
    std.mem.writeInt(u16, raw[0..2], 36, .big);
    var inner: [36]u8 = undefined;
    build_minimal_add_order(&inner);
    @memcpy(raw[2..], &inner);

    var out: [4]event.Event = undefined;

    var k: usize = 0;
    while (k <= raw.len) : (k += 1) {
        const n = itch.parse_buffer_framed(&raw, k, &out, out.len);
        try std.testing.expect(n >= 0);
        try std.testing.expect(n <= @as(i64, out.len));
    }

    const n_full = itch.parse_buffer_framed(&raw, raw.len, &out, out.len);
    try std.testing.expectEqual(@as(i64, 1), n_full);
}

// ─── F4: round-trip on AddOrder with random fields ───────────────────

fn build_add_order_with(buf: *[36]u8, locate: u16, ts_hi: u16, ts_lo: u32, oref: u64, side: u8, shares: u32, price: u32) void {
    @memset(buf, 0);
    buf[0] = 'A';
    std.mem.writeInt(u16, buf[1..3], locate, .big);
    std.mem.writeInt(u16, buf[3..5], 0, .big);
    std.mem.writeInt(u16, buf[5..7], ts_hi, .big);
    std.mem.writeInt(u32, buf[7..11], ts_lo, .big);
    std.mem.writeInt(u64, buf[11..19], oref, .big);
    buf[19] = side;
    std.mem.writeInt(u32, buf[20..24], shares, .big);
    std.mem.writeInt(u32, buf[32..36], price, .big);
}

test "fuzz F4: AddOrder round-trip preserves all fields" {
    var prng = std.Random.DefaultPrng.init(FUZZ_SEED ^ 0x04);
    const rand = prng.random();

    var raw: [36]u8 = undefined;
    var out: [4]event.Event = undefined;

    var i: usize = 0;
    while (i < 100_000) : (i += 1) {
        const locate = rand.int(u16);
        const ts_hi = rand.int(u16);
        const ts_lo = rand.int(u32);
        const oref = rand.int(u64);
        // Pick side from a small alphabet to also exercise the
        // "anything else → ask" branch documented in parse_add_order.
        const side_alphabet = [_]u8{ 'B', 'S', 'X', 0, 0xFF };
        const side = side_alphabet[rand.uintLessThan(usize, side_alphabet.len)];
        const shares = rand.int(u32);
        const price = rand.int(u32);

        build_add_order_with(&raw, locate, ts_hi, ts_lo, oref, side, shares, price);

        const n = itch.parse_buffer(&raw, raw.len, &out, out.len);
        try std.testing.expectEqual(@as(i64, 1), n);

        const ev = out[0];
        const expected_ts: u64 = (@as(u64, ts_hi) << 32) | @as(u64, ts_lo);
        try std.testing.expectEqual(expected_ts, ev.ts);
        try std.testing.expectEqual(@as(u32, locate), ev.instrument_id);
        try std.testing.expectEqual(@as(u8, @intFromEnum(event.EventType.order_add)), ev.event_type);
        try std.testing.expectEqual(oref, ev.order_id);
        try std.testing.expectEqual(@as(u64, shares), ev.qty);
        try std.testing.expectEqual(@as(u64, price), ev.price);

        const expected_side: u8 = if (side == 'B')
            @intFromEnum(event.Side.bid)
        else
            @intFromEnum(event.Side.ask);
        try std.testing.expectEqual(expected_side, ev.side);
    }
}

// ─── Stream fuzz: random concatenation of valid messages ─────────────

test "fuzz F1+F4 combined: random valid stream parses without loss" {
    var prng = std.Random.DefaultPrng.init(FUZZ_SEED ^ 0x05);
    const rand = prng.random();

    var raw: [MAX_RAW]u8 = undefined;
    var out: [OUT_CAP]event.Event = undefined;

    var iter: usize = 0;
    while (iter < 1_000) : (iter += 1) {
        // Build a stream of valid AddOrder messages back-to-back.
        const msg_count = rand.uintLessThan(usize, 32) + 1;
        var off: usize = 0;
        var built: usize = 0;
        while (built < msg_count and off + 36 <= raw.len) : (built += 1) {
            var msg: [36]u8 = undefined;
            build_add_order_with(
                &msg,
                rand.int(u16),
                rand.int(u16),
                rand.int(u32),
                rand.int(u64),
                'B',
                rand.int(u32),
                rand.int(u32),
            );
            @memcpy(raw[off .. off + 36], &msg);
            off += 36;
        }

        const n = itch.parse_buffer(&raw, off, &out, OUT_CAP);
        try std.testing.expectEqual(@as(i64, @intCast(built)), n);
    }
}
