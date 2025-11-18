use crate::config::AppCfg;
use crate::connection::Connection;
use crate::event_bus::{CloseRequest, EventBus, OrderUpdate, PositionUpdate, TradeSignal};
use crate::state::{OpenOrder, OpenPosition, OrderingState, SharedState};
use crate::types::{PositionDirection, Qty, Tif};
use crate::risk::{self, PositionRiskState};
use crate::utils;
use anyhow::{anyhow, Result};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
#[derive(Clone)]
struct BalanceCleanupMessage {
    asset: String,
    amount: Decimal,
    order_id: Option<String>,
    allocation_timestamp: Instant,
}
struct BalanceReservation {
    balance_store: Arc<tokio::sync::RwLock<crate::state::BalanceStore>>,
    asset: String,
    amount: Decimal,
    released: bool,
    order_id: Option<String>,
    allocation_timestamp: Instant,
    cleanup_tx: Option<mpsc::UnboundedSender<BalanceCleanupMessage>>,
    pending_cleanup: Option<Arc<std::sync::Mutex<Vec<BalanceCleanupMessage>>>>,
}
struct MarginCalculationResult {
    margin_usd: Decimal,
    commission_buffer: Decimal,
    safety_margin: Decimal,
    total_buffer: Decimal,
    usable_balance: Decimal,
}
impl BalanceReservation {
    fn new_with_lock(
        balance_store: Arc<tokio::sync::RwLock<crate::state::BalanceStore>>,
        store: &mut crate::state::BalanceStore,
        asset: &str,
        amount: Decimal,
        cleanup_tx: Option<mpsc::UnboundedSender<BalanceCleanupMessage>>,
        pending_cleanup: Option<Arc<std::sync::Mutex<Vec<BalanceCleanupMessage>>>>,
    ) -> Option<Self> {
        if store.try_reserve(asset, amount) {
            Some(Self {
                balance_store: balance_store.clone(),
                asset: asset.to_string(),
                amount,
                released: false,
                order_id: None,
                allocation_timestamp: Instant::now(),
                cleanup_tx,
                pending_cleanup,
            })
        } else {
            None
        }
    }
    async fn new(
        balance_store: Arc<tokio::sync::RwLock<crate::state::BalanceStore>>,
        asset: &str,
        amount: Decimal,
        cleanup_tx: Option<mpsc::UnboundedSender<BalanceCleanupMessage>>,
        pending_cleanup: Option<Arc<std::sync::Mutex<Vec<BalanceCleanupMessage>>>>,
    ) -> Option<Self> {
        let balance_store_clone = balance_store.clone();
        let mut store = balance_store.write().await;
        Self::new_with_lock(balance_store_clone, &mut store, asset, amount, cleanup_tx, pending_cleanup)
    }
    async fn release(&mut self) {
        if !self.released {
            let mut store = self.balance_store.write().await;
            store.release(&self.asset, self.amount);
            self.released = true;
            self.cleanup_tx = None;
            debug!(
                asset = %self.asset,
                amount = %self.amount,
                "ORDERING: Balance reservation released"
            );
        }
    }
}
impl Drop for BalanceReservation {
    fn drop(&mut self) {
        if !self.released {
            let allocation_age = Instant::now().duration_since(self.allocation_timestamp);
                let cleanup_msg = BalanceCleanupMessage {
                    asset: self.asset.clone(),
                    amount: self.amount,
                    order_id: self.order_id.clone(),
                    allocation_timestamp: self.allocation_timestamp,
                };
            let sent = if let Some(cleanup_tx) = self.cleanup_tx.take() {
                cleanup_tx.send(cleanup_msg.clone()).is_ok()
            } else {
                false
            };
            if sent {
                    tracing::warn!(
                        asset = %self.asset,
                        amount = %self.amount,
                        order_id = ?self.order_id,
                        allocation_age_secs = allocation_age.as_secs(),
                        "Balance reservation dropped without explicit release - cleanup task will release it"
                    );
                } else {
                if let Some(pending_cleanup) = &self.pending_cleanup {
                    if let Ok(mut pending) = pending_cleanup.lock() {
                        pending.push(cleanup_msg);
                        tracing::warn!(
                            asset = %self.asset,
                            amount = %self.amount,
                            order_id = ?self.order_id,
                            allocation_age_secs = allocation_age.as_secs(),
                            "Balance reservation added to pending cleanup list (channel was closed)"
                        );
                    } else {
                    tracing::error!(
                        asset = %self.asset,
                        amount = %self.amount,
                        order_id = ?self.order_id,
                        allocation_age_secs = allocation_age.as_secs(),
                            "CRITICAL: Balance reservation dropped without release AND pending_cleanup lock poisoned! Balance may be leaked. Order ID: {:?}, Allocation age: {}s. Manual recovery required.",
                        self.order_id,
                        allocation_age.as_secs()
                    );
                }
            } else {
                tracing::error!(
                    asset = %self.asset,
                    amount = %self.amount,
                    order_id = ?self.order_id,
                    allocation_age_secs = allocation_age.as_secs(),
                        "CRITICAL: Balance reservation dropped without release AND no cleanup mechanism available! Balance may be leaked. Order ID: {:?}, Allocation age: {}s. Manual recovery required.",
                    self.order_id,
                    allocation_age.as_secs()
                );
                }
            }
        }
    }
}
const MAX_RETRIES: u32 = 3;
pub struct Ordering {
    cfg: Arc<AppCfg>,
    connection: Arc<Connection>,
    event_bus: Arc<EventBus>,
    shutdown_flag: Arc<AtomicBool>,
    shared_state: Arc<SharedState>,
    balance_cleanup_tx: Arc<mpsc::UnboundedSender<BalanceCleanupMessage>>,
    pending_cleanup: Arc<std::sync::Mutex<Vec<BalanceCleanupMessage>>>,
}
impl Ordering {
    pub fn new(
        cfg: Arc<AppCfg>,
        connection: Arc<Connection>,
        event_bus: Arc<EventBus>,
        shutdown_flag: Arc<AtomicBool>,
        shared_state: Arc<SharedState>,
    ) -> Self {
        let (cleanup_tx, cleanup_rx) = mpsc::unbounded_channel();
        let pending_cleanup = Arc::new(std::sync::Mutex::new(Vec::<BalanceCleanupMessage>::new()));
        let balance_store_for_cleanup = shared_state.balance_store.clone();
        let shutdown_flag_for_cleanup = shutdown_flag.clone();
        let pending_cleanup_for_task = pending_cleanup.clone();
        tokio::spawn(Self::balance_cleanup_task(
            cleanup_rx, 
            balance_store_for_cleanup, 
            shutdown_flag_for_cleanup,
            pending_cleanup_for_task,
        ));
        Self {
            cfg,
            connection,
            event_bus,
            shutdown_flag,
            shared_state,
            balance_cleanup_tx: Arc::new(cleanup_tx),
            pending_cleanup,
        }
    }
    async fn balance_cleanup_task(
        mut cleanup_rx: mpsc::UnboundedReceiver<BalanceCleanupMessage>,
        balance_store: Arc<tokio::sync::RwLock<crate::state::BalanceStore>>,
        shutdown_flag: Arc<AtomicBool>,
        pending_cleanup: Arc<std::sync::Mutex<Vec<BalanceCleanupMessage>>>,
    ) {
        info!("ORDERING: Balance cleanup task started");
        loop {
            tokio::select! {
                msg = cleanup_rx.recv() => {
                    match msg {
                        Some(cleanup_msg) => {
                            let allocation_age = Instant::now().duration_since(cleanup_msg.allocation_timestamp);
                            let mut store = balance_store.write().await;
                            store.release(&cleanup_msg.asset, cleanup_msg.amount);
                            tracing::info!(
                                asset = %cleanup_msg.asset,
                                amount = %cleanup_msg.amount,
                                order_id = ?cleanup_msg.order_id,
                                allocation_age_secs = allocation_age.as_secs(),
                                "ORDERING: Balance reservation released by cleanup task (was dropped without explicit release)"
                            );
                        }
                        None => {
                            break;
                        }
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(100)) => {
                    let pending_msgs: Vec<BalanceCleanupMessage> = {
                        match pending_cleanup.lock() {
                            Ok(mut pending) => {
                                if !pending.is_empty() {
                                    std::mem::take(&mut *pending)
                                } else {
                                    Vec::new()
                                }
                            }
                            Err(e) => {
                                warn!(error = %e, "ORDERING: Mutex poisoned in balance cleanup, clearing pending messages");
                                Vec::new()
                            }
                        }
                    };
                    if !pending_msgs.is_empty() {
                        let balance_store_clone = balance_store.clone();
                        tokio::spawn(async move {
                            for cleanup_msg in pending_msgs {
                                let allocation_age = Instant::now().duration_since(cleanup_msg.allocation_timestamp);
                                let mut store = balance_store_clone.write().await;
                                store.release(&cleanup_msg.asset, cleanup_msg.amount);
                                tracing::info!(
                                    asset = %cleanup_msg.asset,
                                    amount = %cleanup_msg.amount,
                                    order_id = ?cleanup_msg.order_id,
                                    allocation_age_secs = allocation_age.as_secs(),
                                    "ORDERING: Balance reservation released from pending cleanup list (channel was closed)"
                                );
                            }
                        });
                    }
                    if shutdown_flag.load(AtomicOrdering::Relaxed) {
                        break;
                    }
                }
            }
        }
        let pending_msgs: Vec<BalanceCleanupMessage> = {
            match pending_cleanup.lock() {
                Ok(mut pending) => {
                    if !pending.is_empty() {
                        std::mem::take(&mut *pending)
                    } else {
                        Vec::new()
                    }
                }
                Err(e) => {
                    warn!(error = %e, "ORDERING: Mutex poisoned in balance cleanup final flush, clearing pending messages");
                    Vec::new()
                }
            }
        };
        if !pending_msgs.is_empty() {
            for cleanup_msg in pending_msgs {
                let allocation_age = Instant::now().duration_since(cleanup_msg.allocation_timestamp);
                let mut store = balance_store.write().await;
                store.release(&cleanup_msg.asset, cleanup_msg.amount);
                tracing::info!(
                    asset = %cleanup_msg.asset,
                    amount = %cleanup_msg.amount,
                    order_id = ?cleanup_msg.order_id,
                    allocation_age_secs = allocation_age.as_secs(),
                    "ORDERING: Balance reservation released during shutdown (from pending cleanup list)"
                );
            }
        }
        info!("ORDERING: Balance cleanup task stopped");
    }
    pub async fn start(&self) -> Result<()> {
        Self::reconcile_positions_on_startup(
            &self.connection,
            &self.shared_state,
            &self.event_bus,
        ).await?;
        Self::start_leak_detection_task(self.shared_state.clone(), self.shutdown_flag.clone());
        let event_bus = self.event_bus.clone();
        let shutdown_flag = self.shutdown_flag.clone();
        let connection = self.connection.clone();
        let shared_state = self.shared_state.clone();
        let cfg = self.cfg.clone();
        let state_trade = shared_state.clone();
        let event_bus_trade = event_bus.clone();
        let connection_trade = connection.clone();
        let shutdown_flag_trade = shutdown_flag.clone();
        let cfg_trade = cfg.clone();
        let cleanup_tx_trade = self.balance_cleanup_tx.clone();
        let pending_cleanup_trade = self.pending_cleanup.clone();
        tokio::spawn(async move {
            let trade_signal_rx = event_bus_trade.subscribe_trade_signal();
            let connection_trade_clone = connection_trade.clone();
            let state_trade_clone = state_trade.clone();
            let cfg_trade_clone = cfg_trade.clone();
            let event_bus_trade_clone = event_bus_trade.clone();
            let cleanup_tx_trade_clone = cleanup_tx_trade.clone();
            let pending_cleanup_trade_clone = pending_cleanup_trade.clone();
            crate::event_loop::run_event_loop(
                trade_signal_rx,
                shutdown_flag_trade,
                "ORDERING",
                "TradeSignal",
                move |signal| {
                    let connection_trade = connection_trade_clone.clone();
                    let state_trade = state_trade_clone.clone();
                    let cfg_trade = cfg_trade_clone.clone();
                    let event_bus_trade = event_bus_trade_clone.clone();
                    let cleanup_tx_trade = cleanup_tx_trade_clone.clone();
                    let pending_cleanup_trade = pending_cleanup_trade_clone.clone();
                    async move {
                        Self::handle_trade_signal(
                            &signal,
                            &connection_trade,
                            &state_trade,
                            &cfg_trade,
                            &event_bus_trade,
                            Some(cleanup_tx_trade),
                            Some(pending_cleanup_trade),
                        ).await
                    }
                },
            ).await;
        });
        let state_close = shared_state.clone();
        let event_bus_close = event_bus.clone();
        let connection_close = connection.clone();
        let shutdown_flag_close = shutdown_flag.clone();
        let cfg_close = cfg.clone();
        tokio::spawn(async move {
            let close_request_rx = event_bus_close.subscribe_close_request();
            let connection_close_clone = connection_close.clone();
            let state_close_clone = state_close.clone();
            let cfg_close_clone = cfg_close.clone();
            crate::event_loop::run_event_loop(
                close_request_rx,
                shutdown_flag_close,
                "ORDERING",
                "CloseRequest",
                move |request| {
                    let connection_close = connection_close_clone.clone();
                    let state_close = state_close_clone.clone();
                    let cfg_close = cfg_close_clone.clone();
                    async move {
                        Self::handle_close_request(
                            &request,
                            &connection_close,
                            &state_close,
                            &cfg_close,
                        ).await
                    }
                },
            ).await;
        });
        let state_order = shared_state.clone();
        let event_bus_order = event_bus.clone();
        let shutdown_flag_order = shutdown_flag.clone();
        tokio::spawn(async move {
            let order_update_rx = event_bus_order.subscribe_order_update();
            let state_order_clone = state_order.clone();
            let event_bus_order_clone = event_bus_order.clone();
            crate::event_loop::run_event_loop_async(
                order_update_rx,
                shutdown_flag_order,
                "ORDERING",
                "OrderUpdate",
                move |update| {
                    let state_order = state_order_clone.clone();
                    let event_bus_order = event_bus_order_clone.clone();
                    async move {
                        Self::handle_order_update(&update, &state_order, &event_bus_order, None).await;
                    }
                },
            ).await;
        });
        let state_pos = shared_state.clone();
        let event_bus_pos = event_bus.clone();
        let shutdown_flag_pos = shutdown_flag.clone();
        tokio::spawn(async move {
            let position_update_rx = event_bus_pos.subscribe_position_update();
            let state_pos_clone = state_pos.clone();
            let event_bus_pos_clone = event_bus_pos.clone();
            crate::event_loop::run_event_loop_async(
                position_update_rx,
                shutdown_flag_pos,
                "ORDERING",
                "PositionUpdate",
                move |update| {
                    let state_pos = state_pos_clone.clone();
                    let event_bus_pos = event_bus_pos_clone.clone();
                    async move {
                        Self::handle_position_update(&update, &state_pos, &event_bus_pos).await;
                    }
                },
            ).await;
        });
        Self::start_position_reconciliation_task(
            connection.clone(),
            shared_state.clone(),
            event_bus.clone(),
            shutdown_flag.clone(),
        );
        Ok(())
    }
    fn start_position_reconciliation_task(
        connection: Arc<Connection>,
        shared_state: Arc<SharedState>,
        event_bus: Arc<EventBus>,
        shutdown_flag: Arc<AtomicBool>,
    ) {
        tokio::spawn(async move {
            const RECONCILIATION_INTERVAL_SECS: u64 = 30;
            loop {
                tokio::time::sleep(Duration::from_secs(RECONCILIATION_INTERVAL_SECS)).await;
                if shutdown_flag.load(AtomicOrdering::Relaxed) {
                    break;
                }
                match connection.get_all_positions().await {
                    Ok(exchange_positions) => {
                        let exchange_symbols: Vec<String> = exchange_positions.iter().map(|(sym, _)| sym.clone()).collect();
                        let mut state_guard = shared_state.ordering_state.lock().await;
                        for (symbol, exchange_pos) in exchange_positions {
                            if let Some(internal_pos) = &state_guard.open_position {
                                if internal_pos.symbol == symbol {
                                    let epsilon_qty = Decimal::new(1, 6);
                                    const EPSILON_PRICE_PCT: f64 = 0.01;
                                    let should_update = Self::should_update_position(
                                        internal_pos.qty.0,
                                        internal_pos.entry_price.0,
                                        exchange_pos.qty.0,
                                        exchange_pos.entry.0,
                                        epsilon_qty,
                                        EPSILON_PRICE_PCT,
                                    );
                                    let qty_diff = (internal_pos.qty.0 - exchange_pos.qty.0).abs();
                                    let entry_diff = (internal_pos.entry_price.0 - exchange_pos.entry.0).abs();
                                    let price_diff_pct = if internal_pos.entry_price.0 > Decimal::ZERO {
                                        utils::calculate_percentage(entry_diff, internal_pos.entry_price.0)
                                    } else {
                                        entry_diff
                                    };
                                    let price_diff_pct_f64 = utils::decimal_to_f64(price_diff_pct);
                                    if should_update {
                                        warn!(
                                            symbol = %symbol,
                                            internal_qty = %internal_pos.qty.0,
                                            exchange_qty = %exchange_pos.qty.0,
                                            internal_entry = %internal_pos.entry_price.0,
                                            exchange_entry = %exchange_pos.entry.0,
                                            qty_diff = %qty_diff,
                                            price_diff_pct = price_diff_pct_f64,
                                            "ORDERING: Position mismatch detected, updating state"
                                        );
                                        state_guard.open_position = Some(OpenPosition {
                                            symbol: symbol.clone(),
                                            direction: if exchange_pos.qty.0.is_sign_positive() {
                                                PositionDirection::Long
                                            } else {
                                                PositionDirection::Short
                                            },
                                            qty: exchange_pos.qty,
                                            entry_price: exchange_pos.entry,
                                        });
                                        Self::publish_state_and_drop(state_guard, &event_bus);
                                        state_guard = shared_state.ordering_state.lock().await;
                                    }
                                } else {
                                    warn!(
                                        symbol = %symbol,
                                        internal_symbol = %internal_pos.symbol,
                                        "ORDERING: Exchange has position for different symbol - triggering reconciliation"
                                    );
                                    let position_update = PositionUpdate {
                                        symbol: symbol.clone(),
                                        qty: exchange_pos.qty,
                                        entry_price: exchange_pos.entry,
                                        leverage: exchange_pos.leverage,
                                        unrealized_pnl: None,
                                        is_open: true,
                                        liq_px: exchange_pos.liq_px,
                                        timestamp: Instant::now(),
                                    };
                                    drop(state_guard);
                                    if let Err(e) = event_bus.position_update_tx.send(position_update) {
                                        error!(
                                            error = ?e,
                                            symbol = %symbol,
                                            "ORDERING: Failed to send PositionUpdate for reconciliation"
                                        );
                                    }
                                    state_guard = shared_state.ordering_state.lock().await;
                                }
                            } else {
                                warn!(
                                    symbol = %symbol,
                                    "ORDERING: Exchange has position but internal state doesn't - triggering reconciliation"
                                );
                                let position_update = PositionUpdate {
                                    symbol: symbol.clone(),
                                    qty: exchange_pos.qty,
                                    entry_price: exchange_pos.entry,
                                    leverage: exchange_pos.leverage,
                                    unrealized_pnl: None,
                                    is_open: true,
                                    liq_px: exchange_pos.liq_px,
                                    timestamp: Instant::now(),
                                };
                                drop(state_guard);
                                if let Err(e) = event_bus.position_update_tx.send(position_update) {
                                    error!(
                                        error = ?e,
                                        symbol = %symbol,
                                        "ORDERING: Failed to send PositionUpdate for reconciliation"
                                    );
                                }
                                state_guard = shared_state.ordering_state.lock().await;
                            }
                        }
                        if let Some(internal_pos) = &state_guard.open_position {
                            let exchange_has = exchange_symbols.iter().any(|sym| sym == &internal_pos.symbol);
                            if !exchange_has {
                                warn!(
                                    symbol = %internal_pos.symbol,
                                    "ORDERING: Internal state has position but exchange doesn't - clearing state"
                                );
                                Self::clear_position(&mut state_guard);
                                Self::publish_state_and_drop(state_guard, &event_bus);
                            }
                        }
                    }
                    Err(e) => {
                        warn!(
                            error = %e,
                            "ORDERING: Failed to fetch positions for reconciliation"
                        );
                    }
                }
            }
        });
    }
    fn start_leak_detection_task(shared_state: Arc<SharedState>, shutdown_flag: Arc<AtomicBool>) {
        tokio::spawn(async move {
            const CHECK_INTERVAL_SECS: u64 = 10;
            loop {
                tokio::time::sleep(Duration::from_secs(CHECK_INTERVAL_SECS)).await;
                if shutdown_flag.load(AtomicOrdering::Relaxed) {
                    break;
                }
                let store = shared_state.balance_store.read().await;
                let usdt_leak = store.reserved_usdt > store.usdt;
                let usdc_leak = store.reserved_usdc > store.usdc;
                if usdt_leak || usdc_leak {
                    let open_order_info = {
                        let ordering_state = shared_state.ordering_state.lock().await;
                        ordering_state.open_order.as_ref().map(|order| {
                            format!("order_id={}, symbol={}, side={:?}, qty={}", 
                                order.order_id, order.symbol, order.side, order.qty.0)
                        })
                    };
                    tracing::error!(
                        usdt_total = %store.usdt,
                        usdt_reserved = %store.reserved_usdt,
                        usdc_total = %store.usdc,
                        usdc_reserved = %store.reserved_usdc,
                        open_order = ?open_order_info,
                        "CRITICAL: Balance leak detected! Reserved > Total. Open order info: {:?}. Auto-fixing...",
                        open_order_info
                    );
                    drop(store);
                    let mut store_write = shared_state.balance_store.write().await;
                    if usdt_leak {
                        let old_reserved = store_write.reserved_usdt;
                        store_write.reserved_usdt = store_write.usdt;
                        tracing::warn!(
                            asset = "USDT",
                            old_reserved = %old_reserved,
                            new_reserved = %store_write.reserved_usdt,
                            total = %store_write.usdt,
                            open_order = ?open_order_info,
                            "Auto-fixed USDT balance leak: reset reserved to total. Open order: {:?}",
                            open_order_info
                        );
                    }
                    if usdc_leak {
                        let old_reserved = store_write.reserved_usdc;
                        store_write.reserved_usdc = store_write.usdc;
                        tracing::warn!(
                            asset = "USDC",
                            old_reserved = %old_reserved,
                            new_reserved = %store_write.reserved_usdc,
                            total = %store_write.usdc,
                            open_order = ?open_order_info,
                            "Auto-fixed USDC balance leak: reset reserved to total. Open order: {:?}",
                            open_order_info
                        );
                    }
                }
            }
        });
    }
    fn calculate_position_size(
        signal: &TradeSignal,
        notional: Decimal,
        rules: &crate::types::SymbolRules,
    ) -> Result<Option<Qty>> {
        if signal.entry_price.0.is_zero() {
                warn!(
                    symbol = %signal.symbol,
                "ORDERING: Entry price is zero - skipping order"
            );
            return Ok(None);
        }
        if !rules.min_notional.is_zero() && notional < rules.min_notional {
            debug!(
                symbol = %signal.symbol,
                notional = %notional,
                min_notional = %rules.min_notional,
                "ORDERING: Ignoring TradeSignal - notional below minimum"
            );
            return Ok(None);
        }
        let size_raw = notional
            .checked_div(signal.entry_price.0)
            .ok_or_else(|| {
                anyhow!(
                    "Position size calculation error: notional={} / entry_price={}",
                    notional,
                    signal.entry_price.0
                )
            })?;
        if size_raw.is_zero() || size_raw.is_sign_negative() {
                warn!(
                    symbol = %signal.symbol,
                "ORDERING: Calculated position size is zero or negative - skipping order"
            );
            return Ok(None);
        }
        let size_quantized = crate::utils::quantize_decimal(size_raw, rules.step_size);
        if size_quantized.is_zero() {
            debug!(
                symbol = %signal.symbol,
                "ORDERING: Quantized size is zero - skipping order"
            );
            return Ok(None);
        }
        let notional_after_quantize = size_quantized
            .checked_mul(signal.entry_price.0)
            .ok_or_else(|| {
                anyhow!(
                    "Notional calculation after quantization overflow: size={} × entry_price={}",
                    size_quantized,
                    signal.entry_price.0
                )
            })?;
        if !rules.min_notional.is_zero() && notional_after_quantize < rules.min_notional {
            debug!(
                symbol = %signal.symbol,
                size_raw = %size_raw,
                size_quantized = %size_quantized,
                notional_before_quantize = %notional,
                notional_after_quantize = %notional_after_quantize,
                min_notional = %rules.min_notional,
                "ORDERING: Ignoring TradeSignal - notional after quantization below minimum"
            );
            return Ok(None);
        }
        Ok(Some(Qty(size_quantized)))
    }
    async fn calculate_margin_for_trade(
        signal: &TradeSignal,
        available_balance: Decimal,
        min_margin: Decimal,
        max_margin: Decimal,
        leverage: u32,
        cfg: &Arc<AppCfg>,
    ) -> Result<Option<MarginCalculationResult>> {
        let leverage_decimal = Decimal::from(leverage);
        let estimated_margin = match cfg.margin_strategy.as_str() {
            "fixed" => {
                utils::f64_to_decimal(cfg.max_usd_per_order, max_margin)
                    .max(min_margin)
                    .min(max_margin)
                    .min(available_balance)
            }
            "balance_based" | "max_balance" => {
                if available_balance >= min_margin {
                    available_balance.min(max_margin)
                } else {
                    debug!(
                        symbol = %signal.symbol,
                        available_balance = %available_balance,
                        min_margin = %min_margin,
                        "ORDERING: Ignoring TradeSignal - available balance below minimum margin"
                    );
                    return Ok(None);
                }
            }
            "dynamic" | "trend_based" => {
                if available_balance < min_margin {
                    debug!(
                        symbol = %signal.symbol,
                        available_balance = %available_balance,
                        min_margin = %min_margin,
                        "ORDERING: Ignoring TradeSignal - available balance below minimum margin"
                    );
                    return Ok(None);
                }
                let min_spread_bps = cfg.trending.min_spread_bps;
                let max_spread_bps = cfg.trending.max_spread_bps;
                let spread_range = max_spread_bps - min_spread_bps;
                let spread_quality = if spread_range > 0.0 {
                    let normalized = 1.0 - ((signal.spread_bps - min_spread_bps) / spread_range);
                    normalized.max(0.0).min(1.0)
                } else {
                    cfg.trending.default_spread_quality
                };
                let base_margin = available_balance.min(max_margin);
                let dynamic_margin = base_margin * utils::f64_to_decimal(
                    spread_quality,
                    utils::f64_to_decimal(cfg.trending.default_spread_quality, Decimal::from(50) / Decimal::from(100))
                );
                dynamic_margin.max(min_margin).min(max_margin).min(available_balance)
            }
            _ => {
                if available_balance >= min_margin {
                    available_balance.min(max_margin)
                } else {
                    debug!(
                        symbol = %signal.symbol,
                        available_balance = %available_balance,
                        min_margin = %min_margin,
                        "ORDERING: Ignoring TradeSignal - available balance below minimum margin"
                    );
                    return Ok(None);
                }
            }
        };
        let estimated_notional = estimated_margin * leverage_decimal;
        let taker_commission_rate = utils::get_commission_rate(false, cfg.risk.maker_commission_pct, cfg.risk.taker_commission_pct);
        let total_commission_rate = taker_commission_rate * Decimal::from(2);
        let calculated_commission_buffer = estimated_notional * total_commission_rate;
        let min_commission_buffer = utils::f64_to_decimal(cfg.risk.min_commission_buffer_usd, Decimal::from(1) / Decimal::from(2));
        let commission_buffer = calculated_commission_buffer.max(min_commission_buffer);
        let estimated_margin_for_safety = utils::f64_to_decimal(cfg.max_usd_per_order, max_margin)
            .max(min_margin)
            .min(max_margin);
        let safety_margin = (estimated_margin_for_safety * Decimal::from(5) / Decimal::from(100))
            .max(Decimal::from(1));
        let total_buffer = commission_buffer + safety_margin;
        let usable_balance = available_balance.saturating_sub(total_buffer);
        let margin_usd = match cfg.margin_strategy.as_str() {
            "fixed" => {
                utils::f64_to_decimal(cfg.max_usd_per_order, max_margin)
                    .max(min_margin)
                    .min(max_margin)
                    .min(usable_balance)
            }
            "balance_based" | "max_balance" => {
                if usable_balance >= min_margin {
                    usable_balance.min(max_margin)
                } else {
                    debug!(
                        symbol = %signal.symbol,
                        available_balance = %available_balance,
                        usable_balance = %usable_balance,
                        commission_buffer = %commission_buffer,
                        safety_margin = %safety_margin,
                        total_buffer = %total_buffer,
                        min_margin = %min_margin,
                        "ORDERING: Ignoring TradeSignal - usable balance (after commission buffer and safety margin) below minimum margin"
                    );
                    return Ok(None);
                }
            }
            "dynamic" | "trend_based" => {
                if usable_balance < min_margin {
                    debug!(
                        symbol = %signal.symbol,
                        available_balance = %available_balance,
                        usable_balance = %usable_balance,
                        commission_buffer = %commission_buffer,
                        safety_margin = %safety_margin,
                        total_buffer = %total_buffer,
                        min_margin = %min_margin,
                        "ORDERING: Ignoring TradeSignal - usable balance (after commission buffer and safety margin) below minimum margin"
                    );
                    return Ok(None);
                }
                let min_spread_bps = cfg.trending.min_spread_bps;
                let max_spread_bps = cfg.trending.max_spread_bps;
                let spread_range = max_spread_bps - min_spread_bps;
                let spread_quality = if spread_range > 0.0 {
                    let normalized = 1.0 - ((signal.spread_bps - min_spread_bps) / spread_range);
                    normalized.max(0.0).min(1.0)
                } else {
                    cfg.trending.default_spread_quality
                };
                let base_margin = usable_balance.min(max_margin);
                let dynamic_margin = base_margin * utils::f64_to_decimal(
                    spread_quality,
                    utils::f64_to_decimal(cfg.trending.default_spread_quality, Decimal::from(50) / Decimal::from(100))
                );
                let final_margin = dynamic_margin.max(min_margin).min(max_margin).min(usable_balance);
                debug!(
                    symbol = %signal.symbol,
                    spread_bps = signal.spread_bps,
                    spread_quality = spread_quality,
                    base_margin = %base_margin,
                    dynamic_margin = %final_margin,
                    "ORDERING: Dynamic margin calculated based on spread quality"
                );
                final_margin
            }
            _ => {
                if usable_balance >= min_margin {
                    usable_balance.min(max_margin)
                } else {
                    debug!(
                        symbol = %signal.symbol,
                        available_balance = %available_balance,
                        usable_balance = %usable_balance,
                        commission_buffer = %commission_buffer,
                        safety_margin = %safety_margin,
                        total_buffer = %total_buffer,
                        min_margin = %min_margin,
                        "ORDERING: Ignoring TradeSignal - usable balance (after commission buffer and safety margin) below minimum margin"
                    );
                    return Ok(None);
                }
            }
        };
        Ok(Some(MarginCalculationResult {
                    margin_usd,
            commission_buffer,
            safety_margin,
            total_buffer,
            usable_balance,
        }))
    }
    async fn validate_trade_signal(
        signal: &TradeSignal,
        shared_state: &Arc<SharedState>,
    ) -> Result<()> {
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
            warn!(
                symbol = %signal.symbol,
                "ORDERING: Ignoring TradeSignal - timestamp in the future"
            );
            return Ok(());
        }
        if signal.symbol.is_empty() {
            warn!("ORDERING: Ignoring TradeSignal - empty symbol");
            return Ok(());
        }
        {
            let mut state_guard = shared_state.ordering_state.lock().await;
            state_guard.circuit_breaker.reset_if_cooldown_passed();
            if state_guard.circuit_breaker.should_block() {
            warn!(
                symbol = %signal.symbol,
                    reject_count = state_guard.circuit_breaker.reject_count,
                    "ORDERING: Circuit breaker is OPEN - blocking order placement to prevent Binance ban"
            );
            return Ok(());
            }
        }
        Ok(())
    }
    async fn select_balance_for_trade(
        signal: &TradeSignal,
        cfg: &Arc<AppCfg>,
        shared_state: &Arc<SharedState>,
    ) -> Result<(Decimal, String)> {
        let symbol_upper = signal.symbol.to_uppercase();
        let symbol_quote_asset = if symbol_upper.ends_with("USDC") {
            "USDC"
        } else if symbol_upper.ends_with("USDT") {
            "USDT"
        } else {
            &cfg.quote_asset
        };
        let min_margin = utils::f64_to_decimal(cfg.min_margin_usd, Decimal::from(10));
        let balance_store = shared_state.balance_store.read().await;
        let usdt_balance = balance_store.usdt;
        let usdc_balance = balance_store.usdc;
        let (balance, quote) = match symbol_quote_asset {
            "USDT" => {
                if usdt_balance >= min_margin {
                    (usdt_balance, "USDT")
                } else if usdc_balance >= min_margin {
                    (usdc_balance, "USDC")
                } else {
                    (usdt_balance, "USDT")
                }
            }
            "USDC" => {
                if usdc_balance >= min_margin {
                    (usdc_balance, "USDC")
                } else if usdt_balance >= min_margin {
                    (usdt_balance, "USDT")
                } else {
                    (usdc_balance, "USDC")
                }
            }
            _ => {
                if usdt_balance >= min_margin {
                    (usdt_balance, "USDT")
                } else if usdc_balance >= min_margin {
                    (usdc_balance, "USDC")
                } else {
                    let balance = if cfg.quote_asset.to_uppercase() == "USDT" {
                        usdt_balance
                    } else {
                        usdc_balance
                    };
                    (balance, cfg.quote_asset.as_str())
                }
            }
        };
            debug!(
                symbol = %signal.symbol,
            symbol_quote_asset = symbol_quote_asset,
            selected_quote_asset = quote,
            available_balance = %balance,
            usdt_balance = %usdt_balance,
            usdc_balance = %usdc_balance,
            min_margin = %min_margin,
            last_updated = ?balance_store.last_updated,
            "ORDERING: Balance check for trade signal (selected quote asset: {})",
            quote
        );
        if symbol_quote_asset != quote {
            warn!(
                symbol = %signal.symbol,
                symbol_quote_asset = symbol_quote_asset,
                selected_quote_asset = quote,
                "ORDERING: Quote asset mismatch - symbol requires {} but selected {} balance. This may indicate symbol discovery issue.",
                symbol_quote_asset,
                quote
            );
        }
        Ok((balance, quote.to_string()))
    }
    async fn handle_trade_signal(
        signal: &TradeSignal,
        connection: &Arc<Connection>,
        shared_state: &Arc<SharedState>,
        cfg: &Arc<AppCfg>,
        event_bus: &Arc<EventBus>,
        cleanup_tx: Option<Arc<mpsc::UnboundedSender<BalanceCleanupMessage>>>,
        pending_cleanup: Option<Arc<std::sync::Mutex<Vec<BalanceCleanupMessage>>>>,
    ) -> Result<()> {
        Self::validate_trade_signal(signal, shared_state).await?;
        let now = Instant::now();
        let (available_balance, selected_quote_asset) = Self::select_balance_for_trade(signal, cfg, shared_state).await?;
        let min_margin = utils::f64_to_decimal(cfg.min_margin_usd, Decimal::from(10));
        if available_balance < min_margin {
            debug!(
                symbol = %signal.symbol,
                selected_quote_asset = %selected_quote_asset,
                available_balance = %available_balance,
                min_margin = %min_margin,
                "ORDERING: Insufficient balance for minimum margin in both USDT and USDC - skipping order"
            );
            return Ok(());
        }
        let max_margin = utils::f64_to_decimal(cfg.max_margin_usd, Decimal::from(100));
        let rules = match connection.rules_for(&signal.symbol).await {
            Ok(r) => r,
            Err(e) => {
                warn!(
                    error = %e,
                    symbol = %signal.symbol,
                    "ORDERING: Failed to fetch symbol rules, skipping order"
                );
                return Ok(());
            }
        };
        let desired_leverage = cfg.leverage.unwrap_or(cfg.exec.default_leverage);
        let leverage = connection.get_clamped_leverage(&signal.symbol, desired_leverage).await;
        let margin_result = match Self::calculate_margin_for_trade(
            signal,
            available_balance,
            min_margin,
            max_margin,
            leverage,
            cfg,
        ).await? {
            Some(result) => result,
            None => return Ok(()),
        };
        let margin_usd = margin_result.margin_usd;
        let _commission_buffer = margin_result.commission_buffer;
        let _safety_margin = margin_result.safety_margin;
        let _total_buffer = margin_result.total_buffer;
        let _usable_balance = margin_result.usable_balance;
        let leverage_decimal = Decimal::from(leverage);
        let notional = margin_usd
            .checked_mul(leverage_decimal)
            .ok_or_else(|| {
                anyhow!(
                    "Notional calculation overflow: margin={} × leverage={}",
                    margin_usd,
                    leverage
                )
            })?;
        let size = match Self::calculate_position_size(signal, notional, &rules)? {
            Some(s) => s,
            None => return Ok(()),
        };
        let required_margin = margin_usd;
        let calculated_size = size.clone();
        debug!(
            symbol = %signal.symbol,
            side = ?signal.side,
            margin_usd = %margin_usd,
            leverage,
            notional = %notional,
            size = %size.0,
            entry_price = %signal.entry_price.0,
            "ORDERING: Position sizing calculated from balance and config"
        );
        let tif = match cfg.exec.tif.as_str() {
            "post_only" | "GTX" => Tif::PostOnly,
            "ioc" | "IOC" => Tif::Ioc,
            _ => Tif::Gtc,
        };
        use crate::types::OrderCommand;
        let command = OrderCommand::Open {
            symbol: signal.symbol.clone(),
            side: signal.side,
            price: signal.entry_price,
            qty: size,
            tif,
        };
        let mut balance_reservation = {
            let mut state_guard = shared_state.ordering_state.lock().await;
            if state_guard.open_position.is_some() || state_guard.open_order.is_some() {
                debug!(
                    symbol = %signal.symbol,
                    has_position = state_guard.open_position.is_some(),
                    has_order = state_guard.open_order.is_some(),
                    "ORDERING: Ignoring TradeSignal - already have open position/order"
                );
                return Ok(());
            }
            if let Some(ref open_pos) = state_guard.open_position {
                if open_pos.symbol == signal.symbol {
                    let position_size_notional = (open_pos.entry_price.0 * open_pos.qty.0.abs())
                        .to_f64()
                        .unwrap_or(0.0);
                    let total_active_orders_notional = if let Some(ref open_order) = state_guard.open_order {
                        if open_order.symbol == signal.symbol {
                            utils::decimal_to_f64(signal.entry_price.0 * open_order.qty.0)
                        } else {
                            0.0
                        }
                    } else {
                        0.0
                    };
                    let risk_state = PositionRiskState {
                        position_size_notional,
                        total_active_orders_notional,
                        has_open_orders: state_guard.open_order.is_some(),
                        is_opportunity_mode: false,
                    };
                    let effective_leverage = leverage as f64;
                    let (risk_level, _max_position_size_usd, should_block_new_orders) = 
                        risk::check_position_size_risk(
                            &risk_state,
                            cfg.max_usd_per_order,
                            effective_leverage,
                            cfg,
                        );
                    if should_block_new_orders {
                        warn!(
                            symbol = %signal.symbol,
                            risk_level = ?risk_level,
                            position_size_notional,
                            total_active_orders_notional,
                            "ORDERING: Blocking new order due to risk level"
                        );
                        return Ok(());
                    }
                }
            }
            let spread_age = now.duration_since(signal.spread_timestamp);
            const MAX_SPREAD_AGE_SECS: u64 = 5;
            if spread_age.as_secs() > MAX_SPREAD_AGE_SECS {
                warn!(
                    symbol = %signal.symbol,
                    spread_age_secs = spread_age.as_secs(),
                    max_age_secs = MAX_SPREAD_AGE_SECS,
                    "ORDERING: Signal is too stale ({} seconds old), aborting. Spread may have changed significantly.",
                    spread_age.as_secs()
                );
                return Ok(());
            }
            let balance_store_clone = shared_state.balance_store.clone();
                let mut balance_store_guard = shared_state.balance_store.write().await;
                let available_balance_atomic = balance_store_guard.available(&selected_quote_asset);
                if available_balance_atomic < required_margin {
                debug!(
                    symbol = %signal.symbol,
                    required_margin = %required_margin,
                        available_balance = %available_balance_atomic,
                    quote_asset = %selected_quote_asset,
                        "ORDERING: Ignoring TradeSignal - insufficient balance (atomic check)"
                );
                return Ok(());
            }
            let reservation = match BalanceReservation::new_with_lock(
                    balance_store_clone,
                    &mut balance_store_guard,
                &selected_quote_asset,
                required_margin,
                cleanup_tx.as_ref().map(|tx| tx.as_ref().clone()),
                    pending_cleanup.clone(),
                ) {
                Some(reservation) => {
                    state_guard.reserved_margin += required_margin;
                    reservation
                }
                None => {
                    debug!(
                        symbol = %signal.symbol,
                        required_margin = %required_margin,
                            available_balance = %available_balance_atomic,
                        quote_asset = %selected_quote_asset,
                            "ORDERING: Ignoring TradeSignal - balance reservation failed (insufficient balance)"
                    );
                    return Ok(());
                }
            };
            drop(balance_store_guard);
            reservation
        };
        let order_id = {
            let mut last_error: Option<anyhow::Error> = None;
            let mut order_id_result: Option<String> = None;
            for attempt in 0..MAX_RETRIES {
                debug!(
                    symbol = %signal.symbol,
                    side = ?signal.side,
                    attempt = attempt + 1,
                    max_retries = MAX_RETRIES,
                    "ORDERING: Attempting to place order"
                );
                match connection.send_order(command.clone()).await {
                    Ok(id) => {
                        {
                            let mut state_guard = shared_state.ordering_state.lock().await;
                            state_guard.circuit_breaker.reset();
                        }
                        info!(
                            symbol = %signal.symbol,
                            order_id = %id,
                            side = ?signal.side,
                            attempt = attempt + 1,
                            "ORDERING: Order placed successfully"
                        );
                        order_id_result = Some(id);
                        break;
                    }
                    Err(e) => {
                        if utils::is_permanent_error(&e) {
                            warn!(
                                error = %e,
                                symbol = %signal.symbol,
                                attempt = attempt + 1,
                                "ORDERING: Permanent error, not retrying"
                            );
                            {
                                let mut state_guard = shared_state.ordering_state.lock().await;
                                Self::release_reserved_margin(&mut state_guard, required_margin);
                            }
                            balance_reservation.release().await;
                            return Err(e);
                        }
                        last_error = Some(anyhow::format_err!("{}", e));
                        if attempt < MAX_RETRIES - 1 {
                            let delay = utils::exponential_backoff(attempt, 100, 2);
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
                            warn!(
                                error = %e,
                                symbol = %signal.symbol,
                                attempt = attempt + 1,
                                "ORDERING: All retries exhausted"
                            );
                            {
                                let mut state_guard = shared_state.ordering_state.lock().await;
                                Self::release_reserved_margin(&mut state_guard, required_margin);
                            }
                            balance_reservation.release().await;
                            return Err(e);
                        }
                    }
                }
            }
            match order_id_result {
                Some(id) => {
                    balance_reservation.order_id = Some(id.clone());
                    id
                }
                None => {
                    {
                        let mut state_guard = shared_state.ordering_state.lock().await;
                        Self::release_reserved_margin(&mut state_guard, required_margin);
                    }
                    balance_reservation.release().await;
                    return Err(
                        last_error.unwrap_or_else(|| anyhow!("Unknown error after retries"))
                    );
                }
            }
        };
        match connection.get_current_prices(&signal.symbol).await {
            Ok((current_bid, current_ask)) => {
                if !current_bid.0.is_zero() {
                    let current_spread_bps_f64 = crate::utils::calculate_spread_bps(current_bid, current_ask);
                    let min_acceptable_spread_bps = cfg.trending.min_spread_bps;
                    let max_acceptable_spread_bps = cfg.trending.max_spread_bps;
                    if current_spread_bps_f64 < min_acceptable_spread_bps || current_spread_bps_f64 > max_acceptable_spread_bps {
                        warn!(
                            symbol = %signal.symbol,
                            order_id = %order_id,
                            current_spread_bps = current_spread_bps_f64,
                            signal_spread_bps = signal.spread_bps,
                            min_spread_bps = min_acceptable_spread_bps,
                            max_spread_bps = max_acceptable_spread_bps,
                            "ORDERING: Spread changed after order placement ({} bps -> {} bps), canceling order to prevent slippage",
                            signal.spread_bps,
                            current_spread_bps_f64
                        );
                        match connection.cancel_order(&order_id, &signal.symbol).await {
                            Ok(()) => {
                                info!(
                                    symbol = %signal.symbol,
                                    order_id = %order_id,
                                    "ORDERING: Successfully canceled order due to spread change"
                                );
                                {
                                    let mut state_guard = shared_state.ordering_state.lock().await;
                                    Self::release_reserved_margin(&mut state_guard, required_margin);
                                }
                                balance_reservation.release().await;
                                return Ok(());
                            }
                            Err(cancel_err) => {
                                warn!(
                                    error = %cancel_err,
                                    symbol = %signal.symbol,
                                    order_id = %order_id,
                                    "ORDERING: Failed to cancel order after spread change (order may have already filled)"
                                );
                            }
                        }
                    } else {
                        let spread_change = (current_spread_bps_f64 - signal.spread_bps).abs();
                        if spread_change > 10.0 {
                            warn!(
                                symbol = %signal.symbol,
                                order_id = %order_id,
                                signal_spread_bps = signal.spread_bps,
                                current_spread_bps = current_spread_bps_f64,
                                spread_change_bps = spread_change,
                                "ORDERING: Spread changed after order placement ({} bps -> {} bps), but still within acceptable range",
                                signal.spread_bps,
                                current_spread_bps_f64
                            );
                        }
                    }
                }
            }
            Err(e) => {
                warn!(
                    error = %e,
                    symbol = %signal.symbol,
                    order_id = %order_id,
                    "ORDERING: Failed to get current prices for post-order spread validation, proceeding with order (order was placed with acceptable spread at that moment)"
                );
            }
        }
        {
            let mut state_guard = shared_state.ordering_state.lock().await;
            if state_guard.open_order.is_none() && state_guard.open_position.is_none() {
                state_guard.open_order = Some(OpenOrder {
                    symbol: signal.symbol.clone(),
                    order_id: order_id.clone(),
                    side: signal.side,
                    qty: calculated_size,
                });
                state_guard.last_order_update_timestamp = Some(Instant::now());
                Self::publish_state_and_drop(state_guard, event_bus);
                {
                    let mut state_guard = shared_state.ordering_state.lock().await;
                    state_guard.circuit_breaker.reset();
                    Self::release_reserved_margin(&mut state_guard, required_margin);
                }
                info!(
                    symbol = %signal.symbol,
                    side = ?signal.side,
                    order_id = %order_id,
                    "ORDERING: Order placed successfully"
                );
                let order_id_timeout = order_id.clone();
                let symbol_timeout = signal.symbol.clone();
                let connection_timeout = connection.clone();
                let shared_state_timeout = shared_state.clone();
                let required_margin_timeout = required_margin;
                {
                    let mut state_guard = shared_state_timeout.ordering_state.lock().await;
                    state_guard.order_timeouts.insert(
                        order_id_timeout.clone(),
                        Instant::now() + Duration::from_secs(30),
                    );
                }
                tokio::spawn(async move {
                    const ORDER_TIMEOUT_SECS: u64 = 30;
                    tokio::time::sleep(Duration::from_secs(ORDER_TIMEOUT_SECS)).await;
                    let mut state_guard = shared_state_timeout.ordering_state.lock().await;
                    if let Some(open_order) = &state_guard.open_order {
                        if open_order.order_id == order_id_timeout {
                            warn!(
                                order_id = %order_id_timeout,
                                symbol = %symbol_timeout,
                                timeout_secs = ORDER_TIMEOUT_SECS,
                                "ORDERING: Order timeout - canceling stale order"
                            );
                            Self::release_reserved_margin(&mut state_guard, required_margin_timeout);
                            Self::clear_order(&mut state_guard);
                            state_guard.order_timeouts.remove(&order_id_timeout);
                            drop(state_guard);
                            if let Err(e) = connection_timeout.cancel_order(&order_id_timeout, &symbol_timeout).await {
                                error!(
                                    error = %e,
                                    order_id = %order_id_timeout,
                                    "ORDERING: Failed to cancel timed-out order"
                                );
                            }
                        } else {
                            state_guard.order_timeouts.remove(&order_id_timeout);
                        }
                    } else {
                        state_guard.order_timeouts.remove(&order_id_timeout);
                    }
                });
                balance_reservation.release().await;
            } else {
                warn!(
                    symbol = %signal.symbol,
                    order_id = %order_id,
                    "ORDERING: Race condition detected - another thread placed order/position, canceling our order"
                );
                drop(state_guard);
                match connection.cancel_order(&order_id, &signal.symbol).await {
                    Ok(()) => {
                        info!(
                            symbol = %signal.symbol,
                            order_id = %order_id,
                            "ORDERING: Successfully canceled duplicate order after race condition"
                        );
                        {
                            let mut state_guard = shared_state.ordering_state.lock().await;
                            Self::release_reserved_margin(&mut state_guard, required_margin);
                        }
                        balance_reservation.release().await;
                    }
                    Err(cancel_err) => {
                        warn!(
                            error = %cancel_err,
                            symbol = %signal.symbol,
                            order_id = %order_id,
                            "ORDERING: Failed to cancel duplicate order after race condition, keeping balance reserved to prevent double-spend"
                        );
                    }
                }
                return Ok(());
            }
        }
        Ok(())
    }
    async fn handle_close_request(
        request: &CloseRequest,
        connection: &Arc<Connection>,
        shared_state: &Arc<SharedState>,
        _cfg: &Arc<AppCfg>,
    ) -> Result<()> {
        if let Some(position_id) = &request.position_id {
            warn!(
                symbol = %request.symbol,
                position_id = %position_id,
                reason = ?request.reason,
                "ORDERING: CloseRequest.position_id provided but not used - current implementation closes by symbol only"
            );
        }
        let has_position = {
            let state_guard = shared_state.ordering_state.lock().await;
            state_guard
                .open_position
                .as_ref()
                .map(|p| p.symbol == request.symbol)
                .unwrap_or(false)
        };
        if !has_position {
            debug!(
                symbol = %request.symbol,
                reason = ?request.reason,
                "ORDERING: Position not in state (may be already closed), calling flatten_position to verify"
            );
        }
        use crate::event_bus::CloseReason;
        let use_market_only = matches!(
            request.reason,
            CloseReason::TakeProfit | CloseReason::StopLoss
        );
        match connection
            .flatten_position(&request.symbol, use_market_only)
            .await
        {
            Ok(()) => {
                info!(
                    symbol = %request.symbol,
                    reason = ?request.reason,
                    use_market_only,
                    "ORDERING: Position closed successfully (or was already closed)"
                );
                Ok(())
            }
            Err(e) => {
                if utils::is_position_not_found_error(&e) {
                    info!(
                        symbol = %request.symbol,
                        reason = ?request.reason,
                        "ORDERING: Position already closed by another thread (race condition handled)"
                    );
                    return Ok(());
                }
                if utils::is_permanent_error(&e) {
                    warn!(
                        error = %e,
                        symbol = %request.symbol,
                        reason = ?request.reason,
                        "ORDERING: Permanent error closing position, not retrying"
                    );
                    return Err(e);
                }
                warn!(
                    error = %e,
                    symbol = %request.symbol,
                    reason = ?request.reason,
                    "ORDERING: Error closing position (flatten_position handles retries internally)"
                );
                Err(e)
            }
        }
    }
    fn release_reserved_margin(state: &mut OrderingState, amount: Decimal) {
        state.reserved_margin = state.reserved_margin.saturating_sub(amount);
    }
    fn should_update_position(
        existing_qty: Decimal,
        existing_price: Decimal,
        update_qty: Decimal,
        update_price: Decimal,
        epsilon_qty: Decimal,
        epsilon_price_pct: f64,
    ) -> bool {
        let qty_diff = (update_qty - existing_qty).abs();
        let price_diff_abs = (update_price - existing_price).abs();
        let price_diff_pct = if existing_price > Decimal::ZERO {
            utils::calculate_percentage(price_diff_abs, existing_price)
        } else {
            price_diff_abs
        };
        let price_diff_pct_f64 = utils::decimal_to_f64(price_diff_pct).max(100.0);
        qty_diff >= epsilon_qty || price_diff_pct_f64 >= epsilon_price_pct
    }
    fn is_timestamp_newer(
        update_timestamp: Instant,
        last_timestamp: Option<Instant>,
    ) -> bool {
        last_timestamp
            .map(|last_ts| update_timestamp > last_ts)
            .unwrap_or(true)
    }
    fn is_timestamp_older(
        update_timestamp: Instant,
        last_timestamp: Option<Instant>,
    ) -> bool {
        last_timestamp
            .map(|last_ts| last_ts > update_timestamp)
            .unwrap_or(false)
    }
    fn clear_order(state: &mut OrderingState) {
        state.open_order = None;
    }
    fn clear_order_with_timestamp(state: &mut OrderingState, timestamp: Instant) {
        state.open_order = None;
        state.last_order_update_timestamp = Some(timestamp);
    }
    fn clear_position(state: &mut OrderingState) {
        state.open_position = None;
    }
    fn clear_position_with_timestamp(state: &mut OrderingState, timestamp: Instant) {
        state.open_position = None;
        state.last_position_update_timestamp = Some(timestamp);
    }
    fn publish_state_and_drop(
        state_guard: tokio::sync::MutexGuard<'_, OrderingState>,
        event_bus: &Arc<EventBus>,
    ) {
        let state_to_publish = state_guard.clone();
        drop(state_guard);
        Self::publish_ordering_state_update(&state_to_publish, event_bus);
    }
    fn publish_ordering_state_update(state: &OrderingState, event_bus: &Arc<EventBus>) {
        use crate::event_bus::{OpenOrderSnapshot, OpenPositionSnapshot, OrderingStateUpdate};
        let update = OrderingStateUpdate {
            open_position: state
                .open_position
                .as_ref()
                .map(|pos| OpenPositionSnapshot {
                    symbol: pos.symbol.clone(),
                    direction: format!("{:?}", pos.direction),
                    qty: pos.qty.0.to_string(),
                    entry_price: pos.entry_price.0.to_string(),
                }),
            open_order: state.open_order.as_ref().map(|order| OpenOrderSnapshot {
                symbol: order.symbol.clone(),
                order_id: order.order_id.clone(),
                side: format!("{:?}", order.side),
                qty: order.qty.0.to_string(),
            }),
            timestamp: Instant::now(),
        };
        if let Err(e) = event_bus.ordering_state_update_tx.send(update) {
            warn!(error = ?e, "ORDERING: Failed to publish OrderingStateUpdate event (no subscribers)");
        }
    }
    async fn handle_order_update(
        update: &OrderUpdate,
        shared_state: &Arc<SharedState>,
        event_bus: &Arc<EventBus>,
        _connection: Option<&Arc<Connection>>,
    ) {
        let mut state_guard = shared_state.ordering_state.lock().await;
        let is_newer = Self::is_timestamp_newer(
            update.timestamp,
            state_guard.last_order_update_timestamp,
        );
        if !is_newer {
            tracing::debug!(
                symbol = %update.symbol,
                order_id = %update.order_id,
                update_timestamp = ?update.timestamp,
                last_timestamp = ?state_guard.last_order_update_timestamp,
                "ORDERING: Ignoring stale OrderUpdate event"
            );
            return;
        }
        if let Some(ref mut order) = state_guard.open_order {
            if order.order_id == update.order_id {
                match update.status {
                    crate::event_bus::OrderStatus::Filled => {
                        state_guard.order_timeouts.remove(&update.order_id);
                        if let Some(ref existing_pos) = state_guard.open_position {
                            if existing_pos.symbol == update.symbol {
                                let position_is_newer = Self::is_timestamp_older(
                                    update.timestamp,
                                    state_guard.last_position_update_timestamp,
                                );
                                if position_is_newer {
                                    tracing::debug!(
                                        symbol = %update.symbol,
                                        order_id = %update.order_id,
                                        order_timestamp = ?update.timestamp,
                                        position_timestamp = ?state_guard.last_position_update_timestamp,
                                        "ORDERING: Ignoring stale OrderUpdate - position is newer, but clearing order since it's filled"
                                    );
                                    Self::clear_order_with_timestamp(&mut state_guard, update.timestamp);
                                    return;
                                }
                            }
                        }
                        let direction = PositionDirection::from_order_side(update.side);
                        let (_, qty_abs) = PositionDirection::direction_and_abs_qty(update.filled_qty);
                        let should_update_position = if let Some(ref existing_pos) = state_guard.open_position {
                            if existing_pos.symbol == update.symbol {
                                let qty_abs_existing = existing_pos.qty.0;
                                let existing_entry_price = existing_pos.entry_price.0;
                                let epsilon_qty = Decimal::new(1, 6);
                                const EPSILON_PRICE_PCT: f64 = 0.001;
                                let qty_diff = (qty_abs.0 - qty_abs_existing).abs();
                                let price_diff_abs = (update.average_fill_price.0 - existing_entry_price).abs();
                                let price_diff_pct = if existing_entry_price > Decimal::ZERO {
                                    utils::calculate_percentage(price_diff_abs, existing_entry_price)
                                } else {
                                    price_diff_abs
                                };
                                let price_diff_pct_f64 = utils::decimal_to_f64(price_diff_pct).max(100.0);
                                let should_update = Self::should_update_position(
                                    qty_abs_existing,
                                    existing_entry_price,
                                    qty_abs.0,
                                    update.average_fill_price.0,
                                    epsilon_qty,
                                    EPSILON_PRICE_PCT,
                                );
                                if !should_update {
                                    tracing::debug!(
                                        symbol = %update.symbol,
                                        order_id = %update.order_id,
                                        existing_qty = %qty_abs_existing,
                                        update_qty = %qty_abs.0,
                                        existing_entry = %existing_entry_price,
                                        update_entry = %update.average_fill_price.0,
                                        qty_diff = %qty_diff,
                                        price_diff_pct = price_diff_pct_f64,
                                        "ORDERING: OrderUpdate::Filled - position qty and entry_price unchanged (within {}% threshold), skipping redundant update",
                                        EPSILON_PRICE_PCT
                                    );
                                }
                                should_update
                            } else {
                                true
                            }
                        } else {
                            true
                        };
                        if should_update_position {
                            state_guard.circuit_breaker.reset();
                        state_guard.open_position = Some(OpenPosition {
                            symbol: update.symbol.clone(),
                            direction,
                            qty: qty_abs,
                            entry_price: update.average_fill_price,
                        });
                        Self::clear_order_with_timestamp(&mut state_guard, update.timestamp);
                        Self::publish_state_and_drop(state_guard, event_bus);
                        info!(
                            symbol = %update.symbol,
                            order_id = %update.order_id,
                                qty = %qty_abs.0,
                                entry_price = %update.average_fill_price.0,
                                "ORDERING: Order filled, position opened/updated"
                            );
                        } else {
                            state_guard.circuit_breaker.reset();
                            Self::clear_order_with_timestamp(&mut state_guard, update.timestamp);
                        }
                    }
                    crate::event_bus::OrderStatus::Canceled => {
                        state_guard.order_timeouts.remove(&update.order_id);
                        Self::clear_order_with_timestamp(&mut state_guard, update.timestamp);
                        Self::publish_state_and_drop(state_guard, event_bus);
                        info!(
                            symbol = %update.symbol,
                            order_id = %update.order_id,
                            "ORDERING: Order canceled"
                        );
                    }
                    crate::event_bus::OrderStatus::Expired
                    | crate::event_bus::OrderStatus::ExpiredInMatch => {
                        state_guard.order_timeouts.remove(&update.order_id);
                        Self::clear_order_with_timestamp(&mut state_guard, update.timestamp);
                        info!(
                            symbol = %update.symbol,
                            order_id = %update.order_id,
                            status = ?update.status,
                            "ORDERING: Order expired"
                        );
                    }
                    crate::event_bus::OrderStatus::Rejected => {
                        state_guard.order_timeouts.remove(&update.order_id);
                        Self::clear_order_with_timestamp(&mut state_guard, update.timestamp);
                        let rejection_reason = update.rejection_reason.as_deref();
                        let should_open_circuit = state_guard.circuit_breaker.record_rejection(rejection_reason);
                        if should_open_circuit {
                            error!(
                                symbol = %update.symbol,
                                order_id = %update.order_id,
                                reject_count = state_guard.circuit_breaker.reject_count,
                                "ORDERING: CRITICAL - Circuit breaker OPENED after {} consecutive rejections. Order placement will be blocked for 1 minute to prevent Binance ban.",
                                state_guard.circuit_breaker.reject_count
                            );
                        } else {
                        warn!(
                            symbol = %update.symbol,
                            order_id = %update.order_id,
                                reject_count = state_guard.circuit_breaker.reject_count,
                                "ORDERING: Order rejected ({} consecutive rejections, circuit breaker will open at 5)",
                                state_guard.circuit_breaker.reject_count
                        );
                        }
                    }
                    _ => {
                        order.qty = update.remaining_qty;
                        state_guard.last_order_update_timestamp = Some(update.timestamp);
                    }
                }
            }
        }
    }
    async fn handle_position_update(
        update: &PositionUpdate,
        shared_state: &Arc<SharedState>,
        event_bus: &Arc<EventBus>,
    ) {
        let mut state_guard = shared_state.ordering_state.lock().await;
        let is_newer_position_update = Self::is_timestamp_newer(
            update.timestamp,
            state_guard.last_position_update_timestamp,
        );
        let order_update_is_newer = Self::is_timestamp_older(
            update.timestamp,
            state_guard.last_order_update_timestamp,
        );
        if !is_newer_position_update {
            tracing::debug!(
                symbol = %update.symbol,
                update_timestamp = ?update.timestamp,
                last_position_timestamp = ?state_guard.last_position_update_timestamp,
                "ORDERING: Ignoring stale PositionUpdate event"
            );
            return;
        }
        if order_update_is_newer && update.is_open {
            tracing::debug!(
                symbol = %update.symbol,
                update_timestamp = ?update.timestamp,
                last_order_timestamp = ?state_guard.last_order_update_timestamp,
                "ORDERING: Ignoring PositionUpdate (OrderUpdate is newer and position is open)"
            );
            return;
        }
        if let Some(ref existing_pos) = state_guard.open_position {
            if existing_pos.symbol == update.symbol {
                if !update.is_open {
                    Self::clear_position_with_timestamp(&mut state_guard, update.timestamp);
                    Self::publish_state_and_drop(state_guard, event_bus);
                    info!(
                        symbol = %update.symbol,
                        "ORDERING: Position closed (from PositionUpdate)"
                    );
                    return;
                }
                let qty_abs_update = update.qty.0.abs();
                let qty_abs_existing = existing_pos.qty.0;
                let existing_entry_price = existing_pos.entry_price.0;
                let epsilon_qty = Decimal::new(1, 6);
                const EPSILON_PRICE_PCT: f64 = 0.001;
                let should_update = Self::should_update_position(
                    qty_abs_existing,
                    existing_entry_price,
                    qty_abs_update,
                    update.entry_price.0,
                    epsilon_qty,
                    EPSILON_PRICE_PCT,
                );
                let should_skip = !should_update;
                let qty_diff = (qty_abs_update - qty_abs_existing).abs();
                let price_diff_abs = (update.entry_price.0 - existing_entry_price).abs();
                let price_diff_pct = if existing_entry_price > Decimal::ZERO {
                    utils::calculate_percentage(price_diff_abs, existing_entry_price)
                } else {
                    price_diff_abs
                };
                let price_diff_pct_f64 = utils::decimal_to_f64(price_diff_pct).max(100.0);
                let existing_qty_for_log = qty_abs_existing;
                let update_qty_for_log = qty_abs_update;
                let existing_entry_for_log = existing_entry_price;
                let update_entry_for_log = update.entry_price.0;
                let qty_diff_for_log = qty_diff;
                let price_diff_for_log = price_diff_abs;
                let price_diff_pct_for_log = price_diff_pct_f64;
                if should_skip {
                    state_guard.last_position_update_timestamp = Some(update.timestamp);
                    tracing::debug!(
                        symbol = %update.symbol,
                        existing_qty = %existing_qty_for_log,
                        update_qty = %update_qty_for_log,
                        qty_diff = %qty_diff_for_log,
                        existing_entry = %existing_entry_for_log,
                        update_entry = %update_entry_for_log,
                        price_diff_abs = %price_diff_for_log,
                        price_diff_pct = price_diff_pct_for_log,
                        epsilon_price_pct = EPSILON_PRICE_PCT,
                        "ORDERING: Position qty and entry_price unchanged (within {}% threshold), skipping redundant update (race condition prevention)",
                        EPSILON_PRICE_PCT
                    );
                    return;
                }
                let (direction, qty_abs) = PositionDirection::direction_and_abs_qty(update.qty);
                state_guard.open_position = Some(OpenPosition {
                    symbol: update.symbol.clone(),
                    direction,
                    qty: qty_abs,
                    entry_price: update.entry_price,
                });
                state_guard.last_position_update_timestamp = Some(update.timestamp);
                Self::publish_state_and_drop(state_guard, event_bus);
                info!(
                    symbol = %update.symbol,
                    qty_diff = %qty_diff,
                    price_diff_abs = %price_diff_for_log,
                    price_diff_pct = price_diff_pct_for_log,
                    "ORDERING: Position updated from PositionUpdate (qty or entry_price changed, timestamp verified)"
                );
                return;
            }
        }
        if update.is_open {
            let direction = PositionDirection::from_qty_sign(update.qty.0);
            let qty_abs = if update.qty.0.is_sign_negative() {
                Qty(-update.qty.0)
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
            Self::publish_state_and_drop(state_guard, event_bus);
            info!(
                symbol = %update.symbol,
                "ORDERING: Position created from PositionUpdate"
            );
        } else {
            state_guard.last_position_update_timestamp = Some(update.timestamp);
        }
    }
    async fn reconcile_positions_on_startup(
        connection: &Arc<Connection>,
        shared_state: &Arc<SharedState>,
        event_bus: &Arc<EventBus>,
    ) -> Result<()> {
        info!("ORDERING: Starting position reconciliation on startup");
        match connection.get_all_positions().await {
            Ok(exchange_positions) => {
                if exchange_positions.is_empty() {
                    info!("ORDERING: No open positions found on exchange");
                    let mut state_guard = shared_state.ordering_state.lock().await;
                    if let Some(internal_pos) = &state_guard.open_position {
                        warn!(
                            symbol = %internal_pos.symbol,
                            "ORDERING: Internal state has position but exchange doesn't - clearing orphaned position"
                        );
                        Self::clear_position(&mut state_guard);
                        Self::publish_state_and_drop(state_guard, event_bus);
                    }
                } else {
                    let position_count = exchange_positions.len();
                    info!(
                        position_count,
                        "ORDERING: Found {} open position(s) on exchange, restoring to state",
                        position_count
                    );
                    let exchange_symbols: Vec<String> = exchange_positions.iter().map(|(sym, _)| sym.clone()).collect();
                    for (symbol, exchange_pos) in exchange_positions {
                        let position_update = PositionUpdate {
                            symbol: symbol.clone(),
                            qty: exchange_pos.qty,
                            entry_price: exchange_pos.entry,
                            leverage: exchange_pos.leverage,
                            unrealized_pnl: None,
                            is_open: true,
                            liq_px: exchange_pos.liq_px,
                            timestamp: Instant::now(),
                        };
                        if let Err(e) = event_bus.position_update_tx.send(position_update) {
                            error!(
                                error = ?e,
                                symbol = %symbol,
                                "ORDERING: Failed to send PositionUpdate for startup reconciliation"
                            );
                        } else {
                            info!(
                                symbol = %symbol,
                                qty = %exchange_pos.qty.0,
                                entry_price = %exchange_pos.entry.0,
                                "ORDERING: Position restored from exchange (startup reconciliation)"
                            );
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    let mut state_guard = shared_state.ordering_state.lock().await;
                    if let Some(internal_pos) = &state_guard.open_position {
                        let exchange_has = exchange_symbols.iter().any(|sym| sym == &internal_pos.symbol);
                        if !exchange_has {
                            warn!(
                                symbol = %internal_pos.symbol,
                                "ORDERING: Internal state has position but exchange doesn't - clearing orphaned position"
                            );
                            Self::clear_position(&mut state_guard);
                            Self::publish_state_and_drop(state_guard, event_bus);
                        }
                    }
                    info!("ORDERING: Position reconciliation complete - {} position(s) restored", position_count);
                }
            }
            Err(e) => {
                warn!(
                    error = %e,
                    "ORDERING: Failed to fetch positions from exchange on startup - positions will be restored via WebSocket events"
                );
            }
        }
        Ok(())
    }
}
