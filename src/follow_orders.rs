// FOLLOW_ORDERS: Position tracking, TP/SL control
// Tracks open positions and sends CloseRequest when TP/SL is triggered
// Listens to PositionUpdate and MarketTick events
// Publishes CloseRequest events

use crate::config::AppCfg;
use crate::connection::Connection;
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
use tracing::{debug, error, info, warn};

/// FOLLOW_ORDERS module - position tracking and TP/SL control
pub struct FollowOrders {
    cfg: Arc<AppCfg>,
    event_bus: Arc<EventBus>,
    shutdown_flag: Arc<AtomicBool>,
    connection: Arc<Connection>,
    positions: Arc<RwLock<HashMap<String, PositionInfo>>>,
    tp_sl_from_signals: Arc<RwLock<HashMap<String, (Option<f64>, Option<f64>)>>>,
}

impl FollowOrders {
    pub fn new(
        cfg: Arc<AppCfg>,
        event_bus: Arc<EventBus>,
        shutdown_flag: Arc<AtomicBool>,
        connection: Arc<Connection>,
    ) -> Self {
        Self {
            cfg,
            event_bus,
            shutdown_flag,
            connection,
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
        let connection_tick = self.connection.clone();
        tokio::spawn(async move {
            let mut market_tick_rx = event_bus_tick.subscribe_market_tick();
            
            info!("FOLLOW_ORDERS: Started, listening to MarketTick events for TP/SL checking");
            
            loop {
                match market_tick_rx.recv().await {
                    Ok(tick) => {
                        if shutdown_flag_tick.load(AtomicOrdering::Relaxed) {
                            break;
                        }
                        
                        if let Err(e) = Self::check_tp_sl(&tick, &positions_tick, &event_bus_tick, &cfg_tick, &connection_tick).await {
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
                close_requested: false, // Initialize to false - will be set when CloseRequest is sent
                liquidation_price: update.liq_px, // ✅ NEW: Store liquidation price for risk monitoring
                trailing_stop_placed: false, // ✅ NEW: Initialize trailing stop flag
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
    /// 
    /// ✅ CRITICAL: Race condition prevention
    /// - Check close_requested flag BEFORE calculating PnL
    /// - Remove position and set flag BEFORE sending CloseRequest
    /// - Only send CloseRequest if position was successfully removed
    /// This prevents duplicate CloseRequest events when multiple ticks arrive simultaneously
    /// 
    /// Performance & Responsibility:
    /// - If position doesn't exist: Return immediately (no expensive calculations)
    /// - If position exists: Check TP/SL and send CloseRequest if triggered
    /// - This ensures FOLLOW_ORDERS only processes ticks when position is tracked
    async fn check_tp_sl(
        tick: &MarketTick,
        positions: &Arc<RwLock<HashMap<String, PositionInfo>>>,
        event_bus: &Arc<EventBus>,
        cfg: &Arc<AppCfg>,
        connection: &Arc<Connection>,
    ) -> Result<()> {
        // ✅ CRITICAL: Fast early check - position must exist to process TP/SL
        // Performance: If no position, return immediately (no expensive PnL calculations)
        // Responsibility: FOLLOW_ORDERS only processes when position is tracked
        // This is a fast read-only check, so do it first
        //
        // Race condition prevention (Layer 1 - Early Check):
        // - Check close_requested flag BEFORE expensive PnL calculations
        // - If flag is true, another thread is already processing close - skip this tick
        // - This prevents duplicate PnL calculations and reduces lock contention
        // - Note: This is a read lock, so multiple threads can check simultaneously
        // - The flag is set in Layer 2 (atomic remove), so this is a fast pre-filter
        let position = {
            let positions_read = positions.read().await;
            match positions_read.get(&tick.symbol) {
                Some(pos) => {
                    // ✅ CRITICAL: Early check for close_requested flag (read lock, fast)
                    // If another thread already set this flag, skip expensive PnL calculations
                    // This is the first line of defense against race conditions
                    // Performance: Avoids expensive calculations when close is already in progress
                    if pos.close_requested {
                        return Ok(());
                    }
                    pos.clone()
                }
                None => {
                    // No position for this symbol - return immediately
                    // Performance: Skip all expensive calculations (PnL, commission, etc.)
                    // This is expected behavior - FOLLOW_ORDERS waits for position to be tracked
                    return Ok(());
                }
            }
        };
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
        

        
        // ✅ CRITICAL: Use conservative commission estimate when is_maker is None
        // Problem: position.is_maker may be None if OrderUpdate hasn't arrived yet (100-500ms delay)
        // During this delay, fast price movements can cause SL to be missed
        // Example: Price drops 1% in 100ms → SL should trigger, but TP/SL check is skipped!
        //
        // Solution: Use conservative taker commission (higher) when is_maker is None
        // This ensures:
        // - SL triggers EARLIER (safer - prevents larger losses)
        // - TP triggers LATER (slightly less optimal, but safer)
        // - No delay in TP/SL checks (critical for fast markets)
        //
        // Timeout: If OrderUpdate doesn't arrive within 2 seconds, log warning
        // This prevents indefinite waiting and helps identify issues
        const MAX_WAIT_FOR_ORDER_UPDATE_MS: u64 = 2000; // 2 seconds
        let position_age_ms = position.opened_at.elapsed().as_millis() as u64;
        
        let entry_commission_pct = if let Some(is_maker) = position.is_maker {
            // OrderUpdate arrived - use actual commission
            if is_maker {
                // Maker order - use maker commission
                Decimal::from_str(&cfg.risk.maker_commission_pct.to_string())
                    .unwrap_or_else(|_| Decimal::from_str("0.02").unwrap_or(Decimal::ZERO))
            } else {
                // Taker order - use taker commission
                Decimal::from_str(&cfg.risk.taker_commission_pct.to_string())
                    .unwrap_or_else(|_| Decimal::from_str("0.04").unwrap_or(Decimal::ZERO))
            }
        } else {
            // OrderUpdate hasn't arrived yet - use conservative taker commission
            // This ensures SL triggers earlier (safer) and TP triggers later (acceptable)
            if position_age_ms > MAX_WAIT_FOR_ORDER_UPDATE_MS {
                warn!(
                    symbol = %tick.symbol,
                    position_age_ms,
                    max_wait_ms = MAX_WAIT_FOR_ORDER_UPDATE_MS,
                    "FOLLOW_ORDERS: OrderUpdate with is_maker info delayed > {}ms, using conservative taker commission estimate. This may cause slightly early SL triggers or late TP triggers.",
                    MAX_WAIT_FOR_ORDER_UPDATE_MS
                );
            } else {
                debug!(
                    symbol = %tick.symbol,
                    position_age_ms,
                    "FOLLOW_ORDERS: Using conservative taker commission estimate (OrderUpdate pending, typically arrives < 500ms)"
                );
            }
            // Use taker commission (higher) - conservative estimate
            Decimal::from_str(&cfg.risk.taker_commission_pct.to_string())
                .unwrap_or_else(|_| Decimal::from_str("0.04").unwrap_or(Decimal::ZERO))
        };

        let leverage_decimal = Decimal::from(position.leverage);
        let gross_pnl_pct = price_change_pct * leverage_decimal;
        
        let exit_commission_pct = Decimal::from_str(&cfg.risk.taker_commission_pct.to_string())
            .unwrap_or_else(|_| Decimal::from_str("0.04").unwrap_or(Decimal::ZERO));

        let total_commission_pct = entry_commission_pct + exit_commission_pct;
        
        // Calculate net PnL percentage (gross PnL - commission)
        // Net PnL% = Gross PnL% - (Commission% * 2)
        let net_pnl_pct = gross_pnl_pct - total_commission_pct;
        
        let net_pnl_pct_f64 = net_pnl_pct.to_f64().unwrap_or(0.0);
        let gross_pnl_pct_f64 = gross_pnl_pct.to_f64().unwrap_or(0.0);
        
        // ✅ NEW: Liquidation risk monitoring
        if let Some(liquidation_price) = position.liquidation_price {
            let liquidation_distance_pct = match position.direction {
                PositionDirection::Long => {
                    // Long: liquidation_price < current_price
                    // Distance = (current_price - liquidation_price) / liquidation_price * 100
                    if liquidation_price.0 > Decimal::ZERO {
                        let distance = ((current_price_val - liquidation_price.0) / liquidation_price.0) * Decimal::from(100);
                        distance.to_f64().unwrap_or(0.0)
                    } else {
                        0.0
                    }
                }
                PositionDirection::Short => {
                    // Short: liquidation_price > current_price
                    // Distance = (liquidation_price - current_price) / current_price * 100
                    if current_price_val > Decimal::ZERO {
                        let distance = ((liquidation_price.0 - current_price_val) / current_price_val) * Decimal::from(100);
                        distance.to_f64().unwrap_or(0.0)
                    } else {
                        0.0
                    }
                }
            };
            
            // Warning at 10% distance
            if liquidation_distance_pct < 10.0 && liquidation_distance_pct >= 5.0 {
                warn!(
                    symbol = %tick.symbol,
                    liquidation_distance_pct,
                    liquidation_price = %liquidation_price.0,
                    current_price = %current_price_val,
                    "CRITICAL: Position close to liquidation! Distance: {:.2}%",
                    liquidation_distance_pct
                );
            }
            
            // Auto-close at 5% distance
            if liquidation_distance_pct < 5.0 {
                error!(
                    symbol = %tick.symbol,
                    liquidation_distance_pct,
                    liquidation_price = %liquidation_price.0,
                    current_price = %current_price_val,
                    "CRITICAL: Liquidation risk too high ({}%), auto-closing position",
                    liquidation_distance_pct
                );
                
                // Send CloseRequest with Manual reason
                let position_removed = {
                    let mut positions_guard = positions.write().await;
                    if let Some(pos) = positions_guard.get_mut(&tick.symbol) {
                        pos.close_requested = true;
                        positions_guard.remove(&tick.symbol).is_some()
                    } else {
                        false
                    }
                };
                
                if position_removed {
                    let close_request = CloseRequest {
                        symbol: tick.symbol.clone(),
                        position_id: None,
                        reason: CloseReason::Manual,
                        current_bid: Some(tick.bid),
                        current_ask: Some(tick.ask),
                        timestamp: Instant::now(),
                    };
                    
                    if let Err(e) = event_bus.close_request_tx.send(close_request) {
                        error!(
                            error = ?e,
                            symbol = %tick.symbol,
                            "FOLLOW_ORDERS: Failed to send CloseRequest for liquidation risk"
                        );
                    } else {
                        info!(
                            symbol = %tick.symbol,
                            liquidation_distance_pct,
                            "FOLLOW_ORDERS: Auto-closed position due to liquidation risk"
                        );
                    }
                }
                
                return Ok(()); // Exit early - position will be closed
            }
        }
        
        // ✅ NEW: Trailing stop placement when TP threshold is reached
        if let Some(tp_pct) = position.take_profit_pct {
            if net_pnl_pct_f64 >= tp_pct {
                // TP threshold reached - check if trailing stop should be placed
                if cfg.trending.use_trailing_stop && !position.trailing_stop_placed {
                    // Calculate TP price (activation price for trailing stop)
                    let tp_price = match position.direction {
                        PositionDirection::Long => {
                            // Long: TP = entry * (1 + tp_pct / 100)
                            position.entry_price.0 * (Decimal::from(1) + Decimal::from_str(&format!("{}", tp_pct / 100.0)).unwrap_or(Decimal::ZERO))
                        }
                        PositionDirection::Short => {
                            // Short: TP = entry * (1 - tp_pct / 100)
                            position.entry_price.0 * (Decimal::from(1) - Decimal::from_str(&format!("{}", tp_pct / 100.0)).unwrap_or(Decimal::ZERO))
                        }
                    };
                    
                    // Place trailing stop order
                    match connection.place_trailing_stop_order(
                        &position.symbol,
                        Px(tp_price),
                        cfg.trending.trailing_stop_callback_rate,
                        position.qty,
                    ).await {
                        Ok(order_id) => {
                            info!(
                                symbol = %position.symbol,
                                order_id = %order_id,
                                activation_price = %tp_price,
                                callback_rate = cfg.trending.trailing_stop_callback_rate,
                                "FOLLOW_ORDERS: Trailing stop placed at TP threshold - position will be managed by trailing stop"
                            );
                            
                            // Mark as placed
                            let mut positions_guard = positions.write().await;
                            if let Some(pos) = positions_guard.get_mut(&tick.symbol) {
                                pos.trailing_stop_placed = true;
                            }
                            
                            // ✅ CRITICAL: Do NOT send CloseRequest when trailing stop is placed
                            // Trailing stop will manage the position closure automatically
                            // Return early to prevent immediate CloseRequest
                            return Ok(());
                        }
                        Err(e) => {
                            error!(
                                error = %e,
                                symbol = %position.symbol,
                                "FOLLOW_ORDERS: Failed to place trailing stop order, falling back to immediate close"
                            );
                            // Fall through to normal TP close if trailing stop placement fails
                        }
                    }
                }
                
                // ✅ CRITICAL: Remove position FIRST (before sending CloseRequest)
                // Race condition prevention (Layer 2 - Atomic Remove):
                // - Multiple threads can reach here if they all pass early check (close_requested flag)
                // - Atomic operation: acquire write lock → check position exists → set flag → remove
                // - Only ONE thread can successfully remove the position (HashMap::remove() is atomic)
                // - Threads that fail to remove (position already removed) skip CloseRequest
                //
                // Thread safety guarantee:
                // - Thread A: get_mut() → pos found, close_requested = true, remove() → true → CloseRequest sent ✓
                // - Thread B: get_mut() → pos found (if A hasn't removed yet), close_requested = true, remove() → false (A already removed) → skip ✓
                // - Thread C: get_mut() → None (A already removed) → skip ✓
                //
                // Result: Only ONE CloseRequest is sent, even if multiple threads trigger TP/SL simultaneously
                let position_removed = {
                    let mut positions_guard = positions.write().await;
                    // Double-check: position might have been removed by another thread between read and write lock
                    if let Some(pos) = positions_guard.get_mut(&tick.symbol) {
                        // Mark as close_requested to prevent duplicate requests (defensive, already checked in early check)
                        pos.close_requested = true;
                        // ✅ CRITICAL: Atomic remove - only ONE thread can successfully remove
                        // HashMap::remove() returns Some(value) if key existed, None if key didn't exist
                        // This ensures only the first thread to call remove() gets true
                        positions_guard.remove(&tick.symbol).is_some()
                    } else {
                        // Position already removed by another thread (between read lock and write lock)
                        false
                    }
                };

                // Only send CloseRequest if position was successfully removed
                // This ensures only ONE thread sends CloseRequest, even if multiple threads trigger TP/SL
                if !position_removed {
                    // Position was already removed by another thread - skip
                    // This is expected behavior in high-frequency scenarios where multiple ticks arrive simultaneously
                    debug!(
                        symbol = %tick.symbol,
                        net_pnl_pct = net_pnl_pct_f64,
                        tp_pct,
                        "FOLLOW_ORDERS: Take profit triggered but position already removed by another thread (race condition prevented)"
                    );
                    return Ok(());
                }

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
                                "FOLLOW_ORDERS: CloseRequest sent but no subscribers (ORDERING may have shutdown), position already removed from tracking"
                            );
                            // Position already removed - can't retry on next tick
                            return Ok(());
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
                            "FOLLOW_ORDERS: CloseRequest failed for take profit, position already removed from tracking"
                        );
                        // Position already removed - can't retry on next tick
                    }
                }
                return Ok(());
            }
        }
        
        // Check stop loss (using net PnL - commission included)
        if let Some(sl_pct) = position.stop_loss_pct {
            if net_pnl_pct_f64 <= -sl_pct {
                // ✅ CRITICAL: Remove position FIRST (before sending CloseRequest)
                // Race condition prevention (Layer 2 - Atomic Remove):
                // - Multiple threads can reach here if they all pass early check (close_requested flag)
                // - Atomic operation: acquire write lock → check position exists → set flag → remove
                // - Only ONE thread can successfully remove the position (HashMap::remove() is atomic)
                // - Threads that fail to remove (position already removed) skip CloseRequest
                //
                // Thread safety guarantee:
                // - Thread A: get_mut() → pos found, close_requested = true, remove() → true → CloseRequest sent ✓
                // - Thread B: get_mut() → pos found (if A hasn't removed yet), close_requested = true, remove() → false (A already removed) → skip ✓
                // - Thread C: get_mut() → None (A already removed) → skip ✓
                //
                // Result: Only ONE CloseRequest is sent, even if multiple threads trigger TP/SL simultaneously
                let position_removed = {
                    let mut positions_guard = positions.write().await;
                    // Double-check: position might have been removed by another thread between read and write lock
                    if let Some(pos) = positions_guard.get_mut(&tick.symbol) {
                        // Mark as close_requested to prevent duplicate requests (defensive, already checked in early check)
                        pos.close_requested = true;
                        // ✅ CRITICAL: Atomic remove - only ONE thread can successfully remove
                        // HashMap::remove() returns Some(value) if key existed, None if key didn't exist
                        // This ensures only the first thread to call remove() gets true
                        positions_guard.remove(&tick.symbol).is_some()
                    } else {
                        // Position already removed by another thread (between read lock and write lock)
                        false
                    }
                };

                // Only send CloseRequest if position was successfully removed
                // This ensures only ONE thread sends CloseRequest, even if multiple threads trigger TP/SL
                if !position_removed {
                    // Position was already removed by another thread - skip
                    // This is expected behavior in high-frequency scenarios where multiple ticks arrive simultaneously
                    debug!(
                        symbol = %tick.symbol,
                        net_pnl_pct = net_pnl_pct_f64,
                        sl_pct,
                        "FOLLOW_ORDERS: Stop loss triggered but position already removed by another thread (race condition prevented)"
                    );
                    return Ok(());
                }

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
                                "FOLLOW_ORDERS: CloseRequest sent but no subscribers (ORDERING may have shutdown), position already removed from tracking"
                            );
                            // Position already removed - can't retry on next tick
                            return Ok(());
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
                            "FOLLOW_ORDERS: CloseRequest failed for stop loss, position already removed from tracking"
                        );
                        // Position already removed - can't retry on next tick
                    }
                }
                return Ok(());
            }
        }
        
        Ok(())
    }
}

