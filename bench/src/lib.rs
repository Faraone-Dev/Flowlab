// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

//! Synthetic market data generators for benchmarking.
//!
//! Models real microstructure properties:
//!   - Geometric price decay around mid (top-of-book concentration)
//!   - Consistent order_id tracking (add/cancel/trade reference same order)
//!   - Realistic event-type mix (~60% add, ~25% cancel, ~15% trade)

use flowlab_core::event::{Event, EventType, SequencedEvent};

/// ITCH parser lives in `flowlab-replay` so that non-bench consumers
/// (file replay, live feed handlers) can share the exact same code.
/// Re-exported here for benches and existing callers.
pub use flowlab_replay::itch;

/// Deterministic xorshift64 — reproducible benchmarks, no `rand` dep.
#[derive(Clone)]
pub struct XorShift64(pub u64);

impl XorShift64 {
    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    #[inline]
    pub fn next_bounded(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    /// Geometric: concentrates mass near 0 (top-of-book).
    #[inline]
    pub fn geometric(&mut self, p_num: u64, p_den: u64) -> u64 {
        let mut k: u64 = 0;
        while k < 64 && self.next_bounded(p_den) >= p_num {
            k += 1;
        }
        k
    }
}

pub struct MarketConfig {
    pub mid_price: u64,
    pub tick_size: u64,
    pub max_levels: u64,
    pub top_concentration: u64,
    pub seed: u64,
}

impl Default for MarketConfig {
    fn default() -> Self {
        Self {
            mid_price: 10_000,
            tick_size: 1,
            max_levels: 50,
            top_concentration: 40,
            seed: 0xDEAD_BEEF_CAFE_BABE,
        }
    }
}

pub fn realistic_events(n: usize, cfg: &MarketConfig) -> Vec<SequencedEvent> {
    let mut rng = XorShift64(cfg.seed);
    let mut out = Vec::with_capacity(n);
    let mut live: Vec<(u64, u64, u64, u8)> = Vec::with_capacity(n / 2);
    let mut next_order_id: u64 = 1;

    for i in 0..n {
        let roll = rng.next_bounded(100);
        let etype = if roll < 60 || live.is_empty() {
            EventType::OrderAdd
        } else if roll < 85 {
            EventType::OrderCancel
        } else {
            EventType::Trade
        };

        let event = match etype {
            EventType::OrderAdd => {
                let level_offset = rng.geometric(cfg.top_concentration, 100).min(cfg.max_levels - 1);
                let side_bit = (rng.next_u64() & 1) as u8;
                let price = if side_bit == 0 {
                    cfg.mid_price.saturating_sub(level_offset * cfg.tick_size).max(1)
                } else {
                    cfg.mid_price + (level_offset + 1) * cfg.tick_size
                };
                let qty_base = 10 + rng.next_bounded(90);
                let qty = if rng.next_bounded(100) < 5 {
                    qty_base * (10 + rng.next_bounded(50))
                } else {
                    qty_base
                };
                let oid = next_order_id;
                next_order_id += 1;
                live.push((oid, price, qty, side_bit));

                Event {
                    ts: (i as u64) * 1_000,
                    instrument_id: 1,
                    event_type: EventType::OrderAdd as u8,
                    side: side_bit,
                    price,
                    qty,
                    order_id: oid,
                    _pad: [0; 2],
                }
            }
            EventType::OrderCancel => {
                let bias = rng.next_bounded(100) < 70;
                let idx = if bias && live.len() > 4 {
                    live.len() - 1 - (rng.next_bounded(4) as usize)
                } else {
                    rng.next_bounded(live.len() as u64) as usize
                };
                let (oid, price, qty, side) = live.swap_remove(idx);
                Event {
                    ts: (i as u64) * 1_000,
                    instrument_id: 1,
                    event_type: EventType::OrderCancel as u8,
                    side,
                    price,
                    qty,
                    order_id: oid,
                    _pad: [0; 2],
                }
            }
            EventType::Trade => {
                let idx = rng.next_bounded(live.len() as u64) as usize;
                let (oid, price, qty, side) = live[idx];
                let fill = if rng.next_bounded(100) < 30 { qty } else { (qty / 2).max(1) };
                if fill >= qty {
                    live.swap_remove(idx);
                } else {
                    live[idx].2 -= fill;
                }
                Event {
                    ts: (i as u64) * 1_000,
                    instrument_id: 1,
                    event_type: EventType::Trade as u8,
                    side,
                    price,
                    qty: fill,
                    order_id: oid,
                    _pad: [0; 2],
                }
            }
            _ => unreachable!(),
        };

        out.push(SequencedEvent {
            seq: (i as u64) + 1,
            channel_id: 0,
            event,
        });
    }

    out
}

/// **Bursty** stream: like `realistic_events` but with the cancel
/// share inflated to ~55 % and the trade share to ~10 %. Designed to
/// stress the cancel-rate baseline (`CancellationStormDetector`) and
/// the phantom-cycle path. Best moves often → exercises the
/// `flash_crash` band-cache invalidation path.
pub fn bursty_events(n: usize, cfg: &MarketConfig) -> Vec<SequencedEvent> {
    let mut rng = XorShift64(cfg.seed.wrapping_add(0x42));
    let mut out = Vec::with_capacity(n);
    let mut live: Vec<(u64, u64, u64, u8)> = Vec::with_capacity(n / 2);
    let mut next_order_id: u64 = 1;

    for i in 0..n {
        let roll = rng.next_bounded(100);
        // 35 % adds, 55 % cancels, 10 % trades (flipped vs realistic).
        let etype = if roll < 35 || live.is_empty() {
            EventType::OrderAdd
        } else if roll < 90 {
            EventType::OrderCancel
        } else {
            EventType::Trade
        };

        let event = match etype {
            EventType::OrderAdd => {
                let level_offset = rng.geometric(cfg.top_concentration, 100).min(cfg.max_levels - 1);
                let side_bit = (rng.next_u64() & 1) as u8;
                let price = if side_bit == 0 {
                    cfg.mid_price.saturating_sub(level_offset * cfg.tick_size).max(1)
                } else {
                    cfg.mid_price + (level_offset + 1) * cfg.tick_size
                };
                let qty = 10 + rng.next_bounded(90);
                let oid = next_order_id;
                next_order_id += 1;
                live.push((oid, price, qty, side_bit));
                Event {
                    ts: (i as u64) * 1_000,
                    instrument_id: 1,
                    event_type: EventType::OrderAdd as u8,
                    side: side_bit,
                    price,
                    qty,
                    order_id: oid,
                    _pad: [0; 2],
                }
            }
            EventType::OrderCancel => {
                let idx = rng.next_bounded(live.len() as u64) as usize;
                let (oid, price, qty, side) = live.swap_remove(idx);
                Event {
                    ts: (i as u64) * 1_000,
                    instrument_id: 1,
                    event_type: EventType::OrderCancel as u8,
                    side,
                    price,
                    qty,
                    order_id: oid,
                    _pad: [0; 2],
                }
            }
            EventType::Trade => {
                let idx = rng.next_bounded(live.len() as u64) as usize;
                let (oid, price, qty, side) = live[idx];
                let fill = (qty / 2).max(1);
                if fill >= qty {
                    live.swap_remove(idx);
                } else {
                    live[idx].2 -= fill;
                }
                Event {
                    ts: (i as u64) * 1_000,
                    instrument_id: 1,
                    event_type: EventType::Trade as u8,
                    side,
                    price,
                    qty: fill,
                    order_id: oid,
                    _pad: [0; 2],
                }
            }
            _ => unreachable!(),
        };

        out.push(SequencedEvent {
            seq: (i as u64) + 1,
            channel_id: 0,
            event,
        });
    }
    out
}

/// **Crashy** stream: a `realistic_events` body interrupted every
/// ~5 000 events by a 50-event sweep that wipes the top of one side
/// and closes with a directional trade burst. Designed to actually
/// trigger `FlashCrashDetector` and `MomentumIgnitionDetector` so
/// the `Some(flag)` branch of every detector is exercised under
/// measurement (not just the no-flag fast path).
pub fn crashy_events(n: usize, cfg: &MarketConfig) -> Vec<SequencedEvent> {
    let base = realistic_events(n, cfg);
    let mut out = Vec::with_capacity(n);
    let mut rng = XorShift64(cfg.seed.wrapping_add(0xC0DE));
    let mut seq: u64 = 1;
    let mut ts: u64 = 0;
    let crash_every: usize = 5_000;
    let burst_size: usize = 50;
    let mut next_order_id: u64 = 10_000_000;

    for (i, src) in base.into_iter().enumerate() {
        let mut e = src.event;
        // Reseq + retime to keep the stream monotone after injection.
        e.ts = ts;
        let s = seq;
        seq += 1;
        ts = ts.wrapping_add(1_000);
        out.push(SequencedEvent {
            seq: s,
            channel_id: 0,
            event: e,
        });

        if i > 0 && i % crash_every == 0 {
            // Pick a side to crush.
            let side = if (rng.next_u64() & 1) == 0 { 0u8 } else { 1u8 };
            let mid = cfg.mid_price;
            for k in 0..burst_size {
                // Sweep cancels from top inward.
                let off = (k as u64) + 1;
                let price = if side == 0 { mid.saturating_sub(off) } else { mid.saturating_add(off) };
                let oid = next_order_id;
                next_order_id += 1;
                let s = seq;
                seq += 1;
                ts = ts.wrapping_add(1_000);
                out.push(SequencedEvent {
                    seq: s,
                    channel_id: 0,
                    event: Event {
                        ts,
                        instrument_id: 1,
                        event_type: EventType::OrderCancel as u8,
                        side,
                        price,
                        qty: 100,
                        order_id: oid,
                        _pad: [0; 2],
                    },
                });
            }
            // Closing aggression in the same direction.
            for _ in 0..8 {
                let s = seq;
                seq += 1;
                ts = ts.wrapping_add(1_000);
                out.push(SequencedEvent {
                    seq: s,
                    channel_id: 0,
                    event: Event {
                        ts,
                        instrument_id: 1,
                        event_type: EventType::Trade as u8,
                        side,
                        price: if side == 0 {
                            mid.saturating_sub(burst_size as u64 + 1)
                        } else {
                            mid.saturating_add(burst_size as u64 + 1)
                        },
                        qty: 50,
                        order_id: 0,
                        _pad: [0; 2],
                    },
                });
            }
        }
    }
    out
}

/// Pre-warm book events (all OrderAdds) for steady-state measurement.
pub fn warmup_events(n: usize, cfg: &MarketConfig) -> Vec<SequencedEvent> {
    let mut rng = XorShift64(cfg.seed.wrapping_add(1));
    let mut out = Vec::with_capacity(n);

    for i in 0..n {
        let level_offset = rng.geometric(cfg.top_concentration, 100).min(cfg.max_levels - 1);
        let side_bit = (rng.next_u64() & 1) as u8;
        let price = if side_bit == 0 {
            cfg.mid_price.saturating_sub(level_offset * cfg.tick_size).max(1)
        } else {
            cfg.mid_price + (level_offset + 1) * cfg.tick_size
        };

        out.push(SequencedEvent {
            seq: (i as u64) + 1,
            channel_id: 0,
            event: Event {
                ts: (i as u64) * 1_000,
                instrument_id: 1,
                event_type: EventType::OrderAdd as u8,
                side: side_bit,
                price,
                qty: 10 + rng.next_bounded(90),
                order_id: (i as u64) + 1,
                _pad: [0; 2],
            },
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generator_is_deterministic() {
        let cfg = MarketConfig::default();
        let a = realistic_events(1000, &cfg);
        let b = realistic_events(1000, &cfg);
        assert_eq!(a.len(), b.len());
        for (ea, eb) in a.iter().zip(b.iter()) {
            // Copy out of packed struct before comparing
            let (pa, pb) = (ea.event.price, eb.event.price);
            let (oa, ob) = (ea.event.order_id, eb.event.order_id);
            assert_eq!(ea.seq, eb.seq);
            assert_eq!(pa, pb);
            assert_eq!(oa, ob);
        }
    }

    #[test]
    fn top_of_book_is_concentrated() {
        let cfg = MarketConfig::default();
        let events = realistic_events(10_000, &cfg);
        let mut near = 0usize;
        for e in &events {
            if e.event.event_type == EventType::OrderAdd as u8 {
                let price = e.event.price; // copy out
                let dist = (price as i64 - cfg.mid_price as i64).unsigned_abs();
                if dist <= 3 {
                    near += 1;
                }
            }
        }
        assert!(near > 2000, "expected concentrated top-of-book, got {near}");
    }
}
