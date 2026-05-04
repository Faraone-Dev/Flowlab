// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

//! MoldUDP64 framing + sequence-gap recovery.
//!
//! NASDAQ multicast transport for ITCH. 20-byte header:
//!
//! ```text
//!   [0..10)  session   — 10-byte ASCII
//!   [10..18) sequence  — u64 BE, seq of FIRST message in frame
//!   [18..20) count     — u16 BE, message count
//! ```
//!
//! Body: `count` back-to-back `[u16 BE len][msg]`. Specials:
//! `count == 0xFFFF` → End-of-Session, `count == 0x0000` → Heartbeat.
//!
//! [`MoldFrame::parse`] is alloc-free; [`GapTracker`] buffers forward
//! frames until reorder/retransmit fills the gap.

use std::collections::BTreeMap;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MoldError {
    #[error("frame too short: need {need} bytes, got {got}")]
    TooShort { need: usize, got: usize },
    #[error("declared message length exceeds frame: want {want} bytes at offset {offset}")]
    MessageOverflow { want: usize, offset: usize },
    #[error("unexpected trailing bytes: {0}")]
    TrailingBytes(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MoldHeader {
    pub session: [u8; 10],
    pub sequence: u64,
    pub count: u16,
}

impl MoldHeader {
    pub const SIZE: usize = 20;
    pub const END_OF_SESSION: u16 = 0xFFFF;
    pub const HEARTBEAT: u16 = 0x0000;

    #[inline]
    pub fn is_heartbeat(&self) -> bool {
        self.count == Self::HEARTBEAT
    }

    #[inline]
    pub fn is_end_of_session(&self) -> bool {
        self.count == Self::END_OF_SESSION
    }
}

/// Borrowed view over a parsed MoldUDP64 frame. Iterates messages
/// without copying them out of the original datagram buffer.
#[derive(Debug)]
pub struct MoldFrame<'a> {
    pub header: MoldHeader,
    body: &'a [u8],
}

impl<'a> MoldFrame<'a> {
    pub fn parse(datagram: &'a [u8]) -> Result<Self, MoldError> {
        if datagram.len() < MoldHeader::SIZE {
            return Err(MoldError::TooShort {
                need: MoldHeader::SIZE,
                got: datagram.len(),
            });
        }
        let mut session = [0u8; 10];
        session.copy_from_slice(&datagram[..10]);
        let sequence = u64::from_be_bytes(datagram[10..18].try_into().unwrap());
        let count = u16::from_be_bytes(datagram[18..20].try_into().unwrap());
        let header = MoldHeader {
            session,
            sequence,
            count,
        };
        let body = &datagram[MoldHeader::SIZE..];
        // Heartbeat / EoS have no body.
        if header.is_heartbeat() || header.is_end_of_session() {
            if !body.is_empty() {
                return Err(MoldError::TrailingBytes(body.len()));
            }
        }
        Ok(Self { header, body })
    }

    /// Iterate every message carried by this frame as a `(sequence, payload)`
    /// pair. `sequence` is the 64-bit global sequence number of that message.
    pub fn messages(&self) -> MoldMessageIter<'a> {
        MoldMessageIter {
            body: self.body,
            offset: 0,
            next_seq: self.header.sequence,
            remaining: if self.header.is_heartbeat() || self.header.is_end_of_session() {
                0
            } else {
                self.header.count as u32
            },
        }
    }
}

pub struct MoldMessageIter<'a> {
    body: &'a [u8],
    offset: usize,
    next_seq: u64,
    remaining: u32,
}

impl<'a> Iterator for MoldMessageIter<'a> {
    type Item = Result<(u64, &'a [u8]), MoldError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        if self.offset + 2 > self.body.len() {
            return Some(Err(MoldError::TooShort {
                need: 2,
                got: self.body.len() - self.offset,
            }));
        }
        let len = u16::from_be_bytes([self.body[self.offset], self.body[self.offset + 1]])
            as usize;
        let start = self.offset + 2;
        let end = start + len;
        if end > self.body.len() {
            return Some(Err(MoldError::MessageOverflow {
                want: len,
                offset: self.offset,
            }));
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.offset = end;
        self.remaining -= 1;
        Some(Ok((seq, &self.body[start..end])))
    }
}

/// In-order sequence-gap tracker with a bounded out-of-order buffer.
///
/// Typical usage:
/// ```ignore
/// let mut tracker = GapTracker::new(1); // expect first seq == 1
/// for frame in incoming_frames {
///     for delivery in tracker.ingest(&frame) {
///         handle(delivery);
///     }
///     if let Some(gap) = tracker.open_gap() {
///         request_retransmit(gap);
///     }
/// }
/// ```
pub struct GapTracker {
    expected: u64,
    /// Sparse forward buffer: seq → owned payload. Bounded in size to
    /// avoid unbounded memory growth if a gap never fills.
    pending: BTreeMap<u64, Vec<u8>>,
    max_buffered: usize,
    stats: GapStats,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct GapStats {
    pub in_order_delivered: u64,
    pub reordered_delivered: u64,
    pub gaps_opened: u64,
    pub gaps_closed: u64,
    pub dropped_overflow: u64,
    pub duplicates_ignored: u64,
}

/// A single message delivered to the consumer in strict sequence order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivered {
    pub sequence: u64,
    pub payload: Vec<u8>,
}

impl GapTracker {
    pub fn new(start_sequence: u64) -> Self {
        Self {
            expected: start_sequence,
            pending: BTreeMap::new(),
            max_buffered: 16 * 1024,
            stats: GapStats::default(),
        }
    }

    pub fn with_capacity(start_sequence: u64, max_buffered: usize) -> Self {
        Self {
            expected: start_sequence,
            pending: BTreeMap::new(),
            max_buffered,
            stats: GapStats::default(),
        }
    }

    pub fn stats(&self) -> GapStats {
        self.stats
    }

    pub fn expected(&self) -> u64 {
        self.expected
    }

    /// Currently-open sequence gap, if any. Returns the *inclusive*
    /// range `[first, last]` of missing sequence numbers, or `None`
    /// if no gap is open.
    pub fn open_gap(&self) -> Option<(u64, u64)> {
        let first_buffered = *self.pending.keys().next()?;
        if first_buffered > self.expected {
            Some((self.expected, first_buffered - 1))
        } else {
            None
        }
    }

    /// Consume a single MoldUDP64 frame and return every message that
    /// can now be delivered in-order. Messages buffered by an earlier
    /// call may drain along with the one that filled the gap.
    pub fn ingest(&mut self, frame: &MoldFrame<'_>) -> Vec<Delivered> {
        let mut out = Vec::new();
        for msg in frame.messages() {
            let (seq, payload) = match msg {
                Ok(p) => p,
                Err(_) => continue, // ignore malformed mid-stream
            };
            self.ingest_one(seq, payload, &mut out);
        }
        out
    }

    /// Inject a single `(seq, payload)` directly — useful for
    /// retransmit replies that do not come wrapped in a MoldFrame.
    pub fn ingest_single(&mut self, seq: u64, payload: &[u8]) -> Vec<Delivered> {
        let mut out = Vec::new();
        self.ingest_one(seq, payload, &mut out);
        out
    }

    fn ingest_one(&mut self, seq: u64, payload: &[u8], out: &mut Vec<Delivered>) {
        if seq < self.expected {
            self.stats.duplicates_ignored += 1;
            return;
        }
        if seq == self.expected {
            out.push(Delivered {
                sequence: seq,
                payload: payload.to_vec(),
            });
            self.stats.in_order_delivered += 1;
            self.expected += 1;
            // Drain any forward pending entries that are now contiguous.
            while let Some((&next, _)) = self.pending.iter().next() {
                if next == self.expected {
                    let buf = self.pending.remove(&next).unwrap();
                    out.push(Delivered {
                        sequence: next,
                        payload: buf,
                    });
                    self.stats.reordered_delivered += 1;
                    self.expected += 1;
                    if self.pending.is_empty()
                        || *self.pending.keys().next().unwrap() != self.expected
                    {
                        // Gap just closed if we still have forward buffered.
                        if !self.pending.is_empty() {
                            self.stats.gaps_closed += 1;
                        }
                    }
                } else {
                    break;
                }
            }
            return;
        }
        // seq > expected: forward buffer
        if self.pending.is_empty() {
            self.stats.gaps_opened += 1;
        }
        if self.pending.len() >= self.max_buffered {
            self.stats.dropped_overflow += 1;
            return;
        }
        // Dedupe forward duplicates silently.
        if self.pending.contains_key(&seq) {
            self.stats.duplicates_ignored += 1;
            return;
        }
        self.pending.insert(seq, payload.to_vec());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a MoldUDP64 datagram from `(sequence_of_first, messages)`.
    fn encode(session: &[u8; 10], first_seq: u64, msgs: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::with_capacity(20 + msgs.iter().map(|m| 2 + m.len()).sum::<usize>());
        out.extend_from_slice(session);
        out.extend_from_slice(&first_seq.to_be_bytes());
        out.extend_from_slice(&(msgs.len() as u16).to_be_bytes());
        for m in msgs {
            out.extend_from_slice(&(m.len() as u16).to_be_bytes());
            out.extend_from_slice(m);
        }
        out
    }

    const SESSION: &[u8; 10] = b"ITCH50TEST";

    #[test]
    fn frame_roundtrip_basic() {
        let dg = encode(SESSION, 100, &[b"hello", b"world"]);
        let f = MoldFrame::parse(&dg).unwrap();
        assert_eq!(f.header.session, *SESSION);
        assert_eq!(f.header.sequence, 100);
        assert_eq!(f.header.count, 2);
        let msgs: Vec<_> = f.messages().collect::<Result<_, _>>().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0], (100, b"hello".as_slice()));
        assert_eq!(msgs[1], (101, b"world".as_slice()));
    }

    #[test]
    fn heartbeat_has_no_body() {
        let mut dg = encode(SESSION, 42, &[]);
        // Rewrite count to 0 explicitly (heartbeat).
        dg[18..20].copy_from_slice(&0u16.to_be_bytes());
        let f = MoldFrame::parse(&dg).unwrap();
        assert!(f.header.is_heartbeat());
        assert_eq!(f.messages().count(), 0);
    }

    #[test]
    fn end_of_session_detected() {
        let mut dg = vec![0u8; 20];
        dg[..10].copy_from_slice(SESSION);
        dg[10..18].copy_from_slice(&1_000u64.to_be_bytes());
        dg[18..20].copy_from_slice(&MoldHeader::END_OF_SESSION.to_be_bytes());
        let f = MoldFrame::parse(&dg).unwrap();
        assert!(f.header.is_end_of_session());
    }

    #[test]
    fn too_short_is_rejected() {
        let err = MoldFrame::parse(&[0u8; 10]).unwrap_err();
        assert!(matches!(err, MoldError::TooShort { .. }));
    }

    #[test]
    fn message_overflow_is_rejected() {
        // Declare 100-byte message in a 20-byte body.
        let mut dg = Vec::from(&encode(SESSION, 1, &[b"abc"])[..]);
        let body_start = 20;
        dg[body_start..body_start + 2].copy_from_slice(&100u16.to_be_bytes());
        let f = MoldFrame::parse(&dg).unwrap();
        let err = f.messages().next().unwrap().unwrap_err();
        assert!(matches!(err, MoldError::MessageOverflow { .. }));
    }

    #[test]
    fn in_order_delivery_no_gaps() {
        let mut t = GapTracker::new(1);
        let dg = encode(SESSION, 1, &[b"a", b"b", b"c"]);
        let f = MoldFrame::parse(&dg).unwrap();
        let delivered = t.ingest(&f);
        assert_eq!(delivered.len(), 3);
        assert_eq!(delivered[0].sequence, 1);
        assert_eq!(delivered[2].sequence, 3);
        assert_eq!(t.expected(), 4);
        assert!(t.open_gap().is_none());
        assert_eq!(t.stats().gaps_opened, 0);
    }

    #[test]
    fn gap_is_bridged_by_retransmit() {
        let mut t = GapTracker::new(1);

        // Receive seq 1, then 3, 4, 5 — seq 2 is missing.
        let dg1 = encode(SESSION, 1, &[b"m1"]);
        let dg2 = encode(SESSION, 3, &[b"m3", b"m4", b"m5"]);
        let d1 = t.ingest(&MoldFrame::parse(&dg1).unwrap());
        assert_eq!(d1.len(), 1);
        let d2 = t.ingest(&MoldFrame::parse(&dg2).unwrap());
        assert_eq!(d2.len(), 0, "must not deliver across a gap");
        assert_eq!(t.open_gap(), Some((2, 2)));
        assert_eq!(t.stats().gaps_opened, 1);

        // Retransmit fills seq 2 → drains 2,3,4,5 in order.
        let drained = t.ingest_single(2, b"m2");
        assert_eq!(drained.len(), 4);
        let seqs: Vec<u64> = drained.iter().map(|d| d.sequence).collect();
        assert_eq!(seqs, vec![2, 3, 4, 5]);
        assert!(t.open_gap().is_none());
        assert_eq!(t.expected(), 6);
    }

    #[test]
    fn duplicate_is_ignored() {
        let mut t = GapTracker::new(10);
        t.ingest_single(10, b"x");
        let again = t.ingest_single(10, b"x");
        assert!(again.is_empty());
        assert_eq!(t.stats().duplicates_ignored, 1);
    }

    #[test]
    fn forward_overflow_is_bounded() {
        let mut t = GapTracker::with_capacity(1, 8);
        // Keep seq 1 missing; push 20 forward items.
        for s in 2..22 {
            t.ingest_single(s, b"p");
        }
        assert!(t.stats().dropped_overflow >= 12);
        assert_eq!(t.stats().gaps_opened, 1);
    }

    #[test]
    fn reorder_multi_frame() {
        // Simulate [f1=seq1-2][f3=seq5-6][f2=seq3-4] → final order 1..=6.
        let mut t = GapTracker::new(1);
        let f1 = encode(SESSION, 1, &[b"a", b"b"]);
        let f3 = encode(SESSION, 5, &[b"e", b"f"]);
        let f2 = encode(SESSION, 3, &[b"c", b"d"]);

        let d1 = t.ingest(&MoldFrame::parse(&f1).unwrap());
        let d3 = t.ingest(&MoldFrame::parse(&f3).unwrap());
        let d2 = t.ingest(&MoldFrame::parse(&f2).unwrap());

        assert_eq!(d1.iter().map(|d| d.sequence).collect::<Vec<_>>(), vec![1, 2]);
        assert!(d3.is_empty(), "gap [3,4] blocks 5,6");
        let seqs: Vec<u64> = d2.iter().map(|d| d.sequence).collect();
        assert_eq!(seqs, vec![3, 4, 5, 6]);
        assert!(t.open_gap().is_none());
    }
}
