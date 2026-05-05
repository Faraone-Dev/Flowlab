// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

use std::collections::VecDeque;
use flowlab_core::event::{EventType, SequencedEvent};
use flowlab_core::types::SeqNum;
use crate::{ChaosEvent, ChaosFeatures, ChaosKind};

/// Quote stuffing detector — tracks add/cancel burst rates.
pub struct QuoteStuffDetector {
    /// Rolling window of (seq, ts_ns, event_type) for burst detection.
    /// VecDeque for O(1) pop_front — Vec::remove(0) is O(n).
    window: VecDeque<(SeqNum, u64, u8)>,
    /// Max events in detection window
    window_size: usize,
    /// Cancel/trade ratio threshold
    cancel_ratio_threshold: f64,
}

impl QuoteStuffDetector {
    pub fn new(window_size: usize, cancel_ratio_threshold: f64) -> Self {
        Self {
            window: VecDeque::with_capacity(window_size),
            window_size,
            cancel_ratio_threshold,
        }
    }

    /// Feed an event. Returns ChaosEvent if quote stuffing detected.
    pub fn process(&mut self, seq_event: &SequencedEvent) -> Option<ChaosEvent> {
        let event = &seq_event.event;
        let etype = event.event_type;

        // Only track add/cancel/trade
        if !matches!(
            EventType::from_u8(etype),
            Some(EventType::OrderAdd | EventType::OrderCancel | EventType::Trade)
        ) {
            return None;
        }

        if self.window.len() >= self.window_size {
            self.window.pop_front();
        }
        self.window.push_back((seq_event.seq, event.ts, etype));

        // Check burst: count cancels vs trades in window
        let cancels = self
            .window
            .iter()
            .filter(|(_, _, t)| *t == EventType::OrderCancel as u8)
            .count() as f64;
        let trades = self
            .window
            .iter()
            .filter(|(_, _, t)| *t == EventType::Trade as u8)
            .count()
            .max(1) as f64;

        let ratio = cancels / trades;

        if ratio >= self.cancel_ratio_threshold && self.window.len() >= self.window_size / 2 {
            let (start_seq, start_ts) = self
                .window
                .front()
                .map(|(s, t, _)| (*s, *t))
                .unwrap_or((0, 0));
            let end_seq = seq_event.seq;

            Some(ChaosEvent {
                kind: ChaosKind::QuoteStuff,
                start_seq,
                end_seq,
                start_ts_ns: start_ts,
                end_ts_ns: event.ts,
                severity: (ratio / self.cancel_ratio_threshold / 2.0).min(1.0),
                initiator: None,
                features: ChaosFeatures {
                    event_count: self.window.len() as u64,
                    duration_ns: event.ts.saturating_sub(start_ts),
                    cancel_trade_ratio: ratio,
                    price_displacement: 0,
                    depth_removed: 0,
                },
            })
        } else {
            None
        }
    }
}

/// Spoofing detector — tracks large orders placed and pulled.
pub struct SpoofDetector {
    /// Track large orders: (seq, ts_ns, order_id, qty)
    large_orders: Vec<(SeqNum, u64, u64, u64)>,
    /// Minimum qty to consider "large"
    large_qty_threshold: u64,
    /// How many sequence numbers before a cancel is suspicious
    cancel_window: u64,
}

impl SpoofDetector {
    pub fn new(large_qty_threshold: u64, cancel_window: u64) -> Self {
        Self {
            large_orders: Vec::new(),
            large_qty_threshold,
            cancel_window,
        }
    }

    pub fn process(&mut self, seq_event: &SequencedEvent) -> Option<ChaosEvent> {
        let event = &seq_event.event;
        let etype = EventType::from_u8(event.event_type)?;

        match etype {
            EventType::OrderAdd if event.qty >= self.large_qty_threshold => {
                self.large_orders
                    .push((seq_event.seq, event.ts, event.order_id, event.qty));
                // Prune old entries
                self.large_orders
                    .retain(|(s, _, _, _)| seq_event.seq - s < self.cancel_window * 2);
                None
            }
            EventType::OrderCancel => {
                // Check if this cancels a recently placed large order
                if let Some(pos) = self
                    .large_orders
                    .iter()
                    .position(|(_, _, oid, _)| *oid == event.order_id)
                {
                    let (add_seq, add_ts, _, qty) = self.large_orders.remove(pos);
                    if seq_event.seq - add_seq <= self.cancel_window {
                        return Some(ChaosEvent {
                            kind: ChaosKind::Spoof,
                            start_seq: add_seq,
                            end_seq: seq_event.seq,
                            start_ts_ns: add_ts,
                            end_ts_ns: event.ts,
                            severity: (qty as f64 / self.large_qty_threshold as f64 / 5.0)
                                .min(1.0),
                            initiator: Some(event.order_id),
                            features: ChaosFeatures {
                                event_count: 2,
                                duration_ns: event.ts.saturating_sub(add_ts),
                                cancel_trade_ratio: 0.0,
                                price_displacement: 0,
                                depth_removed: qty,
                            },
                        });
                    }
                }
                None
            }
            _ => None,
        }
    }
}
