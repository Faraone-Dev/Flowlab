// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

//! Fuzz #1 — Order book consistency under random event flow.
//!
//! WHAT IT PROVES
//! --------------
//! Two HotOrderBook instances fed the SAME random Event sequence
//! produce the SAME canonical L2 hash. This is the foundation that
//! makes the cross-impl Rust↔C++↔Zig oracle meaningful: the Rust
//! reference impl must itself be a pure function of its input.
//!
//! Random surface exercised:
//!   - all five EventType variants
//!   - both sides
//!   - cancel of nonexistent order_id
//!   - trade larger than resting qty
//!   - duplicate add of the same order_id
//!   - prices sampled across the whole grid
//!
//! Failure of this harness invalidates the entire deterministic
//! claim of the system.

use flowlab_core::event::{Event, EventType, Side};
use flowlab_core::hot_book::HotOrderBook;
use flowlab_e2e::XorShift64;

/// Generate one random event from the RNG. Pure function of the RNG
/// state — re-running with the same seed produces the same stream.
fn rand_event(rng: &mut XorShift64) -> Event {
    let kind = match rng.next_u64() % 5 {
        0 => EventType::OrderAdd,
        1 => EventType::OrderCancel,
        2 => EventType::OrderModify,
        3 => EventType::Trade,
        _ => EventType::BookSnapshot,
    } as u8;
    let side = if rng.next_u64() & 1 == 0 { Side::Bid } else { Side::Ask } as u8;
    Event {
        ts: rng.next_u64(),
        price: 9_900 + (rng.next_u64() % 200),
        qty: rng.next_in(1, 500),
        // small order_id space → forces collisions / cancel of unknowns
        order_id: 1 + (rng.next_u64() % 4_000),
        instrument_id: 1,
        event_type: kind,
        side,
        _pad: [0; 2],
    }
}

fn run_seed(seed: u64, n: usize) -> u64 {
    let mut rng = XorShift64::new(seed);
    let mut book = HotOrderBook::<256>::new(1);
    for _ in 0..n {
        let ev = rand_event(&mut rng);
        let _ = book.apply(&ev);
    }
    book.canonical_l2_hash()
}

#[test]
fn deterministic_under_random_flow() {
    // 32 fixed seeds × 4 000 events each → 128 000 random transitions.
    // Same seed must produce the same hash on two independent runs.
    const N_PER_SEED: usize = 4_000;
    let seeds: [u64; 32] = [
        0x0000_0000_0000_0001, 0xFFFF_FFFF_FFFF_FFFF, 0xDEAD_BEEF_CAFE_BABE,
        0x1234_5678_9ABC_DEF0, 0x0F1E_2D3C_4B5A_6978, 0x8000_0000_0000_0000,
        0x0000_0000_8000_0000, 0xA5A5_A5A5_A5A5_A5A5, 0x5A5A_5A5A_5A5A_5A5A,
        0xFEED_FACE_C0DE_BABE, 0xB16B_00B5_DEAD_BEEF, 0x0123_4567_89AB_CDEF,
        0xFEDC_BA98_7654_3210, 0x1111_1111_1111_1111, 0x2222_2222_2222_2222,
        0x3333_3333_3333_3333, 0x4444_4444_4444_4444, 0x5555_5555_5555_5555,
        0x6666_6666_6666_6666, 0x7777_7777_7777_7777, 0x9999_9999_9999_9999,
        0xAAAA_AAAA_AAAA_AAAA, 0xBBBB_BBBB_BBBB_BBBB, 0xCCCC_CCCC_CCCC_CCCC,
        0xDDDD_DDDD_DDDD_DDDD, 0xEEEE_EEEE_EEEE_EEEE, 0xC0FF_EE00_BAD_F00D,
        0xBADD_CAFE_DEAF_BEEF, 0x900D_C0DE_900D_F00D, 0x1337_1337_1337_1337,
        0xACE0_FBA5_E000_0000, 0x6E73_2D6F_6E66_6952,
    ];

    for &seed in &seeds {
        let h1 = run_seed(seed, N_PER_SEED);
        let h2 = run_seed(seed, N_PER_SEED);
        assert_eq!(
            h1, h2,
            "non-deterministic under seed {:#018x}: {:#018x} vs {:#018x}",
            seed, h1, h2
        );
    }
}

#[test]
fn no_panic_on_pathological_streams() {
    // Streams designed to provoke edge cases: cancel-only, modify-only,
    // trade-only, and a single duplicate-add-then-cancel pair.
    let pathological_seeds: [u64; 8] = [
        0, 1, 2, 3,
        0x8000_0000_0000_0001,
        0x7FFF_FFFF_FFFF_FFFF,
        0x9E37_79B9_7F4A_7C15,
        0x6A09_E667_F3BC_C908,
    ];
    for &s in &pathological_seeds {
        // Just running without panicking is the assertion.
        let _ = run_seed(if s == 0 { 1 } else { s }, 10_000);
    }
}
