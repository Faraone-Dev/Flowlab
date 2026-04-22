//! NASDAQ ITCH 5.0 — Rust reference parser + synthetic stream generator.
//!
//! The parser logic mirrors `feed-parser/src/itch.zig` byte-for-byte so the
//! integration benchmark can validate end-to-end correctness without
//! requiring the Zig static library at build time.
//!
//! Message layout (big-endian on the wire):
//!   'A' AddOrder (no MPID)      36 bytes
//!   'F' AddOrder (with MPID)    40 bytes
//!   'D' OrderDelete             19 bytes
//!   'X' OrderCancel             23 bytes
//!   'E' OrderExecuted           31 bytes
//!   'C' OrderExecutedWithPrice  36 bytes
//!   'U' OrderReplace            35 bytes
//!   'P' TradeNonCross           44 bytes
//!   'S' SystemEvent             12 bytes

use flowlab_core::event::{Event, EventType, Side};

/// ITCH 5.0 message length lookup — unknown types return 0.
#[inline]
pub fn msg_length(t: u8) -> u16 {
    match t {
        b'S' => 12,
        b'R' => 39,
        b'H' => 25,
        b'Y' => 20,
        b'L' => 26,
        b'V' => 35,
        b'W' => 12,
        b'K' => 28,
        b'J' => 35,
        b'h' => 21,
        b'A' => 36,
        b'F' => 40,
        b'E' => 31,
        b'C' => 36,
        b'X' => 23,
        b'D' => 19,
        b'U' => 35,
        b'P' => 44,
        b'Q' => 40,
        b'B' => 19,
        b'I' => 50,
        b'N' => 20,
        _ => 0,
    }
}

#[inline(always)]
fn rd_u16(b: &[u8], off: usize) -> u16 {
    u16::from_be_bytes([b[off], b[off + 1]])
}

#[inline(always)]
fn rd_u32(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

#[inline(always)]
fn rd_u64(b: &[u8], off: usize) -> u64 {
    u64::from_be_bytes([
        b[off], b[off + 1], b[off + 2], b[off + 3],
        b[off + 4], b[off + 5], b[off + 6], b[off + 7],
    ])
}

/// 6-byte timestamp: 2 bytes high + 4 bytes low.
#[inline(always)]
fn rd_ts48(b: &[u8], off: usize) -> u64 {
    let hi = rd_u16(b, off) as u64;
    let lo = rd_u32(b, off + 2) as u64;
    (hi << 32) | lo
}

#[inline]
fn event_zeroed() -> Event {
    // SAFETY: Event is Pod (bytemuck).
    unsafe { core::mem::zeroed() }
}

/// Stock Directory ('R') message — emitted once per ticker at session
/// start. Gives us the `stock_locate -> symbol` map the rest of the feed
/// assumes is already known.
///
/// Layout (39 bytes total):
///   Type[1] StkLoc[2] TrackNum[2] Timestamp[6] Stock[8] ...
///
/// Returns `(stock_locate, symbol)` with symbol trimmed of trailing spaces.
pub fn parse_stock_directory(m: &[u8]) -> Option<(u32, String)> {
    if m.len() < 19 || m[0] != b'R' {
        return None;
    }
    let stock_locate = rd_u16(m, 1) as u32;
    let raw = &m[11..19];
    let end = raw.iter().rposition(|&c| c != b' ').map(|i| i + 1).unwrap_or(0);
    let sym = std::str::from_utf8(&raw[..end]).ok()?.to_string();
    Some((stock_locate, sym))
}

/// Parse a single ITCH message starting at `m`. Returns Some(Event) if
/// the message type produces a canonical event, None for ignored types.
pub fn dispatch(m: &[u8]) -> Option<Event> {
    match *m.first()? {
        // Add Order (no MPID) or Add Order (MPID) — same payload layout
        b'A' | b'F' => parse_add_order(m),
        // Order Delete
        b'D' => parse_order_delete(m),
        // Order Cancel (partial)
        b'X' => parse_order_cancel(m),
        // Order Executed / Executed with Price → Trade
        b'E' | b'C' => parse_order_executed(m),
        // Trade (Non-Cross)
        b'P' => parse_trade(m),
        _ => None,
    }
}

fn parse_add_order(m: &[u8]) -> Option<Event> {
    // A: Type[1] StkLoc[2] TrackNum[2] Timestamp[6] OrderRefNum[8]
    //    BuySell[1] Shares[4] Stock[8] Price[4]                   = 36
    if m.len() < 36 { return None; }
    let stock_locate = rd_u16(m, 1) as u32;
    let ts = rd_ts48(m, 5);
    let order_id = rd_u64(m, 11);
    let side = if m[19] == b'B' { Side::Bid as u8 } else { Side::Ask as u8 };
    let shares = rd_u32(m, 20) as u64;
    let price = rd_u32(m, 32) as u64; // fixed-point 1/10000
    let mut e = event_zeroed();
    e.ts = ts;
    e.price = price;
    e.qty = shares;
    e.order_id = order_id;
    e.instrument_id = stock_locate;
    e.event_type = EventType::OrderAdd as u8;
    e.side = side;
    Some(e)
}

fn parse_order_delete(m: &[u8]) -> Option<Event> {
    // D: Type[1] StkLoc[2] TrackNum[2] Timestamp[6] OrderRefNum[8] = 19
    if m.len() < 19 { return None; }
    let stock_locate = rd_u16(m, 1) as u32;
    let ts = rd_ts48(m, 5);
    let order_id = rd_u64(m, 11);
    let mut e = event_zeroed();
    e.ts = ts;
    e.order_id = order_id;
    e.instrument_id = stock_locate;
    e.event_type = EventType::OrderCancel as u8;
    Some(e)
}

fn parse_order_cancel(m: &[u8]) -> Option<Event> {
    // X: ... OrderRefNum[8] CancelledShares[4] = 23
    if m.len() < 23 { return None; }
    let stock_locate = rd_u16(m, 1) as u32;
    let ts = rd_ts48(m, 5);
    let order_id = rd_u64(m, 11);
    let qty = rd_u32(m, 19) as u64;
    let mut e = event_zeroed();
    e.ts = ts;
    e.qty = qty;
    e.order_id = order_id;
    e.instrument_id = stock_locate;
    e.event_type = EventType::OrderCancel as u8;
    Some(e)
}

fn parse_order_executed(m: &[u8]) -> Option<Event> {
    // E: ... OrderRefNum[8] Shares[4] MatchNum[8]                = 31
    // C: adds Printable[1] Price[4]                              = 36
    if m.len() < 31 { return None; }
    let stock_locate = rd_u16(m, 1) as u32;
    let ts = rd_ts48(m, 5);
    let order_id = rd_u64(m, 11);
    let qty = rd_u32(m, 19) as u64;
    let price = if m[0] == b'C' && m.len() >= 36 {
        rd_u32(m, 32) as u64
    } else {
        0
    };
    let mut e = event_zeroed();
    e.ts = ts;
    e.price = price;
    e.qty = qty;
    e.order_id = order_id;
    e.instrument_id = stock_locate;
    e.event_type = EventType::Trade as u8;
    Some(e)
}

fn parse_trade(m: &[u8]) -> Option<Event> {
    // P: ... OrderRefNum[8] BuySell[1] Shares[4] Stock[8] Price[4] MatchNum[8] = 44
    if m.len() < 44 { return None; }
    let stock_locate = rd_u16(m, 1) as u32;
    let ts = rd_ts48(m, 5);
    let order_id = rd_u64(m, 11);
    let side = if m[19] == b'B' { Side::Bid as u8 } else { Side::Ask as u8 };
    let qty = rd_u32(m, 20) as u64;
    let price = rd_u32(m, 32) as u64;
    let mut e = event_zeroed();
    e.ts = ts;
    e.price = price;
    e.qty = qty;
    e.order_id = order_id;
    e.instrument_id = stock_locate;
    e.event_type = EventType::Trade as u8;
    e.side = side;
    Some(e)
}

/// Parse an unframed ITCH byte stream, advancing by `msg_length`.
///
/// Returns the number of events written to `out`, or Err if an unknown
/// message type is encountered.
pub fn parse_buffer(raw: &[u8], out: &mut Vec<Event>) -> Result<usize, &'static str> {
    let mut i = 0usize;
    let mut n = 0usize;
    while i < raw.len() {
        let t = raw[i];
        let len = msg_length(t) as usize;
        if len == 0 { return Err("unknown ITCH message type"); }
        if i + len > raw.len() { return Err("truncated ITCH message"); }
        if let Some(e) = dispatch(&raw[i..i + len]) {
            out.push(e);
            n += 1;
        }
        i += len;
    }
    Ok(n)
}

// ─────────────────────────────────────────────────────────────────────
// Synthetic stream generator — produces deterministic valid ITCH bytes
// for the integration benchmark.
// ─────────────────────────────────────────────────────────────────────

/// Emit a single Add Order ('A') message, big-endian.
pub fn emit_add_order(
    buf: &mut Vec<u8>,
    ts: u64,
    order_id: u64,
    side: Side,
    shares: u32,
    price: u32,
) {
    buf.push(b'A');
    buf.extend_from_slice(&0u16.to_be_bytes());            // stock locate
    buf.extend_from_slice(&0u16.to_be_bytes());            // tracking
    let ts_hi = (ts >> 32) as u16;
    let ts_lo = (ts & 0xFFFF_FFFF) as u32;
    buf.extend_from_slice(&ts_hi.to_be_bytes());
    buf.extend_from_slice(&ts_lo.to_be_bytes());
    buf.extend_from_slice(&order_id.to_be_bytes());
    buf.push(if matches!(side, Side::Bid) { b'B' } else { b'S' });
    buf.extend_from_slice(&shares.to_be_bytes());
    buf.extend_from_slice(&[b' '; 8]);                     // stock symbol padding
    buf.extend_from_slice(&price.to_be_bytes());
}

/// Emit a single Order Delete ('D') message, big-endian.
pub fn emit_order_delete(buf: &mut Vec<u8>, ts: u64, order_id: u64) {
    buf.push(b'D');
    buf.extend_from_slice(&0u16.to_be_bytes());
    buf.extend_from_slice(&0u16.to_be_bytes());
    let ts_hi = (ts >> 32) as u16;
    let ts_lo = (ts & 0xFFFF_FFFF) as u32;
    buf.extend_from_slice(&ts_hi.to_be_bytes());
    buf.extend_from_slice(&ts_lo.to_be_bytes());
    buf.extend_from_slice(&order_id.to_be_bytes());
}

/// Emit a single Trade Non-Cross ('P') message, big-endian.
pub fn emit_trade(
    buf: &mut Vec<u8>,
    ts: u64,
    order_id: u64,
    side: Side,
    shares: u32,
    price: u32,
) {
    buf.push(b'P');
    buf.extend_from_slice(&0u16.to_be_bytes());
    buf.extend_from_slice(&0u16.to_be_bytes());
    let ts_hi = (ts >> 32) as u16;
    let ts_lo = (ts & 0xFFFF_FFFF) as u32;
    buf.extend_from_slice(&ts_hi.to_be_bytes());
    buf.extend_from_slice(&ts_lo.to_be_bytes());
    buf.extend_from_slice(&order_id.to_be_bytes());
    buf.push(if matches!(side, Side::Bid) { b'B' } else { b'S' });
    buf.extend_from_slice(&shares.to_be_bytes());
    buf.extend_from_slice(&[b' '; 8]);
    buf.extend_from_slice(&price.to_be_bytes());
    buf.extend_from_slice(&0u64.to_be_bytes());            // match number
}

/// Build a deterministic synthetic ITCH stream of `n_events` messages.
///
/// Uses the same distributional shape as `realistic_events`:
/// 60% add, 25% delete, 15% trade, with a concentrated top of book.
pub fn synthetic_itch_stream(n_events: usize, seed: u64) -> Vec<u8> {
    // Inline xorshift64 to avoid a cross-crate dependency.
    // Same algorithm as `flowlab_bench::XorShift64`.
    struct XorShift64(u64);
    impl XorShift64 {
        #[inline]
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
    }
    let mut rng = XorShift64(seed);
    // Conservative upper bound: 44 bytes per message * n + header slack.
    let mut buf = Vec::with_capacity(n_events * 44 + 64);
    let mid_price: u32 = 100_0000; // $100.0000 in 1/10000 fixed-point
    let mut next_id: u64 = 1;
    let mut live_ids: Vec<u64> = Vec::with_capacity(n_events / 2);

    for i in 0..n_events {
        let r = rng.next_u64();
        let kind = r % 100;
        let ts = i as u64 * 1000;

        if kind < 60 || live_ids.is_empty() {
            let offset = ((rng.next_u64() % 50) as i64 - 25) as i32;
            let price = (mid_price as i32 + offset * 100).max(1) as u32;
            let shares = (rng.next_u64() % 1000 + 1) as u32;
            let side = if rng.next_u64() % 2 == 0 { Side::Bid } else { Side::Ask };
            let id = next_id;
            next_id += 1;
            emit_add_order(&mut buf, ts, id, side, shares, price);
            live_ids.push(id);
        } else if kind < 85 {
            let idx = (rng.next_u64() as usize) % live_ids.len();
            let id = live_ids.swap_remove(idx);
            emit_order_delete(&mut buf, ts, id);
        } else {
            let shares = (rng.next_u64() % 200 + 1) as u32;
            let price = mid_price;
            let side = if rng.next_u64() % 2 == 0 { Side::Bid } else { Side::Ask };
            emit_trade(&mut buf, ts, next_id, side, shares, price);
        }
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use flowlab_core::hot_book::HotOrderBook;

    #[test]
    fn msg_length_covers_active_types() {
        assert_eq!(msg_length(b'A'), 36);
        assert_eq!(msg_length(b'F'), 40);
        assert_eq!(msg_length(b'D'), 19);
        assert_eq!(msg_length(b'X'), 23);
        assert_eq!(msg_length(b'E'), 31);
        assert_eq!(msg_length(b'P'), 44);
        assert_eq!(msg_length(b'Z'), 0);
    }

    #[test]
    fn roundtrip_single_add() {
        let mut bytes = Vec::new();
        emit_add_order(&mut bytes, 42, 7, Side::Bid, 100, 1234);
        let mut events = Vec::new();
        let n = parse_buffer(&bytes, &mut events).unwrap();
        assert_eq!(n, 1);
        let ev = events[0];
        assert_eq!(ev.ts, 42);
        assert_eq!(ev.price, 1234);
        assert_eq!(ev.qty, 100);
        assert_eq!(ev.order_id, 7);
        assert_eq!(ev.event_type, EventType::OrderAdd as u8);
        assert_eq!(ev.side, Side::Bid as u8);
    }

    #[test]
    fn synthetic_stream_parses_and_populates_book() {
        let raw = synthetic_itch_stream(1_000, 0xDEAD_BEEF);
        let mut events = Vec::with_capacity(1_000);
        let n = parse_buffer(&raw, &mut events).unwrap();
        assert!(n >= 900);

        let mut hot = HotOrderBook::<256>::new(1);
        for ev in &events {
            if ev.event_type == EventType::OrderAdd as u8 && ev.price > 0 {
                hot.apply(ev);
            }
        }
        assert!(hot.canonical_l2_hash() != 0);
    }
}
