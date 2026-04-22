# flowlab-verify

Cross-implementation state equivalence. Every stage produces a
deterministic digest; the goal is bit-for-bit agreement across
language implementations.

## Status

| Pair                        | Status                                                       |
| --------------------------- | ------------------------------------------------------------ |
| Rust ↔ Zig (parser / event) | Validated — `flowlab_event_size` checked on every parse call, parser output compared against Rust ITCH parser in `flowlab-e2e` fuzz tests |
| Rust ↔ Rust (BTreeMap vs HotOrderBook) | Validated — `bench/benches/replay.rs` and `bench/benches/pipeline.rs::bench_reference_vs_hot` assert canonical L2 hash agreement inside the benchmark |
| Rust ↔ C++ (`StateHasher`)  | Validated — `bench/benches/pipeline.rs::bench_cpp_agreement` (gated `--features native`) asserts byte-for-byte agreement across FFI; both sides compute `state = xxh3_64(state_le \|\| data)` per update via `xxhash-rust::xxh3_64` (Rust) and the official `XXH3_64bits` single-header `xxhash.h` v0.8.3 (C++) |

## Contents

| File          | Role                                                         |
| ------------- | ------------------------------------------------------------ |
| `hasher.rs`   | `StateHasher` — streaming `xxh3_64` of `H(prev \|\| data)`   |
| `verifier.rs` | `CrossLanguageVerifier` — holds Rust + C++ hashes, returns `Err(VerificationError::HashMismatch)` on divergence |

## Canonical reference

All hashes use the domain tag `FLOWLAB-L2-v1` and are seeded with that
tag to keep the digest space disjoint from raw `xxh3_64` of unrelated
data.

```
Canonical L2 hash over 5000 synthetic events:  0xf54ce1b763823e87
```

Any mismatch within a validated pair is a hard failure: replay aborts
immediately.

## Notes

- `xxh3_64` was chosen for cross-implementation portability and
  bench-friendly speed; the digest is **not** a security primitive.
- The Zig side does not currently compute its own canonical hash;
  Rust↔Zig agreement is validated at the parser layer (event ABI +
  byte-for-byte event sequence), which is sufficient because
  downstream state derivation is single-source (Rust).
