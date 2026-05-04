// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

const std = @import("std");
const itch = @import("itch.zig");
const event = @import("event.zig");

// ─── C ABI Exports (Rust FFI) ────────────────────────────────────────

/// Parse a raw ITCH buffer into canonical events.
/// Returns number of events written, or -1 on error.
export fn flowlab_parse_itch(
    raw: [*]const u8,
    raw_len: usize,
    out: [*]event.Event,
    out_cap: usize,
) callconv(.C) i64 {
    return itch.parse_buffer(raw, raw_len, out, out_cap);
}

/// Parse a FRAMED ITCH stream (u16 BE length per message).
export fn flowlab_parse_itch_framed(
    raw: [*]const u8,
    raw_len: usize,
    out: [*]event.Event,
    out_cap: usize,
) callconv(.C) i64 {
    return itch.parse_buffer_framed(raw, raw_len, out, out_cap);
}

/// Get the size of the canonical Event struct (for validation).
export fn flowlab_event_size() callconv(.C) usize {
    return @sizeOf(event.Event);
}

test "event size is 40 bytes" {
    try std.testing.expectEqual(@as(usize, 40), @sizeOf(event.Event));
}
