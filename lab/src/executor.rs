use flowlab_chaos::{ChaosChain, ChaosEvent, ChaosKind};
use flowlab_core::event::SequencedEvent;
use flowlab_core::orderbook::OrderBook;
use crate::strategy::{Fill, Strategy};
use crate::report::StressReport;

/// Configuration for the simulated execution environment.
#[derive(Debug, Clone)]
pub struct ExecutorConfig {
    /// Simulated latency in nanoseconds (0 = instant)
    pub latency_ns: u64,
    /// Enable partial fills
    pub partial_fills: bool,
    /// Enable queue position estimation
    pub queue_position: bool,
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            latency_ns: 0,
            partial_fills: true,
            queue_position: false,
        }
    }
}

/// Drives a strategy against replayed events with simulated execution.
///
/// Optionally runs a `ChaosChain` in parallel with the strategy. The
/// chain observes the *same* event stream the strategy sees, in the
/// same order, so flagged anomalies are causally aligned with strategy
/// decisions in the resulting `StressReport`.
///
/// ## Event-ordering contract
///
/// For each `SequencedEvent` we apply, in this order:
///   1. `book.apply(&event)` — orderbook view becomes consistent
///   2. `chaos_chain.process(&event)` — chaos detectors observe the
///      event using the post-apply view (matches their internal
///      mini-book semantics)
///   3. `strategy.on_event(&event, &book)` — strategy reacts
///   4. PnL bookkeeping
///
/// Steps 2 and 3 are independent (chain has its own state); we keep
/// chaos before strategy so a future "chaos-aware" strategy can read
/// flags via shared state without re-ordering the call site.
pub struct StrategyExecutor {
    #[allow(dead_code)] // wired in upcoming partial-fill / latency simulation
    config: ExecutorConfig,
    fills: Vec<Fill>,
    max_drawdown: f64,
    peak_pnl: f64,
    chaos_chain: Option<ChaosChain>,
}

impl StrategyExecutor {
    pub fn new(config: ExecutorConfig) -> Self {
        Self {
            config,
            fills: Vec::new(),
            max_drawdown: 0.0,
            peak_pnl: 0.0,
            chaos_chain: None,
        }
    }

    /// Attach a chaos detection chain to this executor. Subsequent
    /// `run` calls will populate `StressReport::chaos_events` and
    /// `chaos_counts`. Building blocks for the dashboard's
    /// "strategy-vs-chaos" overlay.
    pub fn with_chaos_chain(mut self, chain: ChaosChain) -> Self {
        self.chaos_chain = Some(chain);
        self
    }

    /// Run a strategy against a sequence of events.
    pub fn run(
        &mut self,
        strategy: &mut dyn Strategy,
        events: &[SequencedEvent],
        book: &mut OrderBook,
    ) -> StressReport {
        let mut chaos_events: Vec<ChaosEvent> = Vec::new();

        for seq_event in events {
            book.apply(&seq_event.event);

            if let Some(chain) = self.chaos_chain.as_mut() {
                let flags = chain.process(seq_event);
                if !flags.is_empty() {
                    chaos_events.extend(flags);
                }
            }

            let _orders = strategy.on_event(seq_event, book);

            // TODO: simulated matching engine (FIFO, latency, partial fills)

            // Track PnL metrics
            let pnl = strategy.pnl();
            if pnl > self.peak_pnl {
                self.peak_pnl = pnl;
            }
            let drawdown = self.peak_pnl - pnl;
            if drawdown > self.max_drawdown {
                self.max_drawdown = drawdown;
            }
        }

        // Snapshot chain counters into the report. We materialise the
        // five known kinds even when count==0 so consumers can align
        // multiple reports on a fixed schema.
        let chaos_counts = if let Some(chain) = self.chaos_chain.as_ref() {
            vec![
                (ChaosKind::PhantomLiquidity, chain.count(ChaosKind::PhantomLiquidity)),
                (ChaosKind::CancellationStorm, chain.count(ChaosKind::CancellationStorm)),
                (ChaosKind::MomentumIgnition, chain.count(ChaosKind::MomentumIgnition)),
                (ChaosKind::FlashCrash, chain.count(ChaosKind::FlashCrash)),
                (ChaosKind::LatencyArbitrage, chain.count(ChaosKind::LatencyArbitrage)),
            ]
        } else {
            Vec::new()
        };

        StressReport {
            strategy_name: strategy.name().to_string(),
            events_processed: events.len() as u64,
            final_pnl: strategy.pnl(),
            max_drawdown: self.max_drawdown,
            final_inventory: strategy.inventory(),
            fills: self.fills.len() as u64,
            chaos_events,
            chaos_counts,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategy::Strategy;
    use flowlab_chaos::ChaosChain;
    use flowlab_core::event::{Event, EventType, SequencedEvent, Side};
    use flowlab_core::orderbook::OrderBook;

    /// No-op strategy: never trades, never has PnL or inventory.
    /// Used here as a transparent passthrough so the test reasons only
    /// about chaos-chain wiring.
    struct NoopStrat;

    impl Strategy for NoopStrat {
        fn name(&self) -> &str {
            "noop"
        }
        fn on_event(
            &mut self,
            _ev: &SequencedEvent,
            _book: &OrderBook,
        ) -> Vec<crate::strategy::StrategyOrder> {
            Vec::new()
        }
        fn on_fill(&mut self, _fill: &crate::strategy::Fill) {}
        fn pnl(&self) -> f64 {
            0.0
        }
        fn inventory(&self) -> i64 {
            0
        }
    }

    fn ev(
        seq: u64,
        ts: u64,
        etype: EventType,
        side: Side,
        price: u64,
        qty: u64,
        order_id: u64,
    ) -> SequencedEvent {
        SequencedEvent {
            seq,
            channel_id: 0,
            event: Event {
                ts,
                price,
                qty,
                order_id,
                instrument_id: 1,
                event_type: etype as u8,
                side: side as u8,
                _pad: [0; 2],
            },
        }
    }

    /// Without a chain attached, the report has empty chaos fields and
    /// the executor behaves exactly like the original implementation.
    #[test]
    fn no_chain_means_empty_chaos_fields() {
        let mut exec = StrategyExecutor::new(ExecutorConfig::default());
        let mut book = OrderBook::new(1);
        let mut strat = NoopStrat;
        let events: Vec<SequencedEvent> = (1u64..=20)
            .map(|i| ev(i, i * 1_000, EventType::OrderAdd, Side::Bid, 10_000, 100, i))
            .collect();

        let report = exec.run(&mut strat, &events, &mut book);
        assert_eq!(report.events_processed, 20);
        assert!(report.chaos_events.is_empty());
        assert!(report.chaos_counts.is_empty());
    }

    /// With a chain attached, a crafted phantom-liquidity cycle
    /// surfaces in `report.chaos_events` and the per-kind counts
    /// reflect the same flag.
    #[test]
    fn chain_attached_surfaces_phantom_event() {
        let mut exec = StrategyExecutor::new(ExecutorConfig::default())
            .with_chaos_chain(ChaosChain::default_itch());
        let mut book = OrderBook::new(1);
        let mut strat = NoopStrat;

        // Big add then quick full cancel = textbook phantom cycle.
        // Same order_id on both events so the phantom detector links
        // the cancel back to the original add.
        let events = vec![
            ev(1, 1_000, EventType::OrderAdd, Side::Bid, 10_000, 1_000, 42),
            ev(5, 5_000, EventType::OrderCancel, Side::Bid, 10_000, 1_000, 42),
        ];

        let report = exec.run(&mut strat, &events, &mut book);
        assert_eq!(report.events_processed, 2);
        assert_eq!(report.chaos_counts.len(), 5, "all five kinds reported");
        let phantom_count = report
            .chaos_counts
            .iter()
            .find(|(k, _)| *k == ChaosKind::PhantomLiquidity)
            .map(|(_, c)| *c)
            .unwrap_or(0);
        assert!(
            phantom_count >= 1,
            "phantom liquidity should fire: counts={:?}",
            report.chaos_counts
        );
        assert!(
            report
                .chaos_events
                .iter()
                .any(|e| e.kind == ChaosKind::PhantomLiquidity),
            "phantom event missing from report.chaos_events"
        );
    }
}
