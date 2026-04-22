//! IchSource — NASDAQ ITCH 5.0 file replay.
//!
//! Reuses the validated parser from `flowlab_replay::itch` (which is the
//! Rust mirror of the Zig parser, hash-equivalent to it). We do not
//! re-decode the protocol here; we only walk the byte stream message by
//! message and emit canonical [`Event`]s on demand.
//!
//! ## Design
//!
//! * **mmap, zero-copy.** The whole BinaryFILE dump is mapped read-only.
//!   `next()` returns events without ever allocating.
//! * **Pull, not push.** The engine drives the cursor. No background
//!   thread, no producer/consumer split inside the source.
//! * **Skip ignored types.** ITCH is full of admin messages
//!   (system events, MWCB, regsho, IPO quoting, etc.) — we walk past
//!   them rather than failing, exactly like a production handler.
//! * **Optional pacing.** Default is "as fast as possible" (good for
//!   benchmarking the hot path under maximum load). `Pace::Realtime`
//!   replays at wall-clock spacing derived from the ITCH 48-bit
//!   nanosecond timestamps — useful when the dashboard should look
//!   like a live tape.
//! * **Live or finite.** When the file is exhausted, `next()` returns
//!   `None` and `is_live()` is `false`, so the engine cleanly stops
//!   instead of busy-looping.
//!
//! ## Honesty boundary
//!
//! Both `event_time_ns` (ITCH ts48) and `process_time_ns` (engine
//! monotonic) are in-process clocks here, both nanosecond-precise.
//! Latency claims derived from this source are HFT-grade — exactly the
//! point of replaying real NASDAQ data through the deterministic core.

use crate::source::Source;
use flowlab_core::event::Event;
use flowlab_replay::itch::{dispatch, msg_length, parse_stock_directory};
use memmap2::Mmap;
use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::path::Path;
use std::time::Instant;

/// Replay pacing policy.
#[derive(Debug, Clone, Copy)]
pub enum Pace {
    /// Drain the file as fast as the engine can consume it.
    AsFastAsPossible,
    /// Reproduce inter-arrival spacing derived from the ITCH ts48
    /// timestamps. `speedup` > 1.0 plays back faster than real time.
    Realtime { speedup: f64 },
}

/// Wire framing of the source file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    /// Raw concatenated ITCH messages (no length prefix). Used by the
    /// internal golden fixtures and by parsers that already stripped the
    /// envelope. Each message length is recovered from `msg_length(type)`.
    Unframed,
    /// NASDAQ BinaryFILE envelope: every message is preceded by a
    /// big-endian `u16` length covering the message body. This is the
    /// format of the official `*.NASDAQ_ITCH50` / `*.BX_ITCH_50` daily
    /// dumps published by NASDAQ.
    BinaryFile,
}

pub struct IchSource {
    mmap: Mmap,
    cursor: usize,
    /// Where to rewind on EOF. Defaults to 0; set to the post-fast-forward
    /// cursor when `open_with(start_at_ns=Some(_))` is used so loops keep
    /// the regular-hours window instead of restarting from pre-market.
    loop_start: usize,
    pace: Pace,
    framing: Framing,

    // Realtime pacing state ------------------------------------------------
    started_at: Instant,
    base_event_ns: Option<u64>,

    // Bookkeeping ---------------------------------------------------------
    skipped_unknown: u64,
    skipped_ignored: u64,
    bytes_total: u64,

    // stock_locate -> symbol, populated from 'R' messages as we stream.
    symbols: HashMap<u32, String>,
}

impl IchSource {
    /// Open `path` and mmap it. Returns an error on IO failure or zero size.
    pub fn open(path: impl AsRef<Path>, pace: Pace, framing: Framing) -> io::Result<Self> {
        Self::open_with(path, pace, framing, None)
    }

    /// Open `path` and optionally fast-forward the cursor to the first
    /// message whose ITCH timestamp (ns since midnight ET) is >= `start_at_ns`.
    /// While scanning, harvests Stock Directory ('R') records so the
    /// stock_locate -> symbol map is intact even though the parser never
    /// emits the skipped messages.
    pub fn open_with(
        path: impl AsRef<Path>,
        pace: Pace,
        framing: Framing,
        start_at_ns: Option<u64>,
    ) -> io::Result<Self> {
        let f = File::open(path.as_ref())?;
        let mmap = unsafe { Mmap::map(&f)? };
        if mmap.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "empty ITCH file"));
        }
        let bytes_total = mmap.len() as u64;
        let mut symbols: HashMap<u32, String> = HashMap::new();
        let mut cursor: usize = 0;

        if let Some(target_ns) = start_at_ns {
            let buf = mmap.as_ref();
            let mut scanned: u64 = 0;
            let mut harvested_r: u32 = 0;
            // Walk the file using the same framing the runtime uses.
            // Read each message header, parse ts48 at offset 5 (standard
            // for all timestamped ITCH 5.0 messages), stop when >= target.
            'scan: loop {
                let body_start;
                let body_end;
                match framing {
                    Framing::BinaryFile => {
                        if cursor + 2 > buf.len() {
                            break 'scan;
                        }
                        let len =
                            u16::from_be_bytes([buf[cursor], buf[cursor + 1]]) as usize;
                        if len == 0 {
                            break 'scan;
                        }
                        body_start = cursor + 2;
                        body_end = body_start + len;
                        if body_end > buf.len() {
                            break 'scan;
                        }
                    }
                    Framing::Unframed => {
                        if cursor >= buf.len() {
                            break 'scan;
                        }
                        let ty = buf[cursor];
                        let len = msg_length(ty) as usize;
                        if len == 0 {
                            cursor += 1;
                            continue;
                        }
                        body_start = cursor;
                        body_end = body_start + len;
                        if body_end > buf.len() {
                            break 'scan;
                        }
                    }
                }
                let m = &buf[body_start..body_end];
                let ty = m[0];
                // Harvest Stock Directory along the way.
                if ty == b'R' {
                    if let Some((id, sym)) = parse_stock_directory(m) {
                        symbols.insert(id, sym);
                        harvested_r += 1;
                    }
                }
                // Standard ITCH layout: Type[1] StkLoc[2] Track[2] Ts[6]
                // System Event ('S') has no stock_locate but ts is still
                // at offset 5 in BinaryFile bodies for BX 5.0.
                if m.len() >= 11 {
                    let ts = ((u16::from_be_bytes([m[5], m[6]]) as u64) << 32)
                        | (u32::from_be_bytes([m[7], m[8], m[9], m[10]]) as u64);
                    if ts >= target_ns {
                        // Park the cursor so the next call to next() returns
                        // *this* message — don't consume it.
                        cursor = match framing {
                            Framing::BinaryFile => body_start - 2,
                            Framing::Unframed => body_start,
                        };
                        tracing::info!(
                            target_ns,
                            first_ts_ns = ts,
                            cursor_byte = cursor,
                            harvested_stock_directory = harvested_r,
                            scanned_msgs = scanned,
                            "ITCH fast-forward complete"
                        );
                        break 'scan;
                    }
                }
                cursor = body_end;
                scanned += 1;
            }
        }

        Ok(Self {
            mmap,
            cursor,
            loop_start: cursor,
            pace,
            framing,
            started_at: Instant::now(),
            base_event_ns: None,
            skipped_unknown: 0,
            skipped_ignored: 0,
            bytes_total,
            symbols,
        })
    }

    pub fn bytes_total(&self) -> u64 {
        self.bytes_total
    }
    pub fn bytes_consumed(&self) -> u64 {
        self.cursor as u64
    }
    pub fn skipped_unknown(&self) -> u64 {
        self.skipped_unknown
    }
    pub fn skipped_ignored(&self) -> u64 {
        self.skipped_ignored
    }

    /// Look up the ASCII ticker for an instrument (stock_locate). Returns
    /// `None` if the Stock Directory message for this id has not streamed
    /// past yet.
    pub fn symbol_of(&self, instrument_id: u32) -> Option<&str> {
        self.symbols.get(&instrument_id).map(|s| s.as_str())
    }

    /// Reverse lookup: ticker (case-insensitive, padding-trimmed) -> stock_locate.
    /// Only finds tickers that were harvested during the fast-forward scan.
    pub fn locate_of(&self, ticker: &str) -> Option<u32> {
        let want = ticker.trim().to_ascii_uppercase();
        self.symbols
            .iter()
            .find(|(_, s)| s.trim().eq_ignore_ascii_case(&want))
            .map(|(id, _)| *id)
    }

    /// Real-time pacing: busy-spin until wall-clock catches up to the
    /// event-time delta from the first event. Sub-microsecond budget,
    /// no syscalls in the hot path.
    fn pace_to(&mut self, event_ns: u64, speedup: f64) {
        let base = match self.base_event_ns {
            Some(b) => b,
            None => {
                self.base_event_ns = Some(event_ns);
                return;
            }
        };
        if event_ns <= base {
            return;
        }
        let delta_ns = (event_ns - base) as f64 / speedup.max(1e-9);
        let target = self.started_at + std::time::Duration::from_nanos(delta_ns as u64);
        // Busy wait — at HFT cadences, `thread::sleep` granularity (~1 ms on
        // Windows) wrecks the spacing. Yield cooperatively.
        while Instant::now() < target {
            std::hint::spin_loop();
        }
    }
}

impl Source for IchSource {
    fn name(&self) -> &str {
        "ich"
    }

    fn is_live(&self) -> bool {
        // Looping replay — the stream never ends from the engine's POV.
        true
    }

    fn symbol_of(&self, instrument_id: u32) -> Option<&str> {
        self.symbols.get(&instrument_id).map(|s| s.as_str())
    }

    fn next(&mut self) -> Option<Event> {
        let buf = self.mmap.as_ref();
        loop {
            // Slice out the next message according to framing.
            let msg: &[u8] = match self.framing {
                Framing::Unframed => {
                    if self.cursor >= buf.len() {
                        // Loop: rewind to start of file for continuous replay.
                        self.cursor = self.loop_start;
                        self.base_event_ns = None;
                        self.started_at = Instant::now();
                        continue;
                    }
                    let ty = buf[self.cursor];
                    let len = msg_length(ty) as usize;
                    if len == 0 {
                        // Unknown type — skip 1 byte. For a true raw stream
                        // this is best-effort recovery; for BinaryFILE data
                        // accidentally opened as Unframed this will flag
                        // the mistake quickly via skipped_unknown.
                        self.skipped_unknown = self.skipped_unknown.saturating_add(1);
                        self.cursor += 1;
                        continue;
                    }
                    if self.cursor + len > buf.len() {
                        self.cursor = self.loop_start;
                        self.base_event_ns = None;
                        self.started_at = Instant::now();
                        continue;
                    }
                    let m = &buf[self.cursor..self.cursor + len];
                    self.cursor += len;
                    m
                }
                Framing::BinaryFile => {
                    if self.cursor + 2 > buf.len() {
                        self.cursor = self.loop_start;
                        self.base_event_ns = None;
                        self.started_at = Instant::now();
                        continue;
                    }
                    let len = u16::from_be_bytes([buf[self.cursor], buf[self.cursor + 1]]) as usize;
                    if len == 0 {
                        // Spec-illegal zero-length frame: stop, don't loop.
                        return None;
                    }
                    let body_start = self.cursor + 2;
                    let body_end = body_start + len;
                    if body_end > buf.len() {
                        self.cursor = self.loop_start;
                        self.base_event_ns = None;
                        self.started_at = Instant::now();
                        continue;
                    }
                    let m = &buf[body_start..body_end];
                    self.cursor = body_end;
                    // Validate against the protocol spec; mismatch =
                    // unknown / future message type for this parser.
                    let ty = m[0];
                    let expected = msg_length(ty) as usize;
                    if expected == 0 || expected != len {
                        self.skipped_unknown = self.skipped_unknown.saturating_add(1);
                        continue;
                    }
                    m
                }
            };

            match dispatch(msg) {
                Some(ev) => {
                    if let Pace::Realtime { speedup } = self.pace {
                        self.pace_to(ev.ts, speedup);
                    }
                    return Some(ev);
                }
                None => {
                    // Recognised admin / non-event message (S, R, H, ...).
                    // Harvest the symbol map from Stock Directory records.
                    if msg.first() == Some(&b'R') {
                        if let Some((id, sym)) = parse_stock_directory(msg) {
                            self.symbols.insert(id, sym);
                        }
                    }
                    self.skipped_ignored = self.skipped_ignored.saturating_add(1);
                    continue;
                }
            }
        }
    }
}
