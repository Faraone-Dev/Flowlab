pub mod detection;
pub mod clustering;
pub mod window;

use flowlab_core::types::SeqNum;

/// Kind of HFT aggression pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChaosKind {
    QuoteStuff,
    PhantomLiquidity,
    Spoof,
    CancellationStorm,
    MomentumIgnition,
    FlashCrash,
    LatencyArbitrage,
}

/// Structured output of chaos detection — testable, serializable, consumable by Lab.
#[derive(Debug, Clone)]
pub struct ChaosEvent {
    pub kind: ChaosKind,
    pub start_seq: SeqNum,
    pub end_seq: SeqNum,
    /// Normalized severity score [0.0, 1.0]
    pub severity: f64,
    /// Order ID that initiated the pattern (if identifiable)
    pub initiator: Option<u64>,
    pub features: ChaosFeatures,
}

/// Pattern-specific metrics.
#[derive(Debug, Clone)]
pub struct ChaosFeatures {
    /// Number of events in the pattern
    pub event_count: u64,
    /// Duration in nanoseconds (informational)
    pub duration_ns: u64,
    /// Cancel-to-trade ratio during the window
    pub cancel_trade_ratio: f64,
    /// Price displacement (ticks)
    pub price_displacement: i64,
    /// Depth removed (total qty cancelled/traded)
    pub depth_removed: u64,
}

/// Stress window — contiguous chaos segment for targeted replay.
#[derive(Debug, Clone)]
pub struct StressWindow {
    pub start_seq: SeqNum,
    pub end_seq: SeqNum,
    pub regime: flowlab_flow::regime::Regime,
    pub severity: f64,
    pub chaos_events: Vec<ChaosEvent>,
}
