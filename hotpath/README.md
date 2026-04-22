# hotpath

C++20 hot-path kernels linked into `flowlab-core` via the optional
`native` feature. No runtime, no exceptions, no RTTI, no allocations
in steady state.

## Status

| Component                                    | Status                                                                |
| -------------------------------------------- | --------------------------------------------------------------------- |
| `OrderBook<MaxLevels>` (`orderbook.cpp`)     | Implemented — flat-array L2, **branchless binary search (`partition_point`) + `std::memmove` insert/remove**, mirror 1:1 of Rust `HotOrderBook`; cross-FFI L2 digest validated bit-for-bit by `bench/benches/pipeline.rs::bench_cpp_agreement` |
| `RollingStats` (`include/flowlab/stats.h`)   | Implemented — header-only Welford, `O(1)` per update                  |
| `StateHasher` (`hasher.cpp`)                 | Implemented — official `xxh3_64` (single-header `xxhash.h`), bit-identical to Rust `xxhash-rust` |
| `stats.cpp` SIMD batch updates               | **WIP** — file is a TODO marker for AVX2 volatility batches           |
| `extern "C"` FFI surface (`ffi.cpp`)         | Implemented — book + hasher entry points exposed to Rust              |

## Contents

| File                   | Role                                                            |
| ---------------------- | --------------------------------------------------------------- |
| `src/orderbook.cpp`    | `OrderBook<256>` and `OrderBook<1024>` template instantiations  |
| `src/hasher.cpp`       | `StateHasher` — `xxh3_64` (mirrors Rust `flowlab-verify::StateHasher`) |
| `src/stats.cpp`        | Reserved for SIMD batch volatility kernels (TODO)               |
| `src/ffi.cpp`          | `extern "C"` surface consumed by `flowlab-core/src/ffi.rs`      |
| `include/flowlab/*.h`  | Shared C++ headers; `#[repr(C)]` layouts mirror Rust types      |

## Build

Driven from `core/build.rs` via `cc-rs` (CMake is provided as a
secondary path for standalone builds and IDE tooling).

- MSVC: `/O2 /Oi /EHs-c- /GR- /permissive- /arch:AVX2 /DNDEBUG`
- clang/gcc: `-O3 -fno-exceptions -fno-rtti -march=native -DNDEBUG`
- C++20, `.std("c++20")` forced on all compilers.

Built only when `flowlab-core` is compiled with `--features native`.

## FFI contract

All structs crossing the ABI are `#[repr(C)]` on the Rust side with a
matching POD layout here. Canonical sizes:

```
Event          40 B  align(8)
Level          24 B  align(8)   (price, total_qty, order_count)
StateHasher     8 B  align(8)   (running u64)
```

Any change requires bumping the version tag in `flowlab-verify` and
re-computing the canonical L2 hash.

## Hasher — implementation

Both sides now compute the same digest:

```
state = xxh3_64( state.to_le_bytes() || data )   // per update
```

- Rust: `xxhash_rust::xxh3::xxh3_64` (seed 0).
- C++: official `XXH3_64bits` from the single-header
  [`xxhash.h`](include/xxhash.h) (v0.8.3) included with
  `XXH_INLINE_ALL` so all symbols stay in this TU. The hot path
  uses a 4 KiB stack buffer and falls back to the streaming
  `XXH3_state_t` API for larger inputs — no heap allocation.

The two implementations are bit-identical by construction (xxhash-rust
is a pure-Rust port of the same XXH3 spec).

## Invariants

- No C++ exceptions cross the FFI boundary (`-fno-exceptions` /
  `/EHs-c-`).
- No `new` / `delete` in steady state for `OrderBook`,
  `RollingStats`, or `StateHasher`.
- No static mutable state; all hot kernels are reentrant.
- Hasher output equals Rust `xxh3_64` bit-for-bit on identical input.
