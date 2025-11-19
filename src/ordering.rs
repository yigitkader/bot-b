use crate::types::{Connection, NewOrderRequest, OrderingChannels, SharedState};
use crate::types::{CloseRequest, Side, TradeSignal};
use log::{info, warn};
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};

pub async fn run_ordering(
    mut ch: OrderingChannels,
    state: SharedState,
    connection: Arc<Connection>,
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
                handle_signal(signal, &state, order_lock.clone(), connection.clone()).await;
            },
            Some(request) = ch.close_rx.recv() => {
                handle_close_request(request, &state, order_lock.clone(), connection.clone()).await;
            },
            res = order_update_rx.recv() => match res {
                Ok(update) => state.apply_order_update(&update),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            },
            res = position_update_rx.recv() => match res {
                Ok(update) => state.apply_position_update(&update),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
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

    // Risk controls: Minimum margin requirement check
    // Required margin = position_size / leverage
    let required_margin = signal.size_usdt / signal.leverage;
    if required_margin < config.min_margin_usd {
        warn!(
            "ORDERING: required margin {} < {} (min required), ignoring signal {}",
            required_margin, config.min_margin_usd, signal.id
        );
        return;
    }

    // Risk controls: Maximum position size check
    // Notional = position_size * leverage (total position value)
    let notional = signal.size_usdt * signal.leverage;
    if notional > config.max_position_notional_usd {
        warn!(
            "ORDERING: position notional {} > {} (max allowed), ignoring signal {}",
            notional, config.max_position_notional_usd, signal.id
        );
        return;
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

    // Fetch symbol precision info (stepSize, minQuantity, etc.)
    let symbol_info = match connection.fetch_symbol_info(&signal.symbol).await {
        Ok(info) => info,
        Err(err) => {
            warn!(
                "ORDERING: failed to fetch symbol info for {}: {err:?}, ignoring signal {}",
                signal.symbol, signal.id
            );
            state.set_open_order(false);
            return;
        }
    };

    // Calculate quantity with leverage included
    // Notional = position_size * leverage (total position value)
    let notional = signal.size_usdt * signal.leverage;
    let qty = (notional / signal.entry_price).abs();
    
    if qty <= 0.0 {
        warn!(
            "ORDERING: invalid quantity computed from signal {} (size={}, leverage={}, price={})",
            signal.id, signal.size_usdt, signal.leverage, signal.entry_price
        );
        state.set_open_order(false);
        return;
    }

    // Round quantity to step size
    let qty = Connection::round_to_step_size(qty, symbol_info.step_size);

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
        return;
    }

    let order = NewOrderRequest {
        symbol: signal.symbol.clone(),
        side: signal.side,
        quantity: qty,
        reduce_only: false,
        client_order_id: Some(signal.id.to_string()),
    };

    if let Err(err) = connection.send_order(order).await {
        warn!("ORDERING: send_order failed: {err:?}");
        // Order failed - reset state immediately
        state.set_open_order(false);
    } else {
        info!("ORDERING: open order submitted {}", signal.id);
        // Mark that order was sent (for timeout tracking)
        state.mark_order_sent();
        // Order successfully sent - state.has_open_order=true will remain until OrderUpdate event arrives
        // apply_order_update() will update the state based on order status:
        // - If order is Filled/Canceled/Rejected -> has_open_order=false
        // - If order is New/PartiallyFilled -> has_open_order=true
        // This ensures state synchronization with actual order status from Binance
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

    // Re-fetch position from API to ensure we have the latest size
    // WebSocket events may be delayed, so we fetch fresh data to avoid
    // "Order would increase position" errors
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
                "ORDERING: failed to fetch position from API for {}: {err:?}, using state position",
                request.position_id
            );
            // Fallback to state position if API fetch fails
            state_position
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
        warn!("ORDERING: close send_order failed for position {}: {err:?}", request.position_id);
    } else {
        info!(
            "ORDERING: close order submitted for position {} (size: {}, side: {:?})",
            request.position_id, qty, side
        );
    }
}
