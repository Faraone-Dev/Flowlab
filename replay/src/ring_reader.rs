// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

//! SPSC ring buffer IPC reader.
//!
//! Mirrors the Go ingest writer (`ingest/mmap/ring.go`) for zero-copy
//! Zig/Rust consumption. All little-endian.
//!
//! ```text
//!   [0..8)     magic "FLOWRING"
//!   [8..16)    capacity (u64, payload bytes; power-of-two)
//!   [16..64)   reserved
//!   [64..72)   writeIdx (u64 atomic, own cache line)
//!   [128..136) readIdx  (u64 atomic, own cache line)
//!   [192..192+capacity) payload
//! ```
//!
//! Indices are monotonically increasing (NOT wrapped); slot offset =
//! `payload[idx & mask]`. Ordering: writer Release-stores writeIdx after
//! payload memcpy; reader Acquire-loads writeIdx, Release-stores readIdx
//! after consume.

use memmap2::{MmapMut, MmapOptions};
use std::fs::OpenOptions;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

pub const MAGIC: &[u8; 8] = b"FLOWRING";
pub const HEADER_SIZE: usize = 64;
pub const WRITE_IDX_OFFSET: usize = 64;
pub const READ_IDX_OFFSET: usize = 128;
pub const DATA_OFFSET: usize = 192;

#[derive(Debug, thiserror::Error)]
pub enum RingError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("bad magic: expected FLOWRING, got {0:?}")]
    BadMagic([u8; 8]),
    #[error("capacity mismatch: file says {file}, caller expected {expected}")]
    CapacityMismatch { file: u64, expected: u64 },
    #[error("capacity must be a power of two, got {0}")]
    NotPowerOfTwo(u64),
    #[error("ring full: need {need} bytes, available {avail}")]
    Full { need: usize, avail: usize },
    #[error("file too small: {got} < {need}")]
    TooSmall { got: usize, need: usize },
}

/// Zero-copy mmap ring reader. Multi-slot view is exposed via
/// [`RingReader::available`] which returns the contiguous (and
/// optionally wrapped) byte regions currently readable.
#[derive(Debug)]
pub struct RingReader {
    mmap: MmapMut,
    capacity: u64,
    mask: u64,
}

impl RingReader {
    /// Open an existing ring file produced by the writer side.
    /// `expected_capacity` must match the value embedded in the header
    /// (0 means "accept whatever the header says").
    pub fn open<P: AsRef<Path>>(path: P, expected_capacity: u64) -> Result<Self, RingError> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let meta = file.metadata()?;
        if (meta.len() as usize) < DATA_OFFSET {
            return Err(RingError::TooSmall {
                got: meta.len() as usize,
                need: DATA_OFFSET,
            });
        }
        // SAFETY: mmap of a regular file is safe as long as no other
        // process truncates it; this is a cooperative IPC protocol.
        let mmap = unsafe { MmapOptions::new().map_mut(&file)? };

        let mut magic = [0u8; 8];
        magic.copy_from_slice(&mmap[0..8]);
        if &magic != MAGIC {
            return Err(RingError::BadMagic(magic));
        }

        let mut cap_bytes = [0u8; 8];
        cap_bytes.copy_from_slice(&mmap[8..16]);
        let capacity = u64::from_le_bytes(cap_bytes);

        if !capacity.is_power_of_two() {
            return Err(RingError::NotPowerOfTwo(capacity));
        }
        if expected_capacity != 0 && expected_capacity != capacity {
            return Err(RingError::CapacityMismatch {
                file: capacity,
                expected: expected_capacity,
            });
        }
        let need = DATA_OFFSET + capacity as usize;
        if mmap.len() < need {
            return Err(RingError::TooSmall { got: mmap.len(), need });
        }

        Ok(Self {
            mmap,
            capacity,
            mask: capacity - 1,
        })
    }

    #[inline]
    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    #[inline]
    fn write_idx(&self) -> &AtomicU64 {
        // Safe: offset is 8-byte aligned within the mmap and we never
        // re-borrow the same cache line mutably.
        unsafe {
            let p = self.mmap.as_ptr().add(WRITE_IDX_OFFSET) as *const AtomicU64;
            &*p
        }
    }

    #[inline]
    fn read_idx(&self) -> &AtomicU64 {
        unsafe {
            let p = self.mmap.as_ptr().add(READ_IDX_OFFSET) as *const AtomicU64;
            &*p
        }
    }

    /// Current read cursor (monotonic).
    #[inline]
    pub fn read_cursor(&self) -> u64 {
        self.read_idx().load(Ordering::Relaxed)
    }

    /// Current write cursor (monotonic) observed with Acquire ordering
    /// so payload bytes are visible on success.
    #[inline]
    pub fn write_cursor(&self) -> u64 {
        self.write_idx().load(Ordering::Acquire)
    }

    /// Returns the two contiguous slices (primary, wrap) of available
    /// payload, bounded by `max_bytes`. The second slice is empty
    /// when the available region does not wrap around the buffer end.
    pub fn available(&self, max_bytes: usize) -> (&[u8], &[u8]) {
        let wi = self.write_cursor();
        let ri = self.read_cursor();
        let mut n = (wi - ri) as usize;
        if n == 0 {
            return (&[], &[]);
        }
        if n > max_bytes {
            n = max_bytes;
        }
        let start = (ri & self.mask) as usize;
        let cap = self.capacity as usize;
        let payload = &self.mmap[DATA_OFFSET..DATA_OFFSET + cap];

        if start + n <= cap {
            (&payload[start..start + n], &[])
        } else {
            let first = cap - start;
            (&payload[start..], &payload[..n - first])
        }
    }

    /// Copy up to `dst.len()` bytes of available payload into `dst`
    /// and advance the read cursor. Returns the number of bytes read.
    pub fn read(&mut self, dst: &mut [u8]) -> usize {
        let (a, b) = self.available(dst.len());
        let n = a.len() + b.len();
        if n == 0 {
            return 0;
        }
        dst[..a.len()].copy_from_slice(a);
        dst[a.len()..n].copy_from_slice(b);
        // Release: consumer has finished reading payload before the
        // writer is allowed to reuse these slots.
        self.read_idx().fetch_add(n as u64, Ordering::Release);
        n
    }
}

/// Test/bench helper: a Rust writer that produces the exact same
/// byte layout as the Go `RingBuffer.Write` in `ingest/mmap/ring.go`.
///
/// Kept in the same crate so that unit tests can validate the reader
/// without requiring a running Go process. The Go side remains the
/// canonical production writer.
pub struct RingWriter {
    mmap: MmapMut,
    capacity: u64,
    mask: u64,
}

impl RingWriter {
    /// Create (or truncate) a ring file of `capacity` payload bytes.
    /// `capacity` must be a power of two.
    pub fn create<P: AsRef<Path>>(path: P, capacity: u64) -> Result<Self, RingError> {
        if !capacity.is_power_of_two() {
            return Err(RingError::NotPowerOfTwo(capacity));
        }
        let total = DATA_OFFSET as u64 + capacity;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.set_len(total)?;
        let mut mmap = unsafe { MmapOptions::new().map_mut(&file)? };

        mmap[0..8].copy_from_slice(MAGIC);
        mmap[8..16].copy_from_slice(&capacity.to_le_bytes());
        // writeIdx / readIdx already zeroed by set_len on a fresh file.

        Ok(Self {
            mmap,
            capacity,
            mask: capacity - 1,
        })
    }

    #[inline]
    fn write_idx(&self) -> &AtomicU64 {
        unsafe {
            let p = self.mmap.as_ptr().add(WRITE_IDX_OFFSET) as *const AtomicU64;
            &*p
        }
    }

    #[inline]
    fn read_idx(&self) -> &AtomicU64 {
        unsafe {
            let p = self.mmap.as_ptr().add(READ_IDX_OFFSET) as *const AtomicU64;
            &*p
        }
    }

    /// Publish `batch` to the ring. Returns `RingError::Full` if not
    /// enough free space. Matches Go semantics: payload copy followed
    /// by a Release store of the writeIdx.
    pub fn write(&mut self, batch: &[u8]) -> Result<(), RingError> {
        let wi = self.write_idx().load(Ordering::Relaxed);
        let ri = self.read_idx().load(Ordering::Acquire);
        let free = self.capacity - (wi - ri);
        if (batch.len() as u64) > free {
            return Err(RingError::Full {
                need: batch.len(),
                avail: free as usize,
            });
        }
        let start = (wi & self.mask) as usize;
        let cap = self.capacity as usize;
        // We must split the mmap borrow because `write_idx()` lives
        // inside the same mmap. Use pointer arithmetic for the payload.
        unsafe {
            let payload = self.mmap.as_mut_ptr().add(DATA_OFFSET);
            if start + batch.len() <= cap {
                std::ptr::copy_nonoverlapping(batch.as_ptr(), payload.add(start), batch.len());
            } else {
                let first = cap - start;
                std::ptr::copy_nonoverlapping(batch.as_ptr(), payload.add(start), first);
                std::ptr::copy_nonoverlapping(
                    batch.as_ptr().add(first),
                    payload,
                    batch.len() - first,
                );
            }
        }
        // Release: payload visible before index advance.
        self.write_idx()
            .store(wi + batch.len() as u64, Ordering::Release);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("flowlab-ring-{}-{}.bin", name, std::process::id()));
        p
    }

    #[test]
    fn header_roundtrip() {
        let p = tmp_path("header");
        let _w = RingWriter::create(&p, 4096).unwrap();
        let r = RingReader::open(&p, 4096).unwrap();
        assert_eq!(r.capacity(), 4096);
        assert_eq!(r.read_cursor(), 0);
        assert_eq!(r.write_cursor(), 0);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn bad_magic_rejected() {
        let p = tmp_path("bad_magic");
        {
            let _w = RingWriter::create(&p, 1024).unwrap();
        }
        // Corrupt magic.
        let mut bytes = std::fs::read(&p).unwrap();
        bytes[0] = b'X';
        std::fs::write(&p, &bytes).unwrap();
        let err = RingReader::open(&p, 1024).unwrap_err();
        assert!(matches!(err, RingError::BadMagic(_)));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn single_write_read_roundtrip() {
        let p = tmp_path("single");
        let mut w = RingWriter::create(&p, 1024).unwrap();
        let mut r = RingReader::open(&p, 1024).unwrap();

        let payload: Vec<u8> = (0..200u8).collect();
        w.write(&payload).unwrap();

        let mut buf = vec![0u8; 256];
        let n = r.read(&mut buf);
        assert_eq!(n, 200);
        assert_eq!(&buf[..n], payload.as_slice());
        assert_eq!(r.read_cursor(), 200);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn wraparound_split_slice() {
        let p = tmp_path("wrap");
        let cap: u64 = 256;
        let mut w = RingWriter::create(&p, cap).unwrap();
        let mut r = RingReader::open(&p, cap).unwrap();

        // Advance cursors to near-end by filling and draining once.
        let filler = vec![0u8; 200];
        w.write(&filler).unwrap();
        let mut tmp = vec![0u8; 200];
        assert_eq!(r.read(&mut tmp), 200);

        // Now write 100 bytes starting at offset 200 → wraps at 256.
        let payload: Vec<u8> = (0..100u8).collect();
        w.write(&payload).unwrap();

        // The available region must split into two contiguous slices.
        let (a, b) = r.available(256);
        assert_eq!(a.len() + b.len(), 100);
        assert!(!a.is_empty() && !b.is_empty(), "expected wrap, got {}+{}", a.len(), b.len());

        let mut out = vec![0u8; 100];
        assert_eq!(r.read(&mut out), 100);
        assert_eq!(out, payload);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn full_condition() {
        let p = tmp_path("full");
        let mut w = RingWriter::create(&p, 64).unwrap();
        w.write(&[0u8; 64]).unwrap();
        let err = w.write(&[0u8; 1]).unwrap_err();
        assert!(matches!(err, RingError::Full { .. }));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn multi_batch_stream() {
        let p = tmp_path("multi");
        let mut w = RingWriter::create(&p, 4096).unwrap();
        let mut r = RingReader::open(&p, 4096).unwrap();

        let mut total_written = Vec::new();
        for i in 0..32u8 {
            let batch = vec![i; 40]; // mimics 40-byte Event records
            w.write(&batch).unwrap();
            total_written.extend_from_slice(&batch);
        }

        let mut read_back = Vec::new();
        let mut tmp = [0u8; 128];
        loop {
            let n = r.read(&mut tmp);
            if n == 0 {
                break;
            }
            read_back.extend_from_slice(&tmp[..n]);
        }
        assert_eq!(read_back, total_written);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn event_stream_roundtrip() {
        // End-to-end: stream serialized Events through the ring and
        // confirm byte-identity on the consumer side.
        use flowlab_core::event::{Event, EventType};
        let p = tmp_path("events");
        let mut w = RingWriter::create(&p, 8192).unwrap();
        let mut r = RingReader::open(&p, 8192).unwrap();

        let events: Vec<Event> = (0..50)
            .map(|i| Event {
                ts: 1_000_000 + i as u64,
                price: 10_000 + i as u64 * 10,
                qty: 100,
                order_id: i as u64 + 1,
                instrument_id: 1,
                event_type: EventType::OrderAdd as u8,
                side: if i % 2 == 0 { 0 } else { 1 },
                _pad: [0; 2],
            })
            .collect();

        let bytes: &[u8] = bytemuck::cast_slice(&events);
        // Write + read in chunks that do not align to 40 bytes on
        // purpose, to stress partial record handling by the caller.
        w.write(&bytes[..800]).unwrap();
        let mut buf = vec![0u8; 800];
        assert_eq!(r.read(&mut buf), 800);
        w.write(&bytes[800..]).unwrap();
        let mut rest = vec![0u8; bytes.len() - 800];
        assert_eq!(r.read(&mut rest), rest.len());

        let mut reassembled = buf;
        reassembled.extend_from_slice(&rest);
        let parsed: &[Event] = bytemuck::cast_slice(&reassembled);
        assert_eq!(parsed.len(), events.len());
        for (a, b) in parsed.iter().zip(events.iter()) {
            assert_eq!(a.ts, b.ts);
            assert_eq!(a.order_id, b.order_id);
            assert_eq!(a.price, b.price);
        }
        std::fs::remove_file(&p).ok();
    }
}
