//location: /crates/app/src/direction_selector.rs
// Direction selection logic (long/short) based on orderbook imbalance

use crate::types::*;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::time::Instant;
use tracing::debug;

/// Calculate orderbook imbalance and select direction (long/short)
/// Returns the selected direction and applies filter to quotes
pub fn select_direction(
    quotes: &mut Quotes,
    state: &mut SymbolState,
    ob: &crate::types::OrderBook,
    cfg: &crate::config::AppCfg,
) {
    // Calculate orderbook volumes
    let (bid_vol, ask_vol) = if let (Some(ref top_bids), Some(ref top_asks)) = (&ob.top_bids, &ob.top_asks) {
        let bid_vol_sum: Decimal = top_bids.iter().map(|b| b.qty.0).sum();
        let ask_vol_sum: Decimal = top_asks.iter().map(|a| a.qty.0).sum();
        (bid_vol_sum.max(Decimal::ONE), ask_vol_sum.max(Decimal::ONE))
    } else {
        let bid_vol = ob.best_bid.map(|b| b.qty.0).unwrap_or(Decimal::ONE);
        let ask_vol = ob.best_ask.map(|a| a.qty.0).unwrap_or(Decimal::ONE);
        (bid_vol, ask_vol)
    };
    
    // Calculate imbalance ratio
    let imbalance_ratio = if ask_vol > Decimal::ZERO {
        bid_vol / ask_vol
    } else {
        Decimal::ONE
    };
    let imbalance_ratio_f64 = imbalance_ratio.to_f64().unwrap_or(1.0);
    
    // Calculate signal strengths
    let imbalance_long_threshold = cfg.strategy.orderbook_imbalance_long_threshold.unwrap_or(1.2);
    let imbalance_short_threshold = cfg.strategy.orderbook_imbalance_short_threshold.unwrap_or(0.83);
    
    let long_signal_strength = if imbalance_ratio_f64 > imbalance_long_threshold {
        let range = imbalance_long_threshold - 1.0;
        if range > 0.0 {
            ((imbalance_ratio_f64 - 1.0) / range).min(1.0)
        } else {
            0.0
        }
    } else {
        0.0
    };
    
    let short_signal_strength = if imbalance_ratio_f64 < imbalance_short_threshold {
        let range = 1.0 - imbalance_short_threshold;
        if range > 0.0 {
            ((1.0 - imbalance_ratio_f64) / range).min(1.0)
        } else {
            0.0
        }
    } else {
        0.0
    };
    
    // Apply direction selection with cooldown
    let cooldown_secs = cfg.strategy.direction_cooldown_secs.unwrap_or(60);
    let signal_strength_threshold = cfg.strategy.direction_signal_strength_threshold.unwrap_or(0.2);
    
    let now = Instant::now();
    let can_change_direction = state.last_direction_change
        .map(|last| now.duration_since(last).as_secs() >= cooldown_secs)
        .unwrap_or(true);
    
    let new_direction = if long_signal_strength > short_signal_strength + signal_strength_threshold {
        Some(Side::Buy)
    } else if short_signal_strength > long_signal_strength + signal_strength_threshold {
        Some(Side::Sell)
    } else {
        state.current_direction
    };
    
    let direction_changed = new_direction != state.current_direction;
    if direction_changed && can_change_direction {
        state.current_direction = new_direction;
        state.last_direction_change = Some(now);
        state.direction_signal_strength = long_signal_strength.max(short_signal_strength);
        debug!(
            symbol = %state.meta.symbol,
            new_direction = ?new_direction,
            signal_strength = state.direction_signal_strength,
            imbalance_ratio = imbalance_ratio_f64,
            "direction changed (long/short selection)"
        );
    } else if direction_changed && !can_change_direction {
        debug!(
            symbol = %state.meta.symbol,
            requested_direction = ?new_direction,
            "direction change requested but cooldown active"
        );
    }
    
    // Apply direction filter to quotes (only for DynMmStrategy)
    if cfg.strategy.r#type == "dyn_mm" {
        if let Some(dir) = state.current_direction {
            match dir {
                Side::Buy => quotes.ask = None, // Long only
                Side::Sell => quotes.bid = None, // Short only
            }
        } else {
            // First time: apply strong imbalance filter
            let strong_imbalance_long = imbalance_long_threshold + 0.3; // Default: 1.5
            let strong_imbalance_short = imbalance_short_threshold - 0.16; // Default: 0.67
            
            if imbalance_ratio_f64 > strong_imbalance_long {
                quotes.ask = None;
                state.current_direction = Some(Side::Buy);
            } else if imbalance_ratio_f64 < strong_imbalance_short {
                quotes.bid = None;
                state.current_direction = Some(Side::Sell);
            }
        }
    }
}

