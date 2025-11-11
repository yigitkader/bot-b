//location: /crates/app/src/main.rs
// Main application entry point and trading loop

mod app_init;
mod binance_exec;
mod binance_rest;
mod binance_ws;
mod cap_manager;
mod config;
mod constants;
mod exec;
mod event_handler;
mod logger;
mod monitor;
mod order;
mod position_manager;
mod quote_generator;
mod qmel;
#[cfg(test)]
mod qmel_tests;
mod risk;
mod strategy;
mod direction_selector;
mod symbol_discovery;
mod symbol_processor;
mod types;
mod utils;

use anyhow::Result;
use crate::types::*;
use crate::binance_ws::UserEvent;
use crate::constants::*;
use crate::exec::Venue;
use crate::risk::RiskAction;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use crate::strategy::Context;
use tracing::{debug, error, info, warn};
use utils::{
    compute_drawdown_bps, get_price_tick, get_qty_step, rate_limit_guard,
    calculate_effective_leverage, fetch_all_quote_balances,
    apply_fill_rate_decay, should_sync_orders,
    process_order_canceled, process_order_fill_with_logging,
};
use app_init::{initialize_app, AppInitResult};
use event_handler::handle_reconnect_sync;
use symbol_processor::process_symbol;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use futures_util::stream::{self, StreamExt};
use std::sync::Arc;

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
    
    let mut interval = tokio::time::interval_at(
        tokio::time::Instant::now(),
        Duration::from_millis(tick_ms),
    );
    
    static TICK_COUNTER: AtomicU64 = AtomicU64::new(0);
    
    info!(
        symbol_count = states.len(),
        tick_interval_ms = tick_ms,
        min_usd_per_order,
        "main trading loop starting"
    );
    
    let mut symbol_index: HashMap<String, usize> = HashMap::new();
    for (idx, state) in states.iter().enumerate() {
        symbol_index.insert(state.meta.symbol.clone(), idx);
    }
    
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let shutdown_tx_clone = shutdown_tx.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            info!("Ctrl+C signal received, shutting down");
            let _ = shutdown_tx_clone.send(());
        }
    });
    
    let mut force_sync_all = false;
    let mut shutdown_requested = false;
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = shutdown_rx.recv() => {
                info!("shutdown signal received, initiating graceful shutdown");
                shutdown_requested = true;
                break;
            }
        }
        
        let tick_num = TICK_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
        if tick_num <= 5 || tick_num % 10 == 0 {
            info!(tick_num, "=== MAIN LOOP TICK START ===");
        }

        // WebSocket event'lerini işle
        while let Ok(event) = event_rx.try_recv() {
            match event {
                UserEvent::Heartbeat => {
                    force_sync_all = true;
                    info!("websocket reconnect detected, will sync all symbols");
                    handle_reconnect_sync(&venue, &mut states, &cfg).await;
                }
                UserEvent::OrderFill {
                    symbol,
                    order_id,
                    client_order_id: _,
                    side,
                    qty,
                    cumulative_filled_qty,
                    price,
                    is_maker,
                    order_status,
                    commission,
                } => {
                    if let Some(idx) = symbol_index.get(&symbol) {
                        process_order_fill_with_logging(
                            &mut states[*idx],
                            &symbol,
                            &order_id,
                            side,
                            qty,
                            price,
                            is_maker,
                            cumulative_filled_qty,
                            &order_status,
                            commission,
                            &cfg,
                            &json_logger,
                        );
                    }
                }
                UserEvent::OrderCanceled { symbol, order_id, client_order_id } => {
                    if let Some(idx) = symbol_index.get(&symbol) {
                        process_order_canceled(
                            &mut states[*idx],
                            &symbol,
                            &order_id,
                            &client_order_id,
                            &cfg,
                            &json_logger,
                        );
                    }
                }
            }
        }

        let effective_leverage = calculate_effective_leverage(cfg.leverage, cfg.risk.max_leverage);
        
        let unique_quote_assets: std::collections::HashSet<String> = states
            .iter()
            .map(|s| s.meta.quote_asset.clone())
            .collect();
        
        let quote_assets_vec: Vec<String> = unique_quote_assets.iter().cloned().collect();
        let mut quote_balances = fetch_all_quote_balances(&venue, &quote_assets_vec).await;
        
        let mut processed_count = 0;
        let mut skipped_count = 0;
        let mut disabled_count = 0;
        let mut no_balance_count = 0;
        let total_symbols = states.len();
        let mut symbol_index = 0;
        
        let max_symbols_per_tick = cfg.internal.max_symbols_per_tick;
        let mut symbols_processed_this_tick = 0;
        
        // Prioritize symbols with open orders/positions, then round-robin
        use std::sync::atomic::{AtomicUsize, Ordering};
        static ROUND_ROBIN_OFFSET: AtomicUsize = AtomicUsize::new(0);
        let round_robin_offset = ROUND_ROBIN_OFFSET.fetch_add(1, Ordering::Relaxed) % states.len().max(1);
        
        let prioritized_indices: Vec<usize> = {
            let mut indices: Vec<(usize, bool)> = states.iter()
            .enumerate()
                .map(|(i, s)| (i, !s.active_orders.is_empty() || !s.inv.0.is_zero()))
            .collect();
            indices.sort_by_key(|(_, priority)| !*priority); // false (no priority) first, then true
            indices.into_iter()
                .map(|(i, _)| i)
            .cycle()
            .skip(round_robin_offset)
            .take(states.len())
                .collect()
        };
        
        for state_idx in prioritized_indices {
            let state = &mut states[state_idx];
            
            // Rate limit koruması: Her tick'te maksimum sembol sayısı
            if symbols_processed_this_tick >= max_symbols_per_tick {
                // Bu tick'te yeterli sembol işlendi, kalan semboller bir sonraki tick'te işlenecek
                skipped_count += 1;
                continue;
            }
            symbol_index += 1;
            
            // Progress log: Her N sembolde bir veya ilk N sembolde
            if symbol_index <= cfg.internal.progress_log_first_n_symbols || symbol_index % cfg.internal.progress_log_interval == 0 {
                info!(
                    progress = format!("{}/{}", symbol_index, total_symbols),
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
            let has_balance = q_free >= cfg.min_quote_balance_usd && q_free >= min_usd_per_order;
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
            let (bid, ask) = match venue.best_prices(&symbol).await {
                Ok(prices) => prices,
                Err(err) => {
                    warn!(%symbol, ?err, "failed to fetch best prices, skipping tick");
                    continue;
                }
            };
            
            if force_sync_all && symbol_index == total_symbols {
                force_sync_all = false;
                info!("WebSocket reconnect sync completed for all symbols");
            }
            
            match process_symbol(
                &venue,
                &symbol,
                &quote_asset,
                state,
                bid,
                ask,
                &mut quote_balances,
                &cfg,
                &risk_limits,
                &profit_guarantee,
                effective_leverage,
                min_usd_per_order,
                tif,
                &json_logger,
                force_sync_all,
            ).await {
                Ok(true) => {}
                Ok(false) => continue,
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
                total_symbols = states.len(),
                "main loop tick completed"
            );
        }
    }
    
    info!("main loop ended, performing graceful shutdown");
    
    if shutdown_requested {
        for state in states.iter() {
            if !state.inv.0.is_zero() {
                warn!(symbol = %state.meta.symbol, inventory = %state.inv.0, "position still open during shutdown");
            }
        }
    }
    
    drop(event_tx);
    drop(json_logger);
    drop(shutdown_tx);
    tokio::time::sleep(Duration::from_millis(1000)).await;
    
    info!("graceful shutdown completed");
    Ok(())
}

#[cfg(test)]
#[path = "position_order_tests.rs"]
mod position_order_tests;

