// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

//! Episode-aware clustering of [`ChaosEvent`]s.
//!
//! Goal: turn a stream of single-flag detector outputs into a stream of
//! *semantic episodes* a dashboard or report can consume without drowning
//! in near-duplicate flags.
//!
//! ## Merge policy (kind + initiator aware)
//!
//! The previous clusterer merged any two events whose seq distance was
//! below a single `max_gap`. That treats two unrelated `PhantomLiquidity`
//! flags from different `order_id`s as one episode, and refuses to merge a
//! `FlashCrash` with the `MomentumIgnition` that triggered it (the two
//! detectors are explicitly anti-overlap'd at the detector level — once
//! they fire, they describe one event).
//!
//! The current policy is:
//!
//! 1. **Same kind, same initiator**         → always merge if seq-close.
//! 2. **Different initiator, same kind**    → never merge (different actors).
//! 3. **Crash ↔ Ignition** (related kinds)  → merge if seq-close, regardless
//!    of initiator (the price move is the shared substrate).
//! 4. Otherwise                             → never merge.
//!
//! "Seq-close" means `event.start_seq <= current.end_seq + max_gap`.
//!
//! ## Limits (honest)
//!
//! * Pure seq-window merge — feeds with bursty latency can drift and split
//!   one episode into two. The seq+time dual window lives in a separate
//!   commit; this file does not own that decision.
//! * Initiators are compared by `Option<u64>` equality. Two `None`
//!   initiators do **not** count as "same" — `None` means "unknown",
//!   not "anonymous shared actor".

use crate::{ChaosEvent, ChaosKind};
use flowlab_core::types::SeqNum;

/// Episode clusterer for [`ChaosEvent`] streams.
pub struct ChaosClusterer {
    /// Maximum sequence gap between two events to be considered the same
    /// episode under the merge policy described at module level.
    max_gap: u64,
}

impl ChaosClusterer {
    pub fn new(max_gap: u64) -> Self {
        Self { max_gap }
    }

    /// Cluster events into semantic episodes.
    ///
    /// Input is borrowed and not mutated; events are sorted by
    /// `start_seq` internally so callers may pass any order.
    pub fn cluster(&self, events: &[ChaosEvent]) -> Vec<ChaosCluster> {
        if events.is_empty() {
            return Vec::new();
        }

        let mut sorted: Vec<&ChaosEvent> = events.iter().collect();
        sorted.sort_by_key(|e| e.start_seq);

        let mut clusters: Vec<ChaosCluster> = Vec::new();
        let mut current = ChaosCluster::from_event(sorted[0]);

        for event in &sorted[1..] {
            if self.should_merge(&current, event) {
                current.absorb(event);
            } else {
                clusters.push(current);
                current = ChaosCluster::from_event(event);
            }
        }
        clusters.push(current);

        clusters
    }

    /// Merge decision — see module-level policy table.
    fn should_merge(&self, current: &ChaosCluster, event: &ChaosEvent) -> bool {
        let seq_close = event.start_seq <= current.end_seq + self.max_gap;
        if !seq_close {
            return false;
        }
        match (current.dominant_kind, event.kind) {
            // Rule 3: crash ↔ ignition share the underlying price move.
            (ChaosKind::FlashCrash, ChaosKind::MomentumIgnition)
            | (ChaosKind::MomentumIgnition, ChaosKind::FlashCrash) => true,

            // Rule 1 + 2: same kind merges only if same (known) initiator.
            (a, b) if a == b => match (current.initiator, event.initiator) {
                (Some(x), Some(y)) => x == y,
                _ => false,
            },

            // Rule 4: anything else stays separate.
            _ => false,
        }
    }
}

/// A semantic episode built from one or more [`ChaosEvent`]s.
///
/// Three severity scalars are exposed because each one answers a
/// different question and they are not interchangeable:
///
/// * `peak_severity` — the worst single flag in the episode. Useful for
///   triage ("how bad did it get at the worst instant?").
/// * `mean_severity` — average per-event severity. Useful when the
///   episode is long and you want to know if it stayed bad or just
///   spiked once.
/// * `composite_severity` — `peak * (1 + ln(count))`. Combines worst
///   instant with persistence so a long sustained episode ranks above a
///   single isolated spike of the same peak. This is the field the
///   dashboard should sort by when ranking episodes.
///
/// `peak_severity` is the only one preserved from the previous schema;
/// the other two are additive and don't break existing consumers.
#[derive(Debug, Clone)]
pub struct ChaosCluster {
    pub start_seq: SeqNum,
    pub end_seq: SeqNum,
    pub events: Vec<ChaosEvent>,
    pub peak_severity: f64,
    pub mean_severity: f64,
    pub composite_severity: f64,
    pub count: usize,
    /// Kind of the *first* event in the cluster — used by the merge
    /// policy to decide if a follow-up event is a continuation of the
    /// same episode or the start of a new one.
    pub dominant_kind: ChaosKind,
    /// Initiator carried over from the first event; used by the merge
    /// policy. `None` means "unknown actor" (not "anonymous shared").
    pub initiator: Option<u64>,
    /// Running sum of per-event severities; used to incrementally
    /// recompute `mean_severity` as events are absorbed.
    sum_severity: f64,
}

impl ChaosCluster {
    fn from_event(e: &ChaosEvent) -> Self {
        let mut c = Self {
            start_seq: e.start_seq,
            end_seq: e.end_seq,
            peak_severity: e.severity,
            mean_severity: e.severity,
            composite_severity: 0.0,
            count: 1,
            dominant_kind: e.kind,
            initiator: e.initiator,
            sum_severity: e.severity,
            events: vec![e.clone()],
        };
        c.recompute_composite();
        c
    }

    fn absorb(&mut self, e: &ChaosEvent) {
        self.end_seq = self.end_seq.max(e.end_seq);
        self.peak_severity = self.peak_severity.max(e.severity);
        self.sum_severity += e.severity;
        self.count += 1;
        self.mean_severity = self.sum_severity / self.count as f64;
        self.events.push(e.clone());
        self.recompute_composite();
    }

    /// `peak * (1 + ln(count))`. `ln(1) == 0` so a 1-event cluster has
    /// composite == peak, matching intuition. Growth is sub-linear in
    /// count so a 100-event cluster does not dominate a 5-event one
    /// purely on volume.
    fn recompute_composite(&mut self) {
        let persistence = 1.0 + (self.count as f64).ln();
        self.composite_severity = self.peak_severity * persistence;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ChaosFeatures;

    fn ev(kind: ChaosKind, start: u64, end: u64, sev: f64, initiator: Option<u64>) -> ChaosEvent {
        ChaosEvent {
            kind,
            start_seq: start,
            end_seq: end,
            severity: sev,
            initiator,
            features: ChaosFeatures {
                event_count: 1,
                duration_ns: 0,
                cancel_trade_ratio: 0.0,
                price_displacement: 0,
                depth_removed: 0,
            },
        }
    }

    #[test]
    fn same_kind_same_initiator_merges() {
        let c = ChaosClusterer::new(100);
        let evs = vec![
            ev(ChaosKind::PhantomLiquidity, 10, 20, 0.5, Some(42)),
            ev(ChaosKind::PhantomLiquidity, 50, 60, 0.7, Some(42)),
        ];
        let out = c.cluster(&evs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].events.len(), 2);
        assert_eq!(out[0].peak_severity, 0.7);
    }

    #[test]
    fn same_kind_different_initiator_splits() {
        let c = ChaosClusterer::new(100);
        let evs = vec![
            ev(ChaosKind::PhantomLiquidity, 10, 20, 0.5, Some(42)),
            ev(ChaosKind::PhantomLiquidity, 50, 60, 0.7, Some(99)),
        ];
        let out = c.cluster(&evs);
        assert_eq!(out.len(), 2, "different initiators must not merge");
    }

    #[test]
    fn unknown_initiators_do_not_merge() {
        // Two `None` initiators are NOT the same actor — they're both
        // "unknown". Policy rejects the merge.
        let c = ChaosClusterer::new(100);
        let evs = vec![
            ev(ChaosKind::Spoof, 10, 20, 0.5, None),
            ev(ChaosKind::Spoof, 50, 60, 0.7, None),
        ];
        let out = c.cluster(&evs);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn crash_and_ignition_merge_regardless_of_initiator() {
        let c = ChaosClusterer::new(100);
        let evs = vec![
            ev(ChaosKind::MomentumIgnition, 10, 30, 0.6, Some(7)),
            ev(ChaosKind::FlashCrash, 50, 80, 0.9, Some(8)),
        ];
        let out = c.cluster(&evs);
        assert_eq!(out.len(), 1, "crash + ignition share the price move");
        assert_eq!(out[0].peak_severity, 0.9);
    }

    #[test]
    fn unrelated_kinds_never_merge() {
        let c = ChaosClusterer::new(100);
        let evs = vec![
            ev(ChaosKind::QuoteStuff, 10, 20, 0.5, Some(1)),
            ev(ChaosKind::FlashCrash, 30, 40, 0.9, Some(1)),
        ];
        let out = c.cluster(&evs);
        assert_eq!(out.len(), 2, "quote-stuff and flash-crash are unrelated");
    }

    #[test]
    fn distant_events_split_even_when_eligible() {
        let c = ChaosClusterer::new(10);
        let evs = vec![
            ev(ChaosKind::PhantomLiquidity, 10, 20, 0.5, Some(42)),
            ev(ChaosKind::PhantomLiquidity, 200, 210, 0.7, Some(42)),
        ];
        let out = c.cluster(&evs);
        assert_eq!(out.len(), 2, "seq-gap > max_gap must always split");
    }

    #[test]
    fn empty_input_yields_no_clusters() {
        let c = ChaosClusterer::new(100);
        assert!(c.cluster(&[]).is_empty());
    }

    #[test]
    fn singleton_composite_equals_peak() {
        // 1 event -> ln(1) == 0 -> composite == peak. Pinning this so a
        // future tweak of the persistence factor cannot silently change
        // the meaning of the field for the common 1-event case.
        let c = ChaosClusterer::new(100);
        let evs = vec![ev(ChaosKind::Spoof, 10, 20, 0.8, Some(1))];
        let out = c.cluster(&evs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].count, 1);
        assert!((out[0].composite_severity - 0.8).abs() < 1e-9);
        assert!((out[0].mean_severity - 0.8).abs() < 1e-9);
    }

    #[test]
    fn composite_grows_with_persistence_not_volume() {
        // Two episodes with the SAME peak (0.6); the longer one must
        // rank strictly higher on composite.
        let c = ChaosClusterer::new(100);
        let short = vec![
            ev(ChaosKind::PhantomLiquidity, 10, 20, 0.6, Some(1)),
            ev(ChaosKind::PhantomLiquidity, 30, 40, 0.4, Some(1)),
        ];
        let long = vec![
            ev(ChaosKind::PhantomLiquidity, 10, 20, 0.6, Some(1)),
            ev(ChaosKind::PhantomLiquidity, 30, 40, 0.4, Some(1)),
            ev(ChaosKind::PhantomLiquidity, 50, 60, 0.4, Some(1)),
            ev(ChaosKind::PhantomLiquidity, 70, 80, 0.4, Some(1)),
            ev(ChaosKind::PhantomLiquidity, 90, 100, 0.4, Some(1)),
        ];
        let s = &c.cluster(&short)[0];
        let l = &c.cluster(&long)[0];
        assert_eq!(s.peak_severity, l.peak_severity);
        assert!(
            l.composite_severity > s.composite_severity,
            "longer episode at same peak must rank higher"
        );
        // Mean drops as the long episode adds 0.4 events to a 0.6 peak.
        assert!(l.mean_severity < s.mean_severity);
    }
}
