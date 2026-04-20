# feed-parser

Zig 0.13 ITCH 5.0 parser. `comptime`-specialized per message type,
zero-copy, zero-branch at runtime. Built as a static library and
linked into `flowlab-core` when the `native` feature is enabled.

## Contents

| File              | Role                                                  |
| ----------------- | ----------------------------------------------------- |
| `src/itch.zig`    | Framed + unframed ITCH parsers, per-spec length table |
| `src/event.zig`   | Canonical `Event` mirror (layout-identical to Rust)   |
| `src/main.zig`    | CLI entry for standalone parser benchmarks            |
| `build.zig`       | Static lib + test runner                              |

## Supported messages

| Tag | Message              | Length | Emits                |
| --- | -------------------- | ------ | -------------------- |
| `A` | Add Order            | 36     | `order_add`          |
| `F` | Add Order (MPID)     | 40     | `order_add`          |
| `D` | Order Delete         | 19     | `order_cancel`       |
| `X` | Order Cancel (part.) | 23     | `order_cancel`       |
| `E` | Order Executed       | 31     | `trade`              |
| `C` | Order Executed Price | 36     | `trade`              |
| `P` | Trade Non-Cross      | 44     | `trade`              |
| `S` | System Event         | 12     | — (skipped)          |

Other spec message types are consumed (length-correct) but currently
produce no canonical event.

## Build

```bash
zig build -Doptimize=ReleaseFast   # static lib → zig-out/lib/
zig build test --summary all       # 12 unit tests
```

Critical: `lib.bundle_compiler_rt = false` in `build.zig`. Without
this, MSVC `link.exe` rejects the archive with `LNK1143 "invalid
COMDAT symbol"` when consuming the static lib from Rust.

## FFI contract

```
// extern "C" — consumed from flowlab-core/src/ffi.rs
i64 parse_buffer(const uint8_t* raw, size_t raw_len,
                 Event* out, size_t out_cap);
i64 parse_buffer_framed(const uint8_t* raw, size_t raw_len,
                        Event* out, size_t out_cap);
```

- Returns number of events written, or `-1` on unknown message type.
- Truncated tail is not an error: returns the count of complete events.
- Caller owns `out`; parser never allocates.

## Determinism

Parser output is a pure function of its input bytes. Same bytes in →
same events out, on any platform, any build mode.
