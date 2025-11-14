//location: /crates/app/src/main.rs
// Main application entry point and trading loop

mod app_init;
mod config;
mod constants;
mod exchange; // NEW: binance consolidation
mod exec;
mod logger;
mod monitor;
mod order;
mod position_manager;
mod processor; // NEW: symbol processing consolidation
mod qmel;
mod risk;
mod strategy; // Now includes direction_selector
mod types;
mod utils;

use crate::exchange::UserEvent;
use crate::exec::Venue;
use anyhow::Result;
use app_init::{initialize_app, AppInitResult};
use logger::handle_reconnect_sync;
use processor::{process_symbol, continuous_analysis_task};
use tracing::{debug, error, info, warn};
use utils::{
    calculate_effective_leverage, fetch_all_quote_balances, process_order_canceled,
    process_order_fill_with_logging, rate_limit_guard,
};

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize application (config, venue, symbols, states)
    let init = initialize_app().await?;

    let AppInitResult {
        cfg,
        venue,
        mut states,
        risk_limits,
        event_tx,
        mut event_rx,
        json_logger,
        profit_guarantee,
        tick_ms,
        min_usd_per_order,
        tif,
    } = init;

    // ✅ KRİTİK: Hızlı shutdown için AtomicBool kullan (channel yerine)
    // Bu sayede Ctrl+C'ye basıldığında hemen kontrol edilebilir
    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let shutdown_flag_clone = shutdown_flag.clone();

    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            info!("Ctrl+C signal received, shutting down immediately");
            shutdown_flag_clone.store(true, Ordering::Relaxed);
        }
    });

    // ✅ KRİTİK: Continuous analysis için states, venue ve cfg'yi Arc yapıya dönüştür
    // Background task sürekli trend analizi ve EV hesaplama yapacak
    let states_arc = Arc::new(tokio::sync::RwLock::new(states));
    let states_for_main = states_arc.clone();
    let venue_arc = Arc::new(venue);
    let venue_for_main = venue_arc.clone();
    let cfg_arc = Arc::new(cfg);
    let cfg_for_main = cfg_arc.clone();
    let shutdown_flag_for_analysis = shutdown_flag.clone();
    
    // ✅ KRİTİK: Continuous analysis background task'ı başlat
    // Bu task her 1.5 saniyede bir tüm semboller için trend analizi ve EV hesaplama yapar
    // Böylece sistem her an en güncel skorlara sahip olur ve bir sonraki işleme hazır olur
    tokio::spawn(async move {
        processor::continuous_analysis_task(
            venue_arc,
            states_arc,
            cfg_arc,
            shutdown_flag_for_analysis,
        ).await;
    });

    let mut interval =
        tokio::time::interval_at(tokio::time::Instant::now(), Duration::from_millis(tick_ms));

    static TICK_COUNTER: AtomicU64 = AtomicU64::new(0);

    info!(
        symbol_count = states_for_main.read().await.len(),
        tick_interval_ms = tick_ms,
        min_usd_per_order,
        "main trading loop starting (continuous analysis task running in background)"
    );

    // Build symbol index for fast lookup during events (will be rebuilt each tick)
    // Note: symbol_index is rebuilt each tick since states are in Arc<RwLock>

    // ✅ KRİTİK: Hızlı shutdown için AtomicBool kullan (channel yerine)
    // Bu sayede Ctrl+C'ye basıldığında hemen kontrol edilebilir
    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let shutdown_flag_clone = shutdown_flag.clone();

    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            info!("Ctrl+C signal received, shutting down immediately");
            shutdown_flag_clone.store(true, Ordering::Relaxed);
        }
    });

    let mut force_sync_all = false;
    loop {
        // ✅ KRİTİK: Shutdown kontrolü - her tick başında kontrol et
        if shutdown_flag.load(Ordering::Relaxed) {
            info!("shutdown flag detected, initiating graceful shutdown");
            break;
        }

        // ✅ KRİTİK: Interval tick - shutdown kontrolü loop başında yapılıyor
        // Ctrl+C'ye basıldığında hemen break yapılır
        interval.tick().await;

        let tick_num = TICK_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
        if tick_num <= 5 || tick_num % 10 == 0 {
            info!(tick_num, "=== MAIN LOOP TICK START ===");
        }

        // ===== Process WebSocket Events =====
        // Rebuild symbol index each tick (states are in Arc<RwLock>)
        let states_for_events = states_for_main.read().await;
        let symbol_index: HashMap<String, usize> = states_for_events
            .iter()
            .enumerate()
            .map(|(idx, state)| (state.meta.symbol.clone(), idx))
            .collect();
        drop(states_for_events);
        
        while let Ok(event) = event_rx.try_recv() {
            match event {
                UserEvent::Heartbeat => {
                    force_sync_all = true;
                    info!("websocket reconnect detected, will sync all symbols");
                    let mut states_write = states_for_main.write().await;
                    handle_reconnect_sync(&*venue_for_main, &mut *states_write, &*cfg_for_main).await;
                }
                UserEvent::OrderFill {
                    symbol,
                    order_id,
                    side,
                    qty,
                    cumulative_filled_qty,
                    price,
                    is_maker,
                    order_status,
                    commission,
                } => {
                    if let Some(idx) = symbol_index.get(&symbol) {
                        let mut states_write = states_for_main.write().await;
                        if let Some(state) = states_write.get_mut(*idx) {
                        process_order_fill_with_logging(
                                state,
                            &symbol,
                            &order_id,
                            side,
                            qty,
                            price,
                            is_maker,
                            cumulative_filled_qty,
                            &order_status,
                            commission,
                            &*cfg_for_main,
                            &json_logger,
                        );
                        }
                    }
                }
                UserEvent::OrderCanceled {
                    symbol,
                    order_id,
                    client_order_id,
                } => {
                    if let Some(idx) = symbol_index.get(&symbol) {
                        let mut states_write = states_for_main.write().await;
                        if let Some(state) = states_write.get_mut(*idx) {
                        process_order_canceled(
                                state,
                            &symbol,
                            &order_id,
                            &client_order_id,
                            &*cfg_for_main,
                            &json_logger,
                        );
                        }
                    }
                }
            }
        }

        let effective_leverage = calculate_effective_leverage(cfg_for_main.leverage, cfg_for_main.risk.max_leverage);

        // ===== Get States (from Arc<RwLock> for continuous analysis compatibility) =====
        // Note: We'll acquire read lock for each operation that needs it
        // This allows continuous analysis task to run in parallel
        
        // ===== Fetch Balances =====
        let unique_quote_assets: std::collections::HashSet<String> = {
            let states_read = states_for_main.read().await;
            states_read.iter().map(|s| s.meta.quote_asset.clone()).collect()
        };

        let quote_assets_vec: Vec<String> = unique_quote_assets.iter().cloned().collect();
        let mut quote_balances = fetch_all_quote_balances(&*venue_for_main, &quote_assets_vec).await;

        // ===== Symbol Processing Statistics =====
        let mut processed_count = 0;
        let mut skipped_count = 0;
        let mut disabled_count = 0;
        let mut no_balance_count = 0;
        let total_symbols = {
            let states_read = states_for_main.read().await;
            states_read.len()
        };
        let mut symbol_index_counter = 0;

        let max_symbols_per_tick = cfg_for_main.internal.max_symbols_per_tick;
        let mut symbols_processed_this_tick = 0;

        // ✅ OPPORTUNITY SELECTOR: Select best opportunities based on EV scores
        // Quote balances are fetched above, pass them to opportunity selector
        // This ensures USDC/USDT balance differences are properly handled:
        // - If USDC balance exists → USDC symbols (BTCUSDC, ETHUSDC, etc.) are evaluated
        // - If USDT balance exists → USDT symbols (BTCUSDT, ETHUSDT, etc.) are evaluated
        // - Each symbol is checked against its own quote asset balance
        let (opportunities, opportunity_symbols, prioritized_indices, any_open) = {
            let states_read = states_for_main.read().await;
            let opportunities = crate::processor::select_best_opportunities(
                &*states_read,
                &*cfg_for_main,
                &risk_limits,
                &quote_balances,
                min_usd_per_order,
            );
            let opportunity_symbols: std::collections::HashSet<String> = opportunities
                .iter()
                .map(|opp| opp.symbol.clone())
                .collect();
            
            // Log opportunity selection summary (periodically)
            if tick_num <= 5 || tick_num % 20 == 0 {
                let usdc_balance = quote_balances.get("USDC").copied().unwrap_or(0.0);
                let usdt_balance = quote_balances.get("USDT").copied().unwrap_or(0.0);
                info!(
                    opportunities_count = opportunities.len(),
                    usdc_balance,
                    usdt_balance,
                    selected_symbols = ?opportunity_symbols.iter().take(5).collect::<Vec<_>>(),
                    "opportunity selector: evaluated all symbols with quote asset balance checks"
                );
            }

            // Calculate prioritized indices: opportunity selector + priority field + open orders/positions
            static ROUND_ROBIN_OFFSET: AtomicUsize = AtomicUsize::new(0);
            let max_states = states_read.len().max(1);
            let round_robin_offset = ROUND_ROBIN_OFFSET.fetch_add(1, Ordering::Relaxed) % max_states;

            let prioritized_indices: Vec<usize> = {
                let mut indices: Vec<(usize, u32, bool)> = states_read
                    .iter()
                    .enumerate()
                    .map(|(i, s)| {
                        let has_active = !s.active_orders.is_empty() || !s.inv.0.is_zero();
                        let is_opportunity = opportunity_symbols.contains(&s.meta.symbol);
                        // Priority calculation: opportunity bonus (highest) + priority field + active bonus
                        // ✅ KRİTİK DÜZELTME: Acquire ordering kullan
                        // Relaxed ordering garantisiz - farklı CPU core'ları farklı değerler görebilir
                        // Acquire: Load'dan sonraki tüm memory işlemleri store'dan sonra görünür
                        let priority_value = s.priority.load(std::sync::atomic::Ordering::Acquire);
                        let opportunity_bonus = if is_opportunity { 10000 } else { 0 };
                        let priority_score = opportunity_bonus + priority_value + if has_active { 1000 } else { 0 };
                        (i, priority_score, has_active)
                    })
                    .collect();

                // Sort: highest priority first (opportunities first, then by priority field, then by active)
                indices.sort_by_key(|(_, priority_score, _)| std::cmp::Reverse(*priority_score));

                indices
                    .into_iter()
                    .map(|(i, _, _)| i)
                    .cycle()
                    .skip(round_robin_offset)
                    .take(states_read.len())
                    .collect()
            };

            // ✅ KRİTİK: Global tek-pozisyon/tek-order kuralı
            // Tüm semboller arasında aynı anda sadece bir "exposure" olsun
            // Bu kontrol WS event'leriyle uyum sağlar - fill geldiğinde diğer semboller skip edilir
            let any_open = states_read
                .iter()
                .any(|s| !s.inv.0.is_zero() || !s.active_orders.is_empty());
            
            (opportunities, opportunity_symbols, prioritized_indices, any_open)
        };

        for state_idx in prioritized_indices {
            // ✅ KRİTİK: Her sembol işlendikten önce shutdown kontrolü
            // Bu sayede Ctrl+C'ye basıldığında uzun sembol listesi işlenmeden hemen çıkılır
            if shutdown_flag.load(Ordering::Relaxed) {
                info!("shutdown detected during symbol processing, breaking immediately");
                break;
            }

            // Get mutable access to state (need to drop read lock first)
            // Note: states read lock is already dropped at end of previous iteration
            let mut states_write = states_for_main.write().await;
            let state = &mut states_write[state_idx];

            // ✅ KRİTİK: Per-symbol rules zorunlu - fetch başarısızsa trade etme
            // Global tek order modunda "kuralsız" sembolü tamamen skip et
            if state.disabled || state.rules_fetch_failed || state.symbol_rules.is_none() {
                skipped_count += 1;
                continue; // Rules yoksa trade etme - tamamen skip et
            }

            // ✅ KRİTİK: Global kontrol - başka bir sembolde exposure varsa, bu sembol beklesin
            // Bu sayede gereksiz API çağrıları (best_prices, fetch_market_data) azalır
            if any_open {
                // Sadece exposure sahibi sembole izin ver
                if state.inv.0.is_zero() && state.active_orders.is_empty() {
                    skipped_count += 1;
                    // ✅ DEBUG: Global exposure kontrolü logla (sadece ilk birkaç tick'te)
                    if tick_num <= 5 || tick_num % 20 == 0 {
                        let symbol = state.meta.symbol.clone();
                        debug!(
                            %symbol,
                            "skipping symbol: another symbol has open order/position (global single exposure rule)"
                        );
                    }
                    continue; // Bu sembol bu tick beklesin - API çağrısı yapma
                }
            }

            // Rate limit protection: Maximum symbols per tick
            if symbols_processed_this_tick >= max_symbols_per_tick {
                skipped_count += 1;
                continue;
            }
            symbol_index_counter += 1;

            // Progress log: Log first N symbols and then periodically
            // ✅ KRİTİK: İki koşulun "veya" birleşimi - düzgün formatlanmış
            if symbol_index_counter <= cfg_for_main.internal.progress_log_first_n_symbols
                || symbol_index_counter % cfg_for_main.internal.progress_log_interval == 0
            {
                info!(
                    progress = format!("{}/{}", symbol_index_counter, total_symbols),
                    processed_so_far = processed_count,
                    skipped_so_far = skipped_count,
                    "processing symbols..."
                );
            }
            if state.disabled {
                skipped_count += 1;
                disabled_count += 1;
                continue;
            }

            let symbol = state.meta.symbol.clone();
            let quote_asset = state.meta.quote_asset.clone();

            // Early balance check: Skip if no balance and no open orders
            let q_free = quote_balances.get(&quote_asset).copied().unwrap_or(0.0);
            let has_balance = q_free >= cfg_for_main.min_quote_balance_usd && q_free >= min_usd_per_order;
            let has_open_orders = !state.active_orders.is_empty();

            if !has_balance && !has_open_orders {
                skipped_count += 1;
                no_balance_count += 1;
                continue;
            }

            if !has_balance && has_open_orders {
                info!(%symbol, active_orders = state.active_orders.len(), "no balance but has open orders, continuing");
            }

            processed_count += 1;
            symbols_processed_this_tick += 1;

            rate_limit_guard(1).await;
            let (bid, ask) = match venue_for_main.best_prices(&symbol).await {
                Ok(prices) => prices,
                Err(err) => {
                    warn!(%symbol, ?err, "failed to fetch best prices, skipping tick");
                    continue;
                }
            };

            if force_sync_all && symbol_index_counter == total_symbols {
                force_sync_all = false;
                info!("WebSocket reconnect sync completed for all symbols");
            }

            let process_result = process_symbol(
                &*venue_for_main,
                &symbol,
                &quote_asset,
                state,
                bid,
                ask,
                &mut quote_balances,
                &*cfg_for_main,
                &risk_limits,
                &profit_guarantee,
                effective_leverage,
                min_usd_per_order,
                tif,
                &json_logger,
                force_sync_all,
            )
            .await;
            
            // Release write lock before continuing
            drop(states_write);
            
            match process_result {
                Ok(true) => {}
                Ok(false) => {
                    continue;
                }
                Err(e) => {
                    warn!(%symbol, ?e, "error processing symbol, continuing");
                    continue;
                }
            }
        }

        let current_tick = TICK_COUNTER.load(Ordering::Relaxed);
        if current_tick <= 5 || current_tick % 10 == 0 {
            info!(
                tick_count = current_tick,
                processed_symbols = processed_count,
                skipped_symbols = skipped_count,
                disabled_symbols = disabled_count,
                no_balance_symbols = no_balance_count,
                total_symbols,
                "main loop tick completed"
            );
        }
    }

    info!("main loop ended, performing graceful shutdown");

    // ===== Graceful Shutdown Sequence =====
    // ✅ KRİTİK: Timeout'u 3 saniyeye düşür (10 saniye çok uzun, Ctrl+C'ye basıldığında hemen kapanmalı)
    let shutdown_timeout = Duration::from_secs(3);

    let cleanup_result = tokio::time::timeout(shutdown_timeout, async {
        // Step 1: Collect positions and orders to close
        let mut positions_to_close = Vec::new();
        let mut orders_to_cancel = Vec::new();

        {
            let states_read = states_for_main.read().await;
            for state in states_read.iter() {
                let symbol = state.meta.symbol.clone();
                if !state.inv.0.is_zero() {
                    positions_to_close.push(symbol.clone());
                    info!(symbol = %symbol, inventory = %state.inv.0, "position will be closed during shutdown");
                }
                if !state.active_orders.is_empty() {
                    orders_to_cancel.push(symbol.clone());
                    info!(symbol = %symbol, order_count = state.active_orders.len(), "orders will be canceled during shutdown");
                }
            }
        }

        // Step 2: Close positions sequentially (rate limit protection)
        for symbol in positions_to_close {
            let mut states_write = states_for_main.write().await;
            if let Some(state) = states_write.iter_mut().find(|s| s.meta.symbol == symbol) {
                // ✅ KRİTİK: Shutdown sırasında normal kapanış (LIMIT fallback ile)
                if let Err(e) = position_manager::close_position(&*venue_for_main, &symbol, state, false).await {
                    error!(symbol = %symbol, error = %e, "failed to close position during shutdown");
                } else {
                    info!(symbol = %symbol, "position closed successfully during shutdown");
                }
            }
            drop(states_write);
        }

        // Step 3: Cancel orders sequentially (rate limit protection)
        for symbol in orders_to_cancel {
            rate_limit_guard(1).await;
            if let Err(e) = venue_for_main.cancel_all(&symbol).await {
                warn!(symbol = %symbol, error = %e, "failed to cancel orders during shutdown");
            } else {
                info!(symbol = %symbol, "orders canceled successfully during shutdown");
            }
        }

        info!("cleanup completed for all symbols");
    })
    .await;

    match cleanup_result {
        Ok(_) => info!("graceful shutdown cleanup completed successfully"),
        Err(_) => {
            warn!(
                "graceful shutdown cleanup timed out after {} seconds, forcing exit",
                shutdown_timeout.as_secs()
            );
            // Log remaining open positions and orders
            {
                let states_read = states_for_main.read().await;
                for state in states_read.iter() {
                    if !state.inv.0.is_zero() {
                        error!(symbol = %state.meta.symbol, inventory = %state.inv.0, "position still open after shutdown timeout");
                    }
                    if !state.active_orders.is_empty() {
                        error!(symbol = %state.meta.symbol, order_count = state.active_orders.len(), "orders still open after shutdown timeout");
                    }
                }
            }
        }
    }

    // Step 4: Release resources
    drop(event_tx);
    drop(json_logger);

    // ✅ KRİTİK: WebSocket cleanup için kısa bekleme (500ms → 200ms)
    // Daha hızlı kapanma için timeout azaltıldı
    tokio::time::sleep(Duration::from_millis(200)).await;

    info!("graceful shutdown completed");
    Ok(())
}

// Tests are embedded in modules
