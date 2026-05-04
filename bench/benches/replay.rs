// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use flowlab_bench::{realistic_events, warmup_events, MarketConfig};
use flowlab_core::hot_book::HotOrderBook;
use flowlab_core::orderbook::OrderBook;
use flowlab_core::state::ReplayState;

/// Replay 100K realistic events through the canonical state machine.
fn bench_replay(c: &mut Criterion) {
    let events = realistic_events(100_000, &MarketConfig::default());
    let mut g = c.benchmark_group("replay");
    g.throughput(Throughput::Elements(events.len() as u64));
    g.bench_function("100k_events_realistic", |b| {
        b.iter(|| {
            let mut state = ReplayState::new(1);
            for event in &events {
                let _ = state.process(black_box(event));
            }
            black_box(state.hash())
        })
    });
    g.finish();
}

/// Order-book operation throughput on a pre-warmed book (steady-state).
fn bench_book_ops(c: &mut Criterion) {
    let cfg = MarketConfig::default();
    let warmup = warmup_events(2_000, &cfg);
    let ops = realistic_events(10_000, &cfg);

    let mut g = c.benchmark_group("orderbook_steady_state");
    g.throughput(Throughput::Elements(ops.len() as u64));

    g.bench_function("btreemap_l3_10k", |b| {
        b.iter_batched(
            || {
                let mut book = OrderBook::new(1);
                for e in &warmup {
                    book.apply(&e.event);
                }
                book
            },
            |mut book| {
                for e in &ops {
                    book.apply(black_box(&e.event));
                }
                black_box(book.best_bid())
            },
            criterion::BatchSize::SmallInput,
        )
    });

    g.bench_function("hot_array_l2_10k", |b| {
        b.iter_batched(
            || {
                let mut book = HotOrderBook::<256>::new(1);
                for e in &warmup {
                    book.apply(&e.event);
                }
                book
            },
            |mut book| {
                for e in &ops {
                    book.apply(black_box(&e.event));
                }
                black_box(book.best_bid())
            },
            criterion::BatchSize::SmallInput,
        )
    });

    g.finish();
}

/// Top-N depth aggregation — the L3-optimized SIMD scan vs BTreeMap iter.
fn bench_depth(c: &mut Criterion) {
    let cfg = MarketConfig::default();
    let events = realistic_events(5_000, &cfg);

    let mut btree = OrderBook::new(1);
    let mut hot = HotOrderBook::<256>::new(1);
    for e in &events {
        btree.apply(&e.event);
        hot.apply(&e.event);
    }

    let mut g = c.benchmark_group("depth_top10");
    g.bench_function("btreemap", |b| {
        b.iter(|| {
            let bid: u64 = btree.bids.values().rev().take(10).map(|l| l.total_qty).sum();
            let ask: u64 = btree.asks.values().take(10).map(|l| l.total_qty).sum();
            black_box(bid + ask)
        })
    });
    g.bench_function("hot_simd_unrolled", |b| {
        b.iter(|| black_box(hot.bid_depth(10) + hot.ask_depth(10)))
    });
    g.finish();
}

/// Cross-implementation L2 hash agreement: verifies that BTreeMap and
/// the SIMD array book produce identical canonical L2 hashes when
/// fed identical event streams. Also a useful benchmark of the hash itself.
fn bench_canonical_hash(c: &mut Criterion) {
    let cfg = MarketConfig::default();
    let events = realistic_events(5_000, &cfg);

    let mut btree = OrderBook::new(1);
    let mut hot = HotOrderBook::<256>::new(1);
    for e in &events {
        btree.apply(&e.event);
        hot.apply(&e.event);
    }

    // Verification — must agree
    let h1 = btree.canonical_l2_hash();
    let h2 = hot.canonical_l2_hash();
    assert_eq!(
        h1, h2,
        "canonical L2 hashes diverge between OrderBook and HotOrderBook!"
    );

    let mut g = c.benchmark_group("canonical_l2_hash");
    g.bench_function("btreemap", |b| b.iter(|| black_box(btree.canonical_l2_hash())));
    g.bench_function("hot", |b| b.iter(|| black_box(hot.canonical_l2_hash())));
    g.finish();
}

criterion_group!(
    benches,
    bench_replay,
    bench_book_ops,
    bench_depth,
    bench_canonical_hash,
);
criterion_main!(benches);
