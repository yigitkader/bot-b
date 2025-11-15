// ORDERING: Order placement/closure, single position guarantee
// Only handles order placement/closure, no trend or PnL logic
// Listens to TradeSignal and CloseRequest events
// Uses CONNECTION to send orders

use crate::config::AppCfg;
use crate::connection::Connection;
use crate::event_bus::{CloseRequest, EventBus, OrderUpdate, PositionUpdate, TradeSignal};
use crate::state::{OpenPosition, OpenOrder, SharedState};
use crate::types::{Qty, Side, Tif, PositionDirection};
use anyhow::{anyhow, Result};
use rust_decimal::Decimal;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// Maximum number of retry attempts for order placement
const MAX_RETRIES: u32 = 3;

/// ORDERING module - order placement and closure
/// Guarantees single position/order at a time using global lock
pub struct Ordering {
    cfg: Arc<AppCfg>,
    connection: Arc<Connection>,
    event_bus: Arc<EventBus>,
    shutdown_flag: Arc<AtomicBool>,
    shared_state: Arc<SharedState>,
}

impl Ordering {
    /// Create a new Ordering module instance.
    ///
    /// The Ordering module is responsible for placing and managing orders. It guarantees that
    /// only one position/order is open at a time using a global lock mechanism.
    ///
    /// # Arguments
    ///
    /// * `cfg` - Application configuration containing execution parameters (TIF, leverage)
    /// * `connection` - Connection instance for sending orders to the exchange
    /// * `event_bus` - Event bus for subscribing to TradeSignal/CloseRequest and publishing events
    /// * `shutdown_flag` - Shared flag to signal graceful shutdown
    /// * `shared_state` - Shared state for maintaining ordering state (open position/order)
    ///
    /// # Returns
    ///
    /// Returns a new `Ordering` instance. Call `start()` to begin processing trade signals.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # let cfg = Arc::new(crate::config::load_config()?);
    /// # let connection = Arc::new(crate::connection::Connection::from_config(todo!(), todo!(), todo!(), None)?);
    /// # let event_bus = Arc::new(crate::event_bus::EventBus::new());
    /// # let shutdown_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    /// # let shared_state = Arc::new(crate::state::SharedState::new());
    /// let ordering = Ordering::new(cfg, connection, event_bus, shutdown_flag, shared_state);
    /// ordering.start().await?;
    /// ```
    pub fn new(
        cfg: Arc<AppCfg>,
        connection: Arc<Connection>,
        event_bus: Arc<EventBus>,
        shutdown_flag: Arc<AtomicBool>,
        shared_state: Arc<SharedState>,
    ) -> Self {
        Self {
            cfg,
            connection,
            event_bus,
            shutdown_flag,
            shared_state,
        }
    }
    
    /// Convert config TIF string to Tif enum
    fn tif_from_config(&self) -> Tif {
        match self.cfg.exec.tif.as_str() {
            "post_only" | "GTX" => Tif::PostOnly,
            "ioc" | "IOC" => Tif::Ioc,
            _ => Tif::Gtc,
        }
    }

    /// Check if an error is permanent (should not retry)
    /// Permanent errors: invalid parameters, insufficient balance, invalid symbol, etc.
    /// Temporary errors: network errors, rate limits, timeouts, server errors
    fn is_permanent_error(error: &anyhow::Error) -> bool {
        let error_str = error.to_string();
        let error_lower = error_str.to_lowercase();
        
        // Permanent errors - don't retry
        error_lower.contains("invalid")
            || error_lower.contains("margin")
            || error_lower.contains("insufficient balance")
            || error_lower.contains("min notional")
            || error_lower.contains("below min notional")
            || error_lower.contains("invalid symbol")
            || error_lower.contains("symbol not found")
            || error_lower.contains("position not found")
            || error_lower.contains("no position")
            || error_lower.contains("position already closed")
            || error_lower.contains("reduceonly")
            || error_lower.contains("reduce only")
            || error_lower.contains("-2011") // Binance: "Unknown order sent"
            || error_lower.contains("-2019") // Binance: "Margin is insufficient"
            || error_lower.contains("-2021") // Binance: "Order would immediately match"
    }

    /// Start the ordering service and begin processing trade signals.
    ///
    /// This method spawns multiple background tasks that:
    /// - Listen to TradeSignal events and place orders when no position/order is open
    /// - Listen to CloseRequest events and close positions
    /// - Sync state from OrderUpdate and PositionUpdate events
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` immediately after spawning background tasks. Tasks will continue
    /// running until `shutdown_flag` is set to true.
    ///
    /// # Behavior
    ///
    /// - Uses a global lock to ensure only one position/order at a time
    /// - Automatically handles order fills and converts them to positions
    /// - Updates state based on WebSocket order/position updates
    ///
    /// # Safety
    ///
    /// This module uses double-check locking patterns to prevent race conditions when
    /// placing orders. Network calls are never made while holding locks.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # let ordering = crate::ordering::Ordering::new(todo!(), todo!(), todo!(), todo!(), todo!());
    /// ordering.start().await?;
    /// // Service is now running and will place orders when TradeSignal events are received
    /// ```
    pub async fn start(&self) -> Result<()> {
        let event_bus = self.event_bus.clone();
        let shutdown_flag = self.shutdown_flag.clone();
        let connection = self.connection.clone();
        let shared_state = self.shared_state.clone();
        let cfg = self.cfg.clone();
        
        // Spawn task for TradeSignal events
        let state_trade = shared_state.clone();
        let event_bus_trade = event_bus.clone();
        let connection_trade = connection.clone();
        let shutdown_flag_trade = shutdown_flag.clone();
        let cfg_trade = cfg.clone();
        tokio::spawn(async move {
            let mut trade_signal_rx = event_bus_trade.subscribe_trade_signal();
            
            info!("ORDERING: Started, listening to TradeSignal events");
            
            loop {
                match trade_signal_rx.recv().await {
                    Ok(signal) => {
                        if shutdown_flag_trade.load(AtomicOrdering::Relaxed) {
                            break;
                        }
                        
                        if let Err(e) = Self::handle_trade_signal(
                            &signal,
                            &connection_trade,
                            &state_trade,
                            &cfg_trade,
                        ).await {
                            warn!(error = %e, symbol = %signal.symbol, "ORDERING: error handling TradeSignal");
                        }
                    }
                    Err(_) => {
                        // Channel closed or lagged - break
                        break;
                    }
                }
            }
        });
        
        // Spawn task for CloseRequest events
        let state_close = shared_state.clone();
        let event_bus_close = event_bus.clone();
        let connection_close = connection.clone();
        let shutdown_flag_close = shutdown_flag.clone();
        let cfg_close = cfg.clone();
        tokio::spawn(async move {
            let mut close_request_rx = event_bus_close.subscribe_close_request();
            
            info!("ORDERING: Started, listening to CloseRequest events");
            
            loop {
                match close_request_rx.recv().await {
                    Ok(request) => {
                        if shutdown_flag_close.load(AtomicOrdering::Relaxed) {
                            break;
                        }
                        
                        if let Err(e) = Self::handle_close_request(
                            &request,
                            &connection_close,
                            &state_close,
                            &cfg_close,
                        ).await {
                            warn!(error = %e, symbol = %request.symbol, "ORDERING: error handling CloseRequest");
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        
        // Spawn task for OrderUpdate events (state sync)
        let state_order = shared_state.clone();
        let event_bus_order = event_bus.clone();
        let shutdown_flag_order = shutdown_flag.clone();
        tokio::spawn(async move {
            let mut order_update_rx = event_bus_order.subscribe_order_update();
            
            info!("ORDERING: Started, listening to OrderUpdate events");
            
            loop {
                match order_update_rx.recv().await {
                    Ok(update) => {
                        if shutdown_flag_order.load(AtomicOrdering::Relaxed) {
                            break;
                        }
                        
                        Self::handle_order_update(&update, &state_order).await;
                    }
                    Err(_) => break,
                }
            }
        });
        
        // Spawn task for PositionUpdate events (state sync)
        let state_pos = shared_state.clone();
        let event_bus_pos = event_bus.clone();
        let shutdown_flag_pos = shutdown_flag.clone();
        tokio::spawn(async move {
            let mut position_update_rx = event_bus_pos.subscribe_position_update();
            
            info!("ORDERING: Started, listening to PositionUpdate events");
            
            loop {
                match position_update_rx.recv().await {
                    Ok(update) => {
                        if shutdown_flag_pos.load(AtomicOrdering::Relaxed) {
                            break;
                        }
                        
                        Self::handle_position_update(&update, &state_pos).await;
                    }
                    Err(_) => break,
                }
            }
        });
        
        Ok(())
    }

    /// Handle TradeSignal event
    /// If no open position/order, place order via CONNECTION
    async fn handle_trade_signal(
        signal: &TradeSignal,
        connection: &Arc<Connection>,
        shared_state: &Arc<SharedState>,
        cfg: &Arc<AppCfg>,
    ) -> Result<()> {
        // 1. Signal validity check - timestamp age
        let now = Instant::now();
        if let Some(signal_age) = now.checked_duration_since(signal.timestamp) {
            if signal_age > Duration::from_secs(5) {
                warn!(
                    symbol = %signal.symbol,
                    age_seconds = signal_age.as_secs(),
                    "ORDERING: Ignoring TradeSignal - signal too old"
                );
                return Ok(());
            }
        } else {
            // Signal timestamp is in the future (invalid signal)
            warn!(
                symbol = %signal.symbol,
                "ORDERING: Ignoring TradeSignal - timestamp in the future"
            );
            return Ok(());
        }
        
        // 2. Signal validity check - symbol and side
        if signal.symbol.is_empty() {
            warn!("ORDERING: Ignoring TradeSignal - empty symbol");
            return Ok(());
        }
        
        // 3. Balance pre-check and reservation - calculate required margin
        let leverage = signal.leverage;
        let notional = signal.entry_price.0 * signal.size.0;
        let required_margin = notional / Decimal::from(leverage);
        
        // CRITICAL: Reserve balance atomically to prevent double-spending race condition
        // Without reservation, two threads could both check balance (sufficient) and both send orders
        // With reservation, only one thread can reserve the balance, others will see insufficient balance
        let balance_reserved = {
            let mut balance_store = shared_state.balance_store.write().await;
            let available = balance_store.available(&cfg.quote_asset);
            
            if available < required_margin {
                warn!(
                    symbol = %signal.symbol,
                    required_margin = %required_margin,
                    available_balance = %available,
                    "ORDERING: Ignoring TradeSignal - insufficient balance"
                );
                return Ok(());
            }
            
            // Atomically reserve balance (prevents other threads from using it)
            balance_store.try_reserve(&cfg.quote_asset, required_margin)
        };
        
        if !balance_reserved {
            // This should not happen if available >= required_margin, but handle it anyway
            warn!(
                symbol = %signal.symbol,
                required_margin = %required_margin,
                "ORDERING: Failed to reserve balance (race condition detected)"
            );
            return Ok(());
        }
        
        // Balance reserved - will be released after order placement (success or failure)
        // If any check fails after reservation, balance must be released
        
        // 4. Risk control check - max position notional
        let max_position_notional = Decimal::from_str(&cfg.risk.max_position_notional_usd.to_string())
            .unwrap_or(Decimal::from(10000)); // Default to 10000 USD if conversion fails
        
        if notional > max_position_notional {
            // Release balance reservation before returning
            {
                let mut balance_store = shared_state.balance_store.write().await;
                balance_store.release(&cfg.quote_asset, required_margin);
            }
            warn!(
                symbol = %signal.symbol,
                notional = %notional,
                max_notional = %max_position_notional,
                "ORDERING: Ignoring TradeSignal - exceeds max position notional"
            );
            return Ok(());
        }
        
        // 5. Position check - ensure no open position/order
        // CRITICAL: Lock scope minimized - only for state check
        {
            let state_guard = shared_state.ordering_state.lock().await;
            
            // Check if we already have an open position or order
            if state_guard.open_position.is_some() || state_guard.open_order.is_some() {
                // Release balance reservation before returning
                drop(state_guard);
                {
                    let mut balance_store = shared_state.balance_store.write().await;
                    balance_store.release(&cfg.quote_asset, required_margin);
                }
                warn!(
                    symbol = %signal.symbol,
                    "ORDERING: Ignoring TradeSignal - already have open position/order"
                );
                return Ok(());
            }
        } // Lock released here, before async network call
        
        // Get TIF from config
        let tif = match cfg.exec.tif.as_str() {
            "post_only" | "GTX" => Tif::PostOnly,
            "ioc" | "IOC" => Tif::Ioc,
            _ => Tif::Gtc,
        };
        
        // Place order via CONNECTION (lock released, no deadlock risk)
        // Retry logic with exponential backoff
        use crate::connection::OrderCommand;
        let command = OrderCommand::Open {
            symbol: signal.symbol.clone(),
            side: signal.side,
            price: signal.entry_price,
            qty: signal.size,
            tif,
        };
        
        let mut last_error: Option<anyhow::Error> = None;
        
        // Attempt order placement with retry logic
        let order_result: Result<String, anyhow::Error> = {
            let mut result: Option<Result<String, anyhow::Error>> = None;
            for attempt in 0..MAX_RETRIES {
                match connection.send_order(command.clone()).await {
                    Ok(id) => {
                        result = Some(Ok(id));
                        break; // Success, exit retry loop
                    }
                    Err(e) => {
                        // Check if error is permanent (don't retry)
                        if Self::is_permanent_error(&e) {
                            warn!(
                                error = %e,
                                symbol = %signal.symbol,
                                attempt = attempt + 1,
                                "ORDERING: Permanent error, not retrying"
                            );
                            result = Some(Err(e));
                            break;
                        }
                        
                        // Store error for final fallback (only if all retries fail)
                        last_error = Some(anyhow::format_err!("{}", e));
                        
                        // Temporary error - retry with exponential backoff
                        if attempt < MAX_RETRIES - 1 {
                            let delay = Duration::from_millis(100 * 2u64.pow(attempt));
                            warn!(
                                error = %e,
                                symbol = %signal.symbol,
                                attempt = attempt + 1,
                                max_retries = MAX_RETRIES,
                                delay_ms = delay.as_millis(),
                                "ORDERING: Temporary error, retrying with exponential backoff"
                            );
                            tokio::time::sleep(delay).await;
                            continue;
                        } else {
                            // All retries exhausted
                            warn!(
                                error = %e,
                                symbol = %signal.symbol,
                                attempt = attempt + 1,
                                "ORDERING: All retries exhausted"
                            );
                            result = Some(Err(e));
                            break;
                        }
                    }
                }
            }
            result.unwrap_or_else(|| {
                Err(last_error.unwrap_or_else(|| anyhow!("Unknown error after retries")))
            })
        };
        
        // CRITICAL: Release balance reservation after order placement attempt
        // Balance is released regardless of success/failure:
        // - On success: Balance is actually used by the order, reservation can be released
        // - On failure: Balance was not used, reservation must be released
        {
            let mut balance_store = shared_state.balance_store.write().await;
            balance_store.release(&cfg.quote_asset, required_margin);
        }
        
        // Handle order result
        let order_id = match order_result {
            Ok(id) => id,
            Err(e) => {
                // Balance already released above
                return Err(e);
            }
        };
        
        // Update state (re-acquire lock for state update)
        // CRITICAL: Double-check pattern - another thread might have placed an order
        // between the initial check and the network call
        {
            let mut state_guard = shared_state.ordering_state.lock().await;
            
            // Double-check: if another thread placed an order while we were making the network call,
            // we should cancel our order to prevent duplicate orders
            if state_guard.open_order.is_none() && state_guard.open_position.is_none() {
                state_guard.open_order = Some(OpenOrder {
                    symbol: signal.symbol.clone(),
                    order_id: order_id.clone(),
                    side: signal.side,
                    qty: signal.size,
                });
                
                // Update timestamp to reflect manual state change
                // This ensures subsequent OrderUpdate events are compared against this timestamp
                state_guard.last_order_update_timestamp = Some(Instant::now());
                
                info!(
                    symbol = %signal.symbol,
                    side = ?signal.side,
                    order_id = %order_id,
                    "ORDERING: Order placed successfully"
                );
            } else {
                // Another thread placed an order/position while we were making the network call
                // This is a race condition - cancel our order to prevent duplicate orders
                warn!(
                    symbol = %signal.symbol,
                    order_id = %order_id,
                    "ORDERING: Race condition detected - another thread placed order/position, canceling our order"
                );
                
                // Cancel the order we just placed (lock released before async call)
                drop(state_guard);
                
                if let Err(cancel_err) = connection.cancel_order(&order_id, &signal.symbol).await {
                    warn!(
                        error = %cancel_err,
                        symbol = %signal.symbol,
                        order_id = %order_id,
                        "ORDERING: Failed to cancel duplicate order after race condition"
                    );
                } else {
                    info!(
                        symbol = %signal.symbol,
                        order_id = %order_id,
                        "ORDERING: Successfully canceled duplicate order after race condition"
                    );
                }
                
                return Ok(());
            }
        } // Lock released
        
        Ok(())
    }

    /// Handle CloseRequest event
    /// If position is open, close it via CONNECTION
    async fn handle_close_request(
        request: &CloseRequest,
        connection: &Arc<Connection>,
        shared_state: &Arc<SharedState>,
        cfg: &Arc<AppCfg>,
    ) -> Result<()> {
        // CRITICAL: Check if position exists before closing
        // This prevents unnecessary API calls if position was already closed
        // Note: flatten_position also checks position, but this early check saves API calls
        {
            let state_guard = shared_state.ordering_state.lock().await;
            if state_guard.open_position.is_none() || 
               state_guard.open_position.as_ref().map(|p| p.symbol != request.symbol).unwrap_or(true) {
                warn!(
                    symbol = %request.symbol,
                    "ORDERING: Ignoring CloseRequest - no open position for symbol"
                );
                return Ok(());
            }
        } // Lock released
        
        // CRITICAL: Use MARKET order with reduceOnly=true for closing positions
        // LIMIT orders are risky for close orders because:
        // 1. They may not fill immediately if price moves away
        // 2. TP/SL scenarios require immediate execution
        // 3. Position may remain open if limit order doesn't fill
        // Solution: Use flatten_position which sends MARKET orders with reduceOnly=true
        // This guarantees immediate execution and prevents position from staying open
        
        // Determine if this is a fast close scenario (TP/SL)
        // Fast close requires use_market_only=true to prevent LIMIT fallback delays
        use crate::event_bus::CloseReason;
        let use_market_only = matches!(request.reason, CloseReason::TakeProfit | CloseReason::StopLoss);
        
        // Close position using MARKET order with reduceOnly=true
        // flatten_position handles:
        // - Position verification
        // - Retry logic for partial fills
        // - Edge case handling (position already closed, etc.)
        // - MARKET order with reduceOnly=true guarantee
        match connection.flatten_position(&request.symbol, use_market_only).await {
            Ok(()) => {
                info!(
                    symbol = %request.symbol,
                    reason = ?request.reason,
                    use_market_only,
                    "ORDERING: Position closed successfully using MARKET order (reduceOnly=true)"
                );
            }
            Err(e) => {
                // Check if error is permanent (don't retry)
                if Self::is_permanent_error(&e) {
                    warn!(
                        error = %e,
                        symbol = %request.symbol,
                        reason = ?request.reason,
                        "ORDERING: Permanent error closing position, not retrying"
                    );
                    return Err(e);
                }
                
                // Temporary error - flatten_position already has retry logic
                // But we can log it for visibility
                warn!(
                    error = %e,
                    symbol = %request.symbol,
                    reason = ?request.reason,
                    "ORDERING: Error closing position (flatten_position handles retries internally)"
                );
                return Err(e);
            }
        }
        
        Ok(())
    }

    /// Handle OrderUpdate event (state sync)
    /// CRITICAL: Timestamp-based version control prevents stale updates
    /// Only applies updates that are newer than the last known update
    async fn handle_order_update(
        update: &OrderUpdate,
        shared_state: &Arc<SharedState>,
    ) {
        let mut state_guard = shared_state.ordering_state.lock().await;
        
        // CRITICAL: Check if this update is newer than the last known update
        // Prevents stale OrderUpdate events from overwriting newer state
        // If no previous timestamp exists, accept the update (first update)
        let is_newer = state_guard.last_order_update_timestamp
            .map(|last_ts| update.timestamp > last_ts)
            .unwrap_or(true);
        
        if !is_newer {
            // Stale update - ignore it
            tracing::debug!(
                symbol = %update.symbol,
                order_id = %update.order_id,
                update_timestamp = ?update.timestamp,
                last_timestamp = ?state_guard.last_order_update_timestamp,
                "ORDERING: Ignoring stale OrderUpdate event"
            );
            return;
        }
        
        // Update order state
        if let Some(ref mut order) = state_guard.open_order {
            if order.order_id == update.order_id {
                match update.status {
                    crate::event_bus::OrderStatus::Filled => {
                        // Order filled, convert to position
                        // Convert order side to position direction and ensure qty is positive
                        let direction = PositionDirection::from_order_side(update.side);
                        let qty_abs = if update.filled_qty.0.is_sign_negative() {
                            Qty(-update.filled_qty.0) // Make positive
                        } else {
                            update.filled_qty
                        };
                        
                        state_guard.open_position = Some(OpenPosition {
                            symbol: update.symbol.clone(),
                            direction,
                            qty: qty_abs,
                            entry_price: update.average_fill_price, // Use average fill price (weighted average of all fills)
                        });
                        state_guard.open_order = None;
                        
                        // Update timestamp after state change
                        state_guard.last_order_update_timestamp = Some(update.timestamp);
                        
                        info!(
                            symbol = %update.symbol,
                            order_id = %update.order_id,
                            "ORDERING: Order filled, position opened"
                        );
                    }
                    crate::event_bus::OrderStatus::Canceled => {
                        // Order canceled, clear state
                        state_guard.open_order = None;
                        
                        // Update timestamp after state change
                        state_guard.last_order_update_timestamp = Some(update.timestamp);
                        
                        info!(
                            symbol = %update.symbol,
                            order_id = %update.order_id,
                            "ORDERING: Order canceled"
                        );
                    }
                    crate::event_bus::OrderStatus::Expired | crate::event_bus::OrderStatus::ExpiredInMatch => {
                        // Order expired, clear state (similar to canceled)
                        state_guard.open_order = None;
                        
                        // Update timestamp after state change
                        state_guard.last_order_update_timestamp = Some(update.timestamp);
                        
                        info!(
                            symbol = %update.symbol,
                            order_id = %update.order_id,
                            status = ?update.status,
                            "ORDERING: Order expired"
                        );
                    }
                    crate::event_bus::OrderStatus::Rejected => {
                        // Order rejected, clear state
                        state_guard.open_order = None;
                        
                        // Update timestamp after state change
                        state_guard.last_order_update_timestamp = Some(update.timestamp);
                        
                        warn!(
                            symbol = %update.symbol,
                            order_id = %update.order_id,
                            "ORDERING: Order rejected"
                        );
                    }
                    _ => {
                        // Partial fill or other status, update order
                        order.qty = update.remaining_qty;
                        
                        // Update timestamp after state change
                        state_guard.last_order_update_timestamp = Some(update.timestamp);
                    }
                }
            }
        }
    }

    /// Handle PositionUpdate event (state sync)
    /// CRITICAL: Timestamp-based version control prevents stale updates
    /// Only applies updates that are newer than the last known update
    /// This prevents stale PositionUpdate events from overwriting state set by OrderUpdate
    async fn handle_position_update(
        update: &PositionUpdate,
        shared_state: &Arc<SharedState>,
    ) {
        let mut state_guard = shared_state.ordering_state.lock().await;
        
        // CRITICAL: Check if this update is newer than the last known PositionUpdate
        // Prevents stale PositionUpdate events from overwriting newer state
        // If no previous timestamp exists, accept the update (first update)
        let is_newer_position_update = state_guard.last_position_update_timestamp
            .map(|last_ts| update.timestamp > last_ts)
            .unwrap_or(true);
        
        // CRITICAL: Also check if PositionUpdate is newer than the last OrderUpdate
        // OrderUpdate events (from WebSocket) are more reliable and should take precedence
        // If OrderUpdate is newer, only accept PositionUpdate if it indicates position closed
        let order_update_is_newer = state_guard.last_order_update_timestamp
            .map(|order_ts| order_ts > update.timestamp)
            .unwrap_or(false);
        
        if !is_newer_position_update {
            // Stale PositionUpdate - ignore it
            tracing::debug!(
                symbol = %update.symbol,
                update_timestamp = ?update.timestamp,
                last_position_timestamp = ?state_guard.last_position_update_timestamp,
                "ORDERING: Ignoring stale PositionUpdate event"
            );
            return;
        }
        
        // If OrderUpdate is newer, be more cautious about accepting PositionUpdate
        // Only accept if position is closed (trust exchange's position closed signal)
        if order_update_is_newer && update.is_open {
            // OrderUpdate is newer and position is still open - likely stale PositionUpdate
            // Ignore it to prevent overwriting fresh OrderUpdate data
            tracing::debug!(
                symbol = %update.symbol,
                update_timestamp = ?update.timestamp,
                last_order_timestamp = ?state_guard.last_order_update_timestamp,
                "ORDERING: Ignoring PositionUpdate (OrderUpdate is newer and position is open)"
            );
            return;
        }
        
        // Check if we have an existing position for this symbol
        if let Some(ref existing_pos) = state_guard.open_position {
            if existing_pos.symbol == update.symbol {
                if !update.is_open {
                    // Position closed - always trust this (from exchange)
                    state_guard.open_position = None;
                    state_guard.last_position_update_timestamp = Some(update.timestamp);
                    
                    info!(
                        symbol = %update.symbol,
                        "ORDERING: Position closed (from PositionUpdate)"
                    );
                    return;
                }
                
                // Position is open - check if there are significant changes
                // Calculate differences for logging, but use timestamp as primary decision factor
                let qty_diff = (update.qty.0 - existing_pos.qty.0).abs();
                let price_diff = (update.entry_price.0 - existing_pos.entry_price.0).abs();
                
                // Update position (timestamp check already passed)
                // Determine direction from qty sign and ensure qty is positive
                let direction = PositionDirection::from_qty_sign(update.qty.0);
                let qty_abs = if update.qty.0.is_sign_negative() {
                    Qty(-update.qty.0) // Make positive
                } else {
                    update.qty
                };
                
                state_guard.open_position = Some(OpenPosition {
                    symbol: update.symbol.clone(),
                    direction,
                    qty: qty_abs,
                    entry_price: update.entry_price,
                });
                state_guard.last_position_update_timestamp = Some(update.timestamp);
                
                info!(
                    symbol = %update.symbol,
                    qty_diff = %qty_diff,
                    price_diff = %price_diff,
                    "ORDERING: Position updated from PositionUpdate (timestamp verified)"
                );
                return;
            }
        }
        
        // No existing position - create new one if position is open
        if update.is_open {
            // Determine position direction from quantity sign:
            // - Long position: qty > 0 (opened with BUY order)
            // - Short position: qty < 0 (opened with SELL order)
            let direction = PositionDirection::from_qty_sign(update.qty.0);
            let qty_abs = if update.qty.0.is_sign_negative() {
                Qty(-update.qty.0) // Make positive
            } else {
                update.qty
            };
            
            state_guard.open_position = Some(OpenPosition {
                symbol: update.symbol.clone(),
                direction,
                qty: qty_abs,
                entry_price: update.entry_price,
            });
            state_guard.last_position_update_timestamp = Some(update.timestamp);
            
            info!(
                symbol = %update.symbol,
                "ORDERING: Position created from PositionUpdate"
            );
        } else {
            // Position is closed and no existing position - just update timestamp
            state_guard.last_position_update_timestamp = Some(update.timestamp);
        }
    }
}

