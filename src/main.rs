use anyhow::Result;
use log::{error, info, warn};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::interval;
use trading_bot::{
    balance,
    config::BotConfig,
    follow_orders, initialization,
    logging,
    ordering,
    run_slippage_tracker,
    tasks::{TaskInfo, TaskManager},
    trending, trending_tasks,
    EventBus, SlippageTracker,
};

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    let config = BotConfig::from_env();
    info!("Starting trading bot with config: {:?}", config);

    let components = initialization::initialize_app(&config).await?;
    let shared_state = components.shared_state;
    let connection = components.connection;
    let symbol_cache = components.symbol_cache;
    let depth_cache = components.depth_cache;
    let risk_manager = components.risk_manager;
    let metrics_cache = components.metrics_cache;
    let scanner = components.scanner;
    let file_cfg = components.file_config;

    let bus_config = initialization::EventBusConfig::from_file_config(&file_cfg);
    info!(
        "EventBus buffer sizes: market_tick={}, trade_signal={}, close_request={}, order_update={}, position_update={}, balance_update={}",
        bus_config.market_tick_buffer, bus_config.trade_signal_buffer, bus_config.close_request_buffer,
        bus_config.order_update_buffer, bus_config.position_update_buffer, bus_config.balance_update_buffer
    );

    let bus = EventBus::new(
        bus_config.market_tick_buffer,
        bus_config.trade_signal_buffer,
        bus_config.close_request_buffer,
        bus_config.order_update_buffer,
        bus_config.position_update_buffer,
        bus_config.balance_update_buffer,
    );

    let ordering_ch = bus.ordering_channels();
    let follow_ch = bus.follow_channels();
    let balance_ch = bus.balance_channels();
    let logging_ch = bus.logging_channels();
    let connection_market_ch = bus.connection_channels();
    let connection_user_ch = bus.connection_channels();
    let slippage_tracker = SlippageTracker::new(240);

    let mut task_manager = TaskManager::new();

    {
        let tracker = slippage_tracker.clone();
        let market_rx = bus.market_receiver();
        task_manager.spawn("slippage_tracker", async move {
            run_slippage_tracker(tracker, market_rx).await;
        });
    }

    {
        let conn = connection.clone();
        let ch = connection_market_ch.clone();
        let depth_cache_clone = Some(depth_cache.clone());
        task_manager.spawn_with_result("market_ws", async move {
            conn.run_market_ws_with_depth_cache(ch, depth_cache_clone).await
        });
    }

    {
        let conn = connection.clone();
        let ch = connection_user_ch.clone();
        task_manager.spawn_with_result("user_ws", async move {
            conn.run_user_ws(ch).await
        });
    }

    {
        let risk_manager_for_price = risk_manager.clone();
        let mut market_rx = bus.market_receiver();
        task_manager.spawn("price_history_updater", async move {
            loop {
                match market_rx.recv().await {
                    Ok(tick) => {
                        risk_manager_for_price.update_price_history(&tick.symbol, tick.price).await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                        warn!("PRICE_HISTORY: lagged by {} market ticks", count);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        warn!("PRICE_HISTORY: market channel closed, stopping price history updater");
                        break;
                    }
                }
            }
        });
    }

    {
        let conn = connection.clone();
        let state = shared_state.clone();
        let ch = ordering_ch;
        let rm = risk_manager.clone();
        let slippage = slippage_tracker.clone();
        let symbol_cache_clone = symbol_cache.clone();
        let depth_cache_clone = depth_cache.clone();
        task_manager.spawn("ordering", async move {
            ordering::run_ordering(
                ch,
                state,
                conn,
                Some(rm),
                Some(slippage),
                Some(symbol_cache_clone),
                Some(depth_cache_clone),
            ).await;
        });
    }

    {
        let ch = follow_ch;
        let state = shared_state.clone();
        let commission_pct = config.trend_params().fee_bps_round_trip / 100.0;
        let atr_sl_multiplier = config.atr_sl_multiplier;
        let atr_tp_multiplier = config.atr_tp_multiplier;
        task_manager.spawn("follow_orders", async move {
            follow_orders::run_follow_orders(
                ch,
                state,
                config.tp_percent,
                config.sl_percent,
                commission_pct,
                atr_sl_multiplier,
                atr_tp_multiplier,
            )
            .await;
        });
    }

    {
        let ch = balance_ch;
        let state = shared_state.clone();
        task_manager.spawn("balance", async move {
            balance::run_balance(ch, state).await;
        });
    }

    {
        let ch = logging_ch;
        task_manager.spawn("logging", async move {
            logging::run_logging(ch).await;
        });
    }


    let initial_symbols = if scanner.config.enabled {
        match scanner.select_symbols().await {
            Ok(symbols) => {
                info!(
                    "SYMBOL_SCANNER: Selected {} symbols for trading",
                    symbols.len()
                );
                symbols
            }
            Err(err) => {
                warn!(
                    "SYMBOL_SCANNER: Failed to select symbols: {}, falling back to single symbol",
                    err
                );
                vec![config.symbol.clone()]
            }
        }
    } else {
        vec![config.symbol.clone()]
    };

    metrics_cache.set_symbols(initial_symbols.clone()).await;

    info!("CACHE: Warming up symbol info cache...");
    symbol_cache.warmup(&initial_symbols, &connection).await;
    info!("CACHE: Symbol info cache warmup complete");

    info!("CACHE: Warming up metrics cache...");
    if let Err(err) = metrics_cache.pre_warm_symbols(&initial_symbols).await {
        warn!("CACHE: Metrics cache warmup failed: {} (will continue with empty cache)", err);
    } else {
        info!("CACHE: Metrics cache warmup complete");
    }

    let cache_for_update = metrics_cache.clone();
    task_manager.spawn("metrics_cache", async move {
        cache_for_update.run_update_task().await;
    });

    if scanner.config.enabled {
        let rotation_tx = bus.rotation_sender();
        let scanner_for_rotation = scanner.clone();
        let rotation_tx_clone = rotation_tx.clone();
        task_manager.spawn("symbol_scanner_rotation", async move {
            scanner_for_rotation.run_rotation_task(rotation_tx_clone).await;
        });
    }

    let trend_params = config.trend_params();
    let ws_base_url = config.ws_base_url.clone();

    type TaskHandle = Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>;
    let active_trending_tasks: Arc<tokio::sync::RwLock<std::collections::HashMap<String, TaskHandle>>> =
        Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));


    let use_combined_stream = initial_symbols.len() >= 3;
    
    let mut task_infos = task_manager.into_tasks();
    let task_infos_arc = Arc::new(tokio::sync::Mutex::new(task_infos.clone()));
    
    if use_combined_stream {
        info!("TRENDING: Using Combined Stream for {} symbols (reduces {} WebSocket connections to 1)", 
              initial_symbols.len(), initial_symbols.len());
        
        // Create symbol handlers for combined stream
        use std::collections::HashMap;
        let symbol_handlers: Arc<tokio::sync::RwLock<HashMap<String, trending::SymbolHandler>>> = 
            Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        
        // Initialize handlers for each symbol
        for symbol in initial_symbols.iter() {
            let client = trading_bot::types::FuturesClient::new();
            let cfg = trending_tasks::create_algo_config_from_trend_params(&trend_params);
            let kline_interval = "5m";
            let kline_limit = (trend_params.warmup_min_ticks + 10) as u32;
            
            // Create candle buffer and load initial candles
            let candle_buffer = Arc::new(tokio::sync::RwLock::new(Vec::new()));
            match client.fetch_klines(symbol, kline_interval, kline_limit).await {
                Ok(candles) => {
                    *candle_buffer.write().await = candles;
                    info!("TRENDING: loaded {} candles for {} (combined stream)", 
                          candle_buffer.read().await.len(), symbol);
                }
                Err(err) => {
                    warn!("TRENDING: failed to fetch initial candles for {}: {err:?}", symbol);
                }
            }
            
            // Create signal state and market tick state
            let signal_state = Arc::new(tokio::sync::RwLock::new(trending::LastSignalState {
                last_long_time: None,
                last_short_time: None,
            }));
            let latest_market_tick = Arc::new(tokio::sync::RwLock::new(None));
            let signal_tx = bus.trending_channels().signal_tx.clone();
            
            // Create market tick updater for this symbol
            let mut market_rx = bus.market_receiver();
            let latest_tick_clone = latest_market_tick.clone();
            let symbol_clone = symbol.clone();
            tokio::spawn(async move {
                loop {
                    match trading_bot::types::handle_broadcast_recv(market_rx.recv().await) {
                        Ok(Some(tick)) => {
                            if tick.symbol == symbol_clone {
                                *latest_tick_clone.write().await = Some(tick);
                            }
                        }
                        Ok(None) => continue,
                        Err(_) => break,
                    }
                }
            });
            
            // Insert handler
            symbol_handlers.write().await.insert(symbol.clone(), trending::SymbolHandler {
                candle_buffer,
                signal_state,
                signal_tx,
                latest_market_tick,
                client,
                cfg,
                params: trend_params.clone(),
                metrics_cache: Some(metrics_cache.clone()),
            });
        }
        
        // Spawn combined stream task
        let ws_base_url_for_stream = ws_base_url.clone();
        let combined_stream_handle = tokio::spawn(async move {
            trending::run_combined_kline_stream(
                initial_symbols.clone(),
                "5m",
                "5m",
                ws_base_url_for_stream,
                symbol_handlers,
            ).await;
        });
        let handle_arc = Arc::new(tokio::sync::Mutex::new(Some(combined_stream_handle)));
        task_infos.push(TaskInfo {
            name: "combined_kline_stream".to_string(),
            handle: handle_arc,
        });
        // Update task_infos_arc from task_infos
        {
            *task_infos_arc.lock().await = task_infos.clone();
        }
    } else {
        // Use individual streams for <3 symbols (backward compatibility)
        info!("TRENDING: Using individual streams for {} symbols", initial_symbols.len());
        for symbol in initial_symbols.iter() {
            trending_tasks::spawn_trending_task(
                symbol.clone(),
                &bus,
                &trend_params,
                &ws_base_url,
                metrics_cache.clone(),
                active_trending_tasks.clone(),
                task_infos_arc.clone(),
            ).await;
        }
        {
            let infos = task_infos_arc.lock().await;
            task_infos = (*infos).clone();
        }
    }

    if scanner.config.enabled {
        let mut rotation_rx = bus.rotation_channels().rotation_rx;
        let tasks_map = active_trending_tasks.clone();
        let risk_manager_clone = risk_manager.clone();
        let bus_clone = bus.clone();
        let trend_params_clone = trend_params.clone();
        let ws_base_url_clone = ws_base_url.clone();
        let metrics_cache_clone = metrics_cache.clone();
        let supervisor_handle = tokio::spawn(async move {
            use tokio::sync::broadcast::error::RecvError;
            loop {
                match rotation_rx.recv().await {
                    Ok(event) => {
                        info!(
                            "ROTATION_SUPERVISOR: Received rotation event (added: {}, removed: {})",
                            event.new_symbols.len(),
                            event.removed_symbols.len()
                        );

                        // Check if removed symbols have open positions
                        let stats = risk_manager_clone.get_portfolio_stats().await;
                        for removed_symbol in &event.removed_symbols {
                            if stats.symbols.contains(removed_symbol) {
                                warn!(
                                    "ROTATION_SUPERVISOR: Symbol {} has open position (will be managed by follow_orders)",
                                    removed_symbol
                                );
                            }
                        }

                        // Stop trending tasks for removed symbols
                        for removed_symbol in &event.removed_symbols {
                            let mut tasks = tasks_map.write().await;
                            if let Some(handle_arc) = tasks.remove(removed_symbol) {
                                let mut handle_guard = handle_arc.lock().await;
                                if let Some(handle) = handle_guard.take() {
                                    handle.abort();
                                    info!(
                                        "ROTATION_SUPERVISOR: Stopped trending task for {}",
                                        removed_symbol
                                    );
                                }
                            }
                        }

                        // Spawn new trending tasks for added symbols
                        for new_symbol in &event.new_symbols {
                            trending_tasks::spawn_trending_task(
                                new_symbol.clone(),
                                &bus_clone,
                                &trend_params_clone,
                                &ws_base_url_clone,
                                metrics_cache_clone.clone(),
                                tasks_map.clone(),
                                task_infos_arc.clone(),
                            ).await;
                        }

                        // Update metrics cache with all symbols
                        metrics_cache_clone.set_symbols(event.all_symbols.clone()).await;
                        
                        // ✅ CRITICAL: Update symbol cache when symbols rotate (TrendPlan.md)
                        // Note: symbol_cache needs to be passed to rotation supervisor
                        // For now, cache will be updated on-demand when new orders come in
                    }
                    Err(RecvError::Closed) => {
                        warn!("ROTATION_SUPERVISOR: Rotation channel closed");
                        break;
                    }
                    Err(RecvError::Lagged(_)) => {
                        warn!("ROTATION_SUPERVISOR: Lagged behind rotation events");
                    }
                }
            }
        });
        let handle_arc = Arc::new(tokio::sync::Mutex::new(Some(supervisor_handle)));
        task_infos.push(TaskInfo {
            name: "rotation_supervisor".to_string(),
            handle: handle_arc,
        });
    }

    let health_check_task_infos = task_infos.clone();
    let health_check_task = tokio::spawn(async move {
        let mut interval = interval(Duration::from_secs(30)); // Check every 30 seconds
        loop {
            interval.tick().await;

            let mut finished_count = 0;
            for task_info in &health_check_task_infos {
                let handle_guard = task_info.handle.lock().await;
                if let Some(handle) = handle_guard.as_ref() {
                    if handle.is_finished() {
                        finished_count += 1;
                        warn!(
                            "HEALTH: Task '{}' has finished (may have crashed)",
                            task_info.name
                        );
                        // Note: In production, you might want to restart the task here
                        // For now, we just log the event
                    }
                } else {
                    warn!("HEALTH: Task '{}' handle is None", task_info.name);
                }
            }

            if finished_count > 0 {
                error!(
                    "HEALTH: {} out of {} tasks have finished",
                    finished_count,
                    health_check_task_infos.len()
                );
            } else {
                info!(
                    "HEALTH: All {} tasks running normally",
                    health_check_task_infos.len()
                );
            }
        }
    });

    let ordering_close_tx_health = bus.follow_channels().close_tx.clone();
    let connection_health = connection.clone();
    let shared_state_health = shared_state.clone();
    let config_health = config.clone();
    let _ws_health_task = tokio::spawn(async move {
        let mut interval = interval(Duration::from_secs(10)); // Check every 10 seconds
        loop {
            interval.tick().await;

            let (is_stale, seconds_since_update) = connection_health.check_websocket_health().await;
            
            if is_stale {
                warn!(
                    "WS_HEALTH: WebSocket data is stale ({} seconds since last update, threshold: {}s)",
                    seconds_since_update,
                    config_health.market_tick_stale_tolerance_secs
                );

                // ✅ CRITICAL: Position Management on WebSocket Disconnect (Plan.md)
                // If configured, close positions when WebSocket is disconnected for too long
                if config_health.ws_disconnect_close_positions {
                    if seconds_since_update > config_health.ws_disconnect_timeout_secs {
                        // Close all active positions (multi-asset support)
                        let active_positions = shared_state_health.get_all_active_positions();
                        if !active_positions.is_empty() {
                            warn!(
                                "WS_HEALTH: WebSocket disconnected for {}s (threshold: {}s), closing {} position(s) to prevent trading with stale data",
                                seconds_since_update,
                                config_health.ws_disconnect_timeout_secs,
                                active_positions.len()
                            );
                            
                            for position in active_positions {
                                // Send close request for each position
                                let close_request = trading_bot::types::CloseRequest {
                                    position_id: position.position_id,
                                    reason: format!(
                                        "WebSocket disconnected for {}s (threshold: {}s)",
                                        seconds_since_update,
                                        config_health.ws_disconnect_timeout_secs
                                    ),
                                    ts: chrono::Utc::now(),
                                    partial_close_percentage: None, // Full close
                                };
                                
                                if ordering_close_tx_health.send(close_request).await.is_err() {
                                    error!("WS_HEALTH: Failed to send close request for position {} - ordering channel closed", position.position_id);
                                }
                            }
                        }
                    }
                }
            } else {
                // WebSocket is healthy
                if seconds_since_update < i64::MAX {
                    log::debug!(
                        "WS_HEALTH: WebSocket healthy ({}s since last update)",
                        seconds_since_update
                    );
                }
            }
        }
    });

    tokio::select! {
        _ = async {
            // Wait for all main tasks
            for task_info in &task_infos {
                // Take handle from mutex to get owned value for await
                let handle = {
                    let mut handle_guard = task_info.handle.lock().await;
                    handle_guard.take()
                };

                if let Some(handle) = handle {
                    let result = handle.await;
                    if let Err(err) = result {
                        error!("MAIN: Task '{}' panicked: {err:?}", task_info.name);
                    } else {
                        warn!("MAIN: Task '{}' finished normally", task_info.name);
                    }
                }
            }
        } => {
            warn!("MAIN: One or more tasks finished");
        }
        _ = tokio::signal::ctrl_c() => {
            info!("MAIN: Received shutdown signal (Ctrl+C)");
        }
    }

    health_check_task.abort();

    info!("MAIN: Shutting down...");
    Ok(())
}
