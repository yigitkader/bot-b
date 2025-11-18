
pub use crate::types::{
    BalanceUpdate, CloseReason, CloseRequest, FillHistoryAction, FillHistoryData,
    MarketTick, OpenOrderSnapshot, OpenPositionSnapshot,
    OrderFillHistoryUpdate, OrderingStateUpdate, OrderStatus, OrderUpdate,
    PositionUpdate, TradeSignal,
};
use tokio::sync::broadcast;
use std::cmp;
#[derive(Clone)]
pub struct EventBus {
    pub market_tick_tx: broadcast::Sender<MarketTick>,
    pub trade_signal_tx: broadcast::Sender<TradeSignal>,
    pub close_request_tx: broadcast::Sender<CloseRequest>,
    pub order_update_tx: broadcast::Sender<OrderUpdate>,
    pub position_update_tx: broadcast::Sender<PositionUpdate>,
    pub balance_update_tx: broadcast::Sender<BalanceUpdate>,
    pub ordering_state_update_tx: broadcast::Sender<OrderingStateUpdate>,
    pub order_fill_history_update_tx: broadcast::Sender<OrderFillHistoryUpdate>,
}
impl EventBus {
    pub fn new_with_config(cfg: &crate::config::EventBusCfg) -> Self {
        let market_tick_buffer = cmp::max(cfg.market_tick_buffer, 1);
        let trade_signal_buffer = cmp::max(cfg.trade_signal_buffer, 1);
        let close_request_buffer = cmp::max(cfg.close_request_buffer, 1);
        let order_update_buffer = cmp::max(cfg.order_update_buffer, 1);
        let position_update_buffer = cmp::max(cfg.position_update_buffer, 1);
        let balance_update_buffer = cmp::max(cfg.balance_update_buffer, 1);
        let (market_tick_tx, _) = broadcast::channel(market_tick_buffer);
        let (trade_signal_tx, _) = broadcast::channel(trade_signal_buffer);
        let (close_request_tx, _) = broadcast::channel(close_request_buffer);
        let (order_update_tx, _) = broadcast::channel(order_update_buffer);
        let (position_update_tx, _) = broadcast::channel(position_update_buffer);
        let (balance_update_tx, _) = broadcast::channel(balance_update_buffer);
        let (ordering_state_update_tx, _) = broadcast::channel(1000);
        let (order_fill_history_update_tx, _) = broadcast::channel(1000);
        Self {
            market_tick_tx,
            trade_signal_tx,
            close_request_tx,
            order_update_tx,
            position_update_tx,
            balance_update_tx,
            ordering_state_update_tx,
            order_fill_history_update_tx,
        }
    }
    pub fn new() -> Self {
        let default_cfg = crate::config::EventBusCfg::default();
        Self::new_with_config(&default_cfg)
    }
    pub fn subscribe_market_tick(&self) -> broadcast::Receiver<MarketTick> {
        self.market_tick_tx.subscribe()
    }
    pub fn subscribe_trade_signal(&self) -> broadcast::Receiver<TradeSignal> {
        self.trade_signal_tx.subscribe()
    }
    pub fn subscribe_close_request(&self) -> broadcast::Receiver<CloseRequest> {
        self.close_request_tx.subscribe()
    }
    pub fn subscribe_order_update(&self) -> broadcast::Receiver<OrderUpdate> {
        self.order_update_tx.subscribe()
    }
    pub fn subscribe_position_update(&self) -> broadcast::Receiver<PositionUpdate> {
        self.position_update_tx.subscribe()
    }
    pub fn subscribe_balance_update(&self) -> broadcast::Receiver<BalanceUpdate> {
        self.balance_update_tx.subscribe()
    }
    pub fn subscribe_ordering_state_update(&self) -> broadcast::Receiver<OrderingStateUpdate> {
        self.ordering_state_update_tx.subscribe()
    }
    pub fn health_stats(&self) -> EventBusHealth {
        EventBusHealth {
            market_tick_receivers: self.market_tick_tx.receiver_count(),
            trade_signal_receivers: self.trade_signal_tx.receiver_count(),
            close_request_receivers: self.close_request_tx.receiver_count(),
            order_update_receivers: self.order_update_tx.receiver_count(),
            position_update_receivers: self.position_update_tx.receiver_count(),
            balance_update_receivers: self.balance_update_tx.receiver_count(),
        }
    }
}
#[derive(Debug, Clone)]
pub struct EventBusHealth {
    pub market_tick_receivers: usize,
    pub trade_signal_receivers: usize,
    pub close_request_receivers: usize,
    pub order_update_receivers: usize,
    pub position_update_receivers: usize,
    pub balance_update_receivers: usize,
}
impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}
