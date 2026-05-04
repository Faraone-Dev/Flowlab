// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

//! End-to-end pipeline benchmark.
//!
//! Wires the full FLOWLAB hot path together:
//!
//!   ┌──────────────┐   ┌──────────────┐   ┌──────────────────┐   ┌────────────┐
//!   │  synthetic   │──▶│  ITCH 5.0    │──▶│ HotOrderBook<256>│──▶│ canonical  │
//!   │  ITCH bytes  │   │  parser (BE) │   │ (replay apply)   │   │ L2 hash    │
//!   └──────────────┘   └──────────────┘   └──────────────────┘   └────────────┘
//!                              │
//!                              ▼
//!                      ┌─────────────────┐
//!                      │ BTreeMap book   │── canonical L2 hash (must match)
//!                      └─────────────────┘
//!
//! The benchmark measures:
//!   1. Raw ITCH parse throughput (bytes/sec)
//!   2. End-to-end stream ingest (events/sec: parse + apply + hash per chunk)
//!   3. Cross-implementation agreement: HotOrderBook hash == BTreeMap hash
//!      after replaying the same parsed event stream (correctness gate).
//!
//! When built with `--features native`, the same stream is also dispatched
//! to the C++ `CppOrderBook` via FFI and the C++ state hash is compared to
//! the Rust `HotOrderBook::canonical_l2_hash`. That path is skipped by
//! default so the bench runs on any platform without the native toolchain.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use flowlab_bench::itch::{parse_buffer, synthetic_itch_stream};
use flowlab_core::event::{Event, EventType};
use flowlab_core::hot_book::HotOrderBook;
use flowlab_core::orderbook::OrderBook;

const STREAM_SIZES: &[usize] = &[10_000, 100_000];

// ─── Stage 1 — pure parser throughput ──────────────────────────────

fn bench_itch_parse(c: &mut Criterion) {
    let mut group = c.benchmark_group("pipeline/itch_parse");
    for &n in STREAM_SIZES {
        let raw = synthetic_itch_stream(n, 0xC0FFEE);
        group.throughput(Throughput::Bytes(raw.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &raw, |b, raw| {
            let mut out: Vec<Event> = Vec::with_capacity(n);
            b.iter(|| {
                out.clear();
                let _ = parse_buffer(black_box(raw), &mut out).unwrap();
                black_box(&out);
            });
        });
    }
    group.finish();
}

// ─── Stage 2 — end-to-end parse → HotOrderBook → hash ──────────────

fn bench_full_pipeline(c: &mut Criterion) {
    let mut group = c.benchmark_group("pipeline/full_hot");
    for &n in STREAM_SIZES {
        let raw = synthetic_itch_stream(n, 0xC0FFEE);
        group.throughput(Throughput::Bytes(raw.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &raw, |b, raw| {
            let mut events: Vec<Event> = Vec::with_capacity(n);
            b.iter(|| {
                events.clear();
                let _ = parse_buffer(black_box(raw), &mut events).unwrap();
                let mut book = HotOrderBook::<256>::new(1);
                for ev in &events {
                    // Only apply book-affecting events (trades carry qty=fill against order_id).
                    if ev.event_type == EventType::OrderAdd as u8
                        || ev.event_type == EventType::OrderCancel as u8
                    {
                        book.apply(ev);
                    }
                }
                black_box(book.canonical_l2_hash());
            });
        });
    }
    group.finish();
}

// ─── Stage 3 — correctness + BTreeMap reference pipeline ───────────

fn bench_reference_vs_hot(c: &mut Criterion) {
    // Build stream once; both books replay the exact same sequence.
    let n = 50_000usize;
    let raw = synthetic_itch_stream(n, 0xC0FFEE);
    let mut parsed: Vec<Event> = Vec::with_capacity(n);
    parse_buffer(&raw, &mut parsed).unwrap();

    // One-time cross-implementation agreement gate.
    //
    // `HotOrderBook` now carries an `order_id -> (price, side, qty)` lookup
    // map so ITCH `D` (delete) and `E`/`P` (trade) messages — which only
    // carry `order_id` on the wire — resolve the owning level in O(1), the
    // same way `OrderBook` (BTreeMap) does via its internal `orders` map.
    // We apply all book-affecting ITCH message types and both books must
    // still produce bit-identical canonical L2 hashes.
    let mut hot = HotOrderBook::<256>::new(1);
    let mut bt = OrderBook::new(1);
    let mut applied = 0usize;
    for ev in &parsed {
        let t = ev.event_type;
        if t == EventType::OrderAdd as u8
            || t == EventType::OrderCancel as u8
            || t == EventType::Trade as u8
        {
            hot.apply(ev);
            bt.apply(ev);
            applied += 1;
        }
    }
    let h1 = hot.canonical_l2_hash();
    let h2 = bt.canonical_l2_hash();
    assert_eq!(
        h1, h2,
        "HotOrderBook and OrderBook disagree on canonical L2 hash after {applied} events (h1={h1:#x} h2={h2:#x})"
    );
    eprintln!();
    eprintln!("  cross-impl L2 agreement  (HotOrderBook vs OrderBook/BTreeMap)");
    eprintln!("  -----------------------------------------------------------------");
    eprintln!("  events applied   : {applied}");
    eprintln!("  message types    : OrderAdd + OrderCancel + Trade");
    eprintln!("  hot canonical L2 : {h1:#018x}");
    eprintln!("  btm canonical L2 : {h2:#018x}");
    eprintln!("  status           : PASS (bit-identical)");
    eprintln!();

    let mut group = c.benchmark_group("pipeline/reference_replay");
    group.throughput(Throughput::Elements(parsed.len() as u64));
    group.bench_function("btreemap_full", |b| {
        b.iter(|| {
            let mut bt = OrderBook::new(1);
            for ev in &parsed {
                let t = ev.event_type;
                if t == EventType::OrderAdd as u8
                    || t == EventType::OrderCancel as u8
                    || t == EventType::Trade as u8
                {
                    bt.apply(black_box(ev));
                }
            }
            black_box(bt.canonical_l2_hash());
        });
    });
    group.bench_function("hot_array_full", |b| {
        b.iter(|| {
            let mut hot = HotOrderBook::<256>::new(1);
            for ev in &parsed {
                let t = ev.event_type;
                if t == EventType::OrderAdd as u8
                    || t == EventType::OrderCancel as u8
                    || t == EventType::Trade as u8
                {
                    hot.apply(black_box(ev));
                }
            }
            black_box(hot.canonical_l2_hash());
        });
    });
    group.finish();
}

// ─── Stage 4 — C++ FFI agreement (gated) ──────────────────────────

#[cfg(feature = "native")]
fn bench_cpp_agreement(c: &mut Criterion) {
    use flowlab_core::ffi::CppOrderBook;

    let n = 50_000usize;
    let raw = synthetic_itch_stream(n, 0xC0FFEE);
    let mut parsed: Vec<Event> = Vec::with_capacity(n);
    parse_buffer(&raw, &mut parsed).unwrap();

    // Cross-language agreement: C++ `flowlab_orderbook_hash` and Rust
    // `HotOrderBook::canonical_l2_hash` both implement the same canonical
    // L2 scheme (domain tag "FLOWLAB-L2-v1", 16-byte per-level payload of
    // (price_le, total_qty_le), '|' side separator, XOR-fold via
    // XXH3_64bits_withSeed). They MUST agree byte-for-byte for every run.
    let mut rust_hashes = [0u64; 2];
    let mut cpp_hashes = [0u64; 2];
    for run in 0..2 {
        let mut hot = HotOrderBook::<256>::new(1);
        let mut cpp = CppOrderBook::new();
        for ev in &parsed {
            if ev.event_type == EventType::OrderAdd as u8
                || ev.event_type == EventType::OrderCancel as u8
            {
                hot.apply(ev);
                cpp.apply(ev);
            }
        }
        rust_hashes[run] = hot.canonical_l2_hash();
        cpp_hashes[run] = cpp.state_hash();
    }
    assert_eq!(rust_hashes[0], rust_hashes[1], "Rust hash non-deterministic");
    assert_eq!(cpp_hashes[0], cpp_hashes[1], "C++ hash non-deterministic across FFI");
    assert_eq!(
        rust_hashes[0], cpp_hashes[0],
        "Rust/C++ canonical L2 hash disagreement: rust={:#018x} cpp={:#018x}",
        rust_hashes[0], cpp_hashes[0]
    );
    eprintln!();
    eprintln!("  cross-lang L2 agreement  (Rust HotOrderBook vs C++ OrderBook)");
    eprintln!("  -----------------------------------------------------------------");
    eprintln!("  events applied   : {}", parsed.len());
    eprintln!("  scheme           : FLOWLAB-L2-v1 | XXH3 | 16B/level | '|' sep");
    eprintln!("  rust hash        : {:#018x}", rust_hashes[0]);
    eprintln!("  cpp  hash        : {:#018x}", cpp_hashes[0]);
    eprintln!("  determinism      : 2/2 runs equal per impl");
    eprintln!("  status           : PASS (byte-for-byte cross-FFI)");
    eprintln!();

    let mut group = c.benchmark_group("pipeline/cross_lang");
    group.throughput(Throughput::Elements(parsed.len() as u64));
    group.bench_function("cpp_orderbook_apply", |b| {
        b.iter(|| {
            let mut cpp = CppOrderBook::new();
            for ev in &parsed {
                if ev.event_type == EventType::OrderAdd as u8
                    || ev.event_type == EventType::OrderCancel as u8
                {
                    cpp.apply(black_box(ev));
                }
            }
            black_box(cpp.state_hash());
        });
    });
    group.finish();
}

#[cfg(not(feature = "native"))]
fn bench_cpp_agreement(_c: &mut Criterion) {}

criterion_group!(
    pipeline,
    bench_itch_parse,
    bench_full_pipeline,
    bench_reference_vs_hot,
    bench_cpp_agreement
);
criterion_main!(pipeline);
