# flowlab-verify

Cross-implementation state equivalence. Every stage produces a
deterministic digest; Rust, C++, and Zig must agree bit for bit.

## Contents

| File          | Role                                                     |
| ------------- | -------------------------------------------------------- |
| `hasher.rs`   | `xxh3_64` with domain tags; seq-driven rolling digest    |
| `verifier.rs` | Side-by-side driver running Rust ref vs native pipeline  |

## Digests

| Digest                 | Scope                                          |
| ---------------------- | ---------------------------------------------- |
| Event-log checksum     | Raw event sequence integrity                   |
| Book-state hash        | Full book reconstruction at a given seq        |
| Strategy-state hash    | Strategy internal state per event              |
| Replay digest          | End-to-end hash of full replay output          |

## Canonical reference

Canonical L2 hash over 50 000 synthetic events:

```
xxh3_64 with domain "FLOWLAB-L2-v1"  →  0xf54ce1b763823e87
```

Any mismatch is a hard failure: replay aborts immediately.

## Cross-language consistency

```
Rust state hash  ══╗
                   ╠═══  MUST BE IDENTICAL
C++ state hash   ══╝
```

Validated in CI for Linux and Windows, with and without `--features
native`.
