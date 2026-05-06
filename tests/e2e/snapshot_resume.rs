// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

//! E2E #5 — Snapshot resume convergence (operational determinism).
//!
//! WHAT IT PROVES
//! --------------
//! Persisting a checkpoint mid-stream and resuming from it is
//! observationally indistinguishable from a from-scratch replay of
//! the same input. Concretely:
//!
//!   1. Parse a deterministic ITCH stream into Event[].
//!   2. Build book A by applying the FULL stream.
//!   3. Build book B by applying the first half, snapshot to disk,
//!      restore into book B', apply the second half on B'.
//!   4. Assert `canonical_l2_hash(A) == canonical_l2_hash(B')`.
//!
//! Plus byte-determinism of the snapshot frame itself: serialising
//! the SAME book twice MUST produce identical bytes (HashMap iteration
//! order normalised by sort-by-oid in `to_snapshot_bytes`).
//!
//! WHY IT MATTERS
//! --------------
//! Without this, "checkpoint + resume" is a benchmark feature, not a
//! correctness feature. With it, the operator can crash-restart an
//! engine and rejoin the live tape with provably the same state the
//! deceased process had. It's also the precondition for ever moving
//! the engine across hosts mid-day without a full replay-from-open.

use flowlab_core::event::Event;
use flowlab_core::hot_book::HotOrderBook;
use flowlab_e2e::tmp_path;
use flowlab_replay::itch::{parse_buffer, synthetic_itch_stream};
use flowlab_replay::snapshot::Snapshot;

#[test]
fn snapshot_resume_matches_full_replay() {
    const N_EVENTS: usize = 25_000;
    const SEED: u64 = 0xF10F_1AB1_5AAA_AAA1;

    let raw_itch = synthetic_itch_stream(N_EVENTS, SEED);
    let mut events: Vec<Event> = Vec::with_capacity(N_EVENTS);
    parse_buffer(&raw_itch, &mut events).expect("ITCH parse failed");
    assert!(events.len() >= 100, "synthetic stream too short for split");

    let split = events.len() / 2;

    // ─── Reference: from-scratch replay over the entire stream ──────
    let mut book_a: HotOrderBook<256> = HotOrderBook::new(1);
    for ev in &events {
        book_a.apply(ev);
    }
    let hash_a = book_a.canonical_l2_hash();

    // ─── Build B up to `split`, snapshot to disk, then resume ───────
    let mut book_b: HotOrderBook<256> = HotOrderBook::new(1);
    for ev in &events[..split] {
        book_b.apply(ev);
    }
    let snap = Snapshot {
        seq: split as u64,
        state_hash: book_b.canonical_l2_hash(),
        book_data: book_b.to_snapshot_bytes(),
    };

    let path = tmp_path("snapshot_resume");
    snap.write_to_path(&path).expect("snapshot write");

    // Restore from the on-disk frame (exercises the full I/O path,
    // not just the in-memory codec).
    let restored = Snapshot::read_from_path(&path).expect("snapshot read");
    assert_eq!(restored.seq, split as u64);
    assert_eq!(restored.state_hash, snap.state_hash);

    let mut book_b_resumed =
        HotOrderBook::<256>::from_snapshot_bytes(&restored.book_data).expect("decode book");
    assert_eq!(
        book_b_resumed.canonical_l2_hash(),
        snap.state_hash,
        "restored book must match the state_hash declared in the snapshot frame"
    );

    for ev in &events[split..] {
        book_b_resumed.apply(ev);
    }
    let hash_b = book_b_resumed.canonical_l2_hash();

    assert_eq!(
        hash_a, hash_b,
        "RESUME BROKEN: from-scratch and from-snapshot diverged\n  full     = {:#018x}\n  resumed  = {:#018x}",
        hash_a, hash_b
    );

    // ─── Byte-determinism: re-serialise the full book and compare ───
    let bytes1 = book_a.to_snapshot_bytes();
    let bytes2 = book_a.to_snapshot_bytes();
    assert_eq!(
        bytes1, bytes2,
        "snapshot serialisation is non-deterministic — HashMap iteration leaked into output"
    );

    std::fs::remove_file(&path).ok();
}
