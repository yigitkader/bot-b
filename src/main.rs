use anyhow::Result;
use futures::future::join_all;
use log::{error, info};
use std::sync::Arc;
use tokio::task::JoinHandle;
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

    let mut tasks: Vec<JoinHandle<()>> = Vec::new();

    {
        let conn = connection.clone();
        let ch = connection_market_ch.clone();
        tasks.push(tokio::spawn(async move {
            if let Err(err) = conn.run_market_ws(ch).await {
                error!("Market WS task failed: {err:?}");
            }
        }));
    }

    {
        let conn = connection.clone();
        let ch = connection_user_ch.clone();
        tasks.push(tokio::spawn(async move {
            if let Err(err) = conn.run_user_ws(ch).await {
                error!("User WS task failed: {err:?}");
            }
        }));
    }

    {
        let conn = connection.clone();
        let state = shared_state.clone();
        let ch = ordering_ch;
        tasks.push(tokio::spawn(async move {
            ordering::run_ordering(ch, state, conn).await;
        }));
    }

    {
        let ch = follow_ch;
        // Round-trip commission: 2x taker fee (entry + exit)
        // Default: 0.08% (2x 0.04% taker fee)
        let commission_pct = 0.08;
        tasks.push(tokio::spawn(async move {
            follow_orders::run_follow_orders(ch, config.tp_percent, config.sl_percent, commission_pct).await;
        }));
    }

    {
        let ch = balance_ch;
        let conn = connection.clone();
        let state = shared_state.clone();
        tasks.push(tokio::spawn(async move {
            balance::run_balance(conn, ch, state).await;
        }));
    }

    {
        let ch = logging_ch;
        tasks.push(tokio::spawn(async move {
            logging::run_logging(ch).await;
        }));
    }

    {
        let ch = trending_ch;
        let symbol = config.symbol.clone();
        let trend_params = config.trend_params();
        tasks.push(tokio::spawn(async move {
            trending::run_trending(ch, symbol, trend_params).await;
        }));
    }

    join_all(tasks).await;
    Ok(())
}
