# hotpath

C++20 hot-path kernels linked into `flowlab-core` via the `native`
feature. No runtime, no exceptions, no RTTI, no allocations in hot
paths.

## Contents

| File              | Role                                               |
| ----------------- | -------------------------------------------------- |
| `src/orderbook.cpp` | Flat-array L2 book, SIMD-friendly update kernel  |
| `src/hasher.cpp`    | `xxh3_64` with domain tag — must match Rust impl |
| `src/stats.cpp`     | Rolling microstructure statistics                 |
| `src/ffi.cpp`       | `extern "C"` surface consumed by Rust            |
| `include/*.h`       | Shared C headers with `#[repr(C)]` layouts       |

## Build

Driven from `core/build.rs` via `cc-rs`.

- MSVC: `/O2 /Oi /EHs-c- /GR- /permissive- /arch:AVX2 /DNDEBUG`
- clang/gcc: `-O3 -fno-exceptions -fno-rtti -march=native -DNDEBUG`
- C++20, `.std("c++20")` forced on all compilers.

Built only when `flowlab-core` is compiled with `--features native`.

## FFI contract

All structs crossing the ABI are `#[repr(C)]` on the Rust side with a
matching POD layout here. Canonical sizes:

```
Event          40 B  align(8)
BookLevel      16 B  align(8)
HasherState    32 B  align(8)
```

Any change requires bumping the version tag in `flowlab-verify` and
re-computing the canonical L2 hash.

## Invariants

- No C++ exceptions cross the FFI boundary.
- No `new` / `delete` in steady state; scratch buffers are caller-owned.
- No static mutable state; all hot kernels are reentrant.
- Hasher output must equal Rust `xxh3_64` bit-for-bit on identical input.
