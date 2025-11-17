// Position management module
// Smart position closing with 11 different closing conditions
// Based on reference project with adaptations for our event-driven architecture

use crate::config::AppCfg;
use crate::types::{Px, Qty, PositionInfo, PositionDirection, Side};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::time::Instant;

// ============================================================================
// Constants
// ============================================================================

/// Maximum position duration in seconds (market making positions shouldn't stay open too long)
pub const MAX_POSITION_DURATION_SEC: f64 = 300.0; // 5 minutes

/// Maximum loss duration in seconds (if position is in loss for this long, force close)
pub const MAX_LOSS_DURATION_SEC: f64 = 120.0; // 2 minutes

// ============================================================================
// Position State (for smart closing)
// ============================================================================

/// Position state for smart closing decisions
#[derive(Debug, Clone)]
pub struct PositionState {
    /// Position entry time (when position was opened)
    pub entry_time: Option<Instant>,
    /// PnL history (for trend analysis)
    pub pnl_history: Vec<Decimal>,
    /// Peak PnL (for trailing stop)
    pub peak_pnl: Decimal,
    /// Strategy interface for trend/volatility information
    /// This is optional - if not available, some checks will be skipped
    pub strategy_info: Option<StrategyInfo>,
}

/// Strategy information interface (for trend/volatility)
#[derive(Debug, Clone)]
pub struct StrategyInfo {
    /// Trend in basis points (positive = uptrend, negative = downtrend)
    pub trend_bps: f64,
    /// Volatility (EWMA)
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

    /// Update PnL history and peak PnL
    pub fn update_pnl(&mut self, current_pnl: Decimal) {
        self.pnl_history.push(current_pnl);
        // Keep only last 100 entries to prevent memory growth
        if self.pnl_history.len() > 100 {
            self.pnl_history.remove(0);
        }
        
        // Update peak PnL
        if current_pnl > self.peak_pnl {
            self.peak_pnl = current_pnl;
        }
    }
}

// ============================================================================
// Smart Position Closing
// ============================================================================

/// Check if position should be closed based on profit/loss and multiple conditions
/// Returns (should_close, reason)
/// 
/// Closing conditions (in priority order):
/// 1. Fixed Take Profit: net_pnl >= min_profit_usd
/// 2. Stop Loss: net_pnl <= -0.10 USD
/// 3. Inventory Threshold: position size too large
/// 4. Loss Timeout: position in loss for too long
/// 5. Max Duration Timeout: position open for too long
/// 6. Time-weighted Profit: profit threshold based on age
/// 7. Trend Alignment: position against trend
/// 8. Momentum Factor: PnL trend analysis
/// 9. Volatility Factor: high volatility = close early
/// 10. Peak PnL Trailing: trailing stop from peak
/// 11. Drawdown: maximum loss threshold
/// 12. Recovery: from loss to profit (take profit)
pub fn should_close_position_smart(
    position: &PositionInfo,
    mark_px: Px,
    bid: Px,
    ask: Px,
    state: &PositionState,
    min_profit_usd: f64,
    maker_fee_rate: f64,
    taker_fee_rate: f64,
) -> (bool, String) {
    let position_qty_f64 = position.qty.0.to_f64().unwrap_or(0.0);
    let entry_price_f64 = position.entry_price.0.to_f64().unwrap_or(0.0);
    let mark_price_f64 = mark_px.0.to_f64().unwrap_or(0.0);

    // Early exit: no position
    if position_qty_f64.abs() <= 0.0001 || entry_price_f64 <= 0.0 || mark_price_f64 <= 0.0 {
        return (false, "no_position".to_string());
    }

    // Determine position side from direction
    let position_side = match position.direction {
        PositionDirection::Long => Side::Buy,
        PositionDirection::Short => Side::Sell,
    };

    let exit_price = match position_side {
        Side::Buy => bid.0,
        Side::Sell => ask.0,
    };

    // Calculate net PnL (with fees)
    let entry_fee_bps = if position.is_maker.unwrap_or(false) {
        maker_fee_rate * 10000.0
    } else {
        taker_fee_rate * 10000.0
    };
    let exit_fee_bps = estimate_close_fee_bps(true, maker_fee_rate * 10000.0, taker_fee_rate * 10000.0);
    let qty_abs = position.qty.0.abs();
    let net_pnl = calc_net_pnl_usd(
        position.entry_price.0,
        exit_price,
        qty_abs,
        &position_side,
        entry_fee_bps,
        exit_fee_bps,
    );

    // Rule 1: Fixed Take Profit ($0.50 or min_profit_usd)
    if net_pnl >= min_profit_usd {
        return (true, format!("take_profit_{:.2}_usd", net_pnl));
    }

    // Rule 2: Stop Loss (-$0.10)
    // Tight stop loss: close small losses immediately, don't let them grow
    if net_pnl <= -0.10 {
        return (true, format!("stop_loss_{:.2}_usd", net_pnl));
    }

    // Rule 3: Inventory Threshold
    // If position size is too large, force close (risk management)
    if position_qty_f64.abs() > 0.5 {
        return (true, format!("inventory_threshold_exceeded_{:.2}", position_qty_f64));
    }

    // Rule 4 & 5: Timeout checks (require entry_time)
    let entry_time = match state.entry_time {
        Some(t) => t,
        None => return (false, "no_entry_time".to_string()),
    };

    let age_secs = entry_time.elapsed().as_secs() as f64;

    // Rule 4: Loss Timeout
    // If position is in loss for too long, force close
    if net_pnl < 0.0 && age_secs >= MAX_LOSS_DURATION_SEC {
        return (true, format!("loss_timeout_{:.2}_usd_{:.0}_sec", net_pnl, age_secs));
    }

    // Rule 5: Max Duration Timeout
    // Market making positions shouldn't stay open too long
    if age_secs >= MAX_POSITION_DURATION_SEC {
        return (true, format!("max_duration_timeout_{:.2}_usd_{:.0}_sec", net_pnl, age_secs));
    }

    // Rule 6: Time-weighted Profit Thresholds
    // Younger positions need less profit to close (faster turnover)
    let time_weighted_threshold = if age_secs <= 10.0 {
        min_profit_usd * 0.6
    } else if age_secs <= 20.0 {
        min_profit_usd
    } else if age_secs <= 60.0 {
        min_profit_usd * 0.4
    } else {
        min_profit_usd * 0.2
    };

    // Rule 7: Trend Alignment
    let trend_factor = if let Some(ref strategy_info) = state.strategy_info {
        let trend_bps = strategy_info.trend_bps;
        let position_side_f64 = match position.direction {
            PositionDirection::Long => 1.0,
            PositionDirection::Short => -1.0,
        };
        let trend_aligned = (trend_bps > 0.0 && position_side_f64 > 0.0) || (trend_bps < 0.0 && position_side_f64 < 0.0);
        if trend_aligned {
            1.3 // Trend aligned: hold longer
        } else {
            0.8 // Trend against: close earlier
        }
    } else {
        1.0 // No trend info: neutral
    };

    // Rule 8: Momentum Factor
    let momentum_factor = if state.pnl_history.len() >= 10 {
        let recent = &state.pnl_history[state.pnl_history.len().saturating_sub(10)..];
        let first = recent[0];
        let last = recent[recent.len() - 1];
        if first > Decimal::ZERO {
            let pnl_trend = ((last - first) / first).to_f64().unwrap_or(0.0);
            if pnl_trend > 0.1 {
                1.2 // Strong positive momentum: hold longer
            } else if pnl_trend < -0.1 {
                0.7 // Negative momentum: close earlier
            } else {
                1.0
            }
        } else {
            1.0
        }
    } else {
        1.0
    };

    // Rule 9: Volatility Factor
    let volatility_factor = if let Some(ref strategy_info) = state.strategy_info {
        let volatility = strategy_info.volatility;
        if volatility > 0.05 {
            0.7 // High volatility: close earlier (risk reduction)
        } else if volatility < 0.01 {
            1.2 // Low volatility: hold longer
        } else {
            1.0
        }
    } else {
        1.0
    };

    // Rule 10: Peak PnL Trailing Stop
    let peak_pnl_f64 = state.peak_pnl.to_f64().unwrap_or(0.0);
    let drawdown_from_peak = peak_pnl_f64 - net_pnl;
    let trailing_stop_threshold = min_profit_usd * 0.5;
    let should_close_trailing = peak_pnl_f64 > min_profit_usd && drawdown_from_peak > trailing_stop_threshold;

    // Rule 11: Drawdown
    let max_loss_threshold = -min_profit_usd * 2.0;
    let should_close_drawdown = net_pnl < max_loss_threshold;

    // Rule 12: Recovery (from loss to profit)
    let was_in_loss = state.pnl_history.len() >= 2 && {
        let prev_pnl = state.pnl_history[state.pnl_history.len() - 2].to_f64().unwrap_or(0.0);
        prev_pnl < 0.0
    };
    let should_close_recovery = was_in_loss && net_pnl > 0.0;

    // Combined threshold (time-weighted + factors)
    let combined_threshold = time_weighted_threshold / (trend_factor * momentum_factor * volatility_factor);
    let should_close_by_threshold = net_pnl >= combined_threshold;

    // Return result (priority order)
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

// ============================================================================
// Helper Functions
// ============================================================================

/// Calculate net PnL in USD (with fees)
fn calc_net_pnl_usd(
    entry_price: Decimal,
    exit_price: Decimal,
    qty: Decimal,
    side: &Side,
    entry_fee_bps: f64,
    exit_fee_bps: f64,
) -> f64 {
    let entry_f = entry_price.to_f64().unwrap_or(0.0);
    let exit_f = exit_price.to_f64().unwrap_or(0.0);
    let qty_f = qty.to_f64().unwrap_or(0.0);

    if entry_f <= 0.0 || exit_f <= 0.0 || qty_f <= 0.0 {
        return 0.0;
    }

    let notional = entry_f * qty_f;
    let entry_fee = notional * (entry_fee_bps / 10000.0);
    let exit_fee = notional * (exit_fee_bps / 10000.0);
    let total_fees = entry_fee + exit_fee;

    let price_diff = match side {
        Side::Buy => exit_f - entry_f,  // Long: profit when exit > entry
        Side::Sell => entry_f - exit_f, // Short: profit when exit < entry
    };

    let gross_pnl = price_diff * qty_f;
    gross_pnl - total_fees
}

/// Estimate close fee in BPS
/// Returns exit fee in BPS (maker or taker based on is_maker)
fn estimate_close_fee_bps(is_maker: bool, maker_fee_bps: f64, taker_fee_bps: f64) -> f64 {
    if is_maker {
        maker_fee_bps
    } else {
        taker_fee_bps
    }
}

