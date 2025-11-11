//location: /crates/app/src/quote_generator.rs
// Strategy quote generation and profit guarantee logic

use anyhow::Result;
use crate::types::*;
use rust_decimal::Decimal;
use crate::strategy::Context;
use tracing::info;
use crate::config::AppCfg;
use crate::types::SymbolState;
use crate::utils::calculate_spread_bps;
use crate::risk::RiskAction;

#[derive(Debug, Clone)]
pub struct Quotes {
    pub bid: Option<(Px, Qty)>,
    pub ask: Option<(Px, Qty)>,
}

/// Generate quotes from strategy
pub fn generate_quotes(
    state: &mut SymbolState,
    ctx: &Context,
    _cfg: &AppCfg,
) -> Result<Quotes> {
    let quotes = state.strategy.on_tick(ctx);
    
    Ok(Quotes {
        bid: quotes.bid,
        ask: quotes.ask,
    })
}

/// Apply risk adjustments to quotes
pub fn apply_risk_adjustments(
    quotes: &mut Quotes,
    risk_action: &RiskAction,
    cfg: &AppCfg,
) {
    match risk_action {
        RiskAction::Reduce => {
            // Reduce is similar to widen but uses order_price_distance_no_position
            let widen = Decimal::from_f64_retain(cfg.internal.order_price_distance_no_position)
                .unwrap_or(Decimal::ZERO);
            quotes.bid = quotes.bid.map(|(px, qty)| (Px(px.0 * (Decimal::ONE - widen)), qty));
            quotes.ask = quotes.ask.map(|(px, qty)| (Px(px.0 * (Decimal::ONE + widen)), qty));
        }
        RiskAction::Widen => {
            let widen = Decimal::from_f64_retain(cfg.internal.spread_widen_factor)
                .unwrap_or(Decimal::ZERO);
            quotes.bid = quotes.bid.map(|(px, qty)| (Px(px.0 * (Decimal::ONE - widen)), qty));
            quotes.ask = quotes.ask.map(|(px, qty)| (Px(px.0 * (Decimal::ONE + widen)), qty));
        }
        RiskAction::Ok | RiskAction::Halt => {}
    }
}

/// Check profit guarantee before placing trades
pub fn check_profit_guarantee(
    quotes: &Quotes,
    position_size_usd: f64,
    profit_guarantee: &crate::utils::ProfitGuarantee,
    cfg: &AppCfg,
) -> (bool, String) {
    let (bid_px, _bid_qty) = match quotes.bid {
        Some((px, qty)) => (px, qty),
        None => return (false, "no_bid_quote".to_string()),
    };

    let (ask_px, _ask_qty) = match quotes.ask {
        Some((px, qty)) => (px, qty),
        None => return (false, "no_ask_quote".to_string()),
    };

    let spread_bps = calculate_spread_bps(bid_px.0, ask_px.0);

    // Calculate dynamic min spread
    // ✅ calculate_min_spread_bps() artık safety margin içeriyor (slippage, partial fill, volatility)
    // Not: slippage_bps_reserve hala config'de var ama calculate_min_spread_bps içinde safety margin zaten var
    let dyn_min_spread_bps = profit_guarantee.calculate_min_spread_bps(position_size_usd);
    let min_spread_bps_config = cfg.strategy.min_spread_bps.unwrap_or(60.0);
    let min_spread_bps = dyn_min_spread_bps.max(min_spread_bps_config);

    let stop_loss_threshold = cfg.internal.stop_loss_threshold;
    let min_risk_reward_ratio = cfg.internal.min_risk_reward_ratio;

    let (should_place, reason) = crate::utils::should_place_trade(
        spread_bps,
        position_size_usd,
        min_spread_bps,
        stop_loss_threshold,
        min_risk_reward_ratio,
        profit_guarantee,
    );

    if !should_place {
        info!(
            spread_bps,
            min_spread_bps,
            position_size_usd,
            reason = %reason,
            "profit guarantee filter: trade rejected"
        );
    }

    (should_place, reason.to_string())
}

