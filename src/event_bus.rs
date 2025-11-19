use crate::types::{
    BalanceSnapshot, CloseRequest, MarketTick, OrderUpdate, PositionUpdate, TradeSignal,
};
use std::sync::Mutex;
use tokio::sync::{broadcast, mpsc};

pub struct EventBus {
    market_tx: broadcast::Sender<MarketTick>,
    order_update_tx: broadcast::Sender<OrderUpdate>,
    position_update_tx: broadcast::Sender<PositionUpdate>,
    balance_tx: broadcast::Sender<BalanceSnapshot>,
    signal_tx: mpsc::Sender<TradeSignal>,
    signal_rx: Mutex<Option<mpsc::Receiver<TradeSignal>>>,
    close_tx: mpsc::Sender<CloseRequest>,
    close_rx: Mutex<Option<mpsc::Receiver<CloseRequest>>>,
}

impl EventBus {
    pub fn new(buffer: usize) -> Self {
        let (market_tx, _) = broadcast::channel(buffer);
        let (order_update_tx, _) = broadcast::channel(buffer);
        let (position_update_tx, _) = broadcast::channel(buffer);
        let (balance_tx, _) = broadcast::channel(buffer);
        let (signal_tx, signal_rx) = mpsc::channel(buffer);
        let (close_tx, close_rx) = mpsc::channel(buffer);

        Self {
            market_tx,
            order_update_tx,
            position_update_tx,
            balance_tx,
            signal_tx,
            signal_rx: Mutex::new(Some(signal_rx)),
            close_tx,
            close_rx: Mutex::new(Some(close_rx)),
        }
    }

    pub fn trending_channels(&self) -> TrendingChannels {
        TrendingChannels {
            market_rx: self.market_tx.subscribe(),
            signal_tx: self.signal_tx.clone(),
        }
    }

    pub fn ordering_channels(&self) -> OrderingChannels {
        OrderingChannels {
            signal_rx: Self::take_receiver(&self.signal_rx, "TradeSignal"),
            close_rx: Self::take_receiver(&self.close_rx, "CloseRequest"),
            order_update_rx: self.order_update_tx.subscribe(),
            position_update_rx: self.position_update_tx.subscribe(),
        }
    }

    pub fn follow_channels(&self) -> FollowChannels {
        FollowChannels {
            market_rx: self.market_tx.subscribe(),
            position_update_rx: self.position_update_tx.subscribe(),
            close_tx: self.close_tx.clone(),
        }
    }

    pub fn balance_channels(&self) -> BalanceChannels {
        BalanceChannels {
            balance_tx: self.balance_tx.clone(),
        }
    }

    pub fn logging_channels(&self) -> LoggingChannels {
        LoggingChannels {
            market_rx: self.market_tx.subscribe(),
            order_update_rx: self.order_update_tx.subscribe(),
            position_update_rx: self.position_update_tx.subscribe(),
            balance_rx: self.balance_tx.subscribe(),
        }
    }

    pub fn connection_channels(&self) -> ConnectionChannels {
        ConnectionChannels {
            market_tx: self.market_tx.clone(),
            order_update_tx: self.order_update_tx.clone(),
            position_update_tx: self.position_update_tx.clone(),
            balance_tx: self.balance_tx.clone(),
        }
    }

    fn take_receiver<T>(slot: &Mutex<Option<mpsc::Receiver<T>>>, name: &str) -> mpsc::Receiver<T> {
        slot.lock()
            .expect("receiver mutex poisoned")
            .take()
            .unwrap_or_else(|| panic!("{name} receiver already taken"))
    }
}

use tokio::sync::broadcast::{Receiver as BReceiver, Sender as BSender};
use tokio::sync::mpsc::{Receiver as MReceiver, Sender as MSender};

pub struct TrendingChannels {
    pub market_rx: BReceiver<MarketTick>,
    pub signal_tx: MSender<TradeSignal>,
}

pub struct OrderingChannels {
    pub signal_rx: MReceiver<TradeSignal>,
    pub close_rx: MReceiver<CloseRequest>,
    pub order_update_rx: BReceiver<OrderUpdate>,
    pub position_update_rx: BReceiver<PositionUpdate>,
}

pub struct FollowChannels {
    pub market_rx: BReceiver<MarketTick>,
    pub position_update_rx: BReceiver<PositionUpdate>,
    pub close_tx: MSender<CloseRequest>,
}

#[derive(Clone)]
pub struct BalanceChannels {
    pub balance_tx: BSender<BalanceSnapshot>,
}

pub struct LoggingChannels {
    pub market_rx: BReceiver<MarketTick>,
    pub order_update_rx: BReceiver<OrderUpdate>,
    pub position_update_rx: BReceiver<PositionUpdate>,
    pub balance_rx: BReceiver<BalanceSnapshot>,
}

#[derive(Clone)]
pub struct ConnectionChannels {
    pub market_tx: BSender<MarketTick>,
    pub order_update_tx: BSender<OrderUpdate>,
    pub position_update_tx: BSender<PositionUpdate>,
    pub balance_tx: BSender<BalanceSnapshot>,
}
