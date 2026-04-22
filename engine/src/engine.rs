//! Engine — wires Source → HotOrderBook → analytics → telemetry.
//!
//! Single-threaded by default. The Source runs on the main thread; the TCP
//! telemetry server runs on its own thread and consumes a bounded mpsc.
//! Backpressure: see [`crate::backpressure`].
//!
//! Latency stages are measured around discrete spans:
//!   - apply     : HotOrderBook::apply
//!   - analytics : VPIN + imbalance + spread + regime
//!   - risk      : CircuitBreaker::check (no-op when no intent — measured separately)
//!   - wire-out  : encode + try_send
//!
//! `parse` is owned by the source (when it is a real protocol decoder).
//! For SyntheticSource it is reported as 0.

use crate::backpressure::Producer;
use crate::source::Source;
use crate::wire::{
    BookFrame, Header, LatFrame, Level, RiskFrame, StageLat, TelemetryFrame, TickFrame, TradeFrame,
};
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
            vpin_bucket: 1_000,
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

    last_vpin: f64,
    last_imbalance: f64,
    /// Rolling-window blowout ratio from `SpreadTracker::record`. 0.0
    /// until the window is warm. Fed to the regime classifier.
    last_blowout_ratio: f64,
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
            apply_stats: StageStats::new(8192),
            analytics_stats: StageStats::new(8192),
            risk_stats: StageStats::new(8192),
            wire_stats: StageStats::new(8192),
            histo: ApplyHisto::new(),
            last_vpin: 0.0,
            last_imbalance: 0.0,
            last_blowout_ratio: 0.0,
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
            if let (Some(_), Some(last)) = (self.tracked, self.last_locked_trade_at) {
                if last.elapsed() > Duration::from_secs(5) {
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
                    let aggressor = if ba > 0 && price >= ba {
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
            let t0 = Instant::now();
            let _changed = self.book.apply(&ev);
            let apply_ns = t0.elapsed().as_nanos() as u64;
            self.apply_stats.push(apply_ns);
            self.histo.record(apply_ns);

            // ── analytics (only on trades / on imbalance refresh)
            let t1 = Instant::now();
            if ev.event_type == EventType::Trade as u8 {
                self.trades_in_window += 1;
                if let Some(v) = self.vpin.process_trade(&ev) {
                    self.last_vpin = v;
                }
            }
            self.last_imbalance = book_imbalance(&self.book, self.cfg.depth_levels);
            if let Some(sm) = self.spread.record(&self.book) {
                self.last_blowout_ratio = sm.blowout_ratio;
            }
            self.analytics_stats.push(t1.elapsed().as_nanos() as u64);

            // ── risk: probe with a zero-quantity intent — exercises every guard
            // without polluting position. Real intents are pushed by the
            // strategy crate; here we just sample the breaker latency.
            let t2 = Instant::now();
            // No probe call yet — keeps risk_stats honest as "no-op overhead".
            self.risk_stats.push(t2.elapsed().as_nanos() as u64);

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
                let sym = self
                    .tracked
                    .and_then(|id| source.symbol_of(id).map(str::to_string))
                    .unwrap_or_else(|| "<none>".into());
                tracing::debug!(
                    tracked = ?self.tracked,
                    sym = %sym,
                    eps = self.last_eps,
                    trd_per_s = format!("{:.1}", self.last_trade_vel),
                    bb_ticks = bb,
                    ba_ticks = ba,
                    imb = format!("{:.3}", self.last_imbalance),
                    vpin = format!("{:.3}", self.last_vpin),
                    seen_syms = self.instrument_counts.len(),
                    "1s tick"
                );
                self.events_in_window = 0;
                self.trades_in_window = 0;
                self.window_started = now;
                next_eps = now + Duration::from_secs(1);
            }

            if now >= next_tick {
                let frame = self.build_tick(&ev, out.dropped_total());
                let t3 = Instant::now();
                // Skip the tick if the book isn't yet two-sided — emitting
                // mid=0 pollutes the MID spark and ruins the scale.
                if frame.mid_ticks > 0 {
                    out.try_send(TelemetryFrame::Tick(frame));
                }
                self.wire_stats.push(t3.elapsed().as_nanos() as u64);

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
        let regime_input = RegimeInput {
            spread_blowout_ratio: self.last_blowout_ratio,
            book_imbalance: self.last_imbalance,
            vpin: self.last_vpin,
            trade_velocity_ratio: velocity_ratio,
            depth_depletion: 0.0,
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
