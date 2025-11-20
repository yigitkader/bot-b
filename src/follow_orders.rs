use crate::types::{CloseRequest, MarketTick, PositionUpdate, Side};
use crate::types::{FollowChannels, PositionMeta, SharedState};
use chrono::Utc;
use log::{info, warn};
use tokio::sync::broadcast;

pub async fn run_follow_orders(
    ch: FollowChannels,
    state: SharedState,
    tp_percent: f64,
    sl_percent: f64,
    commission_pct: f64,
    atr_sl_multiplier: f64,
    atr_tp_multiplier: f64,
) {
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
                    // Update equity with unrealized PnL
                    state.update_equity(update.unrealized_pnl);
                    
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
                        let position_meta = state.current_position_meta();
                        if let Some(req) = evaluate_position(
                            position,
                            &tick,
                            position_meta,
                            tp_percent,
                            sl_percent,
                            commission_pct,
                            atr_sl_multiplier,
                            atr_tp_multiplier,
                        ) {
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
    position_meta: Option<PositionMeta>,
    tp_percent: f64,
    sl_percent: f64,
    commission_pct: f64,
    atr_sl_multiplier: f64,
    atr_tp_multiplier: f64,
) -> Option<CloseRequest> {
    // CRITICAL: Validate position size first
    // Position size <= 0 means position is already closed or invalid (data corruption)
    // TP/SL calculation should not proceed with invalid position size
    if position.size <= f64::EPSILON {
        warn!(
            "FOLLOW_ORDERS: invalid position size {} for position {} (position already closed or corrupted)",
            position.size, position.position_id
        );
        return None;
    }

    // Validate entry price
    if position.entry_price <= 0.0 {
        warn!(
            "FOLLOW_ORDERS: invalid entry price {} for position {}",
            position.entry_price, position.position_id
        );
        return None;
    }

    // Validate leverage
    if position.leverage <= 0.0 {
        warn!(
            "FOLLOW_ORDERS: invalid leverage {} for position {}",
            position.leverage, position.position_id
        );
        return None;
    }

    if let Some(meta) = position_meta {
        if let Some(atr_value) = meta.atr_at_entry {
            if atr_value > 0.0 {
                let (sl_hit, tp_hit, sl_price, tp_price) = match position.side {
                    Side::Long => {
                        let sl_price = position.entry_price - atr_sl_multiplier * atr_value;
                        let tp_price = position.entry_price + atr_tp_multiplier * atr_value;
                        (
                            tick.price <= sl_price,
                            tick.price >= tp_price,
                            sl_price,
                            tp_price,
                        )
                    }
                    Side::Short => {
                        let sl_price = position.entry_price + atr_sl_multiplier * atr_value;
                        let tp_price = position.entry_price - atr_tp_multiplier * atr_value;
                        (
                            tick.price >= sl_price,
                            tick.price <= tp_price,
                            sl_price,
                            tp_price,
                        )
                    }
                };

                if sl_hit {
                    info!(
                        "FOLLOW_ORDERS: ATR stop hit at {:.4} (entry {:.4}, atr {:.4})",
                        sl_price, position.entry_price, atr_value
                    );
                    return Some(CloseRequest {
                        position_id: position.position_id,
                        reason: format!(
                            "ATR SL hit ({:.4} vs entry {:.4})",
                            sl_price, position.entry_price
                        ),
                        ts: Utc::now(),
                    });
                } else if tp_hit {
                    info!(
                        "FOLLOW_ORDERS: ATR take-profit hit at {:.4} (entry {:.4}, atr {:.4})",
                        tp_price, position.entry_price, atr_value
                    );
                    return Some(CloseRequest {
                        position_id: position.position_id,
                        reason: format!(
                            "ATR TP hit ({:.4} vs entry {:.4})",
                            tp_price, position.entry_price
                        ),
                        ts: Utc::now(),
                    });
                }
            }
        }
    }

    // Calculate direction multiplier
    let direction = if position.side == Side::Long {
        1.0
    } else {
        -1.0
    };

    // Calculate price change percentage (unleveraged)
    // For Long: positive if price goes up, negative if price goes down
    // For Short: positive if price goes down, negative if price goes up
    let price_change_pct = (tick.price - position.entry_price) / position.entry_price * direction;

    // Calculate ROI (Return on Investment) = PnL / Margin
    // ROI = price_change_pct * leverage
    // Example: Entry $40,000, Current $40,400, Leverage 100x
    // price_change_pct = (40400 - 40000) / 40000 = 0.01 (1%)
    // ROI = 0.01 * 100 = 1.0 (100%)
    let roi_pct = price_change_pct * position.leverage * 100.0;

    // Subtract commission (round-trip: entry + exit)
    // Commission is already in percentage (e.g., 0.08 for 0.08%)
    let net_roi_pct = roi_pct - commission_pct;

    // Check TP/SL thresholds
    if net_roi_pct >= tp_percent || net_roi_pct <= -sl_percent {
        info!(
            "FOLLOW_ORDERS: target hit (price_change: {:.4}%, leverage: {:.1}x, ROI: {:.2}%, net ROI: {:.2}%), requesting close",
            price_change_pct * 100.0, position.leverage, roi_pct, net_roi_pct
        );
        Some(CloseRequest {
            position_id: position.position_id,
            reason: format!(
                "TP/SL net ROI: {:.2}% (ROI: {:.2}%, leverage: {:.1}x)",
                net_roi_pct, roi_pct, position.leverage
            ),
            ts: Utc::now(),
        })
    } else {
        None
    }
}
