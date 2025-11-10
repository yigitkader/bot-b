//location: /crates/app/src/position_manager.rs
// Position management, PnL tracking, and position closing logic

use anyhow::Result;
use crate::core::types::*;
use crate::exec::binance::BinanceFutures;
use crate::exec::Venue;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::str::FromStr;
use std::time::Instant;
use tracing::{info, warn};
use crate::config::AppCfg;
use crate::types::SymbolState;
use crate::utils::{calc_net_pnl_usd, estimate_close_fee_bps, rate_limit_guard, record_pnl_snapshot};

/// Sync inventory with API position
pub fn sync_inventory(
    state: &mut SymbolState,
    api_position: &Position,
    force_sync: bool,
    reconcile_threshold: Decimal,
    min_sync_interval_ms: u128,
) {
    let inv_diff = (state.inv.0 - api_position.qty.0).abs();
    let time_since_last_update = state.last_inventory_update
        .map(|t| t.elapsed().as_millis())
        .unwrap_or(1000);

    if force_sync {
        info!(
            ws_inv = %state.inv.0,
            api_inv = %api_position.qty.0,
            diff = %inv_diff,
            "force syncing position"
        );
        state.inv = api_position.qty;
        state.last_inventory_update = Some(Instant::now());
    } else if inv_diff > reconcile_threshold && time_since_last_update > min_sync_interval_ms {
        warn!(
            ws_inv = %state.inv.0,
            api_inv = %api_position.qty.0,
            diff = %inv_diff,
            threshold = %reconcile_threshold,
            time_since_last_update_ms = time_since_last_update,
            "inventory mismatch detected, syncing with API position"
        );
        state.inv = api_position.qty;
        state.last_inventory_update = Some(Instant::now());
    }
}

/// Update daily PnL reset if needed
pub fn update_daily_pnl_reset(state: &mut SymbolState) {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    let should_reset_daily = if let Some(last_reset) = state.last_daily_reset {
        const DAY_MS: u64 = 24 * 3600 * 1000;
        now_ms.saturating_sub(last_reset) >= DAY_MS
    } else {
        true
    };

    if should_reset_daily {
        let old_daily_pnl = state.daily_pnl;
        state.daily_pnl = Decimal::ZERO;
        state.last_daily_reset = Some(now_ms);
        info!(
            old_daily_pnl = %old_daily_pnl,
            "daily PnL reset (new day started)"
        );
    }
}

/// Apply funding cost if needed
pub fn apply_funding_cost(
    state: &mut SymbolState,
    funding_rate: Option<f64>,
    next_funding_time: Option<u64>,
    position_size_notional: f64,
) {
    if let (Some(funding_rate), Some(next_funding_ts)) = (funding_rate, next_funding_time) {
        const FUNDING_INTERVAL_MS: u64 = 8 * 3600 * 1000;
        let this_funding_ts = next_funding_ts.saturating_sub(FUNDING_INTERVAL_MS);

        let should_apply = if let Some(last_applied) = state.last_applied_funding_time {
            this_funding_ts > last_applied
        } else {
            true
        };

        if should_apply && position_size_notional > 0.0 {
            let funding_cost = funding_rate * position_size_notional;
            state.total_funding_cost += Decimal::from_f64_retain(funding_cost).unwrap_or(Decimal::ZERO);
            state.last_applied_funding_time = Some(this_funding_ts);

            info!(
                funding_rate,
                this_funding_ts,
                next_funding_ts,
                position_size_notional,
                funding_cost,
                total_funding_cost = %state.total_funding_cost,
                "funding cost applied (8-hour interval)"
            );
        }
    }
}

/// Update position tracking fields
pub fn update_position_tracking(
    state: &mut SymbolState,
    position: &Position,
    mark_px: Px,
    cfg: &AppCfg,
) {
    // Update average entry price
    if !position.qty.0.is_zero() {
        state.avg_entry_price = Some(position.entry.0);
    } else {
        state.avg_entry_price = None;
    }

    // Record PnL snapshot
    record_pnl_snapshot(
        &mut state.pnl_history,
        position,
        mark_px,
        cfg.internal.pnl_history_max_len,
    );

    // Update position size history
    let position_size_notional = (mark_px.0 * position.qty.0.abs()).to_f64().unwrap_or(0.0);
    state.position_size_notional_history.push(position_size_notional);
    if state.position_size_notional_history.len() > cfg.internal.position_size_history_max_len {
        state.position_size_notional_history.remove(0);
    }

    // Update position entry time if needed
    let position_qty_threshold = Decimal::from_str_radix(&cfg.internal.position_qty_threshold, 10)
        .unwrap_or(Decimal::new(1, 8));
    if position.qty.0.abs() > position_qty_threshold {
        if state.position_entry_time.is_none() {
            state.position_entry_time = Some(Instant::now());
            warn!(
                entry_time_set = "from_position_check_fallback",
                "position entry time set from position check (FALLBACK)"
            );
        }
        if let Some(entry_time) = state.position_entry_time {
            state.position_hold_duration_ms = entry_time.elapsed().as_millis() as u64;
        }
    } else {
        state.position_entry_time = None;
        state.peak_pnl = Decimal::ZERO;
        state.position_hold_duration_ms = 0;
    }
}

/// Check if position should be closed based on profit/loss
pub fn should_close_position_smart(
    state: &SymbolState,
    position: &Position,
    mark_px: Px,
    bid: Px,
    ask: Px,
    min_profit_usd: f64,
    maker_fee_rate: f64,
    taker_fee_rate: f64,
) -> (bool, String) {
    let position_qty_f64 = position.qty.0.to_f64().unwrap_or(0.0);
    let entry_price_f64 = position.entry.0.to_f64().unwrap_or(0.0);
    let mark_price_f64 = mark_px.0.to_f64().unwrap_or(0.0);

    if position_qty_f64.abs() <= 0.0001 || entry_price_f64 <= 0.0 || mark_price_f64 <= 0.0 {
        return (false, "no_position".to_string());
    }

    let position_side = if position.qty.0.is_sign_positive() {
        Side::Buy
    } else {
        Side::Sell
    };

    let exit_price = match position_side {
        Side::Buy => bid.0,
        Side::Sell => ask.0,
    };

    let entry_fee_bps = maker_fee_rate * 10000.0;
    let exit_fee_bps = estimate_close_fee_bps(true, maker_fee_rate * 10000.0, taker_fee_rate * 10000.0);

    let qty_abs = position.qty.0.abs();
    let net_pnl = calc_net_pnl_usd(
        position.entry.0,
        exit_price,
        qty_abs,
        &position_side,
        entry_fee_bps,
        exit_fee_bps,
    );

    // Rule 1: Fixed TP ($0.50)
    if net_pnl >= min_profit_usd {
        return (true, format!("take_profit_{:.2}_usd", net_pnl));
    }

    // Rule 2: Smart position management
    let entry_time = match state.position_entry_time {
        Some(t) => t,
        None => return (false, "no_entry_time".to_string()),
    };

    let age_secs = entry_time.elapsed().as_secs() as f64;

    // Time-weighted profit thresholds
    let time_weighted_threshold = if age_secs <= 10.0 {
        min_profit_usd * 0.6
    } else if age_secs <= 20.0 {
        min_profit_usd
    } else if age_secs <= 60.0 {
        min_profit_usd * 0.4
    } else {
        min_profit_usd * 0.2
    };

    // Trend alignment
    let trend_bps = state.strategy.get_trend_bps();
    let position_side_f64 = if position.qty.0.is_sign_positive() { 1.0 } else { -1.0 };
    let trend_aligned = (trend_bps > 0.0 && position_side_f64 > 0.0) || (trend_bps < 0.0 && position_side_f64 < 0.0);
    let trend_factor = if trend_aligned { 1.3 } else { 0.8 };

    // Momentum
    let pnl_trend = if state.pnl_history.len() >= 10 {
        let recent = &state.pnl_history[state.pnl_history.len().saturating_sub(10)..];
        let first = recent[0];
        let last = recent[recent.len() - 1];
        if first > Decimal::ZERO {
            ((last - first) / first).to_f64().unwrap_or(0.0)
        } else {
            0.0
        }
    } else {
        0.0
    };
    let momentum_factor = if pnl_trend > 0.1 {
        1.2
    } else if pnl_trend < -0.1 {
        0.7
    } else {
        1.0
    };

    // Volatility
    let volatility = state.strategy.get_volatility();
    let volatility_factor = if volatility > 0.05 {
        0.7
    } else if volatility < 0.01 {
        1.2
    } else {
        1.0
    };

    // Peak PnL trailing
    let peak_pnl_f64 = state.peak_pnl.to_f64().unwrap_or(0.0);
    let drawdown_from_peak = peak_pnl_f64 - net_pnl;
    let trailing_stop_threshold = min_profit_usd * 0.5;
    let should_close_trailing = peak_pnl_f64 > min_profit_usd && drawdown_from_peak > trailing_stop_threshold;

    // Drawdown
    let max_loss_threshold = -min_profit_usd * 2.0;
    let should_close_drawdown = net_pnl < max_loss_threshold;

    // Recovery (from loss to profit)
    let was_in_loss = state.pnl_history.len() >= 2 && {
        let prev_pnl = state.pnl_history[state.pnl_history.len() - 2].to_f64().unwrap_or(0.0);
        prev_pnl < 0.0
    };
    let should_close_recovery = was_in_loss && net_pnl > 0.0;

    // Combined threshold
    let combined_threshold = time_weighted_threshold / (trend_factor * momentum_factor * volatility_factor);
    let should_close_by_threshold = net_pnl >= combined_threshold;

    if should_close_by_threshold {
        (true, format!("smart_threshold_{:.2}_usd", net_pnl))
    } else if should_close_trailing {
        (true, format!("trailing_stop_{:.2}_usd", net_pnl))
    } else if should_close_drawdown {
        (true, format!("drawdown_{:.2}_usd", net_pnl))
    } else if should_close_recovery {
        (true, format!("recovery_{:.2}_usd", net_pnl))
    } else {
        (false, "".to_string())
    }
}

/// Close position
pub async fn close_position(
    venue: &BinanceFutures,
    symbol: &str,
    state: &mut SymbolState,
) -> Result<()> {
    state.position_closing = true;
    state.last_close_attempt = Some(Instant::now());

    rate_limit_guard(1).await;
    let result = venue.close_position(symbol).await;

    state.position_closing = false;

    match result {
        Ok(_) => {
            info!(symbol = %symbol, "position closed successfully");
            Ok(())
        }
        Err(e) => {
            warn!(symbol = %symbol, error = %e, "failed to close position");
            Err(e)
        }
    }
}

