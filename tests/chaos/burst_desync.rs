//! Chaos #1 — Burst latency desync (producer/consumer race).
//!
//! WHAT IT PROVES
//! --------------
//! Two threads hammer the SPSC mmap ring with realistic asymmetry:
//!
//!   - PRODUCER  : writes in tight bursts of varying size, sleeps
//!                 zero-to-microseconds between bursts (modelling Go
//!                 ingest spikes off a multicast feed).
//!   - CONSUMER  : reads in a steady loop but periodically stalls
//!                 for tens-to-hundreds of microseconds (modelling
//!                 Rust analytics doing heavier work on a frame).
//!
//! With a deliberately small ring, the producer hits `Full` thousands
//! of times. With consumer stalls, the producer waits for cursor
//! advancement that crosses cache lines on a *different* thread.
//!
//! The system MUST converge to the SAME canonical L2 hash as a
//! single-threaded reference replay of the same input. Any divergence
//! reveals a memory-visibility bug in the cursor publication or a
//! split-byte boundary corruption between writer and reader.

use bytemuck::cast_slice;
use flowlab_core::event::Event;
use flowlab_core::hot_book::HotOrderBook;
use flowlab_e2e::tmp_path;
use flowlab_replay::itch::{parse_buffer, synthetic_itch_stream};
use flowlab_replay::{RingError, RingReader, RingWriter};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[test]
fn burst_desync_preserves_hash() {
    const N_EVENTS: usize = 80_000;
    const SEED: u64 = 0xB0F5_7DE5_FAC1_C0DE;
    // Tiny ring → guaranteed contention. 4 KiB / 40 B ≈ 102 events
    // resident max; producer bursts of 64-256 events MUST block.
    const RING_CAPACITY: u64 = 4 * 1024;

    let raw = synthetic_itch_stream(N_EVENTS, SEED);
    let mut events: Vec<Event> = Vec::with_capacity(N_EVENTS);
    parse_buffer(&raw, &mut events).expect("ITCH parse");
    let total_events = events.len();

    // Single-threaded reference.
    let reference = {
        let mut b = HotOrderBook::<256>::new(1);
        for ev in &events {
            b.apply(ev);
        }
        b.canonical_l2_hash()
    };

    let path = tmp_path("burst_desync");
    let writer = RingWriter::create(&path, RING_CAPACITY).unwrap();
    let reader = RingReader::open(&path, RING_CAPACITY).unwrap();

    let producer_done = Arc::new(AtomicBool::new(false));
    let consumed_events = Arc::new(AtomicU64::new(0));
    let backpressure_hits = Arc::new(AtomicU64::new(0));
    let consumer_stalls = Arc::new(AtomicU64::new(0));

    // ─── Producer thread ────────────────────────────────────────────
    let prod_done = producer_done.clone();
    let bp_hits = backpressure_hits.clone();
    let prod_handle = std::thread::spawn(move || {
        let mut writer = writer;
        let bytes: &[u8] = cast_slice(&events);
        let event_size = std::mem::size_of::<Event>();
        // PRNG for burst sizing — pure function of fixed seed for
        // reproducibility. Bursts of 8–50 events (320–2 000 bytes)
        // ALWAYS fit in the 4 KiB ring on their own (no deadlock),
        // but multiple back-to-back bursts overflow it.
        let mut rng = flowlab_e2e::XorShift64::new(0xBEEF_F00D_DEAD_C0DE);
        let mut produced = 0;
        while produced < bytes.len() {
            let burst_events = (rng.next_in(8, 50)) as usize;
            let burst_bytes = (burst_events * event_size).min(bytes.len() - produced);
            let chunk = &bytes[produced..produced + burst_bytes];
            loop {
                match writer.write(chunk) {
                    Ok(()) => break,
                    Err(RingError::Full { .. }) => {
                        bp_hits.fetch_add(1, Ordering::Relaxed);
                        // Tiny back-off; lets consumer drain a slot.
                        std::thread::yield_now();
                    }
                    Err(e) => panic!("producer ring error: {e:?}"),
                }
            }
            produced += burst_bytes;
            // No sleep — producer runs flat-out. Chaos comes from
            // burst-size variance + consumer stalls.
        }
        prod_done.store(true, Ordering::Release);
    });

    // ─── Consumer thread ────────────────────────────────────────────
    let cons_done_flag = producer_done.clone();
    let consumed = consumed_events.clone();
    let stalls = consumer_stalls.clone();
    let cons_handle = std::thread::spawn(move || -> u64 {
        let mut reader = reader;
        let mut book = HotOrderBook::<256>::new(1);
        let event_size = std::mem::size_of::<Event>();
        let mut scratch = vec![0u8; 8 * 1024];
        let mut leftover: Vec<u8> = Vec::with_capacity(8 * 1024);
        let mut rng = flowlab_e2e::XorShift64::new(0xC0FF_EE00_5747_F00D);
        let mut idle_passes: u32 = 0;

        loop {
            let n = reader.read(&mut scratch);
            if n == 0 {
                // Producer finished AND ring drained AND no leftover.
                if cons_done_flag.load(Ordering::Acquire)
                    && reader.write_cursor() == reader.read_cursor()
                    && leftover.is_empty()
                {
                    break;
                }
                idle_passes = idle_passes.saturating_add(1);
                if idle_passes > 8 {
                    std::thread::yield_now();
                }
                continue;
            }
            idle_passes = 0;
            leftover.extend_from_slice(&scratch[..n]);
            let full = (leftover.len() / event_size) * event_size;
            if full > 0 {
                let evs: &[Event] = cast_slice(&leftover[..full]);
                for ev in evs {
                    book.apply(ev);
                    consumed.fetch_add(1, Ordering::Relaxed);
                }
                leftover.drain(..full);
            }
            // Random stall — emulates heavier downstream work.
            // 1-in-4 frames triggers a 20–100 µs stall so the producer
            // has time to fill the small ring and hit backpressure.
            if rng.next_u64() % 4 == 0 {
                stalls.fetch_add(1, Ordering::Relaxed);
                let micros = 20 + (rng.next_u64() % 80);
                std::thread::sleep(Duration::from_micros(micros));
            }
        }
        assert!(leftover.is_empty(), "consumer left dangling bytes");
        book.canonical_l2_hash()
    });

    prod_handle.join().expect("producer panicked");
    let ipc_hash = cons_handle.join().expect("consumer panicked");

    let bp = backpressure_hits.load(Ordering::Relaxed);
    let st = consumer_stalls.load(Ordering::Relaxed);
    let cn = consumed_events.load(Ordering::Relaxed);

    assert!(
        bp > 50,
        "did not actually stress backpressure (only {bp} hits) — \
         increase N_EVENTS or shrink RING_CAPACITY"
    );
    assert!(st > 0, "consumer never stalled — chaos not triggered");
    assert_eq!(
        cn as usize, total_events,
        "consumer processed {cn} events but stream had {total_events}"
    );

    assert_eq!(
        ipc_hash, reference,
        "MEMORY VISIBILITY BUG: desync changed L2 state\n  \
         ref       = {:#018x}\n  \
         ipc       = {:#018x}\n  \
         backpressure_hits = {bp}, stalls = {st}, consumed = {cn}",
        reference, ipc_hash
    );

    std::fs::remove_file(&path).ok();
}
