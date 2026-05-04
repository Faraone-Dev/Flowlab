// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

use xxhash_rust::xxh3::xxh3_64;

/// Incremental deterministic hasher for state verification.
pub struct StateHasher {
    state: u64,
}

impl StateHasher {
    pub fn new() -> Self {
        Self { state: 0 }
    }

    /// Feed bytes into the running hash: H(prev || data)
    pub fn update(&mut self, data: &[u8]) {
        let mut buf = Vec::with_capacity(8 + data.len());
        buf.extend_from_slice(&self.state.to_le_bytes());
        buf.extend_from_slice(data);
        self.state = xxh3_64(&buf);
    }

    /// Current hash value.
    pub fn digest(&self) -> u64 {
        self.state
    }

    pub fn reset(&mut self) {
        self.state = 0;
    }
}

impl Default for StateHasher {
    fn default() -> Self {
        Self::new()
    }
}
