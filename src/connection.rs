
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
pub struct Connection {
    venue: Arc<BinanceFutures>,
    cfg: Arc<AppCfg>,
    event_bus: Arc<EventBus>,
    shutdown_flag: Arc<AtomicBool>,
    shared_state: Option<Arc<SharedState>>,
    order_fill_history: Arc<DashMap<String, OrderFillHistory>>,
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
            shared_state,
            order_fill_history: Arc::new(DashMap::new()),
        })
    }
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
            shared_state: None,
            order_fill_history: Arc::new(DashMap::new()),
        }
    }
    pub async fn get_clamped_leverage(&self, symbol: &str, desired_leverage: u32) -> u32 {
        let safe_leverage_fallback = self.cfg.exec.default_leverage;
        match self.venue.rules_for(symbol).await {
            Ok(rules) => {
                if let Some(symbol_max_lev) = rules.max_leverage {
                    let clamped = desired_leverage.min(symbol_max_lev);
                    if clamped < desired_leverage {
                        debug!(
                            symbol = %symbol,
                            desired_leverage,
                            coin_max_leverage = symbol_max_lev,
                            final_leverage = clamped,
                            "CONNECTION: Leverage clamped to coin's max leverage"
                        );
                    }
                    clamped
                } else {
                    if let Some(coin_max_lev) = self.venue.fetch_max_leverage(symbol).await {
                        let clamped = desired_leverage.min(coin_max_lev);
                        if clamped < desired_leverage {
                            debug!(
                                symbol = %symbol,
                                desired_leverage,
                                coin_max_leverage = coin_max_lev,
                                final_leverage = clamped,
                                "CONNECTION: Leverage clamped to coin's max leverage (fetched directly)"
                            );
                        }
                        clamped
                    } else {
                        let clamped = desired_leverage.min(safe_leverage_fallback);
                        warn!(
                            symbol = %symbol,
                            desired_leverage,
                            safe_fallback = safe_leverage_fallback,
                            final_leverage = clamped,
                            "CONNECTION: Coin's max leverage unavailable, using safe fallback ({}x) - this should be rare",
                            safe_leverage_fallback
                        );
                        clamped
                    }
                }
            }
            Err(e) => {
                if let Some(coin_max_lev) = self.venue.fetch_max_leverage(symbol).await {
                    let clamped = desired_leverage.min(coin_max_lev);
                    debug!(
                        symbol = %symbol,
                        desired_leverage,
                        coin_max_leverage = coin_max_lev,
                        final_leverage = clamped,
                        "CONNECTION: Leverage clamped to coin's max leverage (fetched after rules_for failed)"
                    );
                    clamped
                } else {
                    warn!(
                        error = %e,
                        symbol = %symbol,
                        safe_fallback = safe_leverage_fallback,
                        "CONNECTION: Failed to fetch coin info, using safe fallback ({}x) as last resort",
                        safe_leverage_fallback
                    );
                    desired_leverage.min(safe_leverage_fallback)
                }
            }
        }
    }
    pub async fn start(&self, symbols: Vec<String>) -> Result<()> {
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
            let leverage = self.get_clamped_leverage(symbol, desired_leverage).await;
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
                                if actual_leverage != final_leverage {
                                    warn!(
                                        symbol = %symbol,
                                        desired_leverage = leverage,
                                        clamped_leverage = final_leverage,
                                        actual_leverage,
                                        "CONNECTION: Leverage set to lower value than clamped (step-down mechanism)"
                                    );
                                }
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
        self.start_market_data_stream(symbols.clone()).await?;
        self.start_user_data_stream(symbols.clone()).await?;
        self.start_rules_refresh_task().await;
        self.start_order_fill_history_cleanup_task().await;
        self.start_order_monitoring_fallback_task().await;
        info!("CONNECTION: All streams started");
        Ok(())
    }
    async fn start_order_fill_history_cleanup_task(&self) {
        let order_fill_history = self.order_fill_history.clone();
        let shutdown_flag = self.shutdown_flag.clone();
        tokio::spawn(async move {
        const CLEANUP_INTERVAL_SECS: u64 = 600;
        const MAX_AGE_SECS: u64 = 3600;
        loop {
            tokio::time::sleep(Duration::from_secs(CLEANUP_INTERVAL_SECS)).await;
            if shutdown_flag.load(AtomicOrdering::Relaxed) {
                break;
            }
            let now = Instant::now();
            let initial_count = order_fill_history.len();
            order_fill_history.retain(|order_id, history| {
                let age = now.duration_since(history.last_update);
                let age_secs = age.as_secs();
                if age_secs > MAX_AGE_SECS {
                    warn!(
                        order_id = %order_id,
                        history_age_secs = age_secs,
                        history_age_hours = age_secs / 3600,
                        "CONNECTION: Order fill history is very old ({} hours), force removing to prevent memory leak",
                        age_secs / 3600
                    );
                    return false;
                }
                let is_in_cache = OPEN_ORDERS_CACHE.iter().any(|entry| {
                    entry.value().iter().any(|o| o.order_id == *order_id)
                });
                if is_in_cache {
                    true
                } else {
                    const SHORT_DELAY_SECS: u64 = 30;
                    if age_secs > SHORT_DELAY_SECS {
                        debug!(
                            order_id = %order_id,
                            history_age_secs = age_secs,
                            "CONNECTION: Order fill history not in cache (filled/cancelled), removing after {} seconds delay",
                            age_secs
                        );
                        false
                    } else {
                        true
                    }
                }
            });
            let final_count = order_fill_history.len();
            let removed_count = initial_count.saturating_sub(final_count);
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
    async fn start_order_monitoring_fallback_task(&self) {
        let venue = self.venue.clone();
        let shutdown_flag = self.shutdown_flag.clone();
        tokio::spawn(async move {
            const MONITORING_INTERVAL_SECS: u64 = 15;
            loop {
                tokio::time::sleep(Duration::from_secs(MONITORING_INTERVAL_SECS)).await;
                if shutdown_flag.load(AtomicOrdering::Relaxed) {
                    break;
                }
                match venue.get_all_positions().await {
                    Ok(positions) => {
                        for (symbol, position) in positions {
                            match venue.get_open_orders(&symbol).await {
                                Ok(orders) => {
                                    let has_open_orders = !orders.is_empty();
                                    if !has_open_orders && !position.qty.0.is_zero() {
                                        warn!(
                                            symbol = %symbol,
                                            qty = %position.qty.0,
                                            "CONNECTION: TP/SL missing for open position, triggering re-placement"
                                        );
                                    }
                                }
                                Err(e) => {
                                    warn!(
                                        error = %e,
                                        symbol = %symbol,
                                        "CONNECTION: Failed to fetch open orders for monitoring"
                                    );
                                }
                            }
                        }
                    }
                    Err(e) => {
                        warn!(
                            error = %e,
                            "CONNECTION: Failed to fetch positions for order monitoring"
                        );
                    }
                }
            }
        });
    }
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
        const MAX_SYMBOLS_PER_STREAM: usize = 10;
        info!(
            total_symbols = unique_symbols.len(),
            streams_needed = (unique_symbols.len() + MAX_SYMBOLS_PER_STREAM - 1) / MAX_SYMBOLS_PER_STREAM,
            "CONNECTION: setting up market data websocket streams"
        );
        for chunk in unique_symbols.chunks(MAX_SYMBOLS_PER_STREAM) {
            let symbols_chunk = chunk.to_vec();
            let event_bus = self.event_bus.clone();
            let shutdown_flag = self.shutdown_flag.clone();
            tokio::spawn(async move {
                const INITIAL_DELAY_SECS: u64 = 1;
                const MAX_DELAY_SECS: u64 = 10;
                let mut connection_retry_delay = INITIAL_DELAY_SECS;
                let mut stream_retry_delay = INITIAL_DELAY_SECS;
                loop {
                    if shutdown_flag.load(AtomicOrdering::Relaxed) {
                        break;
                    }
                    match MarketDataStream::connect(&symbols_chunk).await {
                        Ok(mut stream) => {
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
                                        let symbol = price_update.symbol.clone();
                                        PRICE_CACHE.insert(symbol.clone(), price_update.clone());
                                        let market_tick = MarketTick {
                                            symbol,
                                            bid: price_update.bid,
                                            ask: price_update.ask,
                                            mark_price: None,
                                            volume: None,
                                            timestamp: Instant::now(),
                                        };
                                        let receiver_count = event_bus.market_tick_tx.receiver_count();
                                        if receiver_count == 0 {
                                            tracing::debug!(
                                                symbol = %price_update.symbol,
                                                "CONNECTION: No MarketTick subscribers, skipping event"
                                            );
                                        } else {
                                            match event_bus.market_tick_tx.send(market_tick) {
                                                Ok(receiver_count) => {
                                                    if receiver_count == 0 {
                                                        tracing::debug!(
                                                            symbol = %price_update.symbol,
                                                            "CONNECTION: MarketTick sent but no subscribers (normal if modules haven't started yet)"
                                                        );
                                                    }
                                                }
                                                Err(_) => {
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
                                        stream_retry_delay = (stream_retry_delay * 2).min(MAX_DELAY_SECS);
                                        break;
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
                    let venue_for_validation = venue.clone();
                    let symbols_for_validation = symbols.clone();
                    let shared_state_for_validation_clone = shared_state_for_validation.clone();
                    stream.set_on_reconnect(move || {
                        info!("CONNECTION: WebSocket reconnected - Binance will automatically send state updates via ACCOUNT_UPDATE and ORDER_TRADE_UPDATE events");
                        let venue_clone = venue_for_validation.clone();
                        let symbols_clone = symbols_for_validation.clone();
                        let shared_state_for_validation = shared_state_for_validation_clone.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_secs(2)).await;
                            info!(
                                symbol_count = symbols_clone.len(),
                                "CONNECTION: Validating state after WebSocket reconnect (priority-based validation)"
                            );
                            let (priority_symbols, cached_symbols, remaining_symbols) = {
                                let mut priority = Vec::new();
                                let mut cached = Vec::new();
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
                                for entry in POSITION_CACHE.iter() {
                                    let symbol = entry.key().clone();
                                    if !priority.contains(&symbol) {
                                        cached.push(symbol);
                                    }
                                }
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
                            let validate_symbols_batch = {
                                let venue = venue_clone.clone();
                                let timeout = symbol_timeout;
                                move |symbols: Vec<String>| {
                                    let venue = venue.clone();
                                    symbols.into_iter().map(move |symbol| {
                                        let venue = venue.clone();
                                        let symbol_clone = symbol.clone();
                                        tokio::spawn(async move {
                                            let position_result = tokio::time::timeout(
                                                timeout,
                                                venue.get_position(&symbol_clone)
                                            ).await;
                                            let orders_result = tokio::time::timeout(
                                                timeout,
                                                venue.get_open_orders(&symbol_clone)
                                            ).await;
                                            (symbol_clone, position_result, orders_result)
                                        })
                                    }).collect::<Vec<_>>()
                                }
                            };
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
                            let balance_timeout = Duration::from_secs(5);
                            const MAX_BALANCE_VALIDATION_TIME: Duration = Duration::from_secs(10);
                            for asset in &["USDT", "USDC"] {
                                if validation_start.elapsed() > MAX_BALANCE_VALIDATION_TIME {
                                    warn!(
                                        asset = %asset,
                                        "CONNECTION: Balance validation timeout reached, skipping"
                                    );
                                    break;
                                }
                                match tokio::time::timeout(balance_timeout, venue_clone.available_balance(asset)).await {
                                    Ok(Ok(rest_balance)) => {
                                        if let Some(ws_balance) = BALANCE_CACHE.get(*asset) {
                                            let balance_diff = (rest_balance - *ws_balance.value()).abs();
                                            if balance_diff > Decimal::from_str("0.01").unwrap_or(Decimal::ZERO) {
                                                warn!(
                                                    asset = %asset,
                                                    rest_balance = %rest_balance,
                                                    ws_balance = %ws_balance.value(),
                                                    balance_diff = %balance_diff,
                                                    "CONNECTION: Balance mismatch after reconnect (REST vs WebSocket)"
                                                );
                                            }
                                        } else if !rest_balance.is_zero() {
                                            warn!(
                                                asset = %asset,
                                                rest_balance = %rest_balance,
                                                "CONNECTION: Balance exists in REST API but missing from WebSocket cache after reconnect"
                                            );
                                        }
                                    }
                                    Ok(Err(e)) => {
                                        debug!(
                                            asset = %asset,
                                            error = %e,
                                            "CONNECTION: Failed to fetch balance for validation after reconnect"
                                        );
                                    }
                                    Err(_) => {
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
                                }
                                match event {
                                    UserEvent::OrderFill {
                                        symbol,
                                        order_id,
                                        side,
                                        qty: last_filled_qty,
                                        cumulative_filled_qty,
                                        order_qty,
                                        price: last_fill_price,
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
                                            Qty(Decimal::ZERO)
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
                                            history.weighted_price_sum += last_fill_price.0 * last_filled_qty.0;
                                            history.total_filled_qty = cumulative_filled_qty;
                                            history.last_update = now;
                                            history.total_fill_count += 1;
                                            if is_maker {
                                                history.maker_fill_count += 1;
                                            }
                                            let avg_price = if !cumulative_filled_qty.0.is_zero() {
                                                Px(history.weighted_price_sum / cumulative_filled_qty.0)
                                            } else {
                                                last_fill_price
                                            };
                                            let all_maker = history.maker_fill_count == history.total_fill_count;
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
                            let mut order_entry = OPEN_ORDERS_CACHE.entry(symbol.clone()).or_insert_with(Vec::new);
                                        let existing_order_idx = order_entry.iter().position(|o| o.order_id == order_id);
                                        match status {
                                            OrderStatus::Filled | OrderStatus::Canceled | OrderStatus::Expired | OrderStatus::ExpiredInMatch | OrderStatus::Rejected => {
                                                if let Some(idx) = existing_order_idx {
                                                    order_entry.remove(idx);
                                                }
                                                if order_entry.is_empty() {
                                                    OPEN_ORDERS_CACHE.remove(&symbol);
                                                }
                                            }
                                            OrderStatus::New | OrderStatus::PartiallyFilled => {
                                                let venue_order = VenueOrder {
                                                    order_id: order_id.clone(),
                                                    side,
                                                    price: last_fill_price,
                                                    qty: total_order_qty,
                                                };
                                                if let Some(idx) = existing_order_idx {
                                                    order_entry[idx] = venue_order;
                                                } else {
                                                    order_entry.push(venue_order);
                                                }
                                            }
                                        }
                                        if matches!(
                                            status,
                                            OrderStatus::Filled
                                                | OrderStatus::Canceled
                                                | OrderStatus::Expired
                                                | OrderStatus::ExpiredInMatch
                                        ) {
                                            order_fill_history.remove(&order_id);
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
                                        let rejection_reason = if status == OrderStatus::Rejected {
                                            Some(format!("Order rejected: {}", order_status))
                                        } else {
                                            None
                                        };
                                        let order_update = OrderUpdate {
                                            symbol: symbol.clone(),
                                            order_id,
                                            side,
                                            last_fill_price,
                                            average_fill_price,
                                            qty: total_order_qty,
                                            filled_qty: cumulative_filled_qty,
                                            remaining_qty,
                                            status,
                                            rejection_reason,
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
                                                let delay_ms = BASE_DELAY_MS * (attempt + 1) as u64;
                                                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                                                if let Ok(position) = venue_clone.get_position(&symbol_clone).await {
                                                    let position_update = PositionUpdate {
                                                        symbol: symbol_clone,
                                                        qty: position.qty,
                                                        entry_price: position.entry,
                                                        leverage: position.leverage,
                                                        unrealized_pnl: None,
                                                        is_open: !position.qty.0.is_zero(),
                                                        liq_px: position.liq_px,
                                                        timestamp: Instant::now(),
                                                    };
                                                    if let Err(e) = event_bus_pos.position_update_tx.send(position_update) {
                                                        error!(
                                                            error = ?e,
                                                            "CONNECTION: Failed to send PositionUpdate event (no subscribers or channel closed)"
                                                        );
                                                    }
                                                    break;
                                                }
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
                                        if let Some(mut orders) = OPEN_ORDERS_CACHE.get_mut(&symbol) {
                                            orders.retain(|o| o.order_id != order_id);
                                            if orders.is_empty() {
                                                drop(orders);
                                                OPEN_ORDERS_CACHE.remove(&symbol);
                                            }
                                        }
                                    }
                                    UserEvent::AccountUpdate { positions, balances } => {
                                        for pos in &positions {
                                            let position = Position {
                                                symbol: pos.symbol.clone(),
                                                qty: Qty(pos.position_amt),
                                                entry: Px(pos.entry_price),
                                                leverage: pos.leverage,
                                                liq_px: None,
                                            };
                                            POSITION_CACHE.insert(pos.symbol.clone(), position);
                                        }
                                        for bal in &balances {
                                            BALANCE_CACHE.insert(bal.asset.clone(), bal.available_balance);
                                        }
                                        for pos in positions {
                                            let position_update = PositionUpdate {
                                                symbol: pos.symbol,
                                                qty: Qty(pos.position_amt),
                                                entry_price: Px(pos.entry_price),
                                                leverage: pos.leverage,
                                                unrealized_pnl: pos.unrealized_pnl,
                                                is_open: !pos.position_amt.is_zero(),
                                                liq_px: None,
                                                timestamp: Instant::now(),
                                            };
                                            if let Err(e) = event_bus.position_update_tx.send(position_update) {
                                                error!(
                                                    error = ?e,
                                                    "CONNECTION: Failed to send PositionUpdate event (no subscribers or channel closed)"
                                                );
                                            }
                                        }
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
    async fn process_validation_results(
        results: Vec<Result<(String, Result<Result<Position, anyhow::Error>, tokio::time::error::Elapsed>, Result<Result<Vec<VenueOrder>, anyhow::Error>, tokio::time::error::Elapsed>), tokio::task::JoinError>>,
        _venue: &Arc<BinanceFutures>,
    ) -> usize {
        let mut validated_count = 0;
        for result in results {
            match result {
                Ok((symbol, position_result, orders_result)) => {
                    match position_result {
                        Ok(Ok(rest_position)) => {
                            if let Some(ws_position) = POSITION_CACHE.get(&symbol) {
                                let ws_pos = ws_position.value();
                                let qty_diff = (rest_position.qty.0 - ws_pos.qty.0).abs();
                                let entry_diff = (rest_position.entry.0 - ws_pos.entry.0).abs();
                                let significant_qty_diff = Decimal::from_str("0.0001").unwrap_or(Decimal::ZERO);
                                let significant_entry_diff = Decimal::from_str("0.01").unwrap_or(Decimal::ZERO);
                                let significant_mismatch = qty_diff > significant_qty_diff
                                    || entry_diff > significant_entry_diff;
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
                        }
                    }
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
                        }
                    }
                }
                Err(_) => {
                }
            }
        }
        validated_count
    }
    pub async fn send_order(&self, command: OrderCommand) -> Result<String> {
        // Weight-based rate limiting: POST /fapi/v1/order = weight 1
        crate::utils::rate_limit_guard(1).await;
        use std::time::{SystemTime, UNIX_EPOCH};
        let client_order_id = format!(
            "{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_else(|_| {
                    warn!("System time is before UNIX epoch, using fallback timestamp");
                    Duration::from_secs(0)
                })
                .as_millis()
        );
        match command {
            OrderCommand::Open { symbol, side, price, qty, tif } => {
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
    async fn validate_order_before_send(
        &self,
        symbol: &str,
        price: Px,
        qty: Qty,
        _side: Side,
        _is_open_order: bool,
    ) -> Result<()> {
        let rules = self.venue.rules_for(symbol).await
        .map_err(|e| anyhow!("Failed to fetch symbol rules for {}: {}", symbol, e))?;
        let (_, _, price_quantized, qty_quantized) = BinanceFutures::validate_and_format_order_params(
        price,
        qty,
        &rules,
        symbol,
        )?;
        let notional = price_quantized * qty_quantized;
        if !rules.min_notional.is_zero() && notional < rules.min_notional {
        return Err(anyhow!(
            "Order notional below minimum: {} < {} for symbol {}",
            notional,
            rules.min_notional,
            symbol
        ));
        }
        Ok(())
    }
    pub async fn fetch_balance(&self, asset: &str) -> Result<Decimal> {
        // Weight-based rate limiting: GET /fapi/v2/balance = weight 5
        crate::utils::rate_limit_guard(5).await;
        self.venue.available_balance(asset).await
    }
    pub async fn get_current_prices(&self, symbol: &str) -> Result<(Px, Px)> {
        if let Some(price_update) = PRICE_CACHE.get(symbol) {
            return Ok((price_update.bid, price_update.ask));
        }
        warn!(
            symbol = %symbol,
            "PRICE_CACHE empty, falling back to REST API (should be rare - WebSocket should populate cache)"
        );
        self.venue.best_prices(symbol).await
    }
    pub async fn rules_for(&self, sym: &str) -> Result<Arc<crate::types::SymbolRules>> {
        self.venue.rules_for(sym).await
    }
    pub async fn flatten_position(&self, symbol: &str, use_market_only: bool) -> Result<()> {
        self.venue.flatten_position(symbol, use_market_only).await
    }
    pub async fn get_all_positions(&self) -> Result<Vec<(String, Position)>> {
        self.venue.get_all_positions().await
    }
    pub async fn place_trailing_stop_order(
        &self,
        symbol: &str,
        activation_price: Px,
        callback_rate: f64,
        quantity: Qty,
    ) -> Result<String> {
        self.venue.place_trailing_stop_order(symbol, activation_price, callback_rate, quantity).await
    }
    pub async fn cancel_order(&self, order_id: &str, symbol: &str) -> Result<()> {
        // Weight-based rate limiting: DELETE /fapi/v1/order = weight 1
        crate::utils::rate_limit_guard(1).await;
        self.venue.cancel(order_id, symbol).await
    }
    async fn start_rules_refresh_task(&self) {
    let shutdown_flag = self.shutdown_flag.clone();
    tokio::spawn(async move {
        const REFRESH_INTERVAL_MINUTES: u64 = 15;
        const REFRESH_INTERVAL_SECS: u64 = REFRESH_INTERVAL_MINUTES * 60;
        info!(
        interval_minutes = REFRESH_INTERVAL_MINUTES,
        "CONNECTION: Started symbol rules refresh task"
        );
        loop {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(REFRESH_INTERVAL_SECS)) => {
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
                for sym in &symbols_to_refresh {
                    FUT_RULES.remove(sym);
                }
                info!(
                    symbol_count = symbols_to_refresh.len(),
                    "CONNECTION: Rules cache invalidated, fresh rules will be fetched on next access"
                );
            }
            _ = async {
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
    pub async fn discover_symbols(&self) -> Result<Vec<String>> {
    let quote_asset_upper = self.cfg.quote_asset.to_uppercase();
    let allow_usdt = self.cfg.allow_usdt_quote;
    let min_required_balance = Decimal::from_str(
        &self.cfg.min_usd_per_order.to_string()
    ).unwrap_or(Decimal::from(10));
    let mut usdt_balance = Decimal::ZERO;
    let mut usdc_balance = Decimal::ZERO;
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
    let mut allowed_quotes = Vec::new();
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
        allowed_quotes.push(quote_asset_upper.clone());
    }
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
    let all_symbols = self.venue.symbol_metadata().await?;
    let discovered: Vec<String> = all_symbols
        .into_iter()
        .filter(|meta| {
            let quote_match = allowed_quotes.contains(&meta.quote_asset.to_uppercase());
            let status_match = meta.status.as_deref() == Some("TRADING");
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
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("API error for {}: {} - {}", symbol, status, body));
        }
        let ticker: Ticker24h = resp.json().await?;
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
    pub async fn discover_and_rank_symbols(&self) -> Result<Vec<ScoredSymbol>> {
        let all_symbols = self.discover_symbols().await?;
        info!(
            total_symbols = all_symbols.len(),
            "Discovered {} symbols, fetching 24h stats...",
            all_symbols.len()
        );
        const BATCH_SIZE: usize = 50;
        let mut all_stats_results: Vec<Result<SymbolStats24h, anyhow::Error>> = Vec::new();
        for batch in all_symbols.chunks(BATCH_SIZE) {
            let mut stats_futures = Vec::new();
            for symbol in batch {
                let symbol_clone = symbol.clone();
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
            let batch_results: Vec<Result<SymbolStats24h, anyhow::Error>> = future::join_all(stats_futures).await;
            all_stats_results.extend(batch_results);
            if batch.len() == BATCH_SIZE && all_stats_results.len() < all_symbols.len() {
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            }
        }
        let stats_results = all_stats_results;
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
        scored_symbols.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        Ok(scored_symbols)
    }
    pub async fn restart_market_streams(&self, new_symbols: Vec<String>) -> Result<()> {
        info!(
            symbol_count = new_symbols.len(),
            symbols = ?new_symbols.iter().take(10).collect::<Vec<_>>(),
            "Restarting market data streams with new symbol list"
        );
        let desired_leverage = self.cfg.leverage.unwrap_or(self.cfg.exec.default_leverage);
        let use_isolated_margin = self.cfg.risk.use_isolated_margin;
        for symbol in &new_symbols {
            let leverage = self.get_clamped_leverage(symbol, desired_leverage).await;
            if let Err(e) = self.venue.set_margin_type(symbol, use_isolated_margin).await {
                warn!(
                    error = %e,
                    symbol = %symbol,
                    isolated = use_isolated_margin,
                    "CONNECTION: Failed to set margin type for new symbol (non-critical, continuing)"
                );
            }
            if let Ok(position) = self.venue.get_position(symbol).await {
                if !position.qty.0.is_zero() {
                    if position.leverage != leverage {
                        error!(
                            symbol = %symbol,
                            current_leverage = position.leverage,
                            desired_leverage = desired_leverage,
                            clamped_leverage = leverage,
                            "CONNECTION: CRITICAL - Leverage mismatch detected for new symbol (position is open, cannot change). Order placement will fail!"
                        );
                    } else {
                        info!(
                            symbol = %symbol,
                            leverage,
                            "CONNECTION: Leverage verified for new symbol (matches clamped leverage, position is open)"
                        );
                    }
                } else {
                    let safe_leverage_fallback = self.cfg.exec.default_leverage;
                    let final_leverage = match self.venue.rules_for(symbol).await {
                        Ok(rules) => {
                            if let Some(symbol_max_lev) = rules.max_leverage {
                                leverage.min(symbol_max_lev)
                            } else {
                                leverage.min(safe_leverage_fallback)
                            }
                        }
                        Err(_) => {
                            leverage.min(safe_leverage_fallback)
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
                    }
                }
            }
        }
        self.start_market_data_stream(new_symbols).await?;
        Ok(())
    }
}
fn calculate_opportunity_score(stats: &SymbolStats24h, cfg: &AppCfg) -> f64 {
    let ds_cfg = &cfg.dynamic_symbol_selection;
    let volatility_abs = stats.price_change_percent.abs();
    if volatility_abs < ds_cfg.min_volatility_pct {
        return 0.0;
    }
    let volatility_score = if volatility_abs < 3.0 {
        1.0
    } else if volatility_abs < 6.0 {
        2.0
    } else if volatility_abs < 10.0 {
        3.0
    } else {
        2.5
    };
    if stats.quote_volume < ds_cfg.min_quote_volume {
        return 0.0;
    }
    let volume_score = if stats.quote_volume < 10_000_000.0 {
        0.5
    } else if stats.quote_volume < 50_000_000.0 {
        1.0
    } else {
        1.5
    };
    if stats.trades < ds_cfg.min_trades_24h {
        return 0.0;
    }
    let trades_score = if stats.trades < 10000 {
        0.5
    } else {
        1.0
    };
    let price_mid = (stats.high_price + stats.low_price) / 2.0;
    let range_pct = if price_mid > 0.0 {
        ((stats.high_price - stats.low_price) / price_mid) * 100.0
    } else {
        100.0
    };
    let spread_score = if range_pct > 20.0 {
        0.5
    } else if range_pct > 10.0 {
        0.8
    } else if range_pct > 5.0 {
        1.0
    } else {
        0.9
    };
    let total_score = (volatility_score * ds_cfg.volatility_weight)
        + (volume_score * ds_cfg.volume_weight)
        + (trades_score * ds_cfg.trades_weight)
        + (spread_score * ds_cfg.spread_weight);
    total_score
}
