// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

use std::path::Path;

use flowlab_core::event::{Event, SequencedEvent};
use flowlab_core::state::{ReplayState, StateError};
use tracing::info;

/// Deterministic replay engine — reads event log, drives state machine.
pub struct ReplayEngine {
    state: ReplayState,
    events_processed: u64,
}

impl ReplayEngine {
    pub fn new(instrument_id: u32) -> Self {
        Self {
            state: ReplayState::new(instrument_id),
            events_processed: 0,
        }
    }

    /// Replay a slice of events. Halts on sequence gap.
    pub fn replay(&mut self, events: &[SequencedEvent]) -> Result<ReplayResult, ReplayError> {
        for seq_event in events {
            self.state.process(seq_event).map_err(|e| match e {
                StateError::SequenceGap { expected, got } => ReplayError::SequenceGap {
                    expected,
                    got,
                    events_processed: self.events_processed,
                },
            })?;
            self.events_processed += 1;
        }

        Ok(ReplayResult {
            events_processed: self.events_processed,
            final_hash: self.state.hash(),
            last_seq: self.state.last_seq.unwrap_or(0),
        })
    }

    /// Replay from a binary event log file (mmap, zero-copy).
    pub fn replay_from_file(&mut self, path: &Path) -> Result<ReplayResult, ReplayError> {
        let file = std::fs::File::open(path)
            .map_err(|e| ReplayError::Io(e.to_string()))?;

        let mmap = unsafe { memmap2::Mmap::map(&file) }
            .map_err(|e| ReplayError::Io(e.to_string()))?;

        let event_size = size_of::<Event>();
        if mmap.len() % event_size != 0 {
            return Err(ReplayError::CorruptedLog {
                reason: format!(
                    "file size {} not aligned to event size {}",
                    mmap.len(),
                    event_size
                ),
            });
        }

        let events: &[Event] = bytemuck::cast_slice(&mmap);
        info!(event_count = events.len(), "replaying from file");

        for (i, event) in events.iter().enumerate() {
            let seq_event = SequencedEvent {
                seq: (i as u64) + 1,
                channel_id: 0,
                event: *event,
            };
            self.state.process(&seq_event).map_err(|e| match e {
                StateError::SequenceGap { expected, got } => ReplayError::SequenceGap {
                    expected,
                    got,
                    events_processed: self.events_processed,
                },
            })?;
            self.events_processed += 1;
        }

        Ok(ReplayResult {
            events_processed: self.events_processed,
            final_hash: self.state.hash(),
            last_seq: self.state.last_seq.unwrap_or(0),
        })
    }

    pub fn state(&self) -> &ReplayState {
        &self.state
    }

    pub fn events_processed(&self) -> u64 {
        self.events_processed
    }
}

#[derive(Debug)]
pub struct ReplayResult {
    pub events_processed: u64,
    pub final_hash: u64,
    pub last_seq: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    #[error("sequence gap at event {events_processed}: expected {expected}, got {got}")]
    SequenceGap {
        expected: u64,
        got: u64,
        events_processed: u64,
    },
    #[error("corrupted log: {reason}")]
    CorruptedLog { reason: String },
    #[error("io error: {0}")]
    Io(String),
}
