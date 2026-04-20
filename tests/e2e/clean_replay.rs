//! E2E #1 — Clean replay end-to-end (cross-language oracle).
//!
//! WHAT IT PROVES
//! --------------
//! The full deterministic pipeline produces a bit-exact canonical L2
//! state that is independent of the transport path:
//!
//!   raw ITCH bytes
//!     -> Rust ITCH parser  (mirror of feed-parser/src/itch.zig)
//!     -> serialize as 40 B canonical Event[]
//!     -> SPSC mmap ring (FLOWRING magic, same layout as Go writer)
//!     -> Rust ring consumer
//!     -> HotOrderBook::apply
//!     -> canonical_l2_hash()
//!
//! The hash MUST equal the reference computed by feeding the exact
//! same Event[] directly into HotOrderBook in the same process.
//! Any divergence means the IPC path mutates state — i.e. the system
//! is no longer a deterministic oracle.

use bytemuck::cast_slice;
use flowlab_core::event::Event;
use flowlab_core::hot_book::HotOrderBook;
use flowlab_e2e::tmp_path;
use flowlab_replay::itch::{parse_buffer, synthetic_itch_stream};
use flowlab_replay::{RingReader, RingWriter};

#[test]
fn clean_replay_oracle() {
    // Deterministic input: same seed → same bytes → same hash, forever.
    const N_EVENTS: usize = 25_000;
    const SEED: u64 = 0xF10F_1AB1_DEAD_BEEF;
    const RING_CAPACITY: u64 = 64 * 1024; // power of two; small to stress wrap-around

    let raw_itch = synthetic_itch_stream(N_EVENTS, SEED);
    let mut events: Vec<Event> = Vec::with_capacity(N_EVENTS);
    parse_buffer(&raw_itch, &mut events).expect("ITCH parse failed");

    // ─── Reference (in-process apply) ───────────────────────────────
    let reference_hash = {
        let mut book = HotOrderBook::<256>::new(1);
        for ev in &events {
            book.apply(ev);
        }
        book.canonical_l2_hash()
    };

    // ─── IPC (mmap ring) ────────────────────────────────────────────
    let path = tmp_path("clean_replay");
    let mut writer = RingWriter::create(&path, RING_CAPACITY).unwrap();
    let mut reader = RingReader::open(&path, RING_CAPACITY).unwrap();
    let mut consumer = HotOrderBook::<256>::new(1);

    let bytes: &[u8] = cast_slice(&events);
    let event_size = std::mem::size_of::<Event>();
    let batch = 500 * event_size; // 20 KiB per push
    let mut produced = 0;
    let mut leftover: Vec<u8> = Vec::with_capacity(batch);
    let mut scratch = vec![0u8; 8 * 1024];

    while produced < bytes.len() {
        let end = (produced + batch).min(bytes.len());
        writer.write(&bytes[produced..end]).unwrap();
        produced = end;
        drain(&mut reader, &mut scratch, &mut leftover, &mut consumer, event_size);
    }
    // Final drain.
    drain(&mut reader, &mut scratch, &mut leftover, &mut consumer, event_size);

    assert!(
        leftover.is_empty(),
        "{} dangling bytes after final drain — boundary corrupted",
        leftover.len()
    );

    let ipc_hash = consumer.canonical_l2_hash();
    assert_eq!(
        ipc_hash, reference_hash,
        "ORACLE BROKEN: ring IPC mutated L2 state\n  reference = {:#018x}\n  ipc       = {:#018x}",
        reference_hash, ipc_hash
    );

    std::fs::remove_file(&path).ok();
}

fn drain(
    reader: &mut RingReader,
    scratch: &mut [u8],
    leftover: &mut Vec<u8>,
    consumer: &mut HotOrderBook<256>,
    event_size: usize,
) {
    loop {
        let n = reader.read(scratch);
        if n == 0 {
            break;
        }
        leftover.extend_from_slice(&scratch[..n]);
        let full = (leftover.len() / event_size) * event_size;
        if full > 0 {
            let evs: &[Event] = cast_slice(&leftover[..full]);
            for ev in evs {
                consumer.apply(ev);
            }
            leftover.drain(..full);
        }
    }
}
