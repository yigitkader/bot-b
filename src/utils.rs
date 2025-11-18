
use crate::types::Px;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::{Decimal, RoundingStrategy};
use std::str::FromStr;
use std::time::Duration;

/// Quantize decimal value to step (floor to nearest step multiple)
/// Ensures precision is maintained and result is a multiple of step
pub fn quantize_decimal(value: Decimal, step: Decimal) -> Decimal {
    if step.is_zero() || step.is_sign_negative() {
        return value;
    }

    let ratio = value / step;
    let floored = ratio.floor();
    let result = floored * step;
    let step_scale = step.scale();
    let normalized = result.normalize();

    let rounded = normalized.round_dp_with_strategy(
        step_scale,
        RoundingStrategy::ToNegativeInfinity,
    );

    let re_quantized_ratio = rounded / step;
    let re_quantized_floor = re_quantized_ratio.floor();
    let final_result = re_quantized_floor * step;

    final_result.normalize().round_dp_with_strategy(
        step_scale,
        RoundingStrategy::ToNegativeInfinity,
    )
}

/// Format decimal to fixed precision string
pub fn format_decimal_fixed(value: Decimal, precision: usize) -> String {
    let precision = precision.min(28);
    let scale = precision as u32;
    let normalized = value.normalize();
    let rounded = normalized.round_dp_with_strategy(scale, RoundingStrategy::ToNegativeInfinity);

    if scale == 0 {
        let s = rounded.to_string();
        if let Some(dot_pos) = s.find('.') {
            s[..dot_pos].to_string()
        } else {
            s
        }
    } else {
        let s = rounded.to_string();
        if let Some(dot_pos) = s.find('.') {
            let integer_part = &s[..dot_pos];
            let decimal_part = &s[dot_pos + 1..];
            let current_decimals = decimal_part.len();

            if current_decimals < scale as usize {
                format!(
                    "{}.{}{}",
                    integer_part,
                    decimal_part,
                    "0".repeat(scale as usize - current_decimals)
                )
            } else if current_decimals > scale as usize {
                let truncated_decimal = &decimal_part[..scale as usize];
                format!("{}.{}", integer_part, truncated_decimal)
            } else {
                if decimal_part.len() == scale as usize {
                    s
                } else {
                    format!(
                        "{}.{}{}",
                        integer_part,
                        decimal_part,
                        "0".repeat(scale as usize - decimal_part.len())
                    )
                }
            }
        } else {
            format!("{}.{}", s, "0".repeat(scale as usize))
        }
    }
}

/// Calculate spread in basis points (bps) from bid and ask prices
/// Formula: spread_bps = ((ask - bid) / bid) * 10000
pub fn calculate_spread_bps(bid: Px, ask: Px) -> f64 {
    if bid.0.is_zero() {
        return 0.0;
    }
    let spread_bps = ((ask.0 - bid.0) / bid.0) * Decimal::from(10000);
    spread_bps.to_f64().unwrap_or(0.0)
}

/// Calculate mid price from bid and ask prices
/// Formula: mid_price = (bid + ask) / 2
pub fn calculate_mid_price(bid: Px, ask: Px) -> Decimal {
    (bid.0 + ask.0) / Decimal::from(2)
}

/// Convert f64 percentage to Decimal (for commission rates)
/// Handles conversion with proper fallback
pub fn f64_to_decimal_percent(value: f64, fallback: Decimal) -> Decimal {
    Decimal::from_str(&value.to_string())
        .unwrap_or_else(|_| fallback)
}

/// Convert f64 to Decimal with safe fallback
pub fn f64_to_decimal(value: f64, fallback: Decimal) -> Decimal {
    Decimal::from_str(&value.to_string())
        .unwrap_or_else(|_| fallback)
}

/// Convert f64 to Decimal percentage (divides by 100)
pub fn f64_to_decimal_pct(value: f64) -> Decimal {
    Decimal::from_str(&value.to_string())
        .unwrap_or(Decimal::ZERO) / Decimal::from(100)
}

/// Get commission rate based on order type
pub fn get_commission_rate(is_maker: bool, maker_rate: f64, taker_rate: f64) -> Decimal {
    let rate = if is_maker { maker_rate } else { taker_rate };
    f64_to_decimal_pct(rate)
}

/// Calculate commission amount from notional
pub fn calculate_commission(notional: Decimal, is_maker: bool, maker_rate: f64, taker_rate: f64) -> Decimal {
    let commission_rate = get_commission_rate(is_maker, maker_rate, taker_rate);
    notional * commission_rate
}

/// Calculate total commission for entry and exit
pub fn calculate_total_commission(
    entry_notional: Decimal,
    exit_notional: Decimal,
    entry_is_maker: Option<bool>,
    maker_rate: f64,
    taker_rate: f64,
) -> Decimal {
    let entry_commission = if let Some(is_maker) = entry_is_maker {
        calculate_commission(entry_notional, is_maker, maker_rate, taker_rate)
    } else {
        calculate_commission(entry_notional, false, maker_rate, taker_rate)
    };
    let exit_commission = calculate_commission(exit_notional, false, maker_rate, taker_rate);
    entry_commission + exit_commission
}

/// Calculate exponential backoff delay for retry attempts
/// Formula: base_ms * (multiplier ^ attempt)
/// 
/// # Arguments
/// * `attempt` - Current retry attempt (0-indexed)
/// * `base_ms` - Base delay in milliseconds
/// * `multiplier` - Exponential multiplier (typically 2 or 3)
/// 
/// # Example
/// ```
/// use std::time::Duration;
/// let delay = crate::utils::exponential_backoff(2, 100, 2);
/// // Returns: Duration::from_millis(100 * 2^2) = 400ms
/// ```
pub fn exponential_backoff(attempt: u32, base_ms: u64, multiplier: u64) -> Duration {
    Duration::from_millis(base_ms * multiplier.pow(attempt))
}

pub fn calculate_price_change(
    entry_price: Decimal,
    current_price: Decimal,
    direction: crate::types::PositionDirection,
) -> Decimal {
    if entry_price.is_zero() {
        return Decimal::ZERO;
    }
    if direction == crate::types::PositionDirection::Long {
        (current_price - entry_price) / entry_price
    } else {
        (entry_price - current_price) / entry_price
    }
}

pub fn calculate_pnl_percentage(
    entry_price: Decimal,
    current_price: Decimal,
    direction: crate::types::PositionDirection,
    leverage: u32,
) -> f64 {
    let leverage_decimal = Decimal::from(leverage);
    let price_change = calculate_price_change(entry_price, current_price, direction);
    let pnl_pct = price_change * leverage_decimal * Decimal::from(100);
    pnl_pct.to_f64().unwrap_or(0.0)
}

pub fn calculate_net_pnl(
    entry_price: Decimal,
    exit_price: Decimal,
    qty: Decimal,
    direction: crate::types::PositionDirection,
    leverage: u32,
    entry_commission_pct: Decimal,
    exit_commission_pct: Decimal,
) -> Decimal {
    let notional = entry_price * qty;
    let leverage_decimal = Decimal::from(leverage);
    let price_change = calculate_price_change(entry_price, exit_price, direction);
    let gross_pnl = notional * leverage_decimal * price_change;
    let total_commission = (notional * entry_commission_pct) + (exit_price * qty * exit_commission_pct);
    gross_pnl - total_commission
}

fn error_to_lowercase(error: &anyhow::Error) -> String {
    error.to_string().to_lowercase()
}

pub fn is_position_not_found_error(error: &anyhow::Error) -> bool {
    let error_lower = error_to_lowercase(error);
    error_lower.contains("position not found")
        || error_lower.contains("no position")
        || error_lower.contains("-2011")
}

pub fn is_min_notional_error(error: &anyhow::Error) -> bool {
    let error_lower = error_to_lowercase(error);
    contains_min_notional(&error_lower)
}

pub fn contains_min_notional(text: &str) -> bool {
    let text_lower = text.to_lowercase();
    text_lower.contains("-1013")
        || text_lower.contains("min notional")
        || text_lower.contains("min_notional")
        || text_lower.contains("below min notional")
}

pub fn is_position_already_closed_error(error: &anyhow::Error) -> bool {
    let error_lower = error_to_lowercase(error);
    error_lower.contains("position not found")
        || error_lower.contains("no position")
        || error_lower.contains("position already closed")
        || error_lower.contains("reduceonly")
        || error_lower.contains("reduce only")
        || error_lower.contains("-2011")
        || error_lower.contains("-2019")
        || error_lower.contains("-2021")
}

pub fn is_permanent_error(error: &anyhow::Error) -> bool {
    let error_lower = error_to_lowercase(error);

    error_lower.contains("invalid")
        || error_lower.contains("margin")
        || error_lower.contains("insufficient balance")
        || error_lower.contains("min notional")
        || error_lower.contains("below min notional")
        || error_lower.contains("invalid symbol")
        || error_lower.contains("symbol not found")
        || is_position_not_found_error(error)
        || is_position_already_closed_error(error)
}

pub fn decimal_to_f64(value: Decimal) -> f64 {
    value.to_f64().unwrap_or(0.0)
}

pub fn calculate_percentage(numerator: Decimal, denominator: Decimal) -> Decimal {
    if denominator.is_zero() {
        Decimal::ZERO
    } else {
        (numerator / denominator) * Decimal::from(100)
    }
}

pub fn calculate_dust_threshold(min_notional: Decimal, current_price: Decimal) -> Decimal {
    if !current_price.is_zero() {
        min_notional / current_price
    } else {
        let assumed_min_price = Decimal::new(1, 2);
        min_notional / assumed_min_price
    }
}

