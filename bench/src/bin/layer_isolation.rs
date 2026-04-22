//! Layer-isolation diagnostic.
//!
//! Goal: when sustained-throughput benchmarks drift over multiple runs,
//! separate the three suspect layers:
//!
//!   (A) CPU frequency scaling / turbo throttling
//!   (B) Memory / cache / TLB working-set effects
//!   (C) Allocator / page-cache contamination across processes
//!
//! Strategy: run the same fixed workload N times **inside one process**,
//! with all input buffers pre-allocated **before** the timing loop. Then:
//!
//!   * If per-iter wall time DRIFTS but rdtsc-cycles per iter stays flat
//!     → CPU frequency dropped (turbo throttle / power scheme).
//!     This is layer (A).
//!
//!   * If both wall time AND cycles per iter drift up monotonically
//!     → working-set / cache pressure or branch-predictor pollution.
//!     This is layer (B).
//!
//!   * If iter-1 is much slower than iter-2..N then flat afterwards
//!     → cold-cache + first-touch faulting; not a sustained problem.
//!
//!   * If everything is flat in-process, but multi-process Criterion
//!     runs drift → layer (C) (allocator state across processes,
//!     page cache on shared inputs, etc).
//!
//! No allocations happen inside the timing loop. `Vec` capacity is
//! reserved once. `HotOrderBook` is reset by `clear()` between iters,
//! not re-allocated.
//!
//! Output is one CSV line per iter so the user can paste it into a
//! spreadsheet and see drift trends without our interpretation.

use std::time::Instant;

use flowlab_bench::itch::{parse_buffer, synthetic_itch_stream};
use flowlab_core::event::Event;

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn rdtsc() -> u64 {
    unsafe { core::arch::x86_64::_rdtsc() }
}
#[cfg(not(target_arch = "x86_64"))]
#[inline(always)]
fn rdtsc() -> u64 {
    0
}

fn main() {
    // ---- config ---------------------------------------------------
    let n_events: usize = 100_000;
    let iters: usize = 50;
    let seed: u64 = 0xC0FFEE;

    eprintln!("flowlab layer-isolation diagnostic");
    eprintln!("  events/iter = {n_events}");
    eprintln!("  iterations  = {iters}");
    eprintln!("  workload    = parse_buffer (ITCH zero-copy)");
    eprintln!();

    // ---- pre-allocate everything OUTSIDE the timing loop ----------
    let raw = synthetic_itch_stream(n_events, seed);
    let raw_bytes = raw.len();
    eprintln!("  raw bytes   = {raw_bytes} ({:.2} MiB)", raw_bytes as f64 / (1024.0 * 1024.0));

    // Pre-size the output Vec to the EXACT capacity needed. This
    // pins the allocator out as a variable: no realloc, no growth,
    // no fragmentation noise.
    let mut parsed: Vec<Event> = Vec::with_capacity(n_events);

    // ---- warm-up: 3 untimed runs to fault-in pages, prime caches,
    //               warm branch predictor, and seed turbo bin.
    for _ in 0..3 {
        parsed.clear();
        parse_buffer(&raw, &mut parsed).unwrap();
    }

    // ---- timed loop -----------------------------------------------
    println!("iter,wall_ns,cycles,cycles_per_event,wall_per_event_ns,implied_ghz,parsed");
    for i in 0..iters {
        parsed.clear();
        // No alloc here: capacity preserved across clear()+push()
        // up to the original 100k slots.

        let c0 = rdtsc();
        let t0 = Instant::now();
        parse_buffer(&raw, &mut parsed).unwrap();
        let wall_ns = t0.elapsed().as_nanos() as u64;
        let c1 = rdtsc();

        let cycles = c1.wrapping_sub(c0);
        let cpe = cycles as f64 / parsed.len() as f64;
        let wpe = wall_ns as f64 / parsed.len() as f64;
        // implied frequency: cycles/sec = cycles / wall_seconds
        let implied_ghz = (cycles as f64) / (wall_ns as f64); // cycles per ns == GHz

        println!(
            "{i},{wall_ns},{cycles},{cpe:.2},{wpe:.2},{implied_ghz:.3},{}",
            parsed.len()
        );
    }

    eprintln!();
    eprintln!("Reading the CSV:");
    eprintln!("  * If implied_ghz drops while cycles_per_event stays flat → layer (A) CPU scaling.");
    eprintln!("  * If cycles_per_event drifts up over iterations          → layer (B) cache/TLB/working-set.");
    eprintln!("  * If both flat in-process but Criterion still drifts     → layer (C) allocator/page-cache across processes.");
}
