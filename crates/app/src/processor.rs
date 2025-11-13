// Symbol Processing Module
// Consolidates: quote generation, symbol processing, and symbol discovery
// All symbol-related operations in one place for better organization

use crate::config::AppCfg;
use crate::exchange::BinanceFutures;
use crate::exchange::SymbolMeta;
use crate::exec::Venue;
use crate::logger::SharedLogger;
use crate::order::{analyze_orders, cancel_orders, sync_orders_from_api};
use crate::position_manager;
use crate::qmel::QMelStrategy;
use crate::risk::{
    calculate_caps, calculate_total_active_orders_notional, check_caps_sufficient,
    check_pnl_alerts, check_position_size_risk, handle_risk_level, update_peak_pnl,
};
use crate::strategy::{Context, DynMm, DynMmCfg, Strategy};
use crate::types::*;
use crate::utils::{
    adjust_quotes_for_risk, apply_fill_rate_decay, compute_drawdown_bps, fetch_market_data,
    is_usd_stable, place_side_orders, rate_limit_guard, should_sync_orders, validate_quotes,
    ProfitGuarantee,
};
use anyhow::Result;
use futures_util::stream::{self, StreamExt};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, info, warn};

// ============================================================================
// Symbol Processing
// ============================================================================

/// Process a single symbol in the trading loop
pub async fn process_symbol(
    venue: &BinanceFutures,
    symbol: &str,
    quote_asset: &str,
    state: &mut SymbolState,
    bid: Px,
    ask: Px,
    quote_balances: &mut HashMap<String, f64>,
    cfg: &AppCfg,
    risk_limits: &crate::risk::RiskLimits,
    profit_guarantee: &ProfitGuarantee,
    effective_leverage: f64,
    min_usd_per_order: f64,
    tif: Tif,
    json_logger: &SharedLogger,
    force_sync_all: bool,
) -> anyhow::Result<bool> {
    // Sync orders if needed
    if should_sync_orders(state, cfg.internal.order_sync_interval_sec * 1000) || force_sync_all {
        sync_orders_from_api(venue, symbol, state, cfg).await;

        // ✅ KRİTİK: Sync sonrası çift taraf kontrolü - sembol başına tek taraf açık emir
        // Eğer hem BUY hem SELL açık emir varsa, birini iptal et (basit tercih: ilk giren kalsın)
        let buy_exists = state
            .active_orders
            .values()
            .any(|o| matches!(o.side, Side::Buy));
        let sell_exists = state
            .active_orders
            .values()
            .any(|o| matches!(o.side, Side::Sell));

        if buy_exists && sell_exists {
            // Tercih: SELL'i iptal et, BUY kalsın (basit tercih)
            warn!(
                %symbol,
                "both BUY and SELL orders exist, canceling SELL orders (single side rule)"
            );
            // SELL emirlerini iptal et
            let sell_order_ids: Vec<String> = state
                .active_orders
                .iter()
                .filter(|(_, o)| matches!(o.side, Side::Sell))
                .map(|(id, _)| id.clone())
                .collect();

            for order_id in sell_order_ids {
                use crate::utils::rate_limit_guard;
                rate_limit_guard(1).await;
                if let Err(e) = venue.cancel(&order_id, symbol).await {
                    warn!(%symbol, order_id = %order_id, error = %e, "failed to cancel SELL order");
                } else {
                    state.active_orders.remove(&order_id);
                    state.last_order_price_update.remove(&order_id);
                }
            }
        }
    }

    // Fetch market data
    let has_open_orders = !state.active_orders.is_empty();
    let q_free = quote_balances.get(quote_asset).copied().unwrap_or(0.0);
    let has_balance = q_free >= cfg.min_quote_balance_usd && q_free >= min_usd_per_order;
    let (pos, mark_px, funding_rate, next_funding_time) =
        fetch_market_data(venue, symbol, bid, ask, has_balance, has_open_orders).await?;

    // Force close if position exceeded max duration
    let has_position = !pos.qty.0.is_zero();
    if has_position {
        if let Some(entry_time) = state.position_entry_time {
            let age_secs = entry_time.elapsed().as_secs() as f64;
            if age_secs >= crate::constants::MAX_POSITION_DURATION_SEC {
                warn!(%symbol, position_qty = %pos.qty.0, age_secs, "FORCE CLOSE: Position exceeded max duration");
                if !state.position_closing.load(Ordering::Acquire) {
                    let _ = position_manager::close_position(venue, symbol, state).await;
                }
                return Ok(false);
            }
        }
    }

    // Calculate position metrics once (used multiple times)
    let position_size_notional = (mark_px.0 * pos.qty.0.abs()).to_f64().unwrap_or(0.0);
    let current_pnl = (mark_px.0 - pos.entry.0) * pos.qty.0;
    let pnl_f64 = current_pnl.to_f64().unwrap_or(0.0);

    // Cancel stale/far orders
    if !state.active_orders.is_empty() {
        let orders_to_cancel = analyze_orders(state, bid, ask, position_size_notional, cfg);
        if !orders_to_cancel.is_empty() {
            cancel_orders(
                venue,
                symbol,
                &orders_to_cancel,
                state,
                cfg.internal.cancel_stagger_delay_ms,
                cfg,
            )
            .await?;
        }
    }

    // Build orderbook
    let ob = OrderBook {
        best_bid: Some(BookLevel {
            px: bid,
            qty: Qty(Decimal::ONE),
        }),
        best_ask: Some(BookLevel {
            px: ask,
            qty: Qty(Decimal::ONE),
        }),
        top_bids: None,
        top_asks: None,
    };

    // Sync inventory and update position tracking
    let reconcile_threshold = Decimal::from_str(&cfg.internal.inventory_reconcile_threshold)
        .unwrap_or_else(|_| Decimal::new(1, 4));
    position_manager::sync_inventory(state, &pos, force_sync_all, reconcile_threshold, 500);
    position_manager::update_position_tracking(state, &pos, mark_px, cfg);
    position_manager::update_daily_pnl_reset(state);
    position_manager::apply_funding_cost(
        state,
        funding_rate,
        next_funding_time,
        position_size_notional,
    );

    // Risk management
    let total_active_orders_notional = calculate_total_active_orders_notional(state);
    let (risk_level, max_position_size_usd, should_block_new_orders) = check_position_size_risk(
        state,
        position_size_notional,
        total_active_orders_notional,
        cfg.max_usd_per_order,
        effective_leverage,
        cfg,
    );

    if !handle_risk_level(
        venue,
        symbol,
        state,
        risk_level,
        position_size_notional,
        position_size_notional + total_active_orders_notional,
        max_position_size_usd,
    )
    .await
    {
        return Ok(false);
    }

    check_pnl_alerts(state, pnl_f64, position_size_notional, cfg);
    update_peak_pnl(state, current_pnl);

    // Check position close
    let (should_close, reason) = position_manager::should_close_position_smart(
        state,
        &pos,
        mark_px,
        bid,
        ask,
        profit_guarantee.min_profit_usd(),
        profit_guarantee.maker_fee_rate(),
        profit_guarantee.taker_fee_rate(),
    );

    let close_cooldown_ms = cfg.strategy.position_close_cooldown_ms.unwrap_or(500) as u128;
    let can_attempt_close = state
        .last_close_attempt
        .map(|last| Instant::now().duration_since(last).as_millis() >= close_cooldown_ms)
        .unwrap_or(true);

    if should_close && !state.position_closing.load(Ordering::Acquire) && can_attempt_close {
        if position_manager::close_position(venue, symbol, state)
            .await
            .is_ok()
        {
            let side_str = if pos.qty.0.is_sign_positive() {
                "long"
            } else {
                "short"
            };
            let exit_price = if side_str == "long" { bid } else { ask };

            let (realized_pnl, total_fees, net_profit) = crate::utils::calculate_close_pnl(
                pos.entry,
                exit_price,
                pos.qty,
                profit_guarantee.maker_fee_rate(),
            );

            // ✅ KRİTİK: Async-safe logger - lock yok, kanal kullanır (non-blocking)
            json_logger.log_position_closed(
                symbol,
                side_str,
                pos.entry,
                exit_price,
                pos.qty,
                pos.leverage,
                &reason,
            );
            json_logger.log_trade_completed(
                symbol,
                side_str,
                pos.entry,
                exit_price,
                pos.qty,
                total_fees,
                pos.leverage,
            );

            crate::utils::update_trade_stats(
                state,
                net_profit,
                realized_pnl,
                total_fees,
                pos.entry,
                exit_price,
                pos.qty,
                symbol,
            );

            state.strategy.learn_from_trade(net_profit, None, None);

            if state.trade_count > 0 && state.trade_count % 20 == 0 {
                if let Some(top_features) = state.strategy.get_feature_importance() {
                    info!(
                        %symbol,
                        total_trades = state.trade_count,
                        top_5_features = ?top_features.iter().take(5).map(|(name, score)| format!("{}: {:.4}", name, score)).collect::<Vec<_>>(),
                        "feature importance analysis"
                    );
                }
            }

            // PnL summary log
            if state
                .last_pnl_summary_time
                .map(|last| last.elapsed().as_secs() >= 3600 || state.trade_count % 10 == 0)
                .unwrap_or(false)
                && state.trade_count > 0
            {
                let total_profit_f = state.total_profit.to_f64().unwrap_or(0.0);
                let total_loss_f = state.total_loss.to_f64().unwrap_or(0.0);
                let net_pnl_f = total_profit_f - total_loss_f;

                // ✅ KRİTİK: Async-safe logger - lock yok, kanal kullanır (non-blocking)
                json_logger.log_pnl_summary(
                    "hourly",
                    state.trade_count as u32,
                    state.profitable_trade_count as u32,
                    state.losing_trade_count as u32,
                    total_profit_f,
                    total_loss_f,
                    net_pnl_f,
                    state.largest_win.to_f64().unwrap_or(0.0),
                    state.largest_loss.to_f64().unwrap_or(0.0),
                    state.total_fees_paid.to_f64().unwrap_or(0.0),
                );

                info!(
                    %symbol,
                    total_trades = state.trade_count,
                    profitable = state.profitable_trade_count,
                    losing = state.losing_trade_count,
                    total_profit = total_profit_f,
                    total_loss = total_loss_f,
                    net_pnl = net_pnl_f,
                    "PnL summary"
                );

                state.last_pnl_summary_time = Some(Instant::now());
            }

            state.position_entry_time = None;
            state.avg_entry_price = None;
            state.peak_pnl = Decimal::ZERO;
            state.position_hold_duration_ms = 0;
            state.last_logged_position_qty = None;
            state.last_logged_pnl = None;
        }
    }

    // Log position updates
    if has_position {
        let should_log = state
            .last_logged_position_qty
            .map(|last_qty| (last_qty - pos.qty.0).abs() > Decimal::new(1, 8))
            .unwrap_or(true)
            || state
                .last_logged_pnl
                .map(|last_pnl| {
                    let pnl_diff = (current_pnl - last_pnl).abs();
                    let pnl_diff_pct = if last_pnl.abs() > Decimal::ZERO {
                        (pnl_diff / last_pnl.abs()).to_f64().unwrap_or(0.0)
                    } else {
                        1.0
                    };
                    pnl_diff_pct > 0.05
                        || (current_pnl.is_sign_positive() != last_pnl.is_sign_positive())
                })
                .unwrap_or(true);

        if should_log {
            // ✅ KRİTİK: Async-safe logger - lock yok, kanal kullanır (non-blocking)
            let side = if pos.qty.0.is_sign_positive() {
                "long"
            } else {
                "short"
            };
            json_logger.log_position_updated(
                symbol,
                side,
                pos.entry,
                pos.qty,
                mark_px,
                pos.leverage,
            );

            let pnl_trend = state
                .pnl_history
                .len()
                .checked_sub(10)
                .and_then(|start| {
                    let recent = &state.pnl_history[start..];
                    let (first, last) = (recent[0], recent[recent.len() - 1]);
                    (first > Decimal::ZERO)
                        .then(|| ((last - first) / first).to_f64().unwrap_or(0.0))
                })
                .unwrap_or(0.0);

            info!(
                %symbol,
                position_qty = %pos.qty.0,
                entry_price = %pos.entry.0,
                mark_price = %mark_px.0,
                current_pnl = pnl_f64,
                position_size_notional,
                pnl_trend,
                active_orders = state.active_orders.len(),
                order_fill_rate = state.order_fill_rate,
                "position status"
            );

            state.last_logged_position_qty = Some(pos.qty.0);
            state.last_logged_pnl = Some(current_pnl);
            state.last_position_check = Some(Instant::now());
        }
    }

    // Calculate risk metrics
    let liq_gap_bps = if let Some(liq_px) = pos.liq_px {
        let mark = mark_px.0.to_f64().unwrap_or(0.0);
        let liq = liq_px.0.to_f64().unwrap_or(0.0);
        if mark > 0.0 {
            ((mark - liq).abs() / mark) * 10_000.0
        } else {
            crate::constants::DEFAULT_LIQ_GAP_BPS
        }
    } else {
        crate::constants::DEFAULT_LIQ_GAP_BPS
    };

    let dd_bps = compute_drawdown_bps(&state.pnl_history);
    let risk_action = crate::risk::check_risk(&pos, state.inv, liq_gap_bps, dd_bps, risk_limits);

    apply_fill_rate_decay(state, cfg);

    if state.order_fill_rate < crate::constants::LOW_FILL_RATE_THRESHOLD
        && !state.active_orders.is_empty()
    {
        warn!(%symbol, fill_rate = state.order_fill_rate, active_orders = state.active_orders.len(), "low fill rate detected");
    }

    if matches!(risk_action, crate::risk::RiskAction::Halt) {
        warn!(%symbol, "risk halt triggered, cancelling and flattening");
        rate_limit_guard(2).await;
        let _ = Venue::cancel_all(venue, symbol).await;
        let _ = Venue::close_position(venue, symbol).await;
        return Ok(false);
    }

    // ✅ KRİTİK: Per-symbol rules zorunlu - fetch başarısızsa trade etme
    // Global tek order modunda "kuralsız" sembolü tamamen skip et
    if state.disabled || state.rules_fetch_failed || state.symbol_rules.is_none() {
        let should_retry = state
            .last_rules_retry
            .map(|last| last.elapsed().as_secs() >= 45)
            .unwrap_or(true);

        if should_retry {
            state.last_rules_retry = Some(Instant::now());
            match venue.rules_for(symbol).await {
                Ok(new_rules) => {
                    state.symbol_rules = Some(new_rules);
                    state.disabled = false;
                    state.rules_fetch_failed = false;
                    info!(%symbol, "symbol re-enabled after successful rules fetch");
                }
                Err(e) => {
                    warn!(
                        %symbol,
                        error = %e,
                        "failed to fetch rules during retry, symbol remains disabled (will not trade)"
                    );
                    state.rules_fetch_failed = true;
                    state.disabled = true;
                }
            }
        }
        // ✅ KRİTİK: Rules yoksa trade etme - global tek order modunda tamamen skip et
        debug!(
            %symbol,
            disabled = state.disabled,
            rules_fetch_failed = state.rules_fetch_failed,
            has_rules = state.symbol_rules.is_some(),
            "skipping symbol: no rules available (required for trading)"
        );
        return Ok(false);
    }

    // ✅ KRİTİK: Post-only violation cooldown kontrolü
    // Post-only violation sonrası 3-5 saniye cooldown (spread widen için)
    if let Some(cooldown_start) = state.post_only_violation_cooldown {
        const POST_ONLY_VIOLATION_COOLDOWN_SEC: u64 = 4; // 4 saniye cooldown
        let elapsed_sec = cooldown_start.elapsed().as_secs();
        if elapsed_sec < POST_ONLY_VIOLATION_COOLDOWN_SEC {
            debug!(
                %symbol,
                elapsed_sec,
                cooldown_sec = POST_ONLY_VIOLATION_COOLDOWN_SEC,
                "symbol in post-only violation cooldown, skipping (spread will widen)"
            );
            return Ok(false);
        } else {
            // Cooldown bitti, temizle
            state.post_only_violation_cooldown = None;
            info!(
                %symbol,
                "post-only violation cooldown expired, resuming trading with widened spread"
            );
        }
    }

    // KRİTİK: Trend analizi ve order işlemleri birbirini bloklamamalı
    // strategy.on_tick() hızlı çalışır, trend analizi içinde yapılır ama bloklamaz
    // Trend analizi sonuçları priority'yi güncellemek için background'da kullanılır
    let (tick_size_decimal, qty_step_decimal) = if let Some(rules) = state.symbol_rules.as_ref() {
        (rules.tick_size, rules.step_size)
    } else {
        let tick = Decimal::from_f64_retain(cfg.price_tick).unwrap_or(Decimal::ZERO);
        let qty = Decimal::from_f64_retain(cfg.qty_step).unwrap_or(Decimal::ZERO);
        (tick, qty)
    };
    let tick_size_f64 = tick_size_decimal.to_f64().unwrap_or(cfg.price_tick);
    let ob_for_orders = ob.clone();

    // Order placement için context oluştur
    let ctx = Context {
        ob,
        sigma: 0.5,
        inv: state.inv,
        liq_gap_bps,
        funding_rate,
        next_funding_time,
        mark_price: mark_px,
        tick_size: Some(tick_size_decimal),
    };

    // Generate quotes - strategy.on_tick() çağrısı hızlıdır ve order placement'ı bloklamaz
    // Trend analizi strategy içinde yapılır ama bu hızlı bir işlemdir
    let mut quotes = state.strategy.on_tick(&ctx);

    // Trend analizi sonuçlarını background task'a gönder (non-blocking priority update)
    // Bu sayede order placement trend analizini beklemek zorunda kalmaz
    let trend_bps = state.strategy.get_trend_bps();
    let priority_clone = state.priority.clone();
    let symbol_clone = symbol.to_string();

    // ✅ KRİTİK: Trend analizi logları - periyodik olarak logla (her 5 sembolde bir)
    static TREND_LOG_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let trend_log_counter = TREND_LOG_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let should_log_trend = trend_log_counter % 5 == 0; // Her 5 sembolde bir log (daha sık görünür)

    if should_log_trend {
        let trend_direction = if trend_bps > 0.0 {
            "uptrend"
        } else if trend_bps < 0.0 {
            "downtrend"
        } else {
            "neutral"
        };
        let trend_strength = if trend_bps.abs() > 100.0 {
            "strong"
        } else if trend_bps.abs() > 50.0 {
            "medium"
        } else {
            "weak"
        };
        let has_sufficient_data = trend_bps != 0.0 || trend_log_counter % 100 == 0; // Her 100'de bir "no data" logla

        if has_sufficient_data {
            info!(
                %symbol,
                trend_bps = format!("{:.2}", trend_bps),
                trend_direction,
                trend_strength,
                "trend analysis: detected trend signal"
            );
        } else if trend_bps == 0.0 {
            debug!(
                %symbol,
                "trend analysis: insufficient price history data (need at least 10 prices)"
            );
        }
    }

    // Background task: Priority'yi trend'e göre güncelle (order placement'ı bloklamaz)
    tokio::spawn(async move {
        // Trend'e göre priority hesapla (güçlü trend = yüksek priority)
        let old_priority = priority_clone.load(std::sync::atomic::Ordering::Relaxed);
        let new_priority = if trend_bps.abs() > 50.0 {
            // Güçlü trend varsa priority'yi artır
            (trend_bps.abs() / 10.0) as u32
        } else {
            0 // Zayıf trend = düşük priority
        };

        // Priority'yi thread-safe şekilde güncelle (order placement'ı bloklamaz)
        priority_clone.store(new_priority, std::sync::atomic::Ordering::Relaxed);

        // ✅ KRİTİK: Priority güncellemesi logla (sadece değiştiğinde)
        if old_priority != new_priority {
            info!(
                symbol = %symbol_clone,
                old_priority,
                new_priority,
                trend_bps = format!("{:.2}", trend_bps),
                "trend analysis: priority updated based on trend strength"
            );
        }
    });

    // Direction selection is now handled in strategy.on_tick()

    if should_block_new_orders {
        quotes.bid = None;
        quotes.ask = None;
    }

    adjust_quotes_for_risk(
        &mut quotes,
        risk_action,
        cfg.internal.order_price_distance_no_position,
    );

    // Calculate caps
    let caps = calculate_caps(
        state,
        &quote_asset,
        quote_balances,
        position_size_notional,
        current_pnl,
        effective_leverage,
        cfg,
    );

    let (buy_cap_ok, sell_cap_ok) =
        check_caps_sufficient(&caps, min_usd_per_order, state.min_notional_req);

    if !buy_cap_ok && !sell_cap_ok {
        info!(%symbol, buy_total = caps.buy_total, min_usd_per_order, "skip tick: insufficient balance");
        return Ok(true);
    }

    if !buy_cap_ok {
        quotes.bid = None;
    }
    if !sell_cap_ok {
        quotes.ask = None;
    }

    // Profit guarantee filter
    if let (Some((bid_px, bid_qty)), Some((ask_px, ask_qty))) = (quotes.bid, quotes.ask) {
        let spread_bps = crate::utils::calculate_spread_bps(bid_px.0, ask_px.0);
        let position_size_usd = bid_px.0.to_f64().unwrap_or(0.0)
            * bid_qty
                .0
                .to_f64()
                .unwrap_or(0.0)
                .max(ask_px.0.to_f64().unwrap_or(0.0) * ask_qty.0.to_f64().unwrap_or(0.0));

        let min_spread_bps = profit_guarantee
            .calculate_min_spread_bps(position_size_usd)
            .max(cfg.strategy.min_spread_bps.unwrap_or(30.0));

        let should_place = crate::utils::should_place_trade(
            spread_bps,
            position_size_usd,
            min_spread_bps,
            cfg.internal.stop_loss_threshold,
            cfg.internal.min_risk_reward_ratio,
            profit_guarantee,
        )
        .0;

        if !should_place {
            // ✅ KRİTİK: Async-safe logger - lock yok, kanal kullanır (non-blocking)
            json_logger.log_trade_rejected(
                symbol,
                "not_profitable",
                spread_bps,
                position_size_usd,
                min_spread_bps,
            );
            info!(
                %symbol,
                spread_bps = format!("{:.2}", spread_bps),
                min_spread_bps = format!("{:.2}", min_spread_bps),
                position_size_usd = format!("{:.2}", position_size_usd),
                "DEBUG: Trade rejected by profit guarantee filter"
            );
            quotes.bid = None;
            quotes.ask = None;
        }
    }

    let qty_step_f64 = qty_step_decimal.to_f64().unwrap_or(cfg.qty_step);
    let qty_step_dec = qty_step_decimal;

    // ✅ KRİTİK: Early exit - bakiyesi olmayan quote için emir üretme (churn önleme)
    // USDC/USDT karışık keşifte hesap bakiyesini doğru quote'a göre filtrele
    let quote_balance = quote_balances.get(quote_asset).copied().unwrap_or(0.0);
    let has_sufficient_balance =
        quote_balance >= cfg.min_quote_balance_usd && quote_balance >= min_usd_per_order;

    if !state.active_orders.is_empty() || has_position {
        // Açık emir veya pozisyon varsa devam et (bakiye kontrolü yapma - zaten işlem var)
    } else if !has_sufficient_balance {
        // Bakiye yok ve açık emir/pozisyon yok - emir üretme (churn önleme)
        debug!(
            %symbol,
            quote_asset = %quote_asset,
            balance = quote_balance,
            min_required = cfg.min_quote_balance_usd.max(min_usd_per_order),
            "skipping order placement: insufficient balance and no active orders/position"
        );
        quotes.bid = None;
        quotes.ask = None;
    }

    // ✅ DEBUG: validate_quotes öncesi durumu logla
    let bid_before = quotes.bid.clone();
    let ask_before = quotes.ask.clone();

    validate_quotes(
        &mut quotes,
        &caps,
        qty_step_f64,
        qty_step_dec,
        min_usd_per_order,
    );

    // ✅ DEBUG: Quote validation sonrası durumu logla
    let has_bid = quotes.bid.is_some();
    let has_ask = quotes.ask.is_some();

    info!(
        %symbol,
        bid_before = bid_before.is_some(),
        ask_before = ask_before.is_some(),
        bid_after = has_bid,
        ask_after = has_ask,
        buy_notional = caps.buy_notional,
        sell_notional = caps.sell_notional,
        min_usd_per_order,
        "DEBUG: Quote validation result"
    );

    if !has_bid && !has_ask {
        info!(
            %symbol,
            quote_balance,
            buy_cap_ok,
            sell_cap_ok,
            buy_total = caps.buy_total,
            buy_notional = caps.buy_notional,
            sell_notional = caps.sell_notional,
            min_usd_per_order,
            "DEBUG: No quotes available after validation - checking why orders aren't placed"
        );
    }

    // ✅ KRİTİK: Global tek-işlem kilidi - aynı anda sadece 1 open_order veya 1 position
    // Global order lock ile tüm semboller arasında aynı anda sadece bir "exposure" garantisi
    // Bu sayede eşzamanlılık ve churn azalır, request sayısı düşer
    use crate::utils::with_order_lock;

    with_order_lock(async {
        // ✅ KRİTİK: Aynı tick'te yalnız bir taraf siparişi
        // Eğer zaten açık emir veya pozisyon varsa, yeni emir yerleştirme
        // Bu kontrol gereksiz test_order ve place_limit çağrılarını önler
        if !state.active_orders.is_empty() || !state.inv.0.is_zero() {
            debug!(
                %symbol,
                open_orders = state.active_orders.len(),
                inv = %state.inv.0,
                "skipping order placement: already has open order or position (reducing API calls)"
            );
            return Ok(true);
        }

        // ✅ KRİTİK: Tek taraf seçimi - trend bazlı
        // Trend pozitifse Buy, negatifse Sell tercih et
        let trend_bps = state.strategy.get_trend_bps();
        let prefer_side = if trend_bps > 0.0 {
            Side::Buy // Uptrend → Buy tercih et
        } else if trend_bps < 0.0 {
            Side::Sell // Downtrend → Sell tercih et
        } else {
            // Trend yok, spread'e göre karar ver
            let spread_bps = if bid.0 > Decimal::ZERO && ask.0 > Decimal::ZERO {
                ((ask.0 - bid.0) / bid.0 * Decimal::from(10000))
                    .to_f64()
                    .unwrap_or(0.0)
            } else {
                0.0
            };
            // Spread büyükse her iki taraf da riskli, küçükse Buy tercih et
            if spread_bps > 50.0 {
                // Spread çok büyük, emir yerleştirme
                debug!(
                    %symbol,
                    spread_bps = format!("{:.2}", spread_bps),
                    "trend analysis: spread too wide, skipping order placement"
                );
                return Ok(true);
            } else {
                Side::Buy // Default: Buy tercih et
            }
        };

        // ✅ KRİTİK: Trend bazlı karar verme logla
        info!(
            %symbol,
            trend_bps = format!("{:.2}", trend_bps),
            prefer_side = ?prefer_side,
            has_bid = quotes.bid.is_some(),
            has_ask = quotes.ask.is_some(),
            "trend analysis: selected side based on trend"
        );

        let mut total_spent_on_bids = 0.0f64;
        let mut total_spent_on_asks = 0.0f64;
        let mut placed = false;

        // ✅ DEBUG: with_order_lock içinde quote durumunu logla
        info!(
            %symbol,
            has_bid = quotes.bid.is_some(),
            has_ask = quotes.ask.is_some(),
            prefer_side = ?prefer_side,
            "DEBUG: Inside with_order_lock - checking quotes before placement"
        );

        // ✅ KRİTİK: Sadece bir taraf yerleştir
        if quotes.bid.is_some() && quotes.ask.is_some() {
            // Her iki taraf da mevcut - trend bazlı seç
            info!(
                %symbol,
                prefer_side = ?prefer_side,
                "DEBUG: Both bid and ask available, selecting based on trend"
            );
            match prefer_side {
                Side::Buy => {
                    let bid_quote = quotes.bid.clone();
                    info!(
                        %symbol,
                        has_bid_quote = bid_quote.is_some(),
                        "DEBUG: Calling place_side_orders for Buy side"
                    );
                    place_side_orders(
                        venue,
                        symbol,
                        Side::Buy,
                        bid_quote,
                        state,
                        bid,
                        ask,
                        position_size_notional,
                        &caps,
                        &mut total_spent_on_bids,
                        0.0,
                        effective_leverage,
                        quote_asset,
                        quote_balances,
                        cfg,
                        tif,
                        json_logger,
                        &ob_for_orders,
                        profit_guarantee,
                    )
                    .await?;
                    placed = true;
                }
                Side::Sell => {
                    let ask_quote = quotes.ask.clone();
                    info!(
                        %symbol,
                        has_ask_quote = ask_quote.is_some(),
                        "DEBUG: Calling place_side_orders for Sell side"
                    );
                    place_side_orders(
                        venue,
                        symbol,
                        Side::Sell,
                        ask_quote,
                        state,
                        bid,
                        ask,
                        position_size_notional,
                        &caps,
                        &mut total_spent_on_asks,
                        0.0,
                        effective_leverage,
                        quote_asset,
                        quote_balances,
                        cfg,
                        tif,
                        json_logger,
                        &ob_for_orders,
                        profit_guarantee,
                    )
                    .await?;
                    placed = true;
                }
            }
        } else if quotes.bid.is_some() {
            let bid_quote = quotes.bid.clone();
            info!(
                %symbol,
                has_bid_quote = bid_quote.is_some(),
                "DEBUG: Only bid available, calling place_side_orders for Buy side"
            );
            place_side_orders(
                venue,
                symbol,
                Side::Buy,
                bid_quote,
                state,
                bid,
                ask,
                position_size_notional,
                &caps,
                &mut total_spent_on_bids,
                0.0,
                effective_leverage,
                quote_asset,
                quote_balances,
                cfg,
                tif,
                json_logger,
                &ob_for_orders,
                profit_guarantee,
            )
            .await?;
            placed = true;
        } else if quotes.ask.is_some() {
            let ask_quote = quotes.ask.clone();
            info!(
                %symbol,
                has_ask_quote = ask_quote.is_some(),
                "DEBUG: Only ask available, calling place_side_orders for Sell side"
            );
            place_side_orders(
                venue,
                symbol,
                Side::Sell,
                ask_quote,
                state,
                bid,
                ask,
                position_size_notional,
                &caps,
                &mut total_spent_on_asks,
                0.0,
                effective_leverage,
                quote_asset,
                quote_balances,
                cfg,
                tif,
                json_logger,
                &ob_for_orders,
                profit_guarantee,
            )
            .await?;
            placed = true;
        } else {
            // ✅ DEBUG: Neden emir yerleştirilmediğini logla
            info!(
                %symbol,
                has_bid = quotes.bid.is_some(),
                has_ask = quotes.ask.is_some(),
                prefer_side = ?prefer_side,
                trend_bps = format!("{:.2}", trend_bps),
                quote_balance,
                buy_cap_ok,
                sell_cap_ok,
                "DEBUG: No quotes available to place orders - both bid and ask are None"
            );
        }

        if placed {
            info!(
                %symbol,
                side = ?prefer_side,
                trend_bps = format!("{:.2}", trend_bps),
                "order placed (single side only)"
            );
        }

        Ok(true)
    })
    .await
}

// ============================================================================
// Symbol Discovery
// ============================================================================

/// Discover and filter symbols based on configuration
pub async fn discover_symbols(
    venue: &BinanceFutures,
    cfg: &AppCfg,
    metadata: &[SymbolMeta],
) -> Result<Vec<SymbolMeta>> {
    let mut requested: Vec<String> = cfg.symbols.clone();
    if let Some(sym) = cfg.symbol.clone() {
        requested.push(sym);
    }

    let mut normalized = Vec::new();
    for sym in requested {
        let s = sym.trim().to_uppercase();
        if s.is_empty() {
            continue;
        }
        if !normalized.iter().any(|existing: &String| existing == &s) {
            normalized.push(s);
        }
    }

    let mut selected: Vec<SymbolMeta> = Vec::new();
    for sym in &normalized {
        if !sym.is_ascii() {
            warn!(
                symbol = %sym,
                "skipping symbol with non-ASCII characters"
            );
            continue;
        }

        if let Some(meta) = metadata.iter().find(|m| &m.symbol == sym) {
            let exact_quote = meta.quote_asset.eq_ignore_ascii_case(&cfg.quote_asset);
            let group_quote = is_usd_stable(&cfg.quote_asset) && is_usd_stable(&meta.quote_asset);
            if !(exact_quote || group_quote) {
                warn!(
                    symbol = %sym,
                    quote_asset = %meta.quote_asset,
                    required_quote = %cfg.quote_asset,
                    "skipping configured symbol that is not in required quote group"
                );
                continue;
            }
            if let Some(status) = meta.status.as_deref() {
                if status != "TRADING" {
                    warn!(symbol = %sym, status, "skipping configured symbol that is not trading");
                    continue;
                }
            }
            match meta.contract_type.as_deref() {
                Some("PERPETUAL") => {}
                Some(other) => {
                    warn!(symbol = %sym, contract_type = %other, "skipping non-perpetual futures symbol");
                    continue;
                }
                None => {
                    warn!(symbol = %sym, "skipping futures symbol with missing contract type metadata");
                    continue;
                }
            }

            let have_min = cfg.min_usd_per_order.unwrap_or(0.0);
            let avail = match venue.available_balance(&meta.quote_asset).await {
                Ok(b) => b.to_f64().unwrap_or(0.0),
                Err(_) => {
                    warn!(
                        symbol = %meta.symbol,
                        quote_asset = %meta.quote_asset,
                        "Failed to get balance, using 0.0"
                    );
                    0.0
                }
            };
            if avail < have_min {
                warn!(
                    symbol = %sym,
                    quote = %meta.quote_asset,
                    avail,
                    min_needed = have_min,
                    "skipping symbol at discovery: zero/low quote balance"
                );
                continue;
            }

            selected.push(meta.clone());
        } else {
            warn!(symbol = %sym, "configured symbol not found on venue");
        }
    }

    if selected.is_empty() && cfg.auto_discover_quote {
        selected = auto_discover_symbols(venue, cfg, metadata).await?;
    }

    Ok(selected)
}

/// Wait for balance to become available and retry symbol discovery
pub async fn wait_and_retry_discovery(
    venue: &BinanceFutures,
    cfg: &AppCfg,
    metadata: &[SymbolMeta],
) -> Result<Vec<SymbolMeta>> {
    loop {
        use tokio::time::{sleep, Duration};
        sleep(Duration::from_secs(
            cfg.internal.symbol_discovery_retry_interval_sec,
        ))
        .await;

        let retry_selected = discover_symbols(venue, cfg, metadata).await?;

        if !retry_selected.is_empty() {
            info!(
                count = retry_selected.len(),
                quote_asset = %cfg.quote_asset,
                "balance became available, proceeding with symbol initialization"
            );
            return Ok(retry_selected);
        } else {
            info!(
                quote_asset = %cfg.quote_asset,
                min_required = cfg.min_quote_balance_usd,
                "still waiting for balance to become available..."
            );
        }
    }
}

/// Auto-discover symbols based on quote asset
async fn auto_discover_symbols(
    venue: &BinanceFutures,
    cfg: &AppCfg,
    metadata: &[SymbolMeta],
) -> Result<Vec<SymbolMeta>> {
    let want_group = is_usd_stable(&cfg.quote_asset);
    let mut auto: Vec<SymbolMeta> = metadata
        .iter()
        .filter(|m| {
            let match_primary_quote = if want_group {
                is_usd_stable(&m.quote_asset)
            } else {
                m.quote_asset.eq_ignore_ascii_case(&cfg.quote_asset)
            };

            let match_cross_quote = if cfg.allow_usdt_quote {
                let cfg_quote_upper = cfg.quote_asset.to_uppercase();
                if cfg_quote_upper == "USDC" {
                    m.quote_asset.eq_ignore_ascii_case("USDT")
                } else if cfg_quote_upper == "USDT" {
                    m.quote_asset.eq_ignore_ascii_case("USDC")
                } else {
                    false
                }
            } else {
                false
            };

            let match_quote = match_primary_quote || match_cross_quote;

            // ✅ KRİTİK: Sadece USDⓈ-M Futures (PERPETUAL) seç
            // fapi.binance.com endpoint'i USDⓈ-M Futures için kullanılır (hem USDT hem USDC destekler)
            match_quote
                && m.status.as_deref().map(|s| s == "TRADING").unwrap_or(true)
                && m.contract_type
                    .as_deref()
                    .map(|ct| ct == "PERPETUAL")
                    .unwrap_or(false) // USDⓈ-M Futures only
        })
        .cloned()
        .collect();

    // Filter by balance
    let mut quote_asset_balances: HashMap<String, f64> = HashMap::new();
    let unique_quotes: HashSet<String> = auto.iter().map(|m| m.quote_asset.clone()).collect();

    for quote in unique_quotes {
        let balance = venue
            .available_balance(&quote)
            .await
            .map(|b| b.to_f64().unwrap_or(0.0))
            .unwrap_or(0.0);
        quote_asset_balances.insert(quote.clone(), balance);

        if balance < cfg.min_quote_balance_usd {
            info!(
                quote_asset = %quote,
                balance,
                min_required = cfg.min_quote_balance_usd,
                "FILTERING: quote asset balance insufficient"
            );
        }
    }

    auto.retain(|m| {
        if let Some(&balance) = quote_asset_balances.get(&m.quote_asset) {
            balance >= cfg.min_quote_balance_usd
        } else {
            false
        }
    });

    auto.retain(|m| m.symbol.is_ascii());
    auto.sort_by(|a, b| a.symbol.cmp(&b.symbol));

    info!(
        count = auto.len(),
        quote_asset = %cfg.quote_asset,
        "auto-discovered symbols"
    );

    Ok(auto)
}

/// Initialize symbol states with strategies
pub fn initialize_symbol_states(
    selected: Vec<SymbolMeta>,
    dyn_cfg: &DynMmCfg,
    strategy_name: &str,
    cfg: &AppCfg,
) -> Vec<SymbolState> {
    let build_strategy = |symbol: &str| -> Box<dyn Strategy> {
        let dyn_cfg_clone = dyn_cfg.clone();
        match strategy_name {
            "dyn_mm" => Box::new(DynMm::from(dyn_cfg_clone)),
            "qmel" => {
                let maker_fee = cfg.strategy.maker_fee_rate.unwrap_or(0.0001);
                let taker_fee = cfg.strategy.taker_fee_rate.unwrap_or(0.0004);
                let ev_threshold = cfg.strategy.qmel_ev_threshold.unwrap_or(0.10);
                let min_margin = cfg.strategy.qmel_min_margin_usdc.unwrap_or(10.0);
                let max_margin = cfg.strategy.qmel_max_margin_usdc.unwrap_or(100.0);
                let max_leverage = cfg.risk.max_leverage as f64;
                Box::new(QMelStrategy::new(
                    maker_fee,
                    taker_fee,
                    ev_threshold,
                    min_margin,
                    max_margin,
                    max_leverage,
                ))
            }
            other => {
                warn!(symbol = %symbol, strategy = %other, "unknown strategy type, defaulting dyn_mm");
                Box::new(DynMm::from(dyn_cfg_clone))
            }
        }
    };

    let mut states = Vec::new();
    for meta in selected {
        let strategy = build_strategy(&meta.symbol);
        states.push(SymbolState {
            meta,
            inv: Qty(Decimal::ZERO),
            strategy,
            active_orders: HashMap::new(),
            pnl_history: Vec::new(),
            min_notional_req: None,
            disabled: false,
            symbol_rules: None,
            rules_fetch_failed: false,
            last_rules_retry: None,
            test_order_passed: false,
            last_position_check: None,
            last_logged_position_qty: None,
            last_logged_pnl: None,
            last_order_sync: None,
            order_fill_rate: cfg.internal.initial_fill_rate,
            consecutive_no_fills: 0,
            last_fill_time: None,
            last_inventory_update: None,
            last_decay_period: None,
            last_decay_check: None,
            post_only_violation_cooldown: None,
            pending_cancels_count: 0,
            last_cancel_time: None,
            cancel_backoff_multiplier: 1.0,
            position_entry_time: None,
            peak_pnl: Decimal::ZERO,
            position_hold_duration_ms: 0,
            last_order_price_update: HashMap::new(),
            daily_pnl: Decimal::ZERO,
            total_funding_cost: Decimal::ZERO,
            position_size_notional_history: Vec::with_capacity(
                cfg.internal.position_size_history_max_len,
            ),
            last_pnl_alert: None,
            cumulative_pnl: Decimal::ZERO,
            last_applied_funding_time: None,
            last_daily_reset_at: None,
            // PnL tracking for summary
            trade_count: 0,
            profitable_trade_count: 0,
            losing_trade_count: 0,
            total_profit: Decimal::ZERO,
            total_loss: Decimal::ZERO,
            largest_win: Decimal::ZERO,
            largest_loss: Decimal::ZERO,
            total_fees_paid: Decimal::ZERO,
            last_pnl_summary_time: None,
            avg_entry_price: None,
            position_closing: Arc::new(AtomicBool::new(false)),
            last_close_attempt: None,
            processed_events: HashSet::new(),
            last_event_cleanup: None,
            priority: Arc::new(std::sync::atomic::AtomicU32::new(0)), // Default priority, updated by trend analysis
        });
    }

    states
}

/// Setup margin type and leverage for all symbols
/// OPTIMIZED: Uses parallel processing with controlled concurrency for faster setup
pub async fn setup_margin_and_leverage(
    venue: &BinanceFutures,
    states: &mut [SymbolState],
    cfg: &AppCfg,
) -> Result<()> {
    if cfg.mode != "futures" {
        return Ok(());
    }

    let use_isolated = cfg.risk.use_isolated_margin;
    let leverage_to_set = cfg.exec.default_leverage.or(cfg.leverage).unwrap_or(1);

    // Parallel processing configuration
    const CONCURRENT_LIMIT: usize = 10;

    let venue = Arc::new(venue.clone());
    let total_symbols = states.len();

    if use_isolated {
        info!(
            total_symbols = total_symbols,
            "setting isolated margin for all symbols (parallel processing)"
        );

        let symbols: Vec<String> = states.iter().map(|s| s.meta.symbol.clone()).collect();
        let venue_clone = venue.clone();

        let results: Vec<_> = stream::iter(symbols.iter())
            .map(|symbol| {
                let venue = venue_clone.clone();
                let symbol = symbol.clone();
                async move {
                    rate_limit_guard(1).await;
                    match venue.get_margin_type(&symbol).await {
                        Ok(current_is_isolated) => {
                            if current_is_isolated == use_isolated {
                                return (symbol, Ok(true), true);
                            }
                        }
                        Err(e) => {
                            warn!(%symbol, error = %e, "failed to get margin type, will attempt to set anyway");
                        }
                    }

                    rate_limit_guard(1).await;
                    match venue.set_margin_type(&symbol, true).await {
                        Ok(_) => (symbol, Ok(true), false),
                        Err(err) => {
                            let error_str = err.to_string();
                            let error_lower = error_str.to_lowercase();

                            if error_lower.contains("-4046") || error_lower.contains("no need to change") {
                                (symbol, Ok(true), true)
                            } else {
                                (symbol, Err(err), false)
                            }
                        }
                    }
                }
            })
            .buffer_unordered(CONCURRENT_LIMIT)
            .collect()
            .await;

        let mut isolated_set_count = 0;
        let mut isolated_skip_count = 0;
        let mut isolated_fail_count = 0;

        for (symbol, result, was_skip) in results {
            match result {
                Ok(_) => {
                    if was_skip {
                        isolated_skip_count += 1;
                        debug!(%symbol, "margin type already set to isolated");
                    } else {
                        isolated_set_count += 1;
                        debug!(%symbol, "isolated margin set successfully");
                    }
                }
                Err(err) => {
                    isolated_fail_count += 1;
                    warn!(%symbol, error = %err, "failed to set isolated margin");
                }
            }
        }

        info!(
            total = total_symbols,
            isolated_set = isolated_set_count,
            isolated_skip = isolated_skip_count,
            isolated_fail = isolated_fail_count,
            "isolated margin setup completed"
        );
    }

    // Set leverage - OPTIMIZED: Skip get_leverage check, just try to set
    info!(
        total_symbols = total_symbols,
        leverage = leverage_to_set,
        "setting leverage for all symbols (parallel processing, skipping pre-check)"
    );

    let symbols: Vec<String> = states.iter().map(|s| s.meta.symbol.clone()).collect();
    let venue_clone = venue.clone();

    let results: Vec<_> = stream::iter(symbols.iter())
        .map(|symbol| {
            let venue = venue_clone.clone();
            let symbol = symbol.clone();
            async move {
                rate_limit_guard(1).await;
                match venue.set_leverage(&symbol, leverage_to_set).await {
                    Ok(_) => (symbol, Ok(true), false),
                    Err(err) => {
                        let error_str = err.to_string();
                        let error_lower = error_str.to_lowercase();

                        if error_lower.contains("-4059")
                            || error_lower.contains("no need to change")
                            || error_lower.contains("leverage not modified")
                        {
                            (symbol, Ok(true), true)
                        } else {
                            (symbol, Err(err), false)
                        }
                    }
                }
            }
        })
        .buffer_unordered(CONCURRENT_LIMIT)
        .collect()
        .await;

    let mut leverage_set_count = 0;
    let mut leverage_skip_count = 0;
    let mut leverage_fail_count = 0;

    for (symbol, result, was_skip) in results {
        match result {
            Ok(_) => {
                if was_skip {
                    leverage_skip_count += 1;
                    debug!(%symbol, leverage = leverage_to_set, "leverage already set");
                } else {
                    leverage_set_count += 1;
                    debug!(%symbol, leverage = leverage_to_set, "leverage set successfully");
                }
            }
            Err(err) => {
                leverage_fail_count += 1;
                warn!(%symbol, leverage = leverage_to_set, error = %err, "failed to set leverage");
            }
        }
    }

    info!(
        total = total_symbols,
        leverage_set = leverage_set_count,
        leverage_skip = leverage_skip_count,
        leverage_fail = leverage_fail_count,
        "leverage setup completed"
    );

    Ok(())
}
