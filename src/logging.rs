use crate::types::LoggingChannels;
use log::info;

pub async fn run_logging(ch: LoggingChannels) {
    let mut market_rx = ch.market_rx;
    let mut order_rx = ch.order_update_rx;
    let mut position_rx = ch.position_update_rx;
    let mut balance_rx = ch.balance_rx;
    let mut signal_rx = ch.signal_rx;

    loop {
        tokio::select! {
            res = market_rx.recv() => match crate::types::handle_broadcast_recv(res) {
                Ok(Some(tick)) => info!("LOG MarketTick {:?}", tick),
                Ok(None) => continue,
                Err(_) => break,
            },
            res = order_rx.recv() => match crate::types::handle_broadcast_recv(res) {
                Ok(Some(update)) => info!("LOG OrderUpdate {:?}", update),
                Ok(None) => continue,
                Err(_) => break,
            },
            res = position_rx.recv() => match crate::types::handle_broadcast_recv(res) {
                Ok(Some(update)) => {
                    info!("LOG PositionUpdate {:?}", update);
                    if update.is_closed {
                        info!("LOG PositionClosed: position_id={}", update.position_id);
                    }
                },
                Ok(None) => continue,
                Err(_) => break,
            },
            res = balance_rx.recv() => match crate::types::handle_broadcast_recv(res) {
                Ok(Some(snapshot)) => info!("LOG BalanceSnapshot {:?}", snapshot),
                Ok(None) => continue,
                Err(_) => break,
            },
            Some(signal) = signal_rx.recv() => {
                info!(
                    "LOG TradeSignal: {} {} @ {:.2} (size={:.2} USDT, leverage={}x, atr={:?})",
                    signal.symbol,
                    match signal.side {
                        crate::types::Side::Long => "LONG",
                        crate::types::Side::Short => "SHORT",
                    },
                    signal.entry_price,
                    signal.size_usdt,
                    signal.leverage,
                    signal.atr_value
                );
            }
        }
    }
}
