# flowlab-core

Source of truth: canonical event type, order book state machine, FFI surface
to the C++ hot path.

## Contents

| File            | Role                                                             |
| --------------- | ---------------------------------------------------------------- |
| `event.rs`      | `#[repr(C)] Event` — 40 B aligned(8), bit-identical across impls |
| `types.rs`      | `EventType`, `Side`, ID newtypes                                 |
| `orderbook.rs`  | Portable reference book (BTreeMap-backed, deterministic)         |
| `hot_book.rs`   | `HotOrderBook` — flat arrays + pre-allocated order map           |
| `state.rs`      | Aggregate state wrapper (book + counters + digest sink)          |
| `ffi.rs`        | Extern "C" surface to `hotpath/` C++                             |
| `build.rs`      | cc-rs driven C++ build + Zig static lib link (`--features native`) |

## Event layout (canonical)

```
offset  size  field
0       8     ts              u64 LE, nanoseconds
8       8     price           u64 LE, integer ticks
16      8     qty             u64 LE
24      8     order_id        u64 LE
32      4     instrument_id   u32 LE
36      1     event_type      u8
37      1     side            u8
38      2     _pad            [u8;2]
—       —     total: 40 B, align(8), #[repr(C)]
```

`bytemuck::Pod + Zeroable` derived. Safe to `cast_slice` between
`[u8]` and `[Event]` on any little-endian target.

## Features

- `native` (off by default): compile `hotpath/` C++ via `cc-rs` and link
  the Zig `feed-parser` static lib. Requires MSVC / clang++ + Zig 0.13.
- default: pure Rust; no external toolchain, portable build.

## Invariants

- `Event` layout is frozen. Any change requires a corpus re-hash and a
  bump of the canonical L2 hash reference in `flowlab-verify`.
- `HotOrderBook` pre-allocates all internal maps via `with_capacity` at
  construction; hot path never triggers `realloc`.
- No allocations in `apply_event` once steady state is reached.
