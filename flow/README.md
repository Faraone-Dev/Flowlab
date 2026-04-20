# flowlab-flow

Microstructure analytics and the pre-trade risk gate. Stateless signal
processors operating on the replayed event stream; everything lives in
pre-allocated rolling windows.

## Modules

| Module              | Output                                                   |
| ------------------- | -------------------------------------------------------- |
| `imbalance.rs`      | Bid/ask volume asymmetry per level                       |
| `spread.rs`         | Spread evolution, mean reversion features                |
| `vpin.rs`           | Volume-synchronized probability of informed trading      |
| `impact.rs`         | Price impact per unit of executed volume                 |
| `regime.rs`         | HMM-lite classifier: Calm / Volatile / Aggressive / Crisis |
| `circuit_breaker.rs`| Fail-closed pre-trade risk gate (6 guards)               |

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

## Execution contract

- No allocations inside hot signal loops once warmed up.
- All ring buffers sized via `Vec::with_capacity` at construction.
- Outputs are `Copy` types; callers never hold borrows across events.

## Tests

`cargo test -p flowlab-flow`: 8 circuit-breaker unit tests + module
tests.
