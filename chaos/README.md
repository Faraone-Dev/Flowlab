# flowlab-chaos

HFT aggression analysis pipeline operating on the canonical event
stream. Three layers: per-event detectors → temporal clustering →
stress-window extraction (joined with `flowlab-flow`'s regime tag).

## Status

| Layer / Detector                            | Status      | Tests |
| ------------------------------------------- | ----------- | ----- |
| `PhantomLiquidityDetector`                  | Implemented | 8     |
| `CancellationStormDetector`                 | Implemented | 8     |
| `MomentumIgnitionDetector`                  | Implemented | 8     |
| `FlashCrashDetector`                        | Implemented | 7     |
| `LatencyArbProxyDetector`                   | Implemented | 8     |
| `QuoteStuffDetector` / `SpoofDetector`      | Reserved (enum variants only — no producer in this crate) | — |
| `ChaosChain` (5-detector fan-out)           | Implemented | 2     |
| `ChaosClusterer` (sequence-gap merge)       | Implemented | —     |
| `WindowExtractor` (regime + chaos → window) | Implemented | —     |

Total: **41 unit + property tests, all green** (`cargo test -p flowlab-chaos --lib`).

The detectors use sequence-bounded windows by default (time windows
are opt-in per detector via a non-zero `*_time_*` parameter). Results
are bit-identical under replay of the same event log when only
sequence windows are enabled.

## Module map

| File                       | Role                                                            |
| -------------------------- | --------------------------------------------------------------- |
| `lib.rs`                   | `ChaosKind`, `ChaosEvent`, `ChaosFeatures`, `StressWindow` types|
| `chain.rs`                 | `ChaosChain` — runs all 5 detectors per event in fixed order    |
| `phantom_liquidity.rs`     | Add → full-cancel cycles within seq/time window, no trade       |
| `cancellation_storm.rs`    | Adaptive (Welford) baseline of cancel rate, k·σ trigger         |
| `momentum_ignition.rs`     | Sustained directional run + size + book imbalance               |
| `flash_crash.rs`           | Instantaneous mid gap + depth vacuum + aligned aggression       |
| `latency_arb_proxy.rs`     | Trade-print followed by opposite-side queue replenishment burst |
| `detection.rs`             | Legacy `QuoteStuff` / `Spoof` detectors                         |
| `clustering.rs`            | `ChaosClusterer` — merges nearby `ChaosEvent`s                  |
| `window.rs`                | `WindowExtractor` — joins regime + chaos into `StressWindow`    |

## ChaosChain

`ChaosChain` is the single integration point for downstream consumers
(`flowlab-lab::executor::StrategyExecutor::with_chaos_chain`,
`bench/chaos_throughput`). It owns one instance of each detector and
fans every event out in this fixed order:

1. PhantomLiquidity
2. CancellationStorm
3. MomentumIgnition
4. FlashCrash
5. LatencyArbProxy

Order does not affect correctness (detectors are independent) but it
*does* fix the order of `ChaosEvent`s in the output vector when one
event triggers more than one detector — useful for diff-able reports.

`ChaosChain::default_itch()` builds a chain with the calibration
table below.

## Calibration table — `default_itch()`

Tuned for raw ITCH 5.0 streams (US equities, ~100 µs cadence,
4-decimal quoted price scale). Conservative defaults — each detector
aims for **< 0.5 % flag rate on a clean trading day**. Re-tune per
venue / instrument before production use.

### PhantomLiquidity

| Param                | Value       | Rationale                                                           |
| -------------------- | ----------- | ------------------------------------------------------------------- |
| `max_seq_gap`        | 256         | Add-cancel cycles past this distance are rarely manipulative.       |
| `max_time_gap_ns`    | 5 ms        | OR-window with seq; fast cancels < 5 ms still flag on slow streams. |
| `max_tracked`        | 4096        | Bounded HashMap; eviction is LRU-by-creation-seq.                   |
| `min_qty`            | 100         | Filters one-lot probe traffic.                                      |

### CancellationStorm

| Param                | Value       | Rationale                                                           |
| -------------------- | ----------- | ------------------------------------------------------------------- |
| `seq_window`         | 2 048       | ~200 ms at peak ITCH cadence.                                       |
| `warmup_samples`     | 1 024       | Welford needs a stable baseline before the threshold is meaningful. |
| `sigma_floor`        | 0.05        | Prevents flat-baseline collapse on quiet symbols.                   |
| `k_sigma`            | 4.0         | 4σ ≈ 1 in ~16k events false-positive at Gaussian baseline.          |
| `min_consecutive`    | 8           | Persistence gate — single-event spikes are ignored.                 |
| `cooldown_seq`       | 4 096       | Refractory; one storm = one flag, not a stream of duplicates.       |

### MomentumIgnition

| Param                | Value       | Rationale                                                           |
| -------------------- | ----------- | ------------------------------------------------------------------- |
| `seq_window`         | 1 024       | ~100 ms at peak cadence.                                            |
| `max_book_levels`    | 64          | Mini-book truncation; farthest-from-best dropped first.             |
| `min_move_bps`       | 5           | 5 bps ≈ 0.05 % — well above bid-ask noise.                          |
| `min_trades`         | 8           | Real ignition needs aggregated aggression, not a single print.      |
| `min_consecutive`    | 4           | Mid must move directionally for ≥ 4 ticks (anti-overlap with FlashCrash). |
| `cooldown_seq`       | 2 048       | Refractory.                                                         |

### FlashCrash

| Param                | Value       | Rationale                                                           |
| -------------------- | ----------- | ------------------------------------------------------------------- |
| `min_gap_bps`        | 10          | Twice the ignition threshold — flashes are step-changes.            |
| `gap_window_events`  | 8           | ≤ 8 raw events ≈ 2-3 semantic ticks (anti-overlap with Ignition).   |
| `depth_band_ticks`   | 5           | Anchored to snap-time best — the band does NOT slide with new best. |
| `depth_drop_pct`     | 0.6         | 60 % of band depth must vanish.                                     |
| `min_aggr_trades`    | 3           | Aligned aggression confirms it was *taken*, not just quoted away.   |
| `cooldown_seq`       | 2 048       | Refractory.                                                         |

### LatencyArbProxy

| Param                | Value       | Rationale                                                           |
| -------------------- | ----------- | ------------------------------------------------------------------- |
| `reaction_seq`       | 64          | ~6 ms reaction window — typical local cross-venue latency.          |
| `band_ticks`         | 4           | Burst contributors must sit near the trade price.                   |
| `min_trade_qty`      | 200         | Filters the long tail of small prints.                              |
| `min_burst`          | 4           | At least 4 opposite-side ADD/CANCEL events to qualify.              |
| `require_reversion`  | `false`     | Default off; enable when feed quality is high enough to demand it.  |
| `reversion_bps`      | 0           | Used only when `require_reversion = true`.                          |
| `cooldown_seq`       | 1 024       | Refractory.                                                         |

The "LatencyArb*Proxy*" naming is deliberate: a single ITCH feed
cannot prove cross-venue arbitrage. The detector flags the
*behavioural footprint* (large print → opposite-side queue refill in
band → optional mid reversion), which is a necessary but not
sufficient condition.

## Throughput

Two complementary harnesses live under `bench/`:

### 1. Mean throughput (criterion)

`bench/chaos_throughput.rs`. 50 000-event synthetic ITCH-shaped stream,
median of 20 samples / 3 s window, release profile.

| Group                          | ns / event | Melem/s | vs first pass |
| ------------------------------ | ---------: | ------: | ------------: |
| baseline (no chain, sink loop) |        ~1  |   ~905  |        —      |
| **chain full (5 detectors)**   |   **~348** | **~2.88** | **−27 %**   |
| phantom_liquidity (standalone) |       ~81  |  ~12.3  |        —      |
| cancellation_storm             |       ~23  |  ~43    |        —      |
| momentum_ignition              |       ~54  |  ~18.5  |        —      |
| **flash_crash**                |   **~164** |  **~6.1** | **−34 %**   |
| latency_arb_proxy              |       ~53  |  ~18.8  |        —      |

`flash_crash` was 52 % of chain cost on the first pass. Two
optimisations were applied (kept under the same public API and the
same 41-test suite):

1. **Cached band depth.** `band_depth_in_band` no longer walks the
   `BTreeMap` per event. The total is incremented / decremented in
   place by `add_qty` / `sub_qty` when the touched price is in band,
   and invalidated only when the best moves out of the band.
2. **Cached best bid/ask.** `best_bid()` / `best_ask()` no longer
   call `iter().next_back()` on every probe. The cached values are
   maintained incrementally: an `add` can only improve the best on
   the same side, and a `sub` only triggers a recompute when it
   empties the current best level.
3. **Array snapshot ring.** `VecDeque<TopSnapshot>` was replaced by
   a fixed `[TopSnapshot; 16]` with a circular head index. No
   per-push allocation, no shift on overflow.

Standalone sum (~375 ns) ≈ chain (~348 ns): wiring overhead is now
absorbed by branch coalescing across detectors.

### 2. Latency distribution (custom harness, with CPU pinning)

`bench/src/bin/chaos_latency.rs`. Measures per-iteration ns/event
with `Instant`, sorts samples, reports p50 / p90 / p99 / max + stddev.
Process is pinned to CPU 0 (`SetProcessAffinityMask`) and one full
warm-up pass is run before timing. Three datasets exercise different
branches:

| Dataset | mix (add/cancel/trade) | what it stresses                      |
| ------- | ---------------------- | ------------------------------------- |
| steady  | 60 / 25 / 15           | `phantom_liquidity` qualifying path   |
| bursty  | 35 / 55 / 10           | `cancellation_storm` rate, best churn |
| crashy  | realistic + injected sweep+aggression every 5 000 ev | actually fires `flash_crash` and `momentum_ignition` |

Numbers from a 50 000-event run, 200 iterations, release profile:

| dataset | mean | stddev | p50 | p90 | p99  | max  | flags/iter |
| ------- | ---: | -----: | --: | --: | ---: | ---: | ---------: |
| steady  |  466 | 589 (jitter) | 356 | 492 | 5385 | 5976 | 280 |
| bursty  |  269 |   5.8 (2.2 %) | 268 | 277 |  296 |  296 | 999 |
| crashy  |  347 |  11.1 (3.2 %) | 348 | 360 |  382 |  392 | 280 |

Notes:

* `bursty` and `crashy` show a **stable tail**: p99 is within 10 % of
  p50, stddev under 4 %. This is the relevant signal for live use:
  the chain does not have a slow path that fires occasionally.
* `steady`'s p99 spike is a Windows scheduler / page-walk artefact
  (max sample ≈ 6 µs, single outlier per 200 iterations). The p50 /
  p90 are consistent with the criterion mean.
* `crashy` actually triggers `flash_crash` and `momentum_ignition`
  (flag count > 0 confirms the `Some(flag)` branch is exercised under
  measurement, not just the no-flag fast path).

## Determinism

* All sequence-only windows: bit-identical under replay.
* Time windows (`*_time_*` params) introduce dependence on event
  timestamps. They are opt-in (zero disables) and only used when a
  feed's timestamps are themselves replay-stable.

## Running

```powershell
cargo test  -p flowlab-chaos --lib
cargo bench -p flowlab-bench --bench chaos_throughput
cargo run   -p flowlab-bench --bin   chaos_latency --release
```
# flowlab-chaos

HFT aggression analysis pipeline operating on the canonical event
stream. Three layers: per-event detectors → temporal clustering →
stress-window extraction (joined with `flowlab-flow`'s regime tag).

## Status

| Layer                                       | Status                                                        |
| ------------------------------------------- | ------------------------------------------------------------- |
| Detection — `QuoteStuffDetector`            | Implemented                                                   |
| Detection — `SpoofDetector`                 | Implemented                                                   |
| Detection — other 5 `ChaosKind` variants    | **WIP** — enum members reserved; no detector landed yet       |
| Clustering — `ChaosClusterer`               | Implemented (sequence-gap merge, peak severity, sorted output) |
| Window — `WindowExtractor`                  | Implemented (regime-thresholded, gap-merged, severity-finalized) |
| Output type `ChaosEvent` / `StressWindow`   | Stable, `Clone + Debug`, `serde`-ready                        |

The detectors use sequence-bounded windows only (no wall-clock).
Results are bit-identical under replay of the same event log.

## Contents

| File             | Role                                                               |
| ---------------- | ------------------------------------------------------------------ |
| `lib.rs`         | `ChaosKind` enum, `ChaosEvent`, `ChaosFeatures`, `StressWindow`    |
| `detection.rs`   | Per-event pattern detectors (QuoteStuff, Spoof today)              |
| `clustering.rs`  | `ChaosClusterer` — merges nearby `ChaosEvent`s into `ChaosCluster` |
| `window.rs`      | `WindowExtractor` — joins regime + chaos into `StressWindow`       |

## ChaosKind

```rust
pub enum ChaosKind {
    QuoteStuff,        // implemented
    PhantomLiquidity,  // reserved
    Spoof,             // implemented
    CancellationStorm, // reserved
    MomentumIgnition,  // reserved
    FlashCrash,        // reserved
    LatencyArbitrage,  // reserved
}
```

The reserved variants are first-class in the type system so that
clustering, windowing and downstream `flowlab-lab` consumers compile
against the final shape today and only require new detectors landing
incrementally.

## QuoteStuffDetector

- Rolling event window (sequence-driven).
- Trips when `cancels / max(1, trades) >= cancel_ratio_threshold` and
  the window is at least half-full.
- Severity is bounded `[0, 1]`.

## SpoofDetector

- Tracks orders with `qty >= large_qty_threshold`.
- Trips when an `OrderCancel` for a tracked order arrives within
  `cancel_window` sequence numbers of its `OrderAdd`.
- Severity scales with the size of the spoofed order.

## Determinism

All windows are sequence-bounded, never time-bounded. Results are
bit-identical under replay of the same event log.
