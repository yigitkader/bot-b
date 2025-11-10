//location: /crates/app/src/cap_manager.rs
// Cap calculation and balance management logic

use crate::core::types::*;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::collections::HashMap;
use tracing::info;
use crate::config::AppCfg;
use crate::types::SymbolState;

#[derive(Debug, Clone)]
pub struct Caps {
    pub buy_notional: f64,
    pub sell_notional: f64,
    pub buy_total: f64,
}

/// Calculate caps for a symbol based on available balance and position
pub fn calculate_caps(
    state: &SymbolState,
    quote_asset: &str,
    quote_balances: &HashMap<String, f64>,
    position_size_notional: f64,
    current_pnl: Decimal,
    effective_leverage: f64,
    cfg: &AppCfg,
) -> Caps {
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

    // ✅ KRİTİK: Opportunity Mode Leverage Reduction
    // Opportunity mode'da leverage reduction uygulanır (örn: 0.5 → leverage yarıya düşer)
    // Bu reduction SADECE burada (cap_manager) uygulanır, main.rs'de tekrar uygulanmaz
    // Çünkü caps.buy_notional ve caps.sell_notional zaten bu reduction ile hesaplanmıştır
    let is_opportunity_mode = state.strategy.is_opportunity_mode();
    let effective_leverage_for_caps = if is_opportunity_mode {
        effective_leverage * cfg.internal.opportunity_mode_leverage_reduction
    } else {
        effective_leverage
    };

    // ✅ KRİTİK: Margin vs Notional ayrımı
    // - margin: Hesaptan çıkan para (USD)
    // - notional: Pozisyon büyüklüğü (margin * leverage)
    // Örnek: 100 USD margin, 20x leverage → 2000 USD notional
    // Örnek (opportunity mode): 100 USD margin, 20x leverage * 0.5 reduction → 10x → 1000 USD notional
    
    let max_usable_from_account = available_after_position.min(cfg.max_usd_per_order);
    
    // Per-order limits (margin ve notional)
    // ✅ KRİTİK: per_order_notional zaten opportunity mode leverage reduction içeriyor
    // main.rs'de caps.buy_notional/caps.sell_notional kullanılırken leverage ile çarpılmaz
    // Çünkü bu değerler zaten doğru notional değerlerini içeriyor
    let per_order_cap_margin = cfg.max_usd_per_order; // Margin limit (USD)
    let per_order_notional = per_order_cap_margin * effective_leverage_for_caps; // Notional limit (USD) - opportunity mode reduction dahil

    info!(
        quote_asset = %quote_asset,
        available_balance = avail,
        existing_position_margin,
        available_after_position,
        effective_leverage,
        effective_leverage_for_caps,
        is_opportunity_mode,
        max_usable_from_account,
        per_order_limit_margin_usd = per_order_cap_margin,
        per_order_limit_notional_usd = per_order_notional,
        "calculated futures caps"
    );

    Caps {
        buy_notional: per_order_notional,
        sell_notional: per_order_notional,
        buy_total: max_usable_from_account,
    }
}

/// Check if caps are sufficient for trading
pub fn check_caps_sufficient(
    caps: &Caps,
    min_usd_per_order: f64,
    min_notional_req: Option<f64>,
) -> (bool, bool) {
    let buy_cap_ok = caps.buy_total >= min_usd_per_order;
    let sell_cap_ok = caps.buy_total >= min_usd_per_order;

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

