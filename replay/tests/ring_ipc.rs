//! Integration test: byte-exact interop with the Go ring-buffer writer.
//!
//! The Go `ingest/mmap/ring.go` writer produces this exact on-disk
//! layout. We here emulate the Go writer (via our in-process
//! [`RingWriter`], which mirrors the Go code 1:1) and verify that a
//! Rust consumer:
//!
//! 1. accepts files produced with the FLOWRING magic,
//! 2. reads back the byte stream without corruption across wrap-around,
//! 3. can reconstruct a deterministic L2 canonical hash identical to a
//!    direct in-memory pipeline over the same synthetic ITCH stream.
//!
//! When the Go side is executable (non-Windows CI), a shell-out driver
//! can replace [`RingWriter`] with a real Go producer — the reader code
//! under test is unchanged.

use flowlab_core::event::{Event, EventType};
use flowlab_core::hot_book::HotOrderBook;
use flowlab_replay::itch::{parse_buffer, synthetic_itch_stream};
use flowlab_replay::{RingReader, RingWriter};

fn tmp_path(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "flowlab-ring-it-{}-{}.bin",
        name,
        std::process::id()
    ));
    p
}

/// Drive a full pipeline (parse → serialize → ring IPC → deserialize →
/// apply) and prove determinism against the reference in-memory path.
#[test]
fn ring_ipc_preserves_canonical_l2_hash() {
    let n = 10_000usize;
    let raw = synthetic_itch_stream(n, 0xDEADBEEF);
    let mut events: Vec<Event> = Vec::with_capacity(n);
    parse_buffer(&raw, &mut events).unwrap();

    // Reference: direct in-memory apply.
    let mut ref_book = HotOrderBook::<256>::new(1);
    for ev in &events {
        ref_book.apply(ev);
    }
    let reference_hash = ref_book.canonical_l2_hash();

    // IPC path: stream serialized events through a real mmap ring.
    let path = tmp_path("l2_hash");
    // Capacity must comfortably hold worst-case burst. 64KiB is enough
    // for ~1638 events; we consume eagerly so it never overflows.
    let capacity: u64 = 64 * 1024;
    let mut writer = RingWriter::create(&path, capacity).unwrap();
    let mut reader = RingReader::open(&path, capacity).unwrap();

    let bytes: &[u8] = bytemuck::cast_slice(&events);
    let mut produced = 0usize;
    let mut consumer_book = HotOrderBook::<256>::new(1);
    let mut leftover: Vec<u8> = Vec::new();
    let mut read_buf = vec![0u8; 4096];

    // Interleave producer (batches of ~500 events) and consumer to
    // exercise wrap-around and the Acquire/Release fences.
    let event_size = std::mem::size_of::<Event>();
    let batch_events = 500;
    let batch_bytes = batch_events * event_size;

    while produced < bytes.len() {
        let end = (produced + batch_bytes).min(bytes.len());
        writer.write(&bytes[produced..end]).unwrap();
        produced = end;

        loop {
            let n = reader.read(&mut read_buf);
            if n == 0 {
                break;
            }
            leftover.extend_from_slice(&read_buf[..n]);
            // Drain complete events from leftover.
            let full = (leftover.len() / event_size) * event_size;
            if full > 0 {
                let slice: &[Event] = bytemuck::cast_slice(&leftover[..full]);
                for ev in slice {
                    consumer_book.apply(ev);
                }
                leftover.drain(..full);
            }
        }
    }

    // Final drain.
    loop {
        let n = reader.read(&mut read_buf);
        if n == 0 {
            break;
        }
        leftover.extend_from_slice(&read_buf[..n]);
    }
    assert!(
        leftover.is_empty(),
        "dangling bytes after stream drained: {}",
        leftover.len()
    );

    let consumer_hash = consumer_book.canonical_l2_hash();
    assert_eq!(
        consumer_hash, reference_hash,
        "ring IPC changed L2 state: ref={:#018x} ipc={:#018x}",
        reference_hash, consumer_hash
    );

    std::fs::remove_file(&path).ok();
}

/// Sanity: a writer that advances the cursor past the capacity boundary
/// must remain coherent with a reader that has drained, so that
/// wrap-around does not corrupt event boundaries even when the cursors
/// are much larger than the capacity.
#[test]
fn ring_survives_many_wraps() {
    let path = tmp_path("many_wraps");
    let capacity: u64 = 1024; // forces dozens of wraps
    let mut writer = RingWriter::create(&path, capacity).unwrap();
    let mut reader = RingReader::open(&path, capacity).unwrap();

    let event = Event {
        ts: 42,
        price: 100,
        qty: 7,
        order_id: 1,
        instrument_id: 1,
        event_type: EventType::OrderAdd as u8,
        side: 0,
        _pad: [0; 2],
    };
    let bytes: [u8; 40] = bytemuck::bytes_of(&event).try_into().unwrap();

    let mut count = 0u64;
    let mut buf = [0u8; 40];
    for _ in 0..10_000 {
        writer.write(&bytes).unwrap();
        let n = reader.read(&mut buf);
        assert_eq!(n, 40);
        let got: Event = *bytemuck::from_bytes(&buf);
        assert_eq!(got.ts, event.ts);
        assert_eq!(got.order_id, event.order_id);
        count += 1;
    }
    assert_eq!(count, 10_000);
    // Cursors must be way past capacity but aligned on event boundary.
    assert_eq!(reader.read_cursor(), 10_000 * 40);
    assert_eq!(reader.write_cursor(), 10_000 * 40);
    std::fs::remove_file(&path).ok();
}
