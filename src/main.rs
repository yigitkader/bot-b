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
use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::time::Duration;
use tracing::info;

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
    
    // Create event bus
    let event_bus = Arc::new(EventBus::new());
    
    // Get symbols from config
    let symbols = if !cfg.symbols.is_empty() {
        cfg.symbols.clone()
    } else if let Some(symbol) = &cfg.symbol {
        vec![symbol.clone()]
    } else {
        vec![] // Will be discovered if auto_discover_quote is true
    };
    
    // Initialize CONNECTION module (only module that creates exchange connection)
    let connection = Arc::new(Connection::from_config(
        cfg.clone(),
        event_bus.clone(),
        shutdown_flag.clone(),
    )?);
    
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
        connection.clone(),
        event_bus.clone(),
        shutdown_flag.clone(),
        shared_state.clone(),
    );
    ordering.start().await?;
    
    // Initialize FOLLOW_ORDERS module
    let follow_orders = FollowOrders::new(
        event_bus.clone(),
        shutdown_flag.clone(),
    );
    follow_orders.start().await?;
    
    // Initialize TRENDING module
    let trending = Trending::new(
        cfg.clone(),
        event_bus.clone(),
        shutdown_flag.clone(),
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
    
    // Wait for shutdown signal
    while !shutdown_flag.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    
    info!("Shutdown signal received, exiting...");
    
    Ok(())
}
