
use crate::config::AppCfg;
use crate::types::{Px, Qty, Side};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::time::Instant;
use tracing::{info, warn};
#[derive(Clone, Debug)]
pub struct OrderInfo {
    pub order_id: String,
    pub side: Side,
    pub price: Px,
    pub qty: Qty,
    pub created_at: Instant,
}
pub fn analyze_orders(
    active_orders: &HashMap<String, OrderInfo>,
    bid: Px,
    ask: Px,
    position_size_notional: f64,
    cfg: &AppCfg,
    max_order_age_ms: u64,
) -> Vec<String> {
    let mut orders_to_cancel = Vec::new();
    for (order_id, order) in active_orders {
        let order_price_f64 = match order.price.0.to_f64() {
            Some(price) => price,
            None => {
                warn!(
                    order_id = %order_id,
                    price_decimal = %order.price.0,
                    "Failed to convert order price to f64"
                );
                continue;
            }
        };
        let order_age_ms = order.created_at.elapsed().as_millis() as u64;
        let market_distance_pct = match order.side {
            Side::Buy => {
                let ask_f64 = ask.0.to_f64().unwrap_or(0.0);
                if ask_f64 > 0.0 {
                    (ask_f64 - order_price_f64) / ask_f64
                } else {
                    0.0
                }
            }
            Side::Sell => {
                let bid_f64 = bid.0.to_f64().unwrap_or(0.0);
                if bid_f64 > 0.0 {
                    (order_price_f64 - bid_f64) / bid_f64
                } else {
                    0.0
                }
            }
        };
        let max_distance_pct = if position_size_notional > 0.0 {
            0.01
        } else {
            0.02
        };
        let should_cancel_far = market_distance_pct.abs() > max_distance_pct;
        let max_age_for_stale = if position_size_notional > 0.0 {
            (max_order_age_ms * 2) / 3
        } else {
            max_order_age_ms
        };
        let should_cancel_stale = order_age_ms > max_age_for_stale;
        if should_cancel_far || should_cancel_stale {
            orders_to_cancel.push(order_id.clone());
            info!(
                order_id = %order_id,
                side = ?order.side,
                order_price = %order.price.0,
                market_distance_pct = market_distance_pct * 100.0,
                order_age_ms,
                reason = if should_cancel_far { "too_far_from_market" } else { "stale" },
                "ORDER_ANALYSIS: Order should be canceled"
            );
        }
    }
    orders_to_cancel
}
#[derive(Debug, Clone)]
pub struct FillRateState {
    pub fill_rate: f64,
    pub consecutive_no_fills: u32,
    pub last_fill_time: Option<Instant>,
    pub last_decay_check: Option<Instant>,
    pub last_decay_period: Option<u64>,
}
impl FillRateState {
    pub fn new() -> Self {
        Self {
            fill_rate: 0.5,
            consecutive_no_fills: 0,
            last_fill_time: None,
            last_decay_check: None,
            last_decay_period: None,
        }
    }
}
pub fn update_fill_rate_on_fill(
    state: &mut FillRateState,
    increase_factor: f64,
    increase_bonus: f64,
) {
    state.fill_rate = (state.fill_rate * increase_factor + increase_bonus).min(1.0);
    state.consecutive_no_fills = 0;
    state.last_fill_time = Some(Instant::now());
}
pub fn update_fill_rate_on_cancel(
    state: &mut FillRateState,
    decrease_factor: f64,
) {
    state.fill_rate = (state.fill_rate * decrease_factor).max(0.0);
    state.consecutive_no_fills += 1;
}
pub fn apply_fill_rate_decay(
    state: &mut FillRateState,
    decay_factor: f64,
    decay_interval_sec: u64,
) {
    let now = Instant::now();
    let should_decay = state.last_decay_check
        .map(|last| now.duration_since(last).as_secs() >= decay_interval_sec)
        .unwrap_or(true);
    if should_decay {
        state.fill_rate = (state.fill_rate * decay_factor).max(0.0);
        state.last_decay_check = Some(now);
        if let Some(last_period) = state.last_decay_period {
            if last_period != decay_interval_sec {
                state.last_decay_period = Some(decay_interval_sec);
            }
        } else {
            state.last_decay_period = Some(decay_interval_sec);
        }
    }
}
pub fn should_sync_orders(
    last_sync: Option<Instant>,
    sync_interval_sec: u64,
) -> bool {
    last_sync
        .map(|last| last.elapsed().as_secs() >= sync_interval_sec)
        .unwrap_or(true)
}
pub fn calculate_fill_rate_from_history(
    filled_orders: usize,
    total_orders: usize,
    time_window_sec: u64,
) -> f64 {
    if total_orders == 0 {
        return 0.5;
    }
    let fill_rate = filled_orders as f64 / total_orders as f64;
    fill_rate.max(0.0).min(1.0)
}
#[derive(Debug, Clone)]
pub struct OrderSyncResult {
    pub new_orders: Vec<String>,
    pub removed_orders: Vec<String>,
    pub unchanged_orders: Vec<String>,
}
impl OrderSyncResult {
    pub fn new() -> Self {
        Self {
            new_orders: Vec::new(),
            removed_orders: Vec::new(),
            unchanged_orders: Vec::new(),
        }
    }
}
pub fn compare_orders(
    local_order_ids: &std::collections::HashSet<String>,
    api_order_ids: &std::collections::HashSet<String>,
) -> OrderSyncResult {
    let mut result = OrderSyncResult::new();
    for order_id in api_order_ids {
        if !local_order_ids.contains(order_id) {
            result.new_orders.push(order_id.clone());
        } else {
            result.unchanged_orders.push(order_id.clone());
        }
    }
    for order_id in local_order_ids {
        if !api_order_ids.contains(order_id) {
            result.removed_orders.push(order_id.clone());
        }
    }
    result
}
pub fn validate_order(
    price: Px,
    qty: Qty,
    bid: Px,
    ask: Px,
    side: Side,
    min_notional: Option<Decimal>,
) -> Result<(), String> {
    if price.0 <= Decimal::ZERO {
        return Err("Order price must be positive".to_string());
    }
    if qty.0 <= Decimal::ZERO {
        return Err("Order quantity must be positive".to_string());
    }
    let market_price = match side {
        Side::Buy => ask.0,
        Side::Sell => bid.0,
    };
    if market_price <= Decimal::ZERO {
        return Err("Market price is invalid".to_string());
    }
    if let Some(min_not) = min_notional {
        let notional = price.0 * qty.0;
        if notional < min_not {
            return Err(format!(
                "Order notional {} is below minimum {}",
                notional, min_not
            ));
        }
    }
    Ok(())
}
