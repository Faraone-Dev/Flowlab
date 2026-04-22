# flowlab-engine

Runtime binary that drives a pluggable `Source` of market events through
the deterministic analytics pipeline (`HotOrderBook`, VPIN, spread,
imbalance, regime classifier, circuit breaker) and publishes telemetry
on a versioned TCP socket.

This crate is the boundary between **deterministic core** (Rust crates
in `core/`, `flow/`, `replay/`) and the **outside world** (Go bridge,
React dashboard, eventually live exchange feeds).

## Topology

```
Source ──► HotOrderBook ──► analytics ──► (risk probe) ──► wire-out
   │                          │
   │                          └─► VPIN, imbalance, spread, regime
   │
   └─► synthetic | ich (WIP) | binance (WIP)

                              bounded mpsc(8192)
                              drop counter exposed in Tick.dropped_total
                                       │
                                       ▼
                            TCP :9090 (bincode default | --wire=json)
                            [u32 len LE][u16 ver=1 LE][payload]
```

Single-threaded by default. The Source runs on the main thread; the TCP
telemetry server runs on its own thread and consumes the bounded mpsc.
Producer never blocks: on overflow it drops and bumps a counter.

## Source trait

```rust
pub trait Source: Send {
    fn name(&self) -> &str;
    fn next(&mut self) -> Option<Event>;
    fn is_live(&self) -> bool { false }
}
```

| Source            | Status   | Notes                                                     |
| ----------------- | -------- | --------------------------------------------------------- |
| `SyntheticSource` | Live     | Deterministic ADD/CANCEL/TRADE mix from a seed.           |
| `IchSource`       | Live     | mmap, zero-copy, BinaryFile / Unframed framing, fast-forward by ts48, optional realtime pacing (busy-spin), looping replay. Reuses the validated parser in `flowlab-replay`. |
| `BinanceSource`   | WIP      | Live WS. Engine-internal latency only — WAN declared separately. |

## Wire protocol

See [`src/wire.rs`](src/wire.rs).

```
+--------+----------+------------------+
| u32 LE |  u16 LE  |   payload (N B)  |
|  len   | version  |  bincode | json  |
+--------+----------+------------------+

len = 2 + payload.len()
version starts at 1; any breaking schema change bumps it.
```

`TelemetryFrame`:

```rust
enum TelemetryFrame {
    Header(Header),       // sent once on connect
    Tick(TickFrame),      // microstructure snapshot, ~tick_hz
    Book(BookFrame),      // top-N ladder, ~book_hz
    Risk(RiskFrame),      // breaker + counters
    Lat(LatFrame),        // per-stage p50/p99/p99.9/max + apply histogram
    Heartbeat,            // empty, keeps live TCP from going silent
}
```

`--wire=json` switches the codec to JSON; same framing. Useful for
debugging the bridge without writing a bincode decoder.

## Clocks (deliberately separate)

```
event_time_ns    source clock (ITCH ts48, Binance E)
process_time_ns  engine CLOCK_MONOTONIC at apply
latency_ns       process - event   (only where comparable)
```

The two clocks **never** alias. ITCH replay can claim nanosecond
latencies because both clocks are in-process; Binance live cannot,
because event_time_ns is whatever the exchange wrote and WAN sits in
between. The dashboard renders the two separately.

## Latency stages

`LatFrame` exposes per-stage stats over a rolling window:

```
parse  →  apply  →  analytics  →  risk  →  wire-out
```

`apply` also publishes a 192-bin log-linear histogram (1 ns ... 16 ms)
suitable for log-scale rendering on the dashboard.

## Backpressure

`backpressure::channel(capacity)` returns a `(Producer, Consumer)` pair
backed by `crossbeam_channel::bounded`. The producer's `try_send`:

- succeeds → frame queued.
- channel full → frame dropped, `dropped_total` atomic counter +1.
- consumer gone → silent drop (counter is meaningless then).

`Tick.dropped_total` carries the running counter so the dashboard can
show "the engine is shedding load" instead of the system silently
freezing.

## Run

```bash
# bincode (default, production)
cargo run -p flowlab-engine --release -- \
  --source synthetic --listen 127.0.0.1:9090 --tick-hz 50

# JSON (debug — pipe through any TCP client)
cargo run -p flowlab-engine --release -- \
  --source synthetic --wire json --listen 127.0.0.1:9090 --tick-hz 50
```

CLI flags:

| Flag           | Default            | Meaning                                          |
| -------------- | ------------------ | ------------------------------------------------ |
| `--source`     | `synthetic`        | `synthetic` \| `ich` \| `binance` (WIP)          |
| `--source-arg` | `""`               | Path / symbol / side info for the source         |
| `--seed`       | `0xC0FFEE`         | Deterministic seed for `SyntheticSource`         |
| `--listen`     | `127.0.0.1:9090`   | TCP listen address                               |
| `--wire`       | `bincode`          | `bincode` \| `json`                              |
| `--tick-hz`    | `50`               | Tick / Risk / Lat publish rate                   |
| `--book-hz`    | `10`               | Book ladder publish rate                         |
| `--capacity`   | `8192`             | Bounded mpsc capacity (drop on overflow)         |

## Tests

The pipeline currently relies on the upstream test suites in
`flowlab-core`, `flowlab-flow`, `flowlab-replay`. Crate-local
integration covers `IchSource` end-to-end:

- `tests/ich_real.rs` — drains a real BinaryFILE-framed ITCH dump for
  3 s (path from the `FLOWLAB_ITCH_FILE` env var; skipped when unset
  so CI on machines without the multi-GB file stays green) and
  asserts `events > 1_000`, `unknown_msgs < 1%`, and ITCH ts48
  monotonicity.

`Source` mock, wire round-trip and drop-counter unit tests are still
pending and land alongside `BinanceSource`.

## Honesty boundary

- `synthetic` and `ich`: nanosecond-grade claims allowed, both clocks
  are in-process and comparable.
- `binance` (when wired): only `process_time_ns` deltas inside the
  engine are HFT-grade. End-to-end latency includes WAN, NTP drift and
  exchange queueing — declared and rendered separately. Never blurred.
