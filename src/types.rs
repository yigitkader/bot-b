use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum Side {
    Long,
    Short,
}

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloseRequest {
    pub position_id: Uuid,
    pub reason: String,
    pub ts: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OrderStatus {
    New,
    PartiallyFilled,
    Filled,
    Canceled,
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderUpdate {
    pub order_id: Uuid,
    pub symbol: String,
    pub side: Side,
    pub status: OrderStatus,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BalanceSnapshot {
    pub asset: String,
    pub free: f64,
    pub ts: DateTime<Utc>,
}
