use crate::types::LoggingChannels;
use crate::types::TradeSignal;
use log::info;
use tokio::sync::{broadcast, mpsc};

pub async fn run_logging(mut ch: LoggingChannels) {
    let mut market_rx = ch.market_rx;
    let mut order_rx = ch.order_update_rx;
    let mut position_rx = ch.position_update_rx;
    let mut balance_rx = ch.balance_rx;
    let mut signal_rx = ch.signal_rx;

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
                Ok(update) => {
                    info!("LOG PositionUpdate {:?}", update);
                    // PnL loglama: PositionUpdate'te realized PnL varsa logla
                    if update.is_closed {
                        info!("LOG PositionClosed: position_id={}", update.position_id);
                    }
                },
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            },
            res = balance_rx.recv() => match res {
                Ok(snapshot) => info!("LOG BalanceSnapshot {:?}", snapshot),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            },
            Some(signal) = signal_rx.recv() => {
                info!("LOG TradeSignal: {} {} @ {:.2} (size={:.2} USDT, leverage={}x)", 
                    signal.symbol,
                    match signal.side {
                        crate::types::Side::Long => "LONG",
                        crate::types::Side::Short => "SHORT",
                    },
                    signal.entry_price,
                    signal.size_usdt,
                    signal.leverage
                );
            }
        }
    }
}
