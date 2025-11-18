
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
mod metrics;
mod ai_analyzer;
mod risk;
mod position_manager;
mod utils;
mod event_loop;
use crate::ai_analyzer::AiAnalyzer;
use crate::balance::Balance;
use crate::config::{load_config, AppCfg};
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
    std::fs::create_dir_all("logs")?;
    let _log_file = std::fs::File::create("logs/console.log")?;
    drop(_log_file);
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("debug"));
    let file_appender = tracing_appender::rolling::never("logs", "console.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(true)
                .with_writer(std::io::stdout)
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(non_blocking)
        )
        .init();
    let _file_guard = _guard;
    let cfg = Arc::new(load_config()?);
    let (json_logger, _logger_handle) = crate::logging::create_logger("logs/trading_events.json")?;
    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let shutdown_flag_clone = shutdown_flag.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            info!("Ctrl+C signal received, shutting down");
            shutdown_flag_clone.store(true, Ordering::Relaxed);
        }
    });
    let shared_state = Arc::new(SharedState::new());
    let event_bus = Arc::new(EventBus::new_with_config(&cfg.event_bus));
    let connection = Arc::new(Connection::from_config(
        cfg.clone(),
        event_bus.clone(),
        shutdown_flag.clone(),
        Some(shared_state.clone()),
    )?);
    let symbols = if !cfg.symbols.is_empty() {
        cfg.symbols.clone()
    } else if let Some(symbol) = &cfg.symbol {
        vec![symbol.clone()]
    } else if cfg.auto_discover_quote {
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
        return Err(anyhow!("No symbols specified and auto_discover_quote is false. Please specify symbols in config or enable auto_discover_quote."));
    };
    connection.start(symbols).await?;
    let balance = Balance::new(
        connection.clone(),
        event_bus.clone(),
        shutdown_flag.clone(),
        shared_state.clone(),
    );
    balance.start().await?;
    let ordering = OrderingModule::new(
        cfg.clone(),
        connection.clone(),
        event_bus.clone(),
        shutdown_flag.clone(),
        shared_state.clone(),
    );
    ordering.start().await?;
    let follow_orders = FollowOrders::new(
        cfg.clone(),
        event_bus.clone(),
        shutdown_flag.clone(),
        connection.clone(),
    );
    follow_orders.start().await?;
    let trending = Trending::new(
        cfg.clone(),
        event_bus.clone(),
        shutdown_flag.clone(),
    );
    trending.start().await?;
    let logging = Logging::new(
        event_bus.clone(),
        json_logger,
        shutdown_flag.clone(),
    );
    logging.start().await?;
    let ai_analyzer = Arc::new(AiAnalyzer::new(
        event_bus.clone(),
        shutdown_flag.clone(),
    ));
    ai_analyzer.start().await?;
    info!("All modules started, waiting for shutdown signal...");
    if cfg.dynamic_symbol_selection.enabled {
        start_dynamic_symbol_rotation(
            connection.clone(),
            event_bus.clone(),
            shutdown_flag.clone(),
            cfg.clone(),
        );
    }
    let event_bus_health = event_bus.clone();
    let ai_analyzer_health = ai_analyzer.clone();
    let shutdown_flag_health = shutdown_flag.clone();
    let app_start_time = Instant::now();
    tokio::spawn(async move {
        const HEALTH_CHECK_INTERVAL_SECS: u64 = 10;
        loop {
            tokio::time::sleep(Duration::from_secs(HEALTH_CHECK_INTERVAL_SECS)).await;
            if shutdown_flag_health.load(Ordering::Relaxed) {
                break;
            }
            let uptime_secs = app_start_time.elapsed().as_secs();
            perform_health_check(&event_bus_health, &ai_analyzer_health, uptime_secs).await;
        }
    });
    while !shutdown_flag.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    info!("Shutdown signal received, exiting...");
    Ok(())
}
async fn perform_health_check(event_bus: &EventBus, ai_analyzer: &AiAnalyzer, uptime_secs: u64) {
    let health = event_bus.health_stats();
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
    let memory_info = get_memory_info();
    if let Some(mem_mb) = memory_info {
        info!(
            memory_mb = mem_mb,
            "HEALTH: Process memory usage"
        );
    }
    let trade_stats = ai_analyzer.get_trade_stats().await;
    let order_stats = ai_analyzer.get_order_stats().await;
    let recent_anomalies = ai_analyzer.get_recent_anomalies(5).await;
    if !trade_stats.is_empty() {
        let total_trades: u64 = trade_stats.values().map(|s| s.total_trades).sum();
        let total_pnl: f64 = trade_stats.values().map(|s| s.total_pnl).sum();
        info!(
            total_trades,
            total_pnl,
            "HEALTH: AI_ANALYZER trade statistics"
        );
    }
    if !order_stats.is_empty() {
        let total_orders: u64 = order_stats.values().map(|s| s.total_orders).sum();
        let rejected_orders: u64 = order_stats.values().map(|s| s.rejected_orders).sum();
        let rejection_rate = if total_orders > 0 {
            (rejected_orders as f64) / (total_orders as f64) * 100.0
        } else {
            0.0
        };
        info!(
            total_orders,
            rejected_orders,
            rejection_rate = format!("{:.1}%", rejection_rate),
            "HEALTH: AI_ANALYZER order statistics"
        );
    }
    if !recent_anomalies.is_empty() {
        let high_severity = recent_anomalies.iter()
            .filter(|a| matches!(a.severity, crate::ai_analyzer::Severity::High))
            .count();
        let medium_severity = recent_anomalies.iter()
            .filter(|a| matches!(a.severity, crate::ai_analyzer::Severity::Medium))
            .count();
        if high_severity > 0 {
            warn!(
                high_severity,
                medium_severity,
                "HEALTH: ⚠️ AI_ANALYZER detected {} high and {} medium severity anomalies",
                high_severity, medium_severity
            );
        } else if medium_severity > 0 {
            info!(
                medium_severity,
                "HEALTH: AI_ANALYZER detected {} medium severity anomalies",
                medium_severity
            );
        }
    }
}
fn get_memory_info() -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            for line in status.lines() {
                if line.starts_with("VmRSS:") {
                    if let Some(kb_str) = line.split_whitespace().nth(1) {
                        if let Ok(kb) = kb_str.parse::<u64>() {
                            return Some(kb as f64 / 1024.0);
                        }
                    }
                }
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
    }
    #[cfg(target_os = "windows")]
    {
    }
    None
}
fn start_dynamic_symbol_rotation(
    connection: Arc<Connection>,
    _event_bus: Arc<EventBus>,
    shutdown_flag: Arc<AtomicBool>,
    cfg: Arc<AppCfg>,
) {
    let rotation_interval = cfg.dynamic_symbol_selection.rotation_interval_minutes;
    let max_symbols = cfg.dynamic_symbol_selection.max_symbols;
    tokio::spawn(async move {
        let mut first_rotation = true;
        loop {
            if shutdown_flag.load(Ordering::Relaxed) {
                break;
            }
            info!("Starting symbol rotation...");
            match connection.discover_and_rank_symbols().await {
                Ok(mut scored_symbols) => {
                    scored_symbols.truncate(max_symbols);
                    info!("Top {} symbols selected:", scored_symbols.len());
                    for (i, scored) in scored_symbols.iter().enumerate().take(10) {
                        info!(
                            rank = i + 1,
                            symbol = %scored.symbol,
                            score = scored.score,
                            volatility = scored.volatility,
                            volume = scored.volume,
                            "Top symbol"
                        );
                    }
                    let new_symbols: Vec<String> = scored_symbols
                        .iter()
                        .map(|s| s.symbol.clone())
                        .collect();
                    if let Err(e) = connection.restart_market_streams(new_symbols).await {
                        error!(error = %e, "Failed to restart market streams");
                    }
                }
                Err(e) => {
                    error!(error = %e, "Failed to discover and rank symbols");
                }
            }
            if first_rotation {
                first_rotation = false;
            } else {
                tokio::time::sleep(Duration::from_secs(rotation_interval * 60)).await;
            }
        }
    });
}
