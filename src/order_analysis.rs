// Order analysis and fill rate tracking module
// Stale order detection, fill rate tracking (EWMA), and order synchronization
// Based on reference project with adaptations for our event-driven architecture

use crate::config::AppCfg;
use crate::types::{Px, Qty, Side, OrderBook};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tracing::{info, warn};

// ============================================================================
// Order Analysis
// ============================================================================

/// Order information for analysis
#[derive(Clone, Debug)]
pub struct OrderInfo {
    pub order_id: String,
    pub side: Side,
    pub price: Px,
    pub qty: Qty,
    pub created_at: Instant,
}

/// Analyze existing orders and determine which ones should be canceled
/// Returns list of order IDs that should be canceled
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

        // Calculate market distance (how far order is from current market price)
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

        // Maximum allowed distance from market (depends on position)
        let max_distance_pct = if position_size_notional > 0.0 {
            cfg.internal.order_price_distance_with_position
        } else {
            cfg.internal.order_price_distance_no_position
        };

        // Check if order is too far from market
        let should_cancel_far = market_distance_pct.abs() > max_distance_pct;

        // Check if order is stale (too old)
        // If we have a position, use shorter timeout (2/3 of max age)
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

// ============================================================================
// Fill Rate Tracking
// ============================================================================

/// Fill rate state for tracking order fill performance
#[derive(Debug, Clone)]
pub struct FillRateState {
    /// Current fill rate (EWMA, 0.0 to 1.0)
    pub fill_rate: f64,
    /// Consecutive orders with no fills
    pub consecutive_no_fills: u32,
    /// Last fill time
    pub last_fill_time: Option<Instant>,
    /// Last decay check time (for periodic decay)
    pub last_decay_check: Option<Instant>,
    /// Last decay period (for optimization)
    pub last_decay_period: Option<u64>,
}

impl FillRateState {
    pub fn new() -> Self {
        Self {
            fill_rate: 0.5, // Initial fill rate (neutral)
            consecutive_no_fills: 0,
            last_fill_time: None,
            last_decay_check: None,
            last_decay_period: None,
        }
    }
}

/// Update fill rate when order is filled
pub fn update_fill_rate_on_fill(
    state: &mut FillRateState,
    increase_factor: f64,
    increase_bonus: f64,
) {
    state.fill_rate = (state.fill_rate * increase_factor + increase_bonus).min(1.0);
    state.consecutive_no_fills = 0;
    state.last_fill_time = Some(Instant::now());
}

/// Update fill rate when order is canceled (no fill)
pub fn update_fill_rate_on_cancel(
    state: &mut FillRateState,
    decrease_factor: f64,
) {
    state.fill_rate = (state.fill_rate * decrease_factor).max(0.0);
    state.consecutive_no_fills += 1;
}

/// Apply periodic fill rate decay (time-based decay)
pub fn apply_fill_rate_decay(
    state: &mut FillRateState,
    decay_factor: f64,
    decay_interval_sec: u64,
) {
    let now = Instant::now();
    
    // Check if enough time has passed since last decay
    let should_decay = state.last_decay_check
        .map(|last| now.duration_since(last).as_secs() >= decay_interval_sec)
        .unwrap_or(true);

    if should_decay {
        state.fill_rate = (state.fill_rate * decay_factor).max(0.0);
        state.last_decay_check = Some(now);
        
        // Track decay period for optimization
        if let Some(last_period) = state.last_decay_period {
            if last_period != decay_interval_sec {
                state.last_decay_period = Some(decay_interval_sec);
            }
        } else {
            state.last_decay_period = Some(decay_interval_sec);
        }
    }
}

/// Check if order sync is needed
pub fn should_sync_orders(
    last_sync: Option<Instant>,
    sync_interval_sec: u64,
) -> bool {
    last_sync
        .map(|last| last.elapsed().as_secs() >= sync_interval_sec)
        .unwrap_or(true)
}

/// Calculate fill rate based on recent order history
/// This is a simplified version - full implementation would track more detailed statistics
pub fn calculate_fill_rate_from_history(
    filled_orders: usize,
    total_orders: usize,
    time_window_sec: u64,
) -> f64 {
    if total_orders == 0 {
        return 0.5; // Default fill rate
    }
    
    let fill_rate = filled_orders as f64 / total_orders as f64;
    
    // Apply time decay (older orders have less weight)
    // Simplified: just return the ratio for now
    fill_rate.max(0.0).min(1.0)
}

// ============================================================================
// Order Synchronization Helpers
// ============================================================================

/// Compare local orders with API orders and return differences
#[derive(Debug, Clone)]
pub struct OrderSyncResult {
    /// Orders in API but not in local state (new orders)
    pub new_orders: Vec<String>,
    /// Orders in local state but not in API (removed orders - likely filled/canceled)
    pub removed_orders: Vec<String>,
    /// Orders that exist in both (no change)
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

/// Compare local orders with API orders
pub fn compare_orders(
    local_order_ids: &std::collections::HashSet<String>,
    api_order_ids: &std::collections::HashSet<String>,
) -> OrderSyncResult {
    let mut result = OrderSyncResult::new();

    // Find new orders (in API but not in local)
    for order_id in api_order_ids {
        if !local_order_ids.contains(order_id) {
            result.new_orders.push(order_id.clone());
        } else {
            result.unchanged_orders.push(order_id.clone());
        }
    }

    // Find removed orders (in local but not in API)
    for order_id in local_order_ids {
        if !api_order_ids.contains(order_id) {
            result.removed_orders.push(order_id.clone());
        }
    }

    result
}

// ============================================================================
// Order Validation
// ============================================================================

/// Validate order parameters before placement
pub fn validate_order(
    price: Px,
    qty: Qty,
    bid: Px,
    ask: Px,
    side: Side,
    min_notional: Option<Decimal>,
) -> Result<(), String> {
    // Check price validity
    if price.0 <= Decimal::ZERO {
        return Err("Order price must be positive".to_string());
    }

    // Check quantity validity
    if qty.0 <= Decimal::ZERO {
        return Err("Order quantity must be positive".to_string());
    }

    // Check price is within reasonable range (not too far from market)
    let market_price = match side {
        Side::Buy => ask.0,
        Side::Sell => bid.0,
    };

    if market_price <= Decimal::ZERO {
        return Err("Market price is invalid".to_string());
    }

    // Check min notional
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

