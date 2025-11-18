use crate::config::AppCfg;
use crate::connection::Connection;
use crate::event_bus::{CloseRequest, CloseReason, EventBus, MarketTick, PositionUpdate, TradeSignal};
use crate::position_manager::{PositionState, should_close_position_smart};
use crate::types::{PositionDirection, PositionInfo, Px, Qty};
use crate::utils;
use anyhow::{anyhow, Result};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};
struct PositionPnL {
    price_change_pct_f64: f64,
    price_change_pct: Decimal,
    gross_pnl_pct_f64: f64,
    gross_pnl_pct: Decimal,
    net_pnl_pct_f64: f64,
    net_pnl_pct: Decimal,
    net_pnl_usd: f64,
    entry_commission_pct: Decimal,
    exit_commission_pct: Decimal,
    total_commission_pct: Decimal,
}
pub struct FollowOrders {
    cfg: Arc<AppCfg>,
    event_bus: Arc<EventBus>,
    shutdown_flag: Arc<AtomicBool>,
    connection: Arc<Connection>,
    positions: Arc<RwLock<HashMap<String, PositionInfo>>>,
    tp_sl_from_signals: Arc<RwLock<HashMap<String, (Option<f64>, Option<f64>)>>>,
    position_states: Arc<RwLock<HashMap<String, PositionState>>>,
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
            position_states: Arc::new(RwLock::new(HashMap::new())),
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
        let tp_sl_signals = tp_sl_from_signals.clone();
        let positions_signal = positions.clone();
        let event_bus_signal = event_bus.clone();
        let shutdown_flag_signal = shutdown_flag.clone();
        tokio::spawn(async move {
            let trade_signal_rx = event_bus_signal.subscribe_trade_signal();
            crate::event_loop::run_event_loop_async(
                trade_signal_rx,
                shutdown_flag_signal,
                "FOLLOW_ORDERS",
                "TradeSignal",
                move |signal| {
                    let tp_sl_signals = tp_sl_signals.clone();
                    let positions_signal = positions_signal.clone();
                    async move {
                        Self::handle_trade_signal(&signal, &tp_sl_signals, &positions_signal).await;
                    }
                },
            ).await;
        });
        let cfg_pos = cfg.clone();
        let positions_pos = positions.clone();
        let tp_sl_pos = tp_sl_from_signals.clone();
        let position_states_pos = self.position_states.clone();
        let event_bus_pos = event_bus.clone();
        let shutdown_flag_pos = shutdown_flag.clone();
        tokio::spawn(async move {
            let position_update_rx = event_bus_pos.subscribe_position_update();
            let cfg_pos_clone = cfg_pos.clone();
            let positions_pos_clone = positions_pos.clone();
            let tp_sl_pos_clone = tp_sl_pos.clone();
            let position_states_pos_clone = position_states_pos.clone();
            crate::event_loop::run_event_loop_async(
                position_update_rx,
                shutdown_flag_pos,
                "FOLLOW_ORDERS",
                "PositionUpdate",
                move |update| {
                    let cfg_pos = cfg_pos_clone.clone();
                    let positions_pos = positions_pos_clone.clone();
                    let tp_sl_pos = tp_sl_pos_clone.clone();
                    let position_states_pos = position_states_pos_clone.clone();
                    async move {
                        Self::handle_position_update(&update, &cfg_pos, &positions_pos, &tp_sl_pos, &position_states_pos).await;
                    }
                },
            ).await;
        });
        let positions_order = positions.clone();
        let event_bus_order = event_bus.clone();
        let shutdown_flag_order = shutdown_flag.clone();
        tokio::spawn(async move {
            let order_update_rx = event_bus_order.subscribe_order_update();
            let positions_order_clone = positions_order.clone();
            crate::event_loop::run_event_loop_async(
                order_update_rx,
                shutdown_flag_order,
                "FOLLOW_ORDERS",
                "OrderUpdate",
                move |update| {
                    let positions_order = positions_order_clone.clone();
                    async move {
                        if matches!(update.status, crate::event_bus::OrderStatus::Filled) {
                            let mut positions_guard = positions_order.write().await;
                            if let Some(position) = positions_guard.get_mut(&update.symbol) {
                                position.is_maker = update.is_maker;
                                debug!(
                                    symbol = %update.symbol,
                                    is_maker = ?update.is_maker,
                                    "FOLLOW_ORDERS: Updated is_maker info from OrderUpdate"
                                );
                            }
                        }
                    }
                },
            ).await;
        });
        let positions_tick = positions.clone();
        let position_states_tick = self.position_states.clone();
        let event_bus_tick = event_bus.clone();
        let shutdown_flag_tick = shutdown_flag.clone();
        let cfg_tick = cfg.clone();
        let connection_tick = self.connection.clone();
        tokio::spawn(async move {
            let market_tick_rx = event_bus_tick.subscribe_market_tick();
            let positions_tick_clone = positions_tick.clone();
            let position_states_tick_clone = position_states_tick.clone();
            let event_bus_tick_clone = event_bus_tick.clone();
            let cfg_tick_clone = cfg_tick.clone();
            let connection_tick_clone = connection_tick.clone();
            crate::event_loop::run_event_loop(
                market_tick_rx,
                shutdown_flag_tick,
                "FOLLOW_ORDERS",
                "MarketTick",
                move |tick| {
                    let positions_tick = positions_tick_clone.clone();
                    let position_states_tick = position_states_tick_clone.clone();
                    let event_bus_tick = event_bus_tick_clone.clone();
                    let cfg_tick = cfg_tick_clone.clone();
                    let connection_tick = connection_tick_clone.clone();
                    async move {
                        Self::check_tp_sl(&tick, &positions_tick, &position_states_tick, &event_bus_tick, &cfg_tick, &connection_tick).await
                    }
                },
            ).await;
        });
        Ok(())
    }
    async fn handle_trade_signal(
        signal: &TradeSignal,
        tp_sl_from_signals: &Arc<RwLock<HashMap<String, (Option<f64>, Option<f64>)>>>,
        positions: &Arc<RwLock<HashMap<String, PositionInfo>>>,
    ) {
        {
            let mut tp_sl_guard = tp_sl_from_signals.write().await;
            tp_sl_guard.insert(
                signal.symbol.clone(),
                (signal.stop_loss_pct, signal.take_profit_pct),
            );
        }
        {
            let mut positions_guard = positions.write().await;
            if let Some(position) = positions_guard.get_mut(&signal.symbol) {
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
        position_states: &Arc<RwLock<HashMap<String, PositionState>>>,
    ) {
        let mut positions_guard = positions.write().await;
        if update.is_open {
            let (stop_loss_pct, take_profit_pct) = {
                let tp_sl_guard = tp_sl_from_signals.read().await;
                tp_sl_guard.get(&update.symbol)
                    .cloned()
                    .unwrap_or((
                        Some(cfg.stop_loss_pct),
                        Some(cfg.take_profit_pct)
                    ))
            };
            let (direction, qty_abs) = PositionDirection::direction_and_abs_qty(update.qty);
            let opened_at = Instant::now();
            positions_guard.insert(update.symbol.clone(), PositionInfo {
                symbol: update.symbol.clone(),
                qty: qty_abs,
                entry_price: update.entry_price,
                direction,
                leverage: update.leverage,
                stop_loss_pct,
                take_profit_pct,
                opened_at,
                is_maker: None,
                close_requested: false,
                liquidation_price: update.liq_px,
                trailing_stop_placed: false,
            });
            {
                let mut states_guard = position_states.write().await;
                states_guard.insert(update.symbol.clone(), PositionState::new(opened_at));
            }
            info!(
                symbol = %update.symbol,
                qty = %qty_abs.0,
                entry_price = %update.entry_price.0,
                direction = ?direction,
                stop_loss_pct = ?stop_loss_pct,
                take_profit_pct = ?take_profit_pct,
                "FOLLOW_ORDERS: Position tracked with TP/SL and smart closing state"
            );
        } else {
            positions_guard.remove(&update.symbol);
            {
                let mut tp_sl_guard = tp_sl_from_signals.write().await;
                tp_sl_guard.remove(&update.symbol);
            }
            {
                let mut states_guard = position_states.write().await;
                states_guard.remove(&update.symbol);
            }
            info!(
                symbol = %update.symbol,
                "FOLLOW_ORDERS: Position closed, removed from tracking"
            );
        }
    }
    async fn get_position_and_price(
        tick: &MarketTick,
        positions: &Arc<RwLock<HashMap<String, PositionInfo>>>,
    ) -> Result<Option<(PositionInfo, Px)>> {
        let position = {
            let positions_read = positions.read().await;
            match positions_read.get(&tick.symbol) {
                Some(pos) => {
                    if pos.close_requested {
                        return Ok(None);
                    }
                    pos.clone()
                }
                None => {
                    return Ok(None);
                }
            }
        };
        let current_price = tick.mark_price.unwrap_or_else(|| {
            Px(crate::utils::calculate_mid_price(tick.bid, tick.ask))
        });
        Ok(Some((position, current_price)))
    }
    fn calculate_position_pnl(
        position: &PositionInfo,
        current_price: Px,
        cfg: &Arc<AppCfg>,
    ) -> Result<PositionPnL> {
        let entry_price = position.entry_price.0;
        let current_price_val = current_price.0;
        let price_change_pct_f64 = utils::calculate_pnl_percentage(
            entry_price,
            current_price_val,
            position.direction,
            position.leverage,
        );
        let price_change_pct = utils::f64_to_decimal_pct(price_change_pct_f64) * Decimal::from(100);
        const MAX_WAIT_FOR_ORDER_UPDATE_MS: u64 = 2000;
        let position_age_ms = position.opened_at.elapsed().as_millis() as u64;
        let entry_commission_pct = if let Some(is_maker) = position.is_maker {
            utils::get_commission_rate(is_maker, cfg.risk.maker_commission_pct, cfg.risk.taker_commission_pct)
        } else {
            if position_age_ms > MAX_WAIT_FOR_ORDER_UPDATE_MS {
                warn!(
                    symbol = %position.symbol,
                    position_age_ms,
                    max_wait_ms = MAX_WAIT_FOR_ORDER_UPDATE_MS,
                    "FOLLOW_ORDERS: OrderUpdate with is_maker info delayed > {}ms, using conservative taker commission estimate.",
                    MAX_WAIT_FOR_ORDER_UPDATE_MS
                );
            } else {
                debug!(
                    symbol = %position.symbol,
                    position_age_ms,
                    "FOLLOW_ORDERS: Using conservative taker commission estimate (OrderUpdate pending)"
                );
            }
            utils::get_commission_rate(false, cfg.risk.maker_commission_pct, cfg.risk.taker_commission_pct)
        };
        let gross_pnl_pct_f64 = price_change_pct_f64;
        let gross_pnl_pct = price_change_pct;
        let exit_commission_pct = utils::get_commission_rate(false, cfg.risk.maker_commission_pct, cfg.risk.taker_commission_pct);
        let total_commission_pct = entry_commission_pct + exit_commission_pct;
        let net_pnl_pct = gross_pnl_pct - total_commission_pct;
        let net_pnl_pct_f64 = utils::decimal_to_f64(net_pnl_pct);
        let position_qty_f64 = utils::decimal_to_f64(position.qty.0);
        let entry_price_f64 = utils::decimal_to_f64(position.entry_price.0);
        let current_price_f64 = utils::decimal_to_f64(current_price_val);
        let price_diff_decimal = utils::calculate_price_change(
            position.entry_price.0,
            current_price_val,
            position.direction,
        );
        let price_diff = utils::decimal_to_f64(price_diff_decimal * position.entry_price.0);
        let entry_notional = utils::f64_to_decimal(entry_price_f64, Decimal::ZERO) * position.qty.0;
        let exit_notional = utils::f64_to_decimal(current_price_f64, Decimal::ZERO) * position.qty.0;
        let total_commission = utils::calculate_total_commission(
            entry_notional,
            exit_notional,
            position.is_maker,
            cfg.risk.maker_commission_pct,
            cfg.risk.taker_commission_pct,
        );
        let gross_pnl_usd = price_diff * position_qty_f64;
        let net_pnl_usd = gross_pnl_usd - utils::decimal_to_f64(total_commission);
        Ok(PositionPnL {
            price_change_pct_f64,
            price_change_pct,
            gross_pnl_pct_f64,
            gross_pnl_pct,
            net_pnl_pct_f64,
            net_pnl_pct,
            net_pnl_usd,
            entry_commission_pct,
            exit_commission_pct,
            total_commission_pct,
        })
    }
    async fn send_close_request_and_remove_position(
        tick: &MarketTick,
        position: &PositionInfo,
        reason: CloseReason,
        positions: &Arc<RwLock<HashMap<String, PositionInfo>>>,
        position_states: &Arc<RwLock<HashMap<String, PositionState>>>,
        event_bus: &Arc<EventBus>,
    ) -> Result<bool> {
        let position_removed = {
            let mut positions_guard = positions.write().await;
            if let Some(pos) = positions_guard.get_mut(&tick.symbol) {
                pos.close_requested = true;
                positions_guard.remove(&tick.symbol).is_some()
            } else {
                false
            }
        };
        if !position_removed {
            return Ok(false);
        }
        {
            let mut states_guard = position_states.write().await;
            states_guard.remove(&tick.symbol);
        }
        let reason_for_log = reason.clone();
        let close_request = CloseRequest {
            symbol: tick.symbol.clone(),
            position_id: None,
            reason,
            current_bid: Some(tick.bid),
            current_ask: Some(tick.ask),
            timestamp: Instant::now(),
        };
        match event_bus.close_request_tx.send(close_request) {
            Ok(receiver_count) => {
                if receiver_count == 0 {
                    warn!(
                        symbol = %tick.symbol,
                        reason = ?reason_for_log,
                        "FOLLOW_ORDERS: CloseRequest sent but no subscribers"
                    );
                }
            }
            Err(e) => {
                warn!(
                    error = ?e,
                    symbol = %tick.symbol,
                    reason = ?reason_for_log,
                    "FOLLOW_ORDERS: CloseRequest failed"
                );
            }
        }
        Ok(true)
    }
    async fn check_tp_sl(
        tick: &MarketTick,
        positions: &Arc<RwLock<HashMap<String, PositionInfo>>>,
        position_states: &Arc<RwLock<HashMap<String, PositionState>>>,
        event_bus: &Arc<EventBus>,
        cfg: &Arc<AppCfg>,
        connection: &Arc<Connection>,
    ) -> Result<()> {
        let (position, current_price) = match Self::get_position_and_price(tick, positions).await? {
            Some(p) => p,
            None => return Ok(()),
        };
        let pnl = Self::calculate_position_pnl(&position, current_price, cfg)?;
        {
            let mut states_guard = position_states.write().await;
            if let Some(state) = states_guard.get_mut(&tick.symbol) {
                state.update_pnl(utils::f64_to_decimal(pnl.net_pnl_usd, Decimal::ZERO));
            }
        }
        let position_state = {
            let states_guard = position_states.read().await;
            states_guard.get(&tick.symbol).cloned()
        };
        if let Some(ref state) = position_state {
            let min_profit_usd = 0.50;
            let maker_fee_rate = cfg.risk.maker_commission_pct / 100.0;
            let taker_fee_rate = cfg.risk.taker_commission_pct / 100.0;
            let (should_close_smart, reason) = should_close_position_smart(
                &position,
                current_price,
                tick.bid,
                tick.ask,
                state,
                min_profit_usd,
                maker_fee_rate,
                taker_fee_rate,
            );
            if should_close_smart {
                info!(
                    symbol = %tick.symbol,
                    reason = %reason,
                    net_pnl_usd = pnl.net_pnl_usd,
                    "FOLLOW_ORDERS: Smart closing condition triggered"
                );
                if Self::send_close_request_and_remove_position(
                    tick,
                    &position,
                    CloseReason::Manual,
                    positions,
                    position_states,
                    event_bus,
                ).await? {
                    info!(
                        symbol = %tick.symbol,
                        reason = %reason,
                        net_pnl_usd = pnl.net_pnl_usd,
                        "FOLLOW_ORDERS: Smart closing CloseRequest sent"
                    );
                }
                return Ok(());
            }
        }
        if let Some(liquidation_price) = position.liquidation_price {
            let current_price_val = current_price.0;
            let liquidation_distance_pct = match position.direction {
                PositionDirection::Long => {
                    if liquidation_price.0 > Decimal::ZERO {
                        let distance = ((current_price_val - liquidation_price.0) / liquidation_price.0) * Decimal::from(100);
                        utils::decimal_to_f64(distance)
                    } else {
                        0.0
                    }
                }
                PositionDirection::Short => {
                    if current_price_val > Decimal::ZERO {
                        let distance = ((liquidation_price.0 - current_price_val) / current_price_val) * Decimal::from(100);
                        utils::decimal_to_f64(distance)
                    } else {
                        0.0
                    }
                }
            };
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
            if liquidation_distance_pct < 5.0 {
                error!(
                    symbol = %tick.symbol,
                    liquidation_distance_pct,
                    liquidation_price = %liquidation_price.0,
                    current_price = %current_price_val,
                    "CRITICAL: Liquidation risk too high ({}%), auto-closing position",
                    liquidation_distance_pct
                );
                if Self::send_close_request_and_remove_position(
                    tick,
                    &position,
                    CloseReason::Manual,
                    positions,
                    position_states,
                    event_bus,
                ).await? {
                    info!(
                        symbol = %tick.symbol,
                        liquidation_distance_pct,
                        "FOLLOW_ORDERS: Auto-closed position due to liquidation risk"
                    );
                }
                return Ok(());
            }
        }
        if let Some(tp_pct) = position.take_profit_pct {
            if pnl.net_pnl_pct_f64 >= tp_pct {
                if cfg.trending.use_trailing_stop && !position.trailing_stop_placed {
                    let tp_price = match position.direction {
                        PositionDirection::Long => {
                            position.entry_price.0 * (Decimal::ONE + utils::f64_to_decimal_pct(tp_pct))
                        }
                        PositionDirection::Short => {
                            position.entry_price.0 * (Decimal::ONE - utils::f64_to_decimal_pct(tp_pct))
                        }
                    };
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
                            let mut positions_guard = positions.write().await;
                            if let Some(pos) = positions_guard.get_mut(&tick.symbol) {
                                pos.trailing_stop_placed = true;
                            }
                            return Ok(());
                        }
                        Err(e) => {
                            error!(
                                error = %e,
                                symbol = %position.symbol,
                                "FOLLOW_ORDERS: Failed to place trailing stop order, falling back to immediate close"
                            );
                        }
                    }
                }
                if !Self::send_close_request_and_remove_position(
                    tick,
                    &position,
                    CloseReason::TakeProfit,
                    positions,
                    position_states,
                    event_bus,
                ).await? {
                    debug!(
                        symbol = %tick.symbol,
                        net_pnl_pct = pnl.net_pnl_pct_f64,
                        tp_pct,
                        "FOLLOW_ORDERS: Take profit triggered but position already removed by another thread"
                    );
                    return Ok(());
                }
                info!(
                    symbol = %tick.symbol,
                    net_pnl_pct = pnl.net_pnl_pct_f64,
                    gross_pnl_pct = pnl.gross_pnl_pct_f64,
                    entry_commission_pct = utils::decimal_to_f64(pnl.entry_commission_pct),
                    exit_commission_pct = utils::decimal_to_f64(pnl.exit_commission_pct),
                    total_commission_pct = utils::decimal_to_f64(pnl.total_commission_pct),
                    tp_pct,
                    leverage = position.leverage,
                    price_change_pct = utils::decimal_to_f64(pnl.price_change_pct),
                    "FOLLOW_ORDERS: Take profit triggered (net PnL), CloseRequest sent"
                );
                return Ok(());
            }
        }
        if let Some(sl_pct) = position.stop_loss_pct {
            if pnl.net_pnl_pct_f64 <= -sl_pct {
                if !Self::send_close_request_and_remove_position(
                    tick,
                    &position,
                    CloseReason::StopLoss,
                    positions,
                    position_states,
                    event_bus,
                ).await? {
                    debug!(
                        symbol = %tick.symbol,
                        net_pnl_pct = pnl.net_pnl_pct_f64,
                        sl_pct,
                        "FOLLOW_ORDERS: Stop loss triggered but position already removed by another thread"
                    );
                    return Ok(());
                }
                info!(
                    symbol = %tick.symbol,
                    net_pnl_pct = pnl.net_pnl_pct_f64,
                    gross_pnl_pct = pnl.gross_pnl_pct_f64,
                    entry_commission_pct = utils::decimal_to_f64(pnl.entry_commission_pct),
                    exit_commission_pct = utils::decimal_to_f64(pnl.exit_commission_pct),
                    total_commission_pct = utils::decimal_to_f64(pnl.total_commission_pct),
                    sl_pct,
                    leverage = position.leverage,
                    price_change_pct = utils::decimal_to_f64(pnl.price_change_pct),
                    "FOLLOW_ORDERS: Stop loss triggered (net PnL), CloseRequest sent"
                );
                return Ok(());
            }
        }
        Ok(())
    }
}
