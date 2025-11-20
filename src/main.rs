use anyhow::Result;
use log::{error, info, warn};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio::time::interval;
use trading_bot::{
    balance, config::BotConfig, follow_orders, logging, ordering, trending,
};
use trading_bot::types::FileConfig;
use trading_bot::{Connection, EventBus, SharedState};

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    let config = BotConfig::from_env();
    info!("Starting trading bot with config: {:?}", config);

    let shared_state = SharedState::new();
    let connection = Arc::new(Connection::new(config.clone()));

    // Initialize trading settings (margin mode and leverage) at startup
    if let Err(err) = connection.initialize_trading_settings().await {
        error!("Failed to initialize trading settings: {err:?}");
        return Err(err);
    }

    // Load event bus buffer sizes from config
    let file_cfg = trading_bot::types::FileConfig::load("config.yaml").unwrap_or_default();
    let event_bus_cfg = file_cfg.event_bus.unwrap_or_default();
    
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

    let trending_ch = bus.trending_channels();
    let ordering_ch = bus.ordering_channels();
    let follow_ch = bus.follow_channels();
    let balance_ch = bus.balance_channels();
    let logging_ch = bus.logging_channels();
    let connection_market_ch = bus.connection_channels();
    let connection_user_ch = bus.connection_channels();

    // Task monitoring: track task names and handles for health checking
    #[derive(Clone)]
    struct TaskInfo {
        name: String,
        handle: Arc<tokio::sync::Mutex<Option<JoinHandle<()>>>>,
    }
    
    let mut task_infos: Vec<TaskInfo> = Vec::new();

    {
        let conn = connection.clone();
        let ch = connection_market_ch.clone();
        let handle = tokio::spawn(async move {
            if let Err(err) = conn.run_market_ws(ch).await {
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

    {
        let conn = connection.clone();
        let state = shared_state.clone();
        let ch = ordering_ch;
        let handle = tokio::spawn(async move {
            ordering::run_ordering(ch, state, conn).await;
        });
        task_infos.push(TaskInfo {
            name: "ordering".to_string(),
            handle: Arc::new(tokio::sync::Mutex::new(Some(handle))),
        });
    }

    {
        let ch = follow_ch;
        // Round-trip commission: 2x taker fee (entry + exit)
        // Default: 0.08% (2x 0.04% taker fee)
        let commission_pct = 0.08;
        // ATR multipliers for dynamic TP/SL (currently not used, but available in config)
        let atr_sl_multiplier = config.atr_sl_multiplier;
        let atr_tp_multiplier = config.atr_tp_multiplier;
        let handle = tokio::spawn(async move {
            follow_orders::run_follow_orders(
                ch, 
                config.tp_percent, 
                config.sl_percent, 
                commission_pct,
                atr_sl_multiplier,
                atr_tp_multiplier,
            ).await;
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

    {
        let ch = trending_ch;
        let symbol = config.symbol.clone();
        let trend_params = config.trend_params();
        let ws_base_url = config.ws_base_url.clone();
        let handle = tokio::spawn(async move {
            trending::run_trending(ch, symbol, trend_params, ws_base_url).await;
        });
        task_infos.push(TaskInfo {
            name: "trending".to_string(),
            handle: Arc::new(tokio::sync::Mutex::new(Some(handle))),
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
                        warn!("HEALTH: Task '{}' has finished (may have crashed)", task_info.name);
                        // Note: In production, you might want to restart the task here
                        // For now, we just log the event
                    }
                } else {
                    warn!("HEALTH: Task '{}' handle is None", task_info.name);
                }
            }
            
            if finished_count > 0 {
                error!("HEALTH: {} out of {} tasks have finished", finished_count, health_check_task_infos.len());
            } else {
                info!("HEALTH: All {} tasks running normally", health_check_task_infos.len());
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
