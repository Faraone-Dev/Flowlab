use crate::ChaosEvent;
use flowlab_core::types::SeqNum;

/// Temporal clustering — groups chaos events by proximity.
pub struct ChaosClusterer {
    /// Maximum gap (in sequence IDs) to merge into same cluster
    max_gap: u64,
}

impl ChaosClusterer {
    pub fn new(max_gap: u64) -> Self {
        Self { max_gap }
    }

    /// Cluster chaos events into contiguous groups.
    pub fn cluster(&self, events: &[ChaosEvent]) -> Vec<ChaosCluster> {
        if events.is_empty() {
            return Vec::new();
        }

        let mut sorted: Vec<&ChaosEvent> = events.iter().collect();
        sorted.sort_by_key(|e| e.start_seq);

        let mut clusters = Vec::new();
        let mut current = ChaosCluster {
            start_seq: sorted[0].start_seq,
            end_seq: sorted[0].end_seq,
            events: vec![sorted[0].clone()],
            peak_severity: sorted[0].severity,
        };

        for event in &sorted[1..] {
            if event.start_seq <= current.end_seq + self.max_gap {
                // Merge into current cluster
                current.end_seq = current.end_seq.max(event.end_seq);
                current.peak_severity = current.peak_severity.max(event.severity);
                current.events.push((*event).clone());
            } else {
                clusters.push(current);
                current = ChaosCluster {
                    start_seq: event.start_seq,
                    end_seq: event.end_seq,
                    events: vec![(*event).clone()],
                    peak_severity: event.severity,
                };
            }
        }
        clusters.push(current);

        clusters
    }
}

#[derive(Debug, Clone)]
pub struct ChaosCluster {
    pub start_seq: SeqNum,
    pub end_seq: SeqNum,
    pub events: Vec<ChaosEvent>,
    pub peak_severity: f64,
}
