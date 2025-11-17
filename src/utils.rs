// Utility functions shared across modules
// Centralized math and formatting helpers to avoid duplication

use crate::types::Px;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::{Decimal, RoundingStrategy};
use std::str::FromStr;

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

/// Calculate PnL percentage for a position
pub fn calculate_pnl_percentage(
    entry_price: Decimal,
    current_price: Decimal,
    direction: crate::types::PositionDirection,
    leverage: u32,
) -> f64 {
    let leverage_decimal = Decimal::from(leverage);
    let price_change = if direction == crate::types::PositionDirection::Long {
        (current_price - entry_price) / entry_price
    } else {
        (entry_price - current_price) / entry_price
    };
    let pnl_pct = price_change * leverage_decimal * Decimal::from(100);
    pnl_pct.to_f64().unwrap_or(0.0)
}

/// Calculate net PnL with commission
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
    
    let price_change = if direction == crate::types::PositionDirection::Long {
        (exit_price - entry_price) / entry_price
    } else {
        (entry_price - exit_price) / entry_price
    };
    
    let gross_pnl = notional * leverage_decimal * price_change;
    let total_commission = (notional * entry_commission_pct) + (exit_price * qty * exit_commission_pct);
    gross_pnl - total_commission
}

