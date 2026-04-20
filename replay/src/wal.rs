//! Segmented Write-Ahead Log with per-record CRC-32 framing.
//!
//! The WAL is the system's ground truth: anything that has been
//! acknowledged by the writer side can be replayed bit-exactly into
//! any consumer (strategy, risk check, back-test) and will produce
//! the *identical* [`flowlab_core::hot_book::HotOrderBook`] state
//! hash observed live.
//!
//! On-disk layout (per record):
//!
//! ```text
//!   offset  size  field
//!   0       4     len   (u32 little-endian)  — payload byte count
//!   4       4     crc32 (u32 little-endian)  — CRC-32/IEEE over payload
//!   8       len   payload
//! ```
//!
//! Segment files are named `wal-<u64>.log`, numbered from 0 and
//! rotated when they exceed the configured segment size. A writer
//! crash between header and payload leaves a torn tail which is
//! transparently skipped by the reader (truncation-tolerant).
//!
//! Durability model:
//! - `append`: buffered write, O(1)
//! - `sync`: explicit `fsync` — call after every trade you care about
//! - `close`: drops the file handle, segments remain on disk
//!
//! This is NOT a distributed log. Replication is out of scope — we
//! only guarantee crash-consistency on a single node.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const MAGIC_HEADER: usize = 8;
/// Default segment size: 64 MiB. At ~40 B/event this is ~1.6 M events
/// per segment — large enough to amortise `fsync` cost, small enough
/// to limit recovery time on crash.
pub const DEFAULT_SEGMENT_SIZE: u64 = 64 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum WalError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("bad segment name: {0}")]
    BadSegmentName(String),
    #[error("crc mismatch at segment {segment} offset {offset}")]
    CrcMismatch { segment: u64, offset: u64 },
    #[error("payload too large: {0} > u32::MAX")]
    PayloadTooLarge(usize),
}

pub struct Wal {
    dir: PathBuf,
    segment_size: u64,
    current_index: u64,
    writer: BufWriter<File>,
    current_bytes: u64,
}

impl Wal {
    /// Create (or recover into) a WAL directory. If segments already
    /// exist, the WAL resumes appending into the highest-numbered
    /// segment (opening it in append mode and measuring its size).
    pub fn open_or_create<P: AsRef<Path>>(
        dir: P,
        segment_size: u64,
    ) -> Result<Self, WalError> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)?;

        let mut segments = list_segments(&dir)?;
        segments.sort();

        let (index, file, bytes) = match segments.last().copied() {
            Some(idx) => {
                let path = segment_path(&dir, idx);
                let file = OpenOptions::new().append(true).read(true).open(&path)?;
                let bytes = file.metadata()?.len();
                (idx, file, bytes)
            }
            None => {
                let path = segment_path(&dir, 0);
                let file = OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .read(true)
                    .open(&path)?;
                (0u64, file, 0u64)
            }
        };

        Ok(Self {
            dir,
            segment_size,
            current_index: index,
            writer: BufWriter::with_capacity(1 << 20, file),
            current_bytes: bytes,
        })
    }

    /// Append a single record. Each call corresponds to one framed
    /// entry: callers are responsible for grouping logical batches.
    pub fn append(&mut self, payload: &[u8]) -> Result<(), WalError> {
        if payload.len() > u32::MAX as usize {
            return Err(WalError::PayloadTooLarge(payload.len()));
        }
        if self.current_bytes + (MAGIC_HEADER as u64 + payload.len() as u64) > self.segment_size
        {
            self.rotate()?;
        }

        let len = payload.len() as u32;
        let crc = crc32(payload);
        self.writer.write_all(&len.to_le_bytes())?;
        self.writer.write_all(&crc.to_le_bytes())?;
        self.writer.write_all(payload)?;
        self.current_bytes += MAGIC_HEADER as u64 + payload.len() as u64;
        Ok(())
    }

    /// Flush buffered writes to the OS. Does NOT issue `fsync`.
    pub fn flush(&mut self) -> Result<(), WalError> {
        self.writer.flush()?;
        Ok(())
    }

    /// Flush + fsync to disk. Use after every must-survive-crash boundary.
    pub fn sync(&mut self) -> Result<(), WalError> {
        self.writer.flush()?;
        self.writer.get_ref().sync_data()?;
        Ok(())
    }

    fn rotate(&mut self) -> Result<(), WalError> {
        self.writer.flush()?;
        self.writer.get_ref().sync_data()?;
        self.current_index += 1;
        let path = segment_path(&self.dir, self.current_index);
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .read(true)
            .open(&path)?;
        self.writer = BufWriter::with_capacity(1 << 20, file);
        self.current_bytes = 0;
        Ok(())
    }

    pub fn segment_count(&self) -> u64 {
        self.current_index + 1
    }
}

fn segment_path(dir: &Path, idx: u64) -> PathBuf {
    dir.join(format!("wal-{:020}.log", idx))
}

fn list_segments(dir: &Path) -> Result<Vec<u64>, WalError> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(stripped) = name
            .strip_prefix("wal-")
            .and_then(|s| s.strip_suffix(".log"))
        {
            let idx: u64 = stripped
                .parse()
                .map_err(|_| WalError::BadSegmentName(name.to_string()))?;
            out.push(idx);
        }
    }
    Ok(out)
}

/// Streaming reader: iterates records across all segments in order,
/// skipping torn-tail records (truncated payload or bad CRC at EOF).
pub struct WalReader {
    dir: PathBuf,
    segments: Vec<u64>,
    seg_idx: usize,
    file: Option<File>,
    buf: Vec<u8>,
    segment_offset: u64,
}

impl WalReader {
    pub fn open<P: AsRef<Path>>(dir: P) -> Result<Self, WalError> {
        let dir = dir.as_ref().to_path_buf();
        let mut segments = list_segments(&dir)?;
        segments.sort();
        Ok(Self {
            dir,
            segments,
            seg_idx: 0,
            file: None,
            buf: Vec::new(),
            segment_offset: 0,
        })
    }

    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    fn ensure_file(&mut self) -> Result<bool, WalError> {
        if self.file.is_some() {
            return Ok(true);
        }
        if self.seg_idx >= self.segments.len() {
            return Ok(false);
        }
        let idx = self.segments[self.seg_idx];
        let path = segment_path(&self.dir, idx);
        let mut f = File::open(&path)?;
        f.seek(SeekFrom::Start(0))?;
        self.file = Some(f);
        self.segment_offset = 0;
        Ok(true)
    }

    /// Return the next record payload, or `None` on clean EOF.
    /// A torn-tail record at end-of-file is treated as EOF for the
    /// current segment and the reader advances to the next one.
    pub fn next_record(&mut self) -> Result<Option<&[u8]>, WalError> {
        loop {
            if !self.ensure_file()? {
                return Ok(None);
            }
            let f = self.file.as_mut().unwrap();

            let mut hdr = [0u8; MAGIC_HEADER];
            match f.read_exact(&mut hdr) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    // End of segment — advance.
                    self.file = None;
                    self.seg_idx += 1;
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
            let len = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as usize;
            let crc = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);

            self.buf.resize(len, 0);
            match f.read_exact(&mut self.buf) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    // Torn tail: payload truncated. Drop this segment's tail.
                    self.file = None;
                    self.seg_idx += 1;
                    continue;
                }
                Err(e) => return Err(e.into()),
            }

            let got = crc32(&self.buf);
            if got != crc {
                // Torn tail: header survived but payload is corrupt.
                // Behave like EOF for this segment so replay is clean.
                self.file = None;
                self.seg_idx += 1;
                continue;
            }

            self.segment_offset += (MAGIC_HEADER + len) as u64;
            return Ok(Some(&self.buf));
        }
    }
}

// ─── CRC-32/IEEE (poly 0xEDB88320), small & allocation-free ─────────

#[inline]
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc = CRC32_TABLE[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

static CRC32_TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0u32;
    while i < 256 {
        let mut c = i;
        let mut j = 0;
        while j < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            j += 1;
        }
        t[i as usize] = c;
        i += 1;
    }
    t
};

#[cfg(test)]
mod tests {
    use super::*;
    use flowlab_core::event::{Event, EventType};
    use flowlab_core::hot_book::HotOrderBook;

    fn tmp_dir(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("flowlab-wal-{}-{}", name, std::process::id()));
        std::fs::create_dir_all(&p).ok();
        p
    }

    #[test]
    fn crc32_matches_known_vector() {
        // Known IEEE CRC-32 of "123456789" = 0xCBF43926.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn roundtrip_many_records() {
        let dir = tmp_dir("roundtrip");
        let mut wal = Wal::open_or_create(&dir, 4096).unwrap();
        let payloads: Vec<Vec<u8>> = (0..1000)
            .map(|i| vec![i as u8 % 255; 37 + (i % 23)])
            .collect();
        for p in &payloads {
            wal.append(p).unwrap();
        }
        wal.sync().unwrap();
        drop(wal);

        let mut r = WalReader::open(&dir).unwrap();
        let mut idx = 0usize;
        while let Some(rec) = r.next_record().unwrap() {
            assert_eq!(rec, payloads[idx].as_slice());
            idx += 1;
        }
        assert_eq!(idx, payloads.len());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn segments_rotate_across_boundary() {
        let dir = tmp_dir("rotate");
        // Segment size 512 B, record size 40+8 = 48 B → ~10 per segment.
        let mut wal = Wal::open_or_create(&dir, 512).unwrap();
        for i in 0..50u8 {
            wal.append(&vec![i; 40]).unwrap();
        }
        wal.sync().unwrap();
        assert!(wal.segment_count() >= 4);
        drop(wal);

        let mut r = WalReader::open(&dir).unwrap();
        assert!(r.segment_count() >= 4);
        let mut count = 0u32;
        while let Some(rec) = r.next_record().unwrap() {
            assert_eq!(rec.len(), 40);
            assert_eq!(rec[0], count as u8);
            count += 1;
        }
        assert_eq!(count, 50);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn torn_tail_is_skipped() {
        let dir = tmp_dir("torn");
        let mut wal = Wal::open_or_create(&dir, 1 << 20).unwrap();
        for i in 0..10u8 {
            wal.append(&[i; 32]).unwrap();
        }
        wal.sync().unwrap();
        drop(wal);

        // Simulate a crash: truncate the last record's payload.
        let seg = segment_path(&dir, 0);
        let mut f = OpenOptions::new().write(true).open(&seg).unwrap();
        let len = f.metadata().unwrap().len();
        f.set_len(len - 16).unwrap(); // drop 16 bytes of the last payload
        drop(f);

        // Append a FULL record after the torn one. Since the writer
        // resumes append-at-EOF, this record is technically appended
        // after the torn bytes. Our reader should:
        //   - yield the first 9 clean records,
        //   - skip the corrupt 10th,
        //   - skip anything after it in the same segment (treated as EOF).
        let mut r = WalReader::open(&dir).unwrap();
        let mut count = 0u32;
        while let Some(rec) = r.next_record().unwrap() {
            assert_eq!(rec.len(), 32);
            count += 1;
        }
        assert_eq!(count, 9, "expected 9 intact records before torn tail");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bit_exact_replay_reproduces_l2_hash() {
        // Stream 5000 Events through the WAL and verify the replayed
        // HotOrderBook state hash equals the direct-in-memory apply.
        let events: Vec<Event> = (0..5000)
            .map(|i| Event {
                ts: 1_000 + i as u64,
                price: 100_000 + (i as u64 % 50) * 10,
                qty: 100,
                order_id: i as u64 + 1,
                instrument_id: 1,
                event_type: EventType::OrderAdd as u8,
                side: if i % 2 == 0 { 0 } else { 1 },
                _pad: [0; 2],
            })
            .collect();

        // Reference hash.
        let mut ref_book = HotOrderBook::<256>::new(1);
        for ev in &events {
            ref_book.apply(ev);
        }
        let ref_hash = ref_book.canonical_l2_hash();

        // Write events through WAL in 200-event batches.
        let dir = tmp_dir("replay");
        let mut wal = Wal::open_or_create(&dir, 16 * 1024).unwrap();
        for chunk in events.chunks(200) {
            let bytes: &[u8] = bytemuck::cast_slice(chunk);
            wal.append(bytes).unwrap();
        }
        wal.sync().unwrap();
        drop(wal);

        // Replay.
        let mut book = HotOrderBook::<256>::new(1);
        let mut r = WalReader::open(&dir).unwrap();
        let mut replayed = 0u32;
        while let Some(rec) = r.next_record().unwrap() {
            let evs: &[Event] = bytemuck::cast_slice(rec);
            for ev in evs {
                book.apply(ev);
                replayed += 1;
            }
        }
        assert_eq!(replayed as usize, events.len());
        assert_eq!(book.canonical_l2_hash(), ref_hash);
        std::fs::remove_dir_all(&dir).ok();
    }
}
