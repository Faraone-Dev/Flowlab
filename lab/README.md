# flowlab-lab

Trait-based strategy sandbox. Strategies see a deterministic view of
the replayed book and emit `StrategyOrder` records; the executor
threads those records through the replay loop and tracks PnL,
inventory and drawdown.

## Status

| Component                       | Status                                                       |
| ------------------------------- | ------------------------------------------------------------ |
| `Strategy` trait                | Implemented — `on_event`, `on_fill`, `name`, `pnl`, `inventory` |
| `StrategyExecutor::run`         | Implemented for the bookkeeping path (PnL, drawdown, fills counter) |
| Simulated matching engine       | **WIP** — orders are emitted but not matched against the book yet |
| Latency / partial fills / queue position | **WIP** — `ExecutorConfig` flags exist, matching engine landing first |
| Reference strategies            | Not provided — bring your own `impl Strategy`                |

The `// TODO: simulated matching engine (FIFO, latency, partial fills)`
marker in `executor.rs` is the single integration point that will turn
this from a sandbox skeleton into a runnable simulation.

## Contents

| File           | Role                                                      |
| -------------- | --------------------------------------------------------- |
| `strategy.rs`  | `Strategy` trait, `StrategyOrder`, `Fill`                 |
| `executor.rs`  | `StrategyExecutor` + `ExecutorConfig`                     |
| `report.rs`    | `StressReport` (PnL, drawdown, fills, inventory, events)  |

## Strategy trait

```rust
pub trait Strategy {
    fn on_event(&mut self, event: &SequencedEvent, book: &OrderBook) -> Vec<StrategyOrder>;
    fn on_fill(&mut self, fill: &Fill);
    fn name(&self) -> &str;
    fn pnl(&self) -> f64;
    fn inventory(&self) -> i64;
}
```

`on_event` is called per replayed event with read-only access to the
current book. The strategy returns the orders it would submit. Once
the matching engine lands, `on_fill` will be invoked synchronously
inside the same replay loop.

## Scope

Replayed data only. No live execution, ever. This is a research
sandbox — outputs are analytical, not operational.
