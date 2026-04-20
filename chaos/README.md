# flowlab-chaos

Three-layer HFT aggression detector operating on the canonical event
stream. Pure-Rust today; hot kernels will migrate to `hotpath/` C++
behind the stable `#[repr(C)]` ABI.

## Layers

### 1. Detection (`detection.rs`)

Pattern recognizers emitting `ChaosEvent` records.

| Pattern              | Signal                                              |
| -------------------- | --------------------------------------------------- |
| Quote stuffing       | Add/cancel burst < 1 ms, cancel/trade ratio > 50:1 |
| Phantom liquidity    | Orders added and removed before executable         |
| Spoofing             | Large orders systematically placed and pulled      |
| Cancellation storm   | Synchronized cancels across multiple levels        |
| Momentum ignition    | Aggressive sequences triggering cascades           |
| Flash-crash signature| Depth depletion → price gap → cascade              |
| Latency arbitrage    | Trades systematically preceding price moves        |

### 2. Clustering (`clustering.rs`)

Sub-millisecond microburst grouping, cross-level correlation,
temporal clustering of aggressive sequences.

### 3. Causality (`window.rs`)

Initiator / responder attribution within a detected window, impact
propagation, recovery time.

## Output

```rust
pub struct ChaosEvent {
    pub kind: ChaosKind,
    pub start_seq: u64,
    pub end_seq: u64,
    pub severity: f64,            // 0.0 — 1.0
    pub initiator: Option<u64>,   // order_id
    pub features: ChaosFeatures,
}
```

Serializable via `serde`, directly consumable by `flowlab-lab`.

## Determinism

All windows are sequence-bounded, never time-bounded. Results are
bit-identical under replay of the same event log.
