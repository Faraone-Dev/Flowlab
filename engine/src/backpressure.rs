// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

//! Backpressure-aware bounded channel with drop counter.
//!
//! Wraps `crossbeam_channel::bounded`. The producer NEVER blocks: if the
//! channel is full, the sample is dropped and an atomic counter is bumped.
//! That counter is exposed in `TickFrame.dropped_total` so the dashboard
//! can show "the engine is shedding load" instead of the system silently
//! freezing.
//!
//! Capacity is sized for the realistic feed rate. At 200 k events/sec and
//! a 8 192 slot ring, we tolerate ~40 ms of consumer stall before drop.

use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub struct Producer<T> {
    tx: Sender<T>,
    dropped: Arc<AtomicU64>,
}

pub struct Consumer<T> {
    rx: Receiver<T>,
    dropped: Arc<AtomicU64>,
}

impl<T> Consumer<T> {
    pub fn recv(&self) -> Option<T> {
        self.rx.recv().ok()
    }

    pub fn try_recv(&self) -> Option<T> {
        self.rx.try_recv().ok()
    }

    pub fn dropped_total(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl<T> Producer<T> {
    /// Best-effort send. Drops on overflow, never blocks.
    pub fn try_send(&self, t: T) {
        if let Err(e) = self.tx.try_send(t) {
            match e {
                TrySendError::Full(_) => {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                }
                TrySendError::Disconnected(_) => {
                    // consumer gone — drop silently, counter is meaningless now
                }
            }
        }
    }

    /// Cumulative drop count visible to the producer side.
    pub fn dropped_total(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

pub fn channel<T>(capacity: usize) -> (Producer<T>, Consumer<T>) {
    let (tx, rx) = bounded(capacity);
    let dropped = Arc::new(AtomicU64::new(0));
    (
        Producer { tx, dropped: dropped.clone() },
        Consumer { rx, dropped },
    )
}
