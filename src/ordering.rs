use crate::cache;
use crate::risk_manager::RiskManager;
use crate::slippage::SlippageTracker;
use crate::types::{CloseRequest, PositionMeta, Side, TradeSignal};
use crate::types::{Connection, NewOrderRequest, OrderingChannels, SharedState};
use log::{info, warn};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;

pub async fn run_ordering(
    mut ch: OrderingChannels,
    state: SharedState,
    connection: Arc<Connection>,
    risk_manager: Option<Arc<RiskManager>>,
    slippage_tracker: Option<Arc<SlippageTracker>>,
    symbol_cache: Option<Arc<cache::SymbolInfoCache>>,
    depth_cache: Option<Arc<cache::DepthCache>>,
) {
    let order_lock = Arc::new(Mutex::new(()));
    let mut order_update_rx = ch.order_update_rx;
    let mut position_update_rx = ch.position_update_rx;

    // Timeout checker for orders that were sent but no update received
    let state_for_timeout = state.clone();
    let timeout_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(5));
        loop {
            interval.tick().await;
            // Check if order was sent but no update received within 30 seconds
            if state_for_timeout.check_order_timeout(30) {
                warn!("ORDERING: order timeout detected - no OrderUpdate received within 30s, resetting state");
                state_for_timeout.set_open_order(false);
            }
        }
    });

    loop {
        tokio::select! {
            Some(signal) = ch.signal_rx.recv() => {
                let tracker = slippage_tracker.clone();
                let symbol_cache_clone = symbol_cache.clone();
                let depth_cache_clone = depth_cache.clone();
                handle_signal(
                    signal,
                    &state,
                    order_lock.clone(),
                    connection.clone(),
                    risk_manager.as_deref(),
                    tracker,
                    symbol_cache_clone,
                    depth_cache_clone,
                ).await;
            },
            Some(request) = ch.close_rx.recv() => {
                handle_close_request(request, &state, order_lock.clone(), connection.clone()).await;
            },
            res = order_update_rx.recv() => match crate::types::handle_broadcast_recv(res) {
                Ok(Some(update)) => state.apply_order_update(&update),
                Ok(None) => continue,
                Err(_) => break,
            },
            res = position_update_rx.recv() => match crate::types::handle_broadcast_recv(res) {
                Ok(Some(update)) => {
                    state.apply_position_update(&update);
                    if let Some(rm) = &risk_manager {
                        rm.update_position(&update).await;
                    }
                },
                Ok(None) => continue,
                Err(_) => break,
            }
        }
    }

    timeout_task.abort();
}

async fn handle_signal(
    signal: TradeSignal,
    state: &SharedState,
    lock: Arc<Mutex<()>>,
    connection: Arc<Connection>,
    risk_manager: Option<&RiskManager>,
    slippage_tracker: Option<Arc<SlippageTracker>>,
    symbol_cache: Option<Arc<cache::SymbolInfoCache>>,
    depth_cache: Option<Arc<cache::DepthCache>>,
) {
    if state.has_open_position() || state.has_open_order() {
        warn!(
            "ORDERING: active position/order detected, ignoring signal {}",
            signal.id
        );
        return;
    }

    // Risk controls: Balance check
    let config = connection.config();
    let quote_balance = state.get_quote_balance();

    if quote_balance < config.min_quote_balance_usd {
        warn!(
            "ORDERING: insufficient quote balance: {} < {} (min required), ignoring signal {}",
            quote_balance, config.min_quote_balance_usd, signal.id
        );
        return;
    }

    if let Some(rm) = risk_manager {
        let daily_dd = state.get_daily_drawdown_pct();
        let weekly_dd = state.get_weekly_drawdown_pct();
        let (allowed, reason) = rm.check_drawdown_limits(daily_dd, weekly_dd);
        if !allowed {
            warn!(
                "ORDERING: drawdown limit breached, ignoring signal {}: {}",
                signal.id, reason
            );
            return;
        }
    }

    let mut size_usdt = signal.size_usdt;
    if let (Some(atr), Some(rm)) = (signal.atr_value, risk_manager) {
        if atr > 0.0 {
            let equity = state.get_equity();
            // Use equity if available, otherwise fallback to balance
            let base_equity = if equity > 0.0 { equity } else { quote_balance };
            // Get ATR SL multiplier from config
            let atr_sl_multiplier = config.atr_sl_multiplier;
            let (calculated_size, _stop_distance) = rm.calculate_position_size(
                base_equity,
                atr,
                signal.entry_price,
                atr_sl_multiplier,
            );
            if calculated_size > 0.0 {
                size_usdt = calculated_size;
                info!(
                    "ORDERING: ATR-based sizing: {:.2} USDT (equity: {:.2}, ATR: {:.4}, risk_pct: {:.2}%)",
                    size_usdt,
                    base_equity,
                    atr,
                    rm.limits().risk_per_trade_pct * 100.0
                );
            }
        }
    }

    // Risk controls: Minimum margin requirement check
    // Required margin = position_size / leverage
    let required_margin = size_usdt / signal.leverage;
    if required_margin < config.min_margin_usd {
        warn!(
            "ORDERING: required margin {} < {} (min required), ignoring signal {}",
            required_margin, config.min_margin_usd, signal.id
        );
        return;
    }

    // Risk controls: Maximum position size check
    // Notional = position_size * leverage (total position value)
    let notional = size_usdt * signal.leverage;
    if notional > config.max_position_notional_usd {
        warn!(
            "ORDERING: position notional {} > {} (max allowed), ignoring signal {}",
            notional, config.max_position_notional_usd, signal.id
        );
        return;
    }

    if let Some(tracker) = slippage_tracker {
        let estimated_bps = tracker.estimate_for_order(signal.side, notional).await;
        if estimated_bps > config.max_slippage_bps {
            warn!(
                "ORDERING: estimated slippage {:.2} bps exceeds max {:.2} bps, ignoring signal {}",
                estimated_bps, config.max_slippage_bps, signal.id
            );
            return;
        }
    }

    // Risk controls: Balance sufficiency check
    if quote_balance < required_margin {
        warn!(
            "ORDERING: insufficient balance for margin: {} < {} (required), ignoring signal {}",
            quote_balance, required_margin, signal.id
        );
        return;
    }

    let _guard = lock.lock().await;
    state.set_open_order(true);

    let start_time = Instant::now();

    // ✅ CRITICAL: Use cached symbol info (TrendPlan.md - Fast Order Execution)
    let symbol_info = if let Some(cache) = &symbol_cache {
        match cache.get(&signal.symbol).await {
            Some(cached) => {
                // Convert cached info to SymbolPrecision
                crate::types::SymbolPrecision {
                    step_size: cached.step_size,
                    min_quantity: cached.min_quantity,
                    max_quantity: cached.max_quantity,
                    tick_size: cached.tick_size,
                    min_price: 0.0,
                    max_price: f64::MAX,
                }
            }
            None => {
                warn!(
                    "ORDERING: symbol {} not in cache, fetching from API (cache miss)",
                    signal.symbol
                );
                match connection.fetch_symbol_info(&signal.symbol).await {
                    Ok(info) => info,
                    Err(err) => {
                        warn!(
                            "ORDERING: failed to fetch symbol info for {}: {err:?}, ignoring signal {}",
                            signal.symbol, signal.id
                        );
                        state.set_open_order(false);
                        return;
                    }
                }
            }
        }
    } else {
        // Fallback: fetch from API if cache not available
        match connection.fetch_symbol_info(&signal.symbol).await {
            Ok(info) => info,
            Err(err) => {
                warn!(
                    "ORDERING: failed to fetch symbol info for {}: {err:?}, ignoring signal {}",
                    signal.symbol, signal.id
                );
                state.set_open_order(false);
                return;
            }
        }
    };

    // ✅ CRITICAL: Use cached depth data (TrendPlan.md - Fast Order Execution)
    let (best_bid, best_ask) = if let Some(cache) = &depth_cache {
        match cache.get_best_prices(&signal.symbol).await {
            Some((bid, ask)) => (bid, ask),
            None => {
                warn!(
                    "ORDERING: depth cache miss for {}, using calculate_order_price",
                    signal.symbol
                );
                // Fallback to calculate_order_price
                match connection
                    .calculate_order_price(&signal.symbol, &signal.side)
                    .await
                {
                    Ok(price) => {
                        // Use price as both bid and ask (approximation)
                        (price * 0.9999, price * 1.0001)
                    }
                    Err(err) => {
                        warn!(
                            "ORDERING: failed to calculate order price for {}: {err:?}, using signal.entry_price",
                            signal.symbol
                        );
                        (signal.entry_price, signal.entry_price)
                    }
                }
            }
        }
    } else {
        // Fallback: use calculate_order_price
        match connection
            .calculate_order_price(&signal.symbol, &signal.side)
            .await
        {
            Ok(price) => {
                // Use price as both bid and ask (approximation)
                (price * 0.9999, price * 1.0001)
            }
            Err(err) => {
                warn!(
                    "ORDERING: failed to calculate order price for {}: {err:?}, using signal.entry_price",
                    signal.symbol
                );
                (signal.entry_price, signal.entry_price)
            }
        }
    };

    // Calculate order price from cached depth
    let order_price = match signal.side {
        Side::Long => best_ask,
        Side::Short => best_bid,
    };

    // Calculate quantity with leverage included using actual order price
    // Notional = position_size * leverage (total position value)
    let notional = size_usdt * signal.leverage;
    let qty = (notional / order_price).abs();

    if let Some(rm) = risk_manager {
        let (allowed, reason) = rm
            .can_open_position(
                &signal.symbol,
                signal.side,
                order_price, // Gerçek order price kullan
                qty,
                signal.leverage,
            )
            .await;

        if !allowed {
            warn!(
                "ORDERING: risk manager blocked signal {}: {}",
                signal.id, reason
            );
            state.set_open_order(false);
            return;
        }
    }

    if qty <= 0.0 {
        warn!(
            "ORDERING: invalid quantity computed from signal {} (size={}, leverage={}, order_price={})",
            signal.id, signal.size_usdt, signal.leverage, order_price
        );
        state.set_open_order(false);
        return;
    }

    // Round quantity to step size
    let mut qty = Connection::round_to_step_size(qty, symbol_info.step_size);

    // Final check: verify rounded quantity with actual order price doesn't exceed notional limits
    let final_notional = qty * order_price;
    if final_notional > config.max_position_notional_usd {
        warn!(
            "ORDERING: final notional {} exceeds max {} after rounding (qty={}, price={}), reducing quantity",
            final_notional, config.max_position_notional_usd, qty, order_price
        );
        // Reduce quantity to stay within notional limit
        let max_qty = config.max_position_notional_usd / order_price;
        qty = Connection::round_to_step_size(max_qty, symbol_info.step_size);

        if qty < symbol_info.min_quantity {
            warn!(
                "ORDERING: reduced quantity {} below minimum {} for symbol {}, ignoring signal {}",
                qty, symbol_info.min_quantity, signal.symbol, signal.id
            );
            state.set_open_order(false);
            return;
        }
    }

    // Check minimum quantity after rounding
    if qty < symbol_info.min_quantity {
        warn!(
            "ORDERING: quantity {} too small after rounding (min: {}, step_size: {}) for symbol {}, ignoring signal {}",
            qty, symbol_info.min_quantity, symbol_info.step_size, signal.symbol, signal.id
        );
        state.set_open_order(false);
        return;
    }

    // Check maximum quantity
    if qty > symbol_info.max_quantity {
        warn!(
            "ORDERING: quantity {} exceeds maximum {} for symbol {}, ignoring signal {}",
            qty, symbol_info.max_quantity, signal.symbol, signal.id
        );
        state.set_open_order(false);
        state.clear_pending_position_meta();
        return;
    }

    state.set_pending_position_meta(PositionMeta {
        atr_at_entry: signal.atr_value,
    });

    let order = NewOrderRequest {
        symbol: signal.symbol.clone(),
        side: signal.side,
        quantity: qty,
        reduce_only: false,
        client_order_id: Some(signal.id.to_string()),
    };

    // CRITICAL: Mark order as sent BEFORE API call to prevent race condition
    // If OrderUpdate arrives very quickly (WebSocket), order_sent_at must already be set
    // Otherwise timeout check will fail incorrectly
    state.mark_order_sent();

    // ✅ CRITICAL: Use fast order execution with cached data (TrendPlan.md)
    let order_result = if symbol_cache.is_some() && depth_cache.is_some() {
        // Fast path: use cached data
        connection
            .send_order_fast(&order, &symbol_info, best_bid, best_ask)
            .await
    } else {
        // Fallback: use regular order execution
        connection.send_order(order.clone()).await
    };

    let elapsed_ms = start_time.elapsed().as_millis() as f64;

    if let Err(err) = order_result {
        warn!(
            "ORDERING: send_order failed: {err:?} (elapsed: {:.2}ms)",
            elapsed_ms
        );
        // Order failed - reset state immediately
        state.set_open_order(false);
        // Clear timestamp since order was not actually sent
        state.clear_order_sent();
        state.clear_pending_position_meta();
    } else {
        info!(
            "ORDERING: ⚡ FAST ORDER submitted {} in {:.2}ms (target: <50ms)",
            signal.id, elapsed_ms
        );
        // Order successfully sent - state.has_open_order=true will remain until OrderUpdate event arrives
        // apply_order_update() will update the state based on order status:
        // - If order is Filled/Canceled/Rejected -> has_open_order=false
        // - If order is New/PartiallyFilled -> has_open_order=true
        // This ensures state synchronization with actual order status from Binance
        // order_sent_at is already set above, so timeout tracking will work correctly
    }
}

async fn handle_close_request(
    request: CloseRequest,
    state: &SharedState,
    lock: Arc<Mutex<()>>,
    connection: Arc<Connection>,
) {
    let _guard = lock.lock().await;

    // Get current position from state (for position_id verification)
    let state_position = match state.current_position() {
        Some(pos) => pos,
        None => {
            warn!(
                "ORDERING: no position found in state for close request {}",
                request.position_id
            );
            return;
        }
    };

    // Verify position ID matches
    if state_position.position_id != request.position_id {
        warn!(
            "ORDERING: position ID mismatch: expected {}, got {}",
            request.position_id, state_position.position_id
        );
        return;
    }

    // Always fetch fresh position from API to get accurate leverage information
    // WebSocket ACCOUNT_UPDATE events don't include leverage, so we need API data
    // for correct PnL calculations (follow_orders.rs uses leverage)
    let fresh_position = match connection.fetch_position(&state_position.symbol).await {
        Ok(Some(pos)) => pos,
        Ok(None) => {
            warn!(
                "ORDERING: position {} already closed (fetched from API)",
                request.position_id
            );
            return;
        }
        Err(err) => {
            warn!(
                "ORDERING: failed to fetch position from API for {}: {err:?}, cannot close position",
                request.position_id
            );
            return;
        }
    };

    // Check if position is already closed
    if fresh_position.size <= 0.0 {
        warn!(
            "ORDERING: position {} already closed (size: {})",
            request.position_id, fresh_position.size
        );
        return;
    }

    // Fetch symbol info for quantity precision
    let symbol_info = match connection.fetch_symbol_info(&fresh_position.symbol).await {
        Ok(info) => info,
        Err(err) => {
            warn!(
                "ORDERING: failed to fetch symbol info for {}: {err:?}, cannot close position {}",
                fresh_position.symbol, request.position_id
            );
            return;
        }
    };

    // Use fresh position size from API (ensures we have the latest size)
    let mut qty = fresh_position.size;

    // Round quantity to step size
    qty = Connection::round_to_step_size(qty, symbol_info.step_size);

    // Validate rounded quantity
    if qty < symbol_info.min_quantity {
        warn!(
            "ORDERING: rounded quantity {} below minimum {} for symbol {}, cannot close position {}",
            qty, symbol_info.min_quantity, fresh_position.symbol, request.position_id
        );
        return;
    }

    if qty > symbol_info.max_quantity {
        warn!(
            "ORDERING: rounded quantity {} exceeds maximum {} for symbol {}, cannot close position {}",
            qty, symbol_info.max_quantity, fresh_position.symbol, request.position_id
        );
        return;
    }

    // Determine opposite side to close position
    let side = if fresh_position.side == Side::Long {
        Side::Short // Long pozisyonu kapatmak için SELL
    } else {
        Side::Long // Short pozisyonu kapatmak için BUY
    };

    let order = NewOrderRequest {
        symbol: fresh_position.symbol.clone(),
        side,
        quantity: qty,
        reduce_only: true,
        client_order_id: Some(format!("close-{}", request.position_id)),
    };

    if let Err(err) = connection.send_order(order).await {
        warn!(
            "ORDERING: close send_order failed for position {}: {err:?}",
            request.position_id
        );
    } else {
        info!(
            "ORDERING: close order submitted for position {} (size: {}, side: {:?})",
            request.position_id, qty, side
        );
    }
}
