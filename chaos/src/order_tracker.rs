use std::collections::HashMap;

use flowlab_core::event::Event;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggressorSide {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy)]
pub struct ResolvedBookUpdate {
    pub side: u8,
    pub price: u64,
    pub qty: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct ResolvedTrade {
    pub update: ResolvedBookUpdate,
    pub aggressor: AggressorSide,
}

#[derive(Debug, Clone, Copy)]
struct OrderMeta {
    side: u8,
    price: u64,
    qty: u64,
}

#[derive(Default)]
pub struct OrderTracker {
    orders: HashMap<u64, OrderMeta>,
}

impl OrderTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on_add(&mut self, event: &Event) -> Option<ResolvedBookUpdate> {
        let update = anonymous_update(event.side, event.price, event.qty)?;
        if event.order_id != 0 {
            self.orders.insert(
                event.order_id,
                OrderMeta {
                    side: update.side,
                    price: update.price,
                    qty: update.qty,
                },
            );
        }
        Some(update)
    }

    pub fn on_cancel(&mut self, event: &Event) -> Option<ResolvedBookUpdate> {
        if event.order_id != 0 {
            if let Some(meta) = self.orders.get_mut(&event.order_id) {
                let qty = resolved_qty(meta.qty, event.qty);
                let update = ResolvedBookUpdate {
                    side: meta.side,
                    price: meta.price,
                    qty,
                };
                if qty >= meta.qty {
                    self.orders.remove(&event.order_id);
                } else {
                    meta.qty -= qty;
                }
                return Some(update);
            }
            if event.price == 0 {
                return None;
            }
        }
        anonymous_update(event.side, event.price, event.qty)
    }

    pub fn on_trade(&mut self, event: &Event) -> Option<ResolvedTrade> {
        if event.order_id != 0 {
            if let Some(meta) = self.orders.get_mut(&event.order_id) {
                let qty = resolved_qty(meta.qty, event.qty);
                let update = ResolvedBookUpdate {
                    side: meta.side,
                    price: if event.price != 0 { event.price } else { meta.price },
                    qty,
                };
                let aggressor = aggressor_from_resting_side(meta.side)?;
                if qty >= meta.qty {
                    self.orders.remove(&event.order_id);
                } else {
                    meta.qty -= qty;
                }
                return Some(ResolvedTrade { update, aggressor });
            }
            if event.price == 0 {
                return None;
            }
        }

        let update = anonymous_update(event.side, event.price, event.qty)?;
        Some(ResolvedTrade {
            update,
            aggressor: aggressor_from_resting_side(update.side)?,
        })
    }
}

fn anonymous_update(side: u8, price: u64, qty: u64) -> Option<ResolvedBookUpdate> {
    match side {
        0 | 1 => Some(ResolvedBookUpdate { side, price, qty }),
        _ => None,
    }
}

fn aggressor_from_resting_side(side: u8) -> Option<AggressorSide> {
    match side {
        0 => Some(AggressorSide::Sell),
        1 => Some(AggressorSide::Buy),
        _ => None,
    }
}

fn resolved_qty(known_qty: u64, event_qty: u64) -> u64 {
    if event_qty == 0 || event_qty >= known_qty {
        known_qty
    } else {
        event_qty
    }
}