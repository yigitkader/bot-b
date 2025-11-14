// FOLLOW_ORDERS: Position tracking, TP/SL control
// Tracks open positions and sends CloseRequest when TP/SL is triggered
// Listens to PositionUpdate and MarketTick events
// Publishes CloseRequest events

use crate::event_bus::{CloseRequest, CloseReason, EventBus, MarketTick, PositionUpdate, TradeSignal};
use crate::types::{Px, Qty};
use anyhow::Result;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

/// FOLLOW_ORDERS module - position tracking and TP/SL control
pub struct FollowOrders {
    event_bus: Arc<EventBus>,
    shutdown_flag: Arc<AtomicBool>,
    // Tracked positions: symbol -> PositionInfo
    positions: Arc<RwLock<HashMap<String, PositionInfo>>>,
    // TP/SL from TradeSignal: symbol -> (stop_loss_pct, take_profit_pct)
    // This allows us to set TP/SL when position is opened
    tp_sl_from_signals: Arc<RwLock<HashMap<String, (Option<f64>, Option<f64>)>>>,
}

#[derive(Clone, Debug)]
struct PositionInfo {
    symbol: String,
    qty: Qty,
    entry_price: Px,
    side: crate::types::Side,
    leverage: u32,
    stop_loss_pct: Option<f64>,
    take_profit_pct: Option<f64>,
    opened_at: Instant,
}

impl FollowOrders {
    /// Create a new FollowOrders module instance.
    ///
    /// The FollowOrders module tracks open positions and monitors them for take profit (TP)
    /// and stop loss (SL) conditions. When triggered, it publishes CloseRequest events.
    ///
    /// # Arguments
    ///
    /// * `event_bus` - Event bus for subscribing to PositionUpdate/MarketTick and publishing CloseRequest
    /// * `shutdown_flag` - Shared flag to signal graceful shutdown
    ///
    /// # Returns
    ///
    /// Returns a new `FollowOrders` instance. Call `start()` to begin tracking positions.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # let event_bus = Arc::new(crate::event_bus::EventBus::new());
    /// # let shutdown_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    /// let follow_orders = FollowOrders::new(event_bus, shutdown_flag);
    /// follow_orders.start().await?;
    /// ```
    pub fn new(
        event_bus: Arc<EventBus>,
        shutdown_flag: Arc<AtomicBool>,
    ) -> Self {
        Self {
            event_bus,
            shutdown_flag,
            positions: Arc::new(RwLock::new(HashMap::new())),
            tp_sl_from_signals: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Start the follow orders service and begin tracking positions.
    ///
    /// This method spawns background tasks that:
    /// - Listen to TradeSignal events to track new positions with TP/SL from signals
    /// - Listen to PositionUpdate events to track when positions are opened
    /// - Listen to MarketTick events to monitor position PnL and trigger TP/SL
    /// - Publish CloseRequest events when TP or SL conditions are met
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` immediately after spawning background tasks. Tasks will continue
    /// running until `shutdown_flag` is set to true.
    ///
    /// # Behavior
    ///
    /// - Tracks positions from TradeSignal events (extracts TP/SL percentages)
    /// - Monitors real-time PnL using MarketTick price updates
    /// - Triggers CloseRequest when TP or SL thresholds are reached
    /// - Removes positions from tracking immediately after trigger to prevent duplicates
    ///
    /// # Example
    ///
    /// ```no_run
    /// # let follow_orders = crate::follow_orders::FollowOrders::new(todo!(), todo!());
    /// follow_orders.start().await?;
    /// // Service is now tracking positions and monitoring TP/SL
    /// ```
    pub async fn start(&self) -> Result<()> {
        let event_bus = self.event_bus.clone();
        let shutdown_flag = self.shutdown_flag.clone();
        let positions = self.positions.clone();
        let tp_sl_from_signals = self.tp_sl_from_signals.clone();
        
        // Spawn task for TradeSignal events (to capture TP/SL info)
        let tp_sl_signals = tp_sl_from_signals.clone();
        let positions_signal = positions.clone();
        let event_bus_signal = event_bus.clone();
        let shutdown_flag_signal = shutdown_flag.clone();
        tokio::spawn(async move {
            let mut trade_signal_rx = event_bus_signal.subscribe_trade_signal();
            
            info!("FOLLOW_ORDERS: Started, listening to TradeSignal events for TP/SL info");
            
            loop {
                match trade_signal_rx.recv().await {
                    Ok(signal) => {
                        if shutdown_flag_signal.load(AtomicOrdering::Relaxed) {
                            break;
                        }
                        
                        Self::handle_trade_signal(&signal, &tp_sl_signals, &positions_signal).await;
                    }
                    Err(_) => break,
                }
            }
        });
        
        // Spawn task for PositionUpdate events
        let positions_pos = positions.clone();
        let tp_sl_pos = tp_sl_from_signals.clone();
        let event_bus_pos = event_bus.clone();
        let shutdown_flag_pos = shutdown_flag.clone();
        tokio::spawn(async move {
            let mut position_update_rx = event_bus_pos.subscribe_position_update();
            
            info!("FOLLOW_ORDERS: Started, listening to PositionUpdate events");
            
            loop {
                match position_update_rx.recv().await {
                    Ok(update) => {
                        if shutdown_flag_pos.load(AtomicOrdering::Relaxed) {
                            break;
                        }
                        
                        Self::handle_position_update(&update, &positions_pos, &tp_sl_pos).await;
                    }
                    Err(_) => break,
                }
            }
        });
        
        // Spawn task for MarketTick events (TP/SL checking)
        let positions_tick = positions.clone();
        let event_bus_tick = event_bus.clone();
        let shutdown_flag_tick = shutdown_flag.clone();
        tokio::spawn(async move {
            let mut market_tick_rx = event_bus_tick.subscribe_market_tick();
            
            info!("FOLLOW_ORDERS: Started, listening to MarketTick events for TP/SL checking");
            
            loop {
                match market_tick_rx.recv().await {
                    Ok(tick) => {
                        if shutdown_flag_tick.load(AtomicOrdering::Relaxed) {
                            break;
                        }
                        
                        if let Err(e) = Self::check_tp_sl(&tick, &positions_tick, &event_bus_tick).await {
                            warn!(error = %e, symbol = %tick.symbol, "FOLLOW_ORDERS: error checking TP/SL");
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        
        Ok(())
    }

    /// Handle TradeSignal event
    /// Stores TP/SL information for later use when position is opened
    /// Also updates TP/SL if position is already open (handles race condition)
    async fn handle_trade_signal(
        signal: &TradeSignal,
        tp_sl_from_signals: &Arc<RwLock<HashMap<String, (Option<f64>, Option<f64>)>>>,
        positions: &Arc<RwLock<HashMap<String, PositionInfo>>>,
    ) {
        // Store TP/SL info for future position opens
        {
            let mut tp_sl_guard = tp_sl_from_signals.write().await;
            tp_sl_guard.insert(
                signal.symbol.clone(),
                (signal.stop_loss_pct, signal.take_profit_pct),
            );
        }
        
        // CRITICAL: If position is already open, update TP/SL immediately
        // This handles the race condition where PositionUpdate arrives before TradeSignal
        {
            let mut positions_guard = positions.write().await;
            if let Some(position) = positions_guard.get_mut(&signal.symbol) {
                // Position already exists, update TP/SL
                position.stop_loss_pct = signal.stop_loss_pct;
                position.take_profit_pct = signal.take_profit_pct;
                
                info!(
                    symbol = %signal.symbol,
                    stop_loss_pct = ?signal.stop_loss_pct,
                    take_profit_pct = ?signal.take_profit_pct,
                    "FOLLOW_ORDERS: TP/SL updated for existing position from TradeSignal"
                );
            } else {
                info!(
                    symbol = %signal.symbol,
                    stop_loss_pct = ?signal.stop_loss_pct,
                    take_profit_pct = ?signal.take_profit_pct,
                    "FOLLOW_ORDERS: TP/SL info stored from TradeSignal (position not yet open)"
                );
            }
        }
    }

    /// Handle PositionUpdate event
    /// Updates tracked positions and sets TP/SL from TradeSignal if available
    async fn handle_position_update(
        update: &PositionUpdate,
        positions: &Arc<RwLock<HashMap<String, PositionInfo>>>,
        tp_sl_from_signals: &Arc<RwLock<HashMap<String, (Option<f64>, Option<f64>)>>>,
    ) {
        let mut positions_guard = positions.write().await;
        
        if update.is_open {
            // Position opened or updated
            // Determine position direction from quantity sign:
            // - Long position: qty > 0 (opened with BUY order)
            // - Short position: qty < 0 (opened with SELL order)
            // Note: Side::Buy/Sell represents the order side used to open the position,
            // which corresponds to Long/Short position direction
            let side = if update.qty.0.is_sign_positive() {
                crate::types::Side::Buy  // Long position (qty > 0)
            } else {
                crate::types::Side::Sell // Short position (qty < 0)
            };
            
            // Get TP/SL from TradeSignal if available
            let (stop_loss_pct, take_profit_pct) = {
                let tp_sl_guard = tp_sl_from_signals.read().await;
                tp_sl_guard.get(&update.symbol)
                    .cloned()
                    .unwrap_or((None, None))
            };
            
            positions_guard.insert(update.symbol.clone(), PositionInfo {
                symbol: update.symbol.clone(),
                qty: update.qty,
                entry_price: update.entry_price,
                side,
                leverage: update.leverage,
                stop_loss_pct,
                take_profit_pct,
                opened_at: Instant::now(),
            });
            
            info!(
                symbol = %update.symbol,
                qty = %update.qty.0,
                entry_price = %update.entry_price.0,
                side = ?side,
                stop_loss_pct = ?stop_loss_pct,
                take_profit_pct = ?take_profit_pct,
                "FOLLOW_ORDERS: Position tracked with TP/SL"
            );
        } else {
            // Position closed
            positions_guard.remove(&update.symbol);
            
            // Clean up TP/SL info for this symbol
            {
                let mut tp_sl_guard = tp_sl_from_signals.write().await;
                tp_sl_guard.remove(&update.symbol);
            }
            
            info!(
                symbol = %update.symbol,
                "FOLLOW_ORDERS: Position closed, removed from tracking"
            );
        }
    }

    /// Check TP/SL for a market tick
    /// If TP or SL is triggered, send CloseRequest
    async fn check_tp_sl(
        tick: &MarketTick,
        positions: &Arc<RwLock<HashMap<String, PositionInfo>>>,
        event_bus: &Arc<EventBus>,
    ) -> Result<()> {
        let positions_read = positions.read().await;
        
        let position = match positions_read.get(&tick.symbol) {
            Some(pos) => pos.clone(),
            None => {
                // No position for this symbol
                return Ok(());
            }
        };
        
        drop(positions_read);
        
        // Calculate current price (use mark price if available, otherwise mid price)
        let current_price = tick.mark_price.unwrap_or_else(|| {
            Px((tick.bid.0 + tick.ask.0) / Decimal::from(2))
        });
        
        // Calculate unrealized PnL percentage
        // CRITICAL: In futures trading, leverage multiplies the price change to get PnL
        // Example: 20x leverage with 1% price move = 20% PnL
        let entry_price = position.entry_price.0;
        let current_price_val = current_price.0;
        
        // Calculate price change percentage based on position direction
        // Side::Buy = Long position (qty > 0): profit when price goes up
        // Side::Sell = Short position (qty < 0): profit when price goes down
        let price_change_pct = match position.side {
            crate::types::Side::Buy => {
                // Long position: profit when price goes up
                ((current_price_val - entry_price) / entry_price) * Decimal::from(100)
            }
            crate::types::Side::Sell => {
                // Short position: profit when price goes down
                ((entry_price - current_price_val) / entry_price) * Decimal::from(100)
            }
        };
        
        // Apply leverage to get actual PnL percentage
        // PnL% = PriceChange% * Leverage
        let leverage_decimal = Decimal::from(position.leverage);
        let pnl_pct = price_change_pct * leverage_decimal;
        
        let pnl_pct_f64 = pnl_pct.to_f64().unwrap_or(0.0);
        
        // Check take profit
        if let Some(tp_pct) = position.take_profit_pct {
            if pnl_pct_f64 >= tp_pct {
                // Take profit triggered - send close request and remove position from tracking
                // This prevents multiple close requests for the same trigger
                let close_request = CloseRequest {
                    symbol: tick.symbol.clone(),
                    position_id: None,
                    reason: CloseReason::TakeProfit,
                    timestamp: Instant::now(),
                };
                
                // Remove position from tracking immediately to prevent duplicate triggers
                {
                    let mut positions_guard = positions.write().await;
                    positions_guard.remove(&tick.symbol);
                }
                
                if let Err(e) = event_bus.close_request_tx.send(close_request) {
                    error!(
                        error = ?e,
                        symbol = %tick.symbol,
                        "FOLLOW_ORDERS: Failed to send CloseRequest event for take profit (no subscribers or channel closed)"
                    );
                } else {
                    info!(
                        symbol = %tick.symbol,
                        pnl_pct = pnl_pct_f64,
                        tp_pct,
                        leverage = position.leverage,
                        price_change_pct = price_change_pct.to_f64().unwrap_or(0.0),
                        "FOLLOW_ORDERS: Take profit triggered, position removed from tracking"
                    );
                }
                return Ok(());
            }
        }
        
        // Check stop loss
        if let Some(sl_pct) = position.stop_loss_pct {
            if pnl_pct_f64 <= -sl_pct {
                // Stop loss triggered - send close request and remove position from tracking
                // This prevents multiple close requests for the same trigger
                let close_request = CloseRequest {
                    symbol: tick.symbol.clone(),
                    position_id: None,
                    reason: CloseReason::StopLoss,
                    timestamp: Instant::now(),
                };
                
                // Remove position from tracking immediately to prevent duplicate triggers
                {
                    let mut positions_guard = positions.write().await;
                    positions_guard.remove(&tick.symbol);
                }
                
                if let Err(e) = event_bus.close_request_tx.send(close_request) {
                    error!(
                        error = ?e,
                        symbol = %tick.symbol,
                        "FOLLOW_ORDERS: Failed to send CloseRequest event for stop loss (no subscribers or channel closed)"
                    );
                } else {
                    info!(
                        symbol = %tick.symbol,
                        pnl_pct = pnl_pct_f64,
                        sl_pct,
                        leverage = position.leverage,
                        price_change_pct = price_change_pct.to_f64().unwrap_or(0.0),
                        "FOLLOW_ORDERS: Stop loss triggered, position removed from tracking"
                    );
                }
                return Ok(());
            }
        }
        
        Ok(())
    }
}

