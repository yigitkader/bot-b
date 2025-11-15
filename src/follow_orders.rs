// FOLLOW_ORDERS: Position tracking, TP/SL control
// Tracks open positions and sends CloseRequest when TP/SL is triggered
// Listens to PositionUpdate and MarketTick events
// Publishes CloseRequest events

use crate::config::AppCfg;
use crate::event_bus::{CloseRequest, CloseReason, EventBus, MarketTick, PositionUpdate, TradeSignal};
use crate::types::{Px, Qty, PositionDirection};
use anyhow::{anyhow, Result};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

/// FOLLOW_ORDERS module - position tracking and TP/SL control
pub struct FollowOrders {
    cfg: Arc<AppCfg>,
    event_bus: Arc<EventBus>,
    shutdown_flag: Arc<AtomicBool>,
    // Tracked positions: symbol -> PositionInfo
    positions: Arc<RwLock<HashMap<String, PositionInfo>>>,
    // TP/SL from TradeSignal: symbol -> (stop_loss_pct, take_profit_pct)
    // This allows us to set TP/SL when position is opened
    // Falls back to config defaults if TradeSignal hasn't arrived yet
    tp_sl_from_signals: Arc<RwLock<HashMap<String, (Option<f64>, Option<f64>)>>>,
}

#[derive(Clone, Debug)]
struct PositionInfo {
    symbol: String,
    qty: Qty,
    entry_price: Px,
    /// Position direction (Long or Short) - clearer than using Side
    direction: PositionDirection,
    leverage: u32,
    stop_loss_pct: Option<f64>,
    take_profit_pct: Option<f64>,
    opened_at: Instant,
    /// True if entry order was all maker fills, None if unknown
    /// Used for commission calculation: if all maker, use maker commission; otherwise taker
    /// Post-only orders are typically maker, but can become taker if they cross the spread
    is_maker: Option<bool>,
}

impl FollowOrders {
    /// Create a new FollowOrders module instance.
    ///
    /// The FollowOrders module tracks open positions and monitors them for take profit (TP)
    /// and stop loss (SL) conditions. When triggered, it publishes CloseRequest events.
    ///
    /// # Arguments
    ///
    /// * `cfg` - Application configuration containing default TP/SL percentages
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
    /// # let cfg = Arc::new(crate::config::load_config()?);
    /// # let event_bus = Arc::new(crate::event_bus::EventBus::new());
    /// # let shutdown_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    /// let follow_orders = FollowOrders::new(cfg, event_bus, shutdown_flag);
    /// follow_orders.start().await?;
    /// ```
    pub fn new(
        cfg: Arc<AppCfg>,
        event_bus: Arc<EventBus>,
        shutdown_flag: Arc<AtomicBool>,
    ) -> Self {
        Self {
            cfg,
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
        let cfg = self.cfg.clone();
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
        let cfg_pos = cfg.clone();
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
                        
                        Self::handle_position_update(&update, &cfg_pos, &positions_pos, &tp_sl_pos).await;
                    }
                    Err(_) => break,
                }
            }
        });
        
        // Spawn task for OrderUpdate events (to capture is_maker for commission calculation)
        let positions_order = positions.clone();
        let shutdown_flag_order = shutdown_flag.clone();
        tokio::spawn(async move {
            let mut order_update_rx = event_bus.subscribe_order_update();
            
            info!("FOLLOW_ORDERS: Started, listening to OrderUpdate events for is_maker info");
            
            loop {
                match order_update_rx.recv().await {
                    Ok(update) => {
                        if shutdown_flag_order.load(AtomicOrdering::Relaxed) {
                            break;
                        }
                        
                        // Update is_maker info in PositionInfo when order is filled
                        if matches!(update.status, crate::event_bus::OrderStatus::Filled) {
                            let mut positions_guard = positions_order.write().await;
                            if let Some(position) = positions_guard.get_mut(&update.symbol) {
                                // Update is_maker info for commission calculation
                                position.is_maker = update.is_maker;
                                
                                debug!(
                                    symbol = %update.symbol,
                                    is_maker = ?update.is_maker,
                                    "FOLLOW_ORDERS: Updated is_maker info from OrderUpdate"
                                );
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        
        // Spawn task for MarketTick events (TP/SL checking)
        let positions_tick = positions.clone();
        let event_bus_tick = event_bus.clone();
        let shutdown_flag_tick = shutdown_flag.clone();
        let cfg_tick = cfg.clone();
        tokio::spawn(async move {
            let mut market_tick_rx = event_bus_tick.subscribe_market_tick();
            
            info!("FOLLOW_ORDERS: Started, listening to MarketTick events for TP/SL checking");
            
            loop {
                match market_tick_rx.recv().await {
                    Ok(tick) => {
                        if shutdown_flag_tick.load(AtomicOrdering::Relaxed) {
                            break;
                        }
                        
                        if let Err(e) = Self::check_tp_sl(&tick, &positions_tick, &event_bus_tick, &cfg_tick).await {
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
    /// Falls back to config defaults if TradeSignal hasn't arrived yet (race condition prevention)
    async fn handle_position_update(
        update: &PositionUpdate,
        cfg: &Arc<AppCfg>,
        positions: &Arc<RwLock<HashMap<String, PositionInfo>>>,
        tp_sl_from_signals: &Arc<RwLock<HashMap<String, (Option<f64>, Option<f64>)>>>,
    ) {
        let mut positions_guard = positions.write().await;
        
        if update.is_open {
            // Position opened or updated
            // Determine position direction from quantity sign:
            // - Long position: qty > 0 (opened with BUY order)
            // - Short position: qty < 0 (opened with SELL order)
            
            // Get TP/SL from TradeSignal if available, otherwise use config defaults
            // CRITICAL: Prevents race condition where PositionUpdate arrives before TradeSignal
            // If TradeSignal hasn't arrived yet, use config defaults to ensure TP/SL is always set
            let (stop_loss_pct, take_profit_pct) = {
                let tp_sl_guard = tp_sl_from_signals.read().await;
                tp_sl_guard.get(&update.symbol)
                    .cloned()
                    .unwrap_or((
                        Some(cfg.stop_loss_pct),    // Default from config
                        Some(cfg.take_profit_pct)   // Default from config
                    ))
            };
            
            // Determine position direction from qty sign and ensure qty is positive
            let direction = PositionDirection::from_qty_sign(update.qty.0);
            let qty_abs = if update.qty.0.is_sign_negative() {
                Qty(-update.qty.0) // Make positive
            } else {
                update.qty
            };
            
            positions_guard.insert(update.symbol.clone(), PositionInfo {
                symbol: update.symbol.clone(),
                qty: qty_abs,
                entry_price: update.entry_price,
                direction,
                leverage: update.leverage,
                stop_loss_pct,
                take_profit_pct,
                opened_at: Instant::now(),
                is_maker: None, // Will be updated from OrderUpdate when order is filled
            });
            
            info!(
                symbol = %update.symbol,
                qty = %qty_abs.0,
                entry_price = %update.entry_price.0,
                direction = ?direction,
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
        cfg: &Arc<AppCfg>,
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
        
        // CRITICAL: Use mark price if available (more accurate for futures PnL calculation)
        // Mark price is the fair value price used for liquidation and PnL calculations
        // If mark price is not available, fall back to bid/ask mid price
        let current_price = tick.mark_price.unwrap_or_else(|| {
            Px((tick.bid.0 + tick.ask.0) / Decimal::from(2))
        });
        
        // Calculate unrealized PnL percentage
        // CRITICAL: In futures trading, leverage multiplies the price change to get PnL
        // Example: 20x leverage with 1% price move = 20% PnL
        let entry_price = position.entry_price.0;
        let current_price_val = current_price.0;
        
        // Calculate price change percentage based on position direction
        // Long position: profit when price goes up
        // Short position: profit when price goes down
        let price_change_pct = match position.direction {
            PositionDirection::Long => {
                // Long position: profit when price goes up
                ((current_price_val - entry_price) / entry_price) * Decimal::from(100)
            }
            PositionDirection::Short => {
                // Short position: profit when price goes down
                ((entry_price - current_price_val) / entry_price) * Decimal::from(100)
            }
        };
        
        // Apply leverage to get gross PnL percentage (without commission)
        // Gross PnL% = PriceChange% * Leverage
        // 
        // ✅ CRITICAL: This calculation is correct ONLY for ISOLATED MARGIN mode
        // In isolated margin, each position has its own margin and leverage applies directly:
        // - Position margin = Notional / Leverage
        // - PnL% = PriceChange% * Leverage (correct for isolated margin)
        //
        // ⚠️ CRITICAL WARNING: This calculation is INCORRECT for CROSS MARGIN mode
        // Cross margin uses shared account equity across all positions:
        // - Total account equity is shared between all positions
        // - Effective leverage might differ from position leverage
        // - PnL calculation needs to account for total account balance and margin allocation
        // - Formula should be: PnL% = (PriceChange% * PositionNotional) / TotalAccountEquity
        // 
        // Current implementation assumes use_isolated_margin: true (from config)
        // If cross margin is used, TP/SL trigger levels will be WRONG, leading to:
        // - Premature or delayed TP/SL triggers
        // - Incorrect risk management
        // - Potential financial losses
        if !cfg.risk.use_isolated_margin {
            error!(
                symbol = %tick.symbol,
                "FOLLOW_ORDERS: CRITICAL - Cross margin mode detected but TP/SL PnL calculation assumes isolated margin. TP/SL trigger levels will be incorrect!"
            );
            // Continue with calculation but log error - user should fix config
        }
        
        let leverage_decimal = Decimal::from(position.leverage);
        let gross_pnl_pct = price_change_pct * leverage_decimal;
        
        // ✅ CRITICAL FIX: Calculate commission correctly using is_maker from OrderUpdate
        // 
        // SOLUTION: Use is_maker flag from OrderUpdate to determine commission type
        // - If all fills were maker (is_maker = Some(true)), use maker commission
        // - If any fill was taker (is_maker = Some(false)), use taker commission
        // - If is_maker is None (OrderUpdate hasn't arrived yet), estimate based on TIF:
        //   * If TIF is "post_only", likely maker (use maker commission)
        //   * Otherwise, use taker commission (conservative approach)
        // Post-only orders are typically maker, but can become taker if they cross the spread
        //
        // Exit commission (TP/SL) is always Taker (market order with reduceOnly) - guaranteed.
        let entry_commission_pct = match position.is_maker {
            Some(true) => {
                // All fills were maker - use maker commission (lower, better for PnL)
                Decimal::from_str(&cfg.risk.maker_commission_pct.to_string())
                    .unwrap_or_else(|_| Decimal::from_str("0.02").unwrap_or(Decimal::ZERO))
            }
            Some(false) => {
                // Any fill was taker - use taker commission
                Decimal::from_str(&cfg.risk.taker_commission_pct.to_string())
                    .unwrap_or_else(|_| Decimal::from_str("0.04").unwrap_or(Decimal::ZERO))
            }
            None => {
                // is_maker henüz bilinmiyor - TIF'e göre tahmin et
                // Post-only orders are typically maker, but can become taker if they cross the spread
                if cfg.exec.tif == "post_only" {
                    // Post-only genelde maker olur
                    Decimal::from_str(&cfg.risk.maker_commission_pct.to_string())
                        .unwrap_or_else(|_| Decimal::from_str("0.02").unwrap_or(Decimal::ZERO))
                } else {
                    // Conservative yaklaşım - taker commission kullan
                    // This ensures we don't underestimate commission, which would cause TP/SL to trigger
                    // too early and result in money loss. Better to be conservative and trigger slightly
                    // later than to trigger too early.
                    Decimal::from_str(&cfg.risk.taker_commission_pct.to_string())
                        .unwrap_or_else(|_| Decimal::from_str("0.04").unwrap_or(Decimal::ZERO))
                }
            }
        };
        
        // Exit commission (TP/SL close orders) is always Taker (market order with reduceOnly)
        // This is guaranteed because TP/SL orders are always market orders
        let exit_commission_pct = Decimal::from_str(&cfg.risk.taker_commission_pct.to_string())
            .unwrap_or_else(|_| Decimal::from_str("0.04").unwrap_or(Decimal::ZERO));
        
        // Total commission = Entry + Exit (both Taker - conservative approach)
        let total_commission_pct = entry_commission_pct + exit_commission_pct;
        
        // Calculate net PnL percentage (gross PnL - commission)
        // Net PnL% = Gross PnL% - (Commission% * 2)
        let net_pnl_pct = gross_pnl_pct - total_commission_pct;
        
        let net_pnl_pct_f64 = net_pnl_pct.to_f64().unwrap_or(0.0);
        let gross_pnl_pct_f64 = gross_pnl_pct.to_f64().unwrap_or(0.0);
        
        // Check take profit (using net PnL - commission included)
        if let Some(tp_pct) = position.take_profit_pct {
            if net_pnl_pct_f64 >= tp_pct {
                // ✅ CRITICAL: Take profit triggered - send close request FIRST, then remove position
                // Order matters: If CloseRequest fails, position should remain in tracking
                // This prevents position from being removed without sending close request
                // Include current bid/ask prices to reduce slippage (avoid price fetch delay)
                let close_request = CloseRequest {
                    symbol: tick.symbol.clone(),
                    position_id: None,
                    reason: CloseReason::TakeProfit,
                    current_bid: Some(tick.bid),
                    current_ask: Some(tick.ask),
                    timestamp: Instant::now(),
                };
                
                // ✅ CRITICAL: Send CloseRequest FIRST, only remove position if successful
                match event_bus.close_request_tx.send(close_request) {
                    Ok(()) => {
                        // CloseRequest sent successfully - now safe to remove position from tracking
                        // This prevents duplicate triggers while ensuring close request is sent
                        {
                            let mut positions_guard = positions.write().await;
                            positions_guard.remove(&tick.symbol);
                        }
                        
                        info!(
                            symbol = %tick.symbol,
                            net_pnl_pct = net_pnl_pct_f64,
                            gross_pnl_pct = gross_pnl_pct_f64,
                            entry_commission_pct = entry_commission_pct.to_f64().unwrap_or(0.0),
                            exit_commission_pct = exit_commission_pct.to_f64().unwrap_or(0.0),
                            total_commission_pct = total_commission_pct.to_f64().unwrap_or(0.0),
                            tp_pct,
                            leverage = position.leverage,
                            price_change_pct = price_change_pct.to_f64().unwrap_or(0.0),
                            "FOLLOW_ORDERS: Take profit triggered (net PnL), CloseRequest sent, position removed from tracking"
                        );
                    }
                    Err(e) => {
                        // ✅ CRITICAL FIX: CloseRequest failed - DO NOT remove position, DO NOT return error
                        // Position remains in tracking so it can be retried on next tick
                        // Returning Ok(()) allows retry on next MarketTick event
                        // If we return error, the caller might stop processing, preventing retry
                        warn!(
                            error = ?e,
                            symbol = %tick.symbol,
                            net_pnl_pct = net_pnl_pct_f64,
                            tp_pct,
                            "FOLLOW_ORDERS: CloseRequest failed for take profit, position will retry on next tick"
                        );
                        // Return Ok(()) to allow retry on next tick
                        // Position remains in tracking, so it will be checked again
                        return Ok(());
                    }
                }
                return Ok(());
            }
        }
        
        // Check stop loss (using net PnL - commission included)
        if let Some(sl_pct) = position.stop_loss_pct {
            if net_pnl_pct_f64 <= -sl_pct {
                // ✅ CRITICAL: Stop loss triggered - send close request FIRST, then remove position
                // Order matters: If CloseRequest fails, position should remain in tracking
                // This prevents position from being removed without sending close request
                // Include current bid/ask prices to reduce slippage (avoid price fetch delay)
                let close_request = CloseRequest {
                    symbol: tick.symbol.clone(),
                    position_id: None,
                    reason: CloseReason::StopLoss,
                    current_bid: Some(tick.bid),
                    current_ask: Some(tick.ask),
                    timestamp: Instant::now(),
                };
                
                // ✅ CRITICAL: Send CloseRequest FIRST, only remove position if successful
                match event_bus.close_request_tx.send(close_request) {
                    Ok(()) => {
                        // CloseRequest sent successfully - now safe to remove position from tracking
                        // This prevents duplicate triggers while ensuring close request is sent
                        {
                            let mut positions_guard = positions.write().await;
                            positions_guard.remove(&tick.symbol);
                        }
                        
                        info!(
                            symbol = %tick.symbol,
                            net_pnl_pct = net_pnl_pct_f64,
                            gross_pnl_pct = gross_pnl_pct_f64,
                            entry_commission_pct = entry_commission_pct.to_f64().unwrap_or(0.0),
                            exit_commission_pct = exit_commission_pct.to_f64().unwrap_or(0.0),
                            total_commission_pct = total_commission_pct.to_f64().unwrap_or(0.0),
                            sl_pct,
                            leverage = position.leverage,
                            price_change_pct = price_change_pct.to_f64().unwrap_or(0.0),
                            "FOLLOW_ORDERS: Stop loss triggered (net PnL), CloseRequest sent, position removed from tracking"
                        );
                    }
                    Err(e) => {
                        // ✅ CRITICAL FIX: CloseRequest failed - DO NOT remove position, DO NOT return error
                        // Position remains in tracking so it can be retried on next tick
                        // Returning Ok(()) allows retry on next MarketTick event
                        // If we return error, the caller might stop processing, preventing retry
                        warn!(
                            error = ?e,
                            symbol = %tick.symbol,
                            net_pnl_pct = net_pnl_pct_f64,
                            sl_pct,
                            "FOLLOW_ORDERS: CloseRequest failed for stop loss, position will retry on next tick"
                        );
                        // Return Ok(()) to allow retry on next tick
                        // Position remains in tracking, so it will be checked again
                        return Ok(());
                    }
                }
                return Ok(());
            }
        }
        
        Ok(())
    }
}

