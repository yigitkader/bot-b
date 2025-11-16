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
use crate::types::{OrderFillHistory, OrderCommand, Position, Px, Qty, Side, UserEvent, UserStreamKind, VenueOrder, ScoredSymbol, SymbolStats24h};
use venue::{BinanceFutures, BALANCE_CACHE, FUT_RULES, OPEN_ORDERS_CACHE, POSITION_CACHE, PRICE_CACHE, Venue};
use websocket::{MarketDataStream, UserDataStream};
use anyhow::{anyhow, Result};
use dashmap::DashMap;
use futures_util::future;
use rust_decimal::Decimal;
use serde::Deserialize;
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

    /// Get clamped leverage for a symbol
    /// Clamps desired leverage to symbol's max leverage (if available) or safe fallback (50x)
    /// This centralizes leverage clamping logic to avoid duplication
    async fn get_clamped_leverage(&self, symbol: &str, desired_leverage: u32) -> u32 {
        const SAFE_LEVERAGE_FALLBACK: u32 = 50;
        
        match self.venue.rules_for(symbol).await {
            Ok(rules) => {
                if let Some(symbol_max_lev) = rules.max_leverage {
                    // Symbol has max leverage - clamp to it
                    let clamped = desired_leverage.min(symbol_max_lev);
                    if clamped < desired_leverage {
                        warn!(
                            symbol = %symbol,
                            desired_leverage,
                            symbol_max_leverage = symbol_max_lev,
                            final_leverage = clamped,
                            "CONNECTION: Leverage clamped to symbol max leverage"
                        );
                    }
                    clamped
                } else {
                    // No symbol max leverage - use safe fallback instead of config max
                    // Config max (100x) might be too high for some symbols
                    let clamped = desired_leverage.min(SAFE_LEVERAGE_FALLBACK);
                    if clamped < desired_leverage {
                        warn!(
                            symbol = %symbol,
                            desired_leverage,
                            safe_fallback = SAFE_LEVERAGE_FALLBACK,
                            final_leverage = clamped,
                            "CONNECTION: Leverage brackets not available, using safe fallback (50x) instead of config max"
                        );
                    }
                    clamped
                }
            }
            Err(e) => {
                // Failed to fetch rules - use safe fallback instead of config max
                warn!(
                    error = %e,
                    symbol = %symbol,
                    safe_fallback = SAFE_LEVERAGE_FALLBACK,
                    "CONNECTION: Failed to fetch symbol rules, using safe fallback (50x) instead of config max"
                );
                desired_leverage.min(SAFE_LEVERAGE_FALLBACK)
            }
        }
    }

    pub async fn start(&self, symbols: Vec<String>) -> Result<()> {

        // Set hedge mode according to config (must be done before any orders)
        // Note: set_position_side_dual handles -4059 "No need to change" as success
        let hedge_mode = self.cfg.binance.hedge_mode;
        if let Err(e) = self.venue.set_position_side_dual(hedge_mode).await {
            warn!(
                error = %e,
                hedge_mode,
                "CONNECTION: Failed to set hedge mode, continuing anyway (may cause issues)"
            );
        } else {
            debug!(
                hedge_mode,
                "CONNECTION: Hedge mode configured (may already be set correctly)"
            );
        }

        let desired_leverage = self.cfg.leverage.unwrap_or(self.cfg.exec.default_leverage);
        let use_isolated_margin = self.cfg.risk.use_isolated_margin;

        for symbol in &symbols {
            // ✅ CRITICAL: Clamp leverage to symbol's max leverage (if available)
            // Use centralized clamping function to avoid code duplication
            let leverage = self.get_clamped_leverage(symbol, desired_leverage).await;
            // Set margin type (isolated or cross)
            // Note: set_margin_type handles -4046 "No need to change" as success
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
                debug!(
                    symbol = %symbol,
                    isolated = use_isolated_margin,
                    "CONNECTION: Margin type configured for symbol (may already be set correctly)"
                );
            }

            if let Ok(position) = self.venue.get_position(symbol).await {
                if !position.qty.0.is_zero() {
                    // Position is OPEN - leverage cannot be changed
                    // Use clamped leverage for comparison
                    if position.leverage != leverage {
                        error!(
                            symbol = %symbol,
                            current_leverage = position.leverage,
                            desired_leverage = leverage,
                            "CONNECTION: CRITICAL - Leverage mismatch detected but position is open! Cannot auto-fix. Please close position manually and restart bot."
                        );
                        return Err(anyhow!(
                            "Leverage mismatch for symbol {}: exchange has {}x but config requires {}x (clamped to {}x). Cannot change leverage while position is open. Please close position manually and restart bot.",
                            symbol,
                            position.leverage,
                            desired_leverage,
                            leverage
                        ));
                    } else {
                        info!(
                            symbol = %symbol,
                            leverage,
                            "CONNECTION: Leverage verified (matches clamped leverage, position is open)"
                        );
                    }
                } else {
                    // Position is CLOSED - safe to set leverage
                    // ✅ CRITICAL: Re-fetch symbol rules to get max leverage (may have changed or not been fetched yet)
                    // Use centralized clamping function to ensure consistency
                    let final_leverage = self.get_clamped_leverage(symbol, leverage).await;
                    
                    if position.leverage != final_leverage {
                        warn!(
                            symbol = %symbol,
                            current_leverage = position.leverage,
                            desired_leverage = leverage,
                            final_leverage = final_leverage,
                            "CONNECTION: Leverage mismatch detected (position is closed). Auto-fixing..."
                        );
                        match self.venue.set_leverage(symbol, final_leverage).await {
                            Ok(actual_leverage) => {
                                // Leverage set successfully (may be lower than final_leverage due to step-down)
                                if actual_leverage != final_leverage {
                                    warn!(
                                        symbol = %symbol,
                                        desired_leverage = leverage,
                                        clamped_leverage = final_leverage,
                                        actual_leverage,
                                        "CONNECTION: Leverage set to lower value than clamped (step-down mechanism)"
                                    );
                                }
                                // Verify the fix worked
                                if let Ok(updated_position) = self.venue.get_position(symbol).await {
                                    if updated_position.leverage == actual_leverage {
                                        info!(
                                            symbol = %symbol,
                                            old_leverage = position.leverage,
                                            new_leverage = actual_leverage,
                                            "CONNECTION: Leverage mismatch auto-fixed successfully (position was closed)"
                                        );
                                    } else {
                                        warn!(
                                            symbol = %symbol,
                                            expected_leverage = actual_leverage,
                                            actual_leverage = updated_position.leverage,
                                            "CONNECTION: Leverage fix may not have taken effect immediately (will retry on next check)"
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                error!(
                                    error = %e,
                                    symbol = %symbol,
                                    current_leverage = position.leverage,
                                    desired_leverage = leverage,
                                    final_leverage = final_leverage,
                                    "CONNECTION: Failed to auto-fix leverage mismatch for symbol (all leverage values failed)"
                                );
                                return Err(anyhow!(
                                    "Failed to set leverage for symbol {} (was {}x, need {}x, clamped to {}x): {}",
                                    symbol,
                                    position.leverage,
                                    leverage,
                                    final_leverage,
                                    e
                                ));
                            }
                        }
                    } else {
                        info!(
                            symbol = %symbol,
                            leverage = final_leverage,
                            "CONNECTION: Leverage verified (matches clamped leverage, position is closed)"
                        );
                    }
                }
            } else {
                // No position exists - safe to set leverage
                // Leverage is already clamped above (in the for loop), so use it directly
                // Set leverage (for new positions or to ensure it's set correctly)
                match self.venue.set_leverage(symbol, leverage).await {
                    Ok(actual_leverage) => {
                        if actual_leverage != leverage {
                            warn!(
                                symbol = %symbol,
                                desired_leverage = leverage,
                                actual_leverage,
                                "CONNECTION: Leverage set to lower value than desired (step-down mechanism)"
                            );
                        } else {
                            info!(
                                symbol = %symbol,
                                leverage = actual_leverage,
                                "CONNECTION: Leverage set successfully for symbol"
                            );
                        }
                    }
                    Err(e) => {
                        error!(
                            error = %e,
                            symbol = %symbol,
                            leverage,
                            "CONNECTION: CRITICAL - Failed to set leverage for symbol (all leverage values failed). Cannot continue with incorrect leverage."
                        );
                        return Err(anyhow!(
                            "Failed to set leverage for symbol {}: {}",
                            symbol,
                            e
                        ));
                    }
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
        // ✅ CRITICAL: Reduced cleanup interval from 1 hour to 10 minutes for HFT
        // HFT requires faster cleanup to prevent memory accumulation
        const CLEANUP_INTERVAL_SECS: u64 = 600; // 10 minutes (HFT optimized, was 1 hour)
        // ✅ CRITICAL: Reduced max age from 24 hours to 1 hour for HFT
        // 24 hours is too long for HFT - filled orders should be cleaned up faster
        const MAX_AGE_SECS: u64 = 3600; // 1 hour (HFT optimized, was 24 hours)

        loop {
            tokio::time::sleep(Duration::from_secs(CLEANUP_INTERVAL_SECS)).await;

            if shutdown_flag.load(AtomicOrdering::Relaxed) {
                break;
            }

            let now = Instant::now();
            let initial_count = order_fill_history.len();
            order_fill_history.retain(|order_id, history| {
                // ✅ CRITICAL: Fixed memory leak - remove filled orders after short delay
                // Problem: Previous code kept entries even when orders were filled/cancelled (not in cache)
                // This caused memory leak: 100 orders/day × 30 days = 3000 entries (only cleaned after 24 hours)
                //
                // Solution: Three-tier cleanup strategy
                // 1. Force remove very old entries (1+ hour) - prevents leaks from stale cache (HFT optimized)
                // 2. Remove filled/cancelled orders after short delay (30 seconds) - prevents accumulation (HFT optimized)
                // 3. Keep active orders (in cache) - protects ongoing orders
                let age = now.duration_since(history.last_update);
                let age_secs = age.as_secs();

                // Step 1: AGE CHECK FIRST (prevents leak even if cache is stale)
                // If entry is very old (1+ hour), force remove to prevent memory leak (HFT optimized)
                // This handles cases where:
                // - Order was filled/cancelled but WebSocket update delayed
                // - Cache still shows order as "open" but history is very old
                // - Order has been open for more than 1 hour (unlikely but possible)
                if age_secs > MAX_AGE_SECS {
                    // Very old entry - force remove regardless of cache status
                    // This prevents memory leaks from stale cache or very long-lived orders
                    warn!(
                        order_id = %order_id,
                        history_age_secs = age_secs,
                        history_age_hours = age_secs / 3600,
                        "CONNECTION: Order fill history is very old ({} hours), force removing to prevent memory leak",
                        age_secs / 3600
                    );
                    return false; // FORCE REMOVE - prevent memory leak
                }

                // Step 2: CACHE CHECK (for recent entries)
                // Check if order is still active in cache
                let is_in_cache = OPEN_ORDERS_CACHE.iter().any(|entry| {
                    entry.value().iter().any(|o| o.order_id == *order_id)
                });

                if is_in_cache {
                    // Order is in cache and history is recent - keep it (active order)
                    true // Keep - order is still active
                } else {
                    // Order NOT in cache (filled/cancelled) - remove after short delay
                    // This prevents memory leak from accumulating filled order histories
                    // ✅ CRITICAL: Reduced from 5 minutes to 30 seconds for HFT
                    // - Order fill olduktan 1 saniye sonra cache'den çıkar
                    // - WebSocket events genellikle < 1 saniye içinde gelir
                    // - 30 saniye late-arriving events için yeterli buffer
                    // - HFT için 5 dakika çok uzun (memory leak riski)
                    const SHORT_DELAY_SECS: u64 = 30; // 30 seconds - enough time for late-arriving events (HFT optimized)
                    
                    if age_secs > SHORT_DELAY_SECS {
                        // Order not in cache and history is older than 30 seconds - remove it
                        // This handles:
                        // - Orders that were filled/cancelled (not in cache)
                        // - Late-arriving WebSocket events (30 seconds is enough buffer for HFT)
                        // - Prevents accumulation of filled order histories (memory leak prevention)
                        debug!(
                            order_id = %order_id,
                            history_age_secs = age_secs,
                            "CONNECTION: Order fill history not in cache (filled/cancelled), removing after {} seconds delay",
                            age_secs
                        );
                        false // Remove - order is filled/cancelled
                    } else {
                        // Order not in cache but history is very recent (< 30 seconds)
                        // Keep for now - may receive late-arriving fill events or order may be closing
                        // Will be cleaned up on next cleanup cycle if still inactive
                        true // Keep - recent entry, may receive late events
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
                // ✅ CRITICAL: Reduced max delay from 60s to 10s for HFT
                // HFT requires fast reconnection - 60 seconds is too long
                // Exponential backoff sequence: 1s → 2s → 4s → 8s → 10s (max)
                const INITIAL_DELAY_SECS: u64 = 1;
                const MAX_DELAY_SECS: u64 = 10; // 10 seconds max (HFT optimized, was 60s)
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
                                            // ✅ CRITICAL: Handle buffer overflow gracefully
                                            // Problem: If buffer is full, send() returns error and event is dropped
                                            // Solution: Log warning when buffer is full to detect backpressure
                                            // Subscribers will receive RecvError::Lagged and handle it
                                            match event_bus.market_tick_tx.send(market_tick) {
                                                Ok(receiver_count) => {
                                                    // Successfully sent - receiver_count indicates how many subscribers received it
                                                    // If receiver_count is 0, no subscribers are listening (not an error, just no consumers)
                                                    if receiver_count == 0 {
                                                        tracing::debug!(
                                                            symbol = %price_update.symbol,
                                                            "CONNECTION: MarketTick sent but no subscribers (normal if modules haven't started yet)"
                                                        );
                                                    }
                                                }
                                                Err(_) => {
                                                    // Buffer is full - event was dropped
                                                    // This indicates subscribers are lagging behind
                                                    // Subscribers will receive RecvError::Lagged and handle it
                                                    warn!(
                                                        symbol = %price_update.symbol,
                                                        receiver_count = event_bus.market_tick_tx.receiver_count(),
                                                        "CONNECTION: MarketTick buffer full - event dropped! Subscribers are lagging. Consider increasing event_bus.market_tick_buffer in config.yaml or optimizing subscriber performance."
                                                    );
                                                }
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
        let shared_state_for_validation = self.shared_state.clone();

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
                    let shared_state_for_validation_clone = shared_state_for_validation.clone();
                    stream.set_on_reconnect(move || {
                        info!("CONNECTION: WebSocket reconnected - Binance will automatically send state updates via ACCOUNT_UPDATE and ORDER_TRADE_UPDATE events");

                        // Spawn background validation task
                        let venue_clone = venue_for_validation.clone();
                        let symbols_clone = symbols_for_validation.clone();
                        let shared_state_for_validation = shared_state_for_validation_clone.clone();
                        tokio::spawn(async move {
                            // Wait a bit for ACCOUNT_UPDATE events to arrive (Binance sends them after reconnect)
                            tokio::time::sleep(Duration::from_secs(2)).await;

                            info!(
                                symbol_count = symbols_clone.len(),
                                "CONNECTION: Validating state after WebSocket reconnect (priority-based validation)"
                            );

                            // ✅ PRIORITY-BASED VALIDATION: Validate symbols in priority order
                            // Problem: Validating all 37 symbols takes ~15 seconds, blocking new trade signals
                            // Solution: Validate high-priority symbols first (active positions), then others in background
                            //
                            // Priority 1: Symbols with open positions/orders (validate immediately, ~1-5 symbols)
                            // Priority 2: Symbols with cached positions (validate next, ~5-10 symbols)
                            // Priority 3: All other symbols (validate in background, non-blocking)
                            //
                            // This ensures critical state is validated quickly (< 2 seconds) while
                            // non-critical validation happens in background without blocking trading

                            // Get priority symbols from shared state
                            let (priority_symbols, cached_symbols, remaining_symbols) = {
                                let mut priority = Vec::new();
                                let mut cached = Vec::new();
                                
                                // Priority 1: Symbols with open positions/orders (from shared_state)
                                if let Some(shared_state) = &shared_state_for_validation {
                                    let ordering_state = shared_state.ordering_state.lock().await;
                                    if let Some(position) = &ordering_state.open_position {
                                        priority.push(position.symbol.clone());
                                    }
                                    if let Some(order) = &ordering_state.open_order {
                                        if !priority.contains(&order.symbol) {
                                            priority.push(order.symbol.clone());
                                        }
                                    }
                                }
                                
                                // Priority 2: Symbols with cached positions (from POSITION_CACHE)
                                for entry in POSITION_CACHE.iter() {
                                    let symbol = entry.key().clone();
                                    if !priority.contains(&symbol) {
                                        cached.push(symbol);
                                    }
                                }
                                
                                // Priority 3: All other symbols
                                let remaining: Vec<String> = symbols_clone.iter()
                                    .filter(|s| !priority.contains(s) && !cached.contains(s))
                                    .cloned()
                                    .collect();
                                
                                (priority, cached, remaining)
                            };

                            let total_symbols = symbols_clone.len();
                            let priority_count = priority_symbols.len();
                            let cached_count = cached_symbols.len();
                            let remaining_count = remaining_symbols.len();

                            info!(
                                total_symbols,
                                priority_count,
                                cached_count,
                                remaining_count,
                                "CONNECTION: Priority-based validation - Priority: {} symbols, Cached: {} symbols, Remaining: {} symbols",
                                priority_count,
                                cached_count,
                                remaining_count
                            );

                            let symbol_timeout = Duration::from_secs(3);
                            let validation_start = Instant::now();

                            // Helper function to validate a batch of symbols
                            let validate_symbols_batch = {
                                let venue = venue_clone.clone();
                                let timeout = symbol_timeout;
                                move |symbols: Vec<String>| {
                                    let venue = venue.clone();
                                    
                                    symbols.into_iter().map(move |symbol| {
                                        let venue = venue.clone();
                                        let symbol_clone = symbol.clone();
                                        
                                        tokio::spawn(async move {
                                            // Validate position
                                            let position_result = tokio::time::timeout(
                                                timeout,
                                                venue.get_position(&symbol_clone)
                                            ).await;
                                            
                                            // Validate orders
                                            let orders_result = tokio::time::timeout(
                                                timeout,
                                                venue.get_open_orders(&symbol_clone)
                                            ).await;
                                            
                                            (symbol_clone, position_result, orders_result)
                                        })
                                    }).collect::<Vec<_>>()
                                }
                            };

                            // Priority 1: Validate active positions/orders immediately (blocking)
                            let mut validated_count = 0;
                            if !priority_symbols.is_empty() {
                                info!(
                                    priority_count,
                                    "CONNECTION: Validating priority symbols (active positions/orders) - {} symbols",
                                    priority_count
                                );
                                let priority_tasks = validate_symbols_batch(priority_symbols);
                                let priority_results = future::join_all(priority_tasks).await;
                                validated_count += Self::process_validation_results(priority_results, &venue_clone).await;
                            }

                            // Priority 2: Validate cached symbols (blocking, but fast)
                            if !cached_symbols.is_empty() {
                                info!(
                                    cached_count,
                                    "CONNECTION: Validating cached symbols - {} symbols",
                                    cached_count
                                );
                                let cached_tasks = validate_symbols_batch(cached_symbols);
                                let cached_results = future::join_all(cached_tasks).await;
                                validated_count += Self::process_validation_results(cached_results, &venue_clone).await;
                            }

                            // Priority 3: Validate remaining symbols in background (non-blocking)
                            if !remaining_symbols.is_empty() {
                                info!(
                                    remaining_count,
                                    "CONNECTION: Starting background validation for {} remaining symbols (non-blocking)",
                                    remaining_count
                                );
                                let venue_bg = venue_clone.clone();
                                tokio::spawn(async move {
                                    let remaining_tasks = validate_symbols_batch(remaining_symbols);
                                    let remaining_results = future::join_all(remaining_tasks).await;
                                    let bg_validated = Self::process_validation_results(remaining_results, &venue_bg).await;
                                    info!(
                                        validated_count = bg_validated,
                                        total_remaining = remaining_count,
                                        "CONNECTION: Background validation completed (validated {}/{} symbols)",
                                        bg_validated,
                                        remaining_count
                                    );
                                });
                            }

                            // Validate balance (USDT/USDC) - with timeout
                            let balance_timeout = Duration::from_secs(5);
                            const MAX_BALANCE_VALIDATION_TIME: Duration = Duration::from_secs(10); // Max 10 seconds for balance validation
                            for asset in &["USDT", "USDC"] {
                                // Check overall timeout
                                if validation_start.elapsed() > MAX_BALANCE_VALIDATION_TIME {
                                    warn!(
                                        asset = %asset,
                                        "CONNECTION: Balance validation timeout reached, skipping"
                                    );
                                    break;
                                }
                                
                                match tokio::time::timeout(balance_timeout, venue_clone.available_balance(asset)).await {
                                    Ok(Ok(rest_balance)) => {
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
                                    Ok(Err(e)) => {
                                        // Failed to fetch balance - log but don't fail
                                        debug!(
                                            asset = %asset,
                                            error = %e,
                                            "CONNECTION: Failed to fetch balance for validation after reconnect"
                                        );
                                    }
                                    Err(_) => {
                                        // Timeout - log warning
                                        warn!(
                                            asset = %asset,
                                            timeout_secs = balance_timeout.as_secs(),
                                            "CONNECTION: Balance validation timeout, skipping"
                                        );
                                    }
                                }
                            }

                            let elapsed_secs = validation_start.elapsed().as_secs();
                            let priority_success_rate = if priority_count > 0 {
                                (validated_count as f64 / (priority_count + cached_count) as f64) * 100.0
                            } else {
                                100.0
                            };
                            
                            info!(
                                validated_count,
                                priority_count,
                                cached_count,
                                remaining_count,
                                elapsed_secs,
                                success_rate = format!("{:.1}%", priority_success_rate),
                                "CONNECTION: Priority validation completed (validated {}/{} priority symbols in {}s, {:.1}% success rate). Background validation running for {} remaining symbols.",
                                validated_count,
                                priority_count + cached_count,
                                elapsed_secs,
                                priority_success_rate,
                                remaining_count
                            );
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

    /// Process validation results for a batch of symbols
    /// Returns the number of successfully validated symbols
    async fn process_validation_results(
        results: Vec<Result<(String, Result<Result<Position, anyhow::Error>, tokio::time::error::Elapsed>, Result<Result<Vec<VenueOrder>, anyhow::Error>, tokio::time::error::Elapsed>), tokio::task::JoinError>>,
        _venue: &Arc<BinanceFutures>,
    ) -> usize {
        let mut validated_count = 0;
        
        for result in results {
            match result {
                Ok((symbol, position_result, orders_result)) => {
                    // Process position validation
                    match position_result {
                        Ok(Ok(rest_position)) => {
                            // Compare with WebSocket cache
                            if let Some(ws_position) = POSITION_CACHE.get(&symbol) {
                                let ws_pos = ws_position.value();
                                let qty_diff = (rest_position.qty.0 - ws_pos.qty.0).abs();
                                let entry_diff = (rest_position.entry.0 - ws_pos.entry.0).abs();
                                
                                const SIGNIFICANT_QTY_DIFF: &str = "0.0001";
                                const SIGNIFICANT_ENTRY_DIFF: &str = "0.01";
                                let significant_mismatch = qty_diff > Decimal::from_str(SIGNIFICANT_QTY_DIFF).unwrap()
                                    || entry_diff > Decimal::from_str(SIGNIFICANT_ENTRY_DIFF).unwrap();
                                
                                POSITION_CACHE.insert(symbol.clone(), rest_position.clone());
                                
                                if significant_mismatch {
                                    warn!(
                                        symbol = %symbol,
                                        rest_qty = %rest_position.qty.0,
                                        ws_qty = %ws_pos.qty.0,
                                        qty_diff = %qty_diff,
                                        "CONNECTION: Position mismatch detected after reconnect, cache updated"
                                    );
                                }
                            } else if !rest_position.qty.0.is_zero() {
                                POSITION_CACHE.insert(symbol.clone(), rest_position.clone());
                                warn!(
                                    symbol = %symbol,
                                    rest_qty = %rest_position.qty.0,
                                    "CONNECTION: Position exists in REST API but missing from WebSocket cache, added to cache"
                                );
                            } else if POSITION_CACHE.contains_key(&symbol) {
                                POSITION_CACHE.remove(&symbol);
                            }
                            validated_count += 1;
                        }
                        Ok(Err(_)) | Err(_) => {
                            // Failed or timeout - skip silently for parallel execution
                        }
                    }
                    
                    // Process orders validation
                    match orders_result {
                        Ok(Ok(rest_orders)) => {
                            let rest_orders_vec: Vec<_> = rest_orders.iter().map(|o| {
                                crate::types::VenueOrder {
                                    order_id: o.order_id.clone(),
                                    side: o.side,
                                    price: o.price,
                                    qty: o.qty,
                                }
                            }).collect();
                            
                            if let Some(ws_orders) = OPEN_ORDERS_CACHE.get(&symbol) {
                                let ws_orders_vec = ws_orders.value();
                                let count_mismatch = rest_orders.len() != ws_orders_vec.len();
                                
                                if count_mismatch {
                                    warn!(
                                        symbol = %symbol,
                                        rest_order_count = rest_orders.len(),
                                        ws_order_count = ws_orders_vec.len(),
                                        "CONNECTION: Open orders mismatch detected after reconnect, cache updated"
                                    );
                                }
                            } else if !rest_orders.is_empty() {
                                warn!(
                                    symbol = %symbol,
                                    rest_order_count = rest_orders.len(),
                                    "CONNECTION: Open orders exist in REST API but missing from WebSocket cache, cache updated"
                                );
                            }
                            
                            OPEN_ORDERS_CACHE.insert(symbol.clone(), rest_orders_vec);
                        }
                        Ok(Err(_)) | Err(_) => {
                            // Failed or timeout - skip silently
                        }
                    }
                }
                Err(_) => {
                    // Task join error - skip
                }
            }
        }
        
        validated_count
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

        // 5. Balance check is done in ORDERING module (before calling send_order)
        // ORDERING module handles balance reservation atomically with state check
        // This prevents race conditions and ensures balance is available when order is placed
        // 
        // IMPORTANT: Do NOT check Binance API balance here because:
        // 1. Binance API balance (BALANCE_CACHE) may be stale (updated via WebSocket with delay)
        // 2. Internal balance reservation (shared_state.balance_store) is the source of truth
        // 3. ORDERING module already validated balance before calling send_order
        // 4. Checking Binance API balance here creates false negatives (balance appears 0 when it's actually available)
        //
        // If Binance API rejects order due to insufficient balance, it will return an error
        // which will be handled by ORDERING module's retry logic
        //
        // Note: Balance check for both open and close orders is handled by ORDERING module
        // This validation function only checks exchange-level constraints (min_notional, precision, etc.)

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
    /// Refreshes rules every 15 minutes (or on shutdown) to pick up Binance rule changes
    /// Also invalidates cache immediately on precision/MIN_NOTIONAL errors
    async fn start_rules_refresh_task(&self) {
    let shutdown_flag = self.shutdown_flag.clone();

    tokio::spawn(async move {
        // ✅ Reduced from 1 hour to 15 minutes for faster rule updates
        // Cache is also invalidated immediately on precision/MIN_NOTIONAL errors
        const REFRESH_INTERVAL_MINUTES: u64 = 15;
        const REFRESH_INTERVAL_SECS: u64 = REFRESH_INTERVAL_MINUTES * 60;

        info!(
        interval_minutes = REFRESH_INTERVAL_MINUTES,
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
    /// - Balance availability (only include quote assets with available balance)
    /// - Status (must be TRADING)
    /// - Contract type (must be PERPETUAL)
    /// Returns list of discovered symbol names
    pub async fn discover_symbols(&self) -> Result<Vec<String>> {
    let quote_asset_upper = self.cfg.quote_asset.to_uppercase();
    let allow_usdt = self.cfg.allow_usdt_quote;

    // ✅ CRITICAL: Check balance availability before including quote assets
    // If balance is zero or insufficient, exclude that quote asset from discovery
    // This prevents discovering symbols we cannot trade with
    // 
    // IMPORTANT: Use min_usd_per_order (not min_quote_balance_usd) for balance check
    // - min_usd_per_order: Minimum margin required (e.g., 10 USD)
    // - min_quote_balance_usd: Safety threshold for order placement (e.g., 120 USD)
    // - User can trade with minimum margin (10 USD), leverage will increase notional
    // - Balance check only needs to ensure minimum margin is available
    let min_required_balance = Decimal::from_str(
        &self.cfg.min_usd_per_order.to_string()
    ).unwrap_or(Decimal::from(10)); // Default to 10 USD if conversion fails
    
    let mut usdt_balance = Decimal::ZERO;
    let mut usdc_balance = Decimal::ZERO;
    
    // Fetch balances to check availability
    match self.fetch_balance("USDT").await {
        Ok(bal) => usdt_balance = bal,
        Err(e) => {
            warn!(
                error = %e,
                "CONNECTION: Failed to fetch USDT balance, excluding USDT symbols from discovery"
            );
        }
    }
    
    match self.fetch_balance("USDC").await {
        Ok(bal) => usdc_balance = bal,
        Err(e) => {
            warn!(
                error = %e,
                "CONNECTION: Failed to fetch USDC balance, excluding USDC symbols from discovery"
            );
        }
    }
    
    let has_usdt_balance = usdt_balance >= min_required_balance;
    let has_usdc_balance = usdc_balance >= min_required_balance;
    
    // Determine which quote assets to include based on balance availability
    let mut allowed_quotes = Vec::new();
    
    // Include primary quote asset only if balance is available
    if quote_asset_upper == "USDT" {
        if has_usdt_balance {
            allowed_quotes.push("USDT".to_string());
        } else {
            warn!(
                usdt_balance = %usdt_balance,
                min_required_balance = %min_required_balance,
                "CONNECTION: USDT balance insufficient (need {} for minimum margin), excluding USDT symbols from discovery",
                min_required_balance
            );
        }
    } else if quote_asset_upper == "USDC" {
        if has_usdc_balance {
            allowed_quotes.push("USDC".to_string());
        } else {
            warn!(
                usdc_balance = %usdc_balance,
                min_required_balance = %min_required_balance,
                "CONNECTION: USDC balance insufficient (need {} for minimum margin), excluding USDC symbols from discovery",
                min_required_balance
            );
        }
    } else {
        // Unknown quote asset - include it anyway (let other filters handle it)
        allowed_quotes.push(quote_asset_upper.clone());
    }
    
    // Include additional quote assets only if balance is available and allowed
    if allow_usdt && quote_asset_upper != "USDT" && has_usdt_balance {
        allowed_quotes.push("USDT".to_string());
    } else if allow_usdt && quote_asset_upper != "USDT" && !has_usdt_balance {
        warn!(
            usdt_balance = %usdt_balance,
            min_required_balance = %min_required_balance,
            "CONNECTION: USDT balance insufficient (need {} for minimum margin), excluding USDT symbols from discovery (allow_usdt_quote=true but no balance)",
            min_required_balance
        );
    }
    
    if allow_usdt && quote_asset_upper != "USDC" && has_usdc_balance {
        allowed_quotes.push("USDC".to_string());
    } else if allow_usdt && quote_asset_upper != "USDC" && !has_usdc_balance {
        warn!(
            usdc_balance = %usdc_balance,
            min_required_balance = %min_required_balance,
            "CONNECTION: USDC balance insufficient (need {} for minimum margin), excluding USDC symbols from discovery (allow_usdt_quote=true but no balance)",
            min_required_balance
        );
    }
    
    if allowed_quotes.is_empty() {
        return Err(anyhow!(
            "No quote assets available for trading. USDT balance: {} (min required: {} for minimum margin), USDC balance: {} (min required: {} for minimum margin). Please ensure sufficient balance for at least one minimum margin trade.",
            usdt_balance,
            min_required_balance,
            usdc_balance,
            min_required_balance
        ));
    }

    info!(
        quote_asset = %self.cfg.quote_asset,
        allow_usdt,
        allowed_quotes = ?allowed_quotes,
        usdt_balance = %usdt_balance,
        usdc_balance = %usdc_balance,
        min_required_balance = %min_required_balance,
        "CONNECTION: Discovering symbols with quote asset filter (balance-aware, using min_usd_per_order for check)"
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

    /// Fetch 24-hour statistics for a symbol from Binance API
    pub async fn fetch_24h_stats(&self, symbol: &str) -> Result<SymbolStats24h> {
        let url = format!(
            "{}/fapi/v1/ticker/24hr?symbol={}",
            self.cfg.binance.futures_base,
            symbol
        );

        #[derive(Deserialize)]
        struct Ticker24h {
            #[serde(rename = "priceChangePercent")]
            price_change_percent: String,
            #[serde(rename = "volume")]
            volume: String,
            #[serde(rename = "quoteVolume")]
            quote_volume: String,
            #[serde(rename = "count")]
            count: u64,
            #[serde(rename = "highPrice")]
            high_price: String,
            #[serde(rename = "lowPrice")]
            low_price: String,
        }

        let resp = self.venue.common.client
            .get(&url)
            .send()
            .await?;

        // Check HTTP status
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("API error for {}: {} - {}", symbol, status, body));
        }

        let ticker: Ticker24h = resp.json().await?;

        // Parse with proper error handling
        let price_change_percent = ticker.price_change_percent.parse::<f64>()
            .map_err(|e| anyhow!("Invalid priceChangePercent for {}: {} ({})", symbol, ticker.price_change_percent, e))?;
        let volume = ticker.volume.parse::<f64>()
            .map_err(|e| anyhow!("Invalid volume for {}: {} ({})", symbol, ticker.volume, e))?;
        let quote_volume = ticker.quote_volume.parse::<f64>()
            .map_err(|e| anyhow!("Invalid quoteVolume for {}: {} ({})", symbol, ticker.quote_volume, e))?;
        let high_price = ticker.high_price.parse::<f64>()
            .map_err(|e| anyhow!("Invalid highPrice for {}: {} ({})", symbol, ticker.high_price, e))?;
        let low_price = ticker.low_price.parse::<f64>()
            .map_err(|e| anyhow!("Invalid lowPrice for {}: {} ({})", symbol, ticker.low_price, e))?;

        // Validate data
        if quote_volume <= 0.0 {
            return Err(anyhow!("Invalid quote volume for {}: {}", symbol, quote_volume));
        }
        if high_price <= 0.0 || low_price <= 0.0 || low_price > high_price {
            return Err(anyhow!("Invalid price range for {}: high={}, low={}", symbol, high_price, low_price));
        }

        Ok(SymbolStats24h {
            symbol: symbol.to_string(),
            price_change_percent,
            volume,
            quote_volume,
            trades: ticker.count,
            high_price,
            low_price,
        })
    }

    /// Discover all symbols and rank them by opportunity score
    pub async fn discover_and_rank_symbols(&self) -> Result<Vec<ScoredSymbol>> {
        // 1. Tüm symbol'leri keşfet (quote asset filtrelemeli)
        let all_symbols = self.discover_symbols().await?;

        info!(
            total_symbols = all_symbols.len(),
            "Discovered {} symbols, fetching 24h stats...",
            all_symbols.len()
        );

        // 2. Her symbol için 24h stats çek (batch processing - rate limit koruması)
        // Binance rate limit: 1200 requests/minute (weight 1 per request)
        // Batch size: 50 symbols at a time to avoid rate limiting
        const BATCH_SIZE: usize = 50;
        let mut all_stats_results: Vec<Result<SymbolStats24h, anyhow::Error>> = Vec::new();

        for batch in all_symbols.chunks(BATCH_SIZE) {
            let mut stats_futures = Vec::new();
            for symbol in batch {
                let symbol_clone = symbol.clone();
                // We need to access self.venue and self.cfg, which are Arc types
                let venue = self.venue.clone();
                let futures_base = self.cfg.binance.futures_base.clone();
                stats_futures.push(async move {
                let url = format!(
                    "{}/fapi/v1/ticker/24hr?symbol={}",
                    futures_base,
                    symbol_clone
                );

                #[derive(Deserialize)]
                struct Ticker24h {
                    #[serde(rename = "priceChangePercent")]
                    price_change_percent: String,
                    #[serde(rename = "volume")]
                    volume: String,
                    #[serde(rename = "quoteVolume")]
                    quote_volume: String,
                    #[serde(rename = "count")]
                    count: u64,
                    #[serde(rename = "highPrice")]
                    high_price: String,
                    #[serde(rename = "lowPrice")]
                    low_price: String,
                }

                let resp = match venue.common.client
                    .get(&url)
                    .send()
                    .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::debug!(symbol = %symbol_clone, error = %e, "Failed to fetch 24h stats (network error)");
                        return Err(anyhow!("Network error for {}: {}", symbol_clone, e));
                    }
                };

                // Check HTTP status
                if !resp.status().is_success() {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    tracing::debug!(symbol = %symbol_clone, %status, %body, "Failed to fetch 24h stats (API error)");
                    return Err(anyhow!("API error for {}: {} - {}", symbol_clone, status, body));
                }

                let ticker: Ticker24h = match resp.json().await {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::debug!(symbol = %symbol_clone, error = %e, "Failed to parse 24h stats (JSON error)");
                        return Err(anyhow!("JSON parse error for {}: {}", symbol_clone, e));
                    }
                };

                // Parse with proper error handling
                let price_change_percent = ticker.price_change_percent.parse::<f64>()
                    .map_err(|e| anyhow!("Invalid priceChangePercent for {}: {} ({})", symbol_clone, ticker.price_change_percent, e))?;
                let volume = ticker.volume.parse::<f64>()
                    .map_err(|e| anyhow!("Invalid volume for {}: {} ({})", symbol_clone, ticker.volume, e))?;
                let quote_volume = ticker.quote_volume.parse::<f64>()
                    .map_err(|e| anyhow!("Invalid quoteVolume for {}: {} ({})", symbol_clone, ticker.quote_volume, e))?;
                let high_price = ticker.high_price.parse::<f64>()
                    .map_err(|e| anyhow!("Invalid highPrice for {}: {} ({})", symbol_clone, ticker.high_price, e))?;
                let low_price = ticker.low_price.parse::<f64>()
                    .map_err(|e| anyhow!("Invalid lowPrice for {}: {} ({})", symbol_clone, ticker.low_price, e))?;

                // Validate data
                if quote_volume <= 0.0 {
                    tracing::debug!(symbol = %symbol_clone, quote_volume, "Invalid quote volume (<= 0)");
                    return Err(anyhow!("Invalid quote volume for {}: {}", symbol_clone, quote_volume));
                }
                if high_price <= 0.0 || low_price <= 0.0 || low_price > high_price {
                    tracing::debug!(symbol = %symbol_clone, high_price, low_price, "Invalid price range");
                    return Err(anyhow!("Invalid price range for {}: high={}, low={}", symbol_clone, high_price, low_price));
                }

                Ok(SymbolStats24h {
                    symbol: symbol_clone,
                    price_change_percent,
                    volume,
                    quote_volume,
                    trades: ticker.count,
                    high_price,
                    low_price,
                })
                });
            }

            // Process this batch
            let batch_results: Vec<Result<SymbolStats24h, anyhow::Error>> = future::join_all(stats_futures).await;
            all_stats_results.extend(batch_results);

            // Rate limit koruması: batch'ler arasında kısa bir bekleme
            // 50 request / batch, 1200 request / minute = ~2.4 batch / second
            // Güvenli tarafta kalmak için batch'ler arasında 500ms bekle
            if batch.len() == BATCH_SIZE && all_stats_results.len() < all_symbols.len() {
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            }
        }

        let stats_results = all_stats_results;

        // 3. Skorlama
        let mut scored_symbols = Vec::new();
        let mut error_count = 0;
        for (symbol, stats_result) in all_symbols.iter().zip(stats_results.iter()) {
            match stats_result {
                Ok(stats) => {
                    let score = calculate_opportunity_score(stats, &self.cfg);

                    if score > 0.0 {
                        scored_symbols.push(ScoredSymbol {
                            symbol: symbol.clone(),
                            score,
                            volatility: stats.price_change_percent.abs(),
                            volume: stats.quote_volume,
                            trades: stats.trades,
                        });
                    }
                }
                Err(e) => {
                    error_count += 1;
                    // Log only first few errors to avoid spam
                    if error_count <= 5 {
                        tracing::debug!(symbol = %symbol, error = %e, "Failed to fetch/parse 24h stats");
                    }
                }
            }
        }

        if error_count > 0 {
            tracing::warn!(
                total_symbols = all_symbols.len(),
                error_count,
                "Failed to fetch 24h stats for {} symbols (some symbols may be invalid or delisted)",
                error_count
            );
        }

        // 4. En yüksek skordan düşüğe sırala
        scored_symbols.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

        Ok(scored_symbols)
    }

    /// Restart market data streams with new symbol list
    /// This closes existing WebSocket connections and starts new ones
    /// ✅ CRITICAL: Also sets leverage for new symbols (required before placing orders)
    pub async fn restart_market_streams(&self, new_symbols: Vec<String>) -> Result<()> {
        info!(
            symbol_count = new_symbols.len(),
            symbols = ?new_symbols.iter().take(10).collect::<Vec<_>>(),
            "Restarting market data streams with new symbol list"
        );

        // ✅ CRITICAL: Set leverage for new symbols before starting streams
        // Problem: New symbols added via dynamic symbol rotation don't have leverage set
        // This causes "Leverage 50 is not valid" errors when placing orders
        // Solution: Set leverage for all new symbols (same logic as startup)
        let desired_leverage = self.cfg.leverage.unwrap_or(self.cfg.exec.default_leverage);
        let use_isolated_margin = self.cfg.risk.use_isolated_margin;

        for symbol in &new_symbols {
            // ✅ CRITICAL: Clamp leverage to symbol's max leverage (if available)
            // Use centralized clamping function to avoid code duplication
            let leverage = self.get_clamped_leverage(symbol, desired_leverage).await;

            // Set margin type (isolated or cross)
            if let Err(e) = self.venue.set_margin_type(symbol, use_isolated_margin).await {
                warn!(
                    error = %e,
                    symbol = %symbol,
                    isolated = use_isolated_margin,
                    "CONNECTION: Failed to set margin type for new symbol (non-critical, continuing)"
                );
            }

            // Set leverage (only if position is closed or doesn't exist)
            // ✅ CRITICAL: Leverage mismatch with open position will cause order placement failures
            // Problem: New symbol rotation may add symbols with existing positions that have different leverage
            // Solution: Error if leverage mismatch detected (same as startup logic)
            if let Ok(position) = self.venue.get_position(symbol).await {
                if !position.qty.0.is_zero() {
                    // Position is OPEN - leverage cannot be changed
                    // Use clamped leverage for comparison
                    if position.leverage != leverage {
                        // ✅ CRITICAL: Leverage mismatch with open position is a critical error
                        // This will cause order placement to fail with "Leverage X is not valid" error
                        // Cannot auto-fix while position is open - must close position first
                        error!(
                            symbol = %symbol,
                            current_leverage = position.leverage,
                            desired_leverage = desired_leverage,
                            clamped_leverage = leverage,
                            "CONNECTION: CRITICAL - Leverage mismatch detected for new symbol (position is open, cannot change). Order placement will fail!"
                        );
                        // Note: We don't return error here to allow other symbols to be processed
                        // But this is logged as ERROR to make it visible
                        // ORDERING module should handle this by closing position or rejecting orders
                    } else {
                        info!(
                            symbol = %symbol,
                            leverage,
                            "CONNECTION: Leverage verified for new symbol (matches clamped leverage, position is open)"
                        );
                    }
                } else {
                    // Position is CLOSED - safe to set leverage
                    // ✅ CRITICAL: Re-fetch symbol rules to get max leverage (may have changed or not been fetched yet)
                    // Use same safe fallback as startup
                    const SAFE_LEVERAGE_FALLBACK: u32 = 50;
                    
                    let final_leverage = match self.venue.rules_for(symbol).await {
                        Ok(rules) => {
                            if let Some(symbol_max_lev) = rules.max_leverage {
                                leverage.min(symbol_max_lev)
                            } else {
                                leverage.min(SAFE_LEVERAGE_FALLBACK)
                            }
                        }
                        Err(_) => {
                            leverage.min(SAFE_LEVERAGE_FALLBACK)
                        }
                    };
                    
                    if position.leverage != final_leverage {
                        warn!(
                            symbol = %symbol,
                            current_leverage = position.leverage,
                            desired_leverage = desired_leverage,
                            final_leverage = final_leverage,
                            "CONNECTION: Leverage mismatch detected for new symbol (position is closed). Auto-fixing..."
                        );
                        match self.venue.set_leverage(symbol, final_leverage).await {
                            Ok(actual_leverage) => {
                                if actual_leverage != final_leverage {
                                    warn!(
                                        symbol = %symbol,
                                        desired_leverage = desired_leverage,
                                        clamped_leverage = final_leverage,
                                        actual_leverage,
                                        "CONNECTION: Leverage set to lower value than clamped (step-down mechanism)"
                                    );
                                }
                                // Verify the fix worked
                                if let Ok(updated_position) = self.venue.get_position(symbol).await {
                                    if updated_position.leverage == actual_leverage {
                                        info!(
                                            symbol = %symbol,
                                            old_leverage = position.leverage,
                                            new_leverage = actual_leverage,
                                            "CONNECTION: Leverage mismatch auto-fixed successfully for new symbol (position was closed)"
                                        );
                                    } else {
                                        warn!(
                                            symbol = %symbol,
                                            expected_leverage = actual_leverage,
                                            actual_leverage = updated_position.leverage,
                                            "CONNECTION: Leverage fix may not have taken effect immediately (will retry on next check)"
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                error!(
                                    error = %e,
                                    symbol = %symbol,
                                    current_leverage = position.leverage,
                                    desired_leverage = desired_leverage,
                                    final_leverage = final_leverage,
                                    "CONNECTION: Failed to auto-fix leverage mismatch for new symbol (all leverage values failed)"
                                );
                                // Note: We don't return error here to allow other symbols to be processed
                            }
                        }
                    } else {
                        info!(
                            symbol = %symbol,
                            leverage = final_leverage,
                            "CONNECTION: Leverage verified for new symbol (matches clamped leverage, position is closed)"
                        );
                    }
                }
            } else {
                // No position exists - safe to set leverage
                // Leverage is already clamped above, so use it directly
                match self.venue.set_leverage(symbol, leverage).await {
                    Ok(actual_leverage) => {
                        if actual_leverage != leverage {
                            warn!(
                                symbol = %symbol,
                                desired_leverage = desired_leverage,
                                actual_leverage,
                                "CONNECTION: Leverage set to lower value than desired for new symbol (step-down mechanism)"
                            );
                        } else {
                            info!(
                                symbol = %symbol,
                                leverage = actual_leverage,
                                "CONNECTION: Leverage set successfully for new symbol"
                            );
                        }
                    }
                    Err(e) => {
                        error!(
                            error = %e,
                            symbol = %symbol,
                            leverage,
                            "CONNECTION: CRITICAL - Failed to set leverage for new symbol (all leverage values failed). Order placement will fail!"
                        );
                        // Note: We don't return error here to allow other symbols to be processed
                        // But this is logged as ERROR to make it visible
                    }
                }
            }
        }

        // Note: WebSocket streams are managed by tokio::spawn tasks
        // They will automatically reconnect if they fail
        // For a proper restart, we would need to track and close existing streams
        // For now, we'll just start new streams (old ones will eventually timeout)
        // TODO: Implement proper stream management with cancellation tokens
        
        // Start new market data streams
        self.start_market_data_stream(new_symbols).await?;

        Ok(())
    }
}

/// Calculate opportunity score for a symbol based on 24h statistics
fn calculate_opportunity_score(stats: &SymbolStats24h, cfg: &AppCfg) -> f64 {
    let ds_cfg = &cfg.dynamic_symbol_selection;

    // 1. Volatilite skoru (en önemli)
    let volatility_abs = stats.price_change_percent.abs();
    
    // Minimum volatilite kontrolü
    if volatility_abs < ds_cfg.min_volatility_pct {
        return 0.0; // Çok düşük volatilite = işlem yapma
    }

    let volatility_score = if volatility_abs < 3.0 {
        1.0 // Orta volatilite
    } else if volatility_abs < 6.0 {
        2.0 // İyi volatilite
    } else if volatility_abs < 10.0 {
        3.0 // Harika volatilite
    } else {
        2.5 // Çok yüksek = riskli, biraz cezalandır
    };

    // 2. Volume skoru (likidite için)
    if stats.quote_volume < ds_cfg.min_quote_volume {
        return 0.0; // Çok düşük volume = slippage riski
    }

    let volume_score = if stats.quote_volume < 10_000_000.0 {
        0.5 // Düşük volume
    } else if stats.quote_volume < 50_000_000.0 {
        1.0 // Normal volume
    } else {
        1.5 // Yüksek volume = iyi likidite
    };

    // 3. İşlem sayısı skoru (aktivite için)
    if stats.trades < ds_cfg.min_trades_24h {
        return 0.0; // Çok az işlem
    }

    let trades_score = if stats.trades < 10000 {
        0.5
    } else {
        1.0
    };

    // 4. Spread tahmini (24h high-low range - bu gerçek spread değil, sadece volatilite göstergesi)
    // Not: Gerçek spread (bid-ask) için @bookTicker stream'inden alınmalı
    // Burada 24h high-low range'i kullanıyoruz, bu da volatilite göstergesi olarak işlev görür
    let price_mid = (stats.high_price + stats.low_price) / 2.0;
    let range_pct = if price_mid > 0.0 {
        ((stats.high_price - stats.low_price) / price_mid) * 100.0
    } else {
        100.0 // Hata durumu
    };

    // Range çok genişse (aşırı volatilite) cezalandır, dar ise (stabil) ödüllendir
    // Ancak çok dar range (düşük volatilite) zaten min_volatility_pct ile filtreleniyor
    let spread_score = if range_pct > 20.0 {
        0.5 // Çok geniş range = aşırı volatilite, riskli
    } else if range_pct > 10.0 {
        0.8 // Geniş range = yüksek volatilite
    } else if range_pct > 5.0 {
        1.0 // Normal range
    } else {
        0.9 // Dar range = düşük volatilite (ama zaten min_volatility_pct geçti)
    };

    // TOPLAM SKOR (ağırlıklı)
    let total_score = (volatility_score * ds_cfg.volatility_weight)
        + (volume_score * ds_cfg.volume_weight)
        + (trades_score * ds_cfg.trades_weight)
        + (spread_score * ds_cfg.spread_weight);

    total_score
}

