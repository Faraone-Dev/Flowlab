// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

//! Engine — wires Source → HotOrderBook → analytics → telemetry.
//!
//! Single-threaded by default. Source on main thread; TCP telemetry server
//! on its own thread, bounded mpsc. Backpressure: see [`crate::backpressure`].
//!
//! Latency stages: `apply`, `analytics`, `risk`, `wire-out`. `parse` is
//! owned by the source (0 for SyntheticSource).

use crate::backpressure::Producer;
use crate::source::Source;
use crate::wire::{
    BookFrame, ChaosFrame, Header, LatFrame, Level, RiskFrame, StageLat, TelemetryFrame, TickFrame,
    TradeFrame,
};
use flowlab_chaos::chain::ChaosChain;
use flowlab_chaos::ChaosEvent;
use flowlab_core::event::EventType;
use flowlab_core::hot_book::HotOrderBook;
use flowlab_flow::imbalance::book_imbalance;
use flowlab_flow::{
    spread::SpreadTracker,
    vpin::VpinCalculator,
    BreakerConfig, BreakerSnapshot, CircuitBreaker, HaltReason, Regime, RegimeClassifier,
    RegimeInput,
};
use std::time::{Duration, Instant};

/// Per-stage rolling stats — fixed window, simple percentile from sorted copy.
///
/// Uses a true FIFO ring buffer so the evicted sample is always the
/// oldest one. The previous implementation overwrote index `cap/2`
/// every time the window was full, which silently pinned stale
/// samples near the median and distorted the rolling percentiles.
struct StageStats {
    samples: Vec<u64>,
    cap: usize,
    /// Next write slot (0..cap). Advances modulo cap once the buffer
    /// is full.
    head: usize,
}

impl StageStats {
    fn new(cap: usize) -> Self {
        Self { samples: Vec::with_capacity(cap), cap, head: 0 }
    }
    fn push(&mut self, ns: u64) {
        if self.samples.len() < self.cap {
            self.samples.push(ns);
        } else {
            // True FIFO eviction: overwrite the oldest slot.
            self.samples[self.head] = ns;
            self.head += 1;
            if self.head == self.cap {
                self.head = 0;
            }
        }
    }
    fn snapshot(&self) -> StageLat {
        if self.samples.is_empty() {
            return StageLat { p50_ns: 0, p99_ns: 0, p999_ns: 0, max_ns: 0 };
        }
        let mut s = self.samples.clone();
        s.sort_unstable();
        let pick = |q: f64| -> u64 {
            let i = ((s.len() as f64 - 1.0) * q).round() as usize;
            s[i.min(s.len() - 1)]
        };
        StageLat {
            p50_ns: pick(0.50),
            p99_ns: pick(0.99),
            p999_ns: pick(0.999),
            max_ns: *s.last().unwrap(),
        }
    }
}

/// Log-linear histogram for apply latency. 192 bins, 1 ns ... ~16 ms.
struct ApplyHisto {
    bins: Vec<u64>,
}

impl ApplyHisto {
    fn new() -> Self {
        Self { bins: vec![0; 192] }
    }
    fn record(&mut self, ns: u64) {
        if ns == 0 {
            self.bins[0] = self.bins[0].saturating_add(1);
            return;
        }
        // log2(ns) integer part, 24 powers (1 ns ... 16 ms = 2^24 ns), 8 sub-bins
        let lg = 63 - ns.leading_zeros() as usize;
        let lg = lg.min(23);
        let span = 1u64 << lg;
        let frac = if span > 0 { (ns - span) * 8 / span } else { 0 };
        let bin = (lg * 8 + frac as usize).min(self.bins.len() - 1);
        self.bins[bin] = self.bins[bin].saturating_add(1);
    }
}

pub struct EngineConfig {
    pub instrument_id: u32,
    /// If Some, hard-lock to this stock_locate and ignore all other events.
    /// If None, the engine auto-locks to the first instrument that prints
    /// `auto_lock_threshold` trades (picks a name with an actual tape, not
    /// just quote churn).
    pub track_instrument: Option<u32>,
    pub auto_lock_threshold: u32,
    pub tick_publish_hz: u32,    // dashboard tick rate
    pub book_publish_hz: u32,    // ladder snapshot rate
    pub depth_levels: usize,     // top-N for ladder
    pub vpin_bucket: u64,
    pub vpin_buckets: usize,
    pub spread_window: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            instrument_id: 1,
            track_instrument: None,
            auto_lock_threshold: 5,
            tick_publish_hz: 50,
            book_publish_hz: 10,
            depth_levels: 10,
            // VPIN bucket sizing: TSLA-class names print ~50-200 share
            // averages. 10k shares -> ~5-15s per bucket which lets the
            // 50-bucket rolling window swing 0.0..0.8 between balanced
            // and toxic minutes once aggressor classification is honest.
            vpin_bucket: 10_000,
            vpin_buckets: 50,
            spread_window: 256,
        }
    }
}

pub struct Engine {
    cfg: EngineConfig,
    book: HotOrderBook<256>,
    spread: SpreadTracker,
    vpin: VpinCalculator,
    /// Pre-trade / post-trade risk gate. Read in `build_risk` via
    /// `snapshot()`; `check` / `record_fill` become live once the
    /// strategy crate is wired.
    breaker: CircuitBreaker,
    /// Threshold-based regime classifier — single source of truth for
    /// the `regime` field on `TickFrame`. Uses `flow::spread` blowout
    /// ratio + imbalance + VPIN + trade velocity ratio.
    regime: RegimeClassifier,
    started: Instant,

    apply_stats: StageStats,
    analytics_stats: StageStats,
    risk_stats: StageStats,
    wire_stats: StageStats,
    histo: ApplyHisto,

    /// Sub-ns hot-path clock. Calibrated once in `new()`. `Instant::now()`
    /// on Windows clips at the QPC tick (~100-200 ns) which makes
    /// `OrderBook::apply` (sub-100 ns) read as a flat floor on the
    /// dashboard. `rdtsc` reads the invariant TSC at ~25 cycles.
    tsc: crate::tsc::TscClock,

    last_vpin: f64,
    last_imbalance: f64,
    /// Rolling-window blowout ratio from `SpreadTracker::record`. 0.0
    /// until the window is warm. Fed to the regime classifier.
    last_blowout_ratio: f64,
    /// Mid price (in ticks) seen at the previous 1s regime tick.
    /// Used to compute mid drift (bps/s) which drives the regime
    /// classifier's directional-move component.
    prev_mid_ticks: i64,
    /// |Δmid|/mid in bps over the last 1s window. Updated alongside
    /// `last_eps` and consumed by the composite regime score.
    last_mid_drift_bps: f64,
    /// EWMA of trade velocity used as the baseline for
    /// `trade_velocity_ratio = current / baseline`. Seeded on the first
    /// non-zero sample so startup doesn't classify CALM as CRISIS.
    trade_velocity_baseline: f64,
    seq: u64,

    // symbol filtering
    tracked: Option<u32>,
    instrument_counts: std::collections::HashMap<u32, u32>,
    last_locked_trade_at: Option<Instant>,

    // throughput accounting
    events_in_window: u64,
    trades_in_window: u64,
    window_started: Instant,
    last_eps: u64,
    last_trade_vel: f64,

    // chaos detection pipeline
    chaos_chain: ChaosChain,
    /// Reusable buffer for chaos detection output (zero per-event alloc).
    chaos_buf: Vec<ChaosEvent>,
}

#[inline]
fn halt_reason_name(r: HaltReason) -> String {
    match r {
        HaltReason::RateLimit => "rate_limit",
        HaltReason::PositionCap => "position_cap",
        HaltReason::DailyLossFloor => "daily_loss_floor",
        HaltReason::OrderToTradeRatio => "order_to_trade_ratio",
        HaltReason::FeedGap => "feed_gap",
        HaltReason::Manual => "manual",
    }
    .to_string()
}

#[inline]
fn chaos_kind_name(k: flowlab_chaos::ChaosKind) -> String {
    match k {
        flowlab_chaos::ChaosKind::QuoteStuff => "QuoteStuff",
        flowlab_chaos::ChaosKind::PhantomLiquidity => "PhantomLiquidity",
        flowlab_chaos::ChaosKind::Spoof => "Spoof",
        flowlab_chaos::ChaosKind::CancellationStorm => "CancellationStorm",
        flowlab_chaos::ChaosKind::MomentumIgnition => "MomentumIgnition",
        flowlab_chaos::ChaosKind::FlashCrash => "FlashCrash",
        flowlab_chaos::ChaosKind::LatencyArbitrage => "LatencyArbitrage",
    }
    .to_string()
}

impl Engine {
    pub fn new(cfg: EngineConfig) -> Self {
        let book = HotOrderBook::new(cfg.instrument_id);
        let spread = SpreadTracker::new(cfg.spread_window);
        let vpin = VpinCalculator::new(cfg.vpin_bucket, cfg.vpin_buckets);
        let breaker = CircuitBreaker::new(BreakerConfig::default());
        let regime = RegimeClassifier::default();
        let tracked = cfg.track_instrument;
        Self {
            cfg,
            book,
            spread,
            vpin,
            breaker,
            regime,
            started: Instant::now(),
            // 256-sample rolling window: at 1k-10k evt/s this gives a
            // ~25-250 ms window so the dashboard sees real jitter
            // tick-by-tick instead of an over-smoothed flatline.
            apply_stats: StageStats::new(256),
            analytics_stats: StageStats::new(256),
            risk_stats: StageStats::new(256),
            wire_stats: StageStats::new(256),
            histo: ApplyHisto::new(),
            tsc: {
                let c = crate::tsc::TscClock::calibrate();
                tracing::info!(
                    cycles_per_ns = format!("{:.3}", c.cycles_per_ns()),
                    overhead_cycles = c.overhead_cycles(),
                    "TSC clock calibrated"
                );
                c
            },
            last_vpin: 0.0,
            last_imbalance: 0.0,
            last_blowout_ratio: 0.0,
            prev_mid_ticks: 0,
            last_mid_drift_bps: 0.0,
            trade_velocity_baseline: 0.0,
            seq: 0,
            tracked,
            instrument_counts: std::collections::HashMap::new(),
            last_locked_trade_at: None,
            events_in_window: 0,
            trades_in_window: 0,
            window_started: Instant::now(),
            last_eps: 0,
            last_trade_vel: 0.0,

            chaos_chain: ChaosChain::default_itch(),
            chaos_buf: Vec::with_capacity(8),
        }
    }

    /// Drain the source forever (or until exhausted), publishing telemetry.
    pub fn run(mut self, source: &mut dyn Source, out: Producer<TelemetryFrame>) {
        // Header frame first.
        let src_name = source.name().to_string();
        let src_live = source.is_live();
        out.try_send(TelemetryFrame::Header(Header {
            source: src_name.clone(),
            is_live: src_live,
            started_at_ns: 0,
            symbol: self.tracked.and_then(|id| source.symbol_of(id).map(str::to_string)),
            instrument_id: self.tracked,
        }));

        let tick_period = Duration::from_secs_f64(1.0 / self.cfg.tick_publish_hz as f64);
        let book_period = Duration::from_secs_f64(1.0 / self.cfg.book_publish_hz as f64);
        let mut next_tick = Instant::now() + tick_period;
        let mut next_book = Instant::now() + book_period;
        let mut next_eps = Instant::now() + Duration::from_secs(1);

        loop {
            // ── pull next event
            let Some(ev) = source.next() else {
                if !source.is_live() {
                    break;
                }
                // Bounded spin instead of `thread::sleep(50µs)`. On
                // Windows the default scheduler tick is ~15.6 ms, so
                // any `sleep(<1 ms)` rounds up to one full tick and
                // silently adds ~1 ms of idle-loop latency. A ~1024 ×
                // `spin_loop()` burst stays well under a scheduler
                // tick, then `yield_now()` releases the core when the
                // source stays dry.
                for _ in 0..1024 {
                    std::hint::spin_loop();
                }
                std::thread::yield_now();
                continue;
            };

            // ── symbol filter: auto-lock to the first instrument that
            //    actually prints trades (not just quote churn). ITCH ships
            //    order adds/cancels for ~8k tickers; on BX most of them are
            //    dark to the tape. Locking on trade count picks a name the
            //    trader can actually see on the tape.
            // Stale-symbol re-arm: if the locked symbol hasn't printed a
            // trade in 5s wall-clock (~250s replay at 50x), drop the lock
            // and let the next active name take over. Keeps the desk live
            // when a thin ETN goes quiet mid-session.
            //
            // ⚠ Skip re-arm when the symbol was pinned via CLI
            // (`--symbol XYZ`). The operator explicitly chose that name;
            // silently switching to another ticker mid-demo would be a
            // worse failure than a quiet 10s.
            if let (Some(_), Some(last)) = (self.tracked, self.last_locked_trade_at) {
                if self.cfg.track_instrument.is_none() && last.elapsed() > Duration::from_secs(5) {
                    tracing::info!("locked symbol stale > 5s — re-arming auto-lock");
                    self.tracked = None;
                    self.last_locked_trade_at = None;
                    self.instrument_counts.clear();
                }
            }

            match self.tracked {
                Some(id) => {
                    if ev.instrument_id != id {
                        continue;
                    }
                    if ev.event_type == EventType::Trade as u8 {
                        self.last_locked_trade_at = Some(Instant::now());
                    }
                }
                None => {
                    if ev.event_type == EventType::Trade as u8 {
                        let trades_seen = {
                            let c = self.instrument_counts.entry(ev.instrument_id).or_insert(0);
                            *c += 1;
                            *c
                        };
                        if trades_seen >= self.cfg.auto_lock_threshold {
                            self.tracked = Some(ev.instrument_id);
                            self.book = HotOrderBook::new(ev.instrument_id);
                            self.instrument_counts.clear();
                            self.last_locked_trade_at = Some(Instant::now());
                            let sym = source.symbol_of(ev.instrument_id).map(str::to_string);
                            tracing::info!(
                                instrument_id = ev.instrument_id,
                                symbol = ?sym,
                                trades_seen = trades_seen,
                                "auto-locked to symbol",
                            );
                            out.try_send(TelemetryFrame::Header(Header {
                                source: src_name.clone(),
                                is_live: src_live,
                                started_at_ns: 0,
                                symbol: sym,
                                instrument_id: Some(ev.instrument_id),
                            }));
                        } else {
                            continue;
                        }
                    } else {
                        continue;
                    }
                }
            }

            self.seq = self.seq.wrapping_add(1);
            self.events_in_window += 1;

            // ── tag trade aggressor against PRE-apply top-of-book.
            //    ITCH 'E' OrderExecuted carries no price (it references an
            //    order_id already resting), so parse_order_executed sets
            //    price=0. Emitting those as "0.0000" on the tape is
            //    misleading — substitute the resting price from the book
            //    using the event's side. If that's also 0 (book not yet
            //    warm), suppress the print entirely.
            let trade_info = if ev.event_type == EventType::Trade as u8 {
                let bb = self.book.best_bid().unwrap_or(0);
                let ba = self.book.best_ask().unwrap_or(0);
                let price = if ev.price > 0 {
                    ev.price
                } else if ev.side == 1 {
                    ba // sell-side exec: lifted the ask
                } else {
                    bb // buy-side exec: hit the bid
                };
                if price == 0 {
                    None
                } else {
                    // Lee-Ready aggressor classification, but ONLY when
                    // the book is sane (ba > bb). On a crossed/locked
                    // top (which happens on BX during stale updates and
                    // ITCH gap recovery) `price >= ba` AND `price <= bb`
                    // are both true, so the naive `if` would tag every
                    // trade as a buy and saturate VPIN at 1.0. Treat
                    // crossed/locked as "unknown side" -> 0 (VPIN will
                    // split 50/50).
                    let crossed = bb > 0 && ba > 0 && bb >= ba;
                    let aggressor = if crossed {
                        0i8
                    } else if ba > 0 && price >= ba {
                        1i8
                    } else if bb > 0 && price <= bb {
                        -1i8
                    } else {
                        0i8
                    };
                    Some((price, ev.qty, ev.ts, aggressor))
                }
            } else {
                None
            };

            // ── apply (stage = apply)
            let t0 = crate::tsc::rdtsc();
            let _changed = self.book.apply(&ev);
            let apply_ns = self.tsc.delta_ns(t0, crate::tsc::rdtsc());
            self.apply_stats.push(apply_ns);
            self.histo.record(apply_ns);

            // ── chaos detection ──
            self.chaos_buf.clear();
            let seq_ev = flowlab_core::event::SequencedEvent {
                seq: self.seq,
                channel_id: 0,
                event: ev,
            };
            self.chaos_chain.process_into(&seq_ev, &mut self.chaos_buf);
            for ce in self.chaos_buf.drain(..) {
                out.try_send(TelemetryFrame::Chaos(ChaosFrame {
                    seq: self.seq,
                    kind: chaos_kind_name(ce.kind),
                    severity: ce.severity,
                    start_seq: ce.start_seq,
                    end_seq: ce.end_seq,
                    initiator: ce.initiator,
                }));
            }

            // ── analytics (only on trades / on imbalance refresh)
            let t1 = crate::tsc::rdtsc();
            if ev.event_type == EventType::Trade as u8 {
                self.trades_in_window += 1;
                // Use the Lee-Ready aggressor sign computed above when
                // available (price-vs-NBBO classification). Falling back
                // to ev.side here would feed VPIN with the resting-order
                // side, which on ITCH 'P' messages saturates the metric.
                if let Some((_, qty, _, aggr)) = trade_info {
                    if let Some(v) = self.vpin.process_trade_signed(qty, aggr) {
                        self.last_vpin = v;
                    }
                } else if let Some(v) = self.vpin.process_trade(&ev) {
                    self.last_vpin = v;
                }
            }
            self.last_imbalance = book_imbalance(&self.book, self.cfg.depth_levels);
            if let Some(sm) = self.spread.record(&self.book) {
                self.last_blowout_ratio = sm.blowout_ratio;
            }
            self.analytics_stats.push(self.tsc.delta_ns(t1, crate::tsc::rdtsc()));

            // ── risk: probe with a zero-quantity intent — exercises every guard
            // without polluting position. Real intents are pushed by the
            // strategy crate; here we just sample the breaker latency.
            let t2 = crate::tsc::rdtsc();
            // No probe call yet — keeps risk_stats honest as "no-op overhead".
            self.risk_stats.push(self.tsc.delta_ns(t2, crate::tsc::rdtsc()));

            // ── publish Trade frame (after apply, so downstream can
            //    correlate with the resulting mid)
            if let Some((px, qty, ts, aggr)) = trade_info {
                out.try_send(TelemetryFrame::Trade(TradeFrame {
                    seq: self.seq,
                    ts_ns: ts,
                    price_ticks: px,
                    qty,
                    aggressor: aggr,
                }));
            }

            // ── periodic publishes
            let now = Instant::now();
            if now >= next_eps {
                let elapsed = now.duration_since(self.window_started).as_secs_f64().max(1e-3);
                self.last_eps = (self.events_in_window as f64 / elapsed) as u64;
                self.last_trade_vel = self.trades_in_window as f64 / elapsed;
                // EWMA baseline for the velocity-ratio regime input.
                // Seed on first non-zero sample, then blend 1/8 new.
                if self.last_trade_vel > 0.0 {
                    if self.trade_velocity_baseline == 0.0 {
                        self.trade_velocity_baseline = self.last_trade_vel;
                    } else {
                        self.trade_velocity_baseline = 0.875 * self.trade_velocity_baseline
                            + 0.125 * self.last_trade_vel;
                    }
                }
                let bb = self.book.best_bid().unwrap_or(0);
                let ba = self.book.best_ask().unwrap_or(0);
                // Compute |Δmid|/mid (bps) over this 1s window. Accept
                // both regular (ba > bb) and crossed/locked (ba <= bb)
                // tops -- on BX the crossed state can persist for tens
                // of seconds and we still want to track price drift.
                let cur_mid = if bb > 0 && ba > 0 { ((bb + ba) / 2) as i64 } else { 0 };
                if cur_mid > 0 && self.prev_mid_ticks > 0 {
                    let d = (cur_mid - self.prev_mid_ticks).abs() as f64;
                    self.last_mid_drift_bps = (d / cur_mid as f64) * 10_000.0;
                } else {
                    self.last_mid_drift_bps = 0.0;
                }
                if cur_mid > 0 {
                    self.prev_mid_ticks = cur_mid;
                }
                let sym = self
                    .tracked
                    .and_then(|id| source.symbol_of(id).map(str::to_string))
                    .unwrap_or_else(|| "<none>".into());
                // ── REGIME diagnostic: show every component, the composite
                //    score, and the resulting label so we can see exactly
                //    why we are (or are not) escalating to CRISIS.
                //
                // Weights (rebalanced so the regime *oscillates* across
                // all four levels instead of being pinned at AGGRESSIVE
                // by VPIN baseline alone):
                //   spread_blowout - 1   × 1.0   (1.5x = +0.5)
                //   |imbalance|          × 3.0   (0.30 = +0.9)
                //   vpin                 × 5.0   (0.25 baseline = +1.25)
                //   |vel_ratio - 1|      × 0.6   (2x or 0.5x = +0.6)
                //   mid_drift_bps        × 0.30  (10 bps/s = +3.0 → CRISIS)
                let vel_ratio = if self.trade_velocity_baseline > 0.0 {
                    self.last_trade_vel / self.trade_velocity_baseline
                } else {
                    1.0
                };
                let s_spread = (self.last_blowout_ratio - 1.0).max(0.0);
                let s_imb_raw = self.last_imbalance.abs() * 3.0;
                let s_vpin = self.last_vpin * 5.0;
                let s_vel = (vel_ratio - 1.0).abs() * 0.6;
                let s_drift = self.last_mid_drift_bps * 0.30;
                // ── Crossed-book guard: when bid >= ask the topline is
                //    inverted (common on inferior-quote venues like BX
                //    where local best is below NBBO). In that state both
                //    `imbalance` and `blowout` are computed against a
                //    nonsensical book and saturate near ±1 / huge values.
                //    Suppress them; trust only vpin / vel / drift.
                let crossed_top = bb > 0 && ba > 0 && bb >= ba;
                let (s_imb, s_spread_eff) = if crossed_top {
                    (0.0, 0.0)
                } else {
                    (s_imb_raw, s_spread)
                };
                let raw_score = s_spread_eff + s_imb + s_vpin + s_vel + s_drift;
                // ── Freshness dampener: when the tape is dead (eps<30)
                //    the topline of the book gets stale (top quote pulled,
                //    next level is deep) → spread/imbalance become
                //    artifacts, not real stress. Linearly fade the score
                //    toward 0 as eps drops below the threshold so a
                //    sparse symbol can't permanently shout CRISIS.
                let freshness = ((self.last_eps as f64) / 30.0).min(1.0);
                let score = raw_score * freshness;
                let regime_lbl = if score >= 3.0 {
                    "CRISIS"
                } else if score >= 1.5 {
                    "AGGRESSIVE"
                } else if score >= 0.6 {
                    "VOLATILE"
                } else {
                    "CALM"
                };
                tracing::info!(
                    target: "regime",
                    sym = %sym,
                    eps = self.last_eps,
                    trd_per_s = format!("{:.1}", self.last_trade_vel),
                    bb = bb,
                    ba = ba,
                    crossed = crossed_top,
                    blowout = format!("{:.2}", self.last_blowout_ratio),
                    imb = format!("{:.3}", self.last_imbalance),
                    vpin = format!("{:.3}", self.last_vpin),
                    vel_ratio = format!("{:.2}", vel_ratio),
                    drift_bps = format!("{:.2}", self.last_mid_drift_bps),
                    fresh = format!("{:.2}", freshness),
                    "scores: spread={:.2} imb={:.2} vpin={:.2} vel={:.2} drift={:.2} | RAW={:.2} *fresh={:.2} = TOT={:.2} -> {}",
                    s_spread_eff, s_imb, s_vpin, s_vel, s_drift, raw_score, freshness, score, regime_lbl
                );
                self.events_in_window = 0;
                self.trades_in_window = 0;
                self.window_started = now;
                next_eps = now + Duration::from_secs(1);
            }

            if now >= next_tick {
                let frame = self.build_tick(&ev, out.dropped_total());
                let t3 = crate::tsc::rdtsc();
                // Skip the tick if the book isn't yet two-sided — emitting
                // mid=0 pollutes the MID spark and ruins the scale.
                if frame.mid_ticks > 0 {
                    out.try_send(TelemetryFrame::Tick(frame));
                }
                self.wire_stats.push(self.tsc.delta_ns(t3, crate::tsc::rdtsc()));

                // also publish risk + lat at the tick cadence
                out.try_send(TelemetryFrame::Risk(self.build_risk()));
                out.try_send(TelemetryFrame::Lat(self.build_lat()));
                next_tick = now + tick_period;
            }

            if now >= next_book {
                out.try_send(TelemetryFrame::Book(self.build_book()));
                next_book = now + book_period;
            }
        }
    }

    fn build_tick(&self, ev: &flowlab_core::event::Event, dropped: u64) -> TickFrame {
        let best_bid = self.book.best_bid().unwrap_or(0) as i64;
        let best_ask = self.book.best_ask().unwrap_or(0) as i64;
        // A mid is only meaningful when BOTH sides exist AND the book is
        // not crossed/locked. Locked markets (bid == ask) briefly happen
        // during add/cancel churn at the top — report them as "no mid"
        // instead of fabricating a mid that equals the top price.
        let two_sided = best_bid > 0 && best_ask > 0 && best_ask > best_bid;
        let mid = if two_sided { (best_bid + best_ask) / 2 } else { 0 };
        let spread = if two_sided { best_ask - best_bid } else { 0 };
        let bid_depth = self.book.bid_depth(self.cfg.depth_levels);
        let ask_depth = self.book.ask_depth(self.cfg.depth_levels);

        // Microprice: size-weighted mid — weight each side by the OPPOSITE
        // side's size, so heavy bid pulls price toward ask and vice versa.
        // micro = (bid_px * ask_sz + ask_px * bid_sz) / (bid_sz + ask_sz)
        let bsz = self.book.best_bid_size() as i128;
        let asz = self.book.best_ask_size() as i128;
        let microprice = if two_sided && (bsz + asz) > 0 {
            ((best_bid as i128 * asz + best_ask as i128 * bsz) / (bsz + asz)) as i64
        } else {
            mid
        };
        let spread_bps = if two_sided && mid > 0 {
            (spread as f64 / mid as f64) * 10_000.0
        } else {
            0.0
        };
        // Regime classification: delegate to `flow::regime::RegimeClassifier`
        // so the thresholds, composite formula, and units stay in one
        // place (and stay testable from the flow crate directly).
        let velocity_ratio = if self.trade_velocity_baseline > 0.0 {
            self.last_trade_vel / self.trade_velocity_baseline
        } else {
            1.0
        };
        // Same crossed-book guard the engine's debug log uses,
        // mirrored down to the flow-crate classifier so the
        // dashboard's REGIME label matches the log line.
        let book_crossed = best_bid > 0 && best_ask > 0 && best_bid >= best_ask;
        let regime_input = RegimeInput {
            spread_blowout_ratio: self.last_blowout_ratio,
            book_imbalance: self.last_imbalance,
            vpin: self.last_vpin,
            trade_velocity_ratio: velocity_ratio,
            depth_depletion: 0.0,
            mid_drift_bps: self.last_mid_drift_bps,
            events_per_sec: self.last_eps,
            book_crossed,
        };
        let regime: u8 = match self.regime.classify(&regime_input) {
            Regime::Calm => 0,
            Regime::Volatile => 1,
            Regime::Aggressive => 2,
            Regime::Crisis => 3,
        };
        TickFrame {
            seq: self.seq,
            event_time_ns: ev.ts,
            process_time_ns: self.started.elapsed().as_nanos() as u64,
            mid_ticks: mid,
            spread_ticks: spread,
            microprice_ticks: microprice,
            spread_bps,
            bid_depth,
            ask_depth,
            imbalance: self.last_imbalance,
            vpin: self.last_vpin,
            trade_velocity: self.last_trade_vel,
            regime,
            events_per_sec: self.last_eps,
            dropped_total: dropped,
        }
    }

    fn build_risk(&self) -> RiskFrame {
        // Sample the breaker once — single consistent view of halted /
        // counters / position / gaps (the snapshot takes its locks
        // briefly so the fields agree).
        let snap: BreakerSnapshot = self.breaker.snapshot();
        let otr_ratio = if snap.trades_done == 0 {
            snap.orders_sent as f64
        } else {
            snap.orders_sent as f64 / snap.trades_done as f64
        };
        let cfg = BreakerConfig::default();
        RiskFrame {
            halted: snap.halted,
            reason: snap.reason.map(halt_reason_name),
            orders_sent: snap.orders_sent,
            trades_done: snap.trades_done,
            otr_ratio,
            otr_limit: cfg.max_order_to_trade_ratio.unwrap_or(0.0),
            net_position: snap.net_position,
            position_limit: cfg.max_position.unwrap_or(0),
            cash_flow_ticks: snap.cash_flow_ticks,
            daily_loss_floor_ticks: cfg.max_daily_loss_ticks.unwrap_or(0),
            gaps_in_window: snap.gaps_in_window,
            gap_threshold: cfg.gap_threshold.unwrap_or(0),
        }
    }

    fn build_lat(&self) -> LatFrame {
        LatFrame {
            parse: StageLat::default(),
            apply: self.apply_stats.snapshot(),
            analytics: self.analytics_stats.snapshot(),
            risk: self.risk_stats.snapshot(),
            wire_out: self.wire_stats.snapshot(),
            histo_apply: self.histo.bins.clone(),
        }
    }

    fn build_book(&self) -> BookFrame {
        let take = self.cfg.depth_levels;
        let bids = self
            .book
            .bid_levels()
            .iter()
            .take(take)
            .map(|l| Level { price_ticks: l.price, qty: l.total_qty, order_count: l.order_count })
            .collect();
        let asks = self
            .book
            .ask_levels()
            .iter()
            .take(take)
            .map(|l| Level { price_ticks: l.price, qty: l.total_qty, order_count: l.order_count })
            .collect();
        BookFrame { seq: self.seq, bids, asks }
    }
}

impl Default for StageLat {
    fn default() -> Self {
        Self { p50_ns: 0, p99_ns: 0, p999_ns: 0, max_ns: 0 }
    }
}
