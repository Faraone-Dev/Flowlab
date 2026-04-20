# flowlab-lab

Trait-based strategy sandbox. Strategies see a deterministic view of
the replayed book and submit orders through a simulated execution
gateway with configurable latency and queue-position modelling.

## Contents

| File           | Role                                                     |
| -------------- | -------------------------------------------------------- |
| `strategy.rs`  | `Strategy` trait: `on_event`, `on_fill`, `on_regime`     |
| `executor.rs`  | Simulated matching engine, FIFO price-time priority      |
| `report.rs`    | PnL, slippage, inventory, drawdown-by-regime aggregator  |

## Execution model

| Parameter                 | Default            | Configurable |
| ------------------------- | ------------------ | ------------ |
| Latency model             | zero (instant)     | ns-resolution |
| Matching                  | FIFO (price-time)  | yes          |
| Partial fills             | supported          | yes          |
| Queue position estimation | off                | yes          |
| Fill notification         | synchronous        | —            |

## Metrics under stress

Strategies run against stress windows extracted by `flowlab-flow`'s
`regime` module. Reported metrics:

| Metric                    | Captures                                      |
| ------------------------- | --------------------------------------------- |
| Slippage under stress     | Execution quality degradation                 |
| Inventory risk            | Position accumulation in adverse regimes      |
| Max drawdown per regime   | Worst-case loss by market state               |
| Recovery time             | Time to return to baseline PnL                |
| Fill-rate degradation     | Fill behaviour under extreme conditions       |

## Scope

Replayed data only. No live execution, ever. This is a research
sandbox — outputs are analytical, not operational.
