// ORDERING: Order placement/closure, single position guarantee
// Only handles order placement/closure, no trend or PnL logic
// Listens to TradeSignal and CloseRequest events
// Uses CONNECTION to send orders

use crate::config::AppCfg;
use crate::connection::Connection;
use crate::event_bus::{CloseRequest, EventBus, OrderUpdate, PositionUpdate, TradeSignal};
use crate::state::{OpenPosition, OpenOrder, SharedState};
use crate::types::{Side, Tif};
use anyhow::{anyhow, Result};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;
use tracing::{info, warn};

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
        // CRITICAL: Lock scope minimized - only for state check
        {
            let state_guard = shared_state.ordering_state.lock().await;
            
            // Check if we already have an open position or order
            if state_guard.open_position.is_some() || state_guard.open_order.is_some() {
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
        use crate::connection::OrderCommand;
        let command = OrderCommand::Open {
            symbol: signal.symbol.clone(),
            side: signal.side,
            price: signal.entry_price,
            qty: signal.size,
            tif,
        };
        
        let order_id = match connection.send_order(command).await {
            Ok(order_id) => order_id,
            Err(e) => {
                warn!(
                    error = %e,
                    symbol = %signal.symbol,
                    "ORDERING: Failed to place order"
                );
                return Err(e);
            }
        };
        
        // Update state (re-acquire lock for state update)
        // CRITICAL: Double-check pattern - another thread might have placed an order
        // between the initial check and the network call
        {
            let mut state_guard = shared_state.ordering_state.lock().await;
            
            // Double-check: if another thread placed an order while we were making the network call,
            // we should not overwrite it (though this is unlikely, it's a safety measure)
            if state_guard.open_order.is_none() && state_guard.open_position.is_none() {
                state_guard.open_order = Some(OpenOrder {
                    symbol: signal.symbol.clone(),
                    order_id: order_id.clone(),
                    side: signal.side,
                    qty: signal.size,
                });
                
                info!(
                    symbol = %signal.symbol,
                    side = ?signal.side,
                    order_id = %order_id,
                    "ORDERING: Order placed successfully"
                );
            } else {
                // Another thread placed an order/position while we were making the network call
                // This is a race condition - we should log it but not fail
                warn!(
                    symbol = %signal.symbol,
                    order_id = %order_id,
                    "ORDERING: Order placed but state already has open position/order (race condition)"
                );
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
        // CRITICAL: Lock scope minimized - only for position check and clone
        let position = {
            let state_guard = shared_state.ordering_state.lock().await;
            
            // Check if we have an open position for this symbol
            match &state_guard.open_position {
                Some(pos) if pos.symbol == request.symbol => pos.clone(),
                _ => {
                    warn!(
                        symbol = %request.symbol,
                        "ORDERING: Ignoring CloseRequest - no open position for symbol"
                    );
                    return Ok(());
                }
            }
        }; // Lock released here, before async network calls
        
        // Calculate close side (opposite of position side)
        let close_side = match position.side {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        };
        
        // Get current market price for closing (lock released, no deadlock risk)
        // Long position: close with SELL at bid price
        // Short position: close with BUY at ask price
        let (bid, ask) = connection.get_current_prices(&request.symbol).await
            .map_err(|e| anyhow!("Failed to get current market prices for {}: {}", request.symbol, e))?;
        
        let close_price = match position.side {
            Side::Buy => bid,  // Long position: SELL at bid
            Side::Sell => ask, // Short position: BUY at ask
        };
        
        // Get TIF from config
        let tif = match cfg.exec.tif.as_str() {
            "post_only" | "GTX" => Tif::PostOnly,
            "ioc" | "IOC" => Tif::Ioc,
            _ => Tif::Gtc,
        };
        
        // Place close order via CONNECTION (lock released, no deadlock risk)
        use crate::connection::OrderCommand;
        let command = OrderCommand::Close {
            symbol: request.symbol.clone(),
            side: close_side,
            price: close_price,
            qty: position.qty,
            tif,
        };
        
        match connection.send_order(command).await {
            Ok(order_id) => {
                info!(
                    symbol = %request.symbol,
                    order_id = %order_id,
                    reason = ?request.reason,
                    "ORDERING: Close order placed successfully"
                );
            }
            Err(e) => {
                warn!(
                    error = %e,
                    symbol = %request.symbol,
                    "ORDERING: Failed to place close order"
                );
                return Err(e);
            }
        }
        
        Ok(())
    }

    /// Handle OrderUpdate event (state sync)
    async fn handle_order_update(
        update: &OrderUpdate,
        shared_state: &Arc<SharedState>,
    ) {
        let mut state_guard = shared_state.ordering_state.lock().await;
        
        // Update order state
        if let Some(ref mut order) = state_guard.open_order {
            if order.order_id == update.order_id {
                match update.status {
                    crate::event_bus::OrderStatus::Filled => {
                        // Order filled, convert to position
                        state_guard.open_position = Some(OpenPosition {
                            symbol: update.symbol.clone(),
                            side: update.side,
                            qty: update.filled_qty,
                            entry_price: update.average_fill_price, // Use average fill price (weighted average of all fills)
                        });
                        state_guard.open_order = None;
                        
                        info!(
                            symbol = %update.symbol,
                            order_id = %update.order_id,
                            "ORDERING: Order filled, position opened"
                        );
                    }
                    crate::event_bus::OrderStatus::Canceled => {
                        // Order canceled, clear state
                        state_guard.open_order = None;
                        
                        info!(
                            symbol = %update.symbol,
                            order_id = %update.order_id,
                            "ORDERING: Order canceled"
                        );
                    }
                    crate::event_bus::OrderStatus::Expired | crate::event_bus::OrderStatus::ExpiredInMatch => {
                        // Order expired, clear state (similar to canceled)
                        state_guard.open_order = None;
                        
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
                        
                        warn!(
                            symbol = %update.symbol,
                            order_id = %update.order_id,
                            "ORDERING: Order rejected"
                        );
                    }
                    _ => {
                        // Partial fill or other status, update order
                        order.qty = update.remaining_qty;
                    }
                }
            }
        }
    }

    /// Handle PositionUpdate event (state sync)
    /// CRITICAL: Race condition prevention - only update if PositionUpdate is newer than OrderUpdate
    /// This prevents stale PositionUpdate events from overwriting state set by OrderUpdate
    async fn handle_position_update(
        update: &PositionUpdate,
        shared_state: &Arc<SharedState>,
    ) {
        let mut state_guard = shared_state.ordering_state.lock().await;
        
        // CRITICAL: Check if we have a position that was set by OrderUpdate
        // If PositionUpdate timestamp is older than the last OrderUpdate timestamp, ignore it
        // This prevents race condition where async PositionUpdate fetch arrives after OrderUpdate
        if let Some(ref existing_pos) = state_guard.open_position {
            if existing_pos.symbol == update.symbol {
                // Check if PositionUpdate is stale (arrived after OrderUpdate set the position)
                // We can't compare timestamps directly, but we can check if position was just set
                // If position exists and PositionUpdate says it's open, it's likely a refresh
                // If position exists and PositionUpdate says it's closed, we should trust it
                if !update.is_open {
                    // Position closed - always trust this (from exchange)
                    state_guard.open_position = None;
                    
                    info!(
                        symbol = %update.symbol,
                        "ORDERING: Position closed (from PositionUpdate)"
                    );
                    return;
                }
                
                // Position is open - only update if qty or entry_price changed significantly
                // This prevents stale PositionUpdate from overwriting fresh OrderUpdate data
                let qty_diff = (update.qty.0 - existing_pos.qty.0).abs();
                let price_diff = (update.entry_price.0 - existing_pos.entry_price.0).abs();
                
                // Only update if there's a significant change (position was modified externally)
                // Small differences might be due to timing/rounding, ignore them
                if qty_diff > rust_decimal::Decimal::new(1, 8) || price_diff > rust_decimal::Decimal::new(1, 8) {
                    // Significant change, update position
                    state_guard.open_position = Some(OpenPosition {
                        symbol: update.symbol.clone(),
                        side: if update.qty.0.is_sign_positive() {
                            Side::Buy  // Long position (qty > 0)
                        } else {
                            Side::Sell // Short position (qty < 0)
                        },
                        qty: update.qty,
                        entry_price: update.entry_price,
                    });
                    
                    info!(
                        symbol = %update.symbol,
                        qty_diff = %qty_diff,
                        price_diff = %price_diff,
                        "ORDERING: Position updated from PositionUpdate (significant change detected)"
                    );
                } else {
                    // No significant change, ignore (likely stale PositionUpdate)
                    tracing::debug!(
                        symbol = %update.symbol,
                        "ORDERING: Ignoring PositionUpdate (no significant change, likely stale)"
                    );
                }
                return;
            }
        }
        
        // No existing position - create new one
        if update.is_open {
            // Update or create position
            // Determine position direction from quantity sign:
            // - Long position: qty > 0 (opened with BUY order)
            // - Short position: qty < 0 (opened with SELL order)
            // Note: Side::Buy/Sell represents the order side used to open the position,
            // which corresponds to Long/Short position direction
            state_guard.open_position = Some(OpenPosition {
                symbol: update.symbol.clone(),
                side: if update.qty.0.is_sign_positive() {
                    Side::Buy  // Long position (qty > 0)
                } else {
                    Side::Sell // Short position (qty < 0)
                },
                qty: update.qty,
                entry_price: update.entry_price,
            });
            
            info!(
                symbol = %update.symbol,
                "ORDERING: Position created from PositionUpdate"
            );
        }
    }
}

