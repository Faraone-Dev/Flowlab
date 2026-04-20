use flowlab_core::event::SequencedEvent;
use flowlab_core::orderbook::OrderBook;
use flowlab_core::types::{OrderId, Price, Qty};

/// Order submitted by a strategy.
#[derive(Debug, Clone)]
pub struct StrategyOrder {
    pub side: flowlab_core::event::Side,
    pub price: Price,
    pub qty: Qty,
}

/// Fill received by a strategy.
#[derive(Debug, Clone)]
pub struct Fill {
    pub order_id: OrderId,
    pub price: Price,
    pub qty: Qty,
    pub side: flowlab_core::event::Side,
}

/// Trait for pluggable research strategies.
pub trait Strategy {
    /// Called on each replayed event with current book state.
    /// Returns orders to submit (if any).
    fn on_event(&mut self, event: &SequencedEvent, book: &OrderBook) -> Vec<StrategyOrder>;

    /// Called when a fill is received.
    fn on_fill(&mut self, fill: &Fill);

    /// Strategy name for reporting.
    fn name(&self) -> &str;

    /// Current PnL (strategy tracks internally).
    fn pnl(&self) -> f64;

    /// Current inventory (net position).
    fn inventory(&self) -> i64;
}
