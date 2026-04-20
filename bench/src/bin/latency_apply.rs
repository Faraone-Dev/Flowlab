//! Latency histogram harness — per-event distribution of `HotOrderBook::apply`.
//!
//! ## Why this file exists
//!
//! Criterion gives `[low mean high]` confidence intervals on the
//! AVERAGE of N iterations. That hides every interesting failure mode
//! of a low-latency engine:
//!
//!   * cache eviction spikes after long idle gaps,
//!   * branch-mispredict bursts on rare event types,
//!   * hash-index rehash stalls,
//!   * scheduler preemption / interrupt service routines,
//!   * page faults on first touch of a fresh slab slot,
//!   * TLB pressure as the working set crosses 2 MB.
//!
//! This binary measures the **distribution shape** of every single
//! `apply()` call across four phases.
//!
//! ## Clock — `rdtsc`
//!
//! `Instant::now()` on Windows uses `QueryPerformanceCounter` with
//! ~100 ns granularity. That is wider than a single `apply()` (~30 ns
//! on this engine), so per-call samples would be quantised to 0 and
//! 100 ns. Useless.
//!
//! We use `rdtsc` instead — a CPU read of the time-stamp counter.
//! Resolution: cycle-accurate (~0.25 ns at 4 GHz). Overhead: a few
//! cycles. We calibrate the cycles→ns ratio against `Instant` once at
//! startup and subtract a measured per-pair overhead from every sample.
//!
//! ## Phases
//!
//!   1. WARMUP        (not reported — populates slab, primes branches)
//!   2. STEADY        realistic 60/25/15 add/cancel/trade
//!   3. BURST         256-event bursts separated by 100 µs cold gaps
//!   4. CANCEL-HEAVY  70% cancel / 30% add — maximum slab churn
//!
//! ## Interpretation
//!
//!   p50         : the everyday cost
//!   p95 / p99   : load sensitivity, cache pressure
//!   p99.9       : real tail behaviour an HFT venue cares about
//!   max         : worst single-event spike (system instability)
//!   p99.9/p50   : variance ratio — lower is more stable

use std::time::Instant;

use flowlab_bench::{realistic_events, warmup_events, MarketConfig, XorShift64};
use flowlab_core::event::{Event, EventType, SequencedEvent};
use flowlab_core::hot_book::HotOrderBook;

// ─── rdtsc clock ─────────────────────────────────────────────────────

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn rdtsc() -> u64 {
    // SAFETY: `_rdtsc` is always available on x86_64.
    unsafe { core::arch::x86_64::_rdtsc() }
}

#[cfg(not(target_arch = "x86_64"))]
#[inline(always)]
fn rdtsc() -> u64 {
    use std::sync::OnceLock;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

/// Returns (cycles per nanosecond, per-pair `rdtsc` overhead in cycles).
fn calibrate() -> (f64, u64) {
    // 1. cycles per nanosecond — median of five 100 ms windows so that
    //    a single context switch in the calibration window is rejected.
    let mut ratios: [f64; 5] = [0.0; 5];
    for slot in ratios.iter_mut() {
        let t0 = Instant::now();
        let c0 = rdtsc();
        while t0.elapsed().as_millis() < 100 {
            std::hint::spin_loop();
        }
        let c1 = rdtsc();
        let elapsed_ns = t0.elapsed().as_nanos() as f64;
        *slot = (c1 - c0) as f64 / elapsed_ns;
    }
    let mut sorted = ratios;
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let cycles_per_ns = sorted[2];

    // 2. per-pair overhead — median of 100 K back-to-back rdtsc pairs.
    const N: usize = 100_000;
    let mut samples: Vec<u64> = Vec::with_capacity(N);
    for _ in 0..N {
        let a = rdtsc();
        let b = rdtsc();
        samples.push(b.wrapping_sub(a));
    }
    samples.sort_unstable();
    let overhead_cycles = samples[N / 2];

    (cycles_per_ns, overhead_cycles)
}

#[inline(always)]
fn cycles_to_ns(cycles: u64, cycles_per_ns: f64) -> u64 {
    (cycles as f64 / cycles_per_ns) as u64
}

// ─── Histogram ───────────────────────────────────────────────────────

/// Log-linear: 1 ns .. ~16 ms with 8 sub-bins per power of two ⇒ 192 bins.
const BIN_GROUPS: usize = 24;
const SUBBINS: usize = 8;
const N_BINS: usize = BIN_GROUPS * SUBBINS;

#[derive(Clone)]
struct Histogram {
    counts: [u64; N_BINS],
    overflow: u64,
    n: u64,
}

impl Histogram {
    fn new() -> Self {
        Self { counts: [0; N_BINS], overflow: 0, n: 0 }
    }

    #[inline]
    fn bin_of(ns: u64) -> usize {
        if ns == 0 {
            return 0;
        }
        let group = 63 - ns.leading_zeros() as usize;
        if group >= BIN_GROUPS {
            return N_BINS;
        }
        let lo = 1u64 << group;
        let span = lo;
        let sub = ((ns - lo) * SUBBINS as u64 / span) as usize;
        group * SUBBINS + sub.min(SUBBINS - 1)
    }

    fn lower_ns(idx: usize) -> u64 {
        let group = idx / SUBBINS;
        let sub = idx % SUBBINS;
        let lo = 1u64 << group;
        let span = lo;
        lo + (sub as u64) * span / SUBBINS as u64
    }

    #[inline]
    fn record(&mut self, ns: u64) {
        let idx = Self::bin_of(ns);
        if idx >= N_BINS {
            self.overflow += 1;
        } else {
            self.counts[idx] += 1;
        }
        self.n += 1;
    }

    fn percentile(&self, p: f64) -> u64 {
        if self.n == 0 {
            return 0;
        }
        let target = ((self.n as f64) * p / 100.0).ceil() as u64;
        let mut acc: u64 = 0;
        for (i, &c) in self.counts.iter().enumerate() {
            acc += c;
            if acc >= target {
                return Self::lower_ns(i);
            }
        }
        u64::MAX
    }

    fn max(&self) -> u64 {
        if self.overflow > 0 {
            return u64::MAX;
        }
        for i in (0..N_BINS).rev() {
            if self.counts[i] > 0 {
                return Self::lower_ns(i + 1).saturating_sub(1);
            }
        }
        0
    }
}

// ─── Phase generators ────────────────────────────────────────────────

fn cancel_heavy_events(n: usize, seed: u64) -> Vec<SequencedEvent> {
    let mut rng = XorShift64(seed);
    let mut out = Vec::with_capacity(n);
    let mut live: Vec<u64> = Vec::with_capacity(n / 4);
    let mut next_oid: u64 = 1;
    for i in 0..n {
        let pick = rng.next_bounded(100);
        let evt = if pick < 70 && !live.is_empty() {
            let idx = (rng.next_bounded(live.len() as u64)) as usize;
            let oid = live.swap_remove(idx);
            Event {
                ts: (i as u64) * 1_000,
                instrument_id: 1,
                event_type: EventType::OrderCancel as u8,
                side: 0,
                price: 0,
                qty: 0,
                order_id: oid,
                _pad: [0; 2],
            }
        } else {
            let oid = next_oid;
            next_oid += 1;
            let side = (rng.next_u64() & 1) as u8;
            let price = 10_000 + (rng.next_bounded(200) as u64);
            let qty = 1 + rng.next_bounded(100);
            live.push(oid);
            Event {
                ts: (i as u64) * 1_000,
                instrument_id: 1,
                event_type: EventType::OrderAdd as u8,
                side,
                price,
                qty,
                order_id: oid,
                _pad: [0; 2],
            }
        };
        out.push(SequencedEvent { seq: (i as u64) + 1, channel_id: 0, event: evt });
    }
    out
}

/// Pre-populate the book with `n_orders` ADDs of HUGE quantity so a
/// long stream of TRADEs never depletes a level. Returned as the
/// warmup payload to apply BEFORE measurement.
fn fat_warmup(n_orders: usize, seed: u64) -> Vec<SequencedEvent> {
    let mut rng = XorShift64(seed);
    let mut out = Vec::with_capacity(n_orders);
    for i in 0..n_orders {
        let side = (rng.next_u64() & 1) as u8;
        let level = rng.geometric(40, 100).min(49) as u64;
        let price = if side == 0 { 10_000 - level } else { 10_001 + level };
        out.push(SequencedEvent {
            seq: (i as u64) + 1,
            channel_id: 0,
            event: Event {
                ts: (i as u64) * 1_000,
                instrument_id: 1,
                event_type: EventType::OrderAdd as u8,
                side,
                price,
                qty: 1_000_000_000, // unkillable by any realistic trade burst
                order_id: (i as u64) + 1,
                _pad: [0; 2],
            },
        });
    }
    out
}

/// Pure TRADE workload, indexed against a pre-populated book of
/// `live_orders` IDs from `[1 ..= live_orders]`. Each trade fills a
/// tiny slice (qty=1) so the level is NEVER consumed — we measure
/// pure decrement-on-existing-level latency, with no structural
/// mutation of the grid.
fn trade_isolated_events(n: usize, live_orders: u64, seed: u64) -> Vec<SequencedEvent> {
    let mut rng = XorShift64(seed);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let oid = 1 + rng.next_bounded(live_orders);
        out.push(SequencedEvent {
            seq: (i as u64) + 1,
            channel_id: 0,
            event: Event {
                ts: (i as u64) * 1_000,
                instrument_id: 1,
                event_type: EventType::Trade as u8,
                side: 0,
                price: 0,
                qty: 1,
                order_id: oid,
                _pad: [0; 2],
            },
        });
    }
    out
}

// ─── Measurement loops ───────────────────────────────────────────────

/// One histogram per event-type plus an aggregate. We dispatch on the
/// event-type byte AFTER reading the closing rdtsc, so the dispatch
/// branch and the recording arithmetic do not pollute the measurement
/// window. The single byte we read for dispatch is already in L1 from
/// the apply() call we just measured.
struct PerType {
    add: Histogram,
    cancel: Histogram,
    trade: Histogram,
    modify: Histogram,
    total: Histogram,
}

impl PerType {
    fn new() -> Self {
        Self {
            add: Histogram::new(),
            cancel: Histogram::new(),
            trade: Histogram::new(),
            modify: Histogram::new(),
            total: Histogram::new(),
        }
    }

    #[inline]
    fn record(&mut self, evt_type: u8, ns: u64) {
        self.total.record(ns);
        if evt_type == EventType::OrderAdd as u8 {
            self.add.record(ns);
        } else if evt_type == EventType::OrderCancel as u8 {
            self.cancel.record(ns);
        } else if evt_type == EventType::Trade as u8 {
            self.trade.record(ns);
        } else if evt_type == EventType::OrderModify as u8 {
            self.modify.record(ns);
        }
    }
}

fn measure_steady<const N: usize>(
    book: &mut HotOrderBook<N>,
    events: &[SequencedEvent],
    out: &mut PerType,
    cycles_per_ns: f64,
    overhead_cycles: u64,
) -> u64 {
    let mut total_cyc: u128 = 0;
    for se in events {
        let c0 = rdtsc();
        std::hint::black_box(book.apply(std::hint::black_box(&se.event)));
        let c1 = rdtsc();
        let net = c1.wrapping_sub(c0).saturating_sub(overhead_cycles);
        total_cyc += net as u128;
        out.record(se.event.event_type, cycles_to_ns(net, cycles_per_ns));
    }
    (total_cyc as f64 / cycles_per_ns) as u64
}

/// Same as `measure_steady` but issues a software prefetch on the
/// slab slot of `events[i+1]` *before* timing event `i`. The prefetch
/// is OUTSIDE the rdtsc window so its overhead is not charged to the
/// per-event histogram, but it IS charged to the wall-time accumulator
/// for an honest end-to-end comparison.
fn measure_steady_prefetch<const N: usize>(
    book: &mut HotOrderBook<N>,
    events: &[SequencedEvent],
    out: &mut PerType,
    cycles_per_ns: f64,
    overhead_cycles: u64,
) -> u64 {
    let mut total_cyc: u128 = 0;
    for i in 0..events.len() {
        // Lookahead = 1: hint the slab slot for event i+1.
        if i + 1 < events.len() {
            book.prefetch_event(&events[i + 1].event);
        }
        let se = &events[i];
        let c0 = rdtsc();
        std::hint::black_box(book.apply(std::hint::black_box(&se.event)));
        let c1 = rdtsc();
        let net = c1.wrapping_sub(c0).saturating_sub(overhead_cycles);
        total_cyc += net as u128;
        out.record(se.event.event_type, cycles_to_ns(net, cycles_per_ns));
    }
    (total_cyc as f64 / cycles_per_ns) as u64
}

fn measure_burst<const N: usize>(
    book: &mut HotOrderBook<N>,
    events: &[SequencedEvent],
    out: &mut PerType,
    cycles_per_ns: f64,
    overhead_cycles: u64,
) {
    const BURST_SIZE: usize = 256;
    const COLD_GAP_NS: u64 = 100_000;
    let mut i = 0;
    while i < events.len() {
        let end = (i + BURST_SIZE).min(events.len());
        for se in &events[i..end] {
            let c0 = rdtsc();
            std::hint::black_box(book.apply(std::hint::black_box(&se.event)));
            let c1 = rdtsc();
            let net = c1.wrapping_sub(c0).saturating_sub(overhead_cycles);
            out.record(se.event.event_type, cycles_to_ns(net, cycles_per_ns));
        }
        i = end;
        // Spin instead of sleep — sleeping yields to the scheduler and
        // we'd be measuring wakeup latency, not cache cold latency.
        let until = Instant::now() + std::time::Duration::from_nanos(COLD_GAP_NS);
        while Instant::now() < until {
            std::hint::spin_loop();
        }
    }
}

/// Measure each of the three lane passes independently. Returns a
/// `LaneStats` with honest per-lane wall time and event counts.
fn measure_lanes<const N: usize>(
    book: &mut HotOrderBook<N>,
    events: &[Event],
    cycles_per_ns: f64,
    overhead_cycles: u64,
) -> LaneStats {
    const WINDOW: usize = 64;
    let mut s = LaneStats::default();

    for chunk in events.chunks(WINDOW) {
        // Count per-lane events in this window.
        for e in chunk {
            if e.event_type == EventType::OrderAdd as u8 {
                s.n_add += 1;
            } else if e.event_type == EventType::Trade as u8 {
                s.n_trade += 1;
            } else if e.event_type == EventType::OrderCancel as u8 {
                s.n_cancel += 1;
            }
        }

        // ADD pass.
        let c0 = rdtsc();
        std::hint::black_box(book.apply_lane_adds(std::hint::black_box(chunk)));
        let c1 = rdtsc();
        s.add_cyc += c1.wrapping_sub(c0).saturating_sub(overhead_cycles) as u128;

        // TRADE pass.
        let c0 = rdtsc();
        std::hint::black_box(book.apply_lane_trades(std::hint::black_box(chunk)));
        let c1 = rdtsc();
        s.trade_cyc += c1.wrapping_sub(c0).saturating_sub(overhead_cycles) as u128;

        // CANCEL pass.
        let c0 = rdtsc();
        std::hint::black_box(book.apply_lane_cancels(std::hint::black_box(chunk)));
        let c1 = rdtsc();
        s.cancel_cyc += c1.wrapping_sub(c0).saturating_sub(overhead_cycles) as u128;
    }

    s.cycles_per_ns = cycles_per_ns;
    s
}

#[derive(Default)]
struct LaneStats {
    add_cyc: u128,
    trade_cyc: u128,
    cancel_cyc: u128,
    n_add: u64,
    n_trade: u64,
    n_cancel: u64,
    cycles_per_ns: f64,
}

impl LaneStats {
    fn add_per_event_ns(&self) -> u64 {
        if self.n_add == 0 {
            0
        } else {
            (self.add_cyc as f64 / self.cycles_per_ns / self.n_add as f64) as u64
        }
    }
    fn trade_per_event_ns(&self) -> u64 {
        if self.n_trade == 0 {
            0
        } else {
            (self.trade_cyc as f64 / self.cycles_per_ns / self.n_trade as f64) as u64
        }
    }
    fn cancel_per_event_ns(&self) -> u64 {
        if self.n_cancel == 0 {
            0
        } else {
            (self.cancel_cyc as f64 / self.cycles_per_ns / self.n_cancel as f64) as u64
        }
    }
    fn total_wall_ns(&self) -> u64 {
        ((self.add_cyc + self.trade_cyc + self.cancel_cyc) as f64 / self.cycles_per_ns) as u64
    }
    fn total_events(&self) -> u64 {
        self.n_add + self.n_trade + self.n_cancel
    }
}

// ─── Reporting ───────────────────────────────────────────────────────

fn report(name: &str, h: &Histogram) {
    if h.n == 0 {
        println!("  {:<14} n=       0  (no events of this type)", name);
        return;
    }
    let p50 = h.percentile(50.0);
    let p95 = h.percentile(95.0);
    let p99 = h.percentile(99.0);
    let p999 = h.percentile(99.9);
    let max = h.max();
    println!(
        "  {:<14} n={:>8}  p50={:>5} ns  p95={:>5} ns  p99={:>5} ns  p99.9={:>6} ns  max={:>9} ns  ovfl={}",
        name, h.n, p50, p95, p99, p999, max, h.overflow
    );
}

fn report_phase(phase: &str, p: &PerType) {
    println!("  [{phase}]");
    report("  TOTAL", &p.total);
    report("  ADD", &p.add);
    report("  CANCEL", &p.cancel);
    report("  TRADE", &p.trade);
    report("  MODIFY", &p.modify);
}

// ─── Main ────────────────────────────────────────────────────────────

fn main() {
    println!("flowlab — latency_apply harness");
    println!("================================\n");

    let (cycles_per_ns, overhead_cycles) = calibrate();
    let overhead_ns = (overhead_cycles as f64 / cycles_per_ns) as u64;
    println!(
        "rdtsc clock         : {:.4} cycles/ns ({:.2} GHz nominal)",
        cycles_per_ns, cycles_per_ns
    );
    println!(
        "rdtsc-pair overhead : {} cycles ≈ {} ns (subtracted from each sample)\n",
        overhead_cycles, overhead_ns
    );

    let cfg = MarketConfig::default();

    // 1. WARMUP — populate slab, fault pages, prime branch predictors.
    let warmup = warmup_events(50_000, &cfg);
    let mut book = HotOrderBook::<256>::new(1);
    for se in &warmup {
        book.apply(&se.event);
    }
    println!(
        "WARMUP done — book has {} bid levels, {} ask levels\n",
        book.bid_levels().len(),
        book.ask_levels().len()
    );

    // 2. STEADY-STATE.
    let steady_events = realistic_events(500_000, &cfg);
    let mut steady = PerType::new();
    let steady_wall_ns =
        measure_steady(&mut book, &steady_events, &mut steady, cycles_per_ns, overhead_cycles);

    // 3. BURST — fresh book + same warmup so cache state is comparable.
    let mut book_burst = HotOrderBook::<256>::new(1);
    for se in &warmup {
        book_burst.apply(&se.event);
    }
    let burst_events = realistic_events(
        200_000,
        &MarketConfig { seed: 0xB075_7000_F10A_DA01, ..MarketConfig::default() },
    );
    let mut burst = PerType::new();
    measure_burst(&mut book_burst, &burst_events, &mut burst, cycles_per_ns, overhead_cycles);

    // 4. CANCEL-HEAVY.
    let mut book_cancel = HotOrderBook::<256>::new(1);
    let cancel_events = cancel_heavy_events(500_000, 0xCA9C_E110_F4A2_B007);
    let mut cancel = PerType::new();
    let _ = measure_steady(
        &mut book_cancel,
        &cancel_events,
        &mut cancel,
        cycles_per_ns,
        overhead_cycles,
    );

    // 5. TRADE-ISOLATED — 100% TRADE on a fat-warmed book. No ADD/
    //    CANCEL between trades, so this is the intrinsic TRADE cost
    //    with the book working set fully resident.
    const FAT_ORDERS: u64 = 50_000;
    let fat = fat_warmup(FAT_ORDERS as usize, 0xF0A7_BEEF_CAFE_2026);
    let mut book_trade = HotOrderBook::<256>::new(1);
    for se in &fat {
        book_trade.apply(&se.event);
    }
    let trade_only_events = trade_isolated_events(500_000, FAT_ORDERS, 0x57AB_E110_5DA1_BABE);
    let mut trade_iso = PerType::new();
    let _ = measure_steady(
        &mut book_trade,
        &trade_only_events,
        &mut trade_iso,
        cycles_per_ns,
        overhead_cycles,
    );

    // 6. LANE-BATCHED STEADY — same input as STEADY phase, but applied
    //    via three homogeneous passes per 64-event window. Compared
    //    against the interleaved STEADY phase via TOTAL WALL TIME on
    //    the same input — the only mathematically honest comparison
    //    when one path uses single-event timing and the other uses
    //    whole-pass timing.
    let mut book_lanes = HotOrderBook::<256>::new(1);
    for se in &warmup {
        book_lanes.apply(&se.event);
    }
    let lane_events: Vec<Event> = steady_events.iter().map(|se| se.event).collect();
    let lanes = measure_lanes(&mut book_lanes, &lane_events, cycles_per_ns, overhead_cycles);

    // 7. STEADY + PREFETCH (lookahead = 1).
    //    Same input as STEADY; harness issues `prefetch_event` on
    //    events[i+1] before timing event i. Tests whether bringing
    //    the next slab slot to L1 ahead of the lookup recovers the
    //    interference tax on TRADE.
    let mut book_pf = HotOrderBook::<256>::new(1);
    for se in &warmup {
        book_pf.apply(&se.event);
    }
    let mut steady_pf = PerType::new();
    let pf_wall_ns = measure_steady_prefetch(
        &mut book_pf,
        &steady_events,
        &mut steady_pf,
        cycles_per_ns,
        overhead_cycles,
    );

    println!("PER-EVENT-TYPE LATENCY DISTRIBUTION");
    println!("-----------------------------------");
    report_phase("STEADY", &steady);
    println!();
    report_phase("BURST", &burst);
    println!();
    report_phase("CANCEL-HEAVY", &cancel);
    println!();
    report_phase("TRADE-ISOLATED", &trade_iso);
    println!();

    println!("TAIL/MEDIAN RATIOS (lower = more stable, TOTAL aggregate)");
    println!("---------------------------------------------------------");
    for (name, p) in [("STEADY", &steady), ("BURST", &burst), ("CANCEL-HEAVY", &cancel)] {
        let h = &p.total;
        let p50 = h.percentile(50.0).max(1);
        let p99 = h.percentile(99.0);
        let p999 = h.percentile(99.9);
        println!(
            "  {:<14} p99/p50 = {:>5.1}x   p99.9/p50 = {:>6.1}x",
            name,
            p99 as f64 / p50 as f64,
            p999 as f64 / p50 as f64
        );
    }

    // Who drives the tail in STEADY? Print p99 contribution by type.
    println!();
    println!("STEADY p99 ATTRIBUTION (which event-type owns the tail?)");
    println!("--------------------------------------------------------");
    let total_p99 = steady.total.percentile(99.0);
    println!("  reference: TOTAL p99 = {} ns", total_p99);
    for (name, h) in [
        ("ADD   ", &steady.add),
        ("CANCEL", &steady.cancel),
        ("TRADE ", &steady.trade),
        ("MODIFY", &steady.modify),
    ] {
        if h.n == 0 {
            continue;
        }
        let p99 = h.percentile(99.0);
        let ratio = p99 as f64 / total_p99.max(1) as f64;
        let bar_len = ((ratio * 40.0).round() as usize).min(80);
        let bar: String = "█".repeat(bar_len);
        println!(
            "  {} p99 = {:>5} ns ({:>5.2}x of TOTAL)  {}",
            name, p99, ratio, bar
        );
    }

    // INTERFERENCE: TRADE in isolation vs TRADE inside the STEADY mix.
    // The delta is the cost paid purely for sharing cache with ADD/CANCEL.
    println!();
    println!("INTERFERENCE: TRADE intrinsic vs TRADE under mix pressure");
    println!("---------------------------------------------------------");
    let iso = &trade_iso.trade;
    let mix = &steady.trade;
    if iso.n > 0 && mix.n > 0 {
        let iso_p50 = iso.percentile(50.0).max(1);
        let iso_p99 = iso.percentile(99.0).max(1);
        let iso_p999 = iso.percentile(99.9).max(1);
        let mix_p50 = mix.percentile(50.0);
        let mix_p99 = mix.percentile(99.0);
        let mix_p999 = mix.percentile(99.9);
        println!(
            "  isolated TRADE   : p50={:>4} ns  p99={:>4} ns  p99.9={:>4} ns  (n={})",
            iso_p50, iso_p99, iso_p999, iso.n
        );
        println!(
            "  TRADE in STEADY  : p50={:>4} ns  p99={:>4} ns  p99.9={:>4} ns  (n={})",
            mix_p50, mix_p99, mix_p999, mix.n
        );
        println!(
            "  interference tax : p50 x{:.2}   p99 x{:.2}   p99.9 x{:.2}",
            mix_p50 as f64 / iso_p50 as f64,
            mix_p99 as f64 / iso_p99 as f64,
            mix_p999 as f64 / iso_p999 as f64,
        );
        println!();
        println!(
            "  intrinsic share of mix p99 : {:>5.1}%   (closer to 100% = compute bound,\n                                                        closer to 0%   = cache bound)",
            (iso_p99 as f64 / mix_p99.max(1) as f64) * 100.0
        );
    }

    // LANE EXPERIMENT — same input, lane-separated execution.
    // Compared via TOTAL WALL TIME because the two methods use
    // different timing units (per-event vs per-pass) and only the
    // wall sum is mathematically comparable.
    println!();
    println!("LANE-SEPARATED EXECUTION (apply_lane_{{adds,trades,cancels}}, 64-ev window)");
    println!("---------------------------------------------------------------------------");
    println!(
        "  events processed     : {} ADD + {} TRADE + {} CANCEL = {}",
        lanes.n_add,
        lanes.n_trade,
        lanes.n_cancel,
        lanes.total_events()
    );
    println!(
        "  per-lane mean        : ADD = {:>4} ns   TRADE = {:>4} ns   CANCEL = {:>4} ns",
        lanes.add_per_event_ns(),
        lanes.trade_per_event_ns(),
        lanes.cancel_per_event_ns()
    );
    println!();
    println!("WALL-TIME COMPARISON (same input: 500 000 STEADY events)");
    println!("--------------------------------------------------------");
    let lanes_wall = lanes.total_wall_ns();
    println!(
        "  serial apply (interleaved) : {:>10} ns total  ({} ns / event)",
        steady_wall_ns,
        steady_wall_ns / steady.total.n.max(1)
    );
    println!(
        "  apply_lanes (3-pass)       : {:>10} ns total  ({} ns / event)",
        lanes_wall,
        lanes_wall / lanes.total_events().max(1)
    );
    if steady_wall_ns > 0 {
        let delta = (lanes_wall as f64 - steady_wall_ns as f64) / steady_wall_ns as f64 * 100.0;
        let verdict = if delta < -2.0 {
            "LANES WIN"
        } else if delta > 2.0 {
            "LANES LOSE"
        } else {
            "LANES TIE"
        };
        println!("  delta                      : {:+.1}%   [{}]", delta, verdict);
    }
    println!();
    println!("PER-LANE vs PER-TYPE-IN-MIX (mean ns per event)");
    println!("-----------------------------------------------");
    println!(
        "  ADD    : in mix p50 = {:>4} ns  |  in lane = {:>4} ns",
        steady.add.percentile(50.0),
        lanes.add_per_event_ns()
    );
    println!(
        "  TRADE  : in mix p50 = {:>4} ns  |  in lane = {:>4} ns",
        steady.trade.percentile(50.0),
        lanes.trade_per_event_ns()
    );
    println!(
        "  CANCEL : in mix p50 = {:>4} ns  |  in lane = {:>4} ns",
        steady.cancel.percentile(50.0),
        lanes.cancel_per_event_ns()
    );

    // α experiment — prefetch lookahead on the slab slot.
    println!();
    println!("ALPHA: PREFETCH LOOKAHEAD (book.prefetch_event(events[i+1]))");
    println!("------------------------------------------------------------");
    report_phase("STEADY+PF", &steady_pf);
    println!();
    println!("DELTA vs STEADY baseline (per-event apply-only)");
    println!("-----------------------------------------------");
    let pct = |before: u64, after: u64| -> String {
        if before == 0 {
            "—".into()
        } else {
            format!("{:+.1}%", (after as f64 - before as f64) / before as f64 * 100.0)
        }
    };
    let row = |name: &str, h0: &Histogram, h1: &Histogram| {
        println!(
            "  {:<8}  p50: {:>4}->{:>4} ns ({:<7})  p99: {:>4}->{:>4} ns ({:<7})  p99.9: {:>4}->{:>4} ns ({:<7})",
            name,
            h0.percentile(50.0),
            h1.percentile(50.0),
            pct(h0.percentile(50.0), h1.percentile(50.0)),
            h0.percentile(99.0),
            h1.percentile(99.0),
            pct(h0.percentile(99.0), h1.percentile(99.0)),
            h0.percentile(99.9),
            h1.percentile(99.9),
            pct(h0.percentile(99.9), h1.percentile(99.9)),
        );
    };
    row("TOTAL", &steady.total, &steady_pf.total);
    row("ADD", &steady.add, &steady_pf.add);
    row("CANCEL", &steady.cancel, &steady_pf.cancel);
    row("TRADE", &steady.trade, &steady_pf.trade);
    println!();
    println!(
        "  wall apply-only : baseline {} ns  |  prefetch {} ns  ({})",
        steady_wall_ns,
        pf_wall_ns,
        pct(steady_wall_ns, pf_wall_ns),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binning_is_monotonic() {
        let mut last = 0u64;
        for i in 0..N_BINS {
            let lo = Histogram::lower_ns(i);
            assert!(lo >= last, "bin {i} lower {lo} < prev {last}");
            last = lo;
        }
    }

    #[test]
    fn percentiles_are_ordered() {
        let mut h = Histogram::new();
        for i in 0..1000 {
            h.record(10 + i);
        }
        assert!(h.percentile(50.0) <= h.percentile(95.0));
        assert!(h.percentile(95.0) <= h.percentile(99.0));
        assert!(h.percentile(99.0) <= h.percentile(99.9));
    }
}
