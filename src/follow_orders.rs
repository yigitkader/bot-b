use crate::event_bus::FollowChannels;
use crate::types::{CloseRequest, MarketTick, PositionUpdate, Side};
use chrono::Utc;
use log::info;
use tokio::sync::broadcast;

pub async fn run_follow_orders(ch: FollowChannels, tp_percent: f64, sl_percent: f64) {
    let FollowChannels {
        mut market_rx,
        mut position_update_rx,
        close_tx,
    } = ch;

    let mut current_position: Option<PositionUpdate> = None;

    loop {
        tokio::select! {
            res = position_update_rx.recv() => match res {
                Ok(update) => {
                    if update.is_closed {
                        current_position = None;
                    } else {
                        current_position = Some(update);
                    }
                },
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            },
            res = market_rx.recv() => match res {
                Ok(tick) => {
                    if let Some(position) = current_position.as_ref() {
                        if let Some(req) = evaluate_position(position, &tick, tp_percent, sl_percent) {
                            if let Err(err) = close_tx.send(req).await {
                                info!("FOLLOW_ORDERS: failed to send close request: {err}");
                            }
                        }
                    }
                },
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    }
}

fn evaluate_position(
    position: &PositionUpdate,
    tick: &MarketTick,
    tp_percent: f64,
    sl_percent: f64,
) -> Option<CloseRequest> {
    let direction = if position.side == Side::Long {
        1.0
    } else {
        -1.0
    };
    let pnl_pct = (tick.price - position.entry_price) / position.entry_price * 100.0 * direction;

    if pnl_pct >= tp_percent || pnl_pct <= -sl_percent {
        info!(
            "FOLLOW_ORDERS: target hit {:.2}%, requesting close",
            pnl_pct
        );
        Some(CloseRequest {
            position_id: position.position_id,
            reason: format!("TP/SL {:.2}%", pnl_pct),
            ts: Utc::now(),
        })
    } else {
        None
    }
}
