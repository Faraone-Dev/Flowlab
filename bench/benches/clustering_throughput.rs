// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

//! Chaos clusterer throughput bench.
//!
//! Goal: measure the per-event cost of `ChaosClusterer::cluster` under
//! the kind+initiator merge policy and the dual seq+time window, on
//! three input shapes that exercise different code paths inside
//! `should_merge` / `absorb`.
//!
//! Structure:
//!   * `clustering/seq_only`    — backwards-compat path (time window disabled),
//!                                realistic burst-shaped input
//!   * `clustering/dual_window` — both windows enabled, same input shape;
//!                                delta vs `seq_only` is the dual-window tax
//!   * `clustering/shape`       — fixed 10k events, two pathological shapes:
//!       - `all_merge`: same kind + same initiator + tight seq → every
//!         comparison goes through the merge branch (`absorb` dominates)
//!       - `all_split`: distinct kind/initiator + wide seq gap → every
//!         comparison goes through the split branch (`from_event` dominates)
//!
//! Notes:
//!   * Inputs are generated once outside `iter` so the bench does not
//!     measure event construction.
//!   * The clusterer is reconstructed at every `iter` (it owns no state
//!     between calls anyway, but this keeps the hot-loop allocator
//!     behaviour identical across samples).
//!   * Throughput is `Throughput::Elements(n)` so Criterion prints
//!     `Melem/s` directly comparable across groups and sizes.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use flowlab_chaos::{ChaosClusterer, ChaosEvent, ChaosFeatures, ChaosKind};

const STREAM_SIZES: &[usize] = &[1_000, 10_000, 50_000];

fn kinds() -> [ChaosKind; 7] {
    [
        ChaosKind::QuoteStuff,
        ChaosKind::Spoof,
        ChaosKind::PhantomLiquidity,
        ChaosKind::CancellationStorm,
        ChaosKind::MomentumIgnition,
        ChaosKind::FlashCrash,
        ChaosKind::LatencyArbitrage,
    ]
}

fn ev(
    kind: ChaosKind,
    seq: u64,
    ts_ns: u64,
    initiator: Option<u64>,
) -> ChaosEvent {
    ChaosEvent {
        kind,
        start_seq: seq,
        end_seq: seq,
        start_ts_ns: ts_ns,
        end_ts_ns: ts_ns,
        severity: 0.5,
        initiator,
        features: ChaosFeatures {
            event_count: 1,
            duration_ns: 0,
            cancel_trade_ratio: 0.0,
            price_displacement: 0,
            depth_removed: 0,
        },
    }
}

/// Realistic shape: short bursts of same-kind/same-initiator events
/// (3..8 contiguous in seq) interleaved with kind/initiator switches
/// and inter-burst gaps. Exercises both merge and split paths.
fn realistic_events(n: usize, seed: u64) -> Vec<ChaosEvent> {
    let kinds = kinds();
    let mut out = Vec::with_capacity(n);
    let mut s = seed;
    let mut seq: u64 = 0;
    let mut ts: u64 = 0;
    while out.len() < n {
        // xorshift64
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        let burst = (s as usize % 6) + 3; // 3..8
        let kind = kinds[((s >> 8) as usize) % kinds.len()];
        let initiator = if (s >> 16) & 1 == 0 {
            Some(((s >> 24) & 0xFFFF) as u64)
        } else {
            None
        };
        for _ in 0..burst {
            if out.len() >= n {
                break;
            }
            seq += 1;
            ts += 1_000;
            out.push(ev(kind, seq, ts, initiator));
        }
        // Inter-burst gap: large enough that the next burst usually
        // starts a new cluster under both windows.
        seq += 50;
        ts += 5_000_000;
    }
    out.truncate(n);
    out
}

/// All events same kind + same initiator + tightly packed in seq.
/// Forces every comparison through `absorb`.
fn all_merge_events(n: usize) -> Vec<ChaosEvent> {
    (0..n)
        .map(|i| ev(ChaosKind::PhantomLiquidity, i as u64, (i as u64) * 1_000, Some(42)))
        .collect()
}

/// Each event a distinct kind/initiator pair, widely spaced in seq AND
/// in ts. Forces every comparison through `from_event` + push.
fn all_split_events(n: usize) -> Vec<ChaosEvent> {
    let kinds = kinds();
    (0..n)
        .map(|i| {
            ev(
                kinds[i % kinds.len()],
                (i as u64) * 10_000,
                (i as u64) * 1_000_000_000,
                Some(i as u64),
            )
        })
        .collect()
}

// ─── Group 1 — seq-only (BC path) ──────────────────────────────────

fn bench_seq_only(c: &mut Criterion) {
    let mut group = c.benchmark_group("clustering/seq_only");
    for &n in STREAM_SIZES {
        let events = realistic_events(n, 0xC0FFEE);
        group.throughput(Throughput::Elements(events.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &events, |b, ev| {
            b.iter(|| {
                let cl = ChaosClusterer::new(100);
                let out = cl.cluster(black_box(ev));
                black_box(out);
            });
        });
    }
    group.finish();
}

// ─── Group 2 — dual window (seq + time) ────────────────────────────

fn bench_dual_window(c: &mut Criterion) {
    let mut group = c.benchmark_group("clustering/dual_window");
    for &n in STREAM_SIZES {
        let events = realistic_events(n, 0xC0FFEE);
        group.throughput(Throughput::Elements(events.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &events, |b, ev| {
            b.iter(|| {
                let cl = ChaosClusterer::with_windows(100, 5_000_000);
                let out = cl.cluster(black_box(ev));
                black_box(out);
            });
        });
    }
    group.finish();
}

// ─── Group 3 — shape (all_merge vs all_split) ──────────────────────

fn bench_shape(c: &mut Criterion) {
    let mut group = c.benchmark_group("clustering/shape");
    let n: usize = 10_000;
    group.throughput(Throughput::Elements(n as u64));

    let merge = all_merge_events(n);
    group.bench_function("all_merge", |b| {
        b.iter(|| {
            let cl = ChaosClusterer::new(100);
            black_box(cl.cluster(black_box(&merge)));
        });
    });

    let split = all_split_events(n);
    group.bench_function("all_split", |b| {
        b.iter(|| {
            let cl = ChaosClusterer::new(100);
            black_box(cl.cluster(black_box(&split)));
        });
    });

    group.finish();
}

criterion_group!(benches, bench_seq_only, bench_dual_window, bench_shape);
criterion_main!(benches);
