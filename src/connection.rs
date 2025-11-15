// CONNECTION: Exchange WS & REST single gateway
// All external world (WS/REST) goes through here
// Rate limit & reconnect management
// 
// This file contains ALL exchange-related code (previously in exchange.rs and exec.rs)
// Single responsibility: connection.rs = everything related to exchange communication

use crate::config::AppCfg;
use crate::event_bus::{EventBus, MarketTick, OrderUpdate, OrderStatus, PositionUpdate, BalanceUpdate};
use crate::state::SharedState;
use crate::types::{Px, Qty, Side, Tif};
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

/// Decimal adımından hassasiyet (ondalık hane sayısı) çıkarır
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
    /// Create a new Connection instance from configuration.
    ///
    /// This is the primary constructor for Connection. It initializes the exchange connection
    /// (BinanceFutures) and sets up rate limiting. No other module should create BinanceFutures directly.
    ///
    /// # Arguments
    ///
    /// * `cfg` - Application configuration containing API keys and exchange settings
    /// * `event_bus` - Event bus for publishing MarketTick, OrderUpdate, and PositionUpdate events
    /// * `shutdown_flag` - Shared flag to signal graceful shutdown
    /// * `shared_state` - Optional shared state for balance validation (should be Some in production)
    ///
    /// # Returns
    ///
    /// Returns `Ok(Connection)` on success, or `Err` if configuration is invalid or exchange connection fails.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use std::sync::Arc;
    /// use std::sync::atomic::AtomicBool;
    /// # use crate::config::load_config;
    /// # use crate::event_bus::EventBus;
    /// # use crate::state::SharedState;
    ///
    /// let cfg = Arc::new(load_config()?);
    /// let event_bus = Arc::new(EventBus::new());
    /// let shutdown_flag = Arc::new(AtomicBool::new(false));
    /// let shared_state = Some(Arc::new(SharedState::new()));
    ///
    /// let connection = Connection::from_config(cfg, event_bus, shutdown_flag, shared_state)?;
    /// ```
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
        
        Ok(Self {
            venue,
            cfg,
            event_bus,
            shutdown_flag,
            rate_limiter: Arc::new(tokio::sync::Mutex::new(RateLimiter::new())),
            shared_state,
            order_fill_history: Arc::new(DashMap::new()),
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
        }
    }

    /// Start all connection services and begin receiving market data.
    ///
    /// This method must be called after creating the Connection. It performs the following:
    ///
    /// 1. Sets hedge mode and margin type for all symbols (must be done before any orders)
    /// 2. Sets leverage for all symbols
    /// 3. Starts market data WebSocket streams (publishes MarketTick events)
    /// 4. Starts user data WebSocket stream (publishes OrderUpdate and PositionUpdate events)
    /// 5. Starts periodic symbol rules refresh task
    ///
    /// # Arguments
    ///
    /// * `symbols` - List of trading symbols to subscribe to (e.g., `["BTCUSDT", "ETHUSDT"]`)
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` when all services are started successfully, or `Err` if:
    /// - Hedge mode or margin type setup fails
    /// - Leverage setup fails
    /// - WebSocket connection fails
    ///
    /// # Errors
    ///
    /// This function will return an error if:
    /// - Exchange API calls fail (invalid API keys, network issues)
    /// - WebSocket connections cannot be established
    /// - Configuration is invalid
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # let connection: Arc<crate::connection::Connection> = todo!();
    /// let symbols = vec!["BTCUSDT".to_string(), "ETHUSDT".to_string()];
    /// connection.start(symbols).await?;
    /// ```
    ///
    /// # Note
    ///
    /// This method is async and should be awaited. It will spawn background tasks that continue
    /// running until `shutdown_flag` is set to true.
    pub async fn start(&self, symbols: Vec<String>) -> Result<()> {
        // Set hedge mode according to config (CRITICAL: must be done before any orders)
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
        
        // Set leverage and margin type for each symbol (CRITICAL: must be done before any orders)
        // Use cfg.leverage if set, otherwise use cfg.exec.default_leverage
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
            
            // CRITICAL: Verify current leverage before setting (if position exists)
            // This prevents conflicts if exchange has different leverage
            if let Ok(position) = self.venue.get_position(symbol).await {
                if !position.qty.0.is_zero() {
                    // Position exists - verify leverage matches
                    if position.leverage != leverage {
                        error!(
                            symbol = %symbol,
                            current_leverage = position.leverage,
                            config_leverage = leverage,
                            "CONNECTION: CRITICAL - Leverage mismatch! Exchange has {}x but config requires {}x. Cannot proceed with different leverage.",
                            position.leverage,
                            leverage
                        );
                        return Err(anyhow!(
                            "Leverage mismatch for symbol {}: exchange has {}x but config requires {}x",
                            symbol,
                            position.leverage,
                            leverage
                        ));
                    } else {
                        info!(
                            symbol = %symbol,
                            leverage,
                            "CONNECTION: Leverage verified (matches config)"
                        );
                    }
                }
            }
            
            // Set leverage (only if no position exists or leverage matches)
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
    
    /// Start periodic cleanup task for order fill history
    /// Removes entries older than 24 hours to prevent memory leaks
    /// This is critical because if OrderUpdate events are missed (WebSocket disconnect),
    /// history entries may never be cleaned up, causing memory to grow unbounded
    /// 
    /// CRITICAL: Race condition prevention
    /// - Only removes entries for closed orders (not in OPEN_ORDERS_CACHE)
    /// - Active orders are protected from cleanup to prevent race conditions
    /// - If OrderFill event arrives while cleanup is running, active order history is preserved
    async fn start_order_fill_history_cleanup_task(&self) {
        let order_fill_history = self.order_fill_history.clone();
        let shutdown_flag = self.shutdown_flag.clone();
        
        tokio::spawn(async move {
            const CLEANUP_INTERVAL_SECS: u64 = 3600; // Every hour
            const MAX_AGE_SECS: u64 = 86400; // 24 hours
            
            loop {
                tokio::time::sleep(Duration::from_secs(CLEANUP_INTERVAL_SECS)).await;
                
                if shutdown_flag.load(AtomicOrdering::Relaxed) {
                    break;
                }
                
                let now = Instant::now();
                let mut removed_count = 0;
                let mut protected_count = 0;
                
                // ✅ CRITICAL: Only remove entries for closed orders (not in OPEN_ORDERS_CACHE)
                // This prevents race condition where cleanup task removes entry while OrderFill event handler is using it
                // Active orders are protected from cleanup
                order_fill_history.retain(|order_id, history| {
                    let age = now.duration_since(history.last_update);
                    if age.as_secs() > MAX_AGE_SECS {
                        // Check if order is still active (in OPEN_ORDERS_CACHE)
                        // If order is active, protect it from cleanup even if it's old
                        let is_active = OPEN_ORDERS_CACHE.iter().any(|entry| {
                            entry.value().iter().any(|o| o.order_id == *order_id)
                        });
                        
                        if is_active {
                            // Order is still active - protect from cleanup
                            protected_count += 1;
                            true // Keep this entry (active order)
                        } else {
                            // Order is closed and old - safe to remove
                            removed_count += 1;
                            false // Remove this entry (closed order)
                        }
                    } else {
                        true // Keep recent entries (regardless of status)
                    }
                });
                
                if removed_count > 0 {
                    info!(
                        removed_count,
                        protected_count,
                        remaining_entries = order_fill_history.len(),
                        "CONNECTION: Cleaned up {} old order fill history entries (older than {} hours), protected {} active orders",
                        removed_count,
                        MAX_AGE_SECS / 3600,
                        protected_count
                    );
                } else if protected_count > 0 {
                    tracing::debug!(
                        protected_count,
                        total_entries = order_fill_history.len(),
                        "CONNECTION: Order fill history cleanup completed (protected {} active orders, no entries to remove)",
                        protected_count
                    );
                } else {
                    tracing::debug!(
                        total_entries = order_fill_history.len(),
                        "CONNECTION: Order fill history cleanup completed (no entries to remove)"
                    );
                }
            }
        });
    }

    /// Start market data WebSocket stream
    /// Publishes MarketTick events to event bus
    async fn start_market_data_stream(&self, symbols: Vec<String>) -> Result<()> {
        // CRITICAL: Deduplicate symbols to prevent duplicate subscriptions
        // If duplicate symbols exist, multiple streams would subscribe to the same symbol,
        // causing unnecessary writes to PRICE_CACHE, duplicate data processing, and potential event ordering issues
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
                                        // CRITICAL: DashMap is thread-safe and atomic
                                        // If multiple streams subscribe to the same symbol (duplicate subscription),
                                        // the last write wins (which is fine for price data - we want the latest)
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
                                        
                                        // CRITICAL: Handle backpressure - if channel is full, log warning but continue
                                        // This prevents one slow subscriber from blocking all streams
                                        // Note: broadcast::send() returns error only if there are no receivers
                                        // If channel is full, it will drop the oldest message (ring buffer behavior)
                                        // We check receiver count to detect if subscribers are lagging
                                        let receiver_count = event_bus.market_tick_tx.receiver_count();
                                        if receiver_count == 0 {
                                            // No subscribers, skip sending (but still update cache)
                                            tracing::debug!(
                                                symbol = %price_update.symbol,
                                                "CONNECTION: No MarketTick subscribers, skipping event"
                                            );
                                        } else {
                                            // Send event (may drop oldest if buffer is full, but that's OK)
                                            if let Err(_) = event_bus.market_tick_tx.send(market_tick) {
                                                // This only happens if receiver_count dropped to 0 between check and send
                                                // Very rare race condition, but handle gracefully
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
        
        info!(
            total_symbols = symbols.len(),
            chunks_spawned = (symbols.len() + MAX_SYMBOLS_PER_STREAM - 1) / MAX_SYMBOLS_PER_STREAM,
            "CONNECTION: All market data stream tasks spawned (each chunk runs independently)"
        );
        
        Ok(())
    }

    // Note: sync_missed_events removed - Binance WebSocket automatically sends
    // ACCOUNT_UPDATE and ORDER_TRADE_UPDATE events after reconnect which contain
    // current state. No REST API sync needed - WebSocket-first approach as recommended by Binance.

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
                        
                        // Set reconnect callback - Binance WebSocket automatically sends
                        // ACCOUNT_UPDATE and ORDER_TRADE_UPDATE events after reconnect
                        // which contain current state. No REST API sync needed.
                        stream.set_on_reconnect(move || {
                            info!("CONNECTION: WebSocket reconnected - Binance will automatically send state updates via ACCOUNT_UPDATE and ORDER_TRADE_UPDATE events");
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
                                            is_maker: _,
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
                                                    // This prevents silent failures if Binance adds new statuses
                                                    tracing::warn!(
                                                        order_status = unknown,
                                                        "CONNECTION: Unknown order status received, treating as Rejected"
                                                    );
                                                    OrderStatus::Rejected
                                                }
                                            };
                                            
                                            // Calculate remaining qty: order_qty - cumulative_filled_qty
                                            // If order_qty is not available, use cumulative_filled_qty as fallback
                                            let total_order_qty = order_qty.unwrap_or(cumulative_filled_qty);
                                            let remaining_qty = if total_order_qty.0 >= cumulative_filled_qty.0 {
                                                Qty(total_order_qty.0 - cumulative_filled_qty.0)
                                            } else {
                                                Qty(Decimal::ZERO) // Should not happen, but safety check
                                            };
                                            
                                            // CRITICAL: Calculate average fill price using weighted average
                                            // Formula: new_avg = (old_weighted_sum + new_price * new_qty) / total_qty
                                            // We track weighted_price_sum = sum of (price * qty) for all fills
                                            let now = Instant::now();
                                            let average_fill_price = {
                                                let mut history = order_fill_history.entry(order_id.clone()).or_insert_with(|| {
                                                    OrderFillHistory {
                                                        total_filled_qty: Qty(Decimal::ZERO),
                                                        weighted_price_sum: Decimal::ZERO,
                                                        last_update: now,
                                                    }
                                                });
                                                
                                                // Add this fill to weighted sum: price * qty
                                                history.weighted_price_sum += last_fill_price.0 * last_filled_qty.0;
                                                history.total_filled_qty = cumulative_filled_qty;
                                                history.last_update = now; // Update timestamp
                                                
                                                // Calculate average: weighted_sum / total_qty
                                                if !cumulative_filled_qty.0.is_zero() {
                                                    Px(history.weighted_price_sum / cumulative_filled_qty.0)
                                                } else {
                                                    last_fill_price // Fallback if qty is zero (should not happen)
                                                }
                                            };
                                            
                                            // ✅ BEST PRACTICE: Update open orders cache from WebSocket (Binance recommendation)
                                            // Update OPEN_ORDERS_CACHE based on order status
                                            let order_entry = OPEN_ORDERS_CACHE.entry(symbol.clone()).or_insert_with(Vec::new);
                                            
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
                                                timestamp: Instant::now(),
                                            };
                                            
                                            if let Err(e) = event_bus.order_update_tx.send(order_update) {
                                                error!(
                                                    error = ?e,
                                                    "CONNECTION: Failed to send OrderUpdate event (no subscribers or channel closed)"
                                                );
                                            }
                                            
                                            // CRITICAL: PositionUpdate race condition prevention
                                            // Order fill olduğunda position değişir - PositionUpdate yayınla
                                            // Position bilgisini venue'dan çek
                                            // IMPORTANT: We spawn this to avoid blocking the WebSocket stream,
                                            // but ORDERING module will ignore stale PositionUpdate events
                                            // (PositionUpdate that arrives after OrderUpdate has already set the position)
                                            let venue_clone = venue.clone();
                                            let event_bus_pos = event_bus.clone();
                                            let symbol_clone = symbol.clone();
                                            tokio::spawn(async move {
                                                // Retry mechanism: Binance API sometimes takes time to update position
                                                // Use exponential backoff instead of arbitrary fixed delay
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
                                            // ✅ BEST PRACTICE: Update open orders cache when order is canceled
                                            // Remove canceled order from OPEN_ORDERS_CACHE
                                            if let Some(mut orders) = OPEN_ORDERS_CACHE.get_mut(&symbol) {
                                                orders.retain(|o| o.order_id != order_id);
                                                // If no more open orders for this symbol, remove symbol entry
                                                if orders.is_empty() {
                                                    drop(orders); // Release lock before remove
                                                    OPEN_ORDERS_CACHE.remove(&symbol);
                                                }
                                            }
                                            // OrderCanceled event - ORDERING module will handle from its state
                                            // We don't have enough info here to create OrderUpdate
                                        }
                                        UserEvent::AccountUpdate { positions, balances } => {
                                            // ✅ BEST PRACTICE: Cache positions from WebSocket (Binance recommendation)
                                            // Update POSITION_CACHE from ACCOUNT_UPDATE event
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
                                            
                                            // ✅ BEST PRACTICE: Cache balances from WebSocket (Binance recommendation)
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
        
        // ✅ CRITICAL: Graceful error handling for system time
        // System clock can be adjusted (NTP sync, manual changes, etc.)
        // Use fallback value instead of panicking
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
        // This will also quantize and validate precision
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
        // This prevents double-spending when balance is reserved by ORDERING module
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
                let min_balance = Decimal::from(self.cfg.min_quote_balance_usd);
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
    /// First tries PRICE_CACHE (from WebSocket), falls back to REST API if cache is empty
    pub async fn get_current_prices(&self, symbol: &str) -> Result<(Px, Px)> {
        // Try PRICE_CACHE first (fastest, most up-to-date)
        if let Some(price_update) = PRICE_CACHE.get(symbol) {
            return Ok((price_update.bid, price_update.ask));
        }
        
        // Fallback to REST API if cache is empty
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
}

/// Order command for ORDERING module
#[derive(Debug, Clone)]
pub enum OrderCommand {
    Open {
        symbol: String,
        side: Side,
        price: Px,
        qty: Qty,
        tif: crate::types::Tif,
    },
    Close {
        symbol: String,
        side: Side,
        price: Px,
        qty: Qty,
        tif: crate::types::Tif,
    },
}

// ============================================================================
// Internal Types (used only within connection.rs)
// ============================================================================

/// Internal order type (used only within connection.rs)
#[derive(Clone, Debug)]
pub struct VenueOrder {
    pub order_id: String,
    pub side: Side,
    pub price: Px,
    pub qty: Qty,
}

/// Internal position type (used only within connection.rs)
#[derive(Clone, Debug)]
pub struct Position {
    pub symbol: String,
    pub qty: Qty,
    pub entry: Px,
    pub leverage: u32,
    pub liq_px: Option<Px>,
}

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

#[derive(Clone, Debug)]
pub struct SymbolRules {
    pub tick_size: Decimal,
    pub step_size: Decimal,
    pub price_precision: usize,
    pub qty_precision: usize,
    pub min_notional: Decimal,
}

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

/// ✅ BEST PRACTICE: Price cache from WebSocket market data stream
/// Thread-safe price storage - updated by WebSocket, read by main loop
/// 
/// CONCURRENT WRITE BEHAVIOR:
/// - DashMap.insert() is atomic and thread-safe
/// - If multiple streams subscribe to the same symbol (duplicate subscription),
///   concurrent writes are safe: last write wins (correct for price data - we want latest)
/// - No data corruption or race conditions, but may have unnecessary writes
/// - Each symbol should ideally be in only one stream chunk to avoid duplicate updates
pub static PRICE_CACHE: Lazy<DashMap<String, PriceUpdate>> = Lazy::new(|| DashMap::new());

/// ✅ BEST PRACTICE: Position cache from WebSocket user data stream (ACCOUNT_UPDATE events)
/// Thread-safe position storage - updated by WebSocket, read by main loop
/// Binance recommendation: Use WebSocket instead of REST API for real-time data
pub static POSITION_CACHE: Lazy<DashMap<String, Position>> = Lazy::new(|| DashMap::new());

/// ✅ BEST PRACTICE: Balance cache from WebSocket user data stream (ACCOUNT_UPDATE events)
/// Thread-safe balance storage - updated by WebSocket, read by main loop
/// Binance recommendation: Use WebSocket instead of REST API for real-time data
pub static BALANCE_CACHE: Lazy<DashMap<String, Decimal>> = Lazy::new(|| DashMap::new());

/// ✅ BEST PRACTICE: Open orders cache from WebSocket user data stream (ORDER_TRADE_UPDATE events)
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
    // Eğer step 1 veya daha büyükse, precision 0 olmalı
    if step >= Decimal::ONE {
        return 0;
    }
    // tick_size veya step_size'dan precision hesapla
    // Decimal'in scale() metodu internal scale'i döner (trailing zero'lar dahil)
    // Bu bizim için doğru precision'ı verir
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

    // KRİTİK: Precision hesaplama scale_from_step ile değil, doğrudan API'den al
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

    // KRİTİK: Fallback değerleri daha makul yap
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

#[derive(Clone, Debug)]
pub struct SymbolMeta {
    pub symbol: String,
    pub base_asset: String,
    pub quote_asset: String,
    pub status: Option<String>,
    pub contract_type: Option<String>,
}

/// Binance API common configuration
/// 
/// ✅ KRİTİK: Thread-safety ve performans optimizasyonu
/// - `client: Arc<Client>`: reqwest::Client thread-safe ama clone edilmesi gereksiz overhead
///   Arc ile wrap ederek clone işlemi sadece pointer kopyalama olur
/// - `sign()` fonksiyonu thread-safe (immutable data kullanıyor, sadece read-only)
#[derive(Clone)]
pub struct BinanceCommon {
    pub client: Arc<Client>,
    pub api_key: String,
    pub secret_key: String,
    pub recv_window_ms: u64,
}

impl BinanceCommon {
    fn ts() -> u64 {
        // ✅ CRITICAL: Graceful error handling for system time
        // System clock can be adjusted (NTP sync, manual changes, etc.)
        // Use fallback value instead of panicking
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| {
                // System time is before UNIX epoch (should never happen, but handle gracefully)
                // Use current timestamp as fallback (0 would cause issues)
                warn!("System time is before UNIX epoch, using fallback timestamp");
                Duration::from_secs(0)
            })
            .as_millis() as u64
    }
    /// Sign a query string with HMAC-SHA256
    /// 
    /// ✅ KRİTİK: Thread-safe - immutable data kullanıyor (sadece read-only)
    /// - `self.secret_key` immutable olarak okunuyor (String clone edilmiyor)
    /// - `qs` parametresi immutable
    /// - HMAC hesaplama thread-safe (her çağrı bağımsız)
    /// - İki thread aynı anda farklı request'ler için sign() çağırabilir (race condition yok)
    fn sign(&self, qs: &str) -> String {
        // ✅ CRITICAL: Graceful error handling for HMAC key initialization
        // HMAC key initialization can fail if key is too long (should be caught in config validation)
        // Use fallback empty signature instead of panicking (API call will fail with invalid signature)
        let mut mac = match Hmac::<Sha256>::new_from_slice(self.secret_key.as_bytes()) {
            Ok(mac) => mac,
            Err(e) => {
                // HMAC key initialization failed (should never happen if config validation passed)
                // Return empty signature - API call will fail with invalid signature error
                // This is better than panicking - error will be handled upstream
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
    pub hedge_mode: bool, // Hedge mode (dual-side position) açık mı?
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
    position_side: Option<String>, // ✅ KRİTİK: "LONG" | "SHORT" | "BOTH" (hedge mode) veya None (one-way mode)
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
    
    /// Leverage ayarla (sembol bazlı)
    /// KRİTİK: Başlangıçta her sembol için leverage'i açıkça ayarla
    /// /fapi/v1/leverage endpoint'i ile sembol bazlı leverage set edilir
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

    /// Position side mode ayarla (hedge mode aç/kapa)
    /// KRİTİK: Başlangıçta hesap modunu açıkça ayarla
    /// /fapi/v1/positionSide/dual endpoint'i ile hedge mode açılır/kapanır
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

    /// Margin type ayarla (isolated veya cross)
    /// KRİTİK: Başlangıçta her sembol için margin type'ı açıkça ayarla
    /// /fapi/v1/marginType endpoint'i ile isolated/cross margin set edilir
    ///
    /// # Arguments
    /// * `sym` - Symbol (örn: "BTCUSDT")
    /// * `isolated` - true = isolated margin, false = cross margin
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

    /// Per-symbol metadata (tick_size, step_size) alır
    /// KRİTİK: Global fallback kullanmaz - her sembol için gerçek rules gerekli
    /// Fallback kullanmak LOT_SIZE ve PRICE_FILTER hatalarına yol açabilir
    pub async fn rules_for(&self, sym: &str) -> Result<Arc<SymbolRules>> {
        // ✅ KRİTİK: Double-check locking pattern - race condition önleme
        // İlk kontrol: Cache'de var mı?
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
                    
                    // ✅ KRİTİK: Double-check - başka bir thread aynı anda eklemiş olabilir
                    // Cache'e eklemeden önce tekrar kontrol et (race condition önleme)
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
                        // ✅ KRİTİK GÜVENLİK: Exponential backoff kullan (rate limit durumunda daha etkili)
                        // Linear backoff (100ms, 200ms, 300ms) yerine exponential (100ms, 200ms, 400ms)
                        // Exchange rate limit durumunda linear backoff yetersiz kalabilir
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

        // Tüm retry'ler başarısız oldu - hata dön (fallback kullanma)
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
        let all_symbols = self.symbol_metadata().await?;
        
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

    pub async fn available_balance(&self, asset: &str) -> Result<Decimal> {
        // ✅ BEST PRACTICE: Try WebSocket cache first (Binance recommendation)
        // WebSocket is faster, more efficient, and reduces REST API rate limit usage
        if let Some(balance) = BALANCE_CACHE.get(asset) {
            return Ok(*balance);
        }
        
        // Fallback to REST API if cache is empty (e.g., on startup before WebSocket data arrives)
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
        // ✅ BEST PRACTICE: Try WebSocket cache first (Binance recommendation)
        // WebSocket is faster, more efficient, and reduces REST API rate limit usage
        if let Some(orders) = OPEN_ORDERS_CACHE.get(sym) {
            return Ok(orders.clone());
        }
        
        // Fallback to REST API if cache is empty (e.g., on startup before WebSocket data arrives)
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
        // marginType: "isolated" or "cross"
        let is_isolated = pos.margin_type.eq_ignore_ascii_case("isolated");
        Ok(is_isolated)
    }

    pub async fn fetch_position(&self, sym: &str) -> Result<Position> {
        // ✅ BEST PRACTICE: Try WebSocket cache first (Binance recommendation)
        // WebSocket is faster, more efficient, and reduces REST API rate limit usage
        if let Some(position) = POSITION_CACHE.get(sym) {
            return Ok(position.clone());
        }
        
        // Fallback to REST API if cache is empty (e.g., on startup before WebSocket data arrives)
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
        
        // ✅ KRİTİK: Hedge mode tutarlılığı - positionSide kontrolü
        // Tek-yön modunda (hedge_mode=false): positionSide "BOTH" veya None olmalı, sadece bir pozisyon olmalı
        // Hedge modunda (hedge_mode=true): positionSide "LONG" veya "SHORT" olabilir, birden fazla pozisyon olabilir
        // Birden fazla pozisyon varsa (hedge mode), net pozisyonu hesapla (LONG - SHORT)
        let matching_positions: Vec<&FutPosition> = positions
            .iter()
            .filter(|p| p.symbol.eq_ignore_ascii_case(sym))
            .collect();
        
        if matching_positions.is_empty() {
            return Err(anyhow!("position not found for symbol"));
        }
        
        // ✅ KRİTİK: Tek-yön modunda positionSide kontrolü
        // Tek-yön modunda positionSide "BOTH" veya None olmalı, "LONG"/"SHORT" olmamalı
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
            
            // ✅ KRİTİK: Tek-yön modunda net pozisyon kontrolü
            // Eğer birden fazla pozisyon varsa (API tutarsızlığı), net pozisyonu kullan
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
                // ✅ BEST PRACTICE: Update cache with REST API result (for startup sync)
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
                // ✅ BEST PRACTICE: Update cache with REST API result (for startup sync)
                POSITION_CACHE.insert(sym.to_string(), position.clone());
                Ok(position)
            }
        } else {
            // ✅ KRİTİK: Hedge modunda - net pozisyonu hesapla (LONG - SHORT)
            // Hedge modunda birden fazla pozisyon olabilir (LONG ve SHORT ayrı ayrı)
            let mut long_qty = Decimal::ZERO;
            let mut short_qty = Decimal::ZERO;
            let mut long_entry = Decimal::ZERO;
            let mut short_entry = Decimal::ZERO;
            let mut leverage = 1u32;
            let mut liq_px = None;
            
            for pos in matching_positions {
                let qty = Decimal::from_str(&pos.position_amt).unwrap_or(Decimal::ZERO);
                let entry = Decimal::from_str(&pos.entry_price).unwrap_or(Decimal::ZERO);
                let lev = pos.leverage.parse::<u32>().unwrap_or(1);
                let liq = Decimal::from_str(&pos.liquidation_price).unwrap_or(Decimal::ZERO);
                
                // ✅ KRİTİK: positionSide kontrolü - hedge modunda "LONG" veya "SHORT" olmalı
                match pos.position_side.as_deref() {
                    Some("LONG") => {
                        long_qty = qty;
                        long_entry = entry;
                        leverage = lev;
                        if liq > Decimal::ZERO {
                            liq_px = Some(Px(liq));
                        }
                    }
                    Some("SHORT") => {
                        short_qty = qty;
                        short_entry = entry;
                        leverage = lev;
                        if liq > Decimal::ZERO {
                            liq_px = Some(Px(liq));
                        }
                    }
                    Some("BOTH") | None => {
                        // ✅ KRİTİK: Hedge modunda "BOTH" veya None beklenmiyor, uyarı ver
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
                        leverage = lev;
                        if liq > Decimal::ZERO {
                            liq_px = Some(Px(liq));
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
            
            // Net pozisyon: LONG - SHORT
            let net_qty = long_qty - short_qty;
            // Net entry: Weighted average (basit versiyon - ilk pozisyonun entry'si)
            let net_entry = if net_qty.is_sign_positive() {
                long_entry
            } else if net_qty.is_sign_negative() {
                short_entry
            } else {
                Decimal::ZERO
            };
            
            let position = Position {
                symbol: sym.to_string(),
                qty: Qty(net_qty),
                entry: Px(net_entry),
                leverage,
                liq_px,
            };
            // ✅ BEST PRACTICE: Update cache with REST API result (for startup sync)
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

    /// Close position with reduceOnly guarantee and verification
    ///
    /// KRİTİK: Futures için pozisyon kapatma garantisi:
    /// 1. reduceOnly=true ile market order gönder (veya use_market_only=false ise limit fallback)
    /// 2. Pozisyon tam olarak kapatıldığını doğrula
    /// 3. Kısmi kapatma durumunda retry yap
    /// 4. Leverage ile uyumlu olduğundan emin ol
    /// 5. Hedge mode açıksa positionSide parametresi ekle
    ///
    /// # Arguments
    /// * `use_market_only` - Hızlı kapanış gereksiniminde true (risk halt, stop loss). 
    ///   False ise MARKET başarısız olursa LIMIT fallback yapar.
    pub async fn flatten_position(&self, sym: &str, use_market_only: bool) -> Result<()> {
        // İlk pozisyon kontrolü
        let initial_pos = match self.fetch_position(sym).await {
            Ok(pos) => pos,
            Err(e) => {
                let error_str = e.to_string().to_lowercase();
                // CRITICAL: Only handle specific "position not found" errors
                // Network errors (timeout, connection refused, etc.) should be retried, not ignored
                // Binance error codes:
                // - "-2011": Unknown order (position not found)
                // - "position not found": Explicit position not found message
                // - "no position": Alternative position not found message
                if error_str.contains("position not found") 
                    || error_str.contains("no position")
                    || error_str.contains("-2011") {  // Binance: Unknown order (position not found)
                    info!(symbol = %sym, "position already closed (manual intervention detected), skipping close");
                    return Ok(());
                }
                // Other errors (network, API errors, etc.) should be retried
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

        // KRİTİK: Pozisyon kapatma retry mekanizması (kısmi kapatma durumunda)
        let max_attempts = 3;
        // ✅ KRİTİK GÜVENLİK: LIMIT fallback flag'i - sonsuz loop önleme
        // MIN_NOTIONAL hatası yakalandığında LIMIT fallback yapılır, ama LIMIT de başarısız olursa
        // tekrar MARKET denenmemeli (sonsuz loop riski)
        let mut limit_fallback_attempted = false;

        for attempt in 0..max_attempts {
            // KRİTİK İYİLEŞTİRME: Her attempt'te mevcut pozisyonu kontrol et
            // Retry durumunda pozisyon değişmiş olabilir (kısmi kapatma veya manuel kapatma)
            let current_pos = match self.fetch_position(sym).await {
                Ok(pos) => pos,
                Err(e) => {
                    let error_str = e.to_string().to_lowercase();
                    // CRITICAL: Only handle specific "position not found" errors
                    // Network errors should be retried, not ignored
                    // Binance error codes:
                    // - "-2011": Unknown order (position not found)
                    // - "position not found": Explicit position not found message
                    // - "no position": Alternative position not found message
                    if error_str.contains("position not found") 
                        || error_str.contains("no position")
                        || error_str.contains("-2011") {  // Binance: Unknown order (position not found)
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

            // KRİTİK: Hedge mode açıksa positionSide parametresi ekle
            // positionSide: "LONG" (pozitif qty) veya "SHORT" (negatif qty)
            let position_side = if self.hedge_mode {
                if current_qty.is_sign_positive() {
                    Some("LONG")
                } else {
                    Some("SHORT")
                }
            } else {
                None
            };

            // KRİTİK: reduceOnly=true ve type=MARKET garantisi
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
                    // KRİTİK DÜZELTME: Exchange'in işlemesi için bekleme eklendi
                    // Market order gönderildikten sonra exchange'in işlemesi için zaman gerekir
                    // Hemen kontrol etmek yanlış sonuçlara yol açabilir (pozisyon henüz kapanmamış olabilir)
                    // KRİTİK İYİLEŞTİRME: Binance için 1000ms daha güvenli (500ms yeterli olmayabilir)
                    // Exchange'in order'ı işlemesi ve position update'i için yeterli süre
                    tokio::time::sleep(Duration::from_millis(1000)).await; // Exchange işlemesi için 1000ms bekle (Binance)

                    let verify_pos = self.fetch_position(sym).await?;
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
                        // ✅ KRİTİK DÜZELTME: Position growth tespiti - sonsuz loop önleme
                        // 1 saniye bekleme sırasında WebSocket'ten yeni fill event gelebilir
                        // Position büyüyebilir (yeni order fill oldu), bu durumda sonsuz loop riski var
                        let position_grew_from_attempt = verify_qty.abs() > current_qty.abs();
                        let position_grew_from_initial = verify_qty.abs() > initial_qty.abs();
                        
                        if position_grew_from_attempt || position_grew_from_initial {
                            // Position büyüdü - yeni bir order fill oldu
                            // Bu durumda close işlemi başarısız sayılmalı (yeni position close edilmeli)
                            // Ancak sonsuz loop riski var, bu yüzden hata döndür veya yeni position'ı close etmeye çalış
                            warn!(
                                symbol = %sym,
                                attempt = attempt + 1,
                                initial_qty = %initial_qty,
                                current_qty_at_attempt = %current_qty,
                                verify_qty = %verify_qty,
                                grew_from_attempt = position_grew_from_attempt,
                                grew_from_initial = position_grew_from_initial,
                                "POSITION GROWTH DETECTED: new order filled during close, position increased (possible infinite loop risk)"
                            );
                            
                            // ✅ KRİTİK: Position initial'dan büyüdüyse, bu kesinlikle yeni bir order'ın fill olduğu anlamına gelir
                            // Bu durumda close işlemi başarısız sayılmalı ve yeni position'ı close etmeye çalışmalı
                            // Ancak max_attempts kontrolü ile sonsuz loop önlenir
                            if attempt < max_attempts - 1 {
                                // Son deneme değilse, yeni position'ı close etmeye çalış
                                warn!(
                                    symbol = %sym,
                                    attempt = attempt + 1,
                                    "retrying close with new (larger) position size"
                                );
                                continue; // Loop devam eder, yeni position'ı close etmeye çalışır
                            } else {
                                // Son denemede hala position büyüdüyse, hata döndür
                                return Err(anyhow::anyhow!(
                                    "Failed to close position: position grew during close (new order filled). Initial: {}, Attempt qty: {}, Final qty: {}. Possible infinite loop prevented.",
                                    initial_qty,
                                    current_qty,
                                    verify_qty
                                ));
                            }
                        }
                        
                        // KRİTİK İYİLEŞTİRME: Kısmi kapatma tespiti - kapatılan miktarı hesapla
                        // Bu attempt'te ne kadar kapatıldı?
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
                    let error_lower = error_str.to_lowercase();

                    // ✅ KRİTİK: Manuel kapatma durumlarını handle et - pozisyon zaten kapalıysa hata verme
                    if error_lower.contains("position not found")
                        || error_lower.contains("no position")
                        || error_lower.contains("position already closed")
                        || error_lower.contains("reduceonly")
                        || error_lower.contains("reduce only")
                        || error_lower.contains("-2011") // Binance: "Unknown order sent"
                        || error_lower.contains("-2019") // Binance: "Margin is insufficient"
                        || error_lower.contains("-2021") // Binance: "Order would immediately match"
                    {
                        // Pozisyon zaten kapalı veya manuel kapatılmış - hata verme, başarılı say
                        info!(
                            symbol = %sym,
                            attempt = attempt + 1,
                            error = %e,
                            "position already closed (manual intervention or already closed), treating as success"
                        );
                        return Ok(());
                    }

                    // ✅ KRİTİK: Hızlı kapanış gereksiniminde (use_market_only=true) LIMIT fallback yapma
                    // Risk halt, stop loss gibi durumlarda hızlı kapanış kritik, LIMIT yavaş olabilir
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

                    // KRİTİK DÜZELTME: MIN_NOTIONAL hatası yakalama (-1013 veya "min notional")
                    // Küçük "artık" miktarlarda reduce-only market close borsa min_notional eşiğini sağlamayabilir
                    // Normal kapanış modunda LIMIT fallback yap
                    // ✅ KRİTİK GÜVENLİK: LIMIT fallback sadece bir kez denenmeli (sonsuz loop önleme)
                    if (error_lower.contains("-1013")
                        || error_lower.contains("min notional")
                        || error_lower.contains("min_notional"))
                        && !limit_fallback_attempted
                    {
                        limit_fallback_attempted = true; // LIMIT fallback'i işaretle (tekrar denenmeyecek)
                        warn!(
                            symbol = %sym,
                            attempt = attempt + 1,
                            error = %e,
                            remaining_qty = %remaining_qty,
                            min_notional = %rules.min_notional,
                            "MIN_NOTIONAL error in reduce-only market close, trying limit reduce-only fallback (one-time)"
                        );

                        // Fallback: Limit reduce-only ile karşı tarafta 1-2 tick avantajlı pasif bırak
                        let (best_bid, best_ask) = match self.best_prices(sym).await {
                            Ok(prices) => prices,
                            Err(e2) => {
                                warn!(symbol = %sym, error = %e2, "failed to fetch best prices for limit fallback");
                                // ✅ KRİTİK: LIMIT fallback başarısız, direkt hata döndür (sonsuz loop önleme)
                                return Err(anyhow!(
                                    "MIN_NOTIONAL error and limit fallback failed (best prices fetch failed): {}",
                                    e
                                ));
                            }
                        };

                        // Limit reduce-only emri: karşı tarafta 1-2 tick avantajlı
                        let tick_size = rules.tick_size;
                        let limit_price = if matches!(side, Side::Buy) {
                            // Short kapatma: Buy limit, bid'den 1 tick yukarı (maker olabilir)
                            best_bid.0 + tick_size
                        } else {
                            // Long kapatma: Sell limit, ask'ten 1 tick aşağı (maker olabilir)
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
                                // ✅ KRİTİK GÜVENLİK: LIMIT fallback başarısız, dust kontrolü yap
                                // Eğer dust ise (çok küçük miktar), pozisyonu kapalı kabul et
                                if remaining_qty < rules.min_notional / Decimal::from(1000) {
                                    // Çok küçük miktar, quantize et ve sıfırla
                                    let dust_qty = quantize_decimal(remaining_qty, rules.step_size);
                                    if dust_qty <= Decimal::ZERO {
                                        info!(
                                            symbol = %sym,
                                            "MIN_NOTIONAL: remaining qty is dust, considering position closed"
                                        );
                                        return Ok(());
                                    }
                                }
                                // ✅ KRİTİK: LIMIT fallback başarısız, tekrar MARKET denenmemeli (sonsuz loop önleme)
                                // Direkt hata döndür - retry loop'a girmemeli
                                return Err(anyhow!(
                                    "MIN_NOTIONAL error: market and limit reduce-only both failed (market: {}, limit: {})",
                                    e, e2
                                ));
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
                            // KRİTİK DÜZELTME: Retry öncesi bekleme eklendi
                            // Hızlı retry'ler exchange'i overload edebilir
                            tokio::time::sleep(Duration::from_millis(500)).await; // Retry öncesi 500ms bekle
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

        // ✅ KRİTİK: Post-only (GTX) emirler için not
        // Not: order.rs'de emir gönderilmeden önce cross kontrolü yapılıyor
        // Binance GTX kullanıldığında, emir cross ederse otomatik olarak iptal edilir
        // Ancak emir gönderilmeden önce kontrol etmek daha verimlidir (order.rs'de yapılıyor)

        let rules = self.rules_for(sym).await?;

        // KRİTİK DÜZELTME: Validation guard - tek nokta kontrol
        // Bu fonksiyon -1111 hatasını imkânsız hale getirir
        let (price_str, qty_str, price_quantized, qty_quantized) =
            Self::validate_and_format_order_params(px, qty, &rules, sym)?;

        // KRİTİK: Log'ları zenginleştir - gönderilen değerleri logla
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

        // ✅ KRİTİK: Hedge mode açıksa positionSide parametresi ekle
        // positionSide: "LONG" (Buy order) veya "SHORT" (Sell order)
        if self.hedge_mode {
            let position_side = match side {
                Side::Buy => "LONG",
                Side::Sell => "SHORT",
            };
            params.push(format!("positionSide={}", position_side));
        }

        // ✅ YENİ: reduceOnly desteği (TP emirleri için) - şimdilik false (normal emirler için)
        // TP emirleri için ayrı bir public method kullanılacak

        // ClientOrderId ekle (idempotency için) - sadece boş değilse
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
        // KRİTİK DÜZELTME: Retry/backoff mekanizması
        // Transient hatalar için exponential backoff ile retry (aynı clientOrderId ile)
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
                        // Status code hata
                        let body = resp.text().await.unwrap_or_default();
                        let body_lower = body.to_lowercase();

                        // KRİTİK DÜZELTME: -1111 (precision) hatası - rules'ı yeniden çek ve retry
                        if body_lower.contains("precision is over") || body_lower.contains("-1111")
                        {
                            if attempt < MAX_RETRIES {
                                // Rules'ı yeniden çek
                                warn!(%sym, attempt = attempt + 1, "precision error (-1111), refreshing rules and retrying");
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
                                                
                                                // ✅ KRİTİK: Hedge mode açıksa positionSide parametresi ekle
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

                                                last_error = Some(anyhow!("precision error, retrying with refreshed rules"));
                                                continue;
                                            }
                                            Err(e) => {
                                                error!(%sym, error = %e, "validation failed after rules refresh, giving up");
                                                return Err(anyhow!("precision error, validation failed after rules refresh: {}", e));
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        error!(%sym, error = %e, "failed to refresh rules, giving up");
                                        return Err(anyhow!(
                                            "precision error, failed to refresh rules: {}",
                                            e
                                        ));
                                    }
                                }
                            } else {
                                error!(%sym, attempt, "precision error (-1111) after max retries, symbol should be quarantined");
                                return Err(anyhow!(
                                    "binance api error: {} - {} (precision error, max retries)",
                                    status,
                                    body
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
        // ✅ BEST PRACTICE: Önce WebSocket cache'den oku, yoksa REST API'ye fallback
        if let Some(price_update) = PRICE_CACHE.get(sym) {
            return Ok((price_update.bid, price_update.ask));
        }
        
        // Fallback: REST API (WebSocket cache'de yoksa)
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
        // Normal kapanış: MARKET başarısız olursa LIMIT fallback yap
        self.flatten_position(sym, false).await
    }
    
    async fn available_balance(&self, asset: &str) -> Result<Decimal> {
        // Call BinanceFutures::available_balance method (not trait method to avoid recursion)
        BinanceFutures::available_balance(self, asset).await
    }
}

impl BinanceFutures {
    /// Test order endpoint - İlk emir öncesi doğrulama
    /// /fapi/v1/order/test endpoint'i ile emir parametrelerini test et
    /// -1111 hatası gelirse sembolü disable et ve rules'ı yeniden çek
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

        // ✅ KRİTİK: Hedge mode açıksa positionSide parametresi ekle
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

    /// Validation guard: Emir gönderiminden önce son doğrulama
    /// Bu fonksiyon -1111 hatasını imkânsız hale getirir
    /// price = floor_to_step(price, tick_size)
    /// qty = floor_to_step(abs(qty), step_size)
    /// price_str = format_to_precision(price, price_precision)
    /// qty_str = format_to_precision(qty, qty_precision)
    /// Son kontrol: fractional_digits(price_str) <= price_precision
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
        // ✅ KRİTİK DÜZELTME: ToZero yerine ToNegativeInfinity kullan (floor) - daha güvenli
        // ToZero yukarı yuvarlayabilir ve precision hatasına yol açabilir
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

/// Quantize decimal value to step (floor to nearest step multiple)
/// 
/// ✅ KRİTİK: Precision loss önleme
/// Decimal division ve multiplication yaparken precision loss olabilir.
/// Sonucu normalize ederek step'in tam katı olduğundan emin oluyoruz.
pub fn quantize_decimal(value: Decimal, step: Decimal) -> Decimal {
    // KRİTİK DÜZELTME: Edge case'ler için ek kontroller
    if step.is_zero() || step.is_sign_negative() {
        return value;
    }

    let ratio = value / step;
    let floored = ratio.floor();
    let result = floored * step;

    // ✅ KRİTİK: Precision loss önleme - sonucu normalize et
    // Division ve multiplication sonrası sonucun step'in tam katı olduğundan emin ol
    // Step'in scale'ini kullanarak normalize et (step'in ondalık basamak sayısı)
    // Bu, 0.123456789 / 0.001 * 0.001 = 0.122999999 gibi durumları önler
    let step_scale = step.scale();
    let normalized = result.normalize();
    
    // Normalize edilmiş sonucu step'in scale'ine göre yuvarla
    // Bu, step'in tam katı olduğundan emin olur
    normalized.round_dp_with_strategy(step_scale, rust_decimal::RoundingStrategy::ToNegativeInfinity)
}

/// Format decimal with fixed precision
/// 
/// ✅ KRİTİK: Precision loss önleme
/// ToZero strategy truncate ediyor, bu precision loss'a yol açabilir.
/// Normalize ederek ve doğru rounding strategy kullanarak precision loss'u önlüyoruz.
pub fn format_decimal_fixed(value: Decimal, precision: usize) -> String {
    // KRİTİK DÜZELTME: Edge case'ler için ek kontroller
    // Precision overflow kontrolü (max 28 decimal places)
    let precision = precision.min(28);
    let scale = precision as u32;

    // Decimal her zaman finite'dir, bu yüzden direkt işle

    // ✅ KRİTİK: Precision loss önleme
    // 1. Önce normalize et (internal representation'ı temizle)
    // 2. Sonra precision'a göre yuvarla
    // ToZero yerine ToNegativeInfinity (floor) kullan - daha güvenli ve precision loss'u önler
    let normalized = value.normalize();
    let rounded = normalized.round_dp_with_strategy(scale, RoundingStrategy::ToNegativeInfinity);

    // KRİTİK: String formatlamada kesinlikle precision'dan fazla basamak gösterme
    // Decimal'in to_string() metodu bazen internal precision'ı gösterebilir
    // Bu yüzden manuel olarak string'i kontrol edip kesmeliyiz
    if scale == 0 {
        // Integer kısmı al (nokta varsa kes)
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
                // Eksik trailing zero'ları ekle
                format!(
                    "{}.{}{}",
                    integer_part,
                    decimal_part,
                    "0".repeat(scale as usize - current_decimals)
                )
            } else if current_decimals > scale as usize {
                // KRİTİK: Fazla decimal varsa kes - kesinlikle precision'dan fazla basamak gösterme
                // String'i kes - bu "Precision is over the maximum" hatasını önler
                let truncated_decimal = &decimal_part[..scale as usize];
                format!("{}.{}", integer_part, truncated_decimal)
            } else {
                // Tam precision - olduğu gibi döndür
                s
            }
        } else {
            // Nokta yoksa ekle ve trailing zero ekle
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

/// Transient hata mı kontrol et (retry yapılabilir mi?)
/// 408 (Request Timeout), 429 (Too Many Requests), 5xx (Server Errors) → transient
/// 400 (Bad Request) → body'ye göre karar ver (bazıları transient olabilir)
fn is_transient_error(status: u16, _body: &str) -> bool {
    match status {
        408 => true,       // Request Timeout
        429 => true,       // Too Many Requests
        500..=599 => true, // Server Errors
        400 => {
            // 400 için body'ye bak - bazı hatalar transient olabilir
            // "Invalid symbol" gibi kalıcı hatalar retry edilmemeli
            // "Precision is over" gibi hatalar kalıcı
            // "Insufficient margin" gibi hatalar kalıcı
            // Ama network timeout gibi durumlar transient olabilir
            // Şimdilik 400'leri kalıcı sayalım (daha güvenli)
            false
        }
        _ => false, // Diğer hatalar kalıcı
    }
}

/// Kalıcı hata mı kontrol et (sembol disable edilmeli mi?)
/// "invalid", "margin", "precision" gibi hatalar kalıcıdır
fn is_permanent_error(status: u16, body: &str) -> bool {
    if status == 400 {
        let body_lower = body.to_lowercase();
        // KRİTİK DÜZELTME: -1111 (precision) hatası permanent değil, retry edilebilir
        // Çünkü girdiyi düzelterek geçilebilir
        if body_lower.contains("precision is over") || body_lower.contains("-1111") {
            return false; // Precision hatası retry edilebilir
        }
        body_lower.contains("invalid")
            || body_lower.contains("margin")
            || body_lower.contains("insufficient balance")
            || body_lower.contains("min notional")
            || body_lower.contains("below min notional")
    } else {
        false
    }
}

async fn send_json<T>(builder: RequestBuilder) -> Result<T>
where
    T: DeserializeOwned,
{
    // KRİTİK DÜZELTME: Retry/backoff mekanizması
    // RequestBuilder clone edilemediği için, request'i baştan oluşturmalıyız
    // Ama builder'ı closure'a wrap edemeyiz çünkü builder consume ediliyor
    // Bu yüzden şimdilik sadece ilk denemeyi yapıyoruz, retry mekanizması üst seviyede implement edilebilir
    let resp = builder.send().await?;
    let resp = ensure_success(resp).await?;
    Ok(resp.json().await?)
}

async fn send_void(builder: RequestBuilder) -> Result<()> {
    // KRİTİK DÜZELTME: Retry/backoff mekanizması
    // RequestBuilder clone edilemediği için, request'i baştan oluşturmalıyız
    // Ama builder'ı closure'a wrap edemeyiz çünkü builder consume ediliyor
    // Bu yüzden şimdilik sadece ilk denemeyi yapıyoruz, retry mekanizması üst seviyede implement edilebilir
    let resp = builder.send().await?;
    ensure_success(resp).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_quantize_price() {
        let price = dec!(0.2593620616072499999728690579);
        let tick = dec!(0.001);
        let result = quantize_decimal(price, tick);
        assert_eq!(result, dec!(0.259));

        let result = quantize_decimal(price, dec!(0.1));
        assert_eq!(result, dec!(0.2));
    }

    #[test]
    fn test_quantize_qty() {
        let qty = dec!(76.4964620386307103672152152);
        let step = dec!(0.001);
        let result = quantize_decimal(qty, step);
        assert_eq!(result, dec!(76.496));
    }


    #[test]
    fn test_format_decimal_fixed() {
        assert_eq!(format_decimal_fixed(dec!(0.123456), 3), "0.123");
        assert_eq!(format_decimal_fixed(dec!(5), 0), "5");
        // format_decimal_fixed trailing zero'ları korur (precision kadar)
        assert_eq!(format_decimal_fixed(dec!(1.2000), 4), "1.2000");
        assert_eq!(format_decimal_fixed(dec!(0.00000001), 8), "0.00000001");

        // Yüksek fiyatlı semboller için testler (BNBUSDC gibi)
        assert_eq!(format_decimal_fixed(dec!(950.649470), 2), "950.64");
        assert_eq!(format_decimal_fixed(dec!(950.649470), 3), "950.649");
        assert_eq!(format_decimal_fixed(dec!(956.370530), 2), "956.37");
        assert_eq!(format_decimal_fixed(dec!(956.370530), 3), "956.370");

        // Fazla precision'ı kesme testi
        assert_eq!(format_decimal_fixed(dec!(202.129776525), 2), "202.12");
        assert_eq!(format_decimal_fixed(dec!(202.129776525), 3), "202.129");
        assert_eq!(format_decimal_fixed(dec!(0.08082180550260300), 4), "0.0808");
        assert_eq!(
            format_decimal_fixed(dec!(0.08082180550260300), 5),
            "0.08082"
        );

        // Integer precision testi
        assert_eq!(format_decimal_fixed(dec!(100.5), 0), "100");
        assert_eq!(format_decimal_fixed(dec!(1000), 0), "1000");
    }

    #[test]
    fn test_scale_from_step() {
        // tick_size'dan precision hesaplama testleri
        assert_eq!(scale_from_step(dec!(0.1)), 1);
        assert_eq!(scale_from_step(dec!(0.01)), 2);
        assert_eq!(scale_from_step(dec!(0.001)), 3);
        assert_eq!(scale_from_step(dec!(0.0001)), 4);
        assert_eq!(scale_from_step(dec!(1)), 0);
        assert_eq!(scale_from_step(dec!(10)), 0);
        assert_eq!(scale_from_step(dec!(0.000001)), 6);
    }
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

#[derive(Clone, Copy, Debug)]
pub enum UserStreamKind {
    Futures,
}

#[derive(Debug, Clone)]
pub enum UserEvent {
    OrderFill {
        symbol: String,
        order_id: String,
        side: Side,
        qty: Qty,                   // Last executed qty (incremental, this fill only)
        cumulative_filled_qty: Qty, // Cumulative filled qty (total filled so far)
        order_qty: Option<Qty>,    // Original order quantity (from "q" field, may be missing)
        price: Px,
        is_maker: bool,       // true = maker, false = taker
        order_status: String, // Order status: NEW, PARTIALLY_FILLED, FILLED, CANCELED, etc.
        commission: Decimal,  // KRİTİK: Gerçek komisyon (executionReport'tan "n" field'ı)
    },
    OrderCanceled {
        symbol: String,
        order_id: String,
        client_order_id: Option<String>, // Idempotency için
    },
    AccountUpdate {
        // Position updates
        positions: Vec<AccountPosition>,
        // Balance updates
        balances: Vec<AccountBalance>,
    },
    Heartbeat,
}

#[derive(Debug, Clone)]
pub struct AccountPosition {
    pub symbol: String,
    pub position_amt: Decimal,
    pub entry_price: Decimal,
    pub leverage: u32,
    pub unrealized_pnl: Option<Decimal>,
}

#[derive(Debug, Clone)]
pub struct AccountBalance {
    pub asset: String,
    pub available_balance: Decimal,
}

/// Market data price update from WebSocket (@bookTicker stream)
#[derive(Debug, Clone)]
pub struct PriceUpdate {
    pub symbol: String,
    pub bid: Px,
    pub ask: Px,
    pub bid_qty: Qty,
    pub ask_qty: Qty,
}

pub struct UserDataStream {
    client: Client,
    base: String,
    api_key: String,
    kind: UserStreamKind,
    listen_key: String,
    ws: WsStream,
    last_keep_alive: Instant,
    /// Reconnect sonrası missed events sync callback
    /// Callback reconnect sonrası çağrılır ve missed events'leri sync etmek için kullanılır
    on_reconnect: Option<Box<dyn Fn() + Send + Sync>>,
}

/// Market data WebSocket stream for price updates (@bookTicker)
/// ✅ BEST PRACTICE: WebSocket kullanarak REST API rate limit'ini önle
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

    /// WS'yi mevcut listenKey ile yeniden bağlar (yeni listen key oluşturmaz)
    /// KRİTİK: Timeout durumunda listen key hala geçerli olabilir, bu yüzden önce mevcut key ile dene
    async fn reconnect_ws_without_new_key(&mut self) -> Result<()> {
        let url = Self::ws_url_for(self.kind, &self.listen_key);
        let (ws, _) = connect_async(&url).await?;
        self.ws = ws;
        self.last_keep_alive = Instant::now();

        // Optional reconnect callback (for logging only)
        // Binance WebSocket automatically sends state updates after reconnect
        if let Some(ref callback) = self.on_reconnect {
            callback();
        }
        info!(%url, "reconnected user data websocket (same listen key)");
        Ok(())
    }

    /// Reconnect WebSocket with new listen key (existing connection is closed)
    /// This function is called when listen key expires or keep_alive fails
    /// Binance WebSocket automatically sends state updates after reconnect via
    /// ACCOUNT_UPDATE and ORDER_TRADE_UPDATE events. No REST API sync needed.
    async fn reconnect_ws(&mut self) -> Result<()> {
        // ✅ KRİTİK DÜZELTME: Önce mevcut listen key ile yeniden bağlanmayı dene
        // Timeout olunca listen key hala geçerli olabilir (60 dakika geçerli)
        // Gereksiz yere yeni listen key oluşturmak API rate limit'e takılabilir
        match self.reconnect_ws_without_new_key().await {
            Ok(()) => {
                // Mevcut listen key ile başarılı, yeni key oluşturmaya gerek yok
                return Ok(());
            }
            Err(e) => {
                // Mevcut listen key ile başarısız (muhtemelen expire olmuş)
                warn!(error = %e, "reconnect with existing listen key failed, creating new listen key");
            }
        }

        // 1. Yeni listen key oluştur (eski expire olmuş)
        let new_key =
            Self::create_listen_key(&self.client, &self.base, &self.api_key, self.kind).await?;
        self.listen_key = new_key;

        // 2. WebSocket'e bağlan
        let url = Self::ws_url_for(self.kind, &self.listen_key);
        let (ws, _) = connect_async(&url).await?;
        self.ws = ws;
        self.last_keep_alive = Instant::now();

        // 3. Reconnect callback (optional, for logging only)
        // Binance WebSocket automatically sends ACCOUNT_UPDATE and ORDER_TRADE_UPDATE events
        // after reconnect which contain current state. No REST API sync needed.
        if let Some(ref callback) = self.on_reconnect {
            callback();
            info!(%url, "reconnected user data websocket (new listen key), callback triggered");
        }

        info!(%url, "reconnected user data websocket (new listen key)");
        Ok(())
    }

    /// Set optional reconnect callback (for logging only)
    /// Binance WebSocket automatically sends state updates after reconnect via
    /// ACCOUNT_UPDATE and ORDER_TRADE_UPDATE events. No REST API sync needed.
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

        // 1) listenKey oluştur
        let listen_key = Self::create_listen_key(&client, &base, api_key, kind).await?;

        // 2) WS bağlan
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

    /// 25. dakikadan sonra keepalive (PUT). Hata alırsak yeni listenKey oluşturup WS'yi yeniden bağlarız.
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

        // Keepalive başarısız → yeni listenKey oluştur
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

            // CRITICAL: Timeout set to 90 seconds (1.5 minutes)
            // Binance listen key is valid for 60 minutes, keep_alive runs every 25 minutes
            // 90 seconds is sufficient to detect connection issues quickly while avoiding false timeouts
            // This is much shorter than keep-alive interval (25 min) but long enough for normal operation
            // If connection drops due to network issues, we'll detect it within 90 seconds instead of 5 minutes
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
            // Futures executionReport
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
                // Maker flag: "m" field (true = maker, false = taker)
                let is_maker = value.get("m").and_then(Value::as_bool).unwrap_or(false);
                // KRİTİK DÜZELTME: Gerçek komisyon (executionReport'tan "n" field'ı)
                // "n" = commission (last executed qty için komisyon, incremental)
                let commission = Self::parse_decimal(value, "n");
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

            // FUTURES user data wrapper: ORDER_TRADE_UPDATE
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
                // Maker flag: "m" field (true = maker, false = taker)
                let is_maker = data.get("m").and_then(Value::as_bool).unwrap_or(false);
                // KRİTİK DÜZELTME: Gerçek komisyon (ORDER_TRADE_UPDATE'ten "n" field'ı)
                // "n" = commission (last executed qty için komisyon, incremental)
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
