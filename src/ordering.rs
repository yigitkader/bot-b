use crate::connection::Connection;
use crate::event_bus::OrderingChannels;
use crate::state::SharedState;
use crate::types::{CloseRequest, TradeSignal};
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
                handle_close_request(request, order_lock.clone(), connection.clone()).await;
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
    let quote_asset = connection.quote_asset();
    let description = format!(
        "OPEN {:?} {} size {:.2} {}",
        signal.side, signal.symbol, signal.size_usdt, quote_asset
    );
    if let Err(err) = connection.send_order(description).await {
        warn!("ORDERING: send_order failed: {err:?}");
        state.set_open_order(false);
    } else {
        info!("ORDERING: open order submitted {}", signal.id);
    }
}

async fn handle_close_request(
    request: CloseRequest,
    lock: Arc<Mutex<()>>,
    connection: Arc<Connection>,
) {
    let _guard = lock.lock().await;
    let description = format!(
        "CLOSE position {} reason {}",
        request.position_id, request.reason
    );
    if let Err(err) = connection.send_order(description).await {
        warn!("ORDERING: close send_order failed: {err:?}");
    } else {
        info!("ORDERING: close order submitted {}", request.position_id);
    }
}
