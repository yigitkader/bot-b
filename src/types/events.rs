use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;

use super::core::Side;
use tokio::sync::broadcast::{Receiver as BReceiver, Sender as BSender};
use tokio::sync::mpsc::{Receiver as MReceiver, Sender as MSender};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketTick {
    pub symbol: String,
    pub price: f64,
    pub bid: f64,
    pub ask: f64,
    pub volume: f64,
    pub ts: DateTime<Utc>,
    pub obi: Option<f64>,
    pub funding_rate: Option<f64>,
    pub liq_long_cluster: Option<f64>,
    pub liq_short_cluster: Option<f64>,
    pub bid_depth_usd: Option<f64>,
    pub ask_depth_usd: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeSignal {
    pub id: Uuid,
    pub symbol: String,
    pub side: Side,
    pub entry_price: f64,
    pub leverage: f64,
    pub size_usdt: f64,
    pub ts: DateTime<Utc>,
    pub atr_value: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloseRequest {
    pub position_id: Uuid,
    pub reason: String,
    pub ts: DateTime<Utc>,
    pub partial_close_percentage: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderUpdate {
    pub order_id: Uuid,
    pub symbol: String,
    pub side: Side,
    pub status: super::core::OrderStatus,
    pub filled_qty: f64,
    pub ts: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PositionUpdate {
    pub position_id: Uuid,
    pub symbol: String,
    pub side: Side,
    pub entry_price: f64,
    pub size: f64,
    pub leverage: f64,
    pub unrealized_pnl: f64,
    pub ts: DateTime<Utc>,
    pub is_closed: bool,
}

impl PositionUpdate {
    pub fn position_id(symbol: &str, side: Side) -> Uuid {
        let timestamp = chrono::Utc::now().timestamp_millis();
        let name = format!("{}:{:?}:{}", symbol, side, timestamp);
        Uuid::new_v5(&Uuid::NAMESPACE_OID, name.as_bytes())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BalanceSnapshot {
    pub asset: String,
    pub free: f64,
    pub ts: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolRotationEvent {
    pub new_symbols: Vec<String>,
    pub removed_symbols: Vec<String>,
    pub all_symbols: Vec<String>,
    pub ts: DateTime<Utc>,
}

pub struct EventBus {
    pub(crate) market_tx: broadcast::Sender<MarketTick>,
    pub(crate) order_update_tx: broadcast::Sender<OrderUpdate>,
    pub(crate) position_update_tx: broadcast::Sender<PositionUpdate>,
    pub(crate) balance_tx: broadcast::Sender<BalanceSnapshot>,
    pub(crate) signal_tx: mpsc::Sender<TradeSignal>,
    pub(crate) signal_rx: Mutex<Option<mpsc::Receiver<TradeSignal>>>,
    pub(crate) close_tx: mpsc::Sender<CloseRequest>,
    pub(crate) close_rx: Mutex<Option<mpsc::Receiver<CloseRequest>>>,
    pub(crate) rotation_tx: broadcast::Sender<SymbolRotationEvent>,
}

impl Clone for EventBus {
    fn clone(&self) -> Self {
        Self {
            market_tx: self.market_tx.clone(),
            order_update_tx: self.order_update_tx.clone(),
            position_update_tx: self.position_update_tx.clone(),
            balance_tx: self.balance_tx.clone(),
            signal_tx: self.signal_tx.clone(),
            signal_rx: Mutex::new(None),
            close_tx: self.close_tx.clone(),
            close_rx: Mutex::new(None),
            rotation_tx: self.rotation_tx.clone(),
        }
    }
}

pub struct TrendingChannels {
    pub market_rx: BReceiver<MarketTick>,
    pub signal_tx: MSender<TradeSignal>,
}

#[derive(Debug)]
pub struct OrderingChannels {
    pub signal_rx: MReceiver<TradeSignal>,
    pub close_rx: MReceiver<CloseRequest>,
    pub order_update_rx: BReceiver<OrderUpdate>,
    pub position_update_rx: BReceiver<PositionUpdate>,
}

#[derive(Debug)]
pub struct FollowChannels {
    pub market_rx: BReceiver<MarketTick>,
    pub position_update_rx: BReceiver<PositionUpdate>,
    pub close_tx: MSender<CloseRequest>,
}

#[derive(Clone)]
pub struct BalanceChannels {
    pub balance_tx: BSender<BalanceSnapshot>,
}

#[derive(Debug)]
pub struct LoggingChannels {
    pub market_rx: BReceiver<MarketTick>,
    pub order_update_rx: BReceiver<OrderUpdate>,
    pub position_update_rx: BReceiver<PositionUpdate>,
    pub balance_rx: BReceiver<BalanceSnapshot>,
    pub signal_rx: MReceiver<TradeSignal>,
}

#[derive(Clone)]
pub struct ConnectionChannels {
    pub market_tx: BSender<MarketTick>,
    pub order_update_tx: BSender<OrderUpdate>,
    pub position_update_tx: BSender<PositionUpdate>,
    pub balance_tx: BSender<BalanceSnapshot>,
}

#[derive(Debug)]
pub struct RotationChannels {
    pub rotation_rx: BReceiver<SymbolRotationEvent>,
}

