// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

//! Chaos detector chain throughput bench.
//!
//! Goal: measure the cost-per-event of running the full 5-detector
//! `ChaosChain` over a realistic synthetic stream, and compare it to a
//! baseline that does *only* the per-event work the executor would
//! otherwise do (a no-op closure). The delta is the marginal chaos
//! cost the lab pays when the chain is attached.
//!
//! Structure:
//!   * `chain_full`        — all 5 detectors via `ChaosChain::default_itch`
//!   * `chain_full_into`   — same chain, `process_into` with reused buffer
//!   * `baseline_no_chain` — same loop, no detection (lower bound)
//!   * `per_detector/*`    — each detector standalone, for breakdown
//!
//! Notes:
//!   * Stream is generated once outside `iter` so the bench does not
//!     measure the generator.
//!   * Detectors are reconstructed at every `iter` so internal state
//!     (mini-books, baselines, cooldowns) does not accumulate across
//!     samples and skew later iterations.
//!   * Throughput is reported in *elements* (events), so Criterion
//!     prints `Melem/s` directly comparable across the three groups.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use flowlab_bench::{realistic_events, MarketConfig};
use flowlab_chaos::{
    CancellationStormDetector, ChaosChain, FlashCrashDetector, LatencyArbProxyDetector,
    MomentumIgnitionDetector, PhantomLiquidityDetector,
};

const STREAM_SIZES: &[usize] = &[10_000, 50_000];

fn make_stream(n: usize) -> Vec<flowlab_core::event::SequencedEvent> {
    let cfg = MarketConfig {
        seed: 0xC0FFEE_u64,
        ..MarketConfig::default()
    };
    realistic_events(n, &cfg)
}

// ─── Group 1 — full chain ──────────────────────────────────────────

fn bench_chain_full(c: &mut Criterion) {
    let mut group = c.benchmark_group("chaos/chain_full");
    for &n in STREAM_SIZES {
        let stream = make_stream(n);
        group.throughput(Throughput::Elements(stream.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &stream, |b, stream| {
            b.iter(|| {
                let mut chain = ChaosChain::default_itch();
                for ev in stream {
                    let flags = chain.process(black_box(ev));
                    black_box(&flags);
                }
                black_box(chain.total_flags());
            });
        });
    }
    group.finish();
}

// ─── Group 1b — full chain, zero-alloc API ─────────────────────────
//
// Identical to `chain_full` except the per-event Vec is hoisted out
// of the inner loop and reused via `process_into`. The Criterion delta
// between the two groups is the per-event allocator cost the chain
// pays when its caller doesn't own the buffer.

fn bench_chain_full_into(c: &mut Criterion) {
    let mut group = c.benchmark_group("chaos/chain_full_into");
    for &n in STREAM_SIZES {
        let stream = make_stream(n);
        group.throughput(Throughput::Elements(stream.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &stream, |b, stream| {
            b.iter(|| {
                let mut chain = ChaosChain::default_itch();
                let mut buf: Vec<flowlab_chaos::ChaosEvent> = Vec::with_capacity(8);
                for ev in stream {
                    buf.clear();
                    chain.process_into(black_box(ev), &mut buf);
                    black_box(&buf);
                }
                black_box(chain.total_flags());
            });
        });
    }
    group.finish();
}

// ─── Group 2 — baseline (no chain) ─────────────────────────────────

fn bench_baseline_no_chain(c: &mut Criterion) {
    let mut group = c.benchmark_group("chaos/baseline_no_chain");
    for &n in STREAM_SIZES {
        let stream = make_stream(n);
        group.throughput(Throughput::Elements(stream.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &stream, |b, stream| {
            b.iter(|| {
                // Same iteration shape as chain_full, but the "work" is a
                // black_box pass-through. Establishes the lower bound the
                // chain delta is measured against.
                let mut sink: u64 = 0;
                for ev in stream {
                    let ev = black_box(ev);
                    sink = sink.wrapping_add(ev.seq);
                }
                black_box(sink);
            });
        });
    }
    group.finish();
}

// ─── Group 3 — per-detector breakdown ──────────────────────────────
//
// Same stream / same loop shape as the chain, isolating one detector
// at a time. Lets us attribute the chain's total cost to each
// component without instrumenting the chain itself.

fn bench_per_detector(c: &mut Criterion) {
    let mut group = c.benchmark_group("chaos/per_detector");
    let n = STREAM_SIZES[1]; // single representative size
    let stream = make_stream(n);
    group.throughput(Throughput::Elements(stream.len() as u64));

    group.bench_function("phantom_liquidity", |b| {
        b.iter(|| {
            let mut det = PhantomLiquidityDetector::new(256, 5_000_000, 4096, 100);
            for ev in &stream {
                black_box(det.process(black_box(ev)));
            }
        });
    });

    group.bench_function("cancellation_storm", |b| {
        b.iter(|| {
            let mut det = CancellationStormDetector::new(2_048, 0, 128, 1_024, 0.05, 4.0, 8, 4_096);
            for ev in &stream {
                black_box(det.process(black_box(ev)));
            }
        });
    });

    group.bench_function("momentum_ignition", |b| {
        b.iter(|| {
            let mut det = MomentumIgnitionDetector::new(1_024, 0, 64, 5, 8, 4, 2_048);
            for ev in &stream {
                black_box(det.process(black_box(ev)));
            }
        });
    });

    group.bench_function("flash_crash", |b| {
        b.iter(|| {
            let mut det = FlashCrashDetector::new(10, 8, 64, 5, 0.6, 3, 2_048);
            for ev in &stream {
                black_box(det.process(black_box(ev)));
            }
        });
    });

    group.bench_function("latency_arb_proxy", |b| {
        b.iter(|| {
            let mut det =
                LatencyArbProxyDetector::new(64, 0, 64, 4, 200, 4, false, 0, 1_024);
            for ev in &stream {
                black_box(det.process(black_box(ev)));
            }
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_chain_full,
    bench_chain_full_into,
    bench_baseline_no_chain,
    bench_per_detector
);
criterion_main!(benches);
