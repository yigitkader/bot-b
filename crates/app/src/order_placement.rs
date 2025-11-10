//location: /crates/app/src/order_placement.rs
// Unified order placement logic for bid/ask (eliminates code duplication)

use anyhow::Result;
use crate::binance_exec::BinanceFutures;
use crate::core::types::{OrderBook, Px, Qty, Side, Tif};
use crate::exec::{Venue, quant_utils_ceil_to_step, quant_utils_floor_to_step};
use crate::constants::*;
use rust_decimal::prelude::{ToPrimitive, FromStr};
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{error, info, warn};
use crate::config::AppCfg;
use crate::logger;
use crate::types::{OrderInfo, SymbolState};
// Utils functions are called with crate::utils:: prefix to avoid import issues

/// Place orders for a side with profit guarantee check
/// This function unifies bid/ask order placement logic to eliminate duplication
/// 
/// IMPORTANT: Opening LIMIT orders always use GTX (post-only) to guarantee maker fee
/// This prevents accidental taker execution which could eliminate the $0.50 profit margin
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
    _tif: Tif, // Parameter kept for compatibility but not used - opening orders always use GTX
    json_logger: &std::sync::Arc<std::sync::Mutex<logger::JsonLogger>>,
    ob: &OrderBook,
    _maker_fee_rate: f64, // Available but not used (using taker for safety in TP calculation)
    taker_fee_rate: f64,
    min_margin: f64,
) -> Result<()> {
    // KRİTİK: Açılış LIMIT emirleri için her zaman GTX (post-only) kullan
    // Bu, yanlışlıkla taker olma riskini önler ve maker fee garantisi sağlar
    // $0.50 profit margin'ı korumak için zorunlu
    const OPENING_ORDER_TIF: Tif = Tif::PostOnly; // GTX on Binance Futures
    let (px_raw, _qty) = match quote {
        Some(q) => q,
        None => return Ok(()),
    };

    // Price adjustment for aggressiveness (same logic as main.rs)
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
    let px_clamped = crate::utils::adjust_price_for_aggressiveness(
        px_raw.0,
        bid.0,
        ask.0,
        side,
        is_opportunity_mode,
        trend_bps,
        max_distance_pct,
    );
    let px = Px(px_clamped);

    // Get exchange rules (clone to avoid borrow issues)
    let rules_opt = state.symbol_rules.clone();
    let rules = match rules_opt.as_ref() {
        Some(r) => r,
        None => {
            warn!(symbol = %symbol, "no exchange rules available, skipping");
            return Ok(());
        }
    };

    // ✅ Volatility-based chunk sizing - optimized for 100+ trades per day
    // Hedef: 100 USDC'yi 4-5 chunk'a bölmek (her biri 20-25 USDC)
    // base_chunk_size: 20.0 → max_margin_adaptive = 40.0 → 100 USDC → [40, 20, 20, 20] (4 chunks)
    let volatility = state.strategy.get_volatility();
    let base_chunk_size: f64 = 20.0; // ✅ Düşürüldü: 50.0 → 20.0 (daha fazla chunk için)
    let volatility_factor: f64 = if volatility > 0.05 {
        0.6  // Yüksek volatilite: daha küçük chunk'lar
    } else if volatility < 0.01 {
        1.2  // Düşük volatilite: daha büyük chunk'lar
    } else {
        1.0  // Normal volatilite
    };

    let adaptive_chunk_size = base_chunk_size * volatility_factor;
    let min_margin_adaptive = (adaptive_chunk_size * 0.2).max(min_margin).min(100.0);
    // ✅ max_margin_adaptive: adaptive_chunk_size * 2.0 (örn: 20 * 2 = 40)
    // Bu sayede 100 USDC → [40, 20, 20, 20] (4 chunks) veya [40, 30, 30] (3 chunks) olabilir
    let max_margin_adaptive = (adaptive_chunk_size * 2.0).min(cfg.max_usd_per_order).max(10.0);

    let margin_chunks = crate::utils::split_margin_into_chunks(
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

    // Process each chunk
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

        let effective_leverage_for_chunk = if state.strategy.is_opportunity_mode() {
            effective_leverage * cfg.internal.opportunity_mode_leverage_reduction
        } else {
            effective_leverage
        };

        // ✅ Depth analysis için notional hesaplama (çift sayma yok, sadece depth için)
        // margin_chunk: USD (hesaptan çıkan para)
        // chunk_notional_estimate: USD (pozisyon büyüklüğü = margin * leverage)
        // Bu sadece depth analysis için kullanılıyor, gerçek order placement calc_qty_from_margin kullanıyor
        let chunk_notional_estimate = *margin_chunk * effective_leverage_for_chunk;
        let min_required_volume_usd = chunk_notional_estimate * DEPTH_VOLUME_MULTIPLIER;
        let optimal_price_from_depth = crate::utils::find_optimal_price_from_depth(
            ob,
            side,
            min_required_volume_usd,
            bid.0,
            ask.0,
        );

        // Combine strategy price with depth price
        let px_with_depth = match side {
            Side::Buy => px.0.max(optimal_price_from_depth).min(bid.0),
            Side::Sell => px.0.min(optimal_price_from_depth).max(ask.0),
        };

        // Calculate quantity and price
        // ✅ KRİTİK: margin_chunk leverage uygulanmamış margin (USD)
        // calc_qty_from_margin içinde leverage uygulanacak: notional = margin * leverage
        // Bu doğru: margin_chunk leverage uygulanmamış, leverage parametre olarak veriliyor
        let qty_price_result = crate::utils::calc_qty_from_margin(
            *margin_chunk, // Leverage uygulanmamış margin (USD)
            effective_leverage_for_chunk, // Leverage değeri (burada uygulanacak)
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

        // Parse strings to Decimal
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

        // Check min notional
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
        // Note: maker_fee_rate is available but not used in TP calculation (using taker for safety)

        let tp_price_raw = crate::utils::required_take_profit_price(
            side,
            chunk_price.0,
            chunk_qty.0,
            fee_bps_entry,
            fee_bps_exit,
            min_profit_usd,
        );

        // Validate TP price (maker/taker check)
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
                        let actual_min_tp = min_tp_for_maker.max(min_tp_for_profit);
                        (actual_min_tp, Decimal::MAX)
                    }
                    Side::Sell => {
                        let max_tp_for_maker = bid.0 - tick_size;
                        let max_tp_for_profit = quant_utils_floor_to_step(tp, tick_size);
                        let actual_max_tp = max_tp_for_maker.min(max_tp_for_profit);
                        (Decimal::MIN, actual_max_tp)
                    }
                };

                let is_valid = match side {
                    Side::Buy => tp_quantized >= min_tp,
                    Side::Sell => tp_quantized <= max_tp,
                };

                if !is_valid {
                    // Try taker fee TP as fallback
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
                        true // Accept taker TP
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
            crate::utils::rate_limit_guard(1).await;
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

                                crate::utils::rate_limit_guard(1).await;
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
        crate::utils::rate_limit_guard(1).await;

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
                    created_at: std::time::Instant::now(),
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

