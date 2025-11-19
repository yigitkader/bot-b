use crate::types::LoggingChannels;
use log::info;
use tokio::sync::broadcast;

pub async fn run_logging(ch: LoggingChannels) {
    let mut market_rx = ch.market_rx;
    let mut order_rx = ch.order_update_rx;
    let mut position_rx = ch.position_update_rx;
    let mut balance_rx = ch.balance_rx;

    loop {
        tokio::select! {
            res = market_rx.recv() => match res {
                Ok(tick) => info!("LOG MarketTick {:?}", tick),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            },
            res = order_rx.recv() => match res {
                Ok(update) => info!("LOG OrderUpdate {:?}", update),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            },
            res = position_rx.recv() => match res {
                Ok(update) => info!("LOG PositionUpdate {:?}", update),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            },
            res = balance_rx.recv() => match res {
                Ok(snapshot) => info!("LOG BalanceSnapshot {:?}", snapshot),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    }
}
