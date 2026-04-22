//! Sub-nanosecond hot-path clock based on `rdtsc`.
//!
//! `Instant::now()` on Windows resolves to QueryPerformanceCounter,
//! which has a tick of ~100-200 ns on most boxes. That floor masks
//! the real cost of `OrderBook::apply` (sub-100 ns) and makes the
//! latency panel look "frozen" at exactly the QPC tick.
//!
//! `rdtsc` reads the CPU time-stamp counter — invariant on every
//! modern x86_64 part since Nehalem — at ~25-cycle overhead.

use std::time::Instant;

#[cfg(target_arch = "x86_64")]
#[inline(always)]
pub fn rdtsc() -> u64 {
    // SAFETY: `_rdtsc` is unconditionally available on x86_64.
    unsafe { core::arch::x86_64::_rdtsc() }
}

#[cfg(not(target_arch = "x86_64"))]
#[inline(always)]
pub fn rdtsc() -> u64 {
    use std::sync::OnceLock;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

/// Calibrated TSC -> nanoseconds converter.
#[derive(Clone, Copy)]
pub struct TscClock {
    /// Cycles per nanosecond, sampled from a 5x100 ms median window.
    cycles_per_ns: f64,
    /// Median back-to-back rdtsc pair overhead, in cycles. Subtracted
    /// from every measurement so the clock itself doesn't show up in
    /// the histogram.
    overhead_cycles: u64,
}

impl TscClock {
    pub fn calibrate() -> Self {
        let mut ratios: [f64; 5] = [0.0; 5];
        for slot in ratios.iter_mut() {
            let t0 = Instant::now();
            let c0 = rdtsc();
            while t0.elapsed().as_millis() < 100 {
                std::hint::spin_loop();
            }
            let c1 = rdtsc();
            let elapsed_ns = t0.elapsed().as_nanos() as f64;
            *slot = (c1.wrapping_sub(c0)) as f64 / elapsed_ns.max(1.0);
        }
        let mut sorted = ratios;
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let cycles_per_ns = sorted[2].max(0.1);

        const N: usize = 50_000;
        let mut samples: Vec<u64> = Vec::with_capacity(N);
        for _ in 0..N {
            let a = rdtsc();
            let b = rdtsc();
            samples.push(b.wrapping_sub(a));
        }
        samples.sort_unstable();
        let overhead_cycles = samples[N / 2];

        Self { cycles_per_ns, overhead_cycles }
    }

    /// Convert a raw cycle delta into nanoseconds, subtracting the
    /// rdtsc-pair overhead. Saturates at zero.
    #[inline(always)]
    pub fn delta_ns(&self, start: u64, end: u64) -> u64 {
        let raw = end.wrapping_sub(start);
        let net = raw.saturating_sub(self.overhead_cycles);
        (net as f64 / self.cycles_per_ns) as u64
    }

    pub fn cycles_per_ns(&self) -> f64 {
        self.cycles_per_ns
    }

    pub fn overhead_cycles(&self) -> u64 {
        self.overhead_cycles
    }
}
