use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use super::events::{OrderUpdate, PositionUpdate};

#[derive(Debug, Default, Clone)]
pub struct BalanceStore {
    pub usdt: f64,
    pub usdc: f64,
    pub last_usdt_update: Option<DateTime<Utc>>,
    pub last_usdc_update: Option<DateTime<Utc>>,
}

#[derive(Debug, Default, Clone)]
pub struct OrderState {
    pub open_orders: HashSet<String>,
    pub last_orders: HashMap<String, OrderUpdate>,
    pub order_sent_at: HashMap<String, DateTime<Utc>>,
}

#[derive(Debug, Default, Clone)]
pub struct PositionState {
    pub active_positions: HashMap<String, PositionUpdate>,
    pub pending_meta: HashMap<String, PositionMeta>,
    pub active_meta: HashMap<String, PositionMeta>,
}

#[derive(Debug, Default, Clone)]
pub struct EquityState {
    pub peak_equity: f64,
    pub current_equity: f64,
    pub daily_peak: f64,
    pub weekly_peak: f64,
    pub daily_reset_time: DateTime<Utc>,
    pub weekly_reset_time: DateTime<Utc>,
}

#[derive(Clone)]
pub struct SharedState {
    pub balance: Arc<Mutex<BalanceStore>>,
    pub order_state: Arc<Mutex<OrderState>>,
    pub position_state: Arc<Mutex<PositionState>>,
    pub equity_state: Arc<Mutex<EquityState>>,
}

#[derive(Debug, Default, Clone)]
pub struct PositionMeta {
    pub atr_at_entry: Option<f64>,
}

