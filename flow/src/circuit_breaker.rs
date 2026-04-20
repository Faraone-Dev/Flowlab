//! Trading-side risk gate: composable pre-trade + in-flight checks.
//!
//! The circuit breaker is the last line of defense before any order
//! reaches the wire. All paths that can generate an outbound order
//! MUST call [`CircuitBreaker::check`] with a prospective [`Intent`]
//! and respect its [`Decision`].
//!
//! Guards implemented:
//!
//! | Guard             | Trigger                                                          |
//! | ----------------- | ---------------------------------------------------------------- |
//! | Rate limit        | Token bucket (orders/sec)                                        |
//! | Position cap      | `abs(net_pos(instrument)) + order_qty > max_position`            |
//! | Daily-loss floor  | `realized_pnl_cents < -max_daily_loss_cents`                     |
//! | OTR ceiling       | `orders_sent / max(1, trades_done) > max_order_to_trade_ratio`   |
//! | Feed health       | `recent_gap_count >= gap_threshold` within the rolling window    |
//! | Manual kill       | Atomic latch flipped by operator / supervisor                    |
//!
//! Once any guard trips, the breaker **latches**: every subsequent
//! [`CircuitBreaker::check`] returns `Decision::Block(reason)` until
//! [`CircuitBreaker::reset`] is called explicitly. This is the HFT
//! equivalent of "fail closed" — recovery is a deliberate operator
//! action, never automatic.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use std::collections::HashMap;

/// Reason a breaker is tripped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HaltReason {
    RateLimit,
    PositionCap,
    DailyLossFloor,
    OrderToTradeRatio,
    FeedGap,
    Manual,
}

/// Outcome of a pre-trade check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Block(HaltReason),
}

/// Side of a prospective order, in the same orientation as
/// [`flowlab_core::event::Side`] (0 = bid/buy, 1 = ask/sell).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}

/// Minimal description of a prospective outbound order.
#[derive(Debug, Clone, Copy)]
pub struct Intent {
    pub instrument_id: u32,
    pub side: Side,
    pub qty: u32,
    /// Limit price in integer ticks (e.g. 1/10000 dollars).
    pub price_ticks: u64,
}

/// Observed execution report used to update PnL / position state.
#[derive(Debug, Clone, Copy)]
pub struct Fill {
    pub instrument_id: u32,
    pub side: Side,
    pub qty: u32,
    pub price_ticks: u64,
}

/// Static configuration for the breaker. Fields left `None` disable
/// the corresponding guard.
#[derive(Debug, Clone)]
pub struct BreakerConfig {
    pub max_orders_per_sec: Option<u32>,
    pub max_position: Option<i64>,
    /// Loss floor expressed in the *same units as Fill.price_ticks*,
    /// i.e. cumulative `sum(signed_qty * price_ticks)`. Trip when the
    /// signed total falls below `-this_value`. `None` disables.
    pub max_daily_loss_ticks: Option<i64>,
    /// Max allowed `orders / max(1, trades)` ratio once
    /// `orders_sent >= otr_warmup`. `None` disables.
    pub max_order_to_trade_ratio: Option<f64>,
    pub otr_warmup: u64,
    /// Max allowed gap events within the last `gap_window` duration.
    pub gap_threshold: Option<u32>,
    pub gap_window: Duration,
}

impl Default for BreakerConfig {
    fn default() -> Self {
        Self {
            max_orders_per_sec: Some(5_000),
            max_position: Some(10_000),
            max_daily_loss_ticks: Some(5_000_000_000),
            max_order_to_trade_ratio: Some(100.0),
            otr_warmup: 100,
            gap_threshold: Some(16),
            gap_window: Duration::from_secs(60),
        }
    }
}

/// Thread-safe circuit breaker.
pub struct CircuitBreaker {
    cfg: BreakerConfig,
    halted: AtomicBool,
    reason: Mutex<Option<HaltReason>>,
    orders_sent: AtomicU64,
    trades_done: AtomicU64,
    /// Signed cumulative `sum(side_sign * qty * price_ticks)` where
    /// buy = -1 (cash out) and sell = +1 (cash in). When positive,
    /// you've realised profit.
    cash_flow_ticks: Mutex<i64>,
    positions: Mutex<HashMap<u32, i64>>,
    rate: Mutex<TokenBucket>,
    gap_log: Mutex<Vec<Instant>>,
}

impl CircuitBreaker {
    pub fn new(cfg: BreakerConfig) -> Self {
        let burst = cfg.max_orders_per_sec.unwrap_or(u32::MAX);
        Self {
            halted: AtomicBool::new(false),
            reason: Mutex::new(None),
            orders_sent: AtomicU64::new(0),
            trades_done: AtomicU64::new(0),
            cash_flow_ticks: Mutex::new(0),
            positions: Mutex::new(HashMap::new()),
            rate: Mutex::new(TokenBucket::new(burst as f64)),
            gap_log: Mutex::new(Vec::new()),
            cfg,
        }
    }

    pub fn is_halted(&self) -> bool {
        self.halted.load(Ordering::Acquire)
    }

    pub fn halt_reason(&self) -> Option<HaltReason> {
        *self.reason.lock().unwrap()
    }

    /// Fail-closed pre-trade check. Call before every outbound order.
    pub fn check(&self, intent: &Intent) -> Decision {
        if let Some(r) = self.halt_reason() {
            return Decision::Block(r);
        }

        // 1) Rate limit.
        if let Some(rps) = self.cfg.max_orders_per_sec {
            let mut bucket = self.rate.lock().unwrap();
            if !bucket.try_take(1.0, rps as f64) {
                drop(bucket);
                return self.latch(HaltReason::RateLimit);
            }
        }

        // 2) Position cap (account for the new order as if fully filled).
        if let Some(max_pos) = self.cfg.max_position {
            let positions = self.positions.lock().unwrap();
            let current = positions.get(&intent.instrument_id).copied().unwrap_or(0);
            let delta = match intent.side {
                Side::Buy => intent.qty as i64,
                Side::Sell => -(intent.qty as i64),
            };
            if (current + delta).abs() > max_pos {
                drop(positions);
                return self.latch(HaltReason::PositionCap);
            }
        }

        // 3) Daily loss floor (evaluate PnL *before* sending more).
        if let Some(floor) = self.cfg.max_daily_loss_ticks {
            let cash = *self.cash_flow_ticks.lock().unwrap();
            if cash < -floor {
                return self.latch(HaltReason::DailyLossFloor);
            }
        }

        // 4) Order-to-trade ratio (after warmup).
        if let Some(max_otr) = self.cfg.max_order_to_trade_ratio {
            let orders = self.orders_sent.load(Ordering::Relaxed);
            if orders >= self.cfg.otr_warmup {
                let trades = self.trades_done.load(Ordering::Relaxed).max(1);
                if (orders as f64 / trades as f64) > max_otr {
                    return self.latch(HaltReason::OrderToTradeRatio);
                }
            }
        }

        // Record the order as "sent" for OTR tracking purposes.
        self.orders_sent.fetch_add(1, Ordering::Relaxed);
        Decision::Allow
    }

    /// Report an execution report observed from the venue.
    pub fn record_fill(&self, fill: &Fill) {
        self.trades_done.fetch_add(1, Ordering::Relaxed);

        let signed_qty: i64 = match fill.side {
            Side::Buy => fill.qty as i64,
            Side::Sell => -(fill.qty as i64),
        };
        // Position update.
        {
            let mut positions = self.positions.lock().unwrap();
            let entry = positions.entry(fill.instrument_id).or_insert(0);
            *entry += signed_qty;
        }
        // Cash flow update (buy: cash out = negative; sell: cash in = positive).
        {
            let mut cash = self.cash_flow_ticks.lock().unwrap();
            *cash += -signed_qty * fill.price_ticks as i64;
        }
    }

    /// Report a feed-level gap event. Tripping is delayed until the
    /// rolling window exceeds `cfg.gap_threshold`.
    pub fn record_gap(&self) {
        let Some(threshold) = self.cfg.gap_threshold else {
            return;
        };
        let now = Instant::now();
        let mut log = self.gap_log.lock().unwrap();
        let cutoff = now - self.cfg.gap_window;
        log.retain(|t| *t >= cutoff);
        log.push(now);
        if log.len() >= threshold as usize {
            drop(log);
            let _ = self.latch(HaltReason::FeedGap);
        }
    }

    /// Operator manual halt. Latches immediately.
    pub fn manual_halt(&self) {
        let _ = self.latch(HaltReason::Manual);
    }

    /// Explicit recovery: clears the halted state. Stats are NOT reset —
    /// that's a separate `start_of_day` concern.
    pub fn reset(&self) {
        *self.reason.lock().unwrap() = None;
        self.halted.store(false, Ordering::Release);
    }

    /// Resets all per-day accumulators. Call at session boundary.
    pub fn start_of_day(&self) {
        self.orders_sent.store(0, Ordering::Relaxed);
        self.trades_done.store(0, Ordering::Relaxed);
        *self.cash_flow_ticks.lock().unwrap() = 0;
        self.positions.lock().unwrap().clear();
        self.gap_log.lock().unwrap().clear();
        self.reset();
    }

    fn latch(&self, reason: HaltReason) -> Decision {
        let mut slot = self.reason.lock().unwrap();
        if slot.is_none() {
            *slot = Some(reason);
            self.halted.store(true, Ordering::Release);
        }
        Decision::Block(slot.unwrap())
    }
}

/// Simple token bucket, evaluated lazily on each `try_take`.
struct TokenBucket {
    tokens: f64,
    last: Instant,
    capacity: f64,
}

impl TokenBucket {
    fn new(capacity: f64) -> Self {
        Self {
            tokens: capacity,
            last: Instant::now(),
            capacity,
        }
    }

    fn try_take(&mut self, n: f64, rate_per_sec: f64) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * rate_per_sec).min(self.capacity);
        if self.tokens >= n {
            self.tokens -= n;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(side: Side, qty: u32) -> Intent {
        Intent {
            instrument_id: 1,
            side,
            qty,
            price_ticks: 100_0000, // $100.0000
        }
    }

    #[test]
    fn default_allows_normal_order() {
        let cb = CircuitBreaker::new(BreakerConfig::default());
        assert_eq!(cb.check(&intent(Side::Buy, 10)), Decision::Allow);
        assert!(!cb.is_halted());
    }

    #[test]
    fn rate_limit_trips_and_latches() {
        let cb = CircuitBreaker::new(BreakerConfig {
            max_orders_per_sec: Some(5),
            ..BreakerConfig::default()
        });
        // First 5 pass — bucket full.
        for _ in 0..5 {
            assert_eq!(cb.check(&intent(Side::Buy, 1)), Decision::Allow);
        }
        assert_eq!(
            cb.check(&intent(Side::Buy, 1)),
            Decision::Block(HaltReason::RateLimit)
        );
        // Latched.
        assert!(cb.is_halted());
        assert_eq!(
            cb.check(&intent(Side::Sell, 1)),
            Decision::Block(HaltReason::RateLimit)
        );
    }

    #[test]
    fn position_cap_rejects_overshoot() {
        let cb = CircuitBreaker::new(BreakerConfig {
            max_position: Some(100),
            max_orders_per_sec: None,
            max_daily_loss_ticks: None,
            max_order_to_trade_ratio: None,
            gap_threshold: None,
            ..BreakerConfig::default()
        });
        // Simulate 90 existing long via fills.
        cb.record_fill(&Fill {
            instrument_id: 1,
            side: Side::Buy,
            qty: 90,
            price_ticks: 100,
        });
        // A further +20 would push us to 110 → block.
        assert_eq!(
            cb.check(&intent(Side::Buy, 20)),
            Decision::Block(HaltReason::PositionCap)
        );
        assert!(cb.is_halted());
    }

    #[test]
    fn daily_loss_floor_blocks_after_tripping() {
        let cb = CircuitBreaker::new(BreakerConfig {
            max_position: None,
            max_orders_per_sec: None,
            max_daily_loss_ticks: Some(1_000),
            max_order_to_trade_ratio: None,
            gap_threshold: None,
            ..BreakerConfig::default()
        });
        // Book a losing round-trip: buy 10 @ 200, sell 10 @ 100 → cash -1000.
        cb.record_fill(&Fill {
            instrument_id: 1,
            side: Side::Buy,
            qty: 10,
            price_ticks: 200,
        });
        cb.record_fill(&Fill {
            instrument_id: 1,
            side: Side::Sell,
            qty: 10,
            price_ticks: 100,
        });
        // cash = -10*200 + 10*100 = -1000 (not below floor -1000)
        assert_eq!(cb.check(&intent(Side::Buy, 1)), Decision::Allow);
        // One more losing buy → -1200 < -1000 after next fill.
        cb.record_fill(&Fill {
            instrument_id: 1,
            side: Side::Buy,
            qty: 2,
            price_ticks: 100,
        });
        assert_eq!(
            cb.check(&intent(Side::Buy, 1)),
            Decision::Block(HaltReason::DailyLossFloor)
        );
    }

    #[test]
    fn otr_trips_after_warmup() {
        let cb = CircuitBreaker::new(BreakerConfig {
            max_position: None,
            max_orders_per_sec: None,
            max_daily_loss_ticks: None,
            max_order_to_trade_ratio: Some(2.0),
            otr_warmup: 5,
            gap_threshold: None,
            ..BreakerConfig::default()
        });
        // Send 10 orders, 1 fill → ratio 10 > 2 → trip (after warmup=5).
        for _ in 0..5 {
            assert_eq!(cb.check(&intent(Side::Buy, 1)), Decision::Allow);
        }
        cb.record_fill(&Fill {
            instrument_id: 1,
            side: Side::Buy,
            qty: 1,
            price_ticks: 1,
        });
        // Ratio now 5/1 = 5 > 2 → next order blocks.
        assert_eq!(
            cb.check(&intent(Side::Buy, 1)),
            Decision::Block(HaltReason::OrderToTradeRatio)
        );
    }

    #[test]
    fn feed_gap_trips_after_threshold() {
        let cb = CircuitBreaker::new(BreakerConfig {
            max_position: None,
            max_orders_per_sec: None,
            max_daily_loss_ticks: None,
            max_order_to_trade_ratio: None,
            gap_threshold: Some(3),
            gap_window: Duration::from_secs(60),
            ..BreakerConfig::default()
        });
        cb.record_gap();
        assert!(!cb.is_halted());
        cb.record_gap();
        assert!(!cb.is_halted());
        cb.record_gap();
        assert!(cb.is_halted());
        assert_eq!(
            cb.check(&intent(Side::Buy, 1)),
            Decision::Block(HaltReason::FeedGap)
        );
    }

    #[test]
    fn manual_halt_and_reset() {
        let cb = CircuitBreaker::new(BreakerConfig::default());
        cb.manual_halt();
        assert_eq!(
            cb.check(&intent(Side::Buy, 1)),
            Decision::Block(HaltReason::Manual)
        );
        cb.reset();
        assert_eq!(cb.check(&intent(Side::Buy, 1)), Decision::Allow);
    }

    #[test]
    fn start_of_day_clears_all_state() {
        let cb = CircuitBreaker::new(BreakerConfig::default());
        cb.record_fill(&Fill {
            instrument_id: 1,
            side: Side::Buy,
            qty: 50,
            price_ticks: 100,
        });
        cb.manual_halt();
        cb.start_of_day();
        assert!(!cb.is_halted());
        assert_eq!(cb.halt_reason(), None);
    }
}
