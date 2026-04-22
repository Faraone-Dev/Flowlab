//! Latency-distribution harness for the chaos detector chain.
//!
//! Why not Criterion: Criterion reports a confidence interval around
//! the *mean*. For a per-event hot path we care about the **tail**:
//! a single 5-µs hiccup inside a tight 50-ns inner loop is invisible
//! at the mean but lethal for live use. This harness measures
//! per-iteration ns/event, sorts, and reports p50 / p90 / p99 / max
//! plus stddev for three datasets that exercise different branches:
//!
//!   * `steady`  — calm `realistic_events` (60 % adds / 25 % cancels)
//!   * `bursty`  — cancel-heavy stream (35 / 55 / 10) hammering the
//!                 storm baseline and best-cache invalidation
//!   * `crashy`  — `realistic_events` + injected sweep+aggression
//!                 bursts every 5 000 events to actually fire the
//!                 flash-crash and ignition branches under timing
//!
//! Methodology:
//!   * Each dataset is generated once (deterministic seed).
//!   * The chain is rebuilt per iteration so internal state does not
//!     accumulate (matches `bench/chaos_throughput.rs`).
//!   * One full warm-up pass is run before timing to populate
//!     branch-predictor / page-walk / icache state.
//!   * On Windows we set the current process affinity to a single
//!     CPU core (`SetProcessAffinityMask`) for the duration of the
//!     run. On other platforms this is a no-op.
//!   * Iterations are measured with `Instant::now()`; resolution on
//!     modern Windows is sub-µs (QPC-backed).
//!
//! Output is plain text, machine-friendly: one block per dataset,
//! one row per detector configuration. Re-run after a code change
//! and diff the rows.

use std::time::Instant;

use flowlab_bench::{bursty_events, crashy_events, realistic_events, MarketConfig};
use flowlab_chaos::ChaosChain;
use flowlab_core::event::SequencedEvent;

const STREAM_LEN: usize = 50_000;
const ITERATIONS: usize = 200;

fn main() {
    pin_to_one_core();
    println!(
        "chaos_latency  stream={} iters={}  (Windows: process pinned to CPU 0)",
        STREAM_LEN, ITERATIONS
    );
    println!();

    let cfg = MarketConfig::default();

    // ── Generate the three datasets ONCE so we don't measure the
    //    generator. They live for the full duration of the bench so
    //    the bytes stay hot in L2/L3 across iterations.
    let steady = realistic_events(STREAM_LEN, &cfg);
    let bursty = bursty_events(STREAM_LEN, &cfg);
    let crashy = crashy_events(STREAM_LEN, &cfg);

    for (name, stream) in [
        ("steady", &steady),
        ("bursty", &bursty),
        ("crashy", &crashy),
    ] {
        let stats = measure_chain(stream, ITERATIONS);
        report(name, stream.len(), &stats);
    }
}

#[derive(Debug)]
struct Stats {
    mean_ns: f64,
    stddev_ns: f64,
    p50_ns: f64,
    p90_ns: f64,
    p99_ns: f64,
    min_ns: f64,
    max_ns: f64,
    flags_per_iter: u64,
}

/// Measures the *full chain* (5 detectors, default ITCH tuning) over
/// `stream` for `iters` iterations. Returns the per-event ns
/// distribution.
fn measure_chain(stream: &[SequencedEvent], iters: usize) -> Stats {
    // ── Warm up: one full pass to prime branch predictor / icache.
    {
        let mut chain = ChaosChain::default_itch();
        let mut sink: u64 = 0;
        for ev in stream {
            sink = sink.wrapping_add(chain.process(ev).len() as u64);
        }
        std::hint::black_box(sink);
    }

    let mut samples_ns: Vec<f64> = Vec::with_capacity(iters);
    let mut last_flags: u64 = 0;

    for _ in 0..iters {
        let mut chain = ChaosChain::default_itch();
        let t0 = Instant::now();
        for ev in stream {
            let flags = chain.process(std::hint::black_box(ev));
            std::hint::black_box(&flags);
        }
        let elapsed_ns = t0.elapsed().as_nanos() as f64;
        let per_event_ns = elapsed_ns / stream.len() as f64;
        samples_ns.push(per_event_ns);
        last_flags = chain.total_flags();
    }

    samples_ns.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = samples_ns.len();
    let mean = samples_ns.iter().sum::<f64>() / n as f64;
    let var = samples_ns
        .iter()
        .map(|x| (x - mean).powi(2))
        .sum::<f64>()
        / n as f64;
    let stddev = var.sqrt();

    Stats {
        mean_ns: mean,
        stddev_ns: stddev,
        p50_ns: samples_ns[n / 2],
        p90_ns: samples_ns[(n * 9) / 10],
        p99_ns: samples_ns[(n * 99) / 100],
        min_ns: samples_ns[0],
        max_ns: samples_ns[n - 1],
        flags_per_iter: last_flags,
    }
}

fn report(label: &str, stream_len: usize, s: &Stats) {
    println!("─── dataset: {label} (n={stream_len}) ───");
    println!(
        "  mean   {:>7.1} ns/ev  stddev {:>6.2}  ({:.2}%)",
        s.mean_ns,
        s.stddev_ns,
        100.0 * s.stddev_ns / s.mean_ns
    );
    println!(
        "  p50    {:>7.1}   p90 {:>7.1}   p99 {:>7.1}",
        s.p50_ns, s.p90_ns, s.p99_ns
    );
    println!(
        "  min    {:>7.1}   max {:>7.1}",
        s.min_ns, s.max_ns
    );
    println!("  flags/iter: {}", s.flags_per_iter);
    println!();
}

// ─── CPU pinning ───────────────────────────────────────────────────
//
// On Windows we pin the current process to CPU 0 via
// `SetProcessAffinityMask`. We avoid pulling the `windows-sys` crate
// just for this — the Win32 ABI is stable, so we declare the symbol
// directly. On non-Windows targets the function is a no-op.

#[cfg(windows)]
fn pin_to_one_core() {
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut std::ffi::c_void;
        fn SetProcessAffinityMask(handle: *mut std::ffi::c_void, mask: usize) -> i32;
    }
    unsafe {
        let h = GetCurrentProcess();
        let _ = SetProcessAffinityMask(h, 0x1);
    }
}

#[cfg(not(windows))]
fn pin_to_one_core() {
    // No-op: cross-platform pinning needs sched_setaffinity (Linux),
    // pthread_setaffinity_np (BSD), thread_policy_set (macOS). Out of
    // scope for this harness; the Criterion bench remains the
    // platform-portable measurement.
}
