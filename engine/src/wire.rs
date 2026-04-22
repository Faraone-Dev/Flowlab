//! Telemetry wire protocol.
//!
//! ── Frame ──────────────────────────────────────────────────────────────
//!
//! ```text
//! +-------+---------+-----------+
//! | u32 LE | u16 LE  | payload  |
//! |  len   | version |   (bin)  |
//! +-------+---------+-----------+
//! ```
//!
//! `len`     = bytes of `version + payload` (NOT including the u32 itself)
//! `version` = wire protocol version. v1 = first release.
//! `payload` = bincode-encoded `TelemetryFrame`, or JSON if `--wire=json`.
//!
//! Versioning rule: any breaking change to `TelemetryFrame` (field rename,
//! removed variant, changed enum tag) MUST bump `WIRE_VERSION`. Additive
//! changes (new variant at the end of the enum, new optional field) MAY
//! ship without a bump but are discouraged — bump anyway, it's free.
//!
//! ── Honesty boundary ───────────────────────────────────────────────────
//! `event_time_ns` and `process_time_ns` are deliberately TWO fields, not
//! one. They MUST never be aliased downstream:
//!   - event_time_ns   : source-provided wall clock (ITCH ts48, Binance E)
//!   - process_time_ns : engine CLOCK_MONOTONIC at apply
//! Latency = process - event ONLY where the two clocks are comparable
//! (i.e. NOT for crypto WAN feeds without NTP discipline).

use serde::{Deserialize, Serialize};

pub const WIRE_VERSION: u16 = 1;

/// Top-level telemetry frame. Exhaustive enum so consumers can match.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TelemetryFrame {
    /// One-shot identifier sent immediately after connection accept.
    Header(Header),
    /// Per-tick microstructure snapshot.
    Tick(TickFrame),
    /// Periodic top-N book snapshot (for the dashboard ladder).
    Book(BookFrame),
    /// Risk-gate guard counters.
    Risk(RiskFrame),
    /// Latency histogram per stage (rolling window).
    Lat(LatFrame),
    /// Single trade print (aggressor-tagged). Emitted once per Trade event.
    Trade(TradeFrame),
    /// Heartbeat — empty payload, used to keep TCP alive on idle live feeds.
    Heartbeat,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Header {
    pub source: String,
    pub is_live: bool,
    pub started_at_ns: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instrument_id: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TickFrame {
    pub seq: u64,
    pub event_time_ns: u64,   // source clock (informational)
    pub process_time_ns: u64, // engine monotonic clock at apply
    pub mid_ticks: i64,
    pub spread_ticks: i64,
    /// Size-weighted mid (a.k.a. microprice) in ticks. More honest fair
    /// value than mid when top-of-book is asymmetric.
    #[serde(default)]
    pub microprice_ticks: i64,
    /// Spread in basis points: (ask-bid)/mid * 10_000. 0 if mid==0.
    #[serde(default)]
    pub spread_bps: f64,
    pub bid_depth: u64,
    pub ask_depth: u64,
    pub imbalance: f64,
    pub vpin: f64,
    pub trade_velocity: f64,
    pub regime: u8,
    pub events_per_sec: u64,
    /// Number of events dropped due to backpressure since stream start.
    pub dropped_total: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Level {
    pub price_ticks: u64,
    pub qty: u64,
    pub order_count: u32,
}

/// Top-N order book snapshot. The dashboard renders this as a ladder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BookFrame {
    pub seq: u64,
    pub bids: Vec<Level>, // descending price
    pub asks: Vec<Level>, // ascending price
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskFrame {
    pub halted: bool,
    pub reason: Option<String>,
    pub orders_sent: u64,
    pub trades_done: u64,
    pub otr_ratio: f64,
    pub otr_limit: f64,
    pub net_position: i64,
    pub position_limit: i64,
    pub cash_flow_ticks: i64,
    pub daily_loss_floor_ticks: i64,
    pub gaps_in_window: u32,
    pub gap_threshold: u32,
}

/// Per-stage latency stats. Stages mirror the engine pipeline:
///   parse → apply → analytics → risk → wire-out
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageLat {
    pub p50_ns: u64,
    pub p99_ns: u64,
    pub p999_ns: u64,
    pub max_ns: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatFrame {
    pub parse: StageLat,
    pub apply: StageLat,
    pub analytics: StageLat,
    pub risk: StageLat,
    pub wire_out: StageLat,
    /// Log-linear histogram bin counts (1ns ... 16ms, 192 bins).
    /// Same layout as bench/src/bin/latency_apply.rs.
    pub histo_apply: Vec<u64>,
}

/// One executed trade. Aggressor side is derived by comparing the print
/// price to top-of-book at apply time:
///   - `1` = aggressor BUY  (price >= best_ask)
///   - `-1` = aggressor SELL (price <= best_bid)
///   - `0` = inside spread / unknown
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeFrame {
    pub seq: u64,
    pub ts_ns: u64,
    pub price_ticks: u64,
    pub qty: u64,
    pub aggressor: i8,
}

/// Wire codec — bincode (binary, fast) or JSON (debug, human-readable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Bincode,
    Json,
}

impl Codec {
    pub fn parse(s: &str) -> Self {
        match s {
            "json" => Self::Json,
            _ => Self::Bincode,
        }
    }

    pub fn encode(self, frame: &TelemetryFrame) -> Result<Vec<u8>, String> {
        match self {
            Self::Bincode => bincode::serialize(frame).map_err(|e| e.to_string()),
            Self::Json => serde_json::to_vec(frame).map_err(|e| e.to_string()),
        }
    }
}
