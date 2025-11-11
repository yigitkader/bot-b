//location: /crates/app/src/order_manager.rs
// Order placement, management, and analysis logic

use anyhow::Result;
use crate::types::*;
use crate::exec::binance::BinanceFutures;
use crate::exec::Venue;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::str::FromStr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{info, warn};
use crate::config::AppCfg;
use crate::logger;
use crate::types::{OrderInfo, SymbolState};
use crate::utils::{
    adjust_price_for_aggressiveness, calc_qty_from_margin, find_optimal_price_from_depth,
    rate_limit_guard, split_margin_into_chunks,
};

/// Analyze existing orders and determine which ones should be canceled
pub fn analyze_orders(
    state: &SymbolState,
    bid: Px,
    ask: Px,
    position_size_notional: f64,
    cfg: &AppCfg,
) -> Vec<String> {
    let mut orders_to_cancel = Vec::new();

    for (order_id, order) in &state.active_orders {
        let order_price_f64 = match order.price.0.to_f64() {
            Some(price) => price,
            None => {
                warn!(
                    order_id = %order_id,
                    price_decimal = %order.price.0,
                    "Failed to convert order price to f64"
                );
                continue;
            }
        };

        let order_age_ms = order.created_at.elapsed().as_millis() as u64;

        // Calculate market distance
        let market_distance_pct = match order.side {
            Side::Buy => {
                let ask_f64 = ask.0.to_f64().unwrap_or(0.0);
                if ask_f64 > 0.0 {
                    (ask_f64 - order_price_f64) / ask_f64
                } else {
                    0.0
                }
            }
            Side::Sell => {
                let bid_f64 = bid.0.to_f64().unwrap_or(0.0);
                if bid_f64 > 0.0 {
                    (order_price_f64 - bid_f64) / bid_f64
                } else {
                    0.0
                }
            }
        };

        // Determine max distance based on position
        let max_distance_pct = if position_size_notional > 0.0 {
            cfg.internal.order_price_distance_with_position
        } else {
            cfg.internal.order_price_distance_no_position
        };

        let should_cancel_far = market_distance_pct.abs() > max_distance_pct;

        // Check if order is stale
        let max_age_for_stale = if position_size_notional > 0.0 {
            (cfg.exec.max_order_age_ms * 2) / 3
        } else {
            cfg.exec.max_order_age_ms
        };
        let should_cancel_stale = order_age_ms > max_age_for_stale;

        if should_cancel_far || should_cancel_stale {
            orders_to_cancel.push(order_id.clone());
            info!(
                order_id = %order_id,
                side = ?order.side,
                order_price = %order.price.0,
                market_distance_pct = market_distance_pct * 100.0,
                order_age_ms,
                reason = if should_cancel_far { "too_far_from_market" } else { "stale" },
                "intelligent order analysis: canceling order"
            );
        }
    }

    orders_to_cancel
}

/// Cancel orders with stagger delay
pub async fn cancel_orders(
    venue: &BinanceFutures,
    symbol: &str,
    order_ids: &[String],
    state: &mut SymbolState,
    stagger_delay_ms: u64,
) -> Result<()> {
    for (idx, order_id) in order_ids.iter().enumerate() {
        if idx > 0 {
            tokio::time::sleep(Duration::from_millis(stagger_delay_ms)).await;
        }

        rate_limit_guard(1).await;
        if let Err(err) = venue.cancel(order_id, symbol).await {
            warn!(symbol = %symbol, order_id = %order_id, ?err, "failed to cancel order");
        } else {
            state.active_orders.remove(order_id);
            state.last_order_price_update.remove(order_id);
        }
    }

    Ok(())
}

/// Place orders for a side (bid or ask) with chunking
pub async fn place_side_orders(
    venue: &BinanceFutures,
    symbol: &str,
    side: Side,
    quotes: &Option<(Px, Qty)>,
    state: &mut SymbolState,
    bid: Px,
    ask: Px,
    position_size_notional: f64,
    available_margin: f64,
    effective_leverage: f64,
    open_orders_count: usize,
    max_chunks: usize,
    quote_asset: &str,
    quote_balances: &mut HashMap<String, f64>,
    total_spent: &mut f64,
    cfg: &AppCfg,
    tif: Tif,
    json_logger: &std::sync::Arc<std::sync::Mutex<logger::JsonLogger>>,
    ob: &OrderBook,
) -> Result<()> {
    let (px, _qty) = match quotes {
        Some(q) => q,
        None => return Ok(()),
    };

    // Calculate adaptive distance based on fill rate
    let base_distance_pct = if position_size_notional > 0.0 {
        cfg.internal.order_price_distance_with_position
    } else {
        cfg.internal.order_price_distance_no_position
    };

    let fill_rate_factor = if state.order_fill_rate < 0.3 {
        0.5 // Düşük fill rate: %50 daha yakın
    } else if state.order_fill_rate > 0.7 {
        1.5 // Yüksek fill rate: %50 daha uzak
    } else {
        1.0 // Normal
    };

    let adaptive_distance_pct = base_distance_pct / fill_rate_factor;

    // Adjust price for aggressiveness
    let is_opportunity_mode = state.strategy.is_opportunity_mode();
    let trend_bps = state.strategy.get_trend_bps();
    let adjusted_price = adjust_price_for_aggressiveness(
        px.0,
        bid.0,
        ask.0,
        side,
        is_opportunity_mode,
        trend_bps,
        adaptive_distance_pct,
    );

    // Calculate available margin for this side
    let available_margin_for_side = available_margin.min(cfg.max_usd_per_order * 5.0);

    // Split into chunks
    let min_margin = cfg.min_usd_per_order.unwrap_or(10.0);
    let max_margin = cfg.max_usd_per_order;
    let margin_chunks = split_margin_into_chunks(available_margin_for_side, min_margin, max_margin);

    if margin_chunks.is_empty() {
        return Ok(());
    }

    // Get exchange rules (clone to avoid borrow issues)
    let rules = match state.symbol_rules.clone() {
        Some(r) => r,
        None => {
            warn!(symbol = %symbol, "no exchange rules available, skipping order placement");
            return Ok(());
        }
    };

    // Place orders for each chunk
    for (chunk_idx, margin_chunk) in margin_chunks.iter().enumerate() {
        if open_orders_count + chunk_idx >= max_chunks {
            info!(
                symbol = %symbol,
                open_orders = open_orders_count,
                chunk_idx,
                max_chunks,
                "skipping chunk: max open chunks per symbol per side reached"
            );
            break;
        }

        // Calculate effective leverage for chunk
        let effective_leverage_for_chunk = if is_opportunity_mode {
            effective_leverage * cfg.internal.opportunity_mode_leverage_reduction
        } else {
            effective_leverage
        };

        // Find optimal price from depth
        let chunk_notional_estimate = *margin_chunk * effective_leverage_for_chunk;
        let min_required_volume_usd = chunk_notional_estimate * 0.5;
        let optimal_price_from_depth = find_optimal_price_from_depth(
            ob,
            side,
            min_required_volume_usd,
            bid.0,
            ask.0,
        );

        // Use the closer price to market
        let px_with_depth = match side {
            Side::Buy => adjusted_price.max(optimal_price_from_depth).min(bid.0),
            Side::Sell => adjusted_price.min(optimal_price_from_depth).max(ask.0),
        };

        // Calculate quantity and price
        // ✅ KRİTİK: margin_chunk leverage uygulanmamış margin (USD)
        // Leverage'i burada uygulayıp leveraged notional'ı calc_qty_from_margin'a geçiriyoruz
        // calc_qty_from_margin içinde leverage UYGULANMAZ (zaten leveraged geliyor)
        let margin_chunk_leveraged = *margin_chunk * effective_leverage_for_chunk;
        let qty_price_result = calc_qty_from_margin(
            margin_chunk_leveraged, // ZATEN leverage uygulanmış notional (USD)
            px_with_depth,
            &rules,
            side,
        );

        let (qty_str, price_str) = match qty_price_result {
            Some((q, p)) => (q, p),
            None => {
                warn!(
                    symbol = %symbol,
                    chunk_idx,
                    margin_chunk = *margin_chunk,
                    "calc_qty_from_margin returned None, skipping chunk"
                );
                continue;
            }
        };

        // Parse strings to Decimal
        let chunk_qty = match Decimal::from_str(&qty_str) {
            Ok(d) => Qty(d),
            Err(e) => {
                warn!(symbol = %symbol, qty_str, error = %e, "failed to parse qty string");
                continue;
            }
        };

        let chunk_price = match Decimal::from_str(&price_str) {
            Ok(d) => Px(d),
            Err(e) => {
                warn!(symbol = %symbol, price_str, error = %e, "failed to parse price string");
                continue;
            }
        };

        // Check min notional
        let chunk_notional = chunk_price.0 * chunk_qty.0;
        let min_req_dec = Decimal::try_from(state.min_notional_req.unwrap_or(min_margin))
            .unwrap_or(Decimal::ZERO);
        if chunk_notional < min_req_dec {
            warn!(
                symbol = %symbol,
                chunk_notional = %chunk_notional,
                min_req = %min_req_dec,
                "chunk notional below min_notional, skipping"
            );
            continue;
        }

        // Test order for first chunk
        if chunk_idx == 0 && !state.test_order_passed {
            rate_limit_guard(1).await;
            match venue.test_order(symbol, side, chunk_price, chunk_qty, tif).await {
                Ok(_) => {
                    state.test_order_passed = true;
                    info!(
                        symbol = %symbol,
                        "test order passed, proceeding with real orders"
                    );
                }
                Err(e) => {
                    let error_str = e.to_string();
                    let error_lower = error_str.to_lowercase();
                    
                    if error_lower.contains("precision is over") || error_lower.contains("-1111") {
                        warn!(
                            symbol = %symbol,
                            error = %e,
                            "test order failed with -1111, refreshing rules"
                        );
                        
                        match venue.rules_for(symbol).await {
                            Ok(new_rules) => {
                                state.symbol_rules = Some(new_rules);
                                state.rules_fetch_failed = false;
                                state.disabled = false;
                                
                                rate_limit_guard(1).await;
                                match venue.test_order(symbol, side, chunk_price, chunk_qty, tif).await {
                                    Ok(_) => {
                                        state.test_order_passed = true;
                                        info!(symbol = %symbol, "test order passed after rules refresh");
                                    }
                                    Err(e2) => {
                                        warn!(symbol = %symbol, error = %e2, "test order still failed, disabling symbol");
                                        state.disabled = true;
                                        state.rules_fetch_failed = true;
                                        break;
                                    }
                                }
                            }
                            Err(e2) => {
                                warn!(symbol = %symbol, error = %e2, "failed to refresh rules, disabling symbol");
                                state.disabled = true;
                                state.rules_fetch_failed = true;
                                break;
                            }
                        }
                    } else {
                        warn!(symbol = %symbol, error = %e, "test order failed, disabling symbol");
                        state.disabled = true;
                        break;
                    }
                }
            }
        }

        // Place order
        rate_limit_guard(1).await;

        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let random_suffix = (timestamp_ms % 10000) as u64;
        let side_char = match side {
            Side::Buy => "B",
            Side::Sell => "S",
        };
        let client_order_id = format!(
            "{}_{}_C{}_{}_{}",
            symbol.replace("-", "_").replace("/", "_"),
            side_char,
            chunk_idx,
            timestamp_ms,
            random_suffix
        );
        let client_order_id = if client_order_id.len() > 36 {
            let symbol_short = symbol.chars().take(8).collect::<String>();
            format!("{}_{}_C{}_{}", symbol_short, side_char, chunk_idx, timestamp_ms)
                .chars()
                .take(36)
                .collect::<String>()
        } else {
            client_order_id
        };

        match venue
            .place_limit_with_client_id(symbol, side, chunk_price, chunk_qty, tif, &client_order_id)
            .await
        {
            Ok((order_id, returned_client_id)) => {
                let info = OrderInfo {
                    order_id: order_id.clone(),
                    client_order_id: returned_client_id.or(Some(client_order_id)),
                    side,
                    price: chunk_price,
                    qty: chunk_qty,
                    filled_qty: Qty(Decimal::ZERO),
                    remaining_qty: chunk_qty,
                    created_at: Instant::now(),
                    last_fill_time: None,
                };
                state.active_orders.insert(order_id.clone(), info.clone());
                state.last_order_price_update.insert(order_id.clone(), info.price);

                *total_spent += *margin_chunk;

                if let Some(cached_balance) = quote_balances.get_mut(quote_asset) {
                    *cached_balance -= *margin_chunk;
                    *cached_balance = cached_balance.max(0.0);
                }

                if let Ok(logger) = json_logger.lock() {
                    logger.log_order_created(
                        symbol,
                        &order_id,
                        side,
                        chunk_price,
                        chunk_qty,
                        "spread_opportunity",
                        &cfg.exec.tif,
                    );
                }

                let chunk_notional_log = (chunk_price.0 * chunk_qty.0).to_f64().unwrap_or(0.0);
                info!(
                    symbol = %symbol,
                    chunk_idx,
                    order_id,
                    margin_chunk = *margin_chunk,
                    lev = effective_leverage_for_chunk,
                    notional = chunk_notional_log,
                    action = if matches!(side, Side::Buy) { "🟢 BUY" } else { "🔴 SELL" },
                    "order created successfully (chunk)"
                );
            }
            Err(err) => {
                warn!(
                    symbol = %symbol,
                    chunk_idx,
                    margin_chunk = *margin_chunk,
                    error = %err,
                    "failed to place order (chunk)"
                );
            }
        }
    }

    Ok(())
}

/// Update fill rate after order fill
pub fn update_fill_rate_on_fill(
    state: &mut SymbolState,
    increase_factor: f64,
    increase_bonus: f64,
) {
    state.consecutive_no_fills = 0;
    state.last_fill_time = Some(Instant::now());
    state.last_decay_period = None;
    state.last_decay_check = None;
    state.order_fill_rate = (state.order_fill_rate * increase_factor + increase_bonus).min(1.0);
}

/// Update fill rate on order cancel
pub fn update_fill_rate_on_cancel(state: &mut SymbolState, decrease_factor: f64) {
    state.consecutive_no_fills += 1;
    state.order_fill_rate = (state.order_fill_rate * decrease_factor).max(0.0);
}

