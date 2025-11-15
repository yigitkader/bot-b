// CONNECTION: Exchange WS & REST single gateway
// All external world (WS/REST) goes through here
// Rate limit & reconnect management
//
// Single responsibility: Connection management (WebSocket + REST coordination)

mod venue;
mod websocket;

use crate::config::AppCfg;
use crate::event_bus::{
    BalanceUpdate, EventBus, MarketTick,
    OrderStatus, OrderUpdate, PositionUpdate,
};
use crate::state::SharedState;
use crate::types::{OrderFillHistory, OrderCommand, Position, Px, Qty, Side, UserEvent, UserStreamKind, VenueOrder};
use venue::{BinanceFutures, BALANCE_CACHE, FUT_RULES, OPEN_ORDERS_CACHE, POSITION_CACHE, PRICE_CACHE, Venue};
use websocket::{MarketDataStream, UserDataStream};
use anyhow::{anyhow, Result};
use dashmap::DashMap;
use rust_decimal::Decimal;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

// ============================================================================
// CONNECTION Module (Public API)
// ============================================================================

// OrderFillHistory is defined in types.rs

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
    pub fn from_config(
        cfg: Arc<AppCfg>,
        event_bus: Arc<EventBus>,
        shutdown_flag: Arc<AtomicBool>,
        shared_state: Option<Arc<SharedState>>,
    ) -> Result<Self> {
        let venue = Arc::new(BinanceFutures::from_config(
            &cfg.binance,
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
            order_fill_history: Arc::new(DashMap::new()),
        }
    }

    pub async fn start(&self, symbols: Vec<String>) -> Result<()> {

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
                // ✅ CRITICAL: Always check age FIRST to prevent memory leaks
                // Problem: If cache is stale (WebSocket lag), order may appear "open" in cache
                // but history is very old. Without age check first, these entries never get cleaned up.
                //
                // Solution: Check age first, then cache status. If age > MAX_AGE_SECS,
                // force remove even if in cache (cache is likely stale).
                let age = now.duration_since(history.last_update);

                // First check: Is order in OPEN_ORDERS_CACHE?
                let is_in_cache = OPEN_ORDERS_CACHE.iter().any(|entry| {
                    entry.value().iter().any(|o| o.order_id == *order_id)
                });

                if is_in_cache {
                    // Order is in cache - but check age first to prevent memory leak
                    if age.as_secs() > MAX_AGE_SECS {
                        // Cache is stale - force remove to prevent leak
                        // This handles the case where:
                        // - Order was filled/cancelled but WebSocket update delayed
                        // - Cache still shows order as "open" but history is very old
                        // - Without this check, entry would never be cleaned up
                        warn!(
                            order_id = %order_id,
                            history_age_secs = age.as_secs(),
                            history_age_hours = age.as_secs() / 3600,
                            "CONNECTION: Order in cache but history is very old ({} hours), cache likely stale. Force removing to prevent memory leak.",
                            age.as_secs() / 3600
                        );
                        false // FORCE REMOVE - prevent memory leak
                    } else {
                        // Order in cache and history is recent - keep it
                        true // Keep
                    }
                } else {
                    // Not in cache - normal age check
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
                            // Connection successful - reset connection retry delay
                            connection_retry_delay = INITIAL_DELAY_SECS;

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

                                            // ✅ CRITICAL: REST API is source of truth after reconnect
                                            // Always update cache with REST API data to ensure state consistency
                                            // Log level varies based on difference magnitude for debugging
                                            
                                            // Determine if this is a significant mismatch (for logging)
                                            const SIGNIFICANT_QTY_DIFF: &str = "0.0001";
                                            const SIGNIFICANT_ENTRY_DIFF: &str = "0.01";
                                            let significant_mismatch = qty_diff > Decimal::from_str(SIGNIFICANT_QTY_DIFF).unwrap()
                                                || entry_diff > Decimal::from_str(SIGNIFICANT_ENTRY_DIFF).unwrap();

                                            // ✅ ALWAYS update cache with REST API data (REST API is source of truth)
                                            // This ensures state consistency after reconnect
                                            POSITION_CACHE.insert(symbol.to_string(), rest_position.clone());

                                            if significant_mismatch {
                                                // Significant mismatch - log warning
                                                warn!(
                                                    symbol = %symbol,
                                                    rest_qty = %rest_position.qty.0,
                                                    ws_qty = %ws_pos.qty.0,
                                                    qty_diff = %qty_diff,
                                                    rest_entry = %rest_position.entry.0,
                                                    ws_entry = %ws_pos.entry.0,
                                                    entry_diff = %entry_diff,
                                                    "CONNECTION: Position mismatch detected after reconnect (REST vs WebSocket), cache updated with REST API data (source of truth)"
                                                );
                                            } else if qty_diff > Decimal::from_str("0.000001").unwrap_or_default() 
                                                || entry_diff > Decimal::from_str("0.001").unwrap_or_default() {
                                                // Small differences - log at debug level
                                                debug!(
                                                    symbol = %symbol,
                                                    qty_diff = %qty_diff,
                                                    entry_diff = %entry_diff,
                                                    "CONNECTION: Small position difference detected after reconnect, cache updated with REST API data"
                                                );
                                            } else {
                                                // No difference - cache already matches REST API
                                                debug!(
                                                    symbol = %symbol,
                                                    "CONNECTION: Position cache matches REST API after reconnect"
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
                                        // ✅ CRITICAL: REST API is source of truth after reconnect
                                        // Always update cache with REST API data to ensure state consistency
                                        
                                        // Compare with WebSocket cache
                                        if let Some(ws_orders) = OPEN_ORDERS_CACHE.get(symbol) {
                                            let ws_orders_vec = ws_orders.value();

                                            // Check if order counts match
                                            let count_mismatch = rest_orders.len() != ws_orders_vec.len();
                                            
                                            // Check for missing orders in WebSocket cache
                                            let mut missing_orders = Vec::new();
                                            for rest_order in &rest_orders {
                                                if !ws_orders_vec.iter().any(|o| o.order_id == rest_order.order_id) {
                                                    missing_orders.push(rest_order.order_id.clone());
                                                }
                                            }

                                            // ✅ ALWAYS update cache with REST API data (REST API is source of truth)
                                            // This ensures state consistency after reconnect
                                            let rest_orders_vec: Vec<_> = rest_orders.iter().map(|o| {
                                                crate::types::VenueOrder {
                                                    order_id: o.order_id.clone(),
                                                    side: o.side,
                                                    price: o.price,
                                                    qty: o.qty,
                                                }
                                            }).collect();
                                            OPEN_ORDERS_CACHE.insert(symbol.to_string(), rest_orders_vec);

                                            if count_mismatch || !missing_orders.is_empty() {
                                                warn!(
                                                    symbol = %symbol,
                                                    rest_order_count = rest_orders.len(),
                                                    ws_order_count = ws_orders_vec.len(),
                                                    missing_order_ids = ?missing_orders,
                                                    "CONNECTION: Open orders mismatch detected after reconnect (REST vs WebSocket), cache updated with REST API data (source of truth)"
                                                );
                                            } else {
                                                debug!(
                                                    symbol = %symbol,
                                                    "CONNECTION: Open orders cache matches REST API after reconnect"
                                                );
                                            }
                                        } else if !rest_orders.is_empty() {
                                            // REST API shows orders but WebSocket cache doesn't
                                            // ✅ Add missing orders to cache
                                            let rest_orders_vec: Vec<_> = rest_orders.iter().map(|o| {
                                                crate::types::VenueOrder {
                                                    order_id: o.order_id.clone(),
                                                    side: o.side,
                                                    price: o.price,
                                                    qty: o.qty,
                                                }
                                            }).collect();
                                            OPEN_ORDERS_CACHE.insert(symbol.to_string(), rest_orders_vec);
                                            
                                            warn!(
                                                symbol = %symbol,
                                                rest_order_count = rest_orders.len(),
                                                "CONNECTION: Open orders exist in REST API but missing from WebSocket cache after reconnect, cache updated with REST API data"
                                            );
                                        } else {
                                            // REST API shows no orders - remove from cache if exists
                                            if OPEN_ORDERS_CACHE.contains_key(symbol) {
                                                OPEN_ORDERS_CACHE.remove(symbol);
                                                debug!(
                                                    symbol = %symbol,
                                                    "CONNECTION: No open orders in REST API, removed stale orders from cache"
                                                );
                                            }
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

                                            if balance_diff > Decimal::from_str("0.01").unwrap_or_default() {
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
                                            // True if all fills were maker orders, false if any fill was taker
                                            // Used for commission calculation: if all maker, use maker commission; otherwise taker
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

    /// Get per-symbol metadata (tick_size, step_size)
    /// Delegates to the venue's rules_for method
    pub async fn rules_for(&self, sym: &str) -> Result<Arc<crate::types::SymbolRules>> {
        self.venue.rules_for(sym).await
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

