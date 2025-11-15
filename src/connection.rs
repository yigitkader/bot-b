// CONNECTION: Exchange WS & REST single gateway
// All external world (WS/REST) goes through here
// Rate limit & reconnect management
// 
// This file contains ALL exchange-related code (previously in exchange.rs and exec.rs)
// Single responsibility: connection.rs = everything related to exchange communication

use crate::config::AppCfg;
use crate::event_bus::{EventBus, MarketTick, OrderUpdate, OrderStatus, PositionUpdate, BalanceUpdate};
use crate::state::SharedState;
use crate::types::{
    Px, Qty, Side, Tif, OrderCommand, VenueOrder, Position, SymbolRules, SymbolMeta,
    UserStreamKind, UserEvent, AccountPosition, AccountBalance, PriceUpdate,
};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use once_cell::sync::Lazy;
use reqwest::{Client, RequestBuilder, Response};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::{Decimal, RoundingStrategy};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;
use sha2::Sha256;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH, Instant};
use tokio::time::{timeout, Duration};
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message, WebSocketStream};
use tracing::{debug, error, info, warn};
use urlencoding::encode;

// ============================================================================
// Helper Functions
// ============================================================================

/// Extract precision (decimal places) from a decimal step size
fn decimal_places(step: Decimal) -> usize {
    if step.is_zero() {
        return 0;
    }
    let s = step.normalize().to_string();
    if let Some(pos) = s.find('.') {
        s[pos + 1..].trim_end_matches('0').len()
    } else {
        0
    }
}

// ============================================================================
// CONNECTION Module (Public API)
// ============================================================================

/// CONNECTION module - single gateway to exchange
/// Fill history for an order (used to calculate average fill price)
#[derive(Clone, Debug)]
struct OrderFillHistory {
    total_filled_qty: Qty,
    weighted_price_sum: Decimal, // Sum of (price * qty) for all fills
    last_update: Instant, // Timestamp of last update (for cleanup)
    /// Number of fills that were maker orders (for commission calculation)
    /// If all fills are maker, use maker commission; otherwise use taker commission
    maker_fill_count: u32,
    /// Total number of fills for this order
    total_fill_count: u32,
}

pub struct Connection {
    venue: Arc<BinanceFutures>,
    cfg: Arc<AppCfg>,
    event_bus: Arc<EventBus>,
    shutdown_flag: Arc<AtomicBool>,
    // Rate limiting: simple token bucket
    rate_limiter: Arc<tokio::sync::Mutex<RateLimiter>>,
    // Shared state for balance validation (optional, set after initialization)
    shared_state: Option<Arc<SharedState>>,
    // Order fill history: order_id -> fill history (for average fill price calculation)
    order_fill_history: Arc<DashMap<String, OrderFillHistory>>,
    // Persistent storage for state (optional, for restart recovery)
    storage: Option<Arc<crate::storage::StateStorage>>,
}

/// Simple rate limiter for REST API calls
/// Binance limits:
/// - Order placement: 300 orders per 5 minutes
/// - Balance query: 1200 requests per minute
struct RateLimiter {
    order_requests: Vec<Instant>,
    balance_requests: Vec<Instant>,
}

impl RateLimiter {
    fn new() -> Self {
        Self {
            order_requests: Vec::new(),
            balance_requests: Vec::new(),
        }
    }
    
    /// Check if order request is allowed (300 per 5 minutes)
    async fn check_order_rate(&mut self) {
        let now = Instant::now();
        let window = Duration::from_secs(5 * 60); // 5 minutes
        
        // Remove old requests outside window
        self.order_requests.retain(|&t| now.duration_since(t) < window);
        
        // If limit reached, wait
        if self.order_requests.len() >= 300 {
            if let Some(oldest) = self.order_requests.first() {
                let wait_time = window.saturating_sub(now.duration_since(*oldest));
                if !wait_time.is_zero() {
                    tokio::time::sleep(wait_time).await;
                    // Clean up again after wait
                    let now = Instant::now();
                    self.order_requests.retain(|&t| now.duration_since(t) < window);
                }
            }
        }
        
        self.order_requests.push(now);
    }
    
    /// Check if balance request is allowed (1200 per minute)
    async fn check_balance_rate(&mut self) {
        let now = Instant::now();
        let window = Duration::from_secs(60); // 1 minute
        
        // Remove old requests outside window
        self.balance_requests.retain(|&t| now.duration_since(t) < window);
        
        // If limit reached, wait
        if self.balance_requests.len() >= 1200 {
            if let Some(oldest) = self.balance_requests.first() {
                let wait_time = window.saturating_sub(now.duration_since(*oldest));
                if !wait_time.is_zero() {
                    tokio::time::sleep(wait_time).await;
                    // Clean up again after wait
                    let now = Instant::now();
                    self.balance_requests.retain(|&t| now.duration_since(t) < window);
                }
            }
        }
        
        self.balance_requests.push(now);
    }
}

impl Connection {
    pub fn from_config(
        cfg: Arc<AppCfg>,
        event_bus: Arc<EventBus>,
        shutdown_flag: Arc<AtomicBool>,
        shared_state: Option<Arc<SharedState>>,
    ) -> Result<Self> {
        let venue = Arc::new(BinanceFutures::from_config(
            &cfg.binance,
            cfg.price_tick,
            cfg.qty_step,
        )?);

        let storage = match crate::storage::StateStorage::new(None) {
            Ok(storage) => {
                debug!("CONNECTION: Storage initialized for fill history restore");
                Some(Arc::new(storage))
            }
            Err(e) => {
                warn!(error = %e, "CONNECTION: Failed to initialize storage for fill history restore");
                None
            }
        };

        Ok(Self {
            venue,
            cfg,
            event_bus,
            shutdown_flag,
            rate_limiter: Arc::new(tokio::sync::Mutex::new(RateLimiter::new())),
            shared_state,
            order_fill_history: Arc::new(DashMap::new()),
            storage,
        })
    }

    /// Create Connection with existing venue (for testing/internal use)
    #[allow(dead_code)]
    pub fn new(
        venue: Arc<BinanceFutures>,
        cfg: Arc<AppCfg>,
        event_bus: Arc<EventBus>,
        shutdown_flag: Arc<AtomicBool>,
    ) -> Self {
        Self {
            venue,
            cfg,
            event_bus,
            shutdown_flag,
            rate_limiter: Arc::new(tokio::sync::Mutex::new(RateLimiter::new())),
            shared_state: None,
            order_fill_history: Arc::new(DashMap::new()),
            storage: None,
        }
    }

    /// Get storage instance (if available)
    pub fn storage(&self) -> Option<Arc<crate::storage::StateStorage>> {
        self.storage.clone()
    }

    pub async fn start(&self, symbols: Vec<String>) -> Result<()> {
        // State restore is handled by STORAGE module via event bus
        // For now, we restore fill history directly here (Connection manages its own cache)
        if let Some(storage) = &self.storage {
            match storage.restore_order_fill_history().await {
                Ok(history_entries) => {
                    for (order_id, history) in history_entries {
                        let conn_history = OrderFillHistory {
                            total_filled_qty: history.total_filled_qty,
                            weighted_price_sum: history.weighted_price_sum,
                            last_update: history.last_update,
                            maker_fill_count: history.maker_fill_count,
                            total_fill_count: history.total_fill_count,
                        };
                        self.order_fill_history.insert(order_id, conn_history);
                    }
                    info!(
                        restored_count = self.order_fill_history.len(),
                        "CONNECTION: Restored {} order fill history entries from persistent storage",
                        self.order_fill_history.len()
                    );
                }
                Err(e) => {
                    warn!(error = %e, "CONNECTION: Failed to restore order fill history from storage, continuing without restore");
                }
            }
        }

        // Set hedge mode according to config (must be done before any orders)
        let hedge_mode = self.cfg.binance.hedge_mode;
        if let Err(e) = self.venue.set_position_side_dual(hedge_mode).await {
            warn!(
                error = %e,
                hedge_mode,
                "CONNECTION: Failed to set hedge mode, continuing anyway (may cause issues)"
            );
        } else {
            info!(
                hedge_mode,
                "CONNECTION: Hedge mode set successfully"
            );
        }

        let leverage = self.cfg.leverage.unwrap_or(self.cfg.exec.default_leverage);
        let use_isolated_margin = self.cfg.risk.use_isolated_margin;

        for symbol in &symbols {
            // Set margin type (isolated or cross)
            if let Err(e) = self.venue.set_margin_type(symbol, use_isolated_margin).await {
                error!(
                    error = %e,
                    symbol = %symbol,
                    isolated = use_isolated_margin,
                    "CONNECTION: CRITICAL - Failed to set margin type for symbol. Cannot continue with incorrect margin type."
                );
                return Err(anyhow!(
                    "Failed to set margin type for symbol {}: {}",
                    symbol,
                    e
                ));
            } else {
                info!(
                    symbol = %symbol,
                    isolated = use_isolated_margin,
                    "CONNECTION: Margin type set successfully for symbol"
                );
            }

            if let Ok(position) = self.venue.get_position(symbol).await {
                if !position.qty.0.is_zero() {
                    if position.leverage != leverage {
                        error!(
                            symbol = %symbol,
                            current_leverage = position.leverage,
                            config_leverage = leverage,
                            "CONNECTION: CRITICAL - Leverage mismatch detected but position is open! Cannot auto-fix. Please close position manually and restart bot."
                        );
                        return Err(anyhow!(
                            "Leverage mismatch for symbol {}: exchange has {}x but config requires {}x. Cannot change leverage while position is open. Please close position manually and restart bot.",
                            symbol,
                            position.leverage,
                            leverage
                        ));
                    } else {
                        info!(
                            symbol = %symbol,
                            leverage,
                            "CONNECTION: Leverage verified (matches config, position is open)"
                        );
                    }
                } else {
                    // Position is CLOSED - safe to set leverage
                    if position.leverage != leverage {
                        warn!(
                            symbol = %symbol,
                            current_leverage = position.leverage,
                            config_leverage = leverage,
                            "CONNECTION: Leverage mismatch detected (position is closed). Auto-fixing..."
                        );
                        if let Err(e) = self.venue.set_leverage(symbol, leverage).await {
                            error!(
                                error = %e,
                                symbol = %symbol,
                                current_leverage = position.leverage,
                                config_leverage = leverage,
                                "CONNECTION: Failed to auto-fix leverage mismatch for symbol"
                            );
                            return Err(anyhow!(
                                "Failed to set leverage for symbol {} (was {}x, need {}x): {}",
                                symbol,
                                position.leverage,
                                leverage,
                                e
                            ));
                        }
                        // Verify the fix worked
                        if let Ok(updated_position) = self.venue.get_position(symbol).await {
                            if updated_position.leverage == leverage {
                                info!(
                                    symbol = %symbol,
                                    old_leverage = position.leverage,
                                    new_leverage = leverage,
                                    "CONNECTION: Leverage mismatch auto-fixed successfully (position was closed)"
                                );
                            } else {
                                warn!(
                                    symbol = %symbol,
                                    expected_leverage = leverage,
                                    actual_leverage = updated_position.leverage,
                                    "CONNECTION: Leverage fix may not have taken effect immediately (will retry on next check)"
                                );
                            }
                        }
                    } else {
                        info!(
                            symbol = %symbol,
                            leverage,
                            "CONNECTION: Leverage verified (matches config, position is closed)"
                        );
                    }
                }
            } else {
                // No position exists - safe to set leverage
                // Set leverage (for new positions or to ensure it's set correctly)
                if let Err(e) = self.venue.set_leverage(symbol, leverage).await {
                    error!(
                        error = %e,
                        symbol = %symbol,
                        leverage,
                        "CONNECTION: CRITICAL - Failed to set leverage for symbol. Cannot continue with incorrect leverage."
                    );
                    return Err(anyhow!(
                        "Failed to set leverage for symbol {}: {}",
                        symbol,
                        e
                    ));
                } else {
                    info!(
                        symbol = %symbol,
                        leverage,
                        "CONNECTION: Leverage set successfully for symbol"
                    );
                }
            }

            // Start market data stream
            self.start_market_data_stream(symbols.clone()).await?;

            // Start user data stream (pass symbols for missed events sync)
            self.start_user_data_stream(symbols.clone()).await?;

            // Start periodic rules refresh task
            self.start_rules_refresh_task().await;

            // Start periodic order fill history cleanup task
            self.start_order_fill_history_cleanup_task().await;

            info!("CONNECTION: All streams started");
            Ok(())
        }

        async fn start_order_fill_history_cleanup_task(&self) {
            let order_fill_history = self.order_fill_history.clone();
            let shutdown_flag = self.shutdown_flag.clone();

            tokio::spawn(async move {
                const CLEANUP_INTERVAL_SECS: u64 = 3600; // Every hour
                const MAX_AGE_SECS: u64 = 86400; // 24 hours (very conservative, memory leak prevention only)

                loop {
                    tokio::time::sleep(Duration::from_secs(CLEANUP_INTERVAL_SECS)).await;

                    if shutdown_flag.load(AtomicOrdering::Relaxed) {
                        break;
                    }

                    let now = Instant::now();
                    let initial_count = order_fill_history.len();
                    order_fill_history.retain(|order_id, history| {
                        // First check: Is order in OPEN_ORDERS_CACHE?
                        let is_in_cache = OPEN_ORDERS_CACHE.iter().any(|entry| {
                            entry.value().iter().any(|o| o.order_id == *order_id)
                        });

                        if is_in_cache {
                            let history_age = now.duration_since(history.last_update);
                            const CACHE_STALE_THRESHOLD_SECS: u64 = 300; // 5 minutes

                            if history_age.as_secs() > CACHE_STALE_THRESHOLD_SECS {
                                if history_age.as_secs() > MAX_AGE_SECS {
                                    tracing::warn!(
                                        order_id = %order_id,
                                        history_age_secs = history_age.as_secs(),
                                        "CONNECTION: Order in cache but history is very old ({} hours), cache likely stale. Removing to prevent memory leak.",
                                        history_age.as_secs() / 3600
                                    );
                                    false // Remove stale entry
                                } else {
                                    tracing::debug!(
                                        order_id = %order_id,
                                        history_age_secs = history_age.as_secs(),
                                        "CONNECTION: Order in cache but history is old ({} minutes), cache may be stale. Keeping for now.",
                                        history_age.as_secs() / 60
                                    );
                                    true // Keep for now (might be active but no recent updates)
                                }
                            } else {
                                true
                            }
                        } else {
                            let age = now.duration_since(history.last_update);
                            if age.as_secs() > MAX_AGE_SECS {
                                // Very old entry (24+ hours) - safe to remove (memory leak prevention)
                                // Normal cleanup should have removed this already when order closed,
                                // but this catches edge cases where cleanup didn't happen
                                false
                            } else {
                                // Recent entry - keep (may receive late-arriving fill events)
                                true
                            }
                        }
                    });

                    let final_count = order_fill_history.len();
                    let removed_count = initial_count.saturating_sub(final_count);

                    // Count protected orders (active orders that were kept)
                    let protected_count = order_fill_history.iter()
                        .filter(|entry| {
                            OPEN_ORDERS_CACHE.iter().any(|cache_entry| {
                                cache_entry.value().iter().any(|o| o.order_id == *entry.key())
                            })
                        })
                        .count();

                    if removed_count > 0 {
                        info!(
                            removed_count,
                            protected_count,
                            remaining_entries = order_fill_history.len(),
                            "CONNECTION: Memory leak prevention - cleaned up {} very old order fill history entries (older than {} hours), protected {} active orders",
                            removed_count,
                            MAX_AGE_SECS / 3600,
                            protected_count
                        );
                    } else if protected_count > 0 {
                        tracing::debug!(
                            protected_count,
                            total_entries = order_fill_history.len(),
                            "CONNECTION: Order fill history cleanup completed (protected {} active orders, no very old entries to remove)",
                            protected_count
                        );
                    } else {
                        tracing::debug!(
                            total_entries = order_fill_history.len(),
                            "CONNECTION: Order fill history cleanup completed (no very old entries to remove)"
                        );
                    }
                }
            });
        }

        /// Start market data WebSocket stream
        /// Publishes MarketTick events to event bus
        async fn start_market_data_stream(&self, symbols: Vec<String>) -> Result<()> {
            let original_count = symbols.len();
            let unique_symbols: Vec<String> = symbols
                .into_iter()
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect();
            let deduplicated_count = unique_symbols.len();

            if original_count != deduplicated_count {
                warn!(
                    original_count,
                    deduplicated_count,
                    "CONNECTION: Removed {} duplicate symbols from market data stream subscription",
                    original_count - deduplicated_count
                );
            }

            // Binance URL limit: max 200 chars, so we need to split symbols into groups
            const MAX_SYMBOLS_PER_STREAM: usize = 10;

            info!(
                total_symbols = unique_symbols.len(),
                streams_needed = (unique_symbols.len() + MAX_SYMBOLS_PER_STREAM - 1) / MAX_SYMBOLS_PER_STREAM,
                "CONNECTION: setting up market data websocket streams"
            );

            // Split symbols into groups
            for chunk in unique_symbols.chunks(MAX_SYMBOLS_PER_STREAM) {
                let symbols_chunk = chunk.to_vec();
                let event_bus = self.event_bus.clone();
                let shutdown_flag = self.shutdown_flag.clone();

                tokio::spawn(async move {
                    // Exponential backoff configuration
                    const INITIAL_DELAY_SECS: u64 = 1;
                    const MAX_DELAY_SECS: u64 = 60;
                    let mut connection_retry_delay = INITIAL_DELAY_SECS;
                    let mut stream_retry_delay = INITIAL_DELAY_SECS;

                    loop {
                        if shutdown_flag.load(AtomicOrdering::Relaxed) {
                            break;
                        }

                        match MarketDataStream::connect(&symbols_chunk).await {
                            Ok(mut stream) => {
                                // Connection successful - reset retry delays
                                connection_retry_delay = INITIAL_DELAY_SECS;
                                stream_retry_delay = INITIAL_DELAY_SECS;

                                info!(
                                    symbol_count = symbols_chunk.len(),
                                    symbols = ?symbols_chunk.iter().take(5).collect::<Vec<_>>(),
                                    "CONNECTION: market data websocket connected"
                                );

                                loop {
                                    if shutdown_flag.load(AtomicOrdering::Relaxed) {
                                        break;
                                    }

                                    match stream.next_price_update().await {
                                        Ok(price_update) => {
                                            // Clone symbol once for reuse
                                            let symbol = price_update.symbol.clone();

                                            // Update price cache (for backward compatibility)
                                        // DashMap is thread-safe and atomic
                                        // If multiple streams subscribe to the same symbol, the last write wins
                                            // Move price_update into cache (no unnecessary clone)
                                            PRICE_CACHE.insert(symbol.clone(), price_update.clone());

                                            // Publish MarketTick event
                                            let market_tick = MarketTick {
                                                symbol,
                                                bid: price_update.bid,
                                                ask: price_update.ask,
                                                mark_price: None, // Can be fetched if needed
                                                volume: None, // Can be added if needed
                                                timestamp: Instant::now(),
                                            };

                                        // Handle backpressure - if channel is full, log warning but continue
                                            let receiver_count = event_bus.market_tick_tx.receiver_count();
                                            if receiver_count == 0 {
                                                // No subscribers, skip sending (but still update cache)
                                                tracing::debug!(
                                                    symbol = %price_update.symbol,
                                                    "CONNECTION: No MarketTick subscribers, skipping event"
                                                );
                                            } else {
                                                if let Err(_) = event_bus.market_tick_tx.send(market_tick) {
                                                    tracing::debug!(
                                                        symbol = %price_update.symbol,
                                                        "CONNECTION: MarketTick send failed (no receivers)"
                                                    );
                                                }
                                            }

                                            // Reset stream retry delay on successful message
                                            stream_retry_delay = INITIAL_DELAY_SECS;
                                        }
                                        Err(e) => {
                                            warn!(
                                                error = %e,
                                                symbol_count = symbols_chunk.len(),
                                                symbols = ?symbols_chunk.iter().take(3).collect::<Vec<_>>(),
                                                retry_delay_secs = stream_retry_delay,
                                                "CONNECTION: market data websocket error for chunk, reconnecting with exponential backoff"
                                            );
                                            tokio::time::sleep(Duration::from_secs(stream_retry_delay)).await;

                                            // Exponential backoff: double delay, cap at MAX_DELAY_SECS
                                            stream_retry_delay = (stream_retry_delay * 2).min(MAX_DELAY_SECS);

                                            break; // Reconnect (only this chunk, others continue)
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                error!(
                                    error = %e,
                                    symbol_count = symbols_chunk.len(),
                                    symbols = ?symbols_chunk.iter().take(3).collect::<Vec<_>>(),
                                    retry_delay_secs = connection_retry_delay,
                                    "CONNECTION: failed to connect market data websocket for chunk, retrying with exponential backoff"
                                );
                                tokio::time::sleep(Duration::from_secs(connection_retry_delay)).await;

                                // Exponential backoff: double delay, cap at MAX_DELAY_SECS
                                connection_retry_delay = (connection_retry_delay * 2).min(MAX_DELAY_SECS);
                            }
                        }
                    }
                });
            }

        let total_symbols = unique_symbols.len();
            info!(
        total_symbols,
        chunks_spawned = (total_symbols + MAX_SYMBOLS_PER_STREAM - 1) / MAX_SYMBOLS_PER_STREAM,
                "CONNECTION: All market data stream tasks spawned (each chunk runs independently)"
            );

            Ok(())
        }

        async fn start_user_data_stream(&self, symbols: Vec<String>) -> Result<()> {
            let client = reqwest::Client::builder().build()?;
            let api_key = self.cfg.binance.api_key.clone();
            let futures_base = self.cfg.binance.futures_base.clone();
            let reconnect_delay = Duration::from_millis(self.cfg.websocket.reconnect_delay_ms);
            let kind = UserStreamKind::Futures;
            let event_bus = self.event_bus.clone();
            let shutdown_flag = self.shutdown_flag.clone();
            let venue = self.venue.clone();
            let order_fill_history = self.order_fill_history.clone();

            info!(
                reconnect_delay_ms = self.cfg.websocket.reconnect_delay_ms,
                ping_interval_ms = self.cfg.websocket.ping_interval_ms,
                ?kind,
                "CONNECTION: launching user data stream task"
            );

            tokio::spawn(async move {
                let base = futures_base;
                loop {
                    if shutdown_flag.load(AtomicOrdering::Relaxed) {
                        break;
                    }

                    match UserDataStream::connect(client.clone(), &base, &api_key, kind).await {
                        Ok(mut stream) => {
                            info!(?kind, "CONNECTION: connected to Binance user data stream");

                            // After reconnect, verify with REST API
                            // Binance WebSocket automatically sends ACCOUNT_UPDATE and ORDER_TRADE_UPDATE
                            // Extra validation can catch missed events
                            let venue_for_validation = venue.clone();
                            let symbols_for_validation = symbols.clone();
                            stream.set_on_reconnect(move || {
                                info!("CONNECTION: WebSocket reconnected - Binance will automatically send state updates via ACCOUNT_UPDATE and ORDER_TRADE_UPDATE events");

                                // Spawn background validation task
                                let venue_clone = venue_for_validation.clone();
                                let symbols_clone = symbols_for_validation.clone();
                                tokio::spawn(async move {
                                    // Wait a bit for ACCOUNT_UPDATE events to arrive (Binance sends them after reconnect)
                                    tokio::time::sleep(Duration::from_secs(2)).await;

                                    info!("CONNECTION: Validating state after WebSocket reconnect (comparing REST API with WebSocket cache)");

                                    // Validate positions for each symbol
                                    for symbol in &symbols_clone {
                                        // Fetch position from REST API
                                        match venue_clone.get_position(symbol).await {
                                            Ok(rest_position) => {
                                                // Compare with WebSocket cache
                                                if let Some(ws_position) = POSITION_CACHE.get(symbol) {
                                                    let ws_pos = ws_position.value();

                                                    // Check if quantities match (with small tolerance for floating point)
                                                    let qty_diff = (rest_position.qty.0 - ws_pos.qty.0).abs();
                                                    let entry_diff = (rest_position.entry.0 - ws_pos.entry.0).abs();

                                                    // REST API is source of truth after reconnect
                                                    // If there's a mismatch, update WebSocket cache with REST API data
                                                    // REST API is considered correct, update WebSocket cache
                                                    let needs_update = qty_diff > Decimal::from_str("0.0001").unwrap()
                                                        || entry_diff > Decimal::from_str("0.01").unwrap();

                                                    if needs_update {
                                                        warn!(
                                                            symbol = %symbol,
                                                            rest_qty = %rest_position.qty.0,
                                                            ws_qty = %ws_pos.qty.0,
                                                            qty_diff = %qty_diff,
                                                            rest_entry = %rest_position.entry.0,
                                                            ws_entry = %ws_pos.entry.0,
                                                            entry_diff = %entry_diff,
                                                            "CONNECTION: Position mismatch after reconnect (REST vs WebSocket), updating cache with REST API data"
                                                        );

                                                        // REST API is considered correct, update WebSocket cache
                                                        // REST API validation not just logging, fixing state
                                                        // Update WebSocket cache with REST API data (REST API is source of truth)
                                                        POSITION_CACHE.insert(symbol.to_string(), rest_position.clone());

                                                        info!(
                                                            symbol = %symbol,
                                                            "CONNECTION: Position cache updated with REST API data"
                                                        );
                                                    } else if qty_diff > Decimal::from_str("0.000001").unwrap() || entry_diff > Decimal::from_str("0.001").unwrap() {
                                                        // Small differences - still update cache but with less logging
                                                        // REST API is source of truth, always update cache
                                                        POSITION_CACHE.insert(symbol.to_string(), rest_position.clone());
                                                        debug!(
                                                            symbol = %symbol,
                                                            qty_diff = %qty_diff,
                                                            entry_diff = %entry_diff,
                                                            "CONNECTION: Small position difference detected, cache updated with REST API data"
                                                        );
                                                    }
                                                } else if !rest_position.qty.0.is_zero() {
                                                    // REST API shows position but WebSocket cache doesn't
                                                    // Add missing position to cache
                                                    warn!(
                                                        symbol = %symbol,
                                                        rest_qty = %rest_position.qty.0,
                                                        "CONNECTION: Position exists in REST API but missing from WebSocket cache after reconnect, adding to cache"
                                                    );

                                                    // Add missing position to cache
                                                    POSITION_CACHE.insert(symbol.to_string(), rest_position.clone());

                                                    info!(
                                                        symbol = %symbol,
                                                        "CONNECTION: Missing position added to cache from REST API"
                                                    );
                                                } else {
                                                    // REST API shows no position - remove from cache if exists
                                                    if POSITION_CACHE.contains_key(symbol) {
                                                        warn!(
                                                            symbol = %symbol,
                                                            "CONNECTION: Position removed from REST API but exists in WebSocket cache, removing from cache"
                                                        );
                                                        POSITION_CACHE.remove(symbol);

                                                        info!(
                                                            symbol = %symbol,
                                                            "CONNECTION: Stale position removed from cache"
                                                        );
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                // Failed to fetch position - log but don't fail
                                                debug!(
                                                    symbol = %symbol,
                                                    error = %e,
                                                    "CONNECTION: Failed to fetch position for validation after reconnect"
                                                );
                                            }
                                        }

                                        // Validate open orders
                                        match venue_clone.get_open_orders(symbol).await {
                                            Ok(rest_orders) => {
                                                // Compare with WebSocket cache
                                                if let Some(ws_orders) = OPEN_ORDERS_CACHE.get(symbol) {
                                                    let ws_orders_vec = ws_orders.value();

                                                    // Check if order counts match
                                                    if rest_orders.len() != ws_orders_vec.len() {
                                                        warn!(
                                                            symbol = %symbol,
                                                            rest_order_count = rest_orders.len(),
                                                            ws_order_count = ws_orders_vec.len(),
                                                            "CONNECTION: Open orders count mismatch after reconnect (REST vs WebSocket)"
                                                        );
                                                    }

                                                    // Check for missing orders in WebSocket cache
                                                    for rest_order in &rest_orders {
                                                        if !ws_orders_vec.iter().any(|o| o.order_id == rest_order.order_id) {
                                                            warn!(
                                                                symbol = %symbol,
                                                                order_id = %rest_order.order_id,
                                                                "CONNECTION: Order exists in REST API but missing from WebSocket cache after reconnect"
                                                            );
                                                        }
                                                    }
                                                } else if !rest_orders.is_empty() {
                                                    // REST API shows orders but WebSocket cache doesn't
                                                    warn!(
                                                        symbol = %symbol,
                                                        rest_order_count = rest_orders.len(),
                                                        "CONNECTION: Open orders exist in REST API but missing from WebSocket cache after reconnect"
                                                    );
                                                }
                                            }
                                            Err(e) => {
                                                // Failed to fetch orders - log but don't fail
                                                debug!(
                                                    symbol = %symbol,
                                                    error = %e,
                                                    "CONNECTION: Failed to fetch open orders for validation after reconnect"
                                                );
                                            }
                                        }
                                    }

                                    // Validate balance (USDT/USDC)
                                    for asset in &["USDT", "USDC"] {
                                        match venue_clone.available_balance(asset).await {
                                            Ok(rest_balance) => {
                                                // Compare with WebSocket cache
                                                if let Some(ws_balance) = BALANCE_CACHE.get(*asset) {
                                                    let balance_diff = (rest_balance - *ws_balance.value()).abs();

                                                    if balance_diff > Decimal::from_str("0.01").unwrap() {
                                                        warn!(
                                                            asset = %asset,
                                                            rest_balance = %rest_balance,
                                                            ws_balance = %ws_balance.value(),
                                                            balance_diff = %balance_diff,
                                                            "CONNECTION: Balance mismatch after reconnect (REST vs WebSocket)"
                                                        );
                                                    }
                                                } else if !rest_balance.is_zero() {
                                                    // REST API shows balance but WebSocket cache doesn't
                                                    warn!(
                                                        asset = %asset,
                                                        rest_balance = %rest_balance,
                                                        "CONNECTION: Balance exists in REST API but missing from WebSocket cache after reconnect"
                                                    );
                                                }
                                            }
                                            Err(e) => {
                                                // Failed to fetch balance - log but don't fail
                                                debug!(
                                                    asset = %asset,
                                                    error = %e,
                                                    "CONNECTION: Failed to fetch balance for validation after reconnect"
                                                );
                                            }
                                        }
                                    }

                                    info!("CONNECTION: State validation after reconnect completed");
                                });
                            });

                            let mut first_event_after_reconnect = true;
                            loop {
                                if shutdown_flag.load(AtomicOrdering::Relaxed) {
                                    break;
                                }

                                match stream.next_event().await {
                                    Ok(event) => {
                                        if first_event_after_reconnect {
                                            first_event_after_reconnect = false;
                                            // Can send heartbeat if needed
                                        }

                                        // Convert UserEvent to OrderUpdate/PositionUpdate/BalanceUpdate
                                        match event {
                                            UserEvent::OrderFill {
                                                symbol,
                                                order_id,
                                                side,
                                                qty: last_filled_qty, // Last executed qty (incremental, this fill only)
                                                cumulative_filled_qty,
                                                order_qty,
                                                price: last_fill_price, // Last executed price (this fill only)
                                                is_maker,
                                                order_status,
                                                commission: _,
                                            } => {
                                                let status = match order_status.as_str() {
                                                    "NEW" => OrderStatus::New,
                                                    "PARTIALLY_FILLED" => OrderStatus::PartiallyFilled,
                                                    "FILLED" => OrderStatus::Filled,
                                                    "CANCELED" => OrderStatus::Canceled,
                                                    "EXPIRED" => OrderStatus::Expired,
                                                    "EXPIRED_IN_MATCH" => OrderStatus::ExpiredInMatch,
                                                    "REJECTED" => OrderStatus::Rejected,
                                                    unknown => {
                                                        // Unknown status - log warning and treat as rejected
                                                        tracing::warn!(
                                                            order_status = unknown,
                                                            "CONNECTION: Unknown order status received, treating as Rejected"
                                                        );
                                                        OrderStatus::Rejected
                                                    }
                                                };

                                                let total_order_qty = order_qty.unwrap_or(cumulative_filled_qty);
                                                let remaining_qty = if total_order_qty.0 >= cumulative_filled_qty.0 {
                                                    Qty(total_order_qty.0 - cumulative_filled_qty.0)
                                                } else {
                                                    Qty(Decimal::ZERO) // Should not happen, but safety check
                                                };

                                                let now = Instant::now();
                                                let (average_fill_price, is_all_maker) = {
                                                    let mut history = order_fill_history.entry(order_id.clone()).or_insert_with(|| {
                                                        OrderFillHistory {
                                                            total_filled_qty: Qty(Decimal::ZERO),
                                                            weighted_price_sum: Decimal::ZERO,
                                                            last_update: now,
                                                            maker_fill_count: 0,
                                                            total_fill_count: 0,
                                                        }
                                                    });

                                                    // Add this fill to weighted sum: price * qty
                                                    history.weighted_price_sum += last_fill_price.0 * last_filled_qty.0;
                                                    history.total_filled_qty = cumulative_filled_qty;
                                                    history.last_update = now; // Update timestamp

                                                    // Track maker/taker status for commission calculation
                                                    history.total_fill_count += 1;
                                                    if is_maker {
                                                        history.maker_fill_count += 1;
                                                    }

                                                    // Calculate average: weighted_sum / total_qty
                                                    let avg_price = if !cumulative_filled_qty.0.is_zero() {
                                                        Px(history.weighted_price_sum / cumulative_filled_qty.0)
                                                    } else {
                                                        last_fill_price // Fallback if qty is zero (should not happen)
                                                    };

                                                    // If all fills are maker, use maker commission; otherwise use taker commission
                                                    let all_maker = history.maker_fill_count == history.total_fill_count;

                                                    // Publish OrderFillHistoryUpdate event for STORAGE module
                                                    use crate::event_bus::{FillHistoryAction, FillHistoryData, OrderFillHistoryUpdate};
                                                    let fill_history_update = OrderFillHistoryUpdate {
                                                        order_id: order_id.clone(),
                                                        symbol: symbol.clone(),
                                                        action: FillHistoryAction::Save,
                                                        data: Some(FillHistoryData {
                                                            total_filled_qty: history.total_filled_qty.0.to_string(),
                                                            weighted_price_sum: history.weighted_price_sum.to_string(),
                                                            maker_fill_count: history.maker_fill_count,
                                                            total_fill_count: history.total_fill_count,
                                                        }),
                                                        timestamp: Instant::now(),
                                                    };
                                        if let Err(e) = event_bus.order_fill_history_update_tx.send(fill_history_update) {
                                                        warn!(error = ?e, order_id = %order_id, "CONNECTION: Failed to publish OrderFillHistoryUpdate event (no subscribers)");
                                                    }

                                                    (avg_price, all_maker)
                                                };

                                                // Update open orders cache from WebSocket
                                                // Update OPEN_ORDERS_CACHE based on order status
                                    let mut order_entry = OPEN_ORDERS_CACHE.entry(symbol.clone()).or_insert_with(Vec::new);

                                                // Find existing order in cache
                                                let existing_order_idx = order_entry.iter().position(|o| o.order_id == order_id);

                                                match status {
                                                    OrderStatus::Filled | OrderStatus::Canceled | OrderStatus::Expired | OrderStatus::ExpiredInMatch | OrderStatus::Rejected => {
                                                        // Order is closed - remove from cache
                                                        if let Some(idx) = existing_order_idx {
                                                            order_entry.remove(idx);
                                                        }
                                                        // If no more open orders for this symbol, remove symbol entry
                                                        if order_entry.is_empty() {
                                                            OPEN_ORDERS_CACHE.remove(&symbol);
                                                        }
                                                    }
                                                    OrderStatus::New | OrderStatus::PartiallyFilled => {
                                                        // Order is still open - update or add to cache
                                                        let venue_order = VenueOrder {
                                                            order_id: order_id.clone(),
                                                            side,
                                                            price: last_fill_price, // Use last fill price (or could use order price if available)
                                                            qty: total_order_qty,
                                                        };
                                                        if let Some(idx) = existing_order_idx {
                                                            order_entry[idx] = venue_order;
                                                        } else {
                                                            order_entry.push(venue_order);
                                                        }
                                                    }
                                                }

                                                // Clean up fill history when order is fully filled, canceled, or expired
                                                if matches!(
                                                    status,
                                                    OrderStatus::Filled
                                                        | OrderStatus::Canceled
                                                        | OrderStatus::Expired
                                                        | OrderStatus::ExpiredInMatch
                                                ) {
                                                    order_fill_history.remove(&order_id);

                                                    // Publish OrderFillHistoryUpdate event (Remove) for STORAGE module
                                                    use crate::event_bus::{FillHistoryAction, OrderFillHistoryUpdate};
                                                    let fill_history_update = OrderFillHistoryUpdate {
                                                        order_id: order_id.clone(),
                                                        symbol: symbol.clone(),
                                                        action: FillHistoryAction::Remove,
                                                        data: None,
                                                        timestamp: Instant::now(),
                                                    };
                                        if let Err(e) = event_bus.order_fill_history_update_tx.send(fill_history_update) {
                                                        warn!(error = ?e, order_id = %order_id, "CONNECTION: Failed to publish OrderFillHistoryUpdate event (Remove) (no subscribers)");
                                                    }
                                                }

                                                let order_update = OrderUpdate {
                                                    symbol: symbol.clone(),
                                                    order_id,
                                                    side,
                                                    last_fill_price, // Last executed price (this fill only)
                                                    average_fill_price, // Weighted average of all fills
                                                    qty: total_order_qty, // Total order quantity (original order size)
                                                    filled_qty: cumulative_filled_qty, // Cumulative filled qty (total filled so far)
                                                    remaining_qty, // Remaining qty = order_qty - cumulative_filled_qty
                                                    status,
                                                    /// True if all fills were maker orders, false if any fill was taker
                                                    /// Used for commission calculation: if all maker, use maker commission; otherwise taker
                                                    is_maker: Some(is_all_maker),
                                                    timestamp: Instant::now(),
                                                };

                                                if let Err(e) = event_bus.order_update_tx.send(order_update) {
                                                    error!(
                                                        error = ?e,
                                                        "CONNECTION: Failed to send OrderUpdate event (no subscribers or channel closed)"
                                                    );
                                                }

                                                let venue_clone = venue.clone();
                                                let event_bus_pos = event_bus.clone();
                                                let symbol_clone = symbol.clone();
                                                tokio::spawn(async move {
                                                    const MAX_RETRIES: u32 = 5;
                                                    const BASE_DELAY_MS: u64 = 200;

                                                    for attempt in 0..MAX_RETRIES {
                                                        // Exponential backoff: 200ms, 400ms, 600ms, 800ms, 1000ms
                                                        let delay_ms = BASE_DELAY_MS * (attempt + 1) as u64;
                                                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;

                                                        if let Ok(position) = venue_clone.get_position(&symbol_clone).await {
                                                            // Position successfully fetched, send update event
                                                            let position_update = PositionUpdate {
                                                                symbol: symbol_clone,
                                                                qty: position.qty,
                                                                entry_price: position.entry,
                                                                leverage: position.leverage,
                                                                unrealized_pnl: None, // Can be calculated from mark price
                                                                is_open: !position.qty.0.is_zero(),
                                                                timestamp: Instant::now(),
                                                            };
                                                            if let Err(e) = event_bus_pos.position_update_tx.send(position_update) {
                                                                error!(
                                                                    error = ?e,
                                                                    "CONNECTION: Failed to send PositionUpdate event (no subscribers or channel closed)"
                                                                );
                                                            }
                                                            // Successfully fetched and sent, break retry loop
                                                            break;
                                                        }

                                                        // If this was the last attempt, log warning
                                                        if attempt == MAX_RETRIES - 1 {
                                                            warn!(
                                                                symbol = %symbol_clone,
                                                                attempts = MAX_RETRIES,
                                                                "CONNECTION: Failed to fetch position after order fill (max retries reached)"
                                                            );
                                                        }
                                                    }
                                                });
                                            }
                                            UserEvent::OrderCanceled {
                                                symbol,
                                                order_id,
                                                client_order_id: _,
                                            } => {
                                                // Update open orders cache when order is canceled
                                                // Remove canceled order from OPEN_ORDERS_CACHE
                                                if let Some(mut orders) = OPEN_ORDERS_CACHE.get_mut(&symbol) {
                                                    orders.retain(|o| o.order_id != order_id);
                                                    // If no more open orders for this symbol, remove symbol entry
                                                    if orders.is_empty() {
                                                        drop(orders); // Release lock before remove
                                                        OPEN_ORDERS_CACHE.remove(&symbol);
                                                    }
                                                }
                                            }
                                            UserEvent::AccountUpdate { positions, balances } => {
                                                for pos in &positions {
                                                    // Convert AccountPosition to Position for cache
                                                    let position = Position {
                                                        symbol: pos.symbol.clone(),
                                                        qty: Qty(pos.position_amt),
                                                        entry: Px(pos.entry_price),
                                                        leverage: pos.leverage,
                                                        liq_px: None, // ACCOUNT_UPDATE doesn't include liquidation price
                                                    };
                                                    POSITION_CACHE.insert(pos.symbol.clone(), position);
                                                }

                                                // Cache balances from WebSocket
                                                // Update BALANCE_CACHE from ACCOUNT_UPDATE event
                                                for bal in &balances {
                                                    BALANCE_CACHE.insert(bal.asset.clone(), bal.available_balance);
                                                }

                                                // Position updates (publish events)
                                                for pos in positions {
                                                    let position_update = PositionUpdate {
                                                        symbol: pos.symbol,
                                                        qty: Qty(pos.position_amt),
                                                        entry_price: Px(pos.entry_price),
                                                        leverage: pos.leverage,
                                                        unrealized_pnl: pos.unrealized_pnl,
                                                        is_open: !pos.position_amt.is_zero(),
                                                        timestamp: Instant::now(),
                                                    };
                                                    if let Err(e) = event_bus.position_update_tx.send(position_update) {
                                                        error!(
                                                            error = ?e,
                                                            "CONNECTION: Failed to send PositionUpdate event (no subscribers or channel closed)"
                                                        );
                                                    }
                                                }

                                                // Balance updates (USDT/USDC)
                                                let mut usdt_balance = Decimal::ZERO;
                                                let mut usdc_balance = Decimal::ZERO;
                                                for bal in balances {
                                                    if bal.asset == "USDT" {
                                                        usdt_balance = bal.available_balance;
                                                    } else if bal.asset == "USDC" {
                                                        usdc_balance = bal.available_balance;
                                                    }
                                                }

                                                if !usdt_balance.is_zero() || !usdc_balance.is_zero() {
                                                    let balance_update = BalanceUpdate {
                                                        usdt: usdt_balance,
                                                        usdc: usdc_balance,
                                                        timestamp: Instant::now(),
                                                    };
                                                    if let Err(e) = event_bus.balance_update_tx.send(balance_update) {
                                                        warn!(
                                                            error = ?e,
                                                            "CONNECTION: Failed to send BalanceUpdate event (no subscribers or channel closed)"
                                                        );
                                                    }
                                                }
                                            }
                                            UserEvent::Heartbeat => {
                                                // Heartbeat can trigger sync if needed
                                                info!("CONNECTION: user data stream heartbeat");
                                            }
                                        }
                                    }
                                    Err(_) => {
                                        break;
                                    }
                                }
                            }
                            warn!("CONNECTION: user data stream reader exited, will reconnect");
                        }
                        Err(err) => {
                            warn!(?err, "CONNECTION: failed to connect user data stream");
                        }
                    }
                    tokio::time::sleep(reconnect_delay).await;
                }
            });

            Ok(())
        }

        /// Send order command (used by ORDERING module)
        /// Returns order ID on success
        /// Performs validation before sending: rules, min_notional, balance
        pub async fn send_order(&self, command: OrderCommand) -> Result<String> {
            // Rate limit check
            {
                let mut limiter = self.rate_limiter.lock().await;
                limiter.check_order_rate().await;
            }

            use std::time::{SystemTime, UNIX_EPOCH};
            let client_order_id = format!(
                "{}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_else(|_| {
                        // System time is before UNIX epoch (should never happen, but handle gracefully)
                        // Use current timestamp as fallback (0 would cause issues)
                        warn!("System time is before UNIX epoch, using fallback timestamp");
                        Duration::from_secs(0)
                    })
                    .as_millis()
            );

            match command {
                OrderCommand::Open { symbol, side, price, qty, tif } => {
                    // Pre-order validation (exchange-level: rules, min_notional, balance)
                    // Position/order state validation is handled by ORDERING module
                    self.validate_order_before_send(&symbol, price, qty, side, true).await?;

                    let (order_id, _) = self.venue.place_limit_with_client_id(
                        &symbol,
                        side,
                        price,
                        qty,
                        tif,
                        &client_order_id,
                    ).await?;
                    Ok(order_id)
                }
                OrderCommand::Close { symbol, side, price, qty, tif } => {
                    // Pre-order validation (exchange-level: rules, min_notional, balance)
                    // Position/order state validation is handled by ORDERING module
                    self.validate_order_before_send(&symbol, price, qty, side, false).await?;

                    let (order_id, _) = self.venue.place_limit_with_client_id(
                        &symbol,
                        side,
                        price,
                        qty,
                        tif,
                        &client_order_id,
                    ).await?;
                    Ok(order_id)
                }
            }
        }

        /// Validate order before sending
        /// Checks: symbol rules, min_notional, balance (if available)
        /// NOTE: Position/order state validation is handled by ORDERING module (separation of concerns)
        /// CONNECTION only handles exchange-level validation, not business logic validation
        async fn validate_order_before_send(
            &self,
            symbol: &str,
            price: Px,
            qty: Qty,
            side: Side,
            is_open_order: bool, // true for Open orders, false for Close orders
        ) -> Result<()> {
            // 1. Fetch and validate symbol rules (early check)
            let rules = self.venue.rules_for(symbol).await
                .map_err(|e| anyhow!("Failed to fetch symbol rules for {}: {}", symbol, e))?;

            // 2. Validate and format order params (includes min_notional check)
            let (_, _, price_quantized, qty_quantized) = BinanceFutures::validate_and_format_order_params(
                price,
                qty,
                &rules,
                symbol,
            )?;

            // 3. Calculate notional value
            let notional = price_quantized * qty_quantized;

            // 4. Check min_notional (early validation, before API call)
            if !rules.min_notional.is_zero() && notional < rules.min_notional {
                return Err(anyhow!(
                    "Order notional below minimum: {} < {} for symbol {}",
                    notional,
                    rules.min_notional,
                    symbol
                ));
            }

            // 5. Check balance (if shared_state is available)
            // CRITICAL: Use available() method which accounts for reserved balance
            if let Some(shared_state) = &self.shared_state {
                // Check available balance (total - reserved) for USDT or USDC
                let balance_store = shared_state.balance_store.read().await;
                let available_balance = balance_store.available(&self.cfg.quote_asset);

                if is_open_order {
                    // For Open orders: check margin requirement
                    // For futures, we need margin = notional / leverage
                    // Get leverage from config (use cfg.leverage if set, otherwise use cfg.exec.default_leverage)
                    let leverage = self.cfg.leverage.unwrap_or(self.cfg.exec.default_leverage) as u32;
                    let required_margin = notional / Decimal::from(leverage);

                    if available_balance < required_margin {
                        return Err(anyhow!(
                            "Insufficient balance: required margin {} (notional {} / leverage {}x) > available balance {} for {}",
                            required_margin,
                            notional,
                            leverage,
                            available_balance,
                            self.cfg.quote_asset
                        ));
                    }

                    info!(
                        symbol = %symbol,
                        side = ?side,
                        notional = %notional,
                        required_margin = %required_margin,
                        available_balance = %available_balance,
                        leverage = leverage,
                        "Order validation passed: balance sufficient for margin"
                    );
                } else {
                    // For Close orders: check minimum balance for commission
                    // Close orders don't require margin (position already open), but commission is needed
                    // Binance futures commission is typically 0.02-0.04% of notional
                    // Use 0.1% as safety margin to account for commission and potential price movement
                    let commission_rate = Decimal::new(1, 3); // 0.001 = 0.1%
                    let commission_estimate = notional * commission_rate;
                    // Also check against minimum quote balance from config
                    let min_balance = Decimal::try_from(self.cfg.min_quote_balance_usd).unwrap_or(Decimal::ZERO);
                    let required_balance = commission_estimate.max(min_balance);

                    if available_balance < required_balance {
                        return Err(anyhow!(
                            "Insufficient balance for close order: required {} (commission estimate {} or min balance {}) > available balance {} for {}",
                            required_balance,
                            commission_estimate,
                            min_balance,
                            available_balance,
                            self.cfg.quote_asset
                        ));
                    }

                    info!(
                        symbol = %symbol,
                        side = ?side,
                        notional = %notional,
                        commission_estimate = %commission_estimate,
                        min_balance = %min_balance,
                        available_balance = %available_balance,
                        "Order validation passed: balance sufficient for commission"
                    );
                }
            } else {
                // Shared state not available, skip balance check (log warning)
                warn!(
                    symbol = %symbol,
                    "Order validation: shared_state not available, skipping balance and position checks"
                );
            }

            Ok(())
        }

        /// Fetch balance (used by BALANCE module)
        /// NOTE: Balance should come from WebSocket stream (AccountUpdate event)
        /// This is only used as fallback on startup
        pub async fn fetch_balance(&self, asset: &str) -> Result<Decimal> {
            // Rate limit check
            {
                let mut limiter = self.rate_limiter.lock().await;
                limiter.check_balance_rate().await;
            }

            self.venue.available_balance(asset).await
        }

        /// Get current market prices (bid, ask) for a symbol
        /// WebSocket-first approach: tries PRICE_CACHE (from WebSocket), falls back to REST API only if cache is empty
        /// REST API fallback should be rare - only on startup before WebSocket data arrives
        pub async fn get_current_prices(&self, symbol: &str) -> Result<(Px, Px)> {
            // Try WebSocket cache first (faster, more efficient, reduces REST API rate limit usage)
            if let Some(price_update) = PRICE_CACHE.get(symbol) {
                return Ok((price_update.bid, price_update.ask));
            }

            // Fallback to REST API only if cache is empty (e.g., on startup before WebSocket data arrives)
            warn!(
                symbol = %symbol,
                "PRICE_CACHE empty, falling back to REST API (should be rare - WebSocket should populate cache)"
            );
            self.venue.best_prices(symbol).await
        }

        /// Get cached market prices (bid, ask) for a symbol synchronously
        /// Returns None if price is not in cache (no async fallback)
        /// Use this when you need prices atomically without async operations
        pub fn get_cached_prices(symbol: &str) -> Option<(Px, Px)> {
            PRICE_CACHE.get(symbol).map(|price_update| (price_update.bid, price_update.ask))
        }

        /// Close position using MARKET order with reduceOnly=true
        /// This is the recommended method for closing positions as it guarantees immediate execution
        ///
        /// # Arguments
        /// * `symbol` - Symbol to close position for
        /// * `use_market_only` - If true, only use MARKET orders (no LIMIT fallback).
        ///   Use true for TP/SL scenarios where fast execution is critical.
        ///   Use false for manual closes where LIMIT fallback is acceptable.
        pub async fn flatten_position(&self, symbol: &str, use_market_only: bool) -> Result<()> {
            self.venue.flatten_position(symbol, use_market_only).await
        }

        /// Cancel an order by order ID and symbol
        /// Used for cleaning up orders in race condition scenarios
        pub async fn cancel_order(&self, order_id: &str, symbol: &str) -> Result<()> {
            // Rate limit check
            {
                let mut limiter = self.rate_limiter.lock().await;
                limiter.check_order_rate().await;
            }

            self.venue.cancel(order_id, symbol).await
        }

    /// Start background task to periodically refresh symbol rules
    /// Refreshes rules every hour (or on shutdown) to pick up Binance rule changes
    async fn start_rules_refresh_task(&self) {
        let shutdown_flag = self.shutdown_flag.clone();

        tokio::spawn(async move {
            const REFRESH_INTERVAL_HOURS: u64 = 1;
            const REFRESH_INTERVAL_SECS: u64 = REFRESH_INTERVAL_HOURS * 3600;

            info!(
                interval_hours = REFRESH_INTERVAL_HOURS,
                "CONNECTION: Started symbol rules refresh task"
            );

            loop {
                // Wait for refresh interval or shutdown
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(REFRESH_INTERVAL_SECS)) => {
                        // Refresh interval elapsed - refresh all cached rules
                        let symbols_to_refresh: Vec<String> = FUT_RULES.iter().map(|entry| entry.key().clone()).collect();

                        if symbols_to_refresh.is_empty() {
                            tracing::debug!("CONNECTION: No cached symbols to refresh");
                            continue;
                        }

                        info!(
                            symbol_count = symbols_to_refresh.len(),
                            "CONNECTION: Refreshing rules for {} cached symbols",
                            symbols_to_refresh.len()
                        );

                        // Refresh each symbol by removing from cache
                        // Next access will fetch fresh rules from exchange
                        for sym in &symbols_to_refresh {
                            FUT_RULES.remove(sym);
                        }

                        info!(
                            symbol_count = symbols_to_refresh.len(),
                            "CONNECTION: Rules cache invalidated, fresh rules will be fetched on next access"
                        );
                    }
                    _ = async {
                        // Wait for shutdown flag
                        loop {
                            if shutdown_flag.load(AtomicOrdering::Relaxed) {
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    } => {
                        info!("CONNECTION: Rules refresh task shutting down");
                        break;
                    }
                }
            }
        });
    }

    /// Discover symbols based on config criteria
    /// Filters symbols by:
    /// - Quote asset (USDC or USDT based on config)
    /// - Status (must be TRADING)
    /// - Contract type (must be PERPETUAL)
    /// Returns list of discovered symbol names
    pub async fn discover_symbols(&self) -> Result<Vec<String>> {
        let quote_asset_upper = self.cfg.quote_asset.to_uppercase();
        let allow_usdt = self.cfg.allow_usdt_quote;

        // Determine which quote assets to include
        let mut allowed_quotes = vec![quote_asset_upper.clone()];
        if allow_usdt && quote_asset_upper != "USDT" {
            allowed_quotes.push("USDT".to_string());
        }
        if allow_usdt && quote_asset_upper != "USDC" {
            allowed_quotes.push("USDC".to_string());
        }

        info!(
            quote_asset = %self.cfg.quote_asset,
            allow_usdt,
            allowed_quotes = ?allowed_quotes,
            "CONNECTION: Discovering symbols with quote asset filter"
        );

        // Fetch all symbol metadata
        let all_symbols = self.venue.symbol_metadata().await?;

        // Filter symbols
        let discovered: Vec<String> = all_symbols
            .into_iter()
            .filter(|meta| {
                // Filter by quote asset
                let quote_match = allowed_quotes.contains(&meta.quote_asset.to_uppercase());

                // Filter by status (must be TRADING)
                let status_match = meta.status.as_deref() == Some("TRADING");

                // Filter by contract type (must be PERPETUAL)
                let contract_match = meta.contract_type.as_deref() == Some("PERPETUAL");

                let matches = quote_match && status_match && contract_match;

                if !matches {
                    tracing::debug!(
                        symbol = %meta.symbol,
                        quote_asset = %meta.quote_asset,
                        status = ?meta.status,
                        contract_type = ?meta.contract_type,
                        quote_match,
                        status_match,
                        contract_match,
                        "symbol filtered out during discovery"
                    );
                }

                matches
            })
            .map(|meta| meta.symbol)
            .collect();

        info!(
            discovered_count = discovered.len(),
            "CONNECTION: Symbol discovery completed"
        );

        if discovered.is_empty() {
            warn!(
                quote_asset = %self.cfg.quote_asset,
                allow_usdt,
                "CONNECTION: No symbols discovered with given criteria"
            );
        }

        Ok(discovered)
    }
}

// ============================================================================
// Internal Types (used only within connection.rs)
// ============================================================================

/// Exchange interface trait (internal to connection.rs)
/// Implemented by BinanceFutures
#[async_trait]
trait Venue: Send + Sync {
    async fn place_limit_with_client_id(
        &self,
        sym: &str,
        side: Side,
        px: Px,
        qty: Qty,
        tif: Tif,
        client_order_id: &str,
    ) -> Result<(String, Option<String>)>;

    async fn cancel(&self, order_id: &str, sym: &str) -> Result<()>;
    async fn best_prices(&self, sym: &str) -> Result<(Px, Px)>;
    async fn get_open_orders(&self, sym: &str) -> Result<Vec<VenueOrder>>;
    async fn get_position(&self, sym: &str) -> Result<Position>;
    async fn close_position(&self, sym: &str) -> Result<()>;
    async fn available_balance(&self, asset: &str) -> Result<Decimal>;
}

// ============================================================================
// Binance Exec Module (from binance_exec.rs)
// ============================================================================

#[derive(Deserialize)]
#[serde(tag = "filterType")]
#[allow(non_snake_case)]
enum FutFilter {
#[serde(rename = "PRICE_FILTER")]
    PriceFilter { tickSize: String },
#[serde(rename = "LOT_SIZE")]
    LotSize { stepSize: String },
#[serde(rename = "MIN_NOTIONAL")]
    MinNotional { notional: String },
#[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct FutExchangeInfo {
    symbols: Vec<FutExchangeSymbol>,
}

#[derive(Deserialize)]
struct FutExchangeSymbol {
    symbol: String,
#[serde(rename = "baseAsset")]
    base_asset: String,
#[serde(rename = "quoteAsset")]
    quote_asset: String,
#[serde(rename = "contractType")]
    contract_type: String,
    status: String,
#[serde(default)]
    filters: Vec<FutFilter>,
#[serde(rename = "pricePrecision", default)]
    price_precision: Option<usize>,
#[serde(rename = "quantityPrecision", default)]
    qty_precision: Option<usize>,
}

pub static FUT_RULES: Lazy<DashMap<String, Arc<SymbolRules>>> = Lazy::new(|| DashMap::new());

/// Price cache from WebSocket market data stream
/// Thread-safe price storage - updated by WebSocket, read by main loop
///
/// CONCURRENT WRITE BEHAVIOR:
/// - DashMap.insert() is atomic and thread-safe
/// - If multiple streams subscribe to the same symbol (duplicate subscription),
///   concurrent writes are safe: last write wins (correct for price data - we want latest)
/// - No data corruption or race conditions, but may have unnecessary writes
/// - Each symbol should ideally be in only one stream chunk to avoid duplicate updates
pub static PRICE_CACHE: Lazy<DashMap<String, PriceUpdate>> = Lazy::new(|| DashMap::new());

/// Position cache from WebSocket user data stream (ACCOUNT_UPDATE events)
/// Thread-safe position storage - updated by WebSocket, read by main loop
/// Binance recommendation: Use WebSocket instead of REST API for real-time data
pub static POSITION_CACHE: Lazy<DashMap<String, Position>> = Lazy::new(|| DashMap::new());

/// Balance cache from WebSocket user data stream (ACCOUNT_UPDATE events)
/// Thread-safe balance storage - updated by WebSocket, read by main loop
/// Binance recommendation: Use WebSocket instead of REST API for real-time data
pub static BALANCE_CACHE: Lazy<DashMap<String, Decimal>> = Lazy::new(|| DashMap::new());

/// Open orders cache from WebSocket user data stream (ORDER_TRADE_UPDATE events)
/// Thread-safe open orders storage - updated by WebSocket, read by main loop
/// Binance recommendation: Use WebSocket instead of REST API for real-time data
pub static OPEN_ORDERS_CACHE: Lazy<DashMap<String, Vec<VenueOrder>>> = Lazy::new(|| DashMap::new());

fn str_dec<S: AsRef<str>>(s: S) -> Decimal {
    let value = s.as_ref();
    Decimal::from_str(value).unwrap_or_else(|err| {
        warn!(input = value, ?err, "failed to parse decimal from string");
        Decimal::ZERO
    })
}

fn scale_from_step(step: Decimal) -> usize {
    if step.is_zero() {
        return 8; // Default precision
    }
    if step >= Decimal::ONE {
        return 0;
    }

    let scale = step.scale() as usize;
    scale
}

fn rules_from_fut_symbol(sym: FutExchangeSymbol) -> SymbolRules {
    let mut tick = Decimal::ZERO;
    let mut step = Decimal::ZERO;
    let mut min_notional = Decimal::ZERO;

    for f in sym.filters {
        match f {
            FutFilter::PriceFilter { tickSize } => {
                tick = str_dec(&tickSize);
                tracing::debug!(
                    symbol = %sym.symbol,
                    tick_size_raw = %tickSize,
                    tick_size_parsed = %tick,
                    "parsed PRICE_FILTER tickSize"
                );
            }
            FutFilter::LotSize { stepSize } => {
                step = str_dec(&stepSize);
                tracing::debug!(
                    symbol = %sym.symbol,
                    step_size_raw = %stepSize,
                    step_size_parsed = %step,
                    "parsed LOT_SIZE stepSize"
                );
            }
            FutFilter::MinNotional { notional } => {
                min_notional = str_dec(&notional);
                tracing::debug!(
                    symbol = %sym.symbol,
                    min_notional_raw = %notional,
                    min_notional_parsed = %min_notional,
                    "parsed MIN_NOTIONAL"
                );
            }
            FutFilter::Other => {}
        }
    }

// Precision calculation: use API value directly, not scale_from_step
    let p_prec = sym.price_precision.unwrap_or_else(|| {
        let calc = scale_from_step(tick);
        tracing::warn!(
            symbol = %sym.symbol,
            tick_size = %tick,
            calculated_precision = calc,
            "pricePrecision missing from API, calculated from tickSize"
        );
        calc
    });

    let q_prec = sym.qty_precision.unwrap_or_else(|| {
        let calc = scale_from_step(step);
        tracing::warn!(
            symbol = %sym.symbol,
            step_size = %step,
            calculated_precision = calc,
            "quantityPrecision missing from API, calculated from stepSize"
        );
        calc
    });

// Use more reasonable fallback values
    let final_tick = if tick.is_zero() {
        tracing::warn!(symbol = %sym.symbol, "tickSize is zero, using fallback 0.01");
        Decimal::new(1, 2) // 0.01
    } else {
        tick
    };

    let final_step = if step.is_zero() {
        tracing::warn!(symbol = %sym.symbol, "stepSize is zero, using fallback 0.001");
        Decimal::new(1, 3) // 0.001
    } else {
        step
    };

    tracing::debug!(
        symbol = %sym.symbol,
        tick_size = %final_tick,
        step_size = %final_step,
        price_precision = p_prec,
        qty_precision = q_prec,
        min_notional = %min_notional,
        "symbol rules parsed from exchangeInfo"
    );

    SymbolRules {
        tick_size: final_tick,
        step_size: final_step,
        price_precision: p_prec,
        qty_precision: q_prec,
        min_notional,
    }
}

// ---- Ortak ----

/// Binance API common configuration
///
/// Thread-safety and performance optimization
/// - `client: Arc<Client>`: reqwest::Client is thread-safe, wrapping in Arc avoids unnecessary overhead
/// - `sign()` function is thread-safe (uses immutable data, read-only)
#[derive(Clone)]
pub struct BinanceCommon {
    pub client: Arc<Client>,
    pub api_key: String,
    pub secret_key: String,
    pub recv_window_ms: u64,
}

impl BinanceCommon {
fn ts() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| {
                warn!("System time is before UNIX epoch, using fallback timestamp");
                Duration::from_secs(0)
            })
            .as_millis() as u64
    }

fn sign(&self, qs: &str) -> String {
        let mut mac = match Hmac::<Sha256>::new_from_slice(self.secret_key.as_bytes()) {
            Ok(mac) => mac,
            Err(e) => {
                error!(
                    error = %e,
                    secret_key_length = self.secret_key.len(),
                    "CRITICAL: HMAC key initialization failed, returning empty signature (API call will fail)"
                );
            // Return empty signature - API call will fail gracefully
                return String::new();
            }
        };
        mac.update(qs.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }
}

// ---- USDT-M Futures ----

#[derive(Clone)]
pub struct BinanceFutures {
    pub base: String, // e.g. https://fapi.binance.com
    pub common: BinanceCommon,
    pub price_tick: Decimal,
    pub qty_step: Decimal,
    pub price_precision: usize,
    pub qty_precision: usize,
    pub hedge_mode: bool, // Is hedge mode (dual-side position) enabled?
}

#[derive(Deserialize)]
struct OrderBookTop {
    bids: Vec<(String, String)>,
    asks: Vec<(String, String)>,
}

#[derive(Deserialize)]
struct FutPlacedOrder {
#[serde(rename = "orderId")]
    order_id: u64,
#[serde(rename = "clientOrderId")]
#[allow(dead_code)]
    client_order_id: Option<String>,
}

#[derive(Deserialize)]
struct FutOpenOrder {
#[serde(rename = "orderId")]
    order_id: u64,
#[serde(rename = "price")]
    price: String,
#[serde(rename = "origQty")]
    orig_qty: String,
#[serde(rename = "side")]
    side: String,
}

#[derive(Deserialize)]
struct FutPosition {
#[serde(rename = "symbol")]
    symbol: String,
#[serde(rename = "positionAmt")]
    position_amt: String,
#[serde(rename = "entryPrice")]
    entry_price: String,
#[serde(rename = "leverage")]
    leverage: String,
#[serde(rename = "liquidationPrice")]
    liquidation_price: String,
#[serde(rename = "positionSide", default)]
    position_side: Option<String>, // "LONG" | "SHORT" | "BOTH" (hedge mode) or None (one-way mode)
#[serde(rename = "marginType", default)]
    margin_type: String, // "isolated" or "cross"
}

#[derive(Deserialize)]
struct PremiumIndex {
#[serde(rename = "markPrice")]
    mark_price: String,
#[serde(rename = "lastFundingRate")]
#[serde(default)]
    last_funding_rate: Option<String>,
#[serde(rename = "nextFundingTime")]
#[serde(default)]
    next_funding_time: Option<u64>,
}

impl BinanceFutures {
/// Create BinanceFutures from config
pub fn from_config(
        binance_cfg: &crate::config::BinanceCfg,
        price_tick: f64,
        qty_step: f64,
    ) -> Result<Self> {
    use rust_decimal::Decimal;
    use std::str::FromStr;

        let base = if binance_cfg.futures_base.contains("testnet") {
            "https://testnet.binancefuture.com".to_string()
        } else {
            binance_cfg.futures_base.clone()
        };

        let common = BinanceCommon {
            client: Arc::new(Client::new()),
            api_key: binance_cfg.api_key.clone(),
            secret_key: binance_cfg.secret_key.clone(),
            recv_window_ms: binance_cfg.recv_window_ms,
        };

        let price_tick_dec = Decimal::from_str(&price_tick.to_string())?;
        let qty_step_dec = Decimal::from_str(&qty_step.to_string())?;

        Ok(BinanceFutures {
            base,
            common,
            price_tick: price_tick_dec,
            qty_step: qty_step_dec,
            price_precision: decimal_places(price_tick_dec),
            qty_precision: decimal_places(qty_step_dec),
            hedge_mode: binance_cfg.hedge_mode,
        })
    }

/// Set leverage (per symbol)
/// Must be set explicitly for each symbol at startup
/// Uses /fapi/v1/leverage endpoint for per-symbol leverage
    pub async fn set_leverage(&self, sym: &str, leverage: u32) -> Result<()> {
        let params = vec![
            format!("symbol={}", sym),
            format!("leverage={}", leverage),
            format!("timestamp={}", BinanceCommon::ts()),
            format!("recvWindow={}", self.common.recv_window_ms),
        ];
        let qs = params.join("&");
        let sig = self.common.sign(&qs);
        let url = format!("{}/fapi/v1/leverage?{}&signature={}", self.base, qs, sig);

        match send_void(
            self.common
                .client
                .post(&url)
                .header("X-MBX-APIKEY", &self.common.api_key),
        )
            .await
        {
            Ok(_) => {
                info!(%sym, leverage, "leverage set successfully");
                Ok(())
            }
            Err(e) => {
                warn!(%sym, leverage, error = %e, "failed to set leverage");
                Err(e)
            }
        }
    }

/// Set position side mode (enable/disable hedge mode)
/// Must be set explicitly at startup
/// Uses /fapi/v1/positionSide/dual endpoint to enable/disable hedge mode
    pub async fn set_position_side_dual(&self, dual: bool) -> Result<()> {
        let params = vec![
            format!("dualSidePosition={}", if dual { "true" } else { "false" }),
            format!("timestamp={}", BinanceCommon::ts()),
            format!("recvWindow={}", self.common.recv_window_ms),
        ];
        let qs = params.join("&");
        let sig = self.common.sign(&qs);
        let url = format!(
            "{}/fapi/v1/positionSide/dual?{}&signature={}",
            self.base, qs, sig
        );

        match send_void(
            self.common
                .client
                .post(&url)
                .header("X-MBX-APIKEY", &self.common.api_key),
        )
            .await
        {
            Ok(_) => {
                info!(dual_side = dual, "position side mode set successfully");
                Ok(())
            }
            Err(e) => {
                warn!(dual_side = dual, error = %e, "failed to set position side mode");
                Err(e)
            }
        }
    }

    pub async fn set_margin_type(&self, sym: &str, isolated: bool) -> Result<()> {
        let margin_type = if isolated { "ISOLATED" } else { "CROSSED" };
        let params = vec![
            format!("symbol={}", sym),
            format!("marginType={}", margin_type),
            format!("timestamp={}", BinanceCommon::ts()),
            format!("recvWindow={}", self.common.recv_window_ms),
        ];
        let qs = params.join("&");
        let sig = self.common.sign(&qs);
        let url = format!("{}/fapi/v1/marginType?{}&signature={}", self.base, qs, sig);

        match send_void(
            self.common
                .client
                .post(&url)
                .header("X-MBX-APIKEY", &self.common.api_key),
        )
            .await
        {
            Ok(_) => {
                info!(%sym, margin_type = %margin_type, "margin type set successfully");
                Ok(())
            }
            Err(e) => {
                warn!(%sym, margin_type = %margin_type, error = %e, "failed to set margin type");
                Err(e)
            }
        }
    }

/// Get per-symbol metadata (tick_size, step_size)
/// Does not use global fallback - real rules required for each symbol
/// Using fallback can cause LOT_SIZE and PRICE_FILTER errors
    pub async fn rules_for(&self, sym: &str) -> Result<Arc<SymbolRules>> {
    // Double-check locking pattern - prevent race condition
    // First check: Is it in cache?
        if let Some(r) = FUT_RULES.get(sym) {
            return Ok(r.clone());
        }

    // Geçici hata durumunda retry mekanizması (max 2 retry)
    const MAX_RETRIES: u32 = 2;
    const INITIAL_BACKOFF_MS: u64 = 100; // Exponential backoff başlangıç değeri
        let mut last_error = None;

        for attempt in 0..=MAX_RETRIES {
            let url = format!("{}/fapi/v1/exchangeInfo?symbol={}", self.base, encode(sym));
            match send_json::<FutExchangeInfo>(self.common.client.get(url)).await {
                Ok(info) => {
                    let sym_rec = info
                        .symbols
                        .into_iter()
                        .next()
                        .ok_or_else(|| anyhow!("symbol info missing"))?;

                // Double-check - another thread might have added it
                    if let Some(existing) = FUT_RULES.get(sym) {
                        return Ok(existing.clone());
                    }

                    let rules = Arc::new(rules_from_fut_symbol(sym_rec));
                    FUT_RULES.insert(sym.to_string(), rules.clone());
                    return Ok(rules);
                }
                Err(err) => {
                    last_error = Some(err);
                    if attempt < MAX_RETRIES {
                    // Use exponential backoff for rate limit handling
                        let backoff_ms = INITIAL_BACKOFF_MS * 2_u64.pow(attempt);
                        warn!(
                            error = ?last_error,
                            %sym,
                            attempt = attempt + 1,
                            max_retries = MAX_RETRIES + 1,
                            backoff_ms,
                            "failed to fetch futures symbol rules, retrying with exponential backoff..."
                        );
                        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                        continue;
                    }
                }
            }
        }

    // All retries failed - return error
        error!(
            error = ?last_error,
            %sym,
            "CRITICAL: failed to fetch futures symbol rules after {} retries, cannot use global fallback (would cause LOT_SIZE/PRICE_FILTER errors)",
            MAX_RETRIES + 1
        );
        Err(anyhow!(
            "failed to fetch symbol rules for {} after {} retries: {}",
            sym,
            MAX_RETRIES + 1,
            last_error
                .map(|e| e.to_string())
                .unwrap_or_else(|| "unknown error".to_string())
        ))
    }

/// Force refresh rules for a specific symbol by removing it from cache
/// Next call to rules_for() will fetch fresh rules from exchange
pub fn refresh_rules_for(&self, sym: &str) {
        FUT_RULES.remove(sym);
        info!(symbol = %sym, "CONNECTION: Rules cache invalidated for symbol");
    }

/// Refresh rules for all cached symbols
/// Removes all entries from cache, forcing fresh fetch on next access
pub fn refresh_all_rules(&self) {
        let count = FUT_RULES.len();
        FUT_RULES.clear();
        info!(
            cached_symbols = count,
            "CONNECTION: Rules cache cleared, all symbols will fetch fresh rules on next access"
        );
    }


    pub async fn symbol_metadata(&self) -> Result<Vec<SymbolMeta>> {
        let url = format!("{}/fapi/v1/exchangeInfo", self.base);
        let info: FutExchangeInfo = send_json(self.common.client.get(url)).await?;
        Ok(info
            .symbols
            .into_iter()
            .map(|s| SymbolMeta {
                symbol: s.symbol,
                base_asset: s.base_asset,
                quote_asset: s.quote_asset,
                status: Some(s.status),
                contract_type: Some(s.contract_type),
            })
            .collect())
    }

    pub async fn available_balance(&self, asset: &str) -> Result<Decimal> {
        if let Some(balance) = BALANCE_CACHE.get(asset) {
            return Ok(*balance);
        }

    // Fallback to REST API only if cache is empty (e.g., on startup before WebSocket data arrives)
        warn!(
            asset = %asset,
            "BALANCE_CACHE empty, falling back to REST API (should be rare - WebSocket should populate cache)"
        );
    #[derive(Deserialize)]
    struct FutBalance {
            asset: String,
        #[serde(rename = "availableBalance")]
            available_balance: String,
        }

        let qs = format!(
            "timestamp={}&recvWindow={}",
            BinanceCommon::ts(),
            self.common.recv_window_ms
        );
        let sig = self.common.sign(&qs);
        let url = format!("{}/fapi/v2/balance?{}&signature={}", self.base, qs, sig);
        let balances: Vec<FutBalance> = send_json(
            self.common
                .client
                .get(url)
                .header("X-MBX-APIKEY", &self.common.api_key),
        )
            .await?;

        let bal = balances.into_iter().find(|b| b.asset == asset);
        let amt = match bal {
            Some(b) => {
                let balance = Decimal::from_str(&b.available_balance)?;
            // Update cache with REST API result (for startup sync)
                BALANCE_CACHE.insert(asset.to_string(), balance);
                balance
            }
            None => Decimal::ZERO,
        };
        Ok(amt)
    }

    pub async fn fetch_open_orders(&self, sym: &str) -> Result<Vec<VenueOrder>> {
        if let Some(orders) = OPEN_ORDERS_CACHE.get(sym) {
            return Ok(orders.clone());
        }

        let qs = format!(
            "symbol={}&timestamp={}&recvWindow={}",
            sym,
            BinanceCommon::ts(),
            self.common.recv_window_ms
        );
        let sig = self.common.sign(&qs);
        let url = format!("{}/fapi/v1/openOrders?{}&signature={}", self.base, qs, sig);
        let orders: Vec<FutOpenOrder> = send_json(
            self.common
                .client
                .get(url)
                .header("X-MBX-APIKEY", &self.common.api_key),
        )
            .await?;
        let mut res = Vec::new();
        for o in orders {
            let price = Decimal::from_str(&o.price)?;
            let qty = Decimal::from_str(&o.orig_qty)?;
            let side = if o.side.eq_ignore_ascii_case("buy") {
                Side::Buy
            } else {
                Side::Sell
            };
            res.push(VenueOrder {
                order_id: o.order_id.to_string(),
                side,
                price: Px(price),
                qty: Qty(qty),
            });
        }

    // Update cache with REST API result (for startup sync)
        if !res.is_empty() {
            OPEN_ORDERS_CACHE.insert(sym.to_string(), res.clone());
        }

        Ok(res)
    }

/// Get current margin type for a symbol
/// Returns true if isolated, false if crossed
    pub async fn get_margin_type(&self, sym: &str) -> Result<bool> {
        let qs = format!(
            "symbol={}&timestamp={}&recvWindow={}",
            sym,
            BinanceCommon::ts(),
            self.common.recv_window_ms
        );
        let sig = self.common.sign(&qs);
        let url = format!(
            "{}/fapi/v2/positionRisk?{}&signature={}",
            self.base, qs, sig
        );
        let mut positions: Vec<FutPosition> = send_json(
            self.common
                .client
                .get(url)
                .header("X-MBX-APIKEY", &self.common.api_key),
        )
            .await?;
        let pos = positions
            .drain(..)
            .find(|p| p.symbol.eq_ignore_ascii_case(sym))
            .ok_or_else(|| anyhow!("position not found for symbol"))?;
        let is_isolated = pos.margin_type.eq_ignore_ascii_case("isolated");
        Ok(is_isolated)
    }

    pub async fn fetch_position(&self, sym: &str) -> Result<Position> {
        if let Some(position) = POSITION_CACHE.get(sym) {
            return Ok(position.clone());
        }

        warn!(
            symbol = %sym,
            "POSITION_CACHE empty, falling back to REST API (should be rare - WebSocket should populate cache)"
        );
        let qs = format!(
            "symbol={}&timestamp={}&recvWindow={}",
            sym,
            BinanceCommon::ts(),
            self.common.recv_window_ms
        );
        let sig = self.common.sign(&qs);
        let url = format!(
            "{}/fapi/v2/positionRisk?{}&signature={}",
            self.base, qs, sig
        );
        let positions: Vec<FutPosition> = send_json(
            self.common
                .client
                .get(url)
                .header("X-MBX-APIKEY", &self.common.api_key),
        )
            .await?;

        let matching_positions: Vec<&FutPosition> = positions
            .iter()
            .filter(|p| p.symbol.eq_ignore_ascii_case(sym))
            .collect();

        if matching_positions.is_empty() {
            return Err(anyhow!("position not found for symbol"));
        }

    // In one-way mode, positionSide should be "BOTH" or None, not "LONG"/"SHORT"
        if !self.hedge_mode {
            for pos in &matching_positions {
                if let Some(ref ps) = pos.position_side {
                    if ps == "LONG" || ps == "SHORT" {
                        warn!(
                            symbol = %sym,
                            position_side = %ps,
                            hedge_mode = self.hedge_mode,
                            "WARNING: positionSide is '{}' but hedge_mode is false - possible API inconsistency",
                            ps
                        );
                    }
                }
            }

        // Tek-yön modunda: Net pozisyonu hesapla (birden fazla pozisyon olmamalı ama kontrol edelim)
            let net_qty: Decimal = matching_positions
                .iter()
                .map(|p| Decimal::from_str(&p.position_amt).unwrap_or(Decimal::ZERO))
                .sum();

        // Tek-yön modunda sadece bir pozisyon olmalı (net pozisyon)
            let pos = matching_positions[0];
            let qty = Decimal::from_str(&pos.position_amt)?;
            let entry = Decimal::from_str(&pos.entry_price)?;
            let leverage = pos.leverage.parse::<u32>().unwrap_or(1);
            let liq = Decimal::from_str(&pos.liquidation_price).unwrap_or(Decimal::ZERO);
            let liq_px = if liq > Decimal::ZERO {
                Some(Px(liq))
            } else {
                None
            };

        // In one-way mode, if multiple positions exist, use net position
            if matching_positions.len() > 1 {
                warn!(
                    symbol = %sym,
                    positions_count = matching_positions.len(),
                    net_qty = %net_qty,
                    hedge_mode = self.hedge_mode,
                    "WARNING: Multiple positions found in one-way mode, using net position"
                );
            // Net pozisyonu kullan (LONG - SHORT)
                let position = Position {
                    symbol: sym.to_string(),
                    qty: Qty(net_qty),
                    entry: Px(entry), // İlk pozisyonun entry'si (net pozisyon için ortalama hesaplanabilir ama basit tutuyoruz)
                    leverage,
                    liq_px,
                };
            // Update cache with REST API result (for startup sync)
                POSITION_CACHE.insert(sym.to_string(), position.clone());
                Ok(position)
            } else {
                let position = Position {
                    symbol: sym.to_string(),
                    qty: Qty(qty),
                    entry: Px(entry),
                    leverage,
                    liq_px,
                };
            // Update cache with REST API result (for startup sync)
                POSITION_CACHE.insert(sym.to_string(), position.clone());
                Ok(position)
            }
        } else {
            warn!(
                symbol = %sym,
                "CONNECTION: Hedge mode enabled but support is incomplete. Position tracking, TP/SL, and closing may not work correctly for multiple positions per symbol."
            );

            let mut long_qty = Decimal::ZERO;
            let mut short_qty = Decimal::ZERO;
            let mut long_entry = Decimal::ZERO;
            let mut short_entry = Decimal::ZERO;
            let mut long_leverage = 1u32;
            let mut short_leverage = 1u32;
            let mut long_liq_px = None;
            let mut short_liq_px = None;

            for pos in matching_positions {
                let qty = Decimal::from_str(&pos.position_amt).unwrap_or(Decimal::ZERO);
                let entry = Decimal::from_str(&pos.entry_price).unwrap_or(Decimal::ZERO);
                let lev = pos.leverage.parse::<u32>().unwrap_or(1);
                let liq = Decimal::from_str(&pos.liquidation_price).unwrap_or(Decimal::ZERO);

            // positionSide check - in hedge mode should be "LONG" or "SHORT"
                match pos.position_side.as_deref() {
                    Some("LONG") => {
                        long_qty = qty;
                        long_entry = entry;
                        long_leverage = lev;
                        if liq > Decimal::ZERO {
                            long_liq_px = Some(Px(liq));
                        }
                    }
                    Some("SHORT") => {
                    // SHORT pozisyon için qty negatif olabilir, mutlak değerini al
                        short_qty = qty.abs();
                        short_entry = entry;
                        short_leverage = lev;
                        if liq > Decimal::ZERO {
                            short_liq_px = Some(Px(liq));
                        }
                    }
                    Some("BOTH") | None => {
                    // In hedge mode, "BOTH" or None is not expected, log warning
                        warn!(
                            symbol = %sym,
                            position_side = ?pos.position_side,
                            hedge_mode = self.hedge_mode,
                            "WARNING: positionSide is 'BOTH' or None in hedge mode - possible API inconsistency"
                        );
                    // Net pozisyonu hesapla (qty zaten net olabilir)
                        long_qty = qty.max(Decimal::ZERO);
                        short_qty = (-qty).max(Decimal::ZERO);
                        long_entry = entry;
                        short_entry = entry;
                        long_leverage = lev;
                        short_leverage = lev;
                        if liq > Decimal::ZERO {
                            long_liq_px = Some(Px(liq));
                            short_liq_px = Some(Px(liq));
                        }
                    }
                    Some(other) => {
                        warn!(
                            symbol = %sym,
                            position_side = %other,
                            hedge_mode = self.hedge_mode,
                            "WARNING: Unknown positionSide value in hedge mode"
                        );
                    }
                }
            }

            let (final_qty, final_entry, final_leverage, final_liq_px) = if long_qty > Decimal::ZERO {
            // LONG position exists - prioritize LONG
                if short_qty > Decimal::ZERO {
                    warn!(
                        symbol = %sym,
                        long_qty = %long_qty,
                        short_qty = %short_qty,
                        "CONNECTION: Both LONG and SHORT positions exist in hedge mode. Returning LONG position. SHORT should be tracked separately."
                    );
                }
                (long_qty, long_entry, long_leverage, long_liq_px)
            } else if short_qty > Decimal::ZERO {
            // Only SHORT position exists
                (short_qty, short_entry, short_leverage, short_liq_px)
            } else {
            // No position (zero)
                (Decimal::ZERO, Decimal::ZERO, 1u32, None)
            };

            let position = Position {
                symbol: sym.to_string(),
                qty: Qty(final_qty),
                entry: Px(final_entry),
                leverage: final_leverage,
                liq_px: final_liq_px,
            };
        // Update cache with REST API result
            POSITION_CACHE.insert(sym.to_string(), position.clone());
            Ok(position)
        }
    }

    pub async fn fetch_premium_index(&self, sym: &str) -> Result<(Px, Option<f64>, Option<u64>)> {
        let url = format!("{}/fapi/v1/premiumIndex?symbol={}", self.base, sym);
        let premium: PremiumIndex = send_json(self.common.client.get(url)).await?;
        let mark = Decimal::from_str(&premium.mark_price)?;
        let funding_rate = premium
            .last_funding_rate
            .as_deref()
            .and_then(|rate| rate.parse::<f64>().ok());
        let next_time = premium.next_funding_time.filter(|ts| *ts > 0);
        Ok((Px(mark), funding_rate, next_time))
    }

    pub async fn flatten_position(&self, sym: &str, use_market_only: bool) -> Result<()> {
        let initial_pos = match self.fetch_position(sym).await {
            Ok(pos) => pos,
            Err(e) => {
                if is_position_not_found_error(&e) {
                    info!(symbol = %sym, "position already closed (manual intervention detected), skipping close");
                    return Ok(());
                }
                return Err(e);
            }
        };
        let initial_qty = initial_pos.qty.0;

        if initial_qty.is_zero() {
        // Pozisyon zaten kapalı
            info!(symbol = %sym, "position already closed (zero quantity), skipping close");
            return Ok(());
        }

        let rules = self.rules_for(sym).await?;
        let initial_qty_abs = quantize_decimal(initial_qty.abs(), rules.step_size);

        if initial_qty_abs <= Decimal::ZERO {
            warn!(
                symbol = %sym,
                original_qty = %initial_qty,
                "quantized position size is zero, skipping close"
            );
            return Ok(());
        }


        let max_attempts = 3;
        let mut limit_fallback_attempted = false;
        let mut growth_event_count = 0u32;
    const MAX_RETRIES_ON_GROWTH: u32 = 2; // Max retries allowed when position grows

        for attempt in 0..max_attempts {
            if limit_fallback_attempted {
                return Err(anyhow::anyhow!(
                    "LIMIT fallback already attempted and failed, cannot proceed with retry. Position may need manual intervention."
                ));
            }

        // Check current position on each attempt
            let current_pos = match self.fetch_position(sym).await {
                Ok(pos) => pos,
                Err(e) => {
                // Only handle specific "position not found" errors
                    if is_position_not_found_error(&e) {
                        info!(symbol = %sym, attempt, "position already closed during retry (manual intervention detected)");
                        return Ok(());
                    }
                // Other errors (network, API errors, etc.) should be retried
                    return Err(e);
                }
            };
            let current_qty = current_pos.qty.0;

            if current_qty.is_zero() {
            // Pozisyon tamamen kapatıldı
                if attempt > 0 {
                    info!(
                        symbol = %sym,
                        attempts = attempt + 1,
                        initial_qty = %initial_qty,
                        "position fully closed after retry"
                    );
                }
                return Ok(());
            }

        // Kalan pozisyon miktarını hesapla (quantize et)
            let remaining_qty = quantize_decimal(current_qty.abs(), rules.step_size);

            if remaining_qty <= Decimal::ZERO {
            // Quantize sonrası sıfır oldu, pozisyon zaten kapalı sayılabilir
                return Ok(());
            }

        // Side belirleme (pozisyon yönüne göre)
            let side = if current_qty.is_sign_positive() {
                Side::Sell // Long → Sell
            } else {
                Side::Buy // Short → Buy
            };

            let qty_str = format_decimal_fixed(remaining_qty, rules.qty_precision);

        // Add positionSide parameter if hedge mode is enabled
            let position_side = if self.hedge_mode {
                if current_qty.is_sign_positive() {
                    Some("LONG")
                } else {
                    Some("SHORT")
                }
            } else {
                None
            };

        // Ensure reduceOnly=true and type=MARKET
            let mut params = vec![
                format!("symbol={}", sym),
                format!(
                    "side={}",
                    if matches!(side, Side::Buy) {
                        "BUY"
                    } else {
                        "SELL"
                    }
                ),
                "type=MARKET".to_string(), // Post-only değil, market order
                format!("quantity={}", qty_str),
                "reduceOnly=true".to_string(), // KRİTİK: Yeni pozisyon açmayı önle
                format!("timestamp={}", BinanceCommon::ts()),
                format!("recvWindow={}", self.common.recv_window_ms),
            ];

        // Hedge mode açıksa positionSide ekle
            if let Some(pos_side) = position_side {
                params.push(format!("positionSide={}", pos_side));
            }

            let qs = params.join("&");
            let sig = self.common.sign(&qs);
            let url = format!("{}/fapi/v1/order?{}&signature={}", self.base, qs, sig);

        // Emir gönder
            match send_void(
                self.common
                    .client
                    .post(&url)
                    .header("X-MBX-APIKEY", &self.common.api_key),
            )
                .await
            {
                Ok(_) => {
                    tokio::time::sleep(Duration::from_millis(1000)).await; // Exchange işlemesi için 1000ms bekle (Binance)
                    let verify_pos = if let Some(position) = POSITION_CACHE.get(sym) {
                        position.clone()
                    } else {
                    // Fallback to REST API only if cache is empty (should be rare)
                        warn!(
                            symbol = %sym,
                            "POSITION_CACHE empty during position verification, falling back to REST API"
                        );
                        self.fetch_position(sym).await?
                    };
                    let verify_qty = verify_pos.qty.0;

                    if verify_qty.is_zero() {
                    // Pozisyon tamamen kapatıldı
                        info!(
                            symbol = %sym,
                            attempt = attempt + 1,
                            initial_qty = %initial_qty,
                            "position fully closed and verified"
                        );
                        return Ok(());
                    } else {
                        let position_grew_from_attempt = verify_qty.abs() > current_qty.abs();
                        let position_grew_from_initial = verify_qty.abs() > initial_qty.abs();

                        if position_grew_from_attempt || position_grew_from_initial {
                        // Position büyümesi tespit edildi - büyüme yüzdesini hesapla
                            let growth_from_initial = if initial_qty.abs() > Decimal::ZERO {
                                ((verify_qty.abs() - initial_qty.abs()) / initial_qty.abs() * Decimal::from(100))
                                    .to_f64()
                                    .unwrap_or(0.0)
                            } else {
                                0.0
                            };

                        const MAX_ACCEPTABLE_GROWTH_PCT: f64 = 10.0;

                        // Position growth detection with abort mechanism
                            if growth_from_initial > MAX_ACCEPTABLE_GROWTH_PCT {
                                growth_event_count += 1;

                            // Too many growth events - abort to prevent infinite loop
                                if growth_event_count > MAX_RETRIES_ON_GROWTH {
                                    tracing::error!(
                                        symbol = %sym,
                                        attempt = attempt + 1,
                                        growth_events = growth_event_count,
                                        initial_qty = %initial_qty,
                                        current_qty_at_attempt = %current_qty,
                                        verify_qty = %verify_qty,
                                        growth_pct = growth_from_initial,
                                        max_growth_retries = MAX_RETRIES_ON_GROWTH,
                                        "POSITION GROWTH ABORT: Position grew {}% during close (exceeds {}% threshold) after {} growth events. Aborting to prevent infinite loop.",
                                        growth_from_initial,
                                        MAX_ACCEPTABLE_GROWTH_PCT,
                                        growth_event_count
                                    );
                                    return Err(anyhow!(
                                        "Position grew {}% during close (exceeds {}% threshold), aborting after {} growth events to prevent infinite loop. Position may need manual intervention.",
                                        growth_from_initial,
                                        MAX_ACCEPTABLE_GROWTH_PCT,
                                        growth_event_count
                                    ));
                                }

                            // ⚠️ WARNING: Position %10'dan fazla büyüdü - volatile market'te normal olabilir
                            // Aynı anda birden fazla fill olabilir, devam et (ama growth event sayısını artır)
                                tracing::warn!(
                                    symbol = %sym,
                                    attempt = attempt + 1,
                                    growth_events = growth_event_count,
                                    max_growth_retries = MAX_RETRIES_ON_GROWTH,
                                    initial_qty = %initial_qty,
                                    current_qty_at_attempt = %current_qty,
                                    verify_qty = %verify_qty,
                                    growth_pct = growth_from_initial,
                                    grew_from_attempt = position_grew_from_attempt,
                                    grew_from_initial = position_grew_from_initial,
                                    "POSITION GROWTH DETECTED: Position grew {}% during close (exceeds {}% threshold). Continuing with retry (attempt {}/{}) - this may be normal in volatile markets with multiple simultaneous fills.",
                                    growth_from_initial,
                                    MAX_ACCEPTABLE_GROWTH_PCT,
                                    growth_event_count,
                                    MAX_RETRIES_ON_GROWTH
                                );
                            } else {
                            // Position grew but below 10% threshold - may be normal in volatile markets
                                tracing::warn!(
                                    symbol = %sym,
                                    attempt = attempt + 1,
                                    initial_qty = %initial_qty,
                                    current_qty_at_attempt = %current_qty,
                                    verify_qty = %verify_qty,
                                    growth_pct = growth_from_initial,
                                    grew_from_attempt = position_grew_from_attempt,
                                    grew_from_initial = position_grew_from_initial,
                                    "POSITION GROWTH DETECTED: Position grew {}% during close (within acceptable {}% threshold). Continuing with retry - this may be normal in volatile markets.",
                                    growth_from_initial,
                                    MAX_ACCEPTABLE_GROWTH_PCT
                                );
                            }
                        }

                    // Calculate how much was closed in this attempt
                        let closed_amount = current_qty.abs() - verify_qty.abs();
                        let close_ratio = if current_qty.abs() > Decimal::ZERO {
                            closed_amount / current_qty.abs()
                        } else {
                            Decimal::ZERO
                        };

                    // Kalan pozisyon yüzdesi (initial'a göre)
                        let remaining_pct = if initial_qty.abs() > Decimal::ZERO {
                            (verify_qty.abs() / initial_qty.abs() * Decimal::from(100))
                                .to_f64()
                                .unwrap_or(0.0)
                        } else {
                            0.0
                        };

                        warn!(
                            symbol = %sym,
                            attempt = attempt + 1,
                            initial_qty = %initial_qty,
                            current_qty_at_attempt = %current_qty,
                            remaining_qty = %verify_qty,
                            closed_amount = %closed_amount,
                            close_ratio = %close_ratio,
                            remaining_pct = remaining_pct,
                            "partial close detected, retrying..."
                        );

                        if attempt < max_attempts - 1 {
                        // Son deneme değilse devam et
                            continue;
                        } else {
                        // Son denemede hala pozisyon varsa hata döndür
                            return Err(anyhow::anyhow!(
                                "Failed to fully close position after {} attempts. Initial: {}, Remaining: {}, Closed in last attempt: {}",
                                max_attempts,
                                initial_qty,
                                verify_qty,
                                closed_amount
                            ));
                        }
                    }
                }
                Err(e) => {
                    let error_str = e.to_string();
                    let _error_lower = error_str.to_lowercase();


                    if is_position_already_closed_error(&e) {
                        info!(
                            symbol = %sym,
                            attempt = attempt + 1,
                            error = %e,
                            "position already closed (manual intervention or already closed), treating as success"
                        );
                        return Ok(());
                    }

                    if use_market_only {
                    // Hızlı kapanış gereksiniminde: Retry yap veya hata döndür, LIMIT fallback yapma
                        warn!(
                            symbol = %sym,
                            attempt = attempt + 1,
                            error = %e,
                            remaining_qty = %remaining_qty,
                            "MARKET reduce-only failed in fast close mode, retrying..."
                        );
                        if attempt < max_attempts - 1 {
                            tokio::time::sleep(Duration::from_millis(500)).await;
                            continue;
                        } else {
                            return Err(anyhow::anyhow!(
                                "Failed to close position with MARKET reduce-only after {} attempts: {}",
                                max_attempts,
                                e
                            ));
                        }
                    }

                    if is_min_notional_error(&e) && !limit_fallback_attempted
                    {
                        let (best_bid, best_ask) = {
                        // Try WebSocket cache first (fastest, most up-to-date)
                            if let Some(price_update) = PRICE_CACHE.get(sym) {
                                (price_update.bid, price_update.ask)
                            } else {
                            // Fallback to REST API only if cache is empty (should be rare)
                                match self.best_prices(sym).await {
                                    Ok(prices) => prices,
                                    Err(e2) => {
                                        warn!(symbol = %sym, error = %e2, "failed to fetch best prices for dust check (WebSocket cache empty and REST API failed)");
                                        let assumed_min_price = Decimal::new(1, 2); // 0.01 (very conservative)
                                        let dust_threshold = rules.min_notional / assumed_min_price;

                                        warn!(
                                            symbol = %sym,
                                            min_notional = %rules.min_notional,
                                            assumed_price = %assumed_min_price,
                                            conservative_threshold = %dust_threshold,
                                            "Price fetch failed, using very conservative dust threshold (assumes price=0.01)"
                                        );

                                        if remaining_qty < dust_threshold {
                                            info!(
                                                symbol = %sym,
                                                remaining_qty = %remaining_qty,
                                                dust_threshold = %dust_threshold,
                                                min_notional = %rules.min_notional,
                                                "MIN_NOTIONAL error: remaining qty is dust (below threshold), considering position closed without LIMIT fallback"
                                            );
                                            return Ok(());
                                        }
                                    // Price fetch failed but not dust - continue to LIMIT fallback
                                        return Err(anyhow!(
                                            "MIN_NOTIONAL error and failed to fetch prices for dust check: {}",
                                            e2
                                        ));
                                    }
                                }
                            }
                        };

                        let dust_threshold = {
                            let current_price = if matches!(side, Side::Buy) {
                            // Short position closing: use ask price (BUY orders execute at ask)
                                best_ask.0
                            } else {
                            // Long position closing: use bid price (SELL orders execute at bid)
                                best_bid.0
                            };

                            if !current_price.is_zero() {
                                rules.min_notional / current_price
                            } else {
                                let assumed_min_price = Decimal::new(1, 2); // 0.01 (very conservative)
                                let conservative_threshold = rules.min_notional / assumed_min_price;

                                warn!(
                                    symbol = %sym,
                                    min_notional = %rules.min_notional,
                                    assumed_price = %assumed_min_price,
                                    conservative_threshold = %conservative_threshold,
                                    "Price is zero, using very conservative dust threshold (assumes price=0.01)"
                                );

                                conservative_threshold
                            }
                        };

                        if remaining_qty < dust_threshold {
                            info!(
                                symbol = %sym,
                                remaining_qty = %remaining_qty,
                                dust_threshold = %dust_threshold,
                                min_notional = %rules.min_notional,
                                "MIN_NOTIONAL error: remaining qty is dust (below threshold), considering position closed without LIMIT fallback"
                            );
                            return Ok(());
                        }

                        limit_fallback_attempted = true; // LIMIT fallback'i işaretle (tekrar denenmeyecek)
                        warn!(
                            symbol = %sym,
                            attempt = attempt + 1,
                            error = %e,
                            remaining_qty = %remaining_qty,
                            min_notional = %rules.min_notional,
                            "MIN_NOTIONAL error in reduce-only market close, trying limit reduce-only fallback (one-time)"
                        );

                        let tick_size = rules.tick_size;
                        let limit_price = if matches!(side, Side::Buy) {
                            best_bid.0 + tick_size
                        } else {
                            best_ask.0 - tick_size
                        };

                        let limit_price_quantized = quantize_decimal(limit_price, tick_size);
                        let limit_price_str =
                            format_decimal_fixed(limit_price_quantized, rules.price_precision);

                    // Limit reduce-only emri gönder
                        let mut limit_params = vec![
                            format!("symbol={}", sym),
                            format!(
                                "side={}",
                                if matches!(side, Side::Buy) {
                                    "BUY"
                                } else {
                                    "SELL"
                                }
                            ),
                            "type=LIMIT".to_string(),
                            "timeInForce=GTC".to_string(),
                            format!("price={}", limit_price_str),
                            format!("quantity={}", qty_str),
                            "reduceOnly=true".to_string(),
                            format!("timestamp={}", BinanceCommon::ts()),
                            format!("recvWindow={}", self.common.recv_window_ms),
                        ];

                        if let Some(pos_side) = position_side {
                            limit_params.push(format!("positionSide={}", pos_side));
                        }

                        let limit_qs = limit_params.join("&");
                        let limit_sig = self.common.sign(&limit_qs);
                        let limit_url = format!(
                            "{}/fapi/v1/order?{}&signature={}",
                            self.base, limit_qs, limit_sig
                        );

                        match send_void(
                            self.common
                                .client
                                .post(&limit_url)
                                .header("X-MBX-APIKEY", &self.common.api_key),
                        )
                            .await
                        {
                            Ok(_) => {
                                info!(
                                    symbol = %sym,
                                    limit_price = %limit_price_quantized,
                                    qty = %remaining_qty,
                                    "MIN_NOTIONAL fallback: limit reduce-only order placed successfully"
                                );
                            // Limit emri başarılı, pozisyon kapatılacak (emir fill olunca)
                                return Ok(());
                            }
                            Err(e2) => {
                                warn!(
                                    symbol = %sym,
                                    error = %e2,
                                    "MIN_NOTIONAL fallback: limit reduce-only order also failed"
                                );

                            // LIMIT fallback failed, check if remaining qty is dust
                            // Recalculate dust threshold with current prices (may have changed)
                                let dust_threshold = {
                                    let current_price = if matches!(side, Side::Buy) {
                                    // Short position closing: use ask price (BUY orders execute at ask)
                                        best_ask.0
                                    } else {
                                    // Long position closing: use bid price (SELL orders execute at bid)
                                        best_bid.0
                                    };
                                // Calculate dust threshold as min_notional / price (more accurate)
                                    if !current_price.is_zero() {
                                        rules.min_notional / current_price
                                    } else {
                                    // Price is zero - use very conservative threshold
                                        let assumed_min_price = Decimal::new(1, 2); // 0.01 (very conservative)
                                        let conservative_threshold = rules.min_notional / assumed_min_price;

                                        warn!(
                                            symbol = %sym,
                                            min_notional = %rules.min_notional,
                                            assumed_price = %assumed_min_price,
                                            conservative_threshold = %conservative_threshold,
                                            "Price is zero, using very conservative dust threshold (assumes price=0.01)"
                                        );

                                        conservative_threshold
                                    }
                                };

                                if remaining_qty < dust_threshold {
                                    info!(
                                        symbol = %sym,
                                        remaining_qty = %remaining_qty,
                                        dust_threshold = %dust_threshold,
                                        min_notional = %rules.min_notional,
                                        "MIN_NOTIONAL: remaining qty is dust (below threshold), considering position closed after LIMIT fallback failed"
                                    );
                                    return Ok(());
                                }

                            // LIMIT fallback failed and not dust - set flag to prevent retry
                                limit_fallback_attempted = true;
                                warn!(
                                    symbol = %sym,
                                    market_error = %e,
                                    limit_error = %e2,
                                    remaining_qty = %remaining_qty,
                                    min_notional = %rules.min_notional,
                                    "MIN_NOTIONAL: LIMIT fallback failed, will exit retry loop to prevent infinite loop"
                                );
                            // Retry loop'un başındaki kontrolle çıkış yapılacak
                            // continue ile retry loop'un başına dön, orada limit_fallback_attempted kontrolü yapılacak
                                continue;
                            }
                        }
                    } else {
                    // MIN_NOTIONAL hatası değil, normal retry
                        if attempt < max_attempts - 1 {
                            warn!(
                                symbol = %sym,
                                attempt = attempt + 1,
                                error = %e,
                                "failed to close position, retrying..."
                            );
                        // Wait before retry to avoid overloading exchange
                            tokio::time::sleep(Duration::from_millis(500)).await;
                            continue;
                        } else {
                            return Err(e);
                        }
                    }
                }
            }
        }

    // Buraya gelmemeli (yukarıdaki return'ler ile çıkılmalı)
        Err(anyhow::anyhow!("Unexpected error in flatten_position"))
    }
}

#[async_trait]
impl Venue for BinanceFutures {
    async fn place_limit_with_client_id(
        &self,
        sym: &str,
        side: Side,
        px: Px,
        qty: Qty,
        tif: Tif,
        client_order_id: &str,
    ) -> Result<(String, Option<String>)> {
        let s_side = match side {
            Side::Buy => "BUY",
            Side::Sell => "SELL",
        };
        let tif_str = match tif {
            Tif::PostOnly => "GTX", // Binance GTX: Post-only, cross ederse otomatik iptal
            Tif::Gtc => "GTC",
            Tif::Ioc => "IOC",
        };

        let rules = self.rules_for(sym).await?;
        let (price_str, qty_str, price_quantized, qty_quantized) =
            Self::validate_and_format_order_params(px, qty, &rules, sym)?;

        info!(
            %sym,
            side = ?side,
            price_original = %px.0,
            price_quantized = %price_quantized,
            price_str,
            qty_original = %qty.0,
            qty_quantized = %qty_quantized,
            qty_str,
            price_precision = rules.price_precision,
            qty_precision = rules.qty_precision,
            endpoint = "/fapi/v1/order",
            "order validation guard passed, submitting order"
        );

        let mut params = vec![
            format!("symbol={}", sym),
            format!("side={}", s_side),
            "type=LIMIT".to_string(),
            format!("timeInForce={}", tif_str),
            format!("price={}", price_str),
            format!("quantity={}", qty_str),
            format!("timestamp={}", BinanceCommon::ts()),
            format!("recvWindow={}", self.common.recv_window_ms),
            "newOrderRespType=RESULT".to_string(),
        ];

        if self.hedge_mode {
            let position_side = match side {
                Side::Buy => "LONG",
                Side::Sell => "SHORT",
            };
            params.push(format!("positionSide={}", position_side));
        }
        if !client_order_id.is_empty() {
        // Binance: max 36 karakter, alphanumeric
            if client_order_id.len() <= 36
                && client_order_id
                .chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
            {
                params.push(format!("newClientOrderId={}", client_order_id));
            } else {
                warn!(
                    %sym,
                    client_order_id = client_order_id,
                    "invalid clientOrderId format (max 36 chars, alphanumeric), skipping"
                );
            }
        }

    const MAX_RETRIES: u32 = 3;
    const INITIAL_BACKOFF_MS: u64 = 100;

        let mut last_error = None;
        let mut order_result: Option<FutPlacedOrder> = None;

        for attempt in 0..=MAX_RETRIES {
        // Her retry'de yeni request oluştur (aynı parametrelerle, aynı clientOrderId ile)
            let retry_qs = params.join("&");
            let retry_sig = self.common.sign(&retry_qs);
            let retry_url = format!(
                "{}/fapi/v1/order?{}&signature={}",
                self.base, retry_qs, retry_sig
            );

            match self
                .common
                .client
                .post(&retry_url)
                .header("X-MBX-APIKEY", &self.common.api_key)
                .send()
                .await
            {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        match resp.json::<FutPlacedOrder>().await {
                            Ok(order) => {
                                order_result = Some(order);
                                break; // Başarılı, döngüden çık
                            }
                            Err(e) => {
                                if attempt < MAX_RETRIES {
                                    let backoff_ms = INITIAL_BACKOFF_MS * 3_u64.pow(attempt);
                                    tracing::warn!(error = %e, attempt = attempt + 1, backoff_ms, "json parse error, retrying");
                                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                                    last_error = Some(anyhow!("json parse error: {}", e));
                                    continue;
                                } else {
                                    return Err(e.into());
                                }
                            }
                        }
                    } else {
                        let body = resp.text().await.unwrap_or_default();
                        let body_lower = body.to_lowercase();
                        let should_refresh_rules = body_lower.contains("precision is over")
                            || body_lower.contains("-1111")
                            || body_lower.contains("-1013")
                            || body_lower.contains("min notional")
                            || body_lower.contains("min_notional")
                            || body_lower.contains("below min notional");

                        if should_refresh_rules {
                            if attempt < MAX_RETRIES {
                                let error_type = if body_lower.contains("precision is over") || body_lower.contains("-1111") {
                                    "precision error (-1111)"
                                } else {
                                    "MIN_NOTIONAL error (-1013)"
                                };
                                warn!(%sym, attempt = attempt + 1, error_type, "rules cache invalidated, refreshing rules and retrying");

                            // Cache'i invalidate et
                                self.refresh_rules_for(sym);

                            // Fresh rules çek
                                match self.rules_for(sym).await {
                                    Ok(new_rules) => {
                                    // Yeni rules ile yeniden validate et
                                        match Self::validate_and_format_order_params(
                                            px, qty, &new_rules, sym,
                                        ) {
                                            Ok((new_price_str, new_qty_str, _, _)) => {
                                            // Yeni değerlerle retry
                                                let backoff_ms =
                                                    INITIAL_BACKOFF_MS * 3_u64.pow(attempt);
                                                tokio::time::sleep(Duration::from_millis(
                                                    backoff_ms,
                                                ))
                                                    .await;

                                            // Params'ı güncelle
                                                params = vec![
                                                    format!("symbol={}", sym),
                                                    format!("side={}", s_side),
                                                    "type=LIMIT".to_string(),
                                                    format!("timeInForce={}", tif_str),
                                                    format!("price={}", new_price_str),
                                                    format!("quantity={}", new_qty_str),
                                                    format!("timestamp={}", BinanceCommon::ts()),
                                                    format!(
                                                        "recvWindow={}",
                                                        self.common.recv_window_ms
                                                    ),
                                                    "newOrderRespType=RESULT".to_string(),
                                                ];

                                            // Add positionSide parameter if hedge mode is enabled
                                                if self.hedge_mode {
                                                    let position_side = match side {
                                                        Side::Buy => "LONG",
                                                        Side::Sell => "SHORT",
                                                    };
                                                    params.push(format!("positionSide={}", position_side));
                                                }

                                                if !client_order_id.is_empty() {
                                                    params.push(format!(
                                                        "newClientOrderId={}",
                                                        client_order_id
                                                    ));
                                                }

                                                last_error = Some(anyhow!("{} error, retrying with refreshed rules", error_type));
                                                continue;
                                            }
                                            Err(e) => {
                                                error!(%sym, error = %e, "validation failed after rules refresh, giving up");
                                                return Err(anyhow!("{} error, validation failed after rules refresh: {}", error_type, e));
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        error!(%sym, error = %e, "failed to refresh rules, giving up");
                                        return Err(anyhow!(
                                            "{} error, failed to refresh rules: {}",
                                            error_type,
                                            e
                                        ));
                                    }
                                }
                            } else {
                                let error_type = if body_lower.contains("precision is over") || body_lower.contains("-1111") {
                                    "precision error (-1111)"
                                } else {
                                    "MIN_NOTIONAL error (-1013)"
                                };
                                error!(%sym, attempt, error_type, "after max retries, symbol should be quarantined");
                                return Err(anyhow!(
                                    "binance api error: {} - {} ({}, max retries)",
                                    status,
                                    body,
                                    error_type
                                ));
                            }
                        }

                    // Kalıcı hata kontrolü
                        if is_permanent_error(status.as_u16(), &body) {
                            tracing::error!(%status, %body, attempt, "permanent error, no retry");
                            return Err(anyhow!(
                                "binance api error: {} - {} (permanent)",
                                status,
                                body
                            ));
                        }

                    // Transient hata kontrolü
                        if is_transient_error(status.as_u16(), &body) && attempt < MAX_RETRIES {
                            let backoff_ms = INITIAL_BACKOFF_MS * 3_u64.pow(attempt);
                            tracing::warn!(%status, %body, attempt = attempt + 1, backoff_ms, "transient error, retrying with exponential backoff (same clientOrderId)");
                            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                            last_error = Some(anyhow!("binance api error: {} - {}", status, body));
                            continue;
                        } else {
                        // Transient değil veya max retry'ye ulaşıldı
                            tracing::error!(%status, %body, attempt, "error after retries");
                            return Err(anyhow!("binance api error: {} - {}", status, body));
                        }
                    }
                }
                Err(e) => {
                // Network hatası
                    if attempt < MAX_RETRIES {
                        let backoff_ms = INITIAL_BACKOFF_MS * 3_u64.pow(attempt);
                        tracing::warn!(error = %e, attempt = attempt + 1, backoff_ms, "network error, retrying with exponential backoff (same clientOrderId)");
                        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                        last_error = Some(e.into());
                        continue;
                    } else {
                        tracing::error!(error = %e, attempt, "network error after retries");
                        return Err(e.into());
                    }
                }
            }
        }

    // Başarılı sonuç döndür
        let order = order_result
            .ok_or_else(|| last_error.unwrap_or_else(|| anyhow!("unknown error after retries")))?;

        info!(
            %sym,
            ?side,
            price_quantized = %price_quantized,
            qty_quantized = %qty_quantized,
            price_str,
            qty_str,
            tif = ?tif,
            order_id = order.order_id,
            "futures place_limit ok"
        );
        Ok((order.order_id.to_string(), order.client_order_id))
    }

    async fn cancel(&self, order_id: &str, sym: &str) -> Result<()> {
        let qs = format!(
            "symbol={}&orderId={}&timestamp={}&recvWindow={}",
            sym,
            order_id,
            BinanceCommon::ts(),
            self.common.recv_window_ms
        );
        let sig = self.common.sign(&qs);
        let url = format!("{}/fapi/v1/order?{}&signature={}", self.base, qs, sig);

        send_void(
            self.common
                .client
                .delete(url)
                .header("X-MBX-APIKEY", &self.common.api_key),
        )
            .await?;
        Ok(())
    }

    async fn best_prices(&self, sym: &str) -> Result<(Px, Px)> {
        if let Some(price_update) = PRICE_CACHE.get(sym) {
            return Ok((price_update.bid, price_update.ask));
        }
        warn!(
            symbol = %sym,
            "PRICE_CACHE empty in best_prices, falling back to REST API (should be rare)"
        );
        let url = format!("{}/fapi/v1/depth?symbol={}&limit=5", self.base, encode(sym));
        let d: OrderBookTop = send_json(self.common.client.get(url)).await?;
    use rust_decimal::Decimal;
        let best_bid = d.bids.get(0).ok_or_else(|| anyhow!("no bid"))?.0.clone();
        let best_ask = d.asks.get(0).ok_or_else(|| anyhow!("no ask"))?.0.clone();
        Ok((
            Px(Decimal::from_str(&best_bid)?),
            Px(Decimal::from_str(&best_ask)?),
        ))
    }

    async fn get_open_orders(&self, sym: &str) -> Result<Vec<VenueOrder>> {
        self.fetch_open_orders(sym).await
    }

    async fn get_position(&self, sym: &str) -> Result<Position> {
        self.fetch_position(sym).await
    }

    async fn close_position(&self, sym: &str) -> Result<()> {
    // Normal close: if MARKET fails, use LIMIT fallback
        self.flatten_position(sym, false).await
    }

    async fn available_balance(&self, asset: &str) -> Result<Decimal> {
    // Call BinanceFutures::available_balance method (not trait method to avoid recursion)
        BinanceFutures::available_balance(self, asset).await
    }
}

impl BinanceFutures {
    pub async fn test_order(
        &self,
        sym: &str,
        side: Side,
        px: Px,
        qty: Qty,
        tif: Tif,
    ) -> Result<()> {
        let s_side = match side {
            Side::Buy => "BUY",
            Side::Sell => "SELL",
        };
        let tif_str = match tif {
            Tif::PostOnly => "GTX",
            Tif::Gtc => "GTC",
            Tif::Ioc => "IOC",
        };

        let rules = self.rules_for(sym).await?;

    // Validation guard ile format et
        let (price_str, qty_str, _, _) =
            Self::validate_and_format_order_params(px, qty, &rules, sym)?;

        let mut params = vec![
            format!("symbol={}", sym),
            format!("side={}", s_side),
            "type=LIMIT".to_string(),
            format!("timeInForce={}", tif_str),
            format!("price={}", price_str),
            format!("quantity={}", qty_str),
            format!("timestamp={}", BinanceCommon::ts()),
            format!("recvWindow={}", self.common.recv_window_ms),
        ];

    // Add positionSide parameter if hedge mode is enabled
        if self.hedge_mode {
            let position_side = match side {
                Side::Buy => "LONG",
                Side::Sell => "SHORT",
            };
            params.push(format!("positionSide={}", position_side));
        }

        let qs = params.join("&");
        let sig = self.common.sign(&qs);
        let url = format!("{}/fapi/v1/order/test?{}&signature={}", self.base, qs, sig);

        match send_void(
            self.common
                .client
                .post(&url)
                .header("X-MBX-APIKEY", &self.common.api_key),
        )
            .await
        {
            Ok(_) => {
                info!(%sym, price_str, qty_str, "test order passed");
                Ok(())
            }
            Err(e) => {
                let error_str = e.to_string();
                let error_lower = error_str.to_lowercase();

            // -1111 hatası gelirse sembolü disable et
                if error_lower.contains("precision is over") || error_lower.contains("-1111") {
                    warn!(
                        %sym,
                        price_str,
                        qty_str,
                        error = %e,
                        "test order failed with -1111 (precision error), symbol should be disabled and rules refreshed"
                    );
                    Err(anyhow!("test order failed with precision error: {}", e))
                } else {
                    warn!(%sym, price_str, qty_str, error = %e, "test order failed");
                    Err(e)
                }
            }
        }
    }

pub fn validate_and_format_order_params(
        px: Px,
        qty: Qty,
        rules: &SymbolRules,
        sym: &str,
    ) -> Result<(String, String, Decimal, Decimal)> {
        let price_precision = rules.price_precision;
        let qty_precision = rules.qty_precision;

    // 1. Quantize: step_size'a göre floor
        let price_quantized = quantize_decimal(px.0, rules.tick_size);
        let qty_quantized = quantize_decimal(qty.0.abs(), rules.step_size);

    // 2. Round: precision'a göre round et
    // Use ToNegativeInfinity (floor) instead of ToZero for safety
        let price = price_quantized
            .round_dp_with_strategy(price_precision as u32, RoundingStrategy::ToNegativeInfinity);
        let qty_rounded =
            qty_quantized.round_dp_with_strategy(qty_precision as u32, RoundingStrategy::ToNegativeInfinity);

    // 3. Format: precision'a göre string'e çevir
        let price_str = format_decimal_fixed(price, price_precision);
        let qty_str = format_decimal_fixed(qty_rounded, qty_precision);

    // 4. KRİTİK: Son kontrol - fractional_digits kontrolü
        let price_fractional = if let Some(dot_pos) = price_str.find('.') {
            price_str[dot_pos + 1..].len()
        } else {
            0
        };
        let qty_fractional = if let Some(dot_pos) = qty_str.find('.') {
            qty_str[dot_pos + 1..].len()
        } else {
            0
        };

        if price_fractional > price_precision {
            let error_msg = format!(
                "CRITICAL: price_str fractional digits ({}) > price_precision ({}) for {}",
                price_fractional, price_precision, sym
            );
            tracing::error!(%sym, price_str, price_precision, price_fractional, %error_msg);
            return Err(anyhow!(error_msg));
        }

        if qty_fractional > qty_precision {
            let error_msg = format!(
                "CRITICAL: qty_str fractional digits ({}) > qty_precision ({}) for {}",
                qty_fractional, qty_precision, sym
            );
            tracing::error!(%sym, qty_str, qty_precision, qty_fractional, %error_msg);
            return Err(anyhow!(error_msg));
        }

    // 5. Min notional kontrolü
        let notional = price * qty_rounded;
        if !rules.min_notional.is_zero() && notional < rules.min_notional {
            return Err(anyhow!(
                "below min notional after validation ({} < {})",
                notional,
                rules.min_notional
            ));
        }

        Ok((price_str, qty_str, price, qty_rounded))
    }
}

// ---- helpers ----

/// Check if error indicates position not found (already closed)
///
/// Returns true if error message contains position not found indicators.
/// This is used to handle cases where position was manually closed or doesn't exist.
fn is_position_not_found_error(error: &anyhow::Error) -> bool {
    let error_str = error.to_string().to_lowercase();
    error_str.contains("position not found")
        || error_str.contains("no position")
        || error_str.contains("-2011") // Binance: Unknown order (position not found)
}

/// Check if error indicates MIN_NOTIONAL error
///
/// Returns true if error message contains MIN_NOTIONAL error indicators.
/// This is used to trigger LIMIT fallback for small position sizes.
fn is_min_notional_error(error: &anyhow::Error) -> bool {
    let error_str = error.to_string().to_lowercase();
    error_str.contains("-1013")
        || error_str.contains("min notional")
        || error_str.contains("min_notional")
}

/// Check if error indicates position already closed or invalid state
///
/// Returns true if error message indicates position is already closed or in invalid state.
/// This is used to treat certain errors as success (position already closed).
fn is_position_already_closed_error(error: &anyhow::Error) -> bool {
    let error_str = error.to_string().to_lowercase();
    error_str.contains("position not found")
        || error_str.contains("no position")
        || error_str.contains("position already closed")
        || error_str.contains("reduceonly")
        || error_str.contains("reduce only")
        || error_str.contains("-2011") // Binance: "Unknown order sent"
        || error_str.contains("-2019") // Binance: "Margin is insufficient"
        || error_str.contains("-2021") // Binance: "Order would immediately match"
}

/// Quantize decimal value to step (floor to nearest step multiple)
///
/// ✅ KRİTİK: Precision loss önleme
/// Decimal division ve multiplication yaparken precision loss olabilir.
/// Sonucu normalize ederek step'in tam katı olduğundan emin oluyoruz.
pub fn quantize_decimal(value: Decimal, step: Decimal) -> Decimal {
    if step.is_zero() || step.is_sign_negative() {
        return value;
    }

    let ratio = value / step;
    let floored = ratio.floor();
    let result = floored * step;
    let step_scale = step.scale();
    let normalized = result.normalize();

// Round to step's scale first
    let rounded = normalized.round_dp_with_strategy(step_scale, rust_decimal::RoundingStrategy::ToNegativeInfinity);

// Double-check quantization - ensure result is a multiple of step
    let re_quantized_ratio = rounded / step;
    let re_quantized_floor = re_quantized_ratio.floor();
    let final_result = re_quantized_floor * step;

// Final normalization and rounding
    final_result.normalize().round_dp_with_strategy(step_scale, rust_decimal::RoundingStrategy::ToNegativeInfinity)
}

/// Format decimal with fixed precision
///
/// Prevents precision loss
/// ToZero strategy truncates, which can cause precision loss.
/// Normalize and use correct rounding strategy to prevent precision loss.
pub fn format_decimal_fixed(value: Decimal, precision: usize) -> String {
    let precision = precision.min(28);
    let scale = precision as u32;
    let normalized = value.normalize();
    let rounded = normalized.round_dp_with_strategy(scale, RoundingStrategy::ToNegativeInfinity);

    if scale == 0 {
    // Get integer part (truncate if decimal point exists)
        let s = rounded.to_string();
        if let Some(dot_pos) = s.find('.') {
            s[..dot_pos].to_string()
        } else {
            s
        }
    } else {
        let s = rounded.to_string();
        if let Some(dot_pos) = s.find('.') {
            let integer_part = &s[..dot_pos];
            let decimal_part = &s[dot_pos + 1..];
            let current_decimals = decimal_part.len();

            if current_decimals < scale as usize {
            // Add missing trailing zeros
                format!(
                    "{}.{}{}",
                    integer_part,
                    decimal_part,
                    "0".repeat(scale as usize - current_decimals)
                )
            } else if current_decimals > scale as usize {
            // If too many decimals, truncate - never show more digits than precision
            // Truncate string - prevents "Precision is over the maximum" error
                let truncated_decimal = &decimal_part[..scale as usize];
                format!("{}.{}", integer_part, truncated_decimal)
            } else {
            // Exact precision - preserve trailing zeros
            // Decimal's to_string() may remove trailing zeros, so check
                if decimal_part.len() == scale as usize {
                    s
                } else {
                // Add trailing zeros if missing
                    format!(
                        "{}.{}{}",
                        integer_part,
                        decimal_part,
                        "0".repeat(scale as usize - decimal_part.len())
                    )
                }
            }
        } else {
        // Add decimal point and trailing zeros if missing
            format!("{}.{}", s, "0".repeat(scale as usize))
        }
    }
}

async fn ensure_success(resp: Response) -> Result<Response> {
    let status = resp.status();
    if status.is_success() {
        Ok(resp)
    } else {
        let body = resp.text().await.unwrap_or_default();
        tracing::error!(%status, %body, "binance api error");
        Err(anyhow!("binance api error: {} - {}", status, body))
    }
}

fn is_transient_error(status: u16, _body: &str) -> bool {
    match status {
        408 => true,       // Request Timeout
        429 => true,       // Too Many Requests
        500..=599 => true, // Server Errors
        400 => {
            false
        }
        _ => false, // Other errors are permanent
    }
}

/// Check if error is permanent (should symbol be disabled?)
/// Errors like "invalid", "margin", "precision" are permanent
fn is_permanent_error(status: u16, body: &str) -> bool {
    if status == 400 {
        let body_lower = body.to_lowercase();
        if body_lower.contains("precision is over") || body_lower.contains("-1111") {
            return false; // Precision error can be retried
        }
        if body_lower.contains("-1013")
            || body_lower.contains("min notional")
            || body_lower.contains("min_notional")
            || body_lower.contains("below min notional")
        {
            return false; // MIN_NOTIONAL hatası retry edilebilir (rules refresh ile)
        }
        body_lower.contains("invalid")
            || body_lower.contains("margin")
            || body_lower.contains("insufficient balance")
    } else {
        false
    }
}

async fn send_json<T>(builder: RequestBuilder) -> Result<T>
where
    T: DeserializeOwned,
{
    let resp = builder.send().await?;
    let resp = ensure_success(resp).await?;
    Ok(resp.json().await?)
}

async fn send_void(builder: RequestBuilder) -> Result<()> {
    let resp = builder.send().await?;
    ensure_success(resp).await?;
    Ok(())
}


// ============================================================================
// Binance WebSocket Module (from binance_ws.rs)
// ============================================================================

pub type WsStream = WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Deserialize)]
struct ListenKeyResp {
#[serde(rename = "listenKey")]
    listen_key: String,
}


pub struct UserDataStream {
    client: Client,
    base: String,
    api_key: String,
    kind: UserStreamKind,
    listen_key: String,
    ws: WsStream,
    last_keep_alive: Instant,
    on_reconnect: Option<Box<dyn Fn() + Send + Sync>>,
}

pub struct MarketDataStream {
    ws: WsStream,
    symbols: Vec<String>,
}

impl MarketDataStream {
/// Create market data stream for multiple symbols
/// Binance @bookTicker stream: wss://fstream.binance.com/stream?streams=btcusdt@bookTicker/ethusdt@bookTicker
    pub async fn connect(symbols: &[String]) -> Result<Self> {
    // Build stream URL: wss://fstream.binance.com/stream?streams=symbol1@bookTicker/symbol2@bookTicker
        let streams: Vec<String> = symbols
            .iter()
            .map(|s| format!("{}@bookTicker", s.to_lowercase()))
            .collect();
        let stream_param = streams.join("/");
        let url = format!("wss://fstream.binance.com/stream?streams={}", stream_param);

        info!(url = %url, symbol_count = symbols.len(), "connecting to market data websocket");
        let (ws, _) = connect_async(&url).await?;
        info!("connected to market data websocket");

        Ok(Self {
            ws,
            symbols: symbols.to_vec(),
        })
    }

/// Get next price update
    pub async fn next_price_update(&mut self) -> Result<PriceUpdate> {
        loop {
            match timeout(Duration::from_secs(300), self.ws.next()).await {
                Ok(Some(msg)) => {
                    let msg = msg.map_err(|e| match e {
                        WsError::ConnectionClosed | WsError::AlreadyClosed => {
                            anyhow!("market data stream closed")
                        }
                        other => anyhow!(other),
                    })?;

                    match msg {
                        Message::Ping(payload) => {
                            self.ws.send(Message::Pong(payload)).await?;
                            continue;
                        }
                        Message::Pong(_) => continue,
                        Message::Text(txt) => {
                            if txt.is_empty() {
                                continue;
                            }

                        // Binance format: {"stream":"btcusdt@bookTicker","data":{...}}
                            let value: Value = serde_json::from_str(&txt)?;
                            let stream_name = value.get("stream")
                                .and_then(|s| s.as_str())
                                .ok_or_else(|| anyhow!("missing stream field"))?;

                        // Extract symbol from stream name (e.g., "btcusdt@bookTicker" -> "BTCUSDT")
                            let symbol = stream_name
                                .split('@')
                                .next()
                                .ok_or_else(|| anyhow!("invalid stream name"))?
                                .to_uppercase();

                            let data = value.get("data")
                                .ok_or_else(|| anyhow!("missing data field"))?;

                        // Parse @bookTicker format
                        // {"b":"50000.00","B":"1.5","a":"50001.00","A":"2.0"}
                            let bid_str = data.get("b")
                                .and_then(|v| v.as_str())
                                .ok_or_else(|| anyhow!("missing bid price"))?;
                            let ask_str = data.get("a")
                                .and_then(|v| v.as_str())
                                .ok_or_else(|| anyhow!("missing ask price"))?;
                            let bid_qty_str = data.get("B")
                                .and_then(|v| v.as_str())
                                .unwrap_or("0");
                            let ask_qty_str = data.get("A")
                                .and_then(|v| v.as_str())
                                .unwrap_or("0");

                            let bid = Decimal::from_str(bid_str)?;
                            let ask = Decimal::from_str(ask_str)?;
                            let bid_qty = Decimal::from_str(bid_qty_str).unwrap_or(Decimal::ZERO);
                            let ask_qty = Decimal::from_str(ask_qty_str).unwrap_or(Decimal::ZERO);

                            return Ok(PriceUpdate {
                                symbol,
                                bid: Px(bid),
                                ask: Px(ask),
                                bid_qty: Qty(bid_qty),
                                ask_qty: Qty(ask_qty),
                            });
                        }
                        Message::Binary(_) => continue,
                        Message::Close(_) => return Err(anyhow!("market data stream closed")),
                        Message::Frame(_) => continue,
                    }
                }
                Ok(None) => return Err(anyhow!("market data stream terminated")),
                Err(_) => {
                    warn!("market data websocket timeout, reconnecting");
                // Reconnect logic could be added here if needed
                    return Err(anyhow!("market data stream timeout"));
                }
            }
        }
    }
}

impl UserDataStream {
#[inline]
fn ws_url_for(_kind: UserStreamKind, listen_key: &str) -> String {
    // USDⓈ-M Futures user data
        format!("wss://fstream.binance.com/ws/{}", listen_key)
    }

    async fn create_listen_key(
        client: &Client,
        base: &str,
        api_key: &str,
        _kind: UserStreamKind,
    ) -> Result<String> {
        let base = base.trim_end_matches('/');
        let endpoint = format!("{}/fapi/v1/listenKey", base);

        let resp = client
            .post(&endpoint)
            .header("X-MBX-APIKEY", api_key)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
        // resp.text() self'i tükettiği için status’u ÖNCE aldık
            let body = resp.text().await.unwrap_or_default();
            error!(status=?status, body=%body, "listenKey create failed");
            return Err(anyhow!("listenKey create failed: {} {}", status, body));
        }

        let lk: ListenKeyResp = resp.json().await?;
        info!(listen_key=%lk.listen_key, "listenKey created");
        Ok(lk.listen_key)
    }

    async fn keepalive_listen_key(&self, listen_key: &str) -> Result<()> {
        let base = self.base.trim_end_matches('/');
        let endpoint = format!("{}/fapi/v1/listenKey?listenKey={}", base, listen_key);

        let resp = self
            .client
            .put(&endpoint)
            .header("X-MBX-APIKEY", &self.api_key)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            warn!(status=?status, body=%body, "listenKey keepalive failed");
            return Err(anyhow!("listenKey keepalive failed: {} {}", status, body));
        }

        debug!("refreshed user data listen key");
        Ok(())
    }

    async fn reconnect_ws_without_new_key(&mut self) -> Result<()> {
        let url = Self::ws_url_for(self.kind, &self.listen_key);
        let (ws, _) = connect_async(&url).await?;
        self.ws = ws;
        self.last_keep_alive = Instant::now();

        if let Some(ref callback) = self.on_reconnect {
            callback();
        }
        info!(%url, "reconnected user data websocket (same listen key)");
        Ok(())
    }

    async fn reconnect_ws(&mut self) -> Result<()> {
        match self.reconnect_ws_without_new_key().await {
            Ok(()) => {
                return Ok(());
            }
            Err(e) => {
                warn!(error = %e, "reconnect with existing listen key failed, creating new listen key");
            }
        }

        let new_key =
            Self::create_listen_key(&self.client, &self.base, &self.api_key, self.kind).await?;
        self.listen_key = new_key;

        let url = Self::ws_url_for(self.kind, &self.listen_key);
        let (ws, _) = connect_async(&url).await?;
        self.ws = ws;
        self.last_keep_alive = Instant::now();

        if let Some(ref callback) = self.on_reconnect {
            callback();
            info!(%url, "reconnected user data websocket (new listen key), callback triggered");
        }

        info!(%url, "reconnected user data websocket (new listen key)");
        Ok(())
    }

pub fn set_on_reconnect<F>(&mut self, callback: F)
    where
        F: Fn() + Send + Sync + 'static,
    {
        self.on_reconnect = Some(Box::new(callback));
    }

    pub async fn connect(
        client: Client,
        base: &str,
        api_key: &str,
        kind: UserStreamKind,
    ) -> Result<Self> {
        let base = base.trim_end_matches('/').to_string();

        let listen_key = Self::create_listen_key(&client, &base, api_key, kind).await?;

        let ws_url = Self::ws_url_for(kind, &listen_key);
        let (ws, _) = connect_async(&ws_url).await?;
        info!(%ws_url, "connected user data websocket");

        Ok(Self {
            client,
            base,
            api_key: api_key.to_string(),
            kind,
            listen_key,
            ws,
            last_keep_alive: Instant::now(),
            on_reconnect: None,
        })
    }

    async fn keep_alive(&mut self) -> Result<()> {
    // Binance listenKey 60dk geçerli; biz 25dk'da bir yeniliyoruz
        if self.last_keep_alive.elapsed() < Duration::from_secs(60 * 25) {
            return Ok(());
        }

        match self.keepalive_listen_key(&self.listen_key).await {
            Ok(()) => {
                self.last_keep_alive = Instant::now();
                return Ok(());
            }
            Err(e) => {
                warn!(err=?e, "keepalive failed; will recreate listenKey and reconnect ws");
            }
        }

        let new_key =
            Self::create_listen_key(&self.client, &self.base, &self.api_key, self.kind).await?;
        self.listen_key = new_key;

    // WS yeniden bağlan
        self.reconnect_ws().await?;
        Ok(())
    }

fn parse_side(side: &str) -> Side {
        if side.eq_ignore_ascii_case("buy") {
            Side::Buy
        } else {
            Side::Sell
        }
    }

fn parse_decimal(value: &Value, key: &str) -> Decimal {
        value
            .get(key)
            .and_then(Value::as_str)
            .and_then(|s| Decimal::from_str(s).ok())
            .unwrap_or(Decimal::ZERO)
    }

    pub async fn next_event(&mut self) -> Result<UserEvent> {
        loop {
            self.keep_alive().await?;
            match timeout(Duration::from_secs(90), self.ws.next()).await {
                Ok(Some(msg)) => {
                    let msg = msg.map_err(|e| match e {
                        WsError::ConnectionClosed | WsError::AlreadyClosed => {
                            anyhow!("user stream closed")
                        }
                        other => anyhow!(other),
                    })?;

                    match msg {
                        Message::Ping(payload) => {
                            self.ws.send(Message::Pong(payload)).await?;
                            return Ok(UserEvent::Heartbeat);
                        }
                        Message::Pong(_) => return Ok(UserEvent::Heartbeat),
                        Message::Text(txt) => {
                            if txt.is_empty() {
                                continue;
                            }
                        // Uyarı: USDⓈ-M tarafında auth wrapper'da {"stream": "...", "data": {...}} gelebilir.
                            let value: Value = serde_json::from_str(&txt)?;
                            let data = value.get("data").cloned().unwrap_or_else(|| value.clone());
                            if let Some(event) = Self::map_event(&data)? {
                                return Ok(event);
                            }
                        }
                        Message::Binary(_) => continue,
                        Message::Close(_) => return Err(anyhow!("user stream closed")),
                        Message::Frame(_) => continue,
                    }
                }
                Ok(None) => return Err(anyhow!("user stream terminated")),
                Err(_) => {
                    warn!("websocket timeout (90s), reconnecting");
                    self.reconnect_ws().await?;
                }
            }
        }
    }

fn map_event(value: &Value) -> Result<Option<UserEvent>> {
        let event_type = value.get("e").and_then(Value::as_str).unwrap_or_default();
        match event_type {
            "executionReport" => {
                let symbol = value
                    .get("s")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let order_id = value
                    .get("i")
                    .and_then(Value::as_i64)
                    .unwrap_or_default()
                    .to_string();
                let client_order_id = value
                    .get("c")
                    .and_then(Value::as_str)
                    .map(|s| s.to_string());
                let status = value.get("X").and_then(Value::as_str).unwrap_or_default();
                if status == "CANCELED" {
                    return Ok(Some(UserEvent::OrderCanceled {
                        symbol,
                        order_id,
                        client_order_id,
                    }));
                }
                let exec_type = value.get("x").and_then(Value::as_str).unwrap_or_default();
                if exec_type != "TRADE" {
                    return Ok(Some(UserEvent::Heartbeat));
                }
                let last_filled_qty = Self::parse_decimal(value, "l"); // last executed qty (incremental)
                let cumulative_filled_qty = Self::parse_decimal(value, "z"); // cumulative filled qty (total)
                let order_qty = Self::parse_decimal(value, "q"); // original order quantity
                let price = Self::parse_decimal(value, "L"); // last executed price
                let side =
                    Self::parse_side(value.get("S").and_then(Value::as_str).unwrap_or("SELL"));
                let is_maker = value.get("m").and_then(Value::as_bool).unwrap_or(false);
                let commission = Self::parse_decimal(value, "n");
                return Ok(Some(UserEvent::OrderFill {
                    symbol,
                    order_id,
                    side,
                    qty: Qty(last_filled_qty),
                    cumulative_filled_qty: Qty(cumulative_filled_qty),
                    order_qty: Some(Qty(order_qty)),
                    price: Px(price),
                    is_maker,
                    order_status: status.to_string(),
                    commission,
                }));
            }

            "ORDER_TRADE_UPDATE" => {
                let data = value
                    .get("o")
                    .ok_or_else(|| anyhow!("missing order payload"))?;
                let symbol = data
                    .get("s")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let order_id = data
                    .get("i")
                    .and_then(Value::as_i64)
                    .unwrap_or_default()
                    .to_string();
                let client_order_id = data.get("c").and_then(Value::as_str).map(|s| s.to_string());
                let status = data.get("X").and_then(Value::as_str).unwrap_or_default();
                if status == "CANCELED" {
                    return Ok(Some(UserEvent::OrderCanceled {
                        symbol,
                        order_id,
                        client_order_id,
                    }));
                }
                let exec_type = data.get("x").and_then(Value::as_str).unwrap_or_default();
                if exec_type != "TRADE" {
                    return Ok(Some(UserEvent::Heartbeat));
                }
                let last_filled_qty = Self::parse_decimal(data, "l"); // last filled (incremental)
                let cumulative_filled_qty = Self::parse_decimal(data, "z"); // cumulative filled qty (total)
                let order_qty = Self::parse_decimal(data, "q"); // original order quantity
                let price = Self::parse_decimal(data, "L"); // last price
                let side =
                    Self::parse_side(data.get("S").and_then(Value::as_str).unwrap_or("SELL"));
                let is_maker = data.get("m").and_then(Value::as_bool).unwrap_or(false);
                let commission = Self::parse_decimal(data, "n");
                return Ok(Some(UserEvent::OrderFill {
                    symbol,
                    order_id,
                    side,
                    qty: Qty(last_filled_qty), // Last executed qty (incremental)
                    cumulative_filled_qty: Qty(cumulative_filled_qty), // Cumulative filled qty (total)
                    order_qty: Some(Qty(order_qty)), // Original order quantity
                    price: Px(price),
                    is_maker,
                    order_status: status.to_string(),
                    commission,
                }));
            }

        // ACCOUNT_UPDATE event - position and balance updates from WebSocket
            "ACCOUNT_UPDATE" => {
                let mut positions = Vec::new();
                let mut balances = Vec::new();

            // Parse positions (a field)
                if let Some(positions_data) = value.get("a").and_then(|v| v.get("P")) {
                    if let Some(positions_array) = positions_data.as_array() {
                        for pos_data in positions_array {
                            if let (Some(symbol), Some(amt), Some(entry)) = (
                                pos_data.get("s").and_then(Value::as_str),
                                pos_data.get("pa").and_then(Value::as_str),
                                pos_data.get("ep").and_then(Value::as_str),
                            ) {
                                let position_amt = Decimal::from_str(amt).unwrap_or(Decimal::ZERO);
                                let entry_price = Decimal::from_str(entry).unwrap_or(Decimal::ZERO);
                                let leverage = pos_data
                                    .get("l")
                                    .and_then(Value::as_str)
                                    .and_then(|s| s.parse::<u32>().ok())
                                    .unwrap_or(1);
                                let unrealized_pnl = pos_data
                                    .get("up")
                                    .and_then(Value::as_str)
                                    .and_then(|s| Decimal::from_str(s).ok());

                                positions.push(AccountPosition {
                                    symbol: symbol.to_string(),
                                    position_amt,
                                    entry_price,
                                    leverage,
                                    unrealized_pnl,
                                });
                            }
                        }
                    }
                }

            // Parse balances (a field -> B array)
                if let Some(balances_data) = value.get("a").and_then(|v| v.get("B")) {
                    if let Some(balances_array) = balances_data.as_array() {
                        for bal_data in balances_array {
                            if let (Some(asset), Some(available)) = (
                                bal_data.get("a").and_then(Value::as_str),
                                bal_data.get("wb").and_then(Value::as_str), // wallet balance
                            ) {
                                let available_balance = Decimal::from_str(available).unwrap_or(Decimal::ZERO);

                            // Only track USDT and USDC
                                if asset == "USDT" || asset == "USDC" {
                                    balances.push(AccountBalance {
                                        asset: asset.to_string(),
                                        available_balance,
                                    });
                                }
                            }
                        }
                    }
                }

                if !positions.is_empty() || !balances.is_empty() {
                    return Ok(Some(UserEvent::AccountUpdate { positions, balances }));
                }
            }

            _ => {}
        }
        Ok(Some(UserEvent::Heartbeat))
    }
}
}
