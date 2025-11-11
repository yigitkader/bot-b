//location: /crates/app/src/risk_manager.rs
// Risk management and limit checking logic

use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use tracing::{info, warn};
use crate::config::AppCfg;
use crate::types::SymbolState;

/// Risk level for position size
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PositionRiskLevel {
    Ok,
    Soft,   // Block new orders
    Medium, // Reduce existing orders
    Hard,   // Force close
}

/// Check position size risk and return risk level
pub fn check_position_size_risk(
    state: &SymbolState,
    position_size_notional: f64,
    total_active_orders_notional: f64,
    max_usd_per_order: f64,
    effective_leverage: f64,
    cfg: &AppCfg,
) -> (PositionRiskLevel, f64, bool) {
    let is_opportunity_mode = state.strategy.is_opportunity_mode();
    let max_position_multiplier = if is_opportunity_mode {
        cfg.internal.opportunity_mode_position_multiplier
    } else {
        1.0
    };

    let max_position_size_usd = max_usd_per_order * effective_leverage * cfg.internal.max_position_size_buffer * max_position_multiplier;
    let total_exposure_notional = position_size_notional + total_active_orders_notional;

    let soft_limit = max_position_size_usd * cfg.internal.opportunity_mode_soft_limit_ratio;
    let medium_limit = max_position_size_usd * cfg.internal.opportunity_mode_medium_limit_ratio;
    let hard_limit = max_position_size_usd * cfg.internal.opportunity_mode_hard_limit_ratio;

    let risk_level = if total_exposure_notional >= hard_limit {
        PositionRiskLevel::Hard
    } else if total_exposure_notional >= medium_limit {
        PositionRiskLevel::Medium
    } else if total_exposure_notional >= soft_limit {
        PositionRiskLevel::Soft
    } else {
        PositionRiskLevel::Ok
    };

    let should_block_new_orders = risk_level != PositionRiskLevel::Ok;

    (risk_level, max_position_size_usd, should_block_new_orders)
}

/// Calculate total active orders notional
pub fn calculate_total_active_orders_notional(state: &SymbolState) -> f64 {
    state.active_orders.values()
        .map(|order| {
            (order.price.0 * order.remaining_qty.0).to_f64().unwrap_or(0.0)
        })
        .sum()
}

/// Check PnL alerts
pub fn check_pnl_alerts(
    state: &mut SymbolState,
    pnl_f64: f64,
    position_size_notional: f64,
    cfg: &AppCfg,
) {
    let should_alert = state.last_pnl_alert
        .map(|last| last.elapsed().as_secs() >= cfg.internal.pnl_alert_interval_sec)
        .unwrap_or(true);

    if !should_alert {
        return;
    }

    if pnl_f64 > 0.0 && position_size_notional > 0.0 {
        let pnl_pct = pnl_f64 / position_size_notional;
        if pnl_pct >= cfg.internal.pnl_alert_threshold_positive {
            info!(
                pnl = pnl_f64,
                pnl_pct = pnl_pct * 100.0,
                position_size = position_size_notional,
                "PNL ALERT: Significant profit achieved"
            );
            state.last_pnl_alert = Some(std::time::Instant::now());
        }
    }

    if pnl_f64 < 0.0 && position_size_notional > 0.0 {
        let pnl_pct = pnl_f64 / position_size_notional;
        if pnl_pct <= cfg.internal.pnl_alert_threshold_negative {
            warn!(
                pnl = pnl_f64,
                pnl_pct = pnl_pct * 100.0,
                position_size = position_size_notional,
                "PNL ALERT: Significant loss detected"
            );
            state.last_pnl_alert = Some(std::time::Instant::now());
        }
    }
}

/// Update peak PnL
pub fn update_peak_pnl(state: &mut SymbolState, current_pnl: Decimal) {
    if current_pnl > state.peak_pnl {
        state.peak_pnl = current_pnl;
    }
}

