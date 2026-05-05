// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

//! Telemetry wire protocol.
//!
//! Frame: `[u32 LE len][u16 LE version][payload]`. `len` excludes itself.
//! `payload` = bincode `TelemetryFrame` (or JSON if `--wire=json`).
//!
//! Bump `WIRE_VERSION` on any breaking `TelemetryFrame` change
//! (rename, removed variant, retagged enum). Additive changes may
//! ship without bump but bumping is free — prefer it.
//!
//! `event_time_ns` and `process_time_ns` are deliberately separate:
//!   - event_time_ns   : source clock (ITCH ts48, Binance E)
//!   - process_time_ns : engine CLOCK_MONOTONIC at apply
//! Latency = process - event ONLY where both clocks are comparable
//! (NOT for crypto WAN feeds without NTP discipline).

use serde::{Deserialize, Serialize};

pub const WIRE_VERSION: u16 = 2;

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
    /// Chaos detection flag — one per event that triggers a detector.
    /// Emitted immediately after the event is applied (not at tick cadence).
    Chaos(ChaosFrame),
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

/// Chaos detection flag emitted when a detector triggers.
/// Mirrors `ChaosKind` + `ChaosEvent` from the chaos crate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChaosFrame {
    /// Engine sequence number at the triggering event.
    pub seq: u64,
    /// Human-readable kind label, e.g. "PhantomLiquidity", "FlashCrash".
    pub kind: String,
    /// Normalized severity [0.0, 1.0].
    pub severity: f64,
    /// Sequence range of the detected pattern.
    pub start_seq: u64,
    pub end_seq: u64,
    /// Order ID that initiated the pattern (if identifiable).
    pub initiator: Option<u64>,
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
