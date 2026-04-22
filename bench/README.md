# flowlab-bench

Criterion benchmarks and a standalone latency harness. Only
deterministic, pre-allocated workloads — allocator churn is an
explicit non-goal and is audited per-run.

## Status

| Bench / bin          | Status                                                                |
| -------------------- | --------------------------------------------------------------------- |
| `benches/replay.rs`  | Implemented — replay engine throughput on a fixed event log           |
| `benches/pipeline.rs`| Implemented — end-to-end ITCH bytes → normalized event → book update  |
| `bin/latency_apply`  | Implemented — per-event-type latency histogram of `HotOrderBook::apply` |
| C++ / Zig native bench harness | Build flag (`--features native`) wired; standalone Makefile targets are placeholders (`@echo "TODO"`) |

## Benches

| Bench           | What it measures                                            |
| --------------- | ----------------------------------------------------------- |
| `pipeline.rs`   | End-to-end: ITCH bytes → normalized event → book update     |
| `replay.rs`     | Replay engine throughput on a fixed event log + cross-impl hash agreement (BTreeMap vs HotOrderBook) |

## Bin harnesses

| Binary           | What it measures                                                                   |
| ---------------- | ---------------------------------------------------------------------------------- |
| `latency_apply`  | Per-event-type latency histogram of `HotOrderBook::apply` across phases.            |
|                  | See [docs/latency-alpha.md](../docs/latency-alpha.md) for the α optimisation log.   |

```bash
cargo build --release -p flowlab-bench --bin latency_apply
./target/release/latency_apply
```

## Running

```bash
cargo bench -p flowlab-bench                         # portable Rust
cargo bench -p flowlab-bench --features native       # + C++ hot path + Zig
cargo bench -p flowlab-bench --bench pipeline --no-run  # CI compile-only
```

## Methodology

- Inputs: deterministic synthetic streams (seeded XorShift64).
- Buffers pre-allocated via `Vec::with_capacity` before measurement.
- `criterion` black-box on both inputs and outputs.
- No timing-dependent branches: all ordering is sequence-driven.

## Cross-implementation hash agreement

`benches/replay.rs` runs the same event stream through both the
`BTreeMap` reference book and `HotOrderBook<256>` and asserts the
canonical L2 hash matches. This is the per-commit guarantee that the
two Rust implementations stay in lock-step. The Zig parser side is
covered by the integration tests in `flowlab-e2e`.

## Non-goals

- No network I/O, no disk I/O inside measured regions.
- No JIT warm-up loops: benches record steady-state behaviour.
- No allocation in the hot path of any bench kernel.
