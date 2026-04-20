//! E2E #3 — Stress burst + ring overflow.
//!
//! WHAT IT PROVES
//! --------------
//! When the producer rate exceeds the consumer rate (a near-full
//! ring), the SPSC mmap ring exhibits BACKPRESSURE, never silent
//! drop, and the resulting state is bit-exact identical to a slow,
//! non-stressed replay of the same byte stream.
//!
//! Concretely:
//!   - producer writes more bytes than the ring capacity in a tight
//!     loop, with the consumer deliberately throttled;
//!   - on `Err(RingError::Full)` the producer parks (yields) and
//!     retries — never drops;
//!   - the consumer drains everything and reaches the SAME canonical
//!     L2 hash as the in-process reference path.
//!
//! Determinism is therefore invariant to scheduling pressure.

use bytemuck::cast_slice;
use flowlab_core::event::Event;
use flowlab_core::hot_book::HotOrderBook;
use flowlab_e2e::tmp_path;
use flowlab_replay::itch::{parse_buffer, synthetic_itch_stream};
use flowlab_replay::{RingError, RingReader, RingWriter};

#[test]
fn stress_overflow_preserves_determinism() {
    const N_EVENTS: usize = 20_000;
    const SEED: u64 = 0x57E5_50ED_F10A_DA01;
    // Tiny capacity guarantees the producer fills the ring repeatedly.
    const RING_CAPACITY: u64 = 4 * 1024;

    let raw_itch = synthetic_itch_stream(N_EVENTS, SEED);
    let mut events: Vec<Event> = Vec::with_capacity(N_EVENTS);
    parse_buffer(&raw_itch, &mut events).expect("ITCH parse failed");

    // Reference (slow, no IPC).
    let reference_hash = {
        let mut book = HotOrderBook::<256>::new(1);
        for ev in &events {
            book.apply(ev);
        }
        book.canonical_l2_hash()
    };

    let path = tmp_path("stress_overflow");
    let mut writer = RingWriter::create(&path, RING_CAPACITY).unwrap();
    let mut reader = RingReader::open(&path, RING_CAPACITY).unwrap();
    let mut consumer = HotOrderBook::<256>::new(1);

    let bytes: &[u8] = cast_slice(&events);
    let event_size = std::mem::size_of::<Event>();
    // Push in mid-sized batches that exceed half the ring, so wrap +
    // backpressure are guaranteed to interleave.
    let batch = 1_500;
    let mut produced = 0;
    let mut leftover: Vec<u8> = Vec::with_capacity(batch * 2);
    let mut scratch = vec![0u8; batch * 2];

    let mut backpressure_hits: u64 = 0;
    let mut consumer_steps: u64 = 0;

    while produced < bytes.len() {
        let end = (produced + batch).min(bytes.len());
        let chunk = &bytes[produced..end];
        loop {
            match writer.write(chunk) {
                Ok(()) => {
                    produced = end;
                    break;
                }
                Err(RingError::Full { .. }) => {
                    backpressure_hits += 1;
                    // Drain a bit, then retry — never drop.
                    drain_one_pass(
                        &mut reader,
                        &mut scratch,
                        &mut leftover,
                        &mut consumer,
                        event_size,
                    );
                    consumer_steps += 1;
                    std::thread::yield_now();
                }
                Err(other) => panic!("unexpected ring error: {other:?}"),
            }
        }
    }
    // Final drain.
    while reader.write_cursor() > reader.read_cursor() || !leftover.is_empty() {
        let progressed = drain_one_pass(
            &mut reader,
            &mut scratch,
            &mut leftover,
            &mut consumer,
            event_size,
        );
        consumer_steps += 1;
        if !progressed && reader.write_cursor() == reader.read_cursor() {
            break;
        }
    }

    assert!(
        backpressure_hits > 0,
        "test did not actually stress the ring (capacity may be too large)"
    );
    assert!(consumer_steps > 0, "consumer never ran");
    assert!(
        leftover.is_empty(),
        "{} dangling bytes after drain — boundary corrupted under stress",
        leftover.len()
    );

    let ipc_hash = consumer.canonical_l2_hash();
    assert_eq!(
        ipc_hash, reference_hash,
        "BACKPRESSURE NOT INVARIANT: stressed pipeline mutated L2 state\n  ref = {:#018x}\n  ipc = {:#018x}",
        reference_hash, ipc_hash
    );

    std::fs::remove_file(&path).ok();
}

fn drain_one_pass(
    reader: &mut RingReader,
    scratch: &mut [u8],
    leftover: &mut Vec<u8>,
    consumer: &mut HotOrderBook<256>,
    event_size: usize,
) -> bool {
    let n = reader.read(scratch);
    if n == 0 && leftover.is_empty() {
        return false;
    }
    if n > 0 {
        leftover.extend_from_slice(&scratch[..n]);
    }
    let full = (leftover.len() / event_size) * event_size;
    if full > 0 {
        let evs: &[Event] = cast_slice(&leftover[..full]);
        for ev in evs {
            consumer.apply(ev);
        }
        leftover.drain(..full);
    }
    true
}
