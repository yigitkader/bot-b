use crate::connection::{Connection, NewOrderRequest};
use crate::event_bus::OrderingChannels;
use crate::state::SharedState;
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

    let _guard = lock.lock().await;
    state.set_open_order(true);
    let qty = (signal.size_usdt / signal.entry_price).abs();
    if qty <= 0.0 {
        warn!(
            "ORDERING: invalid quantity computed from signal {} (size={}, price={})",
            signal.id, signal.size_usdt, signal.entry_price
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
        state.set_open_order(false);
    } else {
        info!("ORDERING: open order submitted {}", signal.id);
    }
}

async fn handle_close_request(
    request: CloseRequest,
    state: &SharedState,
    lock: Arc<Mutex<()>>,
    connection: Arc<Connection>,
) {
    let _guard = lock.lock().await;

    let position = match state
        .current_position()
        .filter(|pos| pos.position_id == request.position_id && pos.size > 0.0)
    {
        Some(pos) => pos,
        None => {
            warn!(
                "ORDERING: no active position found for close request {}",
                request.position_id
            );
            return;
        }
    };

    let side = if position.side == Side::Long {
        Side::Short
    } else {
        Side::Long
    };

    let order = NewOrderRequest {
        symbol: position.symbol.clone(),
        side,
        quantity: position.size,
        reduce_only: true,
        client_order_id: Some(format!("close-{}", request.position_id)),
    };

    if let Err(err) = connection.send_order(order).await {
        warn!("ORDERING: close send_order failed: {err:?}");
    } else {
        info!("ORDERING: close order submitted {}", request.position_id);
    }
}
