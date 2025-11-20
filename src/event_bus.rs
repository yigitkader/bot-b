use crate::types::{
    BalanceChannels, ConnectionChannels, EventBus, FollowChannels,
    LoggingChannels, MarketTick, OrderingChannels, RotationChannels,
    SymbolRotationEvent, TrendingChannels,
};
use std::sync::Mutex;
use tokio::sync::broadcast;
use tokio::sync::mpsc;

use tokio::sync::broadcast::Receiver as BReceiver;

impl EventBus {
    /// Create a new EventBus with optimized buffer sizes for each channel
    /// Buffer sizes are optimized based on message frequency and memory usage
    pub fn new(
        market_tick_buffer: usize,
        trade_signal_buffer: usize,
        close_request_buffer: usize,
        order_update_buffer: usize,
        position_update_buffer: usize,
        balance_update_buffer: usize,
    ) -> Self {
        let (market_tx, _) = broadcast::channel(market_tick_buffer);
        let (order_update_tx, _) = broadcast::channel(order_update_buffer);
        let (position_update_tx, _) = broadcast::channel(position_update_buffer);
        let (balance_tx, _) = broadcast::channel(balance_update_buffer);
        let (signal_tx, signal_rx) = mpsc::channel(trade_signal_buffer);
        let (close_tx, close_rx) = mpsc::channel(close_request_buffer);
        let (rotation_tx, _) = broadcast::channel(10); // Small buffer for rotation events

        Self {
            market_tx,
            order_update_tx,
            position_update_tx,
            balance_tx,
            signal_tx,
            signal_rx: Mutex::new(Some(signal_rx)),
            close_tx,
            close_rx: Mutex::new(Some(close_rx)),
            rotation_tx,
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
            signal_rx: Self::take_receiver(&self.signal_rx, "TradeSignal"),
        }
    }

    pub fn market_receiver(&self) -> BReceiver<MarketTick> {
        self.market_tx.subscribe()
    }

    pub fn connection_channels(&self) -> ConnectionChannels {
        ConnectionChannels {
            market_tx: self.market_tx.clone(),
            order_update_tx: self.order_update_tx.clone(),
            position_update_tx: self.position_update_tx.clone(),
            balance_tx: self.balance_tx.clone(),
        }
    }

    pub fn rotation_channels(&self) -> RotationChannels {
        RotationChannels {
            rotation_rx: self.rotation_tx.subscribe(),
        }
    }

    pub fn rotation_sender(&self) -> broadcast::Sender<SymbolRotationEvent> {
        self.rotation_tx.clone()
    }

    fn take_receiver<T>(slot: &Mutex<Option<mpsc::Receiver<T>>>, name: &str) -> mpsc::Receiver<T> {
        slot.lock()
            .expect("receiver mutex poisoned")
            .take()
            .unwrap_or_else(|| panic!("{name} receiver already taken"))
    }
}
