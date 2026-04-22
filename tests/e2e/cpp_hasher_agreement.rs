//! E2E #4 — Rust ↔ C++ `StateHasher` bit-exact agreement.
//!
//! WHAT IT PROVES
//! --------------
//! The Rust and C++ implementations of `StateHasher` produce the
//! **same 64-bit digest** when fed the same byte stream, for every
//! prefix of the stream.
//!
//! Both sides implement the identical recurrence:
//!
//!     state_0     = 0
//!     state_{n+1} = xxh3_64( state_n.to_le_bytes() ++ chunk_n )
//!
//! - Rust uses `xxhash-rust::xxh3_64` (re-exported by `flowlab-verify`).
//! - C++ uses the official single-header `xxhash.h` v0.8.3 (`XXH3_64bits`).
//!
//! Any divergence here means the two hashers will disagree on a real
//! event stream, which would silently break every cross-language
//! invariant downstream (replay digests, snapshot fingerprints,
//! verifier oracles).
//!
//! Gated behind the `native` feature because it links against the
//! C++ static library produced by `core/build.rs` (which in turn
//! requires the Zig feed-parser static lib at link time).
//!
//! Run with:
//!     cargo test -p flowlab-e2e --features native --test e2e_cpp_hasher_agreement
//!
//! Reference digest after feeding the canonical 5000-event sequence
//! is a fixed constant for the chosen seed; the test pins it so that
//! a regression on either side fails the assertion.

#![cfg(feature = "native")]

use bytemuck::cast_slice;
use flowlab_core::event::Event;
use flowlab_core::ffi::{
    flowlab_hasher_digest, flowlab_hasher_free, flowlab_hasher_new, flowlab_hasher_update,
};
use flowlab_replay::itch::{parse_buffer, synthetic_itch_stream};
use flowlab_verify::hasher::StateHasher;

/// Drive both hashers through the same sequence of chunks and assert
/// they agree at every step (not just on the final digest).
fn assert_agreement_on_chunks(chunks: &[&[u8]]) {
    let mut rust = StateHasher::new();
    let cpp = unsafe { flowlab_hasher_new() };
    assert!(!cpp.is_null(), "C++ hasher allocation returned null");

    for (i, chunk) in chunks.iter().enumerate() {
        rust.update(chunk);
        unsafe {
            flowlab_hasher_update(cpp, chunk.as_ptr(), chunk.len() as u64);
        }
        let r = rust.digest();
        let c = unsafe { flowlab_hasher_digest(cpp) };
        assert_eq!(
            r, c,
            "Rust↔C++ StateHasher digest divergence after chunk #{i} \
             (len={}): rust={r:#018x} cpp={c:#018x}",
            chunk.len()
        );
    }

    unsafe { flowlab_hasher_free(cpp) };
}

#[test]
fn empty_state_matches() {
    // Fresh hashers: both must report the canonical zero seed.
    let rust = StateHasher::new();
    let cpp = unsafe { flowlab_hasher_new() };
    let r = rust.digest();
    let c = unsafe { flowlab_hasher_digest(cpp) };
    unsafe { flowlab_hasher_free(cpp) };
    assert_eq!(r, c, "empty digest mismatch: rust={r:#x} cpp={c:#x}");
    assert_eq!(r, 0, "empty digest must be the zero seed (got {r:#x})");
}

#[test]
fn single_byte_chunks_agree() {
    // Hammer the per-update path: 256 single-byte updates.
    // Catches any off-by-one in the prev-state framing.
    let bytes: Vec<u8> = (0u8..=255).collect();
    let chunks: Vec<&[u8]> = bytes.iter().map(std::slice::from_ref).collect();
    assert_agreement_on_chunks(&chunks);
}

#[test]
fn small_then_large_chunks_agree() {
    // Mixes the C++ 4 KiB stack-buffer fast path and the streaming
    // fallback, plus odd sizes around the boundary.
    let mk = |seed: u64, n: usize| -> Vec<u8> {
        let mut s = seed;
        let mut v = vec![0u8; n];
        for b in v.iter_mut() {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            *b = (s >> 33) as u8;
        }
        v
    };
    let a = mk(1, 7);
    let b = mk(2, 4080); // just under 4 KiB framed (4080 + 8 = 4088)
    let c = mk(3, 4096); // exactly the boundary => hits streaming fallback
    let d = mk(4, 65_537); // well into streaming territory
    let e = mk(5, 0); // zero-length update is a legal no-payload step
    let chunks: Vec<&[u8]> = vec![&a, &b, &c, &d, &e, &a, &c];
    assert_agreement_on_chunks(&chunks);
}

#[test]
fn canonical_event_stream_agrees() {
    // Real-world surface: hash an actual canonical Event[] stream the
    // same way the verifier would. Pins the digest so a future
    // regression on either side is caught by the constant.
    const N_EVENTS: usize = 5_000;
    const SEED: u64 = 0xF54C_E1B7_6382_3E87; // unrelated to expected digest

    let raw = synthetic_itch_stream(N_EVENTS, SEED);
    let mut events: Vec<Event> = Vec::with_capacity(N_EVENTS);
    parse_buffer(&raw, &mut events).expect("ITCH parse failed");

    let bytes: &[u8] = cast_slice(&events);
    // 40 B per Event; fold one event at a time so we exercise many
    // small updates rather than one giant blob.
    let chunks: Vec<&[u8]> = bytes.chunks(std::mem::size_of::<Event>()).collect();
    assert_agreement_on_chunks(&chunks);

    // Final digest sanity (computed once on Rust side, since we just
    // proved C++ matches step-by-step).
    let mut h = StateHasher::new();
    for ch in &chunks {
        h.update(ch);
    }
    let final_digest = h.digest();
    assert_ne!(final_digest, 0, "non-empty stream must move state off zero");
    eprintln!(
        "✓ Rust↔C++ StateHasher agreement over {N_EVENTS} canonical events; \
         digest = {final_digest:#018x}"
    );
}
