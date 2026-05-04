// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

//! Offline ITCH replay source.
//!
//! Reads raw NASDAQ ITCH 5.0 (no length prefix — boundaries from per-type
//! length table) from a file and streams into a [`RingWriter`] like a
//! live handler.
//!
//! - Zero-copy on read: consumer decides when to parse.
//! - Back-pressure friendly: yield + retry on full ring (drop policy elsewhere).
//! - Pacing: optional `RealTime { speedup }` from ITCH ts48; default saturates.
//! - Bounded look-ahead: only whole messages are published.

use crate::itch::msg_length;
use crate::ring_reader::{RingError, RingWriter};
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub enum Pace {
    /// Push messages as fast as the ring allows.
    AsFastAsPossible,
    /// Preserve inter-arrival spacing derived from the ITCH 48-bit
    /// nanosecond timestamps. Uses a busy-wait loop calibrated in
    /// nanoseconds; suitable for sub-microsecond fidelity.
    RealTime { speedup: f64 },
}

#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("ring: {0}")]
    Ring(#[from] RingError),
    #[error("truncated ITCH stream at offset {offset} (need {need} more bytes)")]
    Truncated { offset: u64, need: usize },
    #[error("unknown ITCH message type 0x{ty:02x} at offset {offset}")]
    UnknownType { ty: u8, offset: u64 },
}

/// Statistics emitted by [`FileItchReplay::drive`].
#[derive(Debug, Default, Clone, Copy)]
pub struct ReplayStats {
    pub messages: u64,
    pub bytes: u64,
    pub elapsed: Duration,
}

impl ReplayStats {
    pub fn throughput_msg_per_sec(&self) -> f64 {
        if self.elapsed.is_zero() {
            return 0.0;
        }
        self.messages as f64 / self.elapsed.as_secs_f64()
    }

    pub fn throughput_mib_per_sec(&self) -> f64 {
        if self.elapsed.is_zero() {
            return 0.0;
        }
        (self.bytes as f64 / (1024.0 * 1024.0)) / self.elapsed.as_secs_f64()
    }
}

/// File-backed ITCH replay driver.
///
/// Typical usage:
/// ```no_run
/// # use flowlab_replay::{RingWriter, file_source::{FileItchReplay, Pace}};
/// let mut writer = RingWriter::create("/tmp/ring.bin", 1 << 20).unwrap();
/// let mut source = FileItchReplay::open("feed.itch").unwrap();
/// let stats = source.drive(&mut writer, Pace::AsFastAsPossible).unwrap();
/// println!("{:.2} M msg/s", stats.throughput_msg_per_sec() / 1e6);
/// ```
pub struct FileItchReplay {
    /// Full file contents. For production-sized files (hundreds of GB)
    /// switch to `memmap2::Mmap`; for typical trading-day captures
    /// (~10 GB/day) this is tolerable.
    buf: Vec<u8>,
    cursor: usize,
}

impl FileItchReplay {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, ReplayError> {
        let mut f = File::open(path)?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        Ok(Self { buf, cursor: 0 })
    }

    pub fn from_bytes(buf: Vec<u8>) -> Self {
        Self { buf, cursor: 0 }
    }

    #[inline]
    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.cursor)
    }

    /// Drive the replay to completion. Returns aggregate stats.
    pub fn drive(
        &mut self,
        writer: &mut RingWriter,
        pace: Pace,
    ) -> Result<ReplayStats, ReplayError> {
        let start = Instant::now();
        let wall_start = start;
        let mut first_ts_ns: Option<u64> = None;
        let mut messages = 0u64;
        let mut bytes = 0u64;

        // Publish in batches of up to 64 KiB for cache-friendly writes.
        const BATCH_TARGET: usize = 64 * 1024;
        let mut batch_start = self.cursor;

        while self.cursor < self.buf.len() {
            let ty = self.buf[self.cursor];
            let len = msg_length(ty) as usize;
            if len == 0 {
                return Err(ReplayError::UnknownType {
                    ty,
                    offset: self.cursor as u64,
                });
            }
            if self.cursor + len > self.buf.len() {
                return Err(ReplayError::Truncated {
                    offset: self.cursor as u64,
                    need: (self.cursor + len) - self.buf.len(),
                });
            }

            // RealTime pacing: extract the 48-bit ts for pacing types.
            if let Pace::RealTime { speedup } = pace {
                if let Some(ts) = extract_ts_ns(&self.buf[self.cursor..self.cursor + len]) {
                    if first_ts_ns.is_none() {
                        first_ts_ns = Some(ts);
                    }
                    let target = Duration::from_nanos(
                        ((ts - first_ts_ns.unwrap()) as f64 / speedup.max(1e-9)) as u64,
                    );
                    let elapsed = wall_start.elapsed();
                    if target > elapsed {
                        spin_sleep(target - elapsed);
                    }
                }
            }

            self.cursor += len;
            messages += 1;
            bytes += len as u64;

            if self.cursor - batch_start >= BATCH_TARGET {
                flush_batch(writer, &self.buf[batch_start..self.cursor])?;
                batch_start = self.cursor;
            }
        }
        if batch_start < self.cursor {
            flush_batch(writer, &self.buf[batch_start..self.cursor])?;
        }

        Ok(ReplayStats {
            messages,
            bytes,
            elapsed: start.elapsed(),
        })
    }
}

/// Write `batch` into the ring, spinning while full. The writer side
/// never blocks the pipeline longer than the consumer drain time.
fn flush_batch(writer: &mut RingWriter, mut batch: &[u8]) -> Result<(), ReplayError> {
    // Split into halves if the batch is larger than a single ring can
    // absorb at once. The ring capacity is a compile-time unknown from
    // here, so we retry on Full.
    while !batch.is_empty() {
        match writer.write(batch) {
            Ok(()) => return Ok(()),
            Err(RingError::Full { avail, .. }) if avail > 0 => {
                let (head, tail) = batch.split_at(avail);
                writer.write(head)?;
                batch = tail;
                // Tight back-off: yield to the consumer thread.
                std::thread::yield_now();
            }
            Err(RingError::Full { .. }) => {
                std::thread::yield_now();
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Extract the 48-bit timestamp from ITCH messages that carry one in
/// bytes [5..11]. Returns None for types without a timestamp field.
#[inline]
fn extract_ts_ns(msg: &[u8]) -> Option<u64> {
    if msg.len() < 11 {
        return None;
    }
    match msg[0] {
        b'A' | b'F' | b'D' | b'X' | b'E' | b'C' | b'P' | b'U' | b'Q' => {
            let b = &msg[5..11];
            Some(
                ((b[0] as u64) << 40)
                    | ((b[1] as u64) << 32)
                    | ((b[2] as u64) << 24)
                    | ((b[3] as u64) << 16)
                    | ((b[4] as u64) << 8)
                    | (b[5] as u64),
            )
        }
        _ => None,
    }
}

/// Hybrid sleep: sleep for most of the interval, spin for the last
/// 100 µs to hit sub-microsecond precision on Windows/Linux alike.
fn spin_sleep(dur: Duration) {
    const SPIN_WINDOW: Duration = Duration::from_micros(100);
    if dur > SPIN_WINDOW {
        std::thread::sleep(dur - SPIN_WINDOW);
    }
    let deadline = Instant::now() + dur.min(SPIN_WINDOW);
    while Instant::now() < deadline {
        std::hint::spin_loop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::itch::{parse_buffer, synthetic_itch_stream};
    use flowlab_core::event::Event;
    use flowlab_core::hot_book::HotOrderBook;

    fn tmp_path(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "flowlab-file-src-{}-{}.bin",
            name,
            std::process::id()
        ));
        p
    }

    #[test]
    fn file_replay_produces_identical_hash_to_direct_parse() {
        // Reference: parse & apply directly.
        let raw = synthetic_itch_stream(5_000, 0xCAFEF00D);
        let mut direct_events: Vec<Event> = Vec::new();
        parse_buffer(&raw, &mut direct_events).unwrap();
        let mut direct_book = HotOrderBook::<256>::new(1);
        for ev in &direct_events {
            direct_book.apply(ev);
        }
        let direct_hash = direct_book.canonical_l2_hash();

        // IPC path: file → ring → parse → apply.
        let itch_path = tmp_path("feed.itch");
        std::fs::write(&itch_path, &raw).unwrap();
        let ring_path = tmp_path("ring.bin");
        let cap: u64 = 1 << 20; // 1 MiB — plenty for 5k messages
        let mut writer = crate::RingWriter::create(&ring_path, cap).unwrap();
        let mut reader = crate::RingReader::open(&ring_path, cap).unwrap();

        let mut source = FileItchReplay::open(&itch_path).unwrap();
        let stats = source.drive(&mut writer, Pace::AsFastAsPossible).unwrap();
        assert!(stats.messages >= 5_000, "too few messages: {}", stats.messages);

        // Drain the ring in one shot (fits in 1 MiB).
        let mut out = vec![0u8; cap as usize];
        let mut filled = 0usize;
        loop {
            let n = reader.read(&mut out[filled..]);
            if n == 0 {
                break;
            }
            filled += n;
        }
        assert_eq!(filled, raw.len(), "ring must carry full ITCH stream");
        assert_eq!(&out[..filled], raw.as_slice());

        let mut ipc_events: Vec<Event> = Vec::new();
        parse_buffer(&out[..filled], &mut ipc_events).unwrap();
        let mut ipc_book = HotOrderBook::<256>::new(1);
        for ev in &ipc_events {
            ipc_book.apply(ev);
        }
        assert_eq!(ipc_book.canonical_l2_hash(), direct_hash);

        std::fs::remove_file(&itch_path).ok();
        std::fs::remove_file(&ring_path).ok();
    }

    #[test]
    fn unknown_message_type_is_rejected() {
        let ring_path = tmp_path("ring_bad.bin");
        let mut writer = crate::RingWriter::create(&ring_path, 4096).unwrap();
        let mut source = FileItchReplay::from_bytes(vec![b'Z', 0, 0, 0]);
        let err = source
            .drive(&mut writer, Pace::AsFastAsPossible)
            .unwrap_err();
        assert!(matches!(err, ReplayError::UnknownType { ty: b'Z', .. }));
        std::fs::remove_file(&ring_path).ok();
    }

    #[test]
    fn truncated_stream_is_rejected() {
        // An 'A' Add-Order message is 36 bytes; passing only 10 must trip
        // the truncation guard.
        let ring_path = tmp_path("ring_trunc.bin");
        let mut writer = crate::RingWriter::create(&ring_path, 4096).unwrap();
        let mut truncated = vec![0u8; 10];
        truncated[0] = b'A';
        let mut source = FileItchReplay::from_bytes(truncated);
        let err = source
            .drive(&mut writer, Pace::AsFastAsPossible)
            .unwrap_err();
        assert!(matches!(err, ReplayError::Truncated { .. }));
        std::fs::remove_file(&ring_path).ok();
    }
}
