use flowlab_flow::regime::Regime;
use crate::{ChaosEvent, StressWindow};
use flowlab_core::types::SeqNum;

/// Extracts stress windows from chaos events and regime classification.
pub struct WindowExtractor {
    /// Minimum regime level to trigger stress window
    min_regime: Regime,
    /// Merge windows closer than this gap
    merge_gap: u64,
}

impl WindowExtractor {
    pub fn new(min_regime: Regime, merge_gap: u64) -> Self {
        Self {
            min_regime,
            merge_gap,
        }
    }

    /// Extract stress windows from tagged sequence of (seq, regime, chaos_events).
    pub fn extract(
        &self,
        tagged: &[(SeqNum, Regime, Vec<ChaosEvent>)],
    ) -> Vec<StressWindow> {
        let mut windows = Vec::new();
        let mut current: Option<StressWindow> = None;

        for (seq, regime, chaos) in tagged {
            if *regime >= self.min_regime {
                match current.as_mut() {
                    Some(w) if seq - w.end_seq <= self.merge_gap => {
                        w.end_seq = *seq;
                        if *regime > w.regime {
                            w.regime = *regime;
                        }
                        w.chaos_events.extend(chaos.iter().cloned());
                    }
                    _ => {
                        if let Some(w) = current.take() {
                            w.severity_finalize(&mut windows);
                        }
                        current = Some(StressWindow {
                            start_seq: *seq,
                            end_seq: *seq,
                            regime: *regime,
                            severity: 0.0,
                            chaos_events: chaos.clone(),
                        });
                    }
                }
            } else if let Some(w) = current.take() {
                w.severity_finalize(&mut windows);
            }
        }

        if let Some(w) = current.take() {
            w.severity_finalize(&mut windows);
        }

        windows
    }
}

trait SeverityFinalize {
    fn severity_finalize(self, dest: &mut Vec<StressWindow>);
}

impl SeverityFinalize for StressWindow {
    fn severity_finalize(mut self, dest: &mut Vec<StressWindow>) {
        self.severity = if self.chaos_events.is_empty() {
            0.0
        } else {
            self.chaos_events.iter().map(|e| e.severity).sum::<f64>()
                / self.chaos_events.len() as f64
        };
        dest.push(self);
    }
}
