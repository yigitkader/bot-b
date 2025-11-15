// Utility functions for decimal operations and formatting
// Used across multiple modules (connection, venue, trending)

use rust_decimal::{Decimal, RoundingStrategy};
use std::str::FromStr;

/// Extract precision (decimal places) from a decimal step size
pub fn decimal_places(step: Decimal) -> usize {
    if step.is_zero() {
        return 0;
    }
    let s = step.normalize().to_string();
    if let Some(pos) = s.find('.') {
        s[pos + 1..].trim_end_matches('0').len()
    } else {
        0
    }
}

/// Quantize decimal value to step (floor to nearest step multiple)
///
/// ✅ KRİTİK: Precision loss önleme
/// Decimal division ve multiplication yaparken precision loss olabilir.
/// Sonucu normalize ederek step'in tam katı olduğundan emin oluyoruz.
pub fn quantize_decimal(value: Decimal, step: Decimal) -> Decimal {
    if step.is_zero() || step.is_sign_negative() {
        return value;
    }

    let ratio = value / step;
    let floored = ratio.floor();
    let result = floored * step;
    let step_scale = step.scale();
    let normalized = result.normalize();

    // Round to step's scale first
    let rounded = normalized.round_dp_with_strategy(
        step_scale,
        rust_decimal::RoundingStrategy::ToNegativeInfinity,
    );

    // Double-check quantization - ensure result is a multiple of step
    let re_quantized_ratio = rounded / step;
    let re_quantized_floor = re_quantized_ratio.floor();
    let final_result = re_quantized_floor * step;

    // Final normalization and rounding
    final_result.normalize().round_dp_with_strategy(
        step_scale,
        rust_decimal::RoundingStrategy::ToNegativeInfinity,
    )
}

/// Format decimal with fixed precision
///
/// Prevents precision loss
/// ToZero strategy truncates, which can cause precision loss.
/// Normalize and use correct rounding strategy to prevent precision loss.
pub fn format_decimal_fixed(value: Decimal, precision: usize) -> String {
    let precision = precision.min(28);
    let scale = precision as u32;
    let normalized = value.normalize();
    let rounded =
        normalized.round_dp_with_strategy(scale, RoundingStrategy::ToNegativeInfinity);

    if scale == 0 {
        // Get integer part (truncate if decimal point exists)
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
                // Add missing trailing zeros
                format!(
                    "{}.{}{}",
                    integer_part,
                    decimal_part,
                    "0".repeat(scale as usize - current_decimals)
                )
            } else if current_decimals > scale as usize {
                // If too many decimals, truncate - never show more digits than precision
                // Truncate string - prevents "Precision is over the maximum" error
                let truncated_decimal = &decimal_part[..scale as usize];
                format!("{}.{}", integer_part, truncated_decimal)
            } else {
                // Exact precision - preserve trailing zeros
                // Decimal's to_string() may remove trailing zeros, so check
                if decimal_part.len() == scale as usize {
                    s
                } else {
                    // Add trailing zeros if missing
                    format!(
                        "{}.{}{}",
                        integer_part,
                        decimal_part,
                        "0".repeat(scale as usize - decimal_part.len())
                    )
                }
            }
        } else {
            // No decimal point - add it with zeros
            format!("{}.{}", s, "0".repeat(scale as usize))
        }
    }
}

