// FOLLOW_ORDERS: Position tracking, TP/SL control
// Tracks open positions and sends CloseRequest when TP/SL is triggered
// Listens to PositionUpdate and MarketTick events
// Publishes CloseRequest events

use crate::config::AppCfg;
use crate::event_bus::{CloseRequest, CloseReason, EventBus, MarketTick, PositionUpdate, TradeSignal};
use crate::types::{PositionDirection, PositionInfo, Px, Qty};
use anyhow::{anyhow, Result};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// FOLLOW_ORDERS module - position tracking and TP/SL control
pub struct FollowOrders {
    cfg: Arc<AppCfg>,
    event_bus: Arc<EventBus>,
    shutdown_flag: Arc<AtomicBool>,
    positions: Arc<RwLock<HashMap<String, PositionInfo>>>,
    tp_sl_from_signals: Arc<RwLock<HashMap<String, (Option<f64>, Option<f64>)>>>,
}

impl FollowOrders {
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

    pub async fn start(&self) -> Result<()> {
        if !self.cfg.risk.use_isolated_margin {
            return Err(anyhow!(
                "CRITICAL: Cross margin mode is NOT supported for TP/SL. \
                 PnL calculation assumes isolated margin. \
                 Cross margin uses shared account equity across all positions, which requires: \
                 PnL% = (PriceChange% × PositionNotional) / TotalAccountEquity \
                 instead of: PnL% = PriceChange% × Leverage \
                 Please set risk.use_isolated_margin: true in config.yaml"
            ));
        }
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
        let event_bus_order = event_bus.clone();
        let shutdown_flag_order = shutdown_flag.clone();
        tokio::spawn(async move {
            let mut order_update_rx = event_bus_order.subscribe_order_update();
            
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

    async fn handle_position_update(
        update: &PositionUpdate,
        cfg: &Arc<AppCfg>,
        positions: &Arc<RwLock<HashMap<String, PositionInfo>>>,
        tp_sl_from_signals: &Arc<RwLock<HashMap<String, (Option<f64>, Option<f64>)>>>,
    ) {
        let mut positions_guard = positions.write().await;
        
        if update.is_open {
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
        let current_price = tick.mark_price.unwrap_or_else(|| {
            Px((tick.bid.0 + tick.ask.0) / Decimal::from(2))
        });

        let entry_price = position.entry_price.0;
        let current_price_val = current_price.0;

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
        

        
        // ✅ CRITICAL: Skip TP/SL check if is_maker is None
        // Problem: position.is_maker may be None if OrderUpdate hasn't arrived yet
        // If TP/SL triggers before OrderUpdate, wrong commission is used
        // Example: Maker order (0.02%) but None → taker (0.04%) used → early SL trigger!
        //
        // Previous solution: Use conservative taker commission when None
        // Problem: Still risks early SL trigger if order is actually maker
        //
        // Better solution: Skip TP/SL check until OrderUpdate arrives with is_maker info
        // This is safer because:
        // - OrderUpdate usually arrives very quickly (few hundred ms)
        // - Waiting a few ticks is safer than risking wrong commission calculation
        // - Prevents premature TP/SL triggers due to incorrect commission
        if position.is_maker.is_none() {
            debug!(
                symbol = %tick.symbol,
                "FOLLOW_ORDERS: Skipping TP/SL check - waiting for OrderUpdate with is_maker info. OrderUpdate should arrive soon (typically < 500ms)."
            );
            return Ok(());
        }

        let leverage_decimal = Decimal::from(position.leverage);
        let gross_pnl_pct = price_change_pct * leverage_decimal;
        
        // ✅ Entry commission calculation (is_maker is guaranteed to be Some at this point)
        let entry_commission_pct = if position.is_maker.unwrap() {
            // Maker order - use maker commission
            Decimal::from_str(&cfg.risk.maker_commission_pct.to_string())
                .unwrap_or_else(|_| Decimal::from_str("0.02").unwrap_or(Decimal::ZERO))
        } else {
            // Taker order - use taker commission
            Decimal::from_str(&cfg.risk.taker_commission_pct.to_string())
                .unwrap_or_else(|_| Decimal::from_str("0.04").unwrap_or(Decimal::ZERO))
        };

        let exit_commission_pct = Decimal::from_str(&cfg.risk.taker_commission_pct.to_string())
            .unwrap_or_else(|_| Decimal::from_str("0.04").unwrap_or(Decimal::ZERO));

        let total_commission_pct = entry_commission_pct + exit_commission_pct;
        
        // Calculate net PnL percentage (gross PnL - commission)
        // Net PnL% = Gross PnL% - (Commission% * 2)
        let net_pnl_pct = gross_pnl_pct - total_commission_pct;
        
        let net_pnl_pct_f64 = net_pnl_pct.to_f64().unwrap_or(0.0);
        let gross_pnl_pct_f64 = gross_pnl_pct.to_f64().unwrap_or(0.0);
        
        // Check take profit (using net PnL - commission included)
        if let Some(tp_pct) = position.take_profit_pct {
            if net_pnl_pct_f64 >= tp_pct {
                let close_request = CloseRequest {
                    symbol: tick.symbol.clone(),
                    position_id: None,
                    reason: CloseReason::TakeProfit,
                    current_bid: Some(tick.bid),
                    current_ask: Some(tick.ask),
                    timestamp: Instant::now(),
                };

                match event_bus.close_request_tx.send(close_request) {
                    Ok(receiver_count) => {
                        if receiver_count == 0 {
                            warn!(
                                symbol = %tick.symbol,
                                net_pnl_pct = net_pnl_pct_f64,
                                tp_pct,
                                "FOLLOW_ORDERS: CloseRequest sent but no subscribers (ORDERING may have shutdown), position will retry on next tick"
                            );
                            // Don't remove position from tracking - retry later when ORDERING is back
                            return Ok(());
                        }

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
                        warn!(
                            error = ?e,
                            symbol = %tick.symbol,
                            net_pnl_pct = net_pnl_pct_f64,
                            tp_pct,
                            "FOLLOW_ORDERS: CloseRequest failed for take profit, position will retry on next tick"
                        );
                        return Ok(());
                    }
                }
                return Ok(());
            }
        }
        
        // Check stop loss (using net PnL - commission included)
        if let Some(sl_pct) = position.stop_loss_pct {
            if net_pnl_pct_f64 <= -sl_pct {
                let close_request = CloseRequest {
                    symbol: tick.symbol.clone(),
                    position_id: None,
                    reason: CloseReason::StopLoss,
                    current_bid: Some(tick.bid),
                    current_ask: Some(tick.ask),
                    timestamp: Instant::now(),
                };

                match event_bus.close_request_tx.send(close_request) {
                    Ok(receiver_count) => {
                        if receiver_count == 0 {
                            warn!(
                                symbol = %tick.symbol,
                                net_pnl_pct = net_pnl_pct_f64,
                                sl_pct,
                                "FOLLOW_ORDERS: CloseRequest sent but no subscribers (ORDERING may have shutdown), position will retry on next tick"
                            );
                            // Don't remove position from tracking - retry later when ORDERING is back
                            return Ok(());
                        }

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
                        warn!(
                            error = ?e,
                            symbol = %tick.symbol,
                            net_pnl_pct = net_pnl_pct_f64,
                            sl_pct,
                            "FOLLOW_ORDERS: CloseRequest failed for stop loss, position will retry on next tick"
                        );
                        return Ok(());
                    }
                }
                return Ok(());
            }
        }
        
        Ok(())
    }
}

