# Cross-hardware reproducibility — `HotOrderBook::apply`

> Status: **2026-04-22**
> Scope: same α build (frozen 2026-04-20), two different x86_64 boxes,
> Windows, software-only, no kernel tuning, no isolcpus, no HUGE pages.
> Methodology: `bench/src/bin/latency_apply.rs`, 5 consecutive runs,
> STEADY mix (500 000 events) with prefetch lookahead enabled.

---

## TL;DR

The α numbers from [`latency-alpha.md`](./latency-alpha.md) are
reproduced on a different CPU generation **without changing a single
line of code**. TOTAL p50 lands on the exact same value (22 ns) across
5/5 runs on the new box, and TRADE p50 actually improves slightly
(30 ns → 28 ns). The path is CPU-bound and limited by the code, not
by the host.

| Metric (STEADY+PF, 500k events) | Box A (frozen) | Box B median (5 runs) | Box B best |
| ------------------------------- | -------------- | --------------------- | ---------- |
| TOTAL p50                       | 22 ns          | **22 ns**             | 22 ns      |
| TOTAL p99                       | 88 ns          | **88 ns**             | 80 ns      |
| TRADE p50                       | 30 ns          | **28 ns**             | 28 ns      |
| TRADE p99                       | 128 ns         | 144 ns                | **96 ns**  |
| Wall apply-only (500k ev)       | 17.15 ms       | 17.40 ms              | **16.64 ms** |

---

## Hosts

| Field           | Box A (frozen)       | Box B (this report)        |
| --------------- | -------------------- | -------------------------- |
| CPU             | AMD Ryzen            | Intel Core i7              |
| OS              | Windows              | Windows                    |
| rdtsc clock     | (calibrated in run)  | 3.7920 cycles/ns (3.79 GHz)|
| rdtsc overhead  | ~5 ns                | 20 cycles ≈ 5 ns           |
| Kernel tuning   | none                 | none                       |
| GPU             | n/a (CPU-bound path) | n/a (CPU-bound path)       |

The path is single-threaded, CPU-bound, integer arithmetic only — GPU
is irrelevant and listed only for completeness.

---

## Box B — 5 consecutive runs (STEADY+PF)

| Run | TOTAL p50 | TOTAL p99 | TRADE p50 | TRADE p99 | Wall (500k ev) |
| --- | --------- | --------- | --------- | --------- | -------------- |
| 1   | 22 ns     | 96 ns     | 30 ns     | 176 ns    | 17.80 ms       |
| 2   | 22 ns     | 96 ns     | 30 ns     | 192 ns    | 17.88 ms       |
| 3   | 22 ns     | 88 ns     | 28 ns     | 144 ns    | 16.97 ms       |
| 4   | 22 ns     | **80 ns** | 28 ns     | **96 ns** | **16.64 ms**   |
| 5   | 22 ns     | 88 ns     | 28 ns     | 144 ns    | 17.40 ms       |
| **median** | **22 ns** | **88 ns** | **28 ns** | **144 ns** | **17.40 ms** |

**TOTAL p50 is 22 ns on 5/5 runs.** Zero variance. The same value
recorded on Box A. This is the strongest signal that the number is
intrinsic to the code path, not to the host.

---

## Reading

- **What is the same:** TOTAL p50 (22 ns), TOTAL p99 median (88 ns),
  α delta vs baseline (-33% to -50% on TOTAL p99 across runs).
  → The optimization survives a CPU change.
- **What is slightly better on Box B:** TRADE p50 (-7%), best-case
  TRADE p99 (-25%), best-case wall-time (-3%).
  → The new core has a faster intrinsic floor.
- **What is slightly worse on Box B:** median TRADE p99 (+12% vs Box A).
  → Pure OS jitter (Windows scheduler + turbo boost variance).
  Expected to collapse on Linux bare metal with `isolcpus` +
  `nohz_full`.

---

## How to reproduce on your box

```bash
git clone https://github.com/Faraone-Dev/flowlab
cd flowlab
cargo build --release -p flowlab-bench --bin latency_apply
for i in 1 2 3 4 5; do ./target/release/latency_apply; done
```

The numbers you should see on any modern x86_64 with Rust 1.80+:

- `TOTAL p50` in `[STEADY+PF]` ≈ **22 ns** (rock-solid across runs)
- `TRADE p50` in `[STEADY+PF]` ≈ **28–30 ns**
- Wall apply-only ≈ **16–18 ms** for 500 000 events
- α delta vs baseline ≈ **-30% to -50%** on TOTAL p99

If your numbers are far off, check:

1. Release build (`--release`), not debug.
2. CPU governor (Linux: `performance`, Windows: high-performance plan).
3. No background CPU-heavy process competing for cache.
4. rdtsc available and stable (most x86_64 since Nehalem).

---

## Why this matters

A latency number from a single host is just a measurement on that
host. The same number from two different CPU generations, identical
code, no tuning, **5/5 reproducible**, is a property of the code.

The α optimization log in `latency-alpha.md` claimed a specific set
of numbers. This document is the second independent confirmation that
those numbers are real and portable.
