//location: /crates/app/src/order.rs
// All order management logic (consolidated from order_manager.rs, order_placement.rs, order_sync.rs)

use crate::config::AppCfg;
use crate::constants::*;
use crate::exchange::BinanceFutures;
use crate::exec::{quant_utils_ceil_to_step, quant_utils_floor_to_step, Venue};
use crate::logger;
use crate::types::*;
use crate::utils::{
    adjust_price_for_aggressiveness, find_optimal_price_from_depth, rate_limit_guard,
    split_margin_into_chunks,
};
use anyhow::Result;
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{debug, error, info, warn};

// ============================================================================
// Order Analysis and Cancellation
// ============================================================================

/// Analyze existing orders and determine which ones should be canceled
/// ✅ KRİTİK: Symbol başına cancel limiti ile rate limit koruması
pub fn analyze_orders(
    state: &SymbolState,
    bid: Px,
    ask: Px,
    position_size_notional: f64,
    cfg: &AppCfg,
) -> Vec<String> {
    // ✅ KRİTİK: Symbol başına bekleyen cancel sayısı limiti (rate limit koruması)
    const MAX_PENDING_CANCELS_PER_SYMBOL: u32 = 3; // Symbol başına maksimum bekleyen cancel
    if state.pending_cancels_count >= MAX_PENDING_CANCELS_PER_SYMBOL {
        debug!(
            pending_cancels = state.pending_cancels_count,
            max_allowed = MAX_PENDING_CANCELS_PER_SYMBOL,
            "cancel limit reached, skipping cancel analysis"
        );
        return Vec::new();
    }

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
            // Cancel limiti kontrolü - limit aşılmışsa daha fazla cancel ekleme
            if orders_to_cancel.len() as u32 + state.pending_cancels_count
                >= MAX_PENDING_CANCELS_PER_SYMBOL
            {
                break; // Limit aşıldı, daha fazla cancel ekleme
            }

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

/// Cancel orders with stagger delay and backoff
/// ✅ KRİTİK: Backoff mekanizması ile cancel churn azaltma (1.5s → 3s → 6s)
pub async fn cancel_orders(
    venue: &BinanceFutures,
    symbol: &str,
    order_ids: &[String],
    state: &mut SymbolState,
    stagger_delay_ms: u64,
    cfg: &AppCfg,
) -> Result<()> {
    if order_ids.is_empty() {
        return Ok(());
    }

    // ✅ KRİTİK: Backoff kontrolü - son cancel'dan bu yana yeterli zaman geçti mi?
    // Base interval config'ten alınır, backoff multiplier ile çarpılır (1.5s → 3s → 6s)
    let base_interval_ms = cfg.exec.cancel_replace_interval_ms;
    let backoff_interval_ms = (base_interval_ms as f64 * state.cancel_backoff_multiplier) as u64;

    if let Some(last_cancel) = state.last_cancel_time {
        let elapsed_ms = last_cancel.elapsed().as_millis() as u64;
        if elapsed_ms < backoff_interval_ms {
            let remaining_ms = backoff_interval_ms - elapsed_ms;
            debug!(
                %symbol,
                pending_cancels = state.pending_cancels_count,
                elapsed_ms,
                backoff_interval_ms,
                remaining_ms,
                "cancel backoff active, skipping cancel batch"
            );
            return Ok(()); // Backoff aktif, cancel yapma
        }
    }

    // Pending cancels sayısını artır
    state.pending_cancels_count += order_ids.len() as u32;
    state.last_cancel_time = Some(Instant::now());

    // Backoff multiplier'ı artır (1.0 → 2.0 → 4.0, max 4.0)
    state.cancel_backoff_multiplier = (state.cancel_backoff_multiplier * 2.0).min(4.0);

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

    // Cancel işlemi tamamlandı, pending count'u azalt
    state.pending_cancels_count = state
        .pending_cancels_count
        .saturating_sub(order_ids.len() as u32);

    // Başarılı cancel sonrası backoff'u azalt (yavaşça normale dön)
    if state.pending_cancels_count == 0 {
        state.cancel_backoff_multiplier = (state.cancel_backoff_multiplier * 0.9).max(1.0);
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
            let api_order_ids: std::collections::HashSet<String> =
                api_orders.iter().map(|o| o.order_id.clone()).collect();

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
                        crate::utils::update_fill_rate_on_cancel(
                            state,
                            cfg.internal.fill_rate_decrease_factor,
                        );
                        info!(
                            %symbol,
                            removed_orders = removed_orders.len(),
                            "orders removed but inventory unchanged - likely canceled"
                        );
                    }
                } else {
                    state.consecutive_no_fills = 0;
                    state.order_fill_rate = (state.order_fill_rate
                        * cfg.internal.fill_rate_increase_factor
                        + cfg.internal.fill_rate_increase_bonus
                            * (removed_orders.len() as f64).min(1.0))
                    .min(1.0);
                    warn!(
                        %symbol,
                        removed_orders = removed_orders.len(),
                        "orders removed but position unavailable, assuming filled"
                    );
                }
            }

            for api_order in &api_orders {
                if !state.active_orders.contains_key(&api_order.order_id) {
                    state.active_orders.insert(
                        api_order.order_id.clone(),
                        OrderInfo {
                            order_id: api_order.order_id.clone(),
                            client_order_id: None,
                            side: api_order.side,
                            price: api_order.price,
                            qty: api_order.qty,
                            filled_qty: Qty(Decimal::ZERO),
                            remaining_qty: api_order.qty,
                            created_at: Instant::now(),
                            last_fill_time: None,
                        },
                    );
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
/// Returns true if an order was actually placed, false otherwise
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
    total_open_chunks: usize, // ✅ KRİTİK: Toplam açık chunk sayısı (açık emirler + aktif pozisyon)
    max_chunks: usize,
    quote_asset: &str,
    quote_balances: &mut HashMap<String, f64>,
    total_spent: &mut f64,
    cfg: &AppCfg,
    _tif: Tif,
    json_logger: &logger::SharedLogger,
    ob: &OrderBook,
    _maker_fee_rate: f64,
    taker_fee_rate: f64,
    min_margin: f64,
) -> Result<bool> {
    const OPENING_ORDER_TIF: Tif = Tif::PostOnly;
    let (px_raw, _qty) = match quote {
        Some(q) => q,
        None => {
            info!(
                %symbol,
                side = ?side,
                "DEBUG: place_orders_with_profit_guarantee skipped - quote is None"
            );
            return Ok(false);
        }
    };

    // ✅ DEBUG: place_orders_with_profit_guarantee başlangıcında logla
    info!(
        %symbol,
        side = ?side,
        px = %px_raw.0,
        available_margin,
        open_orders_count,
        total_open_chunks,
        max_chunks,
        "DEBUG: place_orders_with_profit_guarantee starting"
    );

    let is_opportunity_mode = state.strategy.is_opportunity_mode();
    let base_distance_pct = if position_size_notional > 0.0 {
        cfg.internal.order_price_distance_with_position
    } else {
        cfg.internal.order_price_distance_no_position
    };

    // ✅ KRİTİK: No-fill decay ile quote genişletme birleştir
    // Fill rate düşünce ve consecutive_no_fills artınca spread genişlet
    let fill_rate_factor = if state.order_fill_rate < 0.3 {
        0.5
    } else if state.order_fill_rate < 0.6 {
        0.7
    } else {
        1.0
    };

    // Consecutive no-fills için ek spread genişletme
    let no_fill_multiplier = if state.consecutive_no_fills >= 5 {
        1.3 // 5+ consecutive no-fill → %30 daha geniş spread
    } else if state.consecutive_no_fills >= 3 {
        1.15 // 3+ consecutive no-fill → %15 daha geniş spread
    } else {
        1.0
    };

    // ✅ KRİTİK: Post-only violation sonrası spread widen (ek güvenlik)
    // Post-only violation sonrası cooldown varsa spread'i daha da genişlet
    let post_only_violation_multiplier = if state.post_only_violation_cooldown.is_some() {
        1.5 // %50 daha geniş spread (cooldown sırasında)
    } else {
        1.0
    };

    let max_distance_pct =
        base_distance_pct * fill_rate_factor * no_fill_multiplier * post_only_violation_multiplier;
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

    // ✅ KRİTİK: Per-symbol rules zorunlu - fetch başarısızsa trade etme
    // Global tek order modunda "kuralsız" sembolü tamamen skip et
    let rules_opt = state.symbol_rules.clone();
    let rules = match rules_opt.as_ref() {
        Some(r) => r,
        None => {
            warn!(
                symbol = %symbol,
                disabled = state.disabled,
                rules_fetch_failed = state.rules_fetch_failed,
                "CRITICAL: no exchange rules available, skipping order placement (rules required for trading)"
            );
            return Ok(false);
        }
    };

    // ✅ KRİTİK: Her chunk için maximum 100 USDT/USDC margin limiti (cüzdan kontrolü)
    // Leverage ile notional ne kadar olursa olsun sorun değil, ama margin 100 USD'yi geçmemeli
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
    // ✅ KRİTİK: max_margin_adaptive her zaman cfg.max_usd_per_order (100 USD) ile sınırlandırılmış
    let max_margin_adaptive = (adaptive_chunk_size * 2.0)
        .min(cfg.max_usd_per_order) // ✅ 100 USD hard limit
        .max(10.0);

    let margin_chunks =
        split_margin_into_chunks(available_margin, min_margin_adaptive, max_margin_adaptive);

    if margin_chunks.is_empty() {
        info!(
            %symbol,
            side = ?side,
            available_margin,
            min_margin_adaptive,
            max_margin_adaptive,
            "DEBUG: No margin chunks available - available_margin too small"
        );
        return Ok(false);
    }

    info!(
        %symbol,
        side = ?side,
        available_margin,
        chunks_count = margin_chunks.len(),
        open_orders = open_orders_count,
        total_open_chunks,
        max_chunks,
        "margin chunking for orders"
    );

    // ✅ KRİTİK: max_open_chunks_per_symbol_per_side kontrolü
    // Toplam açık chunk sayısı (açık emirler + aktif pozisyon) max_chunks'ı aşmamalı
    if total_open_chunks >= max_chunks {
        warn!(
            %symbol,
            side = ?side,
            total_open_chunks,
            max_chunks,
            open_orders = open_orders_count,
            "max_open_chunks_per_symbol_per_side limit reached, skipping new order placement"
        );
        return Ok(false);
    }

    // KRİTİK: Bir anda sadece 1 open order veya 1 position olmalı
    // Çok fazla order/position kontrol etmek zor ve çok fazla request gerektirir
    // Bu yüzden chunk'lar sırayla işlenir, her chunk tamamlandıktan sonra bir sonraki chunk işlenir
    // Eğer zaten open order varsa, yeni chunk işleme (bir sonraki tick'te işlenecek)
    if open_orders_count > 0 {
        info!(
            %symbol,
            side = ?side,
            open_orders = open_orders_count,
            "skipping chunk processing: already has open order, will process next chunk when current order closes"
        );
        return Ok(false);
    }

    // ✅ KRİTİK: One-way mode'da karşı yöne emir atarken önce reduce-only emir gönder
    // One-way mode'da (hedge_mode: false) aynı sembolde iki yönlü pozisyon olamaz
    // Net qty > 0 (long) iken SELL açılışını önce reduce, sonra gerekiyorsa yeni yöne aç
    // Net qty < 0 (short) iken BUY açılışını önce reduce, sonra gerekiyorsa yeni yöne aç
    let current_inv = state.inv.0;
    let is_one_way_mode = !cfg.binance.hedge_mode; // One-way mode kontrolü

    if is_one_way_mode && !current_inv.is_zero() {
        let would_flip = (current_inv.is_sign_positive() && side == Side::Sell)
            || (current_inv.is_sign_negative() && side == Side::Buy);

        if would_flip {
            // Karşı yöne emir atıyoruz - önce mevcut pozisyonu reduce et
            info!(
                %symbol,
                side = ?side,
                current_inv = %current_inv,
                "ONE-WAY MODE: closing existing position before opening opposite side order"
            );

            // Market order ile reduce-only emir gönder (pozisyonu kapat)
            // close_position -> flatten_position zaten reduceOnly=true kullanıyor
            rate_limit_guard(1).await;
            match venue.close_position(symbol).await {
                Ok(_) => {
                    info!(
                        %symbol,
                        "ONE-WAY MODE: position closed, can now place opposite side order"
                    );
                    // Pozisyon kapatıldı, state'i güncelle
                    state.inv = Qty(Decimal::ZERO);
                    state.last_inventory_update = Some(Instant::now());
                }
                Err(e) => {
                    warn!(
                        %symbol,
                        error = %e,
                        "ONE-WAY MODE: failed to close position, skipping opposite side order"
                    );
                    return Ok(false); // Close başarısız, karşı yöne emir atma
                }
            }
        }
    }

    // Sadece ilk chunk'ı işle (bir anda sadece 1 order)
    // Diğer chunk'lar bir sonraki tick'lerde, önceki order kapanınca işlenecek
    let margin_chunk = match margin_chunks.first() {
        Some(chunk) => chunk,
        None => return Ok(false),
    };
    let chunk_idx = 0;

    let effective_leverage_for_chunk = if is_opportunity_mode {
        effective_leverage * cfg.internal.opportunity_mode_leverage_reduction
    } else {
        effective_leverage
    };

    let chunk_notional_estimate = *margin_chunk * effective_leverage_for_chunk;
    let min_required_volume_usd = chunk_notional_estimate * DEPTH_VOLUME_MULTIPLIER;
    let optimal_price_from_depth =
        find_optimal_price_from_depth(ob, side, min_required_volume_usd, bid.0, ask.0);

    let px_with_depth = match side {
        Side::Buy => px.0.max(optimal_price_from_depth).min(bid.0),
        Side::Sell => px.0.min(optimal_price_from_depth).max(ask.0),
    };

    // ✅ KRİTİK: Post-only emirler için cross kontrolü ve fiyat ayarlaması (yerleştirme öncesi)
    // Post-only emirler cross ederse taker olur, bu maker fee garantisini bozar
    // Cross ediyorsa fiyatı bir tick dışarı it, tekrar cross etmiyorsa devam et
    let px_final = if matches!(OPENING_ORDER_TIF, Tif::PostOnly) {
        // Effective tick size hesapla - eğer tick_size geçersizse gerçek price'dan hesapla
        let effective_tick_size = if rules.tick_size >= px_with_depth || rules.tick_size >= bid.0 {
            // KRİTİK: API'den gelen price_precision genellikle yanlış, gerçek price'ın decimal basamak sayısını kullan
            let price_str = px_with_depth.to_string();
            let decimal_places = if let Some(dot_pos) = price_str.find('.') {
                let decimal_part = &price_str[dot_pos + 1..];
                // Trailing zero'ları sayma, sadece anlamlı basamakları say
                let significant_digits = decimal_part.trim_end_matches('0').len();
                if significant_digits > 0 {
                    significant_digits.min(8)
                } else {
                    1 // En az 1 basamak
                }
            } else {
                0
            };
            
            if decimal_places > 0 {
                Decimal::new(1, decimal_places as u32)
            } else {
                // Fallback: API'den gelen precision'ı kullan
                let api_precision = rules.price_precision;
                if api_precision > 0 {
                    Decimal::new(1, api_precision as u32)
                } else {
                    Decimal::new(1, 8) // Default: 0.00000001
                }
            }
        } else {
            rules.tick_size
        };

        // Cross kontrolü
        let will_cross = match side {
            Side::Buy => px_with_depth >= ask.0, // Buy order ask'e vuruyor mu?
            Side::Sell => px_with_depth <= bid.0, // Sell order bid'e vuruyor mu?
        };

        if will_cross {
            // ✅ KRİTİK: Fiyatı bir tick dışarı it
            let adjust = effective_tick_size;
            let mut px_adjusted = match side {
                Side::Buy => px_with_depth - adjust,  // Buy: bir tick aşağı
                Side::Sell => px_with_depth + adjust, // Sell: bir tick yukarı
            };

            // ✅ KRİTİK: Tekrar cross kontrolü - hala cross ediyorsa bir tick daha uzaklaştır
            if match side {
                Side::Buy => px_adjusted >= ask.0,
                Side::Sell => px_adjusted <= bid.0,
            } {
                px_adjusted = match side {
                    Side::Buy => px_adjusted - adjust,
                    Side::Sell => px_adjusted + adjust,
                };
            }

            if px_adjusted <= Decimal::ZERO
                || match side {
                    Side::Buy => px_adjusted >= ask.0,
                    Side::Sell => px_adjusted <= bid.0,
                }
            {
                warn!(
                    %symbol,
                    side = ?side,
                    original_price = %px_with_depth,
                    adjusted_price = %px_adjusted,
                    bid = %bid.0,
                    ask = %ask.0,
                    effective_tick_size = %effective_tick_size,
                    api_tick_size = %rules.tick_size,
                    "POST-ONLY VIOLATION: unable to adjust price without crossing, skipping"
                );
                return Ok(false);
            }

            debug!(
                %symbol,
                side = ?side,
                original_price = %px_with_depth,
                adjusted_price = %px_adjusted,
                effective_tick_size = %effective_tick_size,
                "POST-ONLY: price adjusted to prevent cross"
            );

            px_adjusted
        } else {
            px_with_depth
        }
    } else {
        px_with_depth
    };

    // ✅ DOĞRULAMA: Leverage sadece burada uygulanıyor (çift sayma yok)
    // margin_chunk: USD (leverage uygulanmamış)
    // margin_chunk_leveraged: USD (leverage uygulanmış notional)
    // calc_qty_from_margin() içinde leverage UYGULANMAZ, direkt notional kullanılır
    let margin_chunk_leveraged = *margin_chunk * effective_leverage_for_chunk;
    let qty_price_result = crate::utils::calc_qty_from_margin(
        margin_chunk_leveraged, // ✅ Zaten leveraged notional
        px_final,               // ✅ Post-only cross kontrolü sonrası final fiyat
        rules,
        side,
    );

    let (chunk_qty_dec, chunk_price_dec) = match qty_price_result {
        Some((q_dec, p_dec)) => (q_dec, p_dec),
        None => {
            warn!(
                %symbol,
                side = ?side,
                margin_chunk = *margin_chunk,
                "calc_qty_from_margin returned None, skipping chunk"
            );
            return Ok(false);
        }
    };

    let mut chunk_qty = Qty(chunk_qty_dec);
    let chunk_price = Px(chunk_price_dec);

    // ✅ KRİTİK: Quantize sonrası final cross kontrolü
    // Quantize işlemi fiyatı değiştirebilir, tekrar kontrol et
    if matches!(OPENING_ORDER_TIF, Tif::PostOnly) {
        // Effective tick size hesapla - eğer tick_size geçersizse gerçek price'dan hesapla
        let effective_tick_size = if rules.tick_size >= chunk_price.0 || rules.tick_size >= bid.0 {
            // KRİTİK: API'den gelen price_precision genellikle yanlış, gerçek price'ın decimal basamak sayısını kullan
            let price_str = chunk_price.0.to_string();
            let decimal_places = if let Some(dot_pos) = price_str.find('.') {
                let decimal_part = &price_str[dot_pos + 1..];
                // Trailing zero'ları sayma, sadece anlamlı basamakları say
                let significant_digits = decimal_part.trim_end_matches('0').len();
                if significant_digits > 0 {
                    significant_digits.min(8)
                } else {
                    1 // En az 1 basamak
                }
            } else {
                0
            };
            
            if decimal_places > 0 {
                Decimal::new(1, decimal_places as u32)
            } else {
                // Fallback: API'den gelen precision'ı kullan
                let api_precision = rules.price_precision;
                if api_precision > 0 {
                    Decimal::new(1, api_precision as u32)
                } else {
                    Decimal::new(1, 8) // Default: 0.00000001
                }
            }
        } else {
            rules.tick_size
        };
        
        let would_cross_after_quantize = match side {
            Side::Buy => chunk_price.0 >= ask.0,
            Side::Sell => chunk_price.0 <= bid.0,
        };

        if would_cross_after_quantize {
            warn!(
                %symbol,
                side = ?side,
                chunk_price = %chunk_price.0,
                bid = %bid.0,
                ask = %ask.0,
                "POST-ONLY VIOLATION: quantized price crosses market, skipping to prevent taker fill"
            );
            return Ok(false);
        }

        // Ek güvenlik: En az 1 tick mesafe kontrolü (quantize sonrası)
        let min_safe_distance = match side {
            Side::Buy => {
                let min_safe_price = ask.0 - effective_tick_size;
                chunk_price.0 <= min_safe_price
            }
            Side::Sell => {
                let min_safe_price = bid.0 + effective_tick_size;
                chunk_price.0 >= min_safe_price
            }
        };

        if !min_safe_distance {
            warn!(
                %symbol,
                side = ?side,
                chunk_price = %chunk_price.0,
                bid = %bid.0,
                ask = %ask.0,
                effective_tick_size = %effective_tick_size,
                api_tick_size = %rules.tick_size,
                "POST-ONLY SAFETY: quantized price too close to market (less than 1 tick), skipping to prevent cross"
            );
            return Ok(false);
        }
    }

    // ✅ KRİTİK: Quantize sonrası notional hesaplama ve 10-100 USD kuralı doğrulama
    // Quantize işlemi (calc_qty_from_margin) qty ve price'ı yuvarladığı için notional değişebilir
    // Örnek: 15 USD margin → quantize sonrası 8 USD notional olabilir → < 10 USD → skip edilmeli
    let mut chunk_notional = chunk_price.0 * chunk_qty.0;

    // 1. Exchange'in minimum notional gereksinimi (genellikle 5-10 USD)
    let min_notional_req_dec = state
        .min_notional_req
        .map(|v| Decimal::from_f64(v).unwrap_or(Decimal::ZERO))
        .unwrap_or(Decimal::ZERO);

    // 2. Risk sınırları: min_margin (10 USD) ve max_usd_per_order (100 USD)
    // ✅ KRİTİK: Tek bir chunk maximum 100 USDT/USDC margin (cüzdan kontrolü)
    // Leverage ile notional ne kadar olursa olsun sorun değil, ama margin 100 USD'yi geçmemeli
    // ✅ KRİTİK: chunk_notional leverage uygulanmış notional, margin kontrolü için leverage'e böl
    let min_margin_dec = Decimal::from_f64(min_margin).unwrap_or(Decimal::ZERO);
    let max_margin_dec = Decimal::from_f64(cfg.max_usd_per_order).unwrap_or(Decimal::from(100));
    
    // ✅ KRİTİK: chunk_notional leverage uygulanmış notional, margin kontrolü için leverage'e böl
    // Örnek: chunk_notional = 2000 USD (100 USD margin * 20x leverage)
    // chunk_margin = 2000 / 20 = 100 USD → max_margin_dec (100 USD) ile karşılaştır
    let mut chunk_margin = if effective_leverage_for_chunk > 0.0 {
        chunk_notional / Decimal::from_f64_retain(effective_leverage_for_chunk).unwrap_or(Decimal::ONE)
    } else {
        Decimal::ZERO
    };

    // 3. Quantize sonrası notional kontrolü
    // a) Exchange'in minimum notional gereksiniminden küçük olamaz
    if !min_notional_req_dec.is_zero() && chunk_notional < min_notional_req_dec {
        warn!(
            %symbol,
            side = ?side,
            chunk_notional = %chunk_notional,
            min_notional_req = %min_notional_req_dec,
            "chunk notional below exchange min_notional after quantization, skipping"
        );
        return Ok(false);
    }

    // c) Risk sınırı: 100 USD maximum margin (quantize sonrası büyüdüyse qty'yi düşürmeyi dene)
    // ✅ KRİTİK: chunk_margin (leverage'e bölünmüş) ile max_margin_dec karşılaştır
    if chunk_margin > max_margin_dec && chunk_price.0 > Decimal::ZERO {
        // max_margin_dec'den max notional hesapla (leverage ile çarp)
        let max_notional_dec = max_margin_dec * Decimal::from_f64_retain(effective_leverage_for_chunk).unwrap_or(Decimal::ONE);
        let max_qty_raw = max_notional_dec / chunk_price.0;
        let adjusted_qty = quant_utils_floor_to_step(max_qty_raw, rules.step_size);
        if adjusted_qty > Decimal::ZERO && adjusted_qty < chunk_qty.0 {
            debug!(
                %symbol,
                side = ?side,
                original_qty = %chunk_qty.0,
                adjusted_qty = %adjusted_qty,
                max_margin = %max_margin_dec,
                effective_leverage = effective_leverage_for_chunk,
                "reducing quantity to respect max margin limit (after leverage)"
            );
            chunk_qty = Qty(adjusted_qty);
            chunk_notional = chunk_price.0 * chunk_qty.0;
            // chunk_margin'i yeniden hesapla
            chunk_margin = if effective_leverage_for_chunk > 0.0 {
                chunk_notional / Decimal::from_f64_retain(effective_leverage_for_chunk).unwrap_or(Decimal::ONE)
            } else {
                Decimal::ZERO
            };
        }
    }

    // b) Risk sınırı: 10 USD minimum margin (quantize sonrası küçüldüyse skip et)
    // ✅ KRİTİK: chunk_margin (leverage'e bölünmüş) ile min_margin_dec karşılaştır
    if chunk_margin < min_margin_dec {
        warn!(
            %symbol,
            side = ?side,
            chunk_notional = %chunk_notional,
            chunk_margin = %chunk_margin,
            min_margin = %min_margin_dec,
            effective_leverage = effective_leverage_for_chunk,
            margin_chunk = *margin_chunk,
            "chunk margin below 10 USD minimum after quantization, skipping (quantize reduced size)"
        );
        return Ok(false);
    }

    // d) Risk sınırı: 100 USD maximum margin (quantize sonrası hâlâ büyükse skip et)
    // ✅ KRİTİK: chunk_margin (leverage'e bölünmüş) ile max_margin_dec karşılaştır
    if chunk_margin > max_margin_dec {
        warn!(
            %symbol,
            side = ?side,
            chunk_notional = %chunk_notional,
            chunk_margin = %chunk_margin,
            max_margin = %max_margin_dec,
            effective_leverage = effective_leverage_for_chunk,
            margin_chunk = *margin_chunk,
            "chunk margin above 100 USD maximum after quantization, skipping (quantize increased size)"
        );
        return Ok(false);
    }

    // ✅ DOĞRULAMA: Quantize sonrası margin 10-100 USD aralığında (leverage'e bölünmüş)
    debug!(
        %symbol,
        side = ?side,
        chunk_notional = %chunk_notional,
        chunk_margin = %chunk_margin,
        effective_leverage = effective_leverage_for_chunk,
        min_margin = %min_margin_dec,
        max_margin = %max_margin_dec,
        margin_chunk = *margin_chunk,
        "quantize validation passed: margin in 10-100 USD range (after leverage division)"
    );

    // Profit guarantee check with TP calculation
    let fee_bps_entry = fee_rate_to_bps(taker_fee_rate);
    let fee_bps_exit = fee_rate_to_bps(taker_fee_rate);
    let min_profit_usd = cfg
        .strategy
        .min_profit_usd
        .unwrap_or(DEFAULT_MIN_PROFIT_USD);

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
        return Ok(false);
    }

    // Test order for first chunk
    if chunk_idx == 0 && !state.test_order_passed {
        rate_limit_guard(1).await;
        match venue
            .test_order(symbol, side, chunk_price, chunk_qty, OPENING_ORDER_TIF)
            .await
        {
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
                            match venue
                                .test_order(symbol, side, chunk_price, chunk_qty, OPENING_ORDER_TIF)
                                .await
                            {
                                Ok(_) => {
                                    state.test_order_passed = true;
                                    info!(%symbol, side = ?side, "test order passed after rules refresh");
                                }
                                Err(e2) => {
                                    error!(%symbol, side = ?side, error = %e2, "test order still failed, disabling symbol");
                                    state.disabled = true;
                                    state.rules_fetch_failed = true;
                                    return Ok(false);
                                }
                            }
                        }
                        Err(e2) => {
                            error!(%symbol, side = ?side, error = %e2, "failed to refresh rules, disabling symbol");
                            state.disabled = true;
                            state.rules_fetch_failed = true;
                            return Ok(false);
                        }
                    }
                } else {
                    warn!(%symbol, side = ?side, error = %e, "test order failed (non-precision), disabling symbol");
                    state.disabled = true;
                    return Ok(false);
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
    let mut client_order_id = format!(
        "{}_{}_C{}_{}_{}",
        symbol.replace("-", "_").replace("/", "_"),
        side_char,
        chunk_idx,
        timestamp_ms,
        random_suffix
    );
    if client_order_id.len() > 36 {
        let symbol_short = symbol.chars().take(8).collect::<String>();
        client_order_id = format!(
            "{}_{}_C{}_{}",
            symbol_short, side_char, chunk_idx, timestamp_ms
        )
        .chars()
        .take(36)
        .collect::<String>();
    }

    match venue
        .place_limit_with_client_id(
            symbol,
            side,
            chunk_price,
            chunk_qty,
            OPENING_ORDER_TIF,
            &client_order_id,
        )
        .await
    {
        Ok((order_id, _returned_client_id)) => {
            let info = OrderInfo {
                order_id: order_id.clone(),
                client_order_id: _returned_client_id.or(Some(client_order_id)),
                side,
                price: chunk_price,
                qty: chunk_qty,
                filled_qty: Qty(Decimal::ZERO),
                remaining_qty: chunk_qty,
                created_at: Instant::now(),
                last_fill_time: None,
            };
            state.active_orders.insert(order_id.clone(), info.clone());
            state
                .last_order_price_update
                .insert(order_id.clone(), info.price);

            *total_spent += *margin_chunk;

            if let Some(cached_balance) = quote_balances.get_mut(quote_asset) {
                *cached_balance -= *margin_chunk;
                *cached_balance = cached_balance.max(0.0);
            }

            // ✅ KRİTİK: Async-safe logger - lock yok, kanal kullanır (non-blocking)
            json_logger.log_order_created(
                symbol,
                &order_id,
                side,
                chunk_price,
                chunk_qty,
                "spread_opportunity",
                &cfg.exec.tif,
            );

            let chunk_notional_log = (chunk_price.0 * chunk_qty.0).to_f64().unwrap_or(0.0);
            info!(
                %symbol,
                side = ?side,
                chunk_idx,
                order_id,
                margin_chunk = *margin_chunk,
                lev = effective_leverage_for_chunk,
                notional = chunk_notional_log,
                action = if matches!(side, Side::Buy) { "\u{1f7e2} BUY" } else { "\u{1f534} SELL" },
                "order created successfully (chunk)"
            );
            return Ok(true); // Emir başarıyla gönderildi
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

    Ok(false) // Emir gönderilmedi (POST-ONLY kontrolü, quantization hatası, vb.)
}
