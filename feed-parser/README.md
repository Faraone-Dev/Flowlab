# feed-parser

Zig 0.13 ITCH 5.0 parser. `comptime`-checked event layout, zero-copy,
no allocations. Built as a static library and linked into
`flowlab-core` when the `native` feature is enabled.

## Status

| Component                          | Status                                                       |
| ---------------------------------- | ------------------------------------------------------------ |
| `event.zig` — canonical `Event`    | Implemented — `extern struct`, layout-identical to Rust       |
| `itch.zig` — `parse_buffer`        | Implemented — unframed stream, per-spec length advancement    |
| `itch.zig` — `parse_buffer_framed` | Implemented — `u16 BE` length-prefixed stream                 |
| `main.zig` — C ABI exports         | Implemented — `flowlab_parse_itch`, `flowlab_parse_itch_framed`, `flowlab_event_size` |
| Comptime size assertion            | Implemented — `@sizeOf(Event) == 40` checked at compile time and validated at runtime by the Rust caller |
| Tests                              | 12 (11 in `itch.zig` + 1 in `main.zig`)                       |

## Contents

| File              | Role                                                  |
| ----------------- | ----------------------------------------------------- |
| `src/itch.zig`    | Framed + unframed parsers, per-spec length table, 11 tests |
| `src/event.zig`   | Canonical `Event` mirror (layout-identical to Rust)   |
| `src/main.zig`    | `extern "C"` exports + size-assertion test            |
| `build.zig`       | Static lib + test runner                              |

## Supported messages

Today the parser recognizes the spec lengths for all major ITCH 5.0
message types and converts the following into canonical events:

| Tag | Message              | Length | Emits                |
| --- | -------------------- | ------ | -------------------- |
| `A` | Add Order            | 36     | `order_add`          |
| `F` | Add Order (MPID)     | 40     | `order_add`          |
| `D` | Order Delete         | 19     | `order_cancel`       |
| `X` | Order Cancel (part.) | 23     | `order_cancel`       |
| `E` | Order Executed       | 31     | `trade`              |
| `C` | Order Executed Price | 36     | `trade`              |
| `P` | Trade Non-Cross      | 44     | `trade`              |

System events (`S`), reg-NMS metadata (`R`/`H`/`Y`/...) and other
non-book messages are length-correct in the lookup table and skipped
without emitting events. The stream advances correctly through them
so the parser stays in sync.

## Build

```bash
zig build -Doptimize=ReleaseFast   # static lib → zig-out/lib/
zig build test --summary all       # runs the 12 unit tests
```

Critical: `lib.bundle_compiler_rt = false` in `build.zig`. Without
this, MSVC `link.exe` rejects the archive with `LNK1143 "invalid
COMDAT symbol"` when consuming the static lib from Rust.

## FFI contract

```c
// extern "C" — consumed from flowlab-core/src/ffi.rs
i64    flowlab_parse_itch(const uint8_t* raw, size_t raw_len,
                          Event* out, size_t out_cap);
i64    flowlab_parse_itch_framed(const uint8_t* raw, size_t raw_len,
                                 Event* out, size_t out_cap);
size_t flowlab_event_size(void);
```

- Return: number of events written, or `-1` on unknown message type.
- A truncated tail at end-of-buffer is **not** an error; the function
  returns the count of complete events parsed so far.
- The caller owns `out`; the parser never allocates.
- `flowlab_event_size()` is checked by Rust on every parse to detect
  any future ABI drift.

## Determinism

Parser output is a pure function of its input bytes. Same bytes in →
same events out, on any platform, in any build mode.
