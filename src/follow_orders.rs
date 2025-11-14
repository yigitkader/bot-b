// FOLLOW_ORDERS: Position tracking, TP/SL control
// Tracks open positions and sends CloseRequest when TP/SL is triggered
// Listens to PositionUpdate and MarketTick events
// Publishes CloseRequest events

use crate::event_bus::{CloseRequest, CloseReason, EventBus, MarketTick, PositionUpdate};
use crate::types::{Px, Qty};
use anyhow::Result;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;
use tracing::{info, warn};

/// FOLLOW_ORDERS module - position tracking and TP/SL control
pub struct FollowOrders {
    event_bus: Arc<EventBus>,
    shutdown_flag: Arc<AtomicBool>,
    // Tracked positions: symbol -> PositionInfo
    positions: Arc<RwLock<HashMap<String, PositionInfo>>>,
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
    pub fn new(
        event_bus: Arc<EventBus>,
        shutdown_flag: Arc<AtomicBool>,
    ) -> Self {
        Self {
            event_bus,
            shutdown_flag,
            positions: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Start follow orders service
    /// Listens to PositionUpdate and MarketTick events
    pub async fn start(&self) -> Result<()> {
        let event_bus = self.event_bus.clone();
        let shutdown_flag = self.shutdown_flag.clone();
        let positions = self.positions.clone();
        
        // Spawn task for PositionUpdate events
        let positions_pos = positions.clone();
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
                        
                        Self::handle_position_update(&update, &positions_pos).await;
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

    /// Handle PositionUpdate event
    /// Updates tracked positions
    async fn handle_position_update(
        update: &PositionUpdate,
        positions: &Arc<RwLock<HashMap<String, PositionInfo>>>,
    ) {
        let mut positions_guard = positions.write().await;
        
        if update.is_open {
            // Position opened or updated
            let side = if update.qty.0.is_sign_positive() {
                crate::types::Side::Buy
            } else {
                crate::types::Side::Sell
            };
            
            positions_guard.insert(update.symbol.clone(), PositionInfo {
                symbol: update.symbol.clone(),
                qty: update.qty,
                entry_price: update.entry_price,
                side,
                leverage: update.leverage,
                stop_loss_pct: None, // Will be set from TradeSignal if available
                take_profit_pct: None, // Will be set from TradeSignal if available
                opened_at: Instant::now(),
            });
            
            info!(
                symbol = %update.symbol,
                qty = %update.qty.0,
                entry_price = %update.entry_price.0,
                side = ?side,
                "FOLLOW_ORDERS: Position tracked"
            );
        } else {
            // Position closed
            positions_guard.remove(&update.symbol);
            
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
        let entry_price = position.entry_price.0;
        let current_price_val = current_price.0;
        
        let pnl_pct = match position.side {
            crate::types::Side::Buy => {
                // Long position: profit when price goes up
                ((current_price_val - entry_price) / entry_price) * Decimal::from(100)
            }
            crate::types::Side::Sell => {
                // Short position: profit when price goes down
                ((entry_price - current_price_val) / entry_price) * Decimal::from(100)
            }
        };
        
        let pnl_pct_f64 = pnl_pct.to_f64().unwrap_or(0.0);
        
        // Check take profit
        if let Some(tp_pct) = position.take_profit_pct {
            if pnl_pct_f64 >= tp_pct {
                // Take profit triggered
                let close_request = CloseRequest {
                    symbol: tick.symbol.clone(),
                    position_id: None,
                    reason: CloseReason::TakeProfit,
                    timestamp: Instant::now(),
                };
                
                let _ = event_bus.close_request_tx.send(close_request);
                {
                    info!(
                        symbol = %tick.symbol,
                        pnl_pct = pnl_pct_f64,
                        tp_pct,
                        "FOLLOW_ORDERS: Take profit triggered"
                    );
                }
                return Ok(());
            }
        }
        
        // Check stop loss
        if let Some(sl_pct) = position.stop_loss_pct {
            if pnl_pct_f64 <= -sl_pct {
                // Stop loss triggered
                let close_request = CloseRequest {
                    symbol: tick.symbol.clone(),
                    position_id: None,
                    reason: CloseReason::StopLoss,
                    timestamp: Instant::now(),
                };
                
                let _ = event_bus.close_request_tx.send(close_request);
                {
                    info!(
                        symbol = %tick.symbol,
                        pnl_pct = pnl_pct_f64,
                        sl_pct,
                        "FOLLOW_ORDERS: Stop loss triggered"
                    );
                }
                return Ok(());
            }
        }
        
        Ok(())
    }
}

