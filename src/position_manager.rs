
use crate::types::{Px, PositionInfo, PositionDirection, Side};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::time::Instant;
#[derive(Debug, Clone)]
pub struct PositionState {
    pub entry_time: Option<Instant>,
    pub pnl_history: Vec<Decimal>,
    pub peak_pnl: Decimal,
    pub strategy_info: Option<StrategyInfo>,
}
struct NetPnlContext {
    net_pnl: f64,
    position_qty_abs: f64,
}
#[derive(Debug, Clone)]
pub struct StrategyInfo {
    pub trend_bps: f64,
    pub volatility: f64,
}
impl PositionState {
    pub fn new(entry_time: Instant) -> Self {
        Self {
            entry_time: Some(entry_time),
            pnl_history: Vec::new(),
            peak_pnl: Decimal::ZERO,
            strategy_info: None,
        }
    }
    pub fn update_pnl(&mut self, current_pnl: Decimal) {
        self.pnl_history.push(current_pnl);
        if self.pnl_history.len() > 100 {
            self.pnl_history.remove(0);
        }
        if current_pnl > self.peak_pnl {
            self.peak_pnl = current_pnl;
        }
    }
}
pub fn should_close_position_smart(
    position: &PositionInfo,
    mark_px: Px,
    bid: Px,
    ask: Px,
    state: &PositionState,
    min_profit_usd: f64,
    maker_fee_rate: f64,
    taker_fee_rate: f64,
    max_position_duration_sec: f64,
    max_loss_duration_sec: f64,
    time_weighted_threshold_early: f64,
    time_weighted_threshold_normal: f64,
    time_weighted_threshold_mid: f64,
    time_weighted_threshold_late: f64,
    trailing_stop_threshold_ratio: f64,
    max_loss_threshold_ratio: f64,
    stop_loss_threshold_ratio: f64,
) -> (bool, String) {
    let net_pnl_ctx = match calculate_net_pnl_context(position, mark_px, bid, ask, maker_fee_rate, taker_fee_rate) {
        Some(ctx) => ctx,
        None => return (false, "no_position".to_string()),
    };
    if let Some(result) = check_basic_rules(&net_pnl_ctx, min_profit_usd, stop_loss_threshold_ratio) {
        return result;
    }
    let entry_time = match state.entry_time {
        Some(t) => t,
        None => return (false, "no_entry_time".to_string()),
    };
    let age_secs = entry_time.elapsed().as_secs() as f64;
    if let Some(result) = check_timeout_rules(net_pnl_ctx.net_pnl, age_secs, max_position_duration_sec, max_loss_duration_sec) {
        return result;
    }
    let time_weighted_threshold = if age_secs <= 10.0 {
        min_profit_usd * time_weighted_threshold_early
    } else if age_secs <= 20.0 {
        min_profit_usd * time_weighted_threshold_normal
    } else if age_secs <= 60.0 {
        min_profit_usd * time_weighted_threshold_mid
    } else {
        min_profit_usd * time_weighted_threshold_late
    };
    let trend_factor = calculate_trend_factor(position, state);
    let momentum_factor = calculate_momentum_factor(state);
    let volatility_factor = calculate_volatility_factor(state);
    let (should_close_trailing, should_close_drawdown, should_close_recovery) =
        evaluate_trailing_rules(state, net_pnl_ctx.net_pnl, min_profit_usd, trailing_stop_threshold_ratio, max_loss_threshold_ratio);
    let combined_threshold = time_weighted_threshold / (trend_factor * momentum_factor * volatility_factor);
    let should_close_by_threshold = net_pnl_ctx.net_pnl >= combined_threshold;
    if should_close_by_threshold {
        (true, format!("smart_threshold_{:.2}_usd", net_pnl_ctx.net_pnl))
    } else if should_close_trailing {
        (true, format!("trailing_stop_{:.2}_usd", net_pnl_ctx.net_pnl))
    } else if should_close_drawdown {
        (true, format!("drawdown_{:.2}_usd", net_pnl_ctx.net_pnl))
    } else if should_close_recovery {
        (true, format!("recovery_{:.2}_usd", net_pnl_ctx.net_pnl))
    } else {
        (false, "".to_string())
    }
}
fn calculate_net_pnl_context(
    position: &PositionInfo,
    mark_px: Px,
    bid: Px,
    ask: Px,
    maker_fee_rate: f64,
    taker_fee_rate: f64,
) -> Option<NetPnlContext> {
    let position_qty_f64 = position.qty.0.to_f64().unwrap_or(0.0);
    let entry_price_f64 = position.entry_price.0.to_f64().unwrap_or(0.0);
    let mark_price_f64 = mark_px.0.to_f64().unwrap_or(0.0);
    if position_qty_f64.abs() <= 0.0001 || entry_price_f64 <= 0.0 || mark_price_f64 <= 0.0 {
        return None;
    }
    let position_side = match position.direction {
        PositionDirection::Long => Side::Buy,
        PositionDirection::Short => Side::Sell,
    };
    let exit_price = match position_side {
        Side::Buy => bid.0,
        Side::Sell => ask.0,
    };
    let qty_abs = position.qty.0.abs();
    let entry_commission_pct = crate::utils::get_commission_rate(
        position.is_maker.unwrap_or(false),
        maker_fee_rate,
        taker_fee_rate,
    );
    let exit_commission_pct = crate::utils::get_commission_rate(false, maker_fee_rate, taker_fee_rate);
    let net_pnl_decimal = crate::utils::calculate_net_pnl(
        position.entry_price.0,
        exit_price,
        qty_abs,
        position.direction,
        position.leverage,
        entry_commission_pct,
        exit_commission_pct,
    );
    Some(NetPnlContext {
        net_pnl: net_pnl_decimal.to_f64().unwrap_or(0.0),
        position_qty_abs: position_qty_f64.abs(),
    })
}
fn check_basic_rules(ctx: &NetPnlContext, min_profit_usd: f64, stop_loss_threshold_ratio: f64) -> Option<(bool, String)> {
    if ctx.net_pnl >= min_profit_usd {
        return Some((true, format!("take_profit_{:.2}_usd", ctx.net_pnl)));
    }
    let stop_loss_threshold = -min_profit_usd * stop_loss_threshold_ratio.max(0.3);
    if ctx.net_pnl <= stop_loss_threshold {
        return Some((true, format!("stop_loss_{:.2}_usd", ctx.net_pnl)));
    }
    if ctx.position_qty_abs > 0.5 {
        return Some((true, format!("inventory_threshold_exceeded_{:.2}", ctx.position_qty_abs)));
    }
    None
}
fn check_timeout_rules(net_pnl: f64, age_secs: f64, max_position_duration_sec: f64, max_loss_duration_sec: f64) -> Option<(bool, String)> {
    if net_pnl < 0.0 && age_secs >= max_loss_duration_sec {
        return Some((true, format!("loss_timeout_{:.2}_usd_{:.0}_sec", net_pnl, age_secs)));
    }
    if age_secs >= max_position_duration_sec {
        return Some((true, format!("max_duration_timeout_{:.2}_usd_{:.0}_sec", net_pnl, age_secs)));
    }
    None
}
fn calculate_trend_factor(position: &PositionInfo, state: &PositionState) -> f64 {
    if let Some(ref strategy_info) = state.strategy_info {
        let trend_bps = strategy_info.trend_bps;
        let position_side_f64 = match position.direction {
            PositionDirection::Long => 1.0,
            PositionDirection::Short => -1.0,
        };
        let trend_aligned = (trend_bps > 0.0 && position_side_f64 > 0.0) || (trend_bps < 0.0 && position_side_f64 < 0.0);
        if trend_aligned {
            1.3
        } else {
            0.8
        }
    } else {
        1.0
    }
}
fn calculate_momentum_factor(state: &PositionState) -> f64 {
    if state.pnl_history.len() >= 10 {
        let recent = &state.pnl_history[state.pnl_history.len().saturating_sub(10)..];
        let first = recent[0];
        let last = recent[recent.len() - 1];
        if first > Decimal::ZERO {
            let pnl_trend = ((last - first) / first).to_f64().unwrap_or(0.0);
            if pnl_trend > 0.1 {
                1.2
            } else if pnl_trend < -0.1 {
                0.7
            } else {
                1.0
            }
        } else {
            1.0
        }
    } else {
        1.0
    }
}
fn calculate_volatility_factor(state: &PositionState) -> f64 {
    if let Some(ref strategy_info) = state.strategy_info {
        let volatility = strategy_info.volatility;
        if volatility > 0.05 {
            0.7
        } else if volatility < 0.01 {
            1.2
        } else {
            1.0
        }
    } else {
        1.0
    }
}
fn evaluate_trailing_rules(state: &PositionState, net_pnl: f64, min_profit_usd: f64, trailing_stop_threshold_ratio: f64, max_loss_threshold_ratio: f64) -> (bool, bool, bool) {
    let peak_pnl_f64 = state.peak_pnl.to_f64().unwrap_or(0.0);
    let drawdown_from_peak = peak_pnl_f64 - net_pnl;
    let trailing_stop_threshold = min_profit_usd * trailing_stop_threshold_ratio;
    let should_close_trailing = peak_pnl_f64 > min_profit_usd && drawdown_from_peak > trailing_stop_threshold;
    let max_loss_threshold = -min_profit_usd * max_loss_threshold_ratio;
    let should_close_drawdown = net_pnl < max_loss_threshold;
    let was_in_loss = state.pnl_history.len() >= 2 && {
        let prev_pnl = state.pnl_history[state.pnl_history.len() - 2].to_f64().unwrap_or(0.0);
        prev_pnl < 0.0
    };
    let should_close_recovery = was_in_loss && net_pnl > 0.0;
    (should_close_trailing, should_close_drawdown, should_close_recovery)
}
