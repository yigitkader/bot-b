// Event bus system for module communication
// All modules communicate through events, no direct coupling

use crate::types::{Px, Qty, Side};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::time::Instant;

// ============================================================================
// Core Events
// ============================================================================

/// Market tick event - published by CONNECTION
#[derive(Clone, Debug, Serialize)]
pub struct MarketTick {
    pub symbol: String,
    pub bid: Px,
    pub ask: Px,
    pub mark_price: Option<Px>,
    pub volume: Option<Decimal>,
    #[serde(skip)]
    pub timestamp: Instant, // Not serialized, only for runtime use
}

/// Trade signal event - published by TRENDING
#[derive(Clone, Debug, Serialize)]
pub struct TradeSignal {
    pub symbol: String,
    pub side: Side,
    pub entry_price: Px,
    pub leverage: u32,
    pub size: Qty,
    pub stop_loss_pct: Option<f64>, // Optional stop loss percentage
    pub take_profit_pct: Option<f64>, // Optional take profit percentage
    #[serde(skip)]
    pub timestamp: Instant, // Not serialized, only for runtime use
}

/// Close request event - published by FOLLOW_ORDERS
#[derive(Clone, Debug, Serialize)]
pub struct CloseRequest {
    pub symbol: String,
    pub position_id: Option<String>, // Optional position identifier
    pub reason: CloseReason,
    #[serde(skip)]
    pub timestamp: Instant, // Not serialized, only for runtime use
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum CloseReason {
    TakeProfit,
    StopLoss,
    Manual, // Manual close request
}

/// Order update event - published by CONNECTION
#[derive(Clone, Debug, Serialize)]
pub struct OrderUpdate {
    pub symbol: String,
    pub order_id: String,
    pub side: Side,
    pub price: Px,
    pub qty: Qty,
    pub filled_qty: Qty,
    pub remaining_qty: Qty,
    pub status: OrderStatus,
    #[serde(skip)]
    pub timestamp: Instant, // Not serialized, only for runtime use
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum OrderStatus {
    New,
    PartiallyFilled,
    Filled,
    Canceled,
    Rejected,
}

/// Position update event - published by CONNECTION
#[derive(Clone, Debug, Serialize)]
pub struct PositionUpdate {
    pub symbol: String,
    pub qty: Qty,
    pub entry_price: Px,
    pub leverage: u32,
    pub unrealized_pnl: Option<Decimal>, // Optional unrealized PnL
    pub is_open: bool, // true if position is open, false if closed
    #[serde(skip)]
    pub timestamp: Instant, // Not serialized, only for runtime use
}

/// Balance update event - published by BALANCE
#[derive(Clone, Debug, Serialize)]
pub struct BalanceUpdate {
    pub usdt: Decimal,
    pub usdc: Decimal,
    #[serde(skip)]
    pub timestamp: Instant, // Not serialized, only for runtime use
}

/// Log event - published by any module
#[derive(Clone, Debug, Serialize)]
pub struct LogEvent {
    pub level: LogLevel,
    pub event_type: String,
    pub message: String,
    pub payload: Option<serde_json::Value>,
    #[serde(skip)]
    pub timestamp: Instant, // Not serialized, only for runtime use
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

// ============================================================================
// Event Bus
// ============================================================================

use tokio::sync::{broadcast, mpsc};

/// Event bus for module communication
/// Each module subscribes to events it needs
/// Uses broadcast channels for multiple subscribers
pub struct EventBus {
    pub market_tick_tx: broadcast::Sender<MarketTick>,
    
    pub trade_signal_tx: broadcast::Sender<TradeSignal>,
    
    pub close_request_tx: broadcast::Sender<CloseRequest>,
    
    pub order_update_tx: broadcast::Sender<OrderUpdate>,
    
    pub position_update_tx: broadcast::Sender<PositionUpdate>,
    
    pub balance_update_tx: broadcast::Sender<BalanceUpdate>,
    
    pub log_event_tx: mpsc::UnboundedSender<LogEvent>,
}

impl EventBus {
    pub fn new() -> Self {
        // Broadcast channels allow multiple subscribers
        let (market_tick_tx, _) = broadcast::channel(1000);
        let (trade_signal_tx, _) = broadcast::channel(1000);
        let (close_request_tx, _) = broadcast::channel(1000);
        let (order_update_tx, _) = broadcast::channel(1000);
        let (position_update_tx, _) = broadcast::channel(1000);
        let (balance_update_tx, _) = broadcast::channel(1000);
        let (log_event_tx, _) = mpsc::unbounded_channel();
        
        Self {
            market_tick_tx,
            trade_signal_tx,
            close_request_tx,
            order_update_tx,
            position_update_tx,
            balance_update_tx,
            log_event_tx,
        }
    }
    
    /// Subscribe to market tick events (returns a new receiver)
    pub fn subscribe_market_tick(&self) -> broadcast::Receiver<MarketTick> {
        self.market_tick_tx.subscribe()
    }
    
    /// Subscribe to trade signal events
    pub fn subscribe_trade_signal(&self) -> broadcast::Receiver<TradeSignal> {
        self.trade_signal_tx.subscribe()
    }
    
    /// Subscribe to close request events
    pub fn subscribe_close_request(&self) -> broadcast::Receiver<CloseRequest> {
        self.close_request_tx.subscribe()
    }
    
    /// Subscribe to order update events
    pub fn subscribe_order_update(&self) -> broadcast::Receiver<OrderUpdate> {
        self.order_update_tx.subscribe()
    }
    
    /// Subscribe to position update events
    pub fn subscribe_position_update(&self) -> broadcast::Receiver<PositionUpdate> {
        self.position_update_tx.subscribe()
    }
    
    /// Subscribe to balance update events
    pub fn subscribe_balance_update(&self) -> broadcast::Receiver<BalanceUpdate> {
        self.balance_update_tx.subscribe()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

