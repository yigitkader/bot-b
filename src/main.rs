use anyhow::Result;
use log::{error, info, warn};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio::time::interval;
use trading_bot::{
    balance,
    cache,
    config::BotConfig,
    follow_orders, logging,
    metrics_cache::MetricsCache,
    ordering,
    risk_manager::{RiskLimits, RiskManager},
    run_slippage_tracker,
    symbol_scanner::{SymbolScanner, SymbolSelectionConfig},
    trending, types::TrendParams, Connection, EventBus, SharedState, SlippageTracker,
};

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    let config = BotConfig::from_env();
    info!("Starting trading bot with config: {:?}", config);

    let shared_state = SharedState::new();
    let connection = Arc::new(Connection::new(config.clone()));

    // ✅ CRITICAL: Initialize cache systems (TrendPlan.md)
    let symbol_cache = Arc::new(cache::SymbolInfoCache::new());
    let depth_cache = Arc::new(cache::DepthCache::new());

    // Initialize trading settings (margin mode and leverage) at startup
    if let Err(err) = connection.initialize_trading_settings().await {
        error!("Failed to initialize trading settings: {err:?}");
        return Err(err);
    }

    // Load event bus buffer sizes from config
    let file_cfg = trading_bot::types::FileConfig::load("config.yaml").unwrap_or_default();
    let event_bus_cfg = file_cfg.event_bus.as_ref().cloned().unwrap_or_default();

    // Optimized buffer sizes (defaults if not in config)
    let market_tick_buffer = event_bus_cfg.market_tick_buffer.unwrap_or(2000);
    let trade_signal_buffer = event_bus_cfg.trade_signal_buffer.unwrap_or(500);
    let close_request_buffer = event_bus_cfg.close_request_buffer.unwrap_or(500);
    let order_update_buffer = event_bus_cfg.order_update_buffer.unwrap_or(1000);
    let position_update_buffer = event_bus_cfg.position_update_buffer.unwrap_or(500);
    let balance_update_buffer = event_bus_cfg.balance_update_buffer.unwrap_or(100);

    info!(
        "EventBus buffer sizes: market_tick={}, trade_signal={}, close_request={}, order_update={}, position_update={}, balance_update={}",
        market_tick_buffer, trade_signal_buffer, close_request_buffer,
        order_update_buffer, position_update_buffer, balance_update_buffer
    );

    let bus = EventBus::new(
        market_tick_buffer,
        trade_signal_buffer,
        close_request_buffer,
        order_update_buffer,
        position_update_buffer,
        balance_update_buffer,
    );

    // trending_ch will be created per symbol in the loop below
    let ordering_ch = bus.ordering_channels();
    let follow_ch = bus.follow_channels();
    let balance_ch = bus.balance_channels();
    let logging_ch = bus.logging_channels();
    let connection_market_ch = bus.connection_channels();
    let connection_user_ch = bus.connection_channels();
    let slippage_tracker = SlippageTracker::new(240);

    // Task monitoring: track task names and handles for health checking
    #[derive(Clone)]
    struct TaskInfo {
        name: String,
        handle: Arc<tokio::sync::Mutex<Option<JoinHandle<()>>>>,
    }

    let mut task_infos: Vec<TaskInfo> = Vec::new();

    {
        let tracker = slippage_tracker.clone();
        let market_rx = bus.market_receiver();
        let handle = tokio::spawn(async move {
            run_slippage_tracker(tracker, market_rx).await;
        });
        task_infos.push(TaskInfo {
            name: "slippage_tracker".to_string(),
            handle: Arc::new(tokio::sync::Mutex::new(Some(handle))),
        });
    }

    {
        let conn = connection.clone();
        let ch = connection_market_ch.clone();
        let depth_cache_clone = Some(depth_cache.clone());
        let handle = tokio::spawn(async move {
            if let Err(err) = conn.run_market_ws_with_depth_cache(ch, depth_cache_clone).await {
                error!("Market WS task failed: {err:?}");
            }
        });
        task_infos.push(TaskInfo {
            name: "market_ws".to_string(),
            handle: Arc::new(tokio::sync::Mutex::new(Some(handle))),
        });
    }

    {
        let conn = connection.clone();
        let ch = connection_user_ch.clone();
        let handle = tokio::spawn(async move {
            if let Err(err) = conn.run_user_ws(ch).await {
                error!("User WS task failed: {err:?}");
            }
        });
        task_infos.push(TaskInfo {
            name: "user_ws".to_string(),
            handle: Arc::new(tokio::sync::Mutex::new(Some(handle))),
        });
    }

    // ✅ ADIM 6: Risk manager oluştur
    let risk_config = file_cfg.risk.as_ref();
    let risk_limits = RiskLimits::from_config(config.max_position_notional_usd, risk_config);
    let risk_manager = Arc::new(RiskManager::new(risk_limits, risk_config));

    // ✅ CRITICAL: Price history updater for dynamic correlation (TrendPlan.md - Action Plan)
    // This task updates price history from market ticks to enable real-time correlation calculation
    {
        let risk_manager_for_price = risk_manager.clone();
        let market_rx = bus.market_receiver();
        let price_update_handle = tokio::spawn(async move {
            loop {
                match market_rx.recv().await {
                    Ok(tick) => {
                        // Update price history for correlation calculation
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
        task_infos.push(TaskInfo {
            name: "price_history_updater".to_string(),
            handle: Arc::new(tokio::sync::Mutex::new(Some(price_update_handle))),
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
        let handle = tokio::spawn(async move {
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
        task_infos.push(TaskInfo {
            name: "ordering".to_string(),
            handle: Arc::new(tokio::sync::Mutex::new(Some(handle))),
        });
    }

    {
        let ch = follow_ch;
        let state = shared_state.clone();
        // Round-trip commission: 2x taker fee (entry + exit)
        // Default: 0.08% (2x 0.04% taker fee)
        let commission_pct = 0.08;
        // ATR multipliers for dynamic TP/SL (matched with backtest exits)
        let atr_sl_multiplier = config.atr_sl_multiplier;
        let atr_tp_multiplier = config.atr_tp_multiplier;
        let handle = tokio::spawn(async move {
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
        task_infos.push(TaskInfo {
            name: "follow_orders".to_string(),
            handle: Arc::new(tokio::sync::Mutex::new(Some(handle))),
        });
    }

    {
        let ch = balance_ch;
        let state = shared_state.clone();
        let handle = tokio::spawn(async move {
            balance::run_balance(ch, state).await;
        });
        task_infos.push(TaskInfo {
            name: "balance".to_string(),
            handle: Arc::new(tokio::sync::Mutex::new(Some(handle))),
        });
    }

    {
        let ch = logging_ch;
        let handle = tokio::spawn(async move {
            logging::run_logging(ch).await;
        });
        task_infos.push(TaskInfo {
            name: "logging".to_string(),
            handle: Arc::new(tokio::sync::Mutex::new(Some(handle))),
        });
    }

    // ✅ ADIM 4: Metrics cache oluştur ve background task başlat
    // Cache update interval'i config'den oku (default: 300 = 5 dakika)
    let cache_update_interval = file_cfg
        .event_bus
        .as_ref()
        .and_then(|eb| eb.metrics_cache_update_interval_secs)
        .unwrap_or(300);
    let metrics_cache = Arc::new(MetricsCache::new(cache_update_interval));

    // ✅ ADIM 3: Multi-symbol scanner desteği
    // Önce balance kontrolü yap - yeterli balance yoksa o quote için tarama yapma

    // Önce scanner config'i oluştur (min_balance_threshold için)
    let temp_allowed_quotes = if file_cfg.allow_usdt_quote.unwrap_or(false) {
        vec!["USDT".to_string(), "USDC".to_string()]
    } else {
        vec![file_cfg
            .quote_asset
            .clone()
            .unwrap_or_else(|| "USDT".to_string())]
    };
    let temp_scanner_config =
        SymbolSelectionConfig::from_file_config(&file_cfg, temp_allowed_quotes);
    let min_balance_threshold = temp_scanner_config.min_balance_threshold(&file_cfg);

    // Balance'ı fetch et (bot başlarken) - tek seferde hem USDT hem USDC
    let (usdt_balance, usdc_balance) = match connection.fetch_balance().await {
        Ok(balances) => {
            let usdt = balances
                .iter()
                .find(|b| b.asset == "USDT")
                .map(|b| b.free)
                .unwrap_or(0.0);
            let usdc = balances
                .iter()
                .find(|b| b.asset == "USDC")
                .map(|b| b.free)
                .unwrap_or(0.0);
            (usdt, usdc)
        }
        Err(err) => {
            warn!(
                "SYMBOL_SCANNER: Failed to fetch balance: {}, assuming 0 for both",
                err
            );
            (0.0, 0.0)
        }
    };

    info!(
        "SYMBOL_SCANNER: Balance check - USDT: {:.2}, USDC: {:.2} (min threshold: {:.2})",
        usdt_balance, usdc_balance, min_balance_threshold
    );

    // Balance'a göre allowed_quotes'u filtrele
    let mut allowed_quotes = Vec::new();

    if file_cfg.allow_usdt_quote.unwrap_or(false) {
        // USDT balance yeterli mi?
        if usdt_balance >= min_balance_threshold {
            allowed_quotes.push("USDT".to_string());
            info!(
                "SYMBOL_SCANNER: USDT balance sufficient ({:.2}), including USDT symbols",
                usdt_balance
            );
        } else {
            warn!(
                "SYMBOL_SCANNER: USDT balance insufficient ({:.2} < {:.2}), excluding USDT symbols",
                usdt_balance, min_balance_threshold
            );
        }

        // USDC balance yeterli mi?
        if usdc_balance >= min_balance_threshold {
            allowed_quotes.push("USDC".to_string());
            info!(
                "SYMBOL_SCANNER: USDC balance sufficient ({:.2}), including USDC symbols",
                usdc_balance
            );
        } else {
            warn!(
                "SYMBOL_SCANNER: USDC balance insufficient ({:.2} < {:.2}), excluding USDC symbols",
                usdc_balance, min_balance_threshold
            );
        }
    } else {
        // Sadece config'teki quote_asset
        let quote = file_cfg
            .quote_asset
            .clone()
            .unwrap_or_else(|| "USDT".to_string());
        let balance = if quote == "USDT" {
            usdt_balance
        } else if quote == "USDC" {
            usdc_balance
        } else {
            0.0
        };

        if balance >= min_balance_threshold {
            allowed_quotes.push(quote.clone());
            info!(
                "SYMBOL_SCANNER: {} balance sufficient ({:.2}), including {} symbols",
                quote, balance, quote
            );
        } else {
            warn!(
                "SYMBOL_SCANNER: {} balance insufficient ({:.2} < {:.2}), excluding {} symbols",
                quote, balance, min_balance_threshold, quote
            );
            // Fallback: Eğer hiç balance yoksa, yine de tek symbol kullan (config.symbol)
            allowed_quotes.push(quote);
        }
    }

    // Eğer hiç allowed_quotes yoksa, fallback olarak config.symbol kullan
    if allowed_quotes.is_empty() {
        warn!("SYMBOL_SCANNER: No sufficient balance for any quote asset, falling back to single symbol mode");
        // allowed_quotes boş kalacak, scanner disabled olacak
    }

    let scanner_config = SymbolSelectionConfig::from_file_config(&file_cfg, allowed_quotes);

    let scanner = Arc::new(SymbolScanner::new(scanner_config));

    // İlk symbol seçimini yap
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

    // Cache'e symbol'leri set et
    metrics_cache.set_symbols(initial_symbols.clone()).await;

    // ✅ CRITICAL: Warm up symbol cache (TrendPlan.md - Fast Order Execution)
    info!("CACHE: Warming up symbol info cache...");
    symbol_cache.warmup(&initial_symbols, &connection).await;
    info!("CACHE: Symbol info cache warmup complete");

    // Cache update task
    let cache_for_update = metrics_cache.clone();
    let cache_update_handle = tokio::spawn(async move {
        cache_for_update.run_update_task().await;
    });
    task_infos.push(TaskInfo {
        name: "metrics_cache".to_string(),
        handle: Arc::new(tokio::sync::Mutex::new(Some(cache_update_handle))),
    });

    // ✅ ADIM 3: Symbol scanner rotation task (eğer enabled ise)
    let rotation_tx = if scanner.config.enabled {
        let rotation_tx = bus.rotation_sender();
        let scanner_for_rotation = scanner.clone();
        let rotation_tx_clone = rotation_tx.clone();
        let rotation_handle = tokio::spawn(async move {
            scanner_for_rotation.run_rotation_task(rotation_tx_clone).await;
        });
        task_infos.push(TaskInfo {
            name: "symbol_scanner_rotation".to_string(),
            handle: Arc::new(tokio::sync::Mutex::new(Some(rotation_handle))),
        });
        Some(rotation_tx)
    } else {
        None
    };

    // ✅ ADIM 3: Her symbol için ayrı trending task spawn et
    let trend_params = config.trend_params();
    let ws_base_url = config.ws_base_url.clone();

    // Track active trending tasks: symbol -> task handle
    type TaskHandle = Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>;
    let active_trending_tasks: Arc<tokio::sync::RwLock<std::collections::HashMap<String, TaskHandle>>> =
        Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));

    // Helper function to spawn a trending task
    async fn spawn_trending_task(
        symbol: String,
        bus: &EventBus,
        trend_params: &TrendParams,
        ws_base_url: &str,
        metrics_cache: Arc<MetricsCache>,
        tasks_map: Arc<tokio::sync::RwLock<std::collections::HashMap<String, TaskHandle>>>,
        task_infos: Arc<tokio::sync::Mutex<Vec<TaskInfo>>>,
    ) {
        let symbol_clone = symbol.clone();
        let ch = bus.trending_channels();
        let trend_params_clone = trend_params.clone();
        let ws_base_url_clone = ws_base_url.to_string();
        let handle = tokio::spawn(async move {
            trending::run_trending(
                ch,
                symbol_clone,
                trend_params_clone,
                ws_base_url_clone,
                Some(metrics_cache),
            )
            .await;
        });
        let handle_arc = Arc::new(tokio::sync::Mutex::new(Some(handle)));
        {
            let mut tasks = tasks_map.write().await;
            tasks.insert(symbol.clone(), handle_arc.clone());
        }
        {
            let mut infos = task_infos.lock().await;
            infos.push(TaskInfo {
                name: format!("trending_{}", symbol),
                handle: handle_arc,
            });
        }
        info!("TRENDING: Started trending task for symbol {}", symbol);
    }

    // ✅ CRITICAL FIX: Use Combined Stream for 3+ symbols to avoid connection limits (TrendPlan.md - Action Plan)
    // Combined Stream reduces WebSocket connections from N to 1, preventing IP bans
    // Threshold: 3+ symbols = use combined stream, <3 symbols = use individual streams
    let use_combined_stream = initial_symbols.len() >= 3;
    
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
            // Create AlgoConfig from TrendParams (same as run_trending)
            let cfg = trading_bot::types::AlgoConfig {
                rsi_trend_long_min: trend_params.rsi_long_min,
                rsi_trend_short_max: trend_params.rsi_short_max,
                funding_extreme_pos: trend_params.funding_max_for_long.max(0.0001),
                funding_extreme_neg: trend_params.funding_min_for_short.min(-0.0001),
                lsr_crowded_long: trend_params.obi_long_min.max(1.3),
                lsr_crowded_short: trend_params.obi_short_max.min(0.8),
                long_min_score: trend_params.long_min_score,
                short_min_score: trend_params.short_min_score,
                // Execution & Backtest Parameters (from config, no hardcoded values)
                fee_bps_round_trip: trend_params.fee_bps_round_trip,
                max_holding_bars: trend_params.max_holding_bars,
                slippage_bps: trend_params.slippage_bps,
                min_holding_bars: trend_params.min_holding_bars,
                // Signal Quality Filtering (from config)
                min_volume_ratio: trend_params.min_volume_ratio,
                max_volatility_pct: trend_params.max_volatility_pct,
                max_price_change_5bars_pct: trend_params.max_price_change_5bars_pct,
                enable_signal_quality_filter: trend_params.enable_signal_quality_filter,
                // Stop Loss & Risk Management
                atr_stop_loss_multiplier: trend_params.atr_sl_multiplier,
                atr_take_profit_multiplier: trend_params.atr_tp_multiplier,
                hft_mode: trend_params.hft_mode,
                base_min_score: trend_params.base_min_score,
                trend_threshold_hft: trend_params.trend_threshold_hft,
                trend_threshold_normal: trend_params.trend_threshold_normal,
                weak_trend_score_multiplier: trend_params.weak_trend_score_multiplier,
                regime_multiplier_trending: trend_params.regime_multiplier_trending,
                regime_multiplier_ranging: trend_params.regime_multiplier_ranging,
                enable_enhanced_scoring: trend_params.enable_enhanced_scoring,
                enhanced_score_excellent: trend_params.enhanced_score_excellent,
                enhanced_score_good: trend_params.enhanced_score_good,
                enhanced_score_marginal: trend_params.enhanced_score_marginal,
                enable_order_flow: trend_params.enable_order_flow,
            };
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
            let market_rx = bus.market_receiver();
            let latest_tick_clone = latest_market_tick.clone();
            let symbol_clone = symbol.clone();
            tokio::spawn(async move {
                loop {
                    match crate::types::handle_broadcast_recv(market_rx.recv().await) {
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
        let combined_stream_handle = tokio::spawn(async move {
            trending::run_combined_kline_stream(
                initial_symbols.clone(),
                "5m",
                "5m",
                ws_base_url,
                symbol_handlers,
            ).await;
        });
        task_infos.push(TaskInfo {
            name: "combined_kline_stream".to_string(),
            handle: Arc::new(tokio::sync::Mutex::new(Some(combined_stream_handle))),
        });
    } else {
        // Use individual streams for <3 symbols (backward compatibility)
        info!("TRENDING: Using individual streams for {} symbols", initial_symbols.len());
        let task_infos_arc = Arc::new(tokio::sync::Mutex::new(task_infos.clone()));
        for symbol in initial_symbols.iter() {
            spawn_trending_task(
                symbol.clone(),
                &bus,
                &trend_params,
                &ws_base_url,
                metrics_cache.clone(),
                active_trending_tasks.clone(),
                task_infos_arc.clone(),
            ).await;
        }
        // Update task_infos from arc
        {
            let infos = task_infos_arc.lock().await;
            task_infos = (*infos).clone();
        }
    }

    // ✅ Rotation supervisor task: handles symbol rotation events
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
                            spawn_trending_task(
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
        task_infos.push(TaskInfo {
            name: "rotation_supervisor".to_string(),
            handle: Arc::new(tokio::sync::Mutex::new(Some(supervisor_handle))),
        });
    }

    // Health check task: monitor all tasks and log their status
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

    // Wait for all tasks (including health check)
    // Use tokio::select! to handle graceful shutdown
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

    // Cancel health check task
    health_check_task.abort();

    info!("MAIN: Shutting down...");
    Ok(())
}
