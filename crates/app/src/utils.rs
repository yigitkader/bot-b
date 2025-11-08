//location: /crates/app/src/utils.rs
// Utility functions and helpers

use bot_core::types::*;
use exec::binance::SymbolRules;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

// ============================================================================
// Quantity and Price Helpers
// ============================================================================

/// Get quantity step size (per-symbol or fallback)
pub fn get_qty_step(
    symbol_rules: Option<&std::sync::Arc<SymbolRules>>,
    fallback: f64,
) -> f64 {
    symbol_rules
        .map(|r| r.step_size.to_f64().unwrap_or(fallback))
        .unwrap_or(fallback)
}

/// Get price tick size (per-symbol or fallback)
pub fn get_price_tick(
    symbol_rules: Option<&std::sync::Arc<SymbolRules>>,
    fallback: f64,
) -> f64 {
    symbol_rules
        .map(|r| r.tick_size.to_f64().unwrap_or(fallback))
        .unwrap_or(fallback)
}

/// Helper trait for floor division by step
pub trait FloorStep {
    fn floor_div_step(self, step: f64) -> f64;
}

impl FloorStep for f64 {
    fn floor_div_step(self, step: f64) -> f64 {
        if step <= 0.0 {
            return self;
        }
        (self / step).floor() * step
    }
}

/// Clamp quantity by USD value
pub fn clamp_qty_by_usd(qty: Qty, px: Px, max_usd: f64, qty_step: f64) -> Qty {
    let p = px.0.to_f64().unwrap_or(0.0);
    if p <= 0.0 || max_usd <= 0.0 {
        return Qty(Decimal::ZERO);
    }
    let max_qty = (max_usd / p).floor_div_step(qty_step);
    let wanted = qty.0.to_f64().unwrap_or(0.0);
    let q = wanted.min(max_qty);
    Qty(Decimal::from_f64_retain(q).unwrap_or(Decimal::ZERO))
}

/// Clamp quantity by base asset amount
pub fn clamp_qty_by_base(qty: Qty, max_base: f64, qty_step: f64) -> Qty {
    if max_base <= 0.0 {
        return Qty(Decimal::ZERO);
    }
    let max_qty = max_base.floor_div_step(qty_step);
    let wanted = qty.0.to_f64().unwrap_or(0.0);
    let q = wanted.min(max_qty);
    Qty(Decimal::from_f64_retain(q).unwrap_or(Decimal::ZERO))
}

/// Check if asset is a USD stablecoin
pub fn is_usd_stable(asset: &str) -> bool {
    matches!(
        asset.to_uppercase().as_str(),
        "USDT" | "USDC" | "BUSD"
    )
}

// ============================================================================
// PnL Calculation Helpers
// ============================================================================

/// Compute drawdown in basis points from equity history
pub fn compute_drawdown_bps(history: &[Decimal]) -> i64 {
    if history.is_empty() {
        return 0;
    }
    
    let mut peak = history[0];
    let mut max_drawdown = Decimal::ZERO;
    
    for value in history {
        if *value > peak {
            peak = *value;
        }
        if peak > Decimal::ZERO {
            let drawdown = ((*value - peak) / peak) * Decimal::from(10_000i32);
            if drawdown < max_drawdown {
                max_drawdown = drawdown;
            }
        }
    }
    
    max_drawdown.to_i64().unwrap_or(0)
}

/// Record PnL snapshot to history
pub fn record_pnl_snapshot(
    history: &mut Vec<Decimal>,
    pos: &Position,
    mark_px: Px,
    max_len: usize,
) {
    let pnl = (mark_px.0 - pos.entry.0) * pos.qty.0;
    let mut equity = Decimal::ONE + pnl;
    
    // Keep the history strictly positive so drawdown math remains stable
    if equity <= Decimal::ZERO {
        equity = Decimal::new(1, 4);
    }
    
    history.push(equity);
    if history.len() > max_len {
        let excess = history.len() - max_len;
        history.drain(0..excess);
    }
}

