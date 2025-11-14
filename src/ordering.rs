// ORDERING: Order placement/closure, single position guarantee
// Only handles order placement/closure, no trend or PnL logic
// Listens to TradeSignal and CloseRequest events
// Uses CONNECTION to send orders

use crate::connection::Connection;
use crate::event_bus::{CloseRequest, EventBus, OrderUpdate, PositionUpdate, TradeSignal};
use crate::state::{OpenPosition, OpenOrder, SharedState};
use crate::types::{Side, Tif};
use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;
use tracing::{info, warn};

/// ORDERING module - order placement and closure
/// Guarantees single position/order at a time using global lock
pub struct Ordering {
    connection: Arc<Connection>,
    event_bus: Arc<EventBus>,
    shutdown_flag: Arc<AtomicBool>,
    shared_state: Arc<SharedState>,
}

impl Ordering {
    pub fn new(
        connection: Arc<Connection>,
        event_bus: Arc<EventBus>,
        shutdown_flag: Arc<AtomicBool>,
        shared_state: Arc<SharedState>,
    ) -> Self {
        Self {
            connection,
            event_bus,
            shutdown_flag,
            shared_state,
        }
    }

    /// Start ordering service
    /// Listens to TradeSignal and CloseRequest events
    pub async fn start(&self) -> Result<()> {
        let event_bus = self.event_bus.clone();
        let shutdown_flag = self.shutdown_flag.clone();
        let connection = self.connection.clone();
        let shared_state = self.shared_state.clone();
        
        // Spawn task for TradeSignal events
        let state_trade = shared_state.clone();
        let event_bus_trade = event_bus.clone();
        let connection_trade = connection.clone();
        let shutdown_flag_trade = shutdown_flag.clone();
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
    ) -> Result<()> {
        let mut state_guard = shared_state.ordering_state.lock().await;
        
        // Check if we already have an open position or order
        if state_guard.open_position.is_some() || state_guard.open_order.is_some() {
            warn!(
                symbol = %signal.symbol,
                "ORDERING: Ignoring TradeSignal - already have open position/order"
            );
            return Ok(());
        }
        
        // Place order via CONNECTION
        use crate::connection::OrderCommand;
        let command = OrderCommand::Open {
            symbol: signal.symbol.clone(),
            side: signal.side,
            price: signal.entry_price,
            qty: signal.size,
            tif: Tif::Gtc, // Default TIF
        };
        
        match connection.send_order(command).await {
            Ok(order_id) => {
                // Update state
                state_guard.open_order = Some(OpenOrder {
                    symbol: signal.symbol.clone(),
                    order_id,
                    side: signal.side,
                    qty: signal.size,
                });
                
                info!(
                    symbol = %signal.symbol,
                    side = ?signal.side,
                    order_id = %state_guard.open_order.as_ref().unwrap().order_id,
                    "ORDERING: Order placed successfully"
                );
            }
            Err(e) => {
                warn!(
                    error = %e,
                    symbol = %signal.symbol,
                    "ORDERING: Failed to place order"
                );
                return Err(e);
            }
        }
        
        Ok(())
    }

    /// Handle CloseRequest event
    /// If position is open, close it via CONNECTION
    async fn handle_close_request(
        request: &CloseRequest,
        connection: &Arc<Connection>,
        shared_state: &Arc<SharedState>,
    ) -> Result<()> {
        let mut state_guard = shared_state.ordering_state.lock().await;
        
        // Check if we have an open position for this symbol
        let position = match &state_guard.open_position {
            Some(pos) if pos.symbol == request.symbol => pos.clone(),
            _ => {
                warn!(
                    symbol = %request.symbol,
                    "ORDERING: Ignoring CloseRequest - no open position for symbol"
                );
                return Ok(());
            }
        };
        
        // Calculate close side (opposite of position side)
        let close_side = match position.side {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        };
        
        // Place close order via CONNECTION
        use crate::connection::OrderCommand;
        let command = OrderCommand::Close {
            symbol: request.symbol.clone(),
            side: close_side,
            price: position.entry_price, // Use entry price as initial close price (will be adjusted)
            qty: position.qty,
            tif: Tif::Gtc,
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
                            entry_price: update.price,
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
                    _ => {
                        // Partial fill or other status, update order
                        order.qty = update.remaining_qty;
                    }
                }
            }
        }
    }

    /// Handle PositionUpdate event (state sync)
    async fn handle_position_update(
        update: &PositionUpdate,
        shared_state: &Arc<SharedState>,
    ) {
        let mut state_guard = shared_state.ordering_state.lock().await;
        
        if update.is_open {
            // Update or create position
            state_guard.open_position = Some(OpenPosition {
                symbol: update.symbol.clone(),
                side: if update.qty.0.is_sign_positive() {
                    Side::Buy
                } else {
                    Side::Sell
                },
                qty: update.qty,
                entry_price: update.entry_price,
            });
        } else {
            // Position closed, clear state
            if let Some(ref pos) = state_guard.open_position {
                if pos.symbol == update.symbol {
                    state_guard.open_position = None;
                    
                    info!(
                        symbol = %update.symbol,
                        "ORDERING: Position closed"
                    );
                }
            }
        }
    }
}

