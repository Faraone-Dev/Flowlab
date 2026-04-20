# flowlab-bench

Criterion benchmarks. Only deterministic, pre-allocated workloads —
allocator churn is an explicit non-goal and is audited per-run.

## Benches

| Bench          | What it measures                                            |
| -------------- | ----------------------------------------------------------- |
| `pipeline.rs`  | End-to-end: ITCH bytes → normalized event → book update     |
| `replay.rs`    | Replay engine throughput on a fixed event log               |

## Bin harnesses

| Binary           | What it measures                                                                |
| ---------------- | ------------------------------------------------------------------------------- |
| `latency_apply`  | Per-event-type latency histogram of `HotOrderBook::apply` across 7 phases.       |
|                  | See [docs/latency-alpha.md](../docs/latency-alpha.md) for the α optimisation log.|

```bash
cargo build --release -p flowlab-bench --bin latency_apply
./target/release/latency_apply
```

## Running

```bash
cargo bench -p flowlab-bench                         # portable Rust
cargo bench -p flowlab-bench --features native       # C++ hot path + Zig
cargo bench -p flowlab-bench --bench pipeline --no-run  # CI compile-only
```

## Methodology

- Inputs: deterministic synthetic streams (seeded XorShift64).
- Buffers pre-allocated via `Vec::with_capacity` before measurement.
- `criterion` black-box on both inputs and outputs.
- No timing-dependent branches: all ordering is sequence-driven.

## Non-goals

- No network I/O, no disk I/O inside measured regions.
- No JIT warm-up loops: benches record steady-state behaviour.
- No allocation in the hot path of any bench kernel.
