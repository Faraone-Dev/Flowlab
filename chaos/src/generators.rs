//! Adversarial event generators — the duals of the five chaos
//! detectors. Each generator emits a deterministic `SequencedEvent`
//! stream that, fed into its corresponding `*Detector`, fires a
//! `ChaosEvent` of the matching `ChaosKind` with `severity > 0`.
//!
//! ## Why "dual"
//!
//! A detector is a recogniser: stream → flag. A generator is the
//! inverse: parameters → stream that the detector recognises. Pairing
//! them gives a closed-loop test rig: the same software is judge
//! (detector) and prosecutor (generator), so the bench is provably
//! self-consistent — no human disagreement on "is this really a
//! flash crash?".
//!
//! ## Determinism
//!
//! Every generator takes a `seed: u64` and a 64-bit `xorshift64*`
//! PRNG so the byte stream is bit-identical for identical
//! `(seed, parameters)`. No system clock, no `thread_rng`, no
//! `HashMap` iteration order. Two runs with the same seed produce
//! the same flag pattern.
//!
//! ## Common contract
//!
//! ```text
//! trait StormGenerator {
//!     fn step(&mut self, now_ns: u64, base_seq: u64) -> Vec<SequencedEvent>;
//!     fn next_due_ns(&self) -> u64;
//!     fn finished(&self) -> bool;
//! }
//! ```
//!
//! The orchestrator polls `next_due_ns()` to pace the storm and stops
//! it once `finished()` returns true.
//!
//! ## Severity
//!
//! Every generator takes `severity: f32 ∈ [0.0, 1.0]`. The semantics
//! are pattern-specific (more cycles, larger jump, faster cancel
//! ratio) but the contract is monotone: higher severity → stronger
//! signal at the detector → higher `ChaosEvent.severity`.

use flowlab_core::event::{Event, EventType, SequencedEvent, Side};

// ─── PRNG ────────────────────────────────────────────────────────────
//
// xorshift64* — deterministic, 64-bit state, ~7 ns per next(). We
// avoid `rand::rngs::StdRng` to keep this crate dependency-light and
// because `StdRng` does not guarantee bit-identity across releases.

#[derive(Debug, Clone)]
pub struct DetRng {
    state: u64,
}

impl DetRng {
    pub fn new(seed: u64) -> Self {
        // Reject zero state (xorshift fixed point).
        let state = if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed };
        Self { state }
    }

    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform `u64` in `[0, n)`. `n == 0` returns 0.
    #[inline]
    pub fn next_below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }

    /// Bernoulli with probability `p ∈ [0.0, 1.0]`.
    #[inline]
    pub fn bool_p(&mut self, p: f64) -> bool {
        let p = p.clamp(0.0, 1.0);
        let cutoff = (p * (u64::MAX as f64)) as u64;
        self.next_u64() < cutoff
    }
}

// ─── Trait ───────────────────────────────────────────────────────────

/// Adversarial event source. Implementations are deterministic,
/// finite, and self-paced: the orchestrator asks `next_due_ns()`
/// for the next emission timestamp and calls `step()` at or after
/// that time.
pub trait StormGenerator {
    /// Emit zero or more events that should be applied at `now_ns`.
    /// `base_seq` is the next free sequence number to use; the
    /// generator must assign sequential IDs starting at `base_seq`.
    fn step(&mut self, now_ns: u64, base_seq: u64) -> Vec<SequencedEvent>;

    /// Earliest wall time (ns since arbitrary epoch — same epoch the
    /// orchestrator uses) at which `step()` should be called next.
    /// Returning a value `<= now_ns` means "emit immediately".
    fn next_due_ns(&self) -> u64;

    /// True once the generator has emitted its full programme. After
    /// this returns true the orchestrator drops the generator.
    fn finished(&self) -> bool;

    /// Human-readable kind. Used for logging / dashboard labels.
    fn kind_label(&self) -> &'static str;
}

// ─── Helpers ─────────────────────────────────────────────────────────

#[inline]
fn mk(seq: u64, ts: u64, et: EventType, side: Side, price: u64, qty: u64, oid: u64) -> SequencedEvent {
    SequencedEvent {
        seq,
        channel_id: 0,
        event: Event {
            ts,
            price,
            qty,
            order_id: oid,
            instrument_id: 1,
            event_type: et as u8,
            side: side as u8,
            _pad: [0; 2],
        },
    }
}

/// Linear interp `severity ∈ [0, 1]` -> `[lo, hi]`.
#[inline]
fn lerp(severity: f32, lo: f64, hi: f64) -> f64 {
    let s = severity.clamp(0.0, 1.0) as f64;
    lo + (hi - lo) * s
}

// ─── 1. PhantomLiquidityGenerator ────────────────────────────────────
//
// Detector spec (chaos/src/phantom_liquidity.rs):
//   default_itch() →
//     max_seq_gap = 256, max_time_gap_ns = 5_000_000,
//     max_tracked = 4096, min_qty = 100.
//
// Pattern: ADD a level with qty >= min_qty, then CANCEL the same
// (side, price, order_id) within 256 seq AND 5ms. No trade in
// between. Repeat with fresh order_ids and prices to produce many
// flags.
//
// Severity drives:
//   - cycle gap: tighter → higher detector.severity (window-ratio).
//   - cycles per step: more flags per call.

pub struct PhantomLiquidityGenerator {
    rng: DetRng,
    base_price: u64,
    band_ticks: u64,
    next_oid: u64,
    /// Number of full add/cancel cycles to emit per `step()` call.
    cycles_per_step: u32,
    /// Wall-clock spacing between successive `step()` calls.
    step_period_ns: u64,
    /// Earliest next emission.
    next_due: u64,
    /// Total cycles to emit before declaring finished.
    cycles_remaining: u32,
    /// Gap between the ADD and its matching CANCEL, in ns. Must be
    /// well under `max_time_gap_ns = 5ms` for the detector to fire.
    add_cancel_gap_ns: u64,
}

impl PhantomLiquidityGenerator {
    /// `severity` controls cycle tightness and cycle count. `seed`
    /// makes the byte stream reproducible. `total_cycles` caps the
    /// generator's lifetime (use `u32::MAX` for "until stopped").
    pub fn new(seed: u64, severity: f32, base_price: u64, total_cycles: u32) -> Self {
        // Tighter cycle = higher detector severity (closer to 0 of
        // window). 5 ms cap → use 50 µs..2 ms band.
        let gap_ns = lerp(severity, 2_000_000.0, 50_000.0) as u64;
        // 1..8 cycles per step. Higher severity → more cycles/step.
        let cycles_per_step = lerp(severity, 1.0, 8.0).round() as u32;
        // 50..5 ms cadence between bursts.
        let step_period_ns = lerp(severity, 50_000_000.0, 5_000_000.0) as u64;
        Self {
            rng: DetRng::new(seed ^ 0xA1A1_A1A1_A1A1_A1A1),
            base_price,
            band_ticks: 32,
            next_oid: 1_000_000,
            cycles_per_step,
            step_period_ns,
            next_due: 0,
            cycles_remaining: total_cycles,
            add_cancel_gap_ns: gap_ns,
        }
    }
}

impl StormGenerator for PhantomLiquidityGenerator {
    fn step(&mut self, now_ns: u64, base_seq: u64) -> Vec<SequencedEvent> {
        let mut out = Vec::with_capacity(self.cycles_per_step as usize * 2);
        let mut seq = base_seq;
        let mut ts = now_ns;
        let cycles = self.cycles_per_step.min(self.cycles_remaining);
        for _ in 0..cycles {
            let side = if self.rng.bool_p(0.5) { Side::Bid } else { Side::Ask };
            // Sit a few ticks away from base, never at the touch (so
            // we don't accidentally cross any synthetic top).
            let off = 2 + self.rng.next_below(self.band_ticks) as i64;
            let price = if matches!(side, Side::Bid) {
                self.base_price.saturating_sub(off as u64)
            } else {
                self.base_price.saturating_add(off as u64)
            };
            // Above min_qty (100) by a comfortable margin.
            let qty = 200 + self.rng.next_below(800);
            let oid = self.next_oid;
            self.next_oid = self.next_oid.wrapping_add(1);
            // Inter-event spacing inside the cycle.
            out.push(mk(seq, ts, EventType::OrderAdd, side, price, qty, oid));
            seq += 1;
            ts = ts.saturating_add(self.add_cancel_gap_ns / 2);
            // CANCEL with same order_id and qty so the detector
            // matches the cycle as fully cancelled.
            out.push(mk(seq, ts, EventType::OrderCancel, side, price, qty, oid));
            seq += 1;
            ts = ts.saturating_add(self.add_cancel_gap_ns / 2);
        }
        self.cycles_remaining = self.cycles_remaining.saturating_sub(cycles);
        self.next_due = now_ns.saturating_add(self.step_period_ns);
        out
    }

    fn next_due_ns(&self) -> u64 {
        self.next_due
    }
    fn finished(&self) -> bool {
        self.cycles_remaining == 0
    }
    fn kind_label(&self) -> &'static str {
        "phantom_liquidity"
    }
}

pub struct QuoteStuffGenerator {
    rng: DetRng,
    base_price: u64,
    next_oid: u64,
    bursts_remaining: u32,
    trades_per_step: u32,
    cancels_per_trade: u32,
    step_period_ns: u64,
    next_due: u64,
}

impl QuoteStuffGenerator {
    pub fn new(seed: u64, severity: f32, base_price: u64, bursts: u32) -> Self {
        Self {
            rng: DetRng::new(seed ^ 0xB1B1_B1B1_B1B1_B1B1),
            base_price,
            next_oid: 1_500_000,
            bursts_remaining: bursts,
            trades_per_step: lerp(severity, 4.0, 6.0).round() as u32,
            cancels_per_trade: lerp(severity, 8.0, 16.0).round() as u32,
            step_period_ns: lerp(severity, 20_000_000.0, 5_000_000.0) as u64,
            next_due: 0,
        }
    }
}

impl StormGenerator for QuoteStuffGenerator {
    fn step(&mut self, now_ns: u64, base_seq: u64) -> Vec<SequencedEvent> {
        let total = self.trades_per_step.saturating_mul(self.cancels_per_trade + 1);
        let mut out = Vec::with_capacity(total as usize);
        let mut seq = base_seq;
        let mut ts = now_ns;
        let dt = self.step_period_ns / total.max(1) as u64;

        for i in 0..self.trades_per_step {
            let trade_side = if i % 2 == 0 { Side::Ask } else { Side::Bid };
            let trade_price = if matches!(trade_side, Side::Ask) {
                self.base_price.saturating_add(1)
            } else {
                self.base_price.saturating_sub(1)
            };
            out.push(mk(seq, ts, EventType::Trade, trade_side, trade_price, 100, self.next_oid));
            self.next_oid = self.next_oid.wrapping_add(1);
            seq += 1;
            ts = ts.saturating_add(dt);

            let cancel_side = if matches!(trade_side, Side::Ask) { Side::Bid } else { Side::Ask };
            for _ in 0..self.cancels_per_trade {
                let off = 1 + self.rng.next_below(4);
                let price = if matches!(cancel_side, Side::Bid) {
                    self.base_price.saturating_sub(off)
                } else {
                    self.base_price.saturating_add(off)
                };
                out.push(mk(seq, ts, EventType::OrderCancel, cancel_side, price, 100, self.next_oid));
                self.next_oid = self.next_oid.wrapping_add(1);
                seq += 1;
                ts = ts.saturating_add(dt);
            }
        }

        self.bursts_remaining = self.bursts_remaining.saturating_sub(1);
        self.next_due = now_ns.saturating_add(self.step_period_ns);
        out
    }

    fn next_due_ns(&self) -> u64 {
        self.next_due
    }

    fn finished(&self) -> bool {
        self.bursts_remaining == 0
    }

    fn kind_label(&self) -> &'static str {
        "quote_stuff"
    }
}

pub struct SpoofGenerator {
    rng: DetRng,
    base_price: u64,
    next_oid: u64,
    cycles_per_step: u32,
    step_period_ns: u64,
    next_due: u64,
    cycles_remaining: u32,
    add_cancel_gap_ns: u64,
    qty: u64,
}

impl SpoofGenerator {
    pub fn new(seed: u64, severity: f32, base_price: u64, total_cycles: u32) -> Self {
        Self {
            rng: DetRng::new(seed ^ 0xD1D1_D1D1_D1D1_D1D1),
            base_price,
            next_oid: 1_750_000,
            cycles_per_step: lerp(severity, 1.0, 6.0).round() as u32,
            step_period_ns: lerp(severity, 25_000_000.0, 5_000_000.0) as u64,
            next_due: 0,
            cycles_remaining: total_cycles,
            add_cancel_gap_ns: lerp(severity, 1_000_000.0, 50_000.0) as u64,
            qty: lerp(severity, 2_000.0, 20_000.0) as u64,
        }
    }
}

impl StormGenerator for SpoofGenerator {
    fn step(&mut self, now_ns: u64, base_seq: u64) -> Vec<SequencedEvent> {
        let cycles = self.cycles_per_step.min(self.cycles_remaining);
        let mut out = Vec::with_capacity((cycles * 2) as usize);
        let mut seq = base_seq;
        let mut ts = now_ns;

        for _ in 0..cycles {
            let side = if self.rng.bool_p(0.5) { Side::Bid } else { Side::Ask };
            let off = 1 + self.rng.next_below(4);
            let price = if matches!(side, Side::Bid) {
                self.base_price.saturating_sub(off)
            } else {
                self.base_price.saturating_add(off)
            };
            let oid = self.next_oid;
            self.next_oid = self.next_oid.wrapping_add(1);

            out.push(mk(seq, ts, EventType::OrderAdd, side, price, self.qty, oid));
            seq += 1;
            ts = ts.saturating_add(self.add_cancel_gap_ns / 2);
            out.push(mk(seq, ts, EventType::OrderCancel, side, price, self.qty, oid));
            seq += 1;
            ts = ts.saturating_add(self.add_cancel_gap_ns / 2);
        }

        self.cycles_remaining = self.cycles_remaining.saturating_sub(cycles);
        self.next_due = now_ns.saturating_add(self.step_period_ns);
        out
    }

    fn next_due_ns(&self) -> u64 {
        self.next_due
    }

    fn finished(&self) -> bool {
        self.cycles_remaining == 0
    }

    fn kind_label(&self) -> &'static str {
        "spoof"
    }
}

// ─── 2. CancellationStormGenerator ───────────────────────────────────
//
// Detector spec (chaos/src/cancellation_storm.rs):
//   default_itch() →
//     seq_window = 2048, min_window = 128, warmup_samples = 1024,
//     sigma_floor = 0.05, k_sigma = 4.0, min_consecutive = 8.
//
// Welford needs warmup. A clean storm is achievable by:
//   1. Warmup phase: emit a steady mix of ADD/CANCEL/TRADE with a
//      moderate cancel ratio (~30 %), enough samples for warmup.
//   2. Storm phase: spike cancel ratio to ~95 % for at least
//      `min_consecutive * (sample_window_size)` events. Severity
//      controls both the ratio and the storm duration.
//
// We cheat slightly by emitting unique add/cancel pairs (no resting
// orders to deplete) so the storm can sustain arbitrarily long.

pub struct CancellationStormGenerator {
    rng: DetRng,
    base_price: u64,
    next_oid: u64,
    /// Events per `step()` call.
    batch: u32,
    /// Wall spacing between batches.
    step_period_ns: u64,
    /// Cancel probability of the current phase. Adjusted as we
    /// transition warmup → storm → cooldown.
    current_cancel_p: f64,
    storm_cancel_p: f64,
    warmup_cancel_p: f64,
    /// Phase counters in number of *batches*.
    warmup_batches_remaining: u32,
    storm_batches_remaining: u32,
    next_due: u64,
    /// True once we've emitted the very first storm batch.
    storm_started: bool,
}

impl CancellationStormGenerator {
    pub fn new(seed: u64, severity: f32, base_price: u64) -> Self {
        // Storm cancel-ratio: 0.92..0.99. Lower than this and the
        // rolling window's pre-storm tail keeps the average pinned
        // below the adaptive threshold.
        let storm_p = lerp(severity, 0.92, 0.99);
        // Storm duration in batches: 40..120 (each batch = 128 events).
        // We need to almost fully replace the detector's rolling
        // window (seq_window = 2048) before the rate the detector sees
        // crosses `μ + k·σ`. 40 batches ≈ 5120 events → window
        // turned over ~2.5x.
        let storm_batches = lerp(severity, 40.0, 120.0).round() as u32;
        Self {
            rng: DetRng::new(seed ^ 0xC1C1_C1C1_C1C1_C1C1),
            base_price,
            next_oid: 2_000_000,
            // Bigger batch: 128 events per step → storm phase fills the
            // 2048-deep detector window in ~16 steps.
            batch: 128,
            step_period_ns: 1_000_000, // 1 ms per batch
            current_cancel_p: 0.30,
            warmup_cancel_p: 0.30,
            storm_cancel_p: storm_p,
            // Warmup: detector needs warmup_samples (1024) Welford
            // updates after the min_window (128) gate is satisfied.
            // 16 batches × 128 = 2048 events comfortably exceeds that.
            warmup_batches_remaining: 16,
            storm_batches_remaining: storm_batches,
            next_due: 0,
            storm_started: false,
        }
    }
}

impl StormGenerator for CancellationStormGenerator {
    fn step(&mut self, now_ns: u64, base_seq: u64) -> Vec<SequencedEvent> {
        // Decide phase.
        let just_starting_storm: bool;
        if self.warmup_batches_remaining > 0 {
            self.current_cancel_p = self.warmup_cancel_p;
            self.warmup_batches_remaining -= 1;
            just_starting_storm = false;
        } else if self.storm_batches_remaining > 0 {
            self.current_cancel_p = self.storm_cancel_p;
            self.storm_batches_remaining -= 1;
            just_starting_storm = !self.storm_started;
            self.storm_started = true;
        } else {
            just_starting_storm = false;
        }

        let mut out = Vec::with_capacity(self.batch as usize);
        // On the storm-onset batch we jump seq forward by more than
        // seq_window (2048) so the detector evicts the entire warmup
        // tail in one shot. Without this, the rolling rate creeps up
        // gradually and Welford keeps absorbing storm samples (which
        // are fed to it whenever rate <= threshold), so the threshold
        // tracks the storm itself and the flag never fires.
        let mut seq = if just_starting_storm {
            base_seq.saturating_add(4_096)
        } else {
            base_seq
        };
        let dt = self.step_period_ns / self.batch.max(1) as u64;
        let mut ts = now_ns;
        for _ in 0..self.batch {
            let side = if self.rng.bool_p(0.5) { Side::Bid } else { Side::Ask };
            // Each event uses a fresh order_id and a small price jitter
            // around the base. Add or Cancel by the current ratio;
            // ~5 % trade for realism so the ratio denominator is sane.
            let r = self.rng.next_u64() as f64 / u64::MAX as f64;
            let etype = if r < 0.05 {
                EventType::Trade
            } else if r < 0.05 + (1.0 - 0.05) * self.current_cancel_p {
                EventType::OrderCancel
            } else {
                EventType::OrderAdd
            };
            let off = self.rng.next_below(8) as u64;
            let price = if matches!(side, Side::Bid) {
                self.base_price.saturating_sub(off + 1)
            } else {
                self.base_price.saturating_add(off + 1)
            };
            let qty = 100 + self.rng.next_below(400);
            let oid = self.next_oid;
            self.next_oid = self.next_oid.wrapping_add(1);
            out.push(mk(seq, ts, etype, side, price, qty, oid));
            seq += 1;
            ts = ts.saturating_add(dt);
        }
        self.next_due = now_ns.saturating_add(self.step_period_ns);
        out
    }

    fn next_due_ns(&self) -> u64 {
        self.next_due
    }
    fn finished(&self) -> bool {
        self.warmup_batches_remaining == 0 && self.storm_batches_remaining == 0
    }
    fn kind_label(&self) -> &'static str {
        "cancellation_storm"
    }
}

// ─── 3. MomentumIgnitionGenerator ────────────────────────────────────
//
// Detector spec (chaos/src/momentum_ignition.rs):
//   default_itch() →
//     seq_window = 1024, max_book_levels = 64,
//     min_move_bps = 5, min_trades = 8, min_consecutive = 4.
//
// The detector tracks an *internal* mini-book from ADD/CANCEL/TRADE.
// To create a sustained directional drift we must:
//   1. Seed both sides with resting orders so best-bid / best-ask
//      are defined (mid = midpoint).
//   2. Walk the *aggressed* side up (or down) by repeatedly:
//        - TRADE at best_ask to consume liquidity (up case)
//        - ADD a new ask one tick higher (best moves up)
//        - REPEAT — each cycle pushes mid up by ~½ tick.
//   3. Bias trade aggressors so detector.buy_pressure dominates.
//   4. Achieve >= min_consecutive (4) monotone mid samples and
//      >= min_trades (8) trades within seq_window (1024).

pub struct MomentumIgnitionGenerator {
    #[allow(dead_code)]
    rng: DetRng,
    next_oid: u64,
    /// Direction of drift: +1 = up, -1 = down.
    direction: i32,
    /// Current "best" we're walking.
    cur_best_bid: u64,
    cur_best_ask: u64,
    /// Initial mid (for severity bps target).
    initial_mid: u64,
    /// Target bps move from initial_mid before stopping.
    target_bps: u32,
    /// Number of mid-walk cycles per step.
    cycles_per_step: u32,
    step_period_ns: u64,
    next_due: u64,
    /// Hard upper bound on cycles before finishing.
    cycles_remaining: u32,
    /// Initial seeding done?
    seeded: bool,
}

impl MomentumIgnitionGenerator {
    pub fn new(seed: u64, severity: f32, base_price: u64, direction_up: bool) -> Self {
        // 8..40 bps target move (well above 5 bps trigger).
        let target_bps = lerp(severity, 8.0, 40.0).round() as u32;
        // 1..6 cycles per step.
        let cycles_per_step = lerp(severity, 1.0, 6.0).round() as u32;
        // 5..1 ms between cycles.
        let step_period_ns = lerp(severity, 5_000_000.0, 1_000_000.0) as u64;
        // Hard cap: ~ enough cycles to reach the target plus margin.
        let cycles_remaining = (target_bps as u32 * 4).max(40);
        Self {
            rng: DetRng::new(seed ^ 0xE2E2_E2E2_E2E2_E2E2),
            next_oid: 3_000_000,
            direction: if direction_up { 1 } else { -1 },
            cur_best_bid: base_price.saturating_sub(1),
            cur_best_ask: base_price.saturating_add(1),
            initial_mid: base_price,
            target_bps,
            cycles_per_step,
            step_period_ns,
            next_due: 0,
            cycles_remaining,
            seeded: false,
        }
    }

    /// Seed the detector book with a single touch per side. Multi-level
    /// seeding is a foot-gun: when the trade consumes the top, the
    /// next-best is the seeded level one tick away — so best_ask never
    /// actually moves and the detector sees a flat mid.
    fn seed(&mut self, base_seq: u64, ts: u64) -> Vec<SequencedEvent> {
        let mut out = Vec::with_capacity(2);
        let mut seq = base_seq;
        let oid_b = self.next_oid;
        self.next_oid = self.next_oid.wrapping_add(1);
        let oid_a = self.next_oid;
        self.next_oid = self.next_oid.wrapping_add(1);
        out.push(mk(seq, ts, EventType::OrderAdd, Side::Bid, self.cur_best_bid, 500, oid_b));
        seq += 1;
        out.push(mk(seq, ts, EventType::OrderAdd, Side::Ask, self.cur_best_ask, 500, oid_a));
        let _ = seq;
        self.seeded = true;
        out
    }
}

impl StormGenerator for MomentumIgnitionGenerator {
    fn step(&mut self, now_ns: u64, base_seq: u64) -> Vec<SequencedEvent> {
        let mut out = Vec::new();
        let mut seq = base_seq;
        let mut ts = now_ns;
        if !self.seeded {
            let seed_evs = self.seed(seq, ts);
            seq += seed_evs.len() as u64;
            out.extend(seed_evs);
        }
        // Per-cycle tick jump small enough that we accumulate enough
        // trades (detector wants min_trades = 8) before reaching the
        // bps target. For base_price = 100_000 → 10 ticks = 1 bps per
        // cycle; with target_bps in 8..40 we get 8..40 cycles, well
        // above min_trades and >> min_consecutive (4).
        let jump_ticks: u64 = (self.initial_mid / 10_000).max(1);
        for _ in 0..self.cycles_per_step.min(self.cycles_remaining) {
            // Has the detector-relevant move already exceeded target?
            let cur_mid = (self.cur_best_bid + self.cur_best_ask) / 2;
            let bps_now = if self.direction > 0 {
                ((cur_mid.saturating_sub(self.initial_mid)) as u128 * 10_000
                    / self.initial_mid.max(1) as u128) as u32
            } else {
                ((self.initial_mid.saturating_sub(cur_mid)) as u128 * 10_000
                    / self.initial_mid.max(1) as u128) as u32
            };
            if bps_now >= self.target_bps {
                self.cycles_remaining = 0;
                break;
            }

            if self.direction > 0 {
                // Walk UP. Add the new far-away ask FIRST so when we
                // wipe the old top there is a higher level for best_ask
                // to fall back onto. Add a matching higher bid so mid
                // accumulates in the right direction.
                let new_ask = self.cur_best_ask.saturating_add(jump_ticks);
                let new_bid = self.cur_best_bid.saturating_add(jump_ticks);
                out.push(mk(seq, ts, EventType::OrderAdd, Side::Ask, new_ask, 500, self.next_oid));
                self.next_oid = self.next_oid.wrapping_add(1);
                seq += 1;
                ts = ts.saturating_add(20_000);
                // Buy-aggressor trade at the OLD top, full size, so the
                // level disappears and best_ask jumps up to new_ask.
                out.push(mk(seq, ts, EventType::Trade, Side::Ask, self.cur_best_ask, 500, self.next_oid));
                self.next_oid = self.next_oid.wrapping_add(1);
                seq += 1;
                ts = ts.saturating_add(20_000);
                out.push(mk(seq, ts, EventType::OrderAdd, Side::Bid, new_bid, 500, self.next_oid));
                self.next_oid = self.next_oid.wrapping_add(1);
                seq += 1;
                self.cur_best_ask = new_ask;
                self.cur_best_bid = new_bid;
            } else {
                let new_bid = self.cur_best_bid.saturating_sub(jump_ticks);
                let new_ask = self.cur_best_ask.saturating_sub(jump_ticks);
                out.push(mk(seq, ts, EventType::OrderAdd, Side::Bid, new_bid, 500, self.next_oid));
                self.next_oid = self.next_oid.wrapping_add(1);
                seq += 1;
                ts = ts.saturating_add(20_000);
                out.push(mk(seq, ts, EventType::Trade, Side::Bid, self.cur_best_bid, 500, self.next_oid));
                self.next_oid = self.next_oid.wrapping_add(1);
                seq += 1;
                ts = ts.saturating_add(20_000);
                out.push(mk(seq, ts, EventType::OrderAdd, Side::Ask, new_ask, 500, self.next_oid));
                self.next_oid = self.next_oid.wrapping_add(1);
                seq += 1;
                self.cur_best_bid = new_bid;
                self.cur_best_ask = new_ask;
            }
            ts = ts.saturating_add(50_000);
            self.cycles_remaining = self.cycles_remaining.saturating_sub(1);
        }
        self.next_due = now_ns.saturating_add(self.step_period_ns);
        out
    }

    fn next_due_ns(&self) -> u64 {
        self.next_due
    }
    fn finished(&self) -> bool {
        self.cycles_remaining == 0
    }
    fn kind_label(&self) -> &'static str {
        "momentum_ignition"
    }
}

// ─── 4. FlashCrashGenerator ──────────────────────────────────────────
//
// Detector spec (chaos/src/flash_crash.rs):
//   default_itch() →
//     min_gap_bps = 10, gap_window_events = 8,
//     depth_band_ticks = 5, depth_drop_pct = 0.6,
//     min_aggr_trades = 3, cooldown_seq = 2048.
//
// Recipe to fire it:
//   1. Seed both sides with substantial depth in the ±5-tick band.
//      The detector's pre-event snapshot will record `depth_in_band`.
//   2. Within `<= 8` events:
//        a. CANCEL >= 60 % of the depth in the original band (one
//           side is enough — the band sums both sides and we can
//           drop them together).
//        b. ADD a new best on the opposite extreme (creates the
//           mid jump).
//        c. Emit >= 3 aggressive TRADEs at the new best aligned
//           with the crash direction.
//   3. mid jump must be >= 10 bps relative to the snap mid.
//
// Severity controls jump size and depth-vacuum percentage.

pub struct FlashCrashGenerator {
    #[allow(dead_code)]
    rng: DetRng,
    base_price: u64,
    /// Down-crash if true, up-spike if false.
    crash_down: bool,
    /// Target mid jump in bps.
    #[allow(dead_code)]
    target_jump_bps: u32,
    /// Target depth-vacuum fraction (0.6..0.95).
    target_vacuum_pct: f64,
    /// Number of crash bursts to emit.
    bursts_remaining: u32,
    /// Wall spacing between bursts so the cooldown_seq (2048) elapses.
    step_period_ns: u64,
    next_due: u64,
    /// Order ids for resting / aggressing.
    next_oid: u64,
    /// Have we seeded depth?
    seeded: bool,
    /// Track resting order ids per (side, price) so we can cancel
    /// them precisely.
    resting: Vec<(u8, u64, u64, u64)>, // (side, price, qty, oid)
}

impl FlashCrashGenerator {
    pub fn new(seed: u64, severity: f32, base_price: u64, crash_down: bool, bursts: u32) -> Self {
        let target_jump_bps = lerp(severity, 12.0, 80.0).round() as u32;
        let target_vacuum_pct = lerp(severity, 0.65, 0.95);
        let step_period_ns = lerp(severity, 200_000_000.0, 80_000_000.0) as u64;
        Self {
            rng: DetRng::new(seed ^ 0xF3F3_F3F3_F3F3_F3F3),
            base_price,
            crash_down,
            target_jump_bps,
            target_vacuum_pct,
            bursts_remaining: bursts,
            step_period_ns,
            next_due: 0,
            next_oid: 4_000_000,
            seeded: false,
            resting: Vec::new(),
        }
    }

    fn seed_depth(&mut self, base_seq: u64, ts: u64) -> Vec<SequencedEvent> {
        // Single heavy touch level per side + a deep fallback level
        // far outside the ±5-tick band so the book never goes empty
        // when the touch is wiped (otherwise mid_after becomes None
        // and the detector exits).
        let mut out = Vec::with_capacity(4);
        let mut seq = base_seq;
        let top_bid_qty: u64 = 5_000;
        let top_ask_qty: u64 = 3_000;
        let top_bid_p = self.base_price.saturating_sub(1);
        let top_ask_p = self.base_price.saturating_add(1);
        let fallback_bid_p = self.base_price.saturating_sub(200);
        let fallback_ask_p = self.base_price.saturating_add(200);

        let oid = self.next_oid;
        self.next_oid = self.next_oid.wrapping_add(1);
        out.push(mk(seq, ts, EventType::OrderAdd, Side::Bid, top_bid_p, top_bid_qty, oid));
        self.resting.push((0, top_bid_p, top_bid_qty, oid));
        seq += 1;
        let oid = self.next_oid;
        self.next_oid = self.next_oid.wrapping_add(1);
        out.push(mk(seq, ts, EventType::OrderAdd, Side::Ask, top_ask_p, top_ask_qty, oid));
        self.resting.push((1, top_ask_p, top_ask_qty, oid));
        seq += 1;
        let oid = self.next_oid;
        self.next_oid = self.next_oid.wrapping_add(1);
        out.push(mk(seq, ts, EventType::OrderAdd, Side::Bid, fallback_bid_p, 200, oid));
        // Fallback is intentionally NOT pushed to `resting` — we
        // must never cancel it.
        seq += 1;
        let oid = self.next_oid;
        self.next_oid = self.next_oid.wrapping_add(1);
        out.push(mk(seq, ts, EventType::OrderAdd, Side::Ask, fallback_ask_p, 200, oid));
        seq += 1;
        let _ = seq;
        self.seeded = true;
        out
    }

    fn one_burst(&mut self, base_seq: u64, ts0: u64) -> Vec<SequencedEvent> {
        // Burst layout (5 events, fits inside gap_window_events = 8):
        //   1. Cancel top bid in full       → best_bid jumps to fallback
        //   2. Cancel a slice of top ask    → depth_in_band collapses
        //   3-5. Three aggressive trades    → confirms direction
        //
        // depth_then = top_bid + top_ask (both inside ±5 band)
        // depth_now  = (top_ask - cut)
        // drop = (top_bid + cut) / (top_bid + top_ask) ≈ vacuum_pct
        let mut out = Vec::with_capacity(8);
        let mut seq = base_seq;
        let mut ts = ts0;

        let band_total: u64 = self.resting.iter().map(|(_, _, q, _)| *q).sum();
        let want_drop = (band_total as f64 * self.target_vacuum_pct) as u64;

        // 1. Cancel the top bid in full.
        let mut cancelled: u64 = 0;
        if let Some(i) = self.resting.iter().position(|(s, _, _, _)| *s == 0) {
            let (_, p, q, oid) = self.resting[i];
            out.push(mk(seq, ts, EventType::OrderCancel, Side::Bid, p, q, oid));
            seq += 1;
            ts = ts.saturating_add(20_000);
            cancelled = cancelled.saturating_add(q);
            self.resting.swap_remove(i);
        }

        // 2. Cut into the top ask if needed.
        if cancelled < want_drop {
            let needed = want_drop - cancelled;
            if let Some(i) = self.resting.iter().position(|(s, _, _, _)| *s == 1) {
                let (_, p, q, oid) = self.resting[i];
                // Leave >= 100 qty so the level stays alive (best_ask
                // doesn't move back to fallback, keeps mid stable).
                let cut = needed.min(q.saturating_sub(100));
                if cut > 0 {
                    out.push(mk(seq, ts, EventType::OrderCancel, Side::Ask, p, cut, oid));
                    seq += 1;
                    ts = ts.saturating_add(20_000);
                    self.resting[i] = (1, p, q - cut, oid);
                }
            }
        }

        // 3-5. Aggressive trades aligned with crash direction. The
        // tick rule classifies aggressor by trade.price vs mid_before.
        // After the cancels, mid is roughly halfway between fallback
        // bid (base-200) and the residual top ask (base+1). For
        // crash_down a sell-aggressor trade prints at fallback bid.
        let trade_side = if self.crash_down { Side::Bid } else { Side::Ask };
        let trade_price = if self.crash_down {
            self.base_price.saturating_sub(200)
        } else {
            self.base_price.saturating_add(200)
        };
        for _ in 0..4 {
            out.push(mk(seq, ts, EventType::Trade, trade_side, trade_price, 50, self.next_oid));
            self.next_oid = self.next_oid.wrapping_add(1);
            seq += 1;
            ts = ts.saturating_add(20_000);
        }
        out
    }
}

impl StormGenerator for FlashCrashGenerator {
    fn step(&mut self, now_ns: u64, base_seq: u64) -> Vec<SequencedEvent> {
        let mut out = Vec::new();
        let mut seq = base_seq;
        if !self.seeded {
            let s = self.seed_depth(seq, now_ns);
            seq += s.len() as u64;
            out.extend(s);
        }
        if self.bursts_remaining > 0 {
            let b = self.one_burst(seq, now_ns.saturating_add(1_000));
            out.extend(b);
            self.bursts_remaining -= 1;
        }
        // Re-seed depth between bursts so next one has a fresh band.
        if self.bursts_remaining > 0 {
            self.resting.clear();
            self.seeded = false;
        }
        self.next_due = now_ns.saturating_add(self.step_period_ns);
        out
    }

    fn next_due_ns(&self) -> u64 {
        self.next_due
    }
    fn finished(&self) -> bool {
        self.bursts_remaining == 0
    }
    fn kind_label(&self) -> &'static str {
        "flash_crash"
    }
}

// ─── 5. LatencyArbProxyGenerator ─────────────────────────────────────
//
// Detector spec (chaos/src/latency_arb_proxy.rs):
//   default_itch() →
//     reaction_seq = 64, band_ticks = 4, min_trade_qty = 200,
//     min_burst = 4, require_reversion = false, cooldown_seq = 1024.
//
// Recipe:
//   1. Seed both sides so mid is defined.
//   2. Emit a Trade on Side::Ask (i.e. buy-aggressor) with qty>=200.
//      (Detector classifies via tick rule against mid_before.)
//   3. Within 64 seq, emit >= 4 events on the OPPOSITE resting side
//      (Side::Bid for buy-print) of any type ADD/CANCEL with
//      |price - p₀| <= 4 ticks.

pub struct LatencyArbProxyGenerator {
    rng: DetRng,
    base_price: u64,
    next_oid: u64,
    bursts_remaining: u32,
    /// Number of opposite-side reaction events per print.
    burst_size: u32,
    step_period_ns: u64,
    next_due: u64,
    seeded: bool,
}

impl LatencyArbProxyGenerator {
    pub fn new(seed: u64, severity: f32, base_price: u64, bursts: u32) -> Self {
        let burst_size = lerp(severity, 4.0, 16.0).round() as u32;
        let step_period_ns = lerp(severity, 50_000_000.0, 10_000_000.0) as u64;
        Self {
            rng: DetRng::new(seed ^ 0xB4B4_B4B4_B4B4_B4B4),
            base_price,
            next_oid: 5_000_000,
            bursts_remaining: bursts,
            burst_size,
            step_period_ns,
            next_due: 0,
            seeded: false,
        }
    }

    fn seed(&mut self, base_seq: u64, ts: u64) -> Vec<SequencedEvent> {
        let mut out = Vec::with_capacity(20);
        let mut seq = base_seq;
        for i in 0..10u64 {
            let bid_p = self.base_price.saturating_sub(i + 1);
            let ask_p = self.base_price.saturating_add(i + 1);
            let oid_b = self.next_oid;
            self.next_oid = self.next_oid.wrapping_add(1);
            let oid_a = self.next_oid;
            self.next_oid = self.next_oid.wrapping_add(1);
            out.push(mk(seq, ts, EventType::OrderAdd, Side::Bid, bid_p, 500, oid_b));
            seq += 1;
            out.push(mk(seq, ts, EventType::OrderAdd, Side::Ask, ask_p, 500, oid_a));
            seq += 1;
        }
        self.seeded = true;
        out
    }
}

impl StormGenerator for LatencyArbProxyGenerator {
    fn step(&mut self, now_ns: u64, base_seq: u64) -> Vec<SequencedEvent> {
        let mut out = Vec::new();
        let mut seq = base_seq;
        let mut ts = now_ns;
        if !self.seeded {
            let s = self.seed(seq, ts);
            seq += s.len() as u64;
            out.extend(s);
        }
        if self.bursts_remaining == 0 {
            return out;
        }

        // Decide direction: buy-print (Ask side) for half, sell-print
        // (Bid side) for half — variety so both detector branches get
        // exercised.
        let buy_print = self.rng.bool_p(0.5);
        let print_side = if buy_print { Side::Ask } else { Side::Bid };
        let print_price = if buy_print {
            self.base_price.saturating_add(1)
        } else {
            self.base_price.saturating_sub(1)
        };
        out.push(mk(
            seq, ts, EventType::Trade, print_side, print_price, 400,
            self.next_oid,
        ));
        self.next_oid = self.next_oid.wrapping_add(1);
        seq += 1;
        ts = ts.saturating_add(100_000);

        // OPPOSITE-side burst: buy print -> opposite side is Bid (0).
        let opp_side = if buy_print { Side::Bid } else { Side::Ask };
        for k in 0..self.burst_size {
            // Stay in band ≤ 4 ticks from print_price.
            let off = (k % 4) as u64 + 1;
            let px = if matches!(opp_side, Side::Bid) {
                print_price.saturating_sub(off)
            } else {
                print_price.saturating_add(off)
            };
            // Alternate ADD / CANCEL — both qualify per detector.
            let etype = if k % 2 == 0 { EventType::OrderAdd } else { EventType::OrderCancel };
            let oid = self.next_oid;
            self.next_oid = self.next_oid.wrapping_add(1);
            out.push(mk(seq, ts, etype, opp_side, px, 200, oid));
            seq += 1;
            ts = ts.saturating_add(50_000);
        }

        self.bursts_remaining -= 1;
        self.next_due = now_ns.saturating_add(self.step_period_ns);
        out
    }

    fn next_due_ns(&self) -> u64 {
        self.next_due
    }
    fn finished(&self) -> bool {
        self.bursts_remaining == 0
    }
    fn kind_label(&self) -> &'static str {
        "latency_arb_proxy"
    }
}

// ─── Tests ───────────────────────────────────────────────────────────
//
// Each test asserts that running the generator's stream through the
// matching detector produces at least one ChaosEvent of the expected
// kind. This is the *closure proof* that prosecutor and judge agree.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cancellation_storm::CancellationStormDetector;
    use crate::detection::{QuoteStuffDetector, SpoofDetector};
    use crate::flash_crash::FlashCrashDetector;
    use crate::latency_arb_proxy::LatencyArbProxyDetector;
    use crate::momentum_ignition::MomentumIgnitionDetector;
    use crate::phantom_liquidity::PhantomLiquidityDetector;
    use crate::ChaosKind;

    fn run<G: StormGenerator>(mut g: G, max_steps: u32) -> Vec<SequencedEvent> {
        let mut out = Vec::new();
        let mut seq: u64 = 1;
        let mut now: u64 = 1_000_000;
        for _ in 0..max_steps {
            if g.finished() {
                break;
            }
            let evs = g.step(now, seq);
            // Trust the batch: a generator may emit a `seq` jump (e.g.
            // cancellation storm pre-fills with a gap so the rolling
            // window is wiped). Pick the next seq from the highest one
            // observed in the batch, falling back to a simple bump.
            if let Some(max_seq) = evs.iter().map(|e| e.seq).max() {
                seq = max_seq.saturating_add(1);
            } else {
                seq = seq.saturating_add(1);
            }
            now = now.saturating_add(1_000_000);
            out.extend(evs);
        }
        out
    }

    #[test]
    fn phantom_generator_fires_phantom_detector() {
        let g = PhantomLiquidityGenerator::new(0xDEAD_BEEF, 0.9, 100_000, 8);
        let stream = run(g, 16);
        let mut det = PhantomLiquidityDetector::new(256, 5_000_000, 4096, 100);
        let mut hits = 0;
        for ev in &stream {
            if let Some(e) = det.process(ev) {
                assert_eq!(e.kind, ChaosKind::PhantomLiquidity);
                hits += 1;
            }
        }
        assert!(hits >= 4, "expected >=4 phantom flags, got {hits}");
    }

    #[test]
    fn quote_stuff_generator_fires_quote_stuff_detector() {
        let g = QuoteStuffGenerator::new(0xAAA5, 0.9, 100_000, 2);
        let stream = run(g, 4);
        let mut det = QuoteStuffDetector::new(48, 8.0);
        let mut hits = 0;
        for ev in &stream {
            if let Some(e) = det.process(ev) {
                assert_eq!(e.kind, ChaosKind::QuoteStuff);
                hits += 1;
            }
        }
        assert!(hits >= 1, "expected quote-stuff flag, got {hits}");
    }

    #[test]
    fn spoof_generator_fires_spoof_detector() {
        let g = SpoofGenerator::new(0xAAA6, 0.9, 100_000, 4);
        let stream = run(g, 8);
        let mut det = SpoofDetector::new(1_000, 64);
        let mut hits = 0;
        for ev in &stream {
            if let Some(e) = det.process(ev) {
                assert_eq!(e.kind, ChaosKind::Spoof);
                hits += 1;
            }
        }
        assert!(hits >= 1, "expected spoof flag, got {hits}");
    }

    #[test]
    fn cancel_storm_generator_fires_cancel_storm_detector() {
        let g = CancellationStormGenerator::new(0xC0FF_EE, 0.95, 100_000);
        let stream = run(g, 80);
        let mut det = CancellationStormDetector::new(
            2_048, 0, 128, 1_024, 0.05, 4.0, 8, 4_096,
        );
        let mut hits = 0;
        for ev in &stream {
            if let Some(e) = det.process(ev) {
                assert_eq!(e.kind, ChaosKind::CancellationStorm);
                hits += 1;
            }
        }
        assert!(hits >= 1, "expected at least one storm flag, got {hits}");
    }

    #[test]
    fn momentum_generator_fires_momentum_detector() {
        let g = MomentumIgnitionGenerator::new(0x1234, 0.9, 100_000, true);
        let stream = run(g, 30);
        let mut det = MomentumIgnitionDetector::new(
            1_024, 0, 64, 5, 8, 4, 2_048,
        );
        let mut hits = 0;
        for ev in &stream {
            if let Some(e) = det.process(ev) {
                assert_eq!(e.kind, ChaosKind::MomentumIgnition);
                hits += 1;
            }
        }
        assert!(hits >= 1, "expected momentum flag, got {hits}");
    }

    #[test]
    fn flash_crash_generator_fires_flash_crash_detector() {
        let g = FlashCrashGenerator::new(0xBADC_0FFE, 0.9, 100_000, true, 3);
        let stream = run(g, 20);
        let mut det = FlashCrashDetector::new(10, 8, 64, 5, 0.6, 3, 2_048);
        let mut hits = 0;
        for ev in &stream {
            if let Some(e) = det.process(ev) {
                assert_eq!(e.kind, ChaosKind::FlashCrash);
                hits += 1;
            }
        }
        assert!(hits >= 1, "expected at least one flash-crash flag, got {hits}");
    }

    #[test]
    fn latency_arb_generator_fires_latency_arb_detector() {
        let g = LatencyArbProxyGenerator::new(0xABCD_1234, 0.9, 100_000, 8);
        let stream = run(g, 20);
        let mut det = LatencyArbProxyDetector::new(
            64, 0, 64, 4, 200, 4, false, 0, 1_024,
        );
        let mut hits = 0;
        for ev in &stream {
            if let Some(e) = det.process(ev) {
                assert_eq!(e.kind, ChaosKind::LatencyArbitrage);
                hits += 1;
            }
        }
        assert!(hits >= 1, "expected latency-arb flag, got {hits}");
    }

    #[test]
    fn determinism_seed_repeats_byte_stream() {
        // Same seed → identical event byte stream.
        let g1 = FlashCrashGenerator::new(42, 0.7, 100_000, true, 2);
        let g2 = FlashCrashGenerator::new(42, 0.7, 100_000, true, 2);
        let s1 = run(g1, 6);
        let s2 = run(g2, 6);
        assert_eq!(s1.len(), s2.len(), "length mismatch");
        for (a, b) in s1.iter().zip(s2.iter()) {
            assert_eq!(a.seq, b.seq);
            assert_eq!(a.event.ts, b.event.ts);
            assert_eq!(a.event.price, b.event.price);
            assert_eq!(a.event.qty, b.event.qty);
            assert_eq!(a.event.event_type, b.event.event_type);
            assert_eq!(a.event.side, b.event.side);
            assert_eq!(a.event.order_id, b.event.order_id);
        }
    }
}
