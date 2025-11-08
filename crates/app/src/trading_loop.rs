//location: /crates/app/src/trading_loop.rs
// Trading loop helper functions

use crate::config;
use crate::rate_limiter::rate_limit_guard;
use crate::types::{OrderInfo, SymbolState};
use bot_core::types::*;
use exec::binance::{BinanceFutures, BinanceSpot};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::time::timeout;

pub enum VenueType {
    Spot(BinanceSpot),
    Futures(BinanceFutures),
}

/// Fetch balance for a quote asset from venue
pub async fn fetch_quote_balance(
    venue: &VenueType,
    quote_asset: &str,
) -> f64 {
    rate_limit_guard().await;
    let result = match venue {
        VenueType::Spot(v) => {
            timeout(Duration::from_secs(5), v.asset_free(quote_asset)).await
        }
        VenueType::Futures(v) => {
            timeout(Duration::from_secs(5), v.available_balance(quote_asset)).await
        }
    };
    
    match result {
        Ok(Ok(balance)) => balance.to_f64().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// Collect unique quote assets from states
pub fn collect_unique_quote_assets(states: &[SymbolState]) -> Vec<String> {
    let mut unique: std::collections::HashSet<String> = std::collections::HashSet::new();
    for state in states {
        unique.insert(state.meta.quote_asset.clone());
    }
    unique.into_iter().collect()
}

/// Fetch balances for all unique quote assets
pub async fn fetch_all_quote_balances(
    venue: &VenueType,
    quote_assets: &[String],
) -> HashMap<String, f64> {
    let mut balances = HashMap::new();
    for quote_asset in quote_assets {
        let balance = fetch_quote_balance(venue, quote_asset).await;
        balances.insert(quote_asset.clone(), balance);
    }
    balances
}

/// Check if symbol should be processed based on balance
pub fn should_process_symbol(
    state: &SymbolState,
    quote_balance: f64,
    min_balance: f64,
    min_order_size: f64,
    effective_leverage: f64,
    mode: &str,
) -> bool {
    let has_open_orders = !state.active_orders.is_empty();
    let has_position = !state.inv.0.is_zero();
    
    if has_open_orders || has_position {
        return true;
    }
    
    if quote_balance < min_balance {
        return false;
    }
    
    match mode {
        "futures" => {
            let total_with_leverage = quote_balance * effective_leverage;
            total_with_leverage >= min_order_size
        }
        _ => quote_balance >= min_order_size,
    }
}

/// Update fill rate after order fill
pub fn update_fill_rate_on_fill(
    state: &mut SymbolState,
    increase_factor: f64,
    increase_bonus: f64,
) {
    state.consecutive_no_fills = 0;
    state.order_fill_rate = (state.order_fill_rate * increase_factor + increase_bonus)
        .min(1.0);
}

/// Update fill rate after order cancel
pub fn update_fill_rate_on_cancel(state: &mut SymbolState, decrease_factor: f64) {
    state.order_fill_rate = (state.order_fill_rate * decrease_factor).max(0.0);
}

/// Check if order should be synced
pub fn should_sync_orders(
    force_sync: bool,
    last_sync: Option<Instant>,
    sync_interval_sec: u64,
) -> bool {
    if force_sync {
        return true;
    }
    last_sync
        .map(|last| last.elapsed().as_secs() >= sync_interval_sec)
        .unwrap_or(true)
}

/// Check if order is stale
pub fn is_order_stale(order: &OrderInfo, max_age_ms: u64, has_position: bool) -> bool {
    let age_ms = order.created_at.elapsed().as_millis() as u64;
    let threshold = if has_position {
        max_age_ms / 2
    } else {
        max_age_ms
    };
    age_ms > threshold
}

/// Check if order is too far from market
pub fn is_order_too_far_from_market(
    order: &OrderInfo,
    best_bid: Px,
    best_ask: Px,
    max_distance_with_position: f64,
    max_distance_no_position: f64,
    has_position: bool,
) -> bool {
    let order_price = order.price.0.to_f64().unwrap_or(0.0);
    let max_distance = if has_position {
        max_distance_with_position
    } else {
        max_distance_no_position
    };
    
    let market_distance = match order.side {
        Side::Buy => {
            let ask = best_ask.0.to_f64().unwrap_or(0.0);
            if ask > 0.0 {
                (ask - order_price) / ask
            } else {
                0.0
            }
        }
        Side::Sell => {
            let bid = best_bid.0.to_f64().unwrap_or(0.0);
            if bid > 0.0 {
                (order_price - bid) / bid
            } else {
                0.0
            }
        }
    };
    
    market_distance.abs() > max_distance
}

/// Check if position should be closed based on profit/loss
pub fn should_close_position(
    current_pnl: Decimal,
    peak_pnl: Decimal,
    price_change_pct: f64,
    position_size_notional: f64,
    position_hold_duration_ms: u64,
    pnl_trend: f64,
    cfg: &config::InternalCfg,
    strategy_cfg: &config::StrategyInternalCfg,
) -> (bool, &'static str) {
    let current_pnl_f64 = current_pnl.to_f64().unwrap_or(0.0);
    let peak_pnl_f64 = peak_pnl.to_f64().unwrap_or(0.0);
    
    let take_profit_threshold = if position_size_notional > 100.0 {
        cfg.take_profit_threshold_large
    } else {
        cfg.take_profit_threshold_small
    };
    
    let should_take_profit = if price_change_pct >= take_profit_threshold {
        pnl_trend < strategy_cfg.trend_analysis_threshold_negative
            || (position_hold_duration_ms > cfg.take_profit_time_threshold_ms
                && price_change_pct < cfg.take_profit_min_profit_threshold)
            || (price_change_pct >= cfg.take_profit_min_profit_threshold
                && pnl_trend < strategy_cfg.trend_analysis_threshold_strong_negative)
    } else {
        false
    };
    
    let should_trailing_stop = if peak_pnl_f64 > 0.0 && current_pnl_f64 < peak_pnl_f64 {
        let drawdown = (peak_pnl_f64 - current_pnl_f64)
            / peak_pnl_f64.abs().max(cfg.trailing_stop_min_peak);
        let threshold = if peak_pnl_f64 > cfg.trailing_stop_peak_threshold_large {
            cfg.trailing_stop_drawdown_large
        } else if peak_pnl_f64 > cfg.trailing_stop_peak_threshold_medium {
            cfg.trailing_stop_drawdown_medium
        } else {
            cfg.trailing_stop_drawdown_small
        };
        drawdown >= threshold
    } else {
        false
    };
    
    let should_stop_loss = if price_change_pct <= cfg.stop_loss_threshold {
        pnl_trend < cfg.stop_loss_trend_threshold
            || position_hold_duration_ms > cfg.stop_loss_time_threshold_ms
    } else {
        false
    };
    
    if should_trailing_stop && peak_pnl_f64 > cfg.trailing_stop_peak_threshold_medium {
        (true, "trailing_stop")
    } else if should_take_profit {
        (true, "take_profit")
    } else if should_stop_loss {
        (true, "stop_loss")
    } else {
        (false, "")
    }
}

/// Calculate effective leverage
pub fn calculate_effective_leverage(config_leverage: Option<u32>, max_leverage: u32) -> f64 {
    config_leverage
        .unwrap_or(max_leverage)
        .max(1) as f64
}

