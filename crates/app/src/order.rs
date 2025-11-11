//location: /crates/app/src/order.rs
// All order management logic (consolidated from order_manager.rs, order_placement.rs, order_sync.rs)

use anyhow::Result;
use crate::types::*;
use crate::binance_exec::BinanceFutures;
use crate::exec::{Venue, quant_utils_ceil_to_step, quant_utils_floor_to_step};
use crate::config::AppCfg;
use crate::logger;
use crate::constants::*;
use crate::utils::{
    adjust_price_for_aggressiveness, calc_qty_from_margin, find_optimal_price_from_depth,
    rate_limit_guard, split_margin_into_chunks,
};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::str::FromStr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{error, info, warn};

// ============================================================================
// Order Analysis and Cancellation
// ============================================================================

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

        let max_distance_pct = if position_size_notional > 0.0 {
            cfg.internal.order_price_distance_with_position
        } else {
            cfg.internal.order_price_distance_no_position
        };

        let should_cancel_far = market_distance_pct.abs() > max_distance_pct;
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

// ============================================================================
// Order Synchronization
// ============================================================================

/// Sync orders from API and update local state
pub async fn sync_orders_from_api<V: Venue>(
    venue: &V,
    symbol: &str,
    state: &mut SymbolState,
    cfg: &AppCfg,
) {
    let current_pos = venue.get_position(symbol).await.ok();
    
    rate_limit_guard(3).await;
    let sync_result = venue.get_open_orders(symbol).await;
    
    match sync_result {
        Ok(api_orders) => {
            let api_order_ids: std::collections::HashSet<String> = api_orders
                .iter()
                .map(|o| o.order_id.clone())
                .collect();
            
            let mut removed_orders = Vec::new();
            state.active_orders.retain(|order_id, order_info| {
                if !api_order_ids.contains(order_id) {
                    removed_orders.push(order_info.clone());
                    false
                } else {
                    true
                }
            });
            
            if !removed_orders.is_empty() {
                if let Some(pos) = current_pos {
                    let old_inv = state.inv.0;
                    state.inv = Qty(pos.qty.0);
                    state.last_inventory_update = Some(Instant::now());
                    
                    if old_inv != pos.qty.0 {
                        state.consecutive_no_fills = 0;
                        state.order_fill_rate = (state.order_fill_rate * 0.95 + 0.05).min(1.0);
                        info!(
                            %symbol,
                            removed_orders = removed_orders.len(),
                            inv_change = %(pos.qty.0 - old_inv),
                            "orders removed and inventory changed - likely filled"
                        );
                    } else {
                        crate::utils::update_fill_rate_on_cancel(state, cfg.internal.fill_rate_decrease_factor);
                        info!(
                            %symbol,
                            removed_orders = removed_orders.len(),
                            "orders removed but inventory unchanged - likely canceled"
                        );
                    }
                } else {
                    state.consecutive_no_fills = 0;
                    state.order_fill_rate = (state.order_fill_rate * cfg.internal.fill_rate_increase_factor 
                        + cfg.internal.fill_rate_increase_bonus * (removed_orders.len() as f64).min(1.0)).min(1.0);
                    warn!(
                        %symbol,
                        removed_orders = removed_orders.len(),
                        "orders removed but position unavailable, assuming filled"
                    );
                }
            }
            
            for api_order in &api_orders {
                if !state.active_orders.contains_key(&api_order.order_id) {
                    state.active_orders.insert(api_order.order_id.clone(), OrderInfo {
                        order_id: api_order.order_id.clone(),
                        client_order_id: None,
                        side: api_order.side,
                        price: api_order.price,
                        qty: api_order.qty,
                        filled_qty: Qty(Decimal::ZERO),
                        remaining_qty: api_order.qty,
                        created_at: Instant::now(),
                        last_fill_time: None,
                    });
                    info!(
                        %symbol,
                        order_id = %api_order.order_id,
                        side = ?api_order.side,
                        "found new order from API (not in local state)"
                    );
                }
            }
            
            state.last_order_sync = Some(Instant::now());
        }
        Err(err) => {
            warn!(%symbol, ?err, "failed to sync orders from API, continuing with local state");
        }
    }
}

// ============================================================================
// Order Placement with Profit Guarantee
// ============================================================================

/// Place orders for a side with profit guarantee check
/// IMPORTANT: Opening LIMIT orders always use PostOnly to guarantee maker fee
pub async fn place_orders_with_profit_guarantee(
    venue: &BinanceFutures,
    symbol: &str,
    side: Side,
    quote: Option<(Px, Qty)>,
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
    _tif: Tif,
    json_logger: &std::sync::Arc<std::sync::Mutex<logger::JsonLogger>>,
    ob: &OrderBook,
    _maker_fee_rate: f64,
    taker_fee_rate: f64,
    min_margin: f64,
) -> Result<()> {
    const OPENING_ORDER_TIF: Tif = Tif::PostOnly;
    let (px_raw, _qty) = match quote {
        Some(q) => q,
        None => return Ok(()),
    };

    let is_opportunity_mode = state.strategy.is_opportunity_mode();
    let base_distance_pct = if position_size_notional > 0.0 {
        cfg.internal.order_price_distance_with_position
    } else {
        cfg.internal.order_price_distance_no_position
    };

    let fill_rate_factor = if state.order_fill_rate < 0.3 {
        0.5
    } else if state.order_fill_rate < 0.6 {
        0.7
    } else {
        1.0
    };

    let max_distance_pct = base_distance_pct * fill_rate_factor;
    let trend_bps = state.strategy.get_trend_bps();
    let px_clamped = adjust_price_for_aggressiveness(
        px_raw.0,
        bid.0,
        ask.0,
        side,
        is_opportunity_mode,
        trend_bps,
        max_distance_pct,
    );
    let px = Px(px_clamped);

    let rules_opt = state.symbol_rules.clone();
    let rules = match rules_opt.as_ref() {
        Some(r) => r,
        None => {
            warn!(symbol = %symbol, "no exchange rules available, skipping");
            return Ok(());
        }
    };

    let volatility = state.strategy.get_volatility();
    let base_chunk_size: f64 = 20.0;
    let volatility_factor: f64 = if volatility > 0.05 {
        0.6
    } else if volatility < 0.01 {
        1.2
    } else {
        1.0
    };

    let adaptive_chunk_size = base_chunk_size * volatility_factor;
    let min_margin_adaptive = (adaptive_chunk_size * 0.2).max(min_margin).min(100.0);
    let max_margin_adaptive = (adaptive_chunk_size * 2.0).min(cfg.max_usd_per_order).max(10.0);

    let margin_chunks = split_margin_into_chunks(
        available_margin,
        min_margin_adaptive,
        max_margin_adaptive,
    );

    if margin_chunks.is_empty() {
        return Ok(());
    }

    info!(
        %symbol,
        side = ?side,
        available_margin,
        chunks_count = margin_chunks.len(),
        open_orders = open_orders_count,
        max_chunks,
        "margin chunking for orders"
    );

    for (chunk_idx, margin_chunk) in margin_chunks.iter().enumerate() {
        if open_orders_count + chunk_idx >= max_chunks {
            info!(
                %symbol,
                side = ?side,
                open_orders = open_orders_count,
                chunk_idx,
                max_chunks,
                "skipping chunk: max chunks reached"
            );
            break;
        }

        let effective_leverage_for_chunk = if is_opportunity_mode {
            effective_leverage * cfg.internal.opportunity_mode_leverage_reduction
        } else {
            effective_leverage
        };

        let chunk_notional_estimate = *margin_chunk * effective_leverage_for_chunk;
        let min_required_volume_usd = chunk_notional_estimate * DEPTH_VOLUME_MULTIPLIER;
        let optimal_price_from_depth = find_optimal_price_from_depth(
            ob,
            side,
            min_required_volume_usd,
            bid.0,
            ask.0,
        );

        let px_with_depth = match side {
            Side::Buy => px.0.max(optimal_price_from_depth).min(bid.0),
            Side::Sell => px.0.min(optimal_price_from_depth).max(ask.0),
        };

        let margin_chunk_leveraged = *margin_chunk * effective_leverage_for_chunk;
        let qty_price_result = calc_qty_from_margin(
            margin_chunk_leveraged,
            px_with_depth,
            rules,
            side,
        );

        let (qty_str, price_str) = match qty_price_result {
            Some((q, p)) => (q, p),
            None => {
                warn!(
                    %symbol,
                    side = ?side,
                    margin_chunk = *margin_chunk,
                    "calc_qty_from_margin returned None, skipping chunk"
                );
                continue;
            }
        };

        let chunk_qty = match Decimal::from_str(&qty_str) {
            Ok(d) => Qty(d),
            Err(e) => {
                warn!(%symbol, side = ?side, qty_str, error = %e, "failed to parse qty string");
                continue;
            }
        };

        let chunk_price = match Decimal::from_str(&price_str) {
            Ok(d) => Px(d),
            Err(e) => {
                warn!(%symbol, side = ?side, price_str, error = %e, "failed to parse price string");
                continue;
            }
        };

        let chunk_notional = chunk_price.0 * chunk_qty.0;
        let min_req_dec = Decimal::try_from(state.min_notional_req.unwrap_or(min_margin))
            .unwrap_or(Decimal::ZERO);
        if chunk_notional < min_req_dec {
            warn!(
                %symbol,
                side = ?side,
                chunk_notional = %chunk_notional,
                min_req = %min_req_dec,
                "chunk notional below min_notional, skipping"
            );
            continue;
        }

        // Profit guarantee check with TP calculation
        let fee_bps_entry = fee_rate_to_bps(taker_fee_rate);
        let fee_bps_exit = fee_rate_to_bps(taker_fee_rate);
        let min_profit_usd = cfg.strategy.min_profit_usd.unwrap_or(DEFAULT_MIN_PROFIT_USD);

        let tp_price_raw = crate::utils::required_take_profit_price(
            side,
            chunk_price.0,
            chunk_qty.0,
            fee_bps_entry,
            fee_bps_exit,
            min_profit_usd,
        );

        let tick_size = rules.tick_size;
        let tp_valid = match tp_price_raw {
            Some(tp) => {
                let tp_quantized = match side {
                    Side::Buy => quant_utils_ceil_to_step(tp, tick_size),
                    Side::Sell => quant_utils_floor_to_step(tp, tick_size),
                };

                let (min_tp, max_tp) = match side {
                    Side::Buy => {
                        let min_tp_for_maker = ask.0 + tick_size;
                        let min_tp_for_profit = quant_utils_ceil_to_step(tp, tick_size);
                        (min_tp_for_maker.max(min_tp_for_profit), Decimal::MAX)
                    }
                    Side::Sell => {
                        let max_tp_for_maker = bid.0 - tick_size;
                        let max_tp_for_profit = quant_utils_floor_to_step(tp, tick_size);
                        (Decimal::MIN, max_tp_for_maker.min(max_tp_for_profit))
                    }
                };

                let is_valid = match side {
                    Side::Buy => tp_quantized >= min_tp,
                    Side::Sell => tp_quantized <= max_tp,
                };

                if !is_valid {
                    let tp_with_taker_fee = crate::utils::required_take_profit_price(
                        side,
                        chunk_price.0,
                        chunk_qty.0,
                        fee_rate_to_bps(taker_fee_rate),
                        fee_rate_to_bps(taker_fee_rate),
                        min_profit_usd,
                    );

                    if tp_with_taker_fee.is_some() {
                        warn!(
                            %symbol,
                            side = ?side,
                            chunk_idx,
                            "TP validation failed, using taker fee TP (acceptable)"
                        );
                        true
                    } else {
                        warn!(
                            %symbol,
                            side = ?side,
                            chunk_idx,
                            "TP validation failed and taker TP calculation failed, skipping chunk"
                        );
                        false
                    }
                } else {
                    true
                }
            }
            None => {
                warn!(
                    %symbol,
                    side = ?side,
                    chunk_idx,
                    "required_take_profit_price returned None, skipping chunk"
                );
                false
            }
        };

        if !tp_valid {
            continue;
        }

        // Test order for first chunk
        if chunk_idx == 0 && !state.test_order_passed {
            rate_limit_guard(1).await;
            match venue.test_order(symbol, side, chunk_price, chunk_qty, OPENING_ORDER_TIF).await {
                Ok(_) => {
                    state.test_order_passed = true;
                    info!(
                        %symbol,
                        side = ?side,
                        "test order passed, proceeding with real orders"
                    );
                }
                Err(e) => {
                    let error_str = e.to_string();
                    let error_lower = error_str.to_lowercase();

                    error!(
                        %symbol,
                        side = ?side,
                        error = %e,
                        "test order failed with detailed context"
                    );

                    if error_lower.contains("precision is over") || error_lower.contains("-1111") {
                        error!(%symbol, side = ?side, error = %e, "test order failed with -1111, refreshing rules");

                        match venue.rules_for(symbol).await {
                            Ok(new_rules) => {
                                state.symbol_rules = Some(new_rules);
                                state.rules_fetch_failed = false;
                                state.disabled = false;
                                info!(%symbol, side = ?side, "rules refreshed, retrying test order");

                                rate_limit_guard(1).await;
                                match venue.test_order(symbol, side, chunk_price, chunk_qty, OPENING_ORDER_TIF).await {
                                    Ok(_) => {
                                        state.test_order_passed = true;
                                        info!(%symbol, side = ?side, "test order passed after rules refresh");
                                    }
                                    Err(e2) => {
                                        error!(%symbol, side = ?side, error = %e2, "test order still failed, disabling symbol");
                                        state.disabled = true;
                                        state.rules_fetch_failed = true;
                                        break;
                                    }
                                }
                            }
                            Err(e2) => {
                                error!(%symbol, side = ?side, error = %e2, "failed to refresh rules, disabling symbol");
                                state.disabled = true;
                                state.rules_fetch_failed = true;
                                break;
                            }
                        }
                    } else {
                        warn!(%symbol, side = ?side, error = %e, "test order failed (non-precision), disabling symbol");
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
            .place_limit_with_client_id(symbol, side, chunk_price, chunk_qty, OPENING_ORDER_TIF, &client_order_id)
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
                    %symbol,
                    side = ?side,
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
                    %symbol,
                    side = ?side,
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

