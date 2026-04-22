# tests (`flowlab-e2e`)

End-to-end, fuzz, and chaos harnesses. The **signal layer** of
FLOWLAB.

These files are intentionally short, intentionally direct, and
intentionally redundant against the per-crate unit tests — because
they validate the *system*, not the modules.

## Layout

```
tests/
├── Cargo.toml          flowlab-e2e crate (workspace member, publish=false)
├── src/lib.rs          shared helpers: tmp_path, XorShift64
├── e2e/
│   ├── clean_replay.rs        ITCH → ring → consumer → hash oracle
│   ├── cancel_heavy.rs        70% cancel churn — no leak, deterministic
│   └── stress_overflow.rs     near-full ring, backpressure invariance
├── fuzz/
│   ├── orderbook_consistency.rs  same seed → same hash, multiple seeds
│   └── itch_parser.rs            random bytes → no panic, no UB
└── chaos/
    ├── burst_desync.rs           multi-thread producer/consumer race
    ├── corruption_injection.rs   truncation + bit-flip + duplication + shift
    └── long_run_drift.rs         10 M events, hash checkpoints, zero drift
```

## What each chaos test proves

| Test                          | Falsifies if it fails                                        |
| ----------------------------- | ------------------------------------------------------------ |
| `long_run_drift.rs`           | Determinism over 10 M events: per-million-event checkpoints from two runs of the same seed must match exactly. Catches map leaks, accumulated overflow, hash iteration-order bugs. |
| `corruption_injection.rs`     | Parser robustness: truncated tails, bit-flips, duplicated frames, and shifted offsets must never panic, never read OOB, never fabricate events. |
| `burst_desync.rs`             | SPSC ring memory ordering: with a producer burst and a consumer stall on a 4 KiB ring, the final L2 hash must match the single-threaded reference. |

## Running

```bash
# all signal harnesses
cargo test -p flowlab-e2e

# one specific harness
cargo test -p flowlab-e2e --test e2e_clean_replay
cargo test -p flowlab-e2e --test fuzz_orderbook_consistency
cargo test -p flowlab-e2e --test chaos_long_run_drift
```

## Philosophy

- **Signal, not coverage.** Each test falsifies a cornerstone claim
  of the system if it fails.
- **Reproducible failures.** Every random input is derived from a
  fixed seed. A failure can be replayed bit-for-bit.
- **No framework.** Helpers fit in 50 lines. If you need more, the
  test should probably be a unit test in its crate.
- **Short shelf life by design.** When the cross-impl C++ digest
  lands behind `--features native`, these tests grow extra
  assertions that compare hashes across implementations. The
  skeleton stays the same.
