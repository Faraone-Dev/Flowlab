# flowlab-flow

Microstructure analytics and the pre-trade risk gate. Stateless
signal processors operating on the replayed event stream; rolling
windows are pre-allocated and `O(1)`-amortized per update.

## Status

| Module               | Status                                                                |
| -------------------- | --------------------------------------------------------------------- |
| `imbalance.rs`       | Implemented — top-N book imbalance + depth snapshot                   |
| `spread.rs`          | Implemented — `SpreadTracker` rolling mean + blowout ratio            |
| `vpin.rs`            | Implemented — `VpinCalculator` with `O(1)` running sum across buckets |
| `regime.rs`          | Implemented — threshold-based composite-score classifier              |
| `circuit_breaker.rs` | Implemented — 6 guards, fail-closed latch, 7 unit tests               |

## Modules

| Module               | Output                                                       |
| -------------------- | ------------------------------------------------------------ |
| `imbalance.rs`       | `book_imbalance(book, levels) -> f64` in `[-1.0, 1.0]`       |
| `spread.rs`          | `SpreadMetrics { current, mean, blowout_ratio }`             |
| `vpin.rs`            | `vpin() -> f64` updated when a volume bucket completes       |
| `regime.rs`          | `Regime::{Calm, Volatile, Aggressive, Crisis}`               |
| `circuit_breaker.rs` | `Decision::{Allow, Block(HaltReason)}`                       |

## Circuit breaker

The breaker is the last line of defence before any order reaches the
wire. All outbound paths MUST call `CircuitBreaker::check(&Intent)`
and respect the returned `Decision`.

| Guard             | Trip condition                                          |
| ----------------- | ------------------------------------------------------- |
| Rate limit        | Token bucket (orders/sec)                               |
| Position cap      | `abs(net_pos + order_qty) > max_position`               |
| Daily loss floor  | `cash_flow_ticks < -max_daily_loss_ticks`               |
| OTR ceiling       | `orders / max(1, trades) > max_otr` (after warmup)      |
| Feed gap          | `gaps_within(window) >= gap_threshold`                  |
| Manual kill       | Operator latch                                          |

Once tripped the breaker latches. Recovery is a deliberate
`reset()` or `start_of_day()` call — never automatic.

## Regime classifier

Composite stress score from 5 inputs:

```
score = max(spread_blowout - 1, 0)
      + |book_imbalance| * 2
      + vpin * 5
      + max(trade_velocity - 1, 0)
      + depth_depletion * 3
```

Default thresholds: `1.5` Volatile, `3.0` Aggressive, `6.0` Crisis.
The classifier is intentionally simple and threshold-tunable; it does
**not** claim HMM or Bayesian semantics.

## Execution contract

- All rolling windows are sized via `Vec::with_capacity` /
  `VecDeque::with_capacity` at construction.
- Outputs are `Copy` types; callers never hold borrows across events.
- VPIN, impact, and spread all maintain a running sum to make the
  per-event update `O(1)` regardless of window size.

## Tests

`cargo test -p flowlab-flow`: 7 circuit-breaker unit tests covering
default-allow, rate-limit latch, position cap, daily-loss floor,
OTR-after-warmup, feed-gap threshold, and manual halt + recovery.
