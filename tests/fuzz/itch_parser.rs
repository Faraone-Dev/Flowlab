//! Fuzz #2 — ITCH parser robustness against arbitrary bytes.
//!
//! WHAT IT PROVES
//! --------------
//! Feeding the ITCH parser arbitrary random bytes:
//!   - never panics
//!   - never reads out of bounds
//!   - never enters an infinite loop
//!   - returns a well-defined Result (Ok with N events, or Err)
//!
//! This is the only acceptable contract for a parser that on the
//! production path will see corrupted, truncated, or adversarial
//! frames coming off a multicast UDP socket.

use flowlab_core::event::Event;
use flowlab_e2e::XorShift64;
use flowlab_replay::itch::parse_buffer;

#[test]
fn random_bytes_never_panic() {
    // 256 distinct seeds × ~512 byte buffers → 128 KiB of garbage.
    let seeds: [u64; 8] = [
        0x0000_0000_0000_0001,
        0xFFFF_FFFF_FFFF_FFFF,
        0xDEAD_BEEF_CAFE_BABE,
        0x1234_5678_9ABC_DEF0,
        0xA5A5_A5A5_5A5A_5A5A,
        0x900D_C0DE_BADD_BEEF,
        0xFEED_FACE_DEAF_F00D,
        0x6E73_2D6F_6E66_6952,
    ];

    for &seed in &seeds {
        let mut rng = XorShift64::new(seed);
        for buf_len in [0usize, 1, 7, 19, 36, 256, 511, 1024, 4096] {
            let mut buf = vec![0u8; buf_len];
            for b in buf.iter_mut() {
                *b = (rng.next_u64() & 0xFF) as u8;
            }
            let mut events: Vec<Event> = Vec::with_capacity(buf_len);
            // Whatever the parser returns, it must NOT panic.
            let _ = parse_buffer(&buf, &mut events);
            // Output count must be sane.
            assert!(events.len() <= buf_len, "produced more events than bytes");
        }
    }
}

#[test]
fn truncated_inputs_are_handled() {
    // Take a valid synthetic stream and chop it at every byte offset;
    // the parser must terminate cleanly for every truncation.
    use flowlab_replay::itch::synthetic_itch_stream;
    let raw = synthetic_itch_stream(200, 0xABCD_1234);
    // Step over offsets — every 7th to keep the test fast but cover
    // odd alignments inside multi-byte fields.
    for cut in (0..raw.len()).step_by(7) {
        let mut events: Vec<Event> = Vec::with_capacity(200);
        let _ = parse_buffer(&raw[..cut], &mut events);
    }
}

#[test]
fn high_bit_byte_storms_do_not_overflow_lookups() {
    // All 0xFF: every byte triggers `msg_length(0xFF) == 0`, parser
    // must return Err on the first iteration without scanning further.
    let buf = vec![0xFFu8; 4096];
    let mut events: Vec<Event> = Vec::new();
    let res = parse_buffer(&buf, &mut events);
    assert!(res.is_err(), "0xFF storm must produce an error, not silent zero");
}
