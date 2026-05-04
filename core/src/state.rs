// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

use xxhash_rust::xxh3::xxh3_64;

use crate::event::SequencedEvent;
use crate::orderbook::OrderBook;
use crate::types::SeqNum;

/// Deterministic state machine — processes sequenced events, maintains book + hash.
///
/// Hash is updated with a zero-allocation streaming scheme:
///   `H_new = xxh3_64(prev_hash || seq || event_bytes)`
/// where the input is assembled in a fixed 56-byte stack buffer
/// (8 + 8 + 40), avoiding any heap allocation in the hot path.
pub struct ReplayState {
    pub book: OrderBook,
    /// Last processed sequence number, `None` until first event.
    pub last_seq: Option<SeqNum>,
    /// Running state hash (xxh3)
    pub state_hash: u64,
}

impl ReplayState {
    pub fn new(instrument_id: u32) -> Self {
        Self {
            book: OrderBook::new(instrument_id),
            last_seq: None,
            state_hash: 0,
        }
    }

    /// Process a single sequenced event. Returns error on sequence gap.
    pub fn process(&mut self, seq_event: &SequencedEvent) -> Result<(), StateError> {
        // Strict monotonic sequencing — first event sets the anchor.
        if let Some(last) = self.last_seq {
            let expected = last + 1;
            if seq_event.seq != expected {
                return Err(StateError::SequenceGap {
                    expected,
                    got: seq_event.seq,
                });
            }
        }

        self.book.apply(&seq_event.event);
        self.last_seq = Some(seq_event.seq);

        // Zero-alloc hash update: 8 (prev hash) + 8 (seq) + 40 (event) = 56 bytes stack.
        let event_bytes = bytemuck::bytes_of(&seq_event.event);
        let mut buf = [0u8; 56];
        buf[0..8].copy_from_slice(&self.state_hash.to_le_bytes());
        buf[8..16].copy_from_slice(&seq_event.seq.to_le_bytes());
        buf[16..56].copy_from_slice(event_bytes);
        self.state_hash = xxh3_64(&buf);

        Ok(())
    }

    /// Current state hash for verification
    pub fn hash(&self) -> u64 {
        self.state_hash
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("sequence gap: expected {expected}, got {got}")]
    SequenceGap { expected: SeqNum, got: SeqNum },
}
