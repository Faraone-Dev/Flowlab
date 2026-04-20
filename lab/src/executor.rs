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
pub struct StrategyExecutor {
    #[allow(dead_code)] // wired in upcoming partial-fill / latency simulation
    config: ExecutorConfig,
    fills: Vec<Fill>,
    max_drawdown: f64,
    peak_pnl: f64,
}

impl StrategyExecutor {
    pub fn new(config: ExecutorConfig) -> Self {
        Self {
            config,
            fills: Vec::new(),
            max_drawdown: 0.0,
            peak_pnl: 0.0,
        }
    }

    /// Run a strategy against a sequence of events.
    pub fn run(
        &mut self,
        strategy: &mut dyn Strategy,
        events: &[SequencedEvent],
        book: &mut OrderBook,
    ) -> StressReport {
        for seq_event in events {
            book.apply(&seq_event.event);
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

        StressReport {
            strategy_name: strategy.name().to_string(),
            events_processed: events.len() as u64,
            final_pnl: strategy.pnl(),
            max_drawdown: self.max_drawdown,
            final_inventory: strategy.inventory(),
            fills: self.fills.len() as u64,
        }
    }
}
