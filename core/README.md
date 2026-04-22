# flowlab-core

Source of truth: canonical event type, two orderbook implementations,
deterministic state machine, and the FFI surface to the C++ hot path
and the Zig parser.

## Contents

| File            | Role                                                                |
| --------------- | ------------------------------------------------------------------- |
| `event.rs`      | `#[repr(C)] Event` — 40 B, align(8), bit-identical across impls     |
| `types.rs`      | `Price`, `Qty`, `OrderId`, `SeqNum` newtypes                        |
| `orderbook.rs`  | Portable reference book (`BTreeMap`-backed) — used as the oracle    |
| `hot_book.rs`   | `HotOrderBook<MAX>` — flat-array L2 + slab-backed order index       |
| `state.rs`      | Aggregate state wrapper (book + counters + running digest)          |
| `ffi.rs`        | `extern "C"` bindings to `hotpath/` (C++) and `feed-parser/` (Zig)  |
| `lib.rs`        | Module re-exports                                                   |
| `build.rs`      | `cc-rs`-driven C++ build + Zig static lib link (`--features native`) |

## Event layout (canonical)

```
offset  size  field
0       8     ts              u64 LE, nanoseconds (informational)
8       8     price           u64 LE, integer ticks
16      8     qty             u64 LE
24      8     order_id        u64 LE
32      4     instrument_id   u32 LE
36      1     event_type      u8
37      1     side            u8
38      2     _pad            [u8;2]
—       —     total: 40 B, align(8), #[repr(C)]
```

`bytemuck::Pod + Zeroable` derived. `const _: () = assert!(size_of::<Event>() == 40);`
is enforced at compile time. Safe to `cast_slice` between `[u8]` and
`[Event]` on any little-endian target.

## HotOrderBook

`HotOrderBook<MAX>` keeps:

- A dense `Vec<Level>` per side (bid descending, ask ascending).
- A per-order index split into a dense slab (`Vec<Slot>`) for hot
  IDs and an `aHasher`-backed sparse map (fixed seed for
  determinism) for the long tail.
- All capacities pre-allocated at construction; no reallocation in
  steady state.
- The canonical L2 hash `xxh3_64` with domain tag `FLOWLAB-L2-v1`,
  computed in deterministic level order (bids descending, asks
  ascending) so cross-impl agreement is well-defined.

## Features

- `native` (off by default): compile `hotpath/` C++ via `cc-rs` and
  link the Zig `feed-parser` static lib. Requires MSVC / clang++ and
  Zig 0.13.
- default: pure Rust; no external toolchain, portable build.

## Invariants

- `Event` layout is frozen. Any change requires a corpus re-hash and
  bump of the canonical L2 hash reference in `flowlab-verify`.
- `HotOrderBook` pre-allocates all internal maps via `with_capacity`
  at construction; hot path never triggers `realloc`.
- No allocations in `apply_event` once steady state is reached.

## Tests

`cargo test -p flowlab-core`: 9 unit tests in `hot_book.rs` covering
add/cancel/modify/trade ordering, canonical hash determinism, and
slab/sparse index transitions.
