//location: /crates/app/src/risk.rs
// All risk management logic (consolidated from risk.rs, risk_handler.rs, risk_manager.rs)

use crate::config::AppCfg;
use crate::exec::Venue;
use crate::types::*;
use crate::utils::rate_limit_guard;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::time::Instant;
use tracing::{error, info, warn};

// ============================================================================
// Core Risk Types and Limits
// ============================================================================

#[derive(Debug, Clone)]
pub struct RiskLimits {
    pub inv_cap_usd: f64, // ✅ KRİTİK: USD notional tabanlı (fiyat * qty) - base asset miktarı değil!
    pub min_liq_gap_bps: f64,
    pub dd_limit_bps: i64,
    pub max_leverage: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskAction {
    Ok,
    Reduce,
    Halt,
}

/// Risk level for position size
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PositionRiskLevel {
    Ok,
    Soft,   // Block new orders
    Medium, // Reduce existing orders
    Hard,   // Force close
}

// ============================================================================
// Core Risk Checking
// ============================================================================

/// Check risk based on position, inventory, liquidation gap, and drawdown
/// ✅ KRİTİK: inv_cap artık USD notional tabanlı (fiyat * qty) - base asset miktarı değil!
/// Check risk limits for a position
/// ✅ KRİTİK: position_size_notional USD notional bazlı (quote asset'ten bağımsız)
/// inv_cap_usd kontrolü USD notional ile yapılır, USDC/USDT ayrımı yok
/// Her sembol için ayrı state var, BTCUSDC ve BTCUSDT ayrı ayrı kontrol edilir
pub fn check_risk(
    pos: &Position,
    inv: Qty,
    position_size_notional: f64, // ✅ USD notional (mark_price * qty) - quote asset'ten bağımsız
    liq_gap_bps: f64,
    dd_bps: i64,
    lim: &RiskLimits,
) -> RiskAction {
    if dd_bps <= -lim.dd_limit_bps {
        return RiskAction::Halt;
    }
    // ✅ KRİTİK: inv_cap kontrolü artık USD notional tabanlı
    // Örnek: inv_cap_usd = 1000 USD → 0.50 BTC ($40k) = $20k notional > $1k limit → Reduce
    // Aynı 0.50 DOGE ($0.05) = $0.025 notional < $1k limit → OK
    if position_size_notional > lim.inv_cap_usd {
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

// ============================================================================
// Position Size Risk Management
// ============================================================================

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

    let max_position_size_usd = max_usd_per_order
        * effective_leverage
        * cfg.internal.max_position_size_buffer
        * max_position_multiplier;
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
    state
        .active_orders
        .values()
        .map(|order| {
            (order.price.0 * order.remaining_qty.0)
                .to_f64()
                .unwrap_or(0.0)
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
    let should_alert = state
        .last_pnl_alert
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
            state.last_pnl_alert = Some(Instant::now());
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
            state.last_pnl_alert = Some(Instant::now());
        }
    }
}

/// Update peak PnL
pub fn update_peak_pnl(state: &mut SymbolState, current_pnl: Decimal) {
    if current_pnl > state.peak_pnl {
        state.peak_pnl = current_pnl;
    }
}

// ============================================================================
// Risk Level Handling
// ============================================================================

/// Handle risk level actions (Hard, Medium, Soft, Ok)
pub async fn handle_risk_level<V: Venue>(
    venue: &V,
    symbol: &str,
    state: &mut SymbolState,
    risk_level: PositionRiskLevel,
    position_size_notional: f64,
    total_exposure: f64,
    max_allowed: f64,
) -> bool {
    match risk_level {
        PositionRiskLevel::Hard => {
            warn!(
                %symbol,
                position_size_notional,
                total_exposure,
                max_allowed,
                "HARD LIMIT: force closing position"
            );
            // ✅ Thread-safe: Atomik swap - eğer zaten true ise skip eder
            if !state.position_closing.swap(true, Ordering::AcqRel) {
                state.last_close_attempt = Some(Instant::now());

                rate_limit_guard(2).await;
                let _ = venue.cancel_all(symbol).await;
                if venue.close_position(symbol).await.is_ok() {
                    info!(%symbol, "closed position due to hard limit");
                } else {
                    error!(%symbol, "failed to close position due to hard limit");
                }
                state.position_closing.store(false, Ordering::Release);
            } else {
                info!(%symbol, "position close already in progress, skipping duplicate close");
            }
            false
        }
        PositionRiskLevel::Medium => {
            warn!(
                %symbol,
                position_size_notional,
                total_exposure,
                "MEDIUM LIMIT: reducing active orders"
            );
            let mut orders_with_times: Vec<(String, Instant)> = state
                .active_orders
                .iter()
                .map(|(id, o)| (id.clone(), o.created_at))
                .collect();
            orders_with_times.sort_by_key(|(_, t)| *t);

            let to_cancel: Vec<String> = orders_with_times
                .into_iter()
                .take((state.active_orders.len() / 2).max(1))
                .map(|(id, _)| id)
                .collect();

            for order_id in &to_cancel {
                rate_limit_guard(1).await;
                if venue.cancel(order_id, symbol).await.is_ok() {
                    state.active_orders.remove(order_id);
                    state.last_order_price_update.remove(order_id);
                }
            }
            true
        }
        PositionRiskLevel::Soft => {
            info!(
                %symbol,
                position_size_notional,
                total_exposure,
                "SOFT LIMIT: blocking new orders"
            );
            true
        }
        PositionRiskLevel::Ok => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    fn create_test_position(symbol: &str, qty: Decimal, entry: Decimal, leverage: u32) -> Position {
        Position {
            symbol: symbol.to_string(),
            qty: Qty(qty),
            entry: Px(entry),
            leverage,
            liq_px: None,
        }
    }

    fn create_test_limits(
        inv_cap_usd: f64, // ✅ USD notional
        min_liq_gap: f64,
        dd_limit: i64,
        max_leverage: u32,
    ) -> RiskLimits {
        RiskLimits {
            inv_cap_usd, // ✅ USD notional
            min_liq_gap_bps: min_liq_gap,
            dd_limit_bps: dd_limit,
            max_leverage,
        }
    }

    #[test]
    fn test_risk_ok() {
        let pos = create_test_position("BTCUSDT", dec!(0.1), dec!(50000), 5);
        let inv = Qty(dec!(0.05));
        let limits = create_test_limits(10000.0, 300.0, 2000, 10); // ✅ 10000 USD notional limit
        let position_size_notional = (dec!(50000) * dec!(0.1)).to_f64().unwrap_or(0.0); // 5000 USD

        let action = check_risk(&pos, inv, position_size_notional, 500.0, -100, &limits);
        assert_eq!(action, RiskAction::Ok);
    }

    #[test]
    fn test_risk_halt_on_drawdown() {
        let pos = create_test_position("BTCUSDT", dec!(0.1), dec!(50000), 5);
        let inv = Qty(dec!(0.05));
        let limits = create_test_limits(10000.0, 300.0, 2000, 10);
        let position_size_notional = (dec!(50000) * dec!(0.1)).to_f64().unwrap_or(0.0);

        let action = check_risk(&pos, inv, position_size_notional, 500.0, -2500, &limits);
        assert_eq!(action, RiskAction::Halt);
    }

    #[test]
    fn test_risk_reduce_on_inventory_exceeded() {
        let pos = create_test_position("BTCUSDT", dec!(0.1), dec!(50000), 5);
        let limits = create_test_limits(4000.0, 300.0, 2000, 10); // ✅ 4000 USD notional limit
        let inv = Qty(dec!(0.1));
        let position_size_notional = (dec!(50000) * dec!(0.1)).to_f64().unwrap_or(0.0); // 5000 USD > 4000 limit

        let action = check_risk(&pos, inv, position_size_notional, 500.0, -100, &limits);
        assert_eq!(action, RiskAction::Reduce);
    }

    #[test]
    fn test_risk_reduce_on_low_liquidation_gap() {
        let pos = create_test_position("BTCUSDT", dec!(0.1), dec!(50000), 5);
        let inv = Qty(dec!(0.05));
        let limits = create_test_limits(10000.0, 300.0, 2000, 10);
        let position_size_notional = (dec!(50000) * dec!(0.1)).to_f64().unwrap_or(0.0);

        let action = check_risk(&pos, inv, position_size_notional, 200.0, -100, &limits);
        assert_eq!(action, RiskAction::Reduce);
    }

    #[test]
    fn test_risk_reduce_on_leverage_exceeded() {
        let pos = create_test_position("BTCUSDT", dec!(0.1), dec!(50000), 15);
        let inv = Qty(dec!(0.05));
        let limits = create_test_limits(10000.0, 300.0, 2000, 10);
        let position_size_notional = (dec!(50000) * dec!(0.1)).to_f64().unwrap_or(0.0);

        let action = check_risk(&pos, inv, position_size_notional, 500.0, -100, &limits);
        assert_eq!(action, RiskAction::Reduce);
    }
}

// ============================================================================
// Cap Manager Module (from cap_manager.rs)
// ============================================================================

#[derive(Debug, Clone)]
pub struct Caps {
    pub buy_notional: f64,
    pub sell_notional: f64,
    pub buy_total: f64,
    pub sell_total: f64, // ✅ KRİTİK: SELL tarafı için de total margin (futures'ta genellikle buy_total ile aynı)
}

/// Calculate caps for a symbol based on available balance and position
/// ✅ KRİTİK: quote_asset parametresi her sembol için ayrı (BTCUSDC → USDC, BTCUSDT → USDT)
/// quote_balances HashMap'inden doğru quote asset'in bakiyesi alınır
pub fn calculate_caps(
    state: &SymbolState,
    quote_asset: &str, // ✅ Her sembol kendi quote asset'ini kullanır (USDC veya USDT)
    quote_balances: &HashMap<String, f64>, // ✅ Tüm quote asset'lerin bakiyeleri (USDC ve USDT ayrı)
    position_size_notional: f64, // ✅ USD notional (quote asset'ten bağımsız)
    current_pnl: Decimal, // ✅ USD notional (quote asset'ten bağımsız)
    effective_leverage: f64,
    cfg: &AppCfg,
) -> Caps {
    // ✅ KRİTİK: Doğru quote asset'in bakiyesini al (BTCUSDC → USDC, BTCUSDT → USDT)
    let avail = *quote_balances.get(quote_asset).unwrap_or(&0.0);

    if avail < cfg.min_quote_balance_usd {
        info!(
            quote_asset = %quote_asset,
            available_balance = avail,
            min_required = cfg.min_quote_balance_usd,
            "SKIPPING: quote asset balance below minimum threshold"
        );
        return Caps {
            buy_notional: 0.0,
            sell_notional: 0.0,
            buy_total: 0.0,
            sell_total: 0.0,
        };
    }

    // Calculate existing position margin (accounting for unrealized PnL)
    let existing_position_margin = if position_size_notional > 0.0 {
        let base_margin = position_size_notional / effective_leverage;
        let position_pnl = current_pnl.to_f64().unwrap_or(0.0);
        (base_margin - position_pnl).max(0.0)
    } else {
        0.0
    };

    let available_after_position = (avail - existing_position_margin).max(0.0);

    // Calculate effective leverage (opportunity mode reduces leverage)
    let is_opportunity_mode = state.strategy.is_opportunity_mode();
    let effective_leverage_for_caps = if is_opportunity_mode {
        effective_leverage * cfg.internal.opportunity_mode_leverage_reduction
    } else {
        effective_leverage
    };

    // ✅ KRİTİK: Long veya short işlem için maximum 100 USDT/USDC margin limiti
    // Ama hesapta daha az varsa (örn: 30 USD), o zaman mevcut bakiyeyi kullan (30 USD)
    // Yani: min(hesaptaki_bakiye, 100) = max_usable_from_account
    // Örnek: Hesapta 30 USD varsa → 30 USD kullan, 150 USD varsa → 100 USD kullan (limit)
    // Leverage ile notional ne kadar olursa olsun sorun değil, ama margin 100 USD'yi geçmemeli
    let max_usable_from_account = available_after_position.min(cfg.max_usd_per_order);
    let position_size_with_leverage = max_usable_from_account * effective_leverage_for_caps;

    // ✅ KRİTİK: Tek bir chunk için maximum 100 USDT/USDC margin limiti
    let per_order_cap_margin = cfg.max_usd_per_order; // 100 USD hard limit
    let per_order_notional = per_order_cap_margin * effective_leverage_for_caps;

    info!(
        quote_asset = %quote_asset,
        available_balance = avail,
        existing_position_margin,
        available_after_position,
        effective_leverage,
        effective_leverage_for_caps,
        is_opportunity_mode,
        max_usable_from_account,
        position_size_with_leverage,
        per_order_limit_margin_usd = per_order_cap_margin,
        per_order_limit_notional_usd = per_order_notional,
        "calculated futures caps"
    );

    // ✅ KRİTİK: buy_total = sell_total = min(hesaptaki_bakiye, 100)
    // Örnek: Hesapta 30 USD varsa → buy_total = sell_total = 30, 150 USD varsa → buy_total = sell_total = 100
    // Futures'ta her iki yön için de aynı bakiyeyi kullanırız (aynı hesaptan geliyor)
    Caps {
        buy_notional: per_order_notional,
        sell_notional: per_order_notional,
        buy_total: max_usable_from_account, // ✅ min(hesaptaki_bakiye, 100)
        sell_total: max_usable_from_account, // ✅ min(hesaptaki_bakiye, 100) - SELL için de aynı
    }
}

/// Check if caps are sufficient for trading
pub fn check_caps_sufficient(
    caps: &Caps,
    min_usd_per_order: f64,
    min_notional_req: Option<f64>,
) -> (bool, bool) {
    // ✅ KRİTİK DÜZELTME: SELL tarafı için sell_total kullanılmalı (buy_total değil!)
    let buy_cap_ok = caps.buy_total >= min_usd_per_order;
    let sell_cap_ok = caps.sell_total >= min_usd_per_order;

    // Check min notional if available
    if let Some(min_req) = min_notional_req {
        let buy_ok = caps.buy_notional >= min_req;
        let sell_ok = caps.sell_notional >= min_req;
        if !buy_ok && !sell_ok {
            return (false, false);
        }
    }

    (buy_cap_ok, sell_cap_ok)
}
