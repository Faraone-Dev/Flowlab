# tests

End-to-end and fuzz harnesses. The **signal layer** of FLOWLAB.

Five files. They are intentionally short, intentionally direct, and
intentionally redundant against the rest of the test suite — because
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
│   ├── orderbook_consistency.rs  same seed → same hash, 32 seeds
│   └── itch_parser.rs            random bytes → no panic, no UB
└── chaos/
    ├── burst_desync.rs           multi-thread producer/consumer race
    ├── corruption_injection.rs   truncation + bit-flip + duplication + shift
    └── long_run_drift.rs         10 M events, hash checkpoints, zero drift
```

## Running

```bash
# all 5 signal harnesses
cargo test -p flowlab-e2e

# one specific harness
cargo test -p flowlab-e2e --test e2e_clean_replay
cargo test -p flowlab-e2e --test fuzz_orderbook_consistency
```

## Philosophy

- **Signal, not coverage.** Five tests. Each one falsifies a
  cornerstone claim of the system if it fails.
- **Reproducible failures.** Every random input is derived from a
  fixed seed. A failure can be replayed bit-for-bit.
- **No framework.** Helpers fit in 50 lines. If you need more, the
  test should probably be a unit test in its crate.
- **Short shelf life by design.** When the cross-impl C++ and Zig
  paths land behind `--features native`, these tests grow extra
  assertions that compare hashes across implementations. The skeleton
  stays the same.
