// Main application entry point
// Clean architecture: event-driven modules with WebSocket-first approach

mod config;
mod types;
mod event_bus;
mod state;
mod connection;
mod trending;
mod ordering;
mod follow_orders;
mod balance;
mod logging;

use crate::balance::Balance;
use crate::config::load_config;
use crate::connection::Connection;
use crate::event_bus::EventBus;
use crate::follow_orders::FollowOrders;
use crate::logging::Logging;
use crate::ordering::Ordering as OrderingModule;
use crate::state::SharedState;
use crate::trending::Trending;
use anyhow::{anyhow, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::time::{Duration, Instant};
use tracing::{error, info, warn};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .compact()
        .with_ansi(true)
        .init();

    // Load configuration
    let cfg = Arc::new(load_config()?);
    
    // Initialize logger
    let (json_logger, _logger_handle) = crate::logging::create_logger("logs/trading_events.json")?;
    
    // Create shutdown flag
    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let shutdown_flag_clone = shutdown_flag.clone();
    
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            info!("Ctrl+C signal received, shutting down");
            shutdown_flag_clone.store(true, Ordering::Relaxed);
        }
    });
    
    // Create shared state
    let shared_state = Arc::new(SharedState::new());
    
    // Create event bus with configuration
    let event_bus = Arc::new(EventBus::new_with_config(&cfg.event_bus));
    
    // Initialize CONNECTION module (only module that creates exchange connection)
    // Pass shared_state for balance validation
    let connection = Arc::new(Connection::from_config(
        cfg.clone(),
        event_bus.clone(),
        shutdown_flag.clone(),
        Some(shared_state.clone()), // Pass shared_state for balance validation
    )?);
    
    // Get symbols from config or discover them
    let symbols = if !cfg.symbols.is_empty() {
        // Use manually specified symbols
        cfg.symbols.clone()
    } else if let Some(symbol) = &cfg.symbol {
        // Use single symbol from config
        vec![symbol.clone()]
    } else if cfg.auto_discover_quote {
        // Auto-discover symbols based on quote asset
        info!("Auto-discovering symbols with quote asset: {}", cfg.quote_asset);
        match connection.discover_symbols().await {
            Ok(discovered) => {
                if discovered.is_empty() {
                    return Err(anyhow!("No symbols discovered with auto_discover_quote=true. Check quote_asset and allow_usdt_quote settings."));
                }
                info!(
                    discovered_count = discovered.len(),
                    symbols = ?discovered.iter().take(10).collect::<Vec<_>>(),
                    "Discovered {} symbols", discovered.len()
                );
                discovered
            }
            Err(e) => {
                error!(
                    error = %e,
                    "Failed to discover symbols, cannot continue"
                );
                return Err(e);
            }
        }
    } else {
        // No symbols specified and auto-discovery is disabled
        return Err(anyhow!("No symbols specified and auto_discover_quote is false. Please specify symbols in config or enable auto_discover_quote."));
    };
    
    // Start CONNECTION
    connection.start(symbols).await?;
    
    // Initialize BALANCE module
    let balance = Balance::new(
        connection.clone(),
        event_bus.clone(),
        shutdown_flag.clone(),
        shared_state.clone(),
    );
    balance.start().await?;
    
    // Initialize ORDERING module
    let ordering = OrderingModule::new(
        cfg.clone(),
        connection.clone(),
        event_bus.clone(),
        shutdown_flag.clone(),
        shared_state.clone(),
    );
    ordering.start().await?;
    
    // Initialize FOLLOW_ORDERS module
    let follow_orders = FollowOrders::new(
        cfg.clone(),
        event_bus.clone(),
        shutdown_flag.clone(),
    );
    follow_orders.start().await?;
    
    // Initialize TRENDING module
    let trending = Trending::new(
        cfg.clone(),
        event_bus.clone(),
        shutdown_flag.clone(),
        shared_state.clone(),
        connection.clone(),
    );
    trending.start().await?;
    
    // Initialize LOGGING module
    let logging = Logging::new(
        event_bus.clone(),
        json_logger,
        shutdown_flag.clone(),
    );
    logging.start().await?;
    
    info!("All modules started, waiting for shutdown signal...");
    
    // Start health check task
    let event_bus_health = event_bus.clone();
    let shutdown_flag_health = shutdown_flag.clone();
    let app_start_time = Instant::now();
    tokio::spawn(async move {
        const HEALTH_CHECK_INTERVAL_SECS: u64 = 10;
        
        loop {
            tokio::time::sleep(Duration::from_secs(HEALTH_CHECK_INTERVAL_SECS)).await;
            
            if shutdown_flag_health.load(Ordering::Relaxed) {
                break;
            }
            
            // Perform health check
            let uptime_secs = app_start_time.elapsed().as_secs();
            perform_health_check(&event_bus_health, uptime_secs);
        }
    });
    
    // Wait for shutdown signal
    while !shutdown_flag.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    
    info!("Shutdown signal received, exiting...");
    
    Ok(())
}

/// Perform health check and log status
/// Checks event bus receiver counts and basic system health
fn perform_health_check(event_bus: &EventBus, uptime_secs: u64) {
    let health = event_bus.health_stats();
    
    // Check if any critical channels have no receivers (potential issue)
    let mut warnings = Vec::new();
    if health.order_update_receivers == 0 {
        warnings.push("OrderUpdate: no subscribers");
    }
    if health.position_update_receivers == 0 {
        warnings.push("PositionUpdate: no subscribers");
    }
    if health.trade_signal_receivers == 0 {
        warnings.push("TradeSignal: no subscribers");
    }
    
    // Log health status
    if warnings.is_empty() {
        info!(
            uptime_secs,
            market_tick_receivers = health.market_tick_receivers,
            trade_signal_receivers = health.trade_signal_receivers,
            close_request_receivers = health.close_request_receivers,
            order_update_receivers = health.order_update_receivers,
            position_update_receivers = health.position_update_receivers,
            balance_update_receivers = health.balance_update_receivers,
            "HEALTH: System healthy - all event channels have subscribers"
        );
    } else {
        warn!(
            uptime_secs,
            market_tick_receivers = health.market_tick_receivers,
            trade_signal_receivers = health.trade_signal_receivers,
            close_request_receivers = health.close_request_receivers,
            order_update_receivers = health.order_update_receivers,
            position_update_receivers = health.position_update_receivers,
            balance_update_receivers = health.balance_update_receivers,
            warnings = ?warnings,
            "HEALTH: Warning - some event channels have no subscribers"
        );
    }
    
    // Log memory usage (basic check)
    // Note: More detailed memory profiling would require additional dependencies
    // This is a basic health check to ensure the process is still running
    let memory_info = get_memory_info();
    if let Some(mem_mb) = memory_info {
        info!(
            memory_mb = mem_mb,
            "HEALTH: Process memory usage"
        );
    }
}

/// Get basic memory information
/// Returns memory usage in MB if available
/// Platform-specific implementation - returns None if not available
fn get_memory_info() -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        // Read from /proc/self/status on Linux
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            for line in status.lines() {
                if line.starts_with("VmRSS:") {
                    // Format: "VmRSS:    12345 kB"
                    if let Some(kb_str) = line.split_whitespace().nth(1) {
                        if let Ok(kb) = kb_str.parse::<u64>() {
                            return Some(kb as f64 / 1024.0); // Convert KB to MB
                        }
                    }
                }
            }
        }
    }
    
    #[cfg(target_os = "macos")]
    {
        // On macOS, we could use task_info, but it's complex
        // For now, return None - can be enhanced later
    }
    
    #[cfg(target_os = "windows")]
    {
        // On Windows, we could use GetProcessMemoryInfo, but it's complex
        // For now, return None - can be enhanced later
    }
    
    None
}
