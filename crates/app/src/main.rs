//location: /crates/app/src/main.rs
// Simplified Main - Parallel architecture: Trend analysis and Order execution

mod balance;
mod trend;
mod connection;
mod order;

// Keep essential modules
mod config;
mod types;
mod utils;
mod logger;

use anyhow::Result;
use crate::config::load_config;
use crate::types::*;
use crate::connection::UserEvent;
use crate::balance::{check_balances, get_symbols_matched_with_our_balance, get_current_symbol_prices};
use crate::trend::{describe_and_analyse_token, TrendLearner, get_best_tokens_to_invest_now_after_analyse, TokenAnalysis};
use crate::connection::{handle_apis, handle_ws_connection};
use crate::order::{create_open_long_order, handle_open_short_order, check_position, close_position, save_profits_and_loses, PnLTracker};
use crate::logger::create_logger;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{info, warn};
use std::sync::Arc;
use tokio::sync::Mutex;

// Trend analysis result for order module
#[derive(Debug, Clone)]
pub struct TrendSignal {
    pub symbol: String,
    pub recommendation: String, // "LONG", "SHORT", "HOLD"
    pub trend_score: f64,
    pub signal_strength: f64,
    pub bid: Decimal,
    pub ask: Decimal,
}

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

    // Load config
    let cfg = load_config()?;
    info!("configuration loaded");

    // 1. Connection Module: Handle APIs and WebSocket
    let venue = Arc::new(handle_apis(&cfg).await?);
    
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    if cfg.websocket.enabled {
        handle_ws_connection(&cfg, event_tx.clone()).await;
    }

    // 2. Log Module: Initialize logger
    let _logger = match create_logger("logs/trading_events.json") {
        Ok(logger) => logger,
        Err(e) => {
            warn!(error = %e, "Failed to create logger, continuing without logging");
            return Err(anyhow::anyhow!("Failed to create logger: {}", e));
        }
    };
    info!("logger initialized");

    // 3. Trend Module: Initialize AI learner
    let learner = Arc::new(Mutex::new(TrendLearner::new()));
    info!("trend learner initialized");

    // 4. Order Module: Initialize PnL tracker
    let pnl_tracker = Arc::new(Mutex::new(PnLTracker::new()));
    info!("PnL tracker initialized");

    // Channel for trend signals -> order module
    let (trend_tx, mut trend_rx) = mpsc::unbounded_channel::<TrendSignal>();

    // Get symbols from config
    let symbols = if cfg.symbols.is_empty() {
        vec!["BTCUSDC".to_string(), "ETHUSDC".to_string()]
    } else {
        cfg.symbols.clone()
    };
    let quote_assets: Vec<String> = symbols.iter()
        .map(|s| {
            if s.ends_with("USDC") { "USDC".to_string() }
            else if s.ends_with("USDT") { "USDT".to_string() }
            else { "USDC".to_string() }
        })
            .collect();
        
    // Price history for trend analysis (per symbol) - shared state
    let price_history: Arc<Mutex<HashMap<String, Vec<(Instant, Decimal)>>>> = 
        Arc::new(Mutex::new(HashMap::new()));

    // ============================================================================
    // PARALLEL SERVICES
    // ============================================================================

    // Service 1: Trend Analysis Service (runs continuously)
    let venue_trend = venue.clone();
    let learner_trend = learner.clone();
    let price_history_trend = price_history.clone();
    let trend_tx_clone = trend_tx.clone();
    let symbols_trend = symbols.clone();
    let quote_assets_trend = quote_assets.clone();
    let cfg_trend = cfg.clone();
    
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        loop {
            interval.tick().await;
            
            // 1. Check balances
            let balances = check_balances(&venue_trend, &quote_assets_trend).await;
            let symbols_with_balance = get_symbols_matched_with_our_balance(
                &symbols_trend,
                &balances,
                cfg_trend.min_quote_balance_usd,
            );
            
            if symbols_with_balance.is_empty() {
                continue;
            }
            
            // 2. Get current prices
            let prices = get_current_symbol_prices(&venue_trend, &symbols_with_balance).await;
            
            // 3. Analyze tokens
            let mut analyses: Vec<TokenAnalysis> = Vec::new();
            for symbol in &symbols_with_balance {
                if let Some((bid, ask)) = prices.get(symbol) {
                    // Update price history
                    let mut history_guard = price_history_trend.lock().await;
                    let history = history_guard.entry(symbol.clone()).or_insert_with(Vec::new);
                    history.push((Instant::now(), (*bid + *ask) / Decimal::from(2)));
                    
                    // Keep only last 100 prices
                    if history.len() > 100 {
                        history.remove(0);
                    }
                    
                    let history_snapshot = history.clone();
                    drop(history_guard);
                    
                    // Analyze token
                    let learner_guard = learner_trend.lock().await;
                    let analysis = describe_and_analyse_token(
                        symbol,
                        *bid,
                        *ask,
                        &history_snapshot,
                        &learner_guard,
                    );
                    drop(learner_guard);
                    analyses.push(analysis);
                }
            }
            
            // 4. Get best tokens
            let best_tokens = get_best_tokens_to_invest_now_after_analyse(&analyses, 5);
            
            // 5. Send signals to order module
            for symbol in &best_tokens {
                if let Some(analysis) = analyses.iter().find(|a| a.symbol == *symbol) {
                    if let Some((bid, ask)) = prices.get(symbol) {
                        if analysis.recommendation != "HOLD" {
                            let signal = TrendSignal {
                                symbol: symbol.clone(),
                                recommendation: analysis.recommendation.clone(),
                                trend_score: analysis.trend_score,
                                signal_strength: analysis.signal_strength,
                                bid: *bid,
                                ask: *ask,
                            };
                            if trend_tx_clone.send(signal).is_err() {
                                warn!("trend signal channel closed");
                                break;
                            }
                        }
                    }
                }
            }
        }
    });

    // Service 2: Order Execution Service (runs continuously, processes trend signals)
    let venue_order = venue.clone();
    let pnl_tracker_order = pnl_tracker.clone();
    
    tokio::spawn(async move {
        loop {
            tokio::select! {
                // Process trend signals
                signal = trend_rx.recv() => {
                    if let Some(signal) = signal {
                        // Check current position
                        match check_position(&venue_order, &signal.symbol).await {
                            Ok(position) => {
                                // If we have a position, check if we should close it
                                if !position.qty.0.is_zero() {
                                    let should_close = (signal.recommendation == "LONG" && position.qty.0.is_sign_negative())
                                        || (signal.recommendation == "SHORT" && position.qty.0.is_sign_positive());
                                    
                                    if should_close {
                                        if let Err(e) = close_position(&venue_order, &signal.symbol, &position).await {
                                            warn!(symbol = %signal.symbol, error = %e, "failed to close position");
                                        } else {
                                            // Save PnL
                                            let mut tracker = pnl_tracker_order.lock().await;
                                            save_profits_and_loses(
                                                &mut tracker,
                                                &signal.symbol,
                                                position.entry.0,
                                                position.entry.0, // TODO: Get actual exit price
                                                position.qty.0.abs(),
                                                if position.qty.0.is_sign_positive() { Side::Buy } else { Side::Sell },
                                                2.0, // entry fee bps
                                                4.0, // exit fee bps
                                            );
                                        }
                                    }
                                        } else {
                                            // No position, create new order
                                            let price = if signal.recommendation == "LONG" {
                                                signal.bid // Buy at bid
                                            } else {
                                                signal.ask // Sell at ask
                                            };
                                    
                                            // Simple quantity calculation (10 USDC per order)
                                            let qty = Decimal::from(10) / price;
                                    
                                    if signal.recommendation == "LONG" {
                                        if let Err(e) = create_open_long_order(&venue_order, &signal.symbol, price, qty, Tif::PostOnly).await {
                                            warn!(symbol = %signal.symbol, error = %e, "failed to create long order");
                                        }
                                    } else if signal.recommendation == "SHORT" {
                                        if let Err(e) = handle_open_short_order(&venue_order, &signal.symbol, price, qty, Tif::PostOnly).await {
                                            warn!(symbol = %signal.symbol, error = %e, "failed to create short order");
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                warn!(symbol = %signal.symbol, error = %e, "failed to check position");
                            }
                        }
                    } else {
                        warn!("trend signal channel closed");
                        break;
                    }
                }
            }
        }
    });

    // Main loop: Handle WebSocket events and periodic tasks
    let mut interval = tokio::time::interval(Duration::from_secs(10));
    info!("main loop starting (orchestration only)");

    loop {
        tokio::select! {
            _ = interval.tick() => {
                // Periodic tasks (e.g., PnL summary)
                static TICK_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let tick = TICK_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if tick % 5 == 0 {
                    let tracker = pnl_tracker.lock().await;
                    let summary = tracker.get_summary();
                    info!(
                        total_trades = summary.total_trades,
                        profitable = summary.profitable_trades,
                        losing = summary.losing_trades,
                        total_pnl = summary.total_pnl,
                        win_rate = summary.win_rate,
                        "PnL summary"
                    );
                }
            }
            event = event_rx.recv() => {
                match event {
                    Some(UserEvent::Heartbeat) => {
                        info!("websocket heartbeat received");
                    }
                    Some(_) => {
                        // Handle other events if needed
                    }
                    None => {
                        warn!("websocket event channel closed");
                        return Ok(());
                    }
                }
            }
        }
    }
}
