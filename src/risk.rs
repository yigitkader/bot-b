
use crate::config::AppCfg;
use crate::types::{Px, Qty, Position};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::time::Instant;
use tracing::{info, warn};
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct RiskLimits {
    pub inv_cap: Qty,
    pub min_liq_gap_bps: f64,
    pub dd_limit_bps: i64,
    pub max_leverage: u32,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum RiskAction {
    Ok,
    Widen,
    Reduce,
    Halt,
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PositionRiskLevel {
    Ok,
    Soft,
    Medium,
    Hard,
}
#[allow(dead_code)]
pub fn check_risk(
    pos: &Position,
    inv: Qty,
    liq_gap_bps: f64,
    dd_bps: i64,
    lim: &RiskLimits,
) -> RiskAction {
    if dd_bps <= -lim.dd_limit_bps {
        return RiskAction::Halt;
    }
    if inv.0.abs() > lim.inv_cap.0 {
        return RiskAction::Reduce;
    }
    let has_position = pos.qty.0.abs() > Decimal::new(1, 8);
    if has_position && liq_gap_bps < lim.min_liq_gap_bps {
        return RiskAction::Reduce;
    }
    if has_position && pos.leverage > lim.max_leverage {
        return RiskAction::Reduce;
    }
    RiskAction::Ok
}
#[derive(Debug, Clone)]
pub struct PositionRiskState {
    pub position_size_notional: f64,
    pub total_active_orders_notional: f64,
    #[allow(dead_code)]
    pub has_open_orders: bool,
    pub is_opportunity_mode: bool,
}
pub fn check_position_size_risk(
    state: &PositionRiskState,
    max_usd_per_order: f64,
    effective_leverage: f64,
    cfg: &AppCfg,
) -> (PositionRiskLevel, f64, bool) {
    let max_position_multiplier = if state.is_opportunity_mode {
        cfg.internal.opportunity_mode_position_multiplier
    } else {
        1.0
    };
    let max_position_size_usd = max_usd_per_order * effective_leverage * cfg.internal.max_position_size_buffer * max_position_multiplier;
    let total_exposure_notional = state.position_size_notional + state.total_active_orders_notional;
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
#[allow(dead_code)]
pub fn calculate_total_active_orders_notional(active_orders: &[(Px, Qty)]) -> f64 {
    active_orders
        .iter()
        .map(|(price, qty)| (price.0 * qty.0).to_f64().unwrap_or(0.0))
        .sum()
}
pub fn check_pnl_alerts(
    last_pnl_alert: &mut Option<Instant>,
    pnl_f64: f64,
    position_size_notional: f64,
    cfg: &AppCfg,
) {
    let should_alert = last_pnl_alert
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
            *last_pnl_alert = Some(Instant::now());
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
            *last_pnl_alert = Some(Instant::now());
        }
    }
}
#[allow(dead_code)]
pub fn update_peak_pnl(peak_pnl: &mut Decimal, current_pnl: Decimal) {
    if current_pnl > *peak_pnl {
        *peak_pnl = current_pnl;
    }
}
#[allow(dead_code)]
pub fn determine_risk_actions(
    risk_level: PositionRiskLevel,
    position_size_notional: f64,
    total_exposure: f64,
    max_allowed: f64,
    active_orders: &[(String, Instant)],
) -> RiskActions {
    match risk_level {
        PositionRiskLevel::Hard => {
            warn!(
                position_size_notional,
                total_exposure,
                max_allowed,
                "HARD LIMIT: force closing position"
            );
            RiskActions {
                should_close_position: true,
                should_cancel_all_orders: true,
                orders_to_cancel: vec![],
                should_block_new_orders: true,
            }
        }
        PositionRiskLevel::Medium => {
            warn!(
                position_size_notional,
                total_exposure,
                "MEDIUM LIMIT: reducing active orders"
            );
            let mut sorted_orders: Vec<_> = active_orders.iter().collect();
            sorted_orders.sort_by_key(|(_, t)| *t);
            let to_cancel_count = (sorted_orders.len() / 2).max(1);
            let orders_to_cancel: Vec<String> = sorted_orders
                .iter()
                .take(to_cancel_count)
                .map(|(id, _)| id.clone())
                .collect();
            RiskActions {
                should_close_position: false,
                should_cancel_all_orders: false,
                orders_to_cancel,
                should_block_new_orders: false,
            }
        }
        PositionRiskLevel::Soft => {
            info!(
                position_size_notional,
                total_exposure,
                "SOFT LIMIT: blocking new orders"
            );
            RiskActions {
                should_close_position: false,
                should_cancel_all_orders: false,
                orders_to_cancel: vec![],
                should_block_new_orders: true,
            }
        }
        PositionRiskLevel::Ok => {
            RiskActions {
                should_close_position: false,
                should_cancel_all_orders: false,
                orders_to_cancel: vec![],
                should_block_new_orders: false,
            }
        }
    }
}
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct RiskActions {
    pub should_close_position: bool,
    pub should_cancel_all_orders: bool,
    pub orders_to_cancel: Vec<String>,
    pub should_block_new_orders: bool,
}
