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
    /// Last fill price (price of the most recent fill)
    pub last_fill_price: Px,
    /// Average fill price (weighted average of all fills for this order)
    /// This is the correct entry price to use for position tracking
    pub average_fill_price: Px,
    /// Total order quantity (original order size)
    pub qty: Qty,
    /// Cumulative filled quantity (total filled so far across all fills)
    pub filled_qty: Qty,
    /// Remaining quantity to be filled (qty - filled_qty)
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
    Expired,
    ExpiredInMatch,
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
    /// Create a new EventBus with custom buffer sizes.
    ///
    /// This constructor allows you to configure buffer sizes for each event channel based on
    /// expected event frequency and subscriber processing speed.
    ///
    /// # Arguments
    ///
    /// * `cfg` - EventBusCfg containing buffer sizes for each event channel
    ///
    /// # Returns
    ///
    /// Returns a new `EventBus` instance with configured buffer sizes.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use crate::config::EventBusCfg;
    ///
    /// let cfg = EventBusCfg {
    ///     market_tick_buffer: 10000,
    ///     trade_signal_buffer: 1000,
    ///     ..Default::default()
    /// };
    /// let event_bus = EventBus::new_with_config(&cfg);
    /// ```
    pub fn new_with_config(cfg: &crate::config::EventBusCfg) -> Self {
        let (market_tick_tx, _) = broadcast::channel(cfg.market_tick_buffer);
        let (trade_signal_tx, _) = broadcast::channel(cfg.trade_signal_buffer);
        let (close_request_tx, _) = broadcast::channel(cfg.close_request_buffer);
        let (order_update_tx, _) = broadcast::channel(cfg.order_update_buffer);
        let (position_update_tx, _) = broadcast::channel(cfg.position_update_buffer);
        let (balance_update_tx, _) = broadcast::channel(cfg.balance_update_buffer);
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
    
    /// Create a new EventBus with default buffer sizes.
    ///
    /// This constructor uses default buffer sizes (10000 for MarketTick, 1000 for others).
    /// For custom buffer sizes, use `new_with_config()` instead.
    ///
    /// # Returns
    ///
    /// Returns a new `EventBus` instance with default configuration.
    ///
    /// # Example
    ///
    /// ```no_run
    /// let event_bus = EventBus::new();
    /// ```
    pub fn new() -> Self {
        let default_cfg = crate::config::EventBusCfg::default();
        Self::new_with_config(&default_cfg)
    }
    
    /// Subscribe to market tick events.
    ///
    /// Returns a new broadcast receiver that will receive all MarketTick events published
    /// to the event bus. Each call creates a new independent receiver.
    ///
    /// # Returns
    ///
    /// Returns a `broadcast::Receiver<MarketTick>` that can be used to receive market tick events.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # let event_bus = crate::event_bus::EventBus::new();
    /// let mut receiver = event_bus.subscribe_market_tick();
    /// // Use receiver.recv().await to receive events
    /// ```
    pub fn subscribe_market_tick(&self) -> broadcast::Receiver<MarketTick> {
        self.market_tick_tx.subscribe()
    }
    
    /// Subscribe to trade signal events.
    ///
    /// Returns a new broadcast receiver for TradeSignal events generated by the TRENDING module.
    ///
    /// # Returns
    ///
    /// Returns a `broadcast::Receiver<TradeSignal>` for receiving trade signals.
    pub fn subscribe_trade_signal(&self) -> broadcast::Receiver<TradeSignal> {
        self.trade_signal_tx.subscribe()
    }
    
    /// Subscribe to close request events.
    ///
    /// Returns a new broadcast receiver for CloseRequest events published by FOLLOW_ORDERS module.
    ///
    /// # Returns
    ///
    /// Returns a `broadcast::Receiver<CloseRequest>` for receiving close requests.
    pub fn subscribe_close_request(&self) -> broadcast::Receiver<CloseRequest> {
        self.close_request_tx.subscribe()
    }
    
    /// Subscribe to order update events.
    ///
    /// Returns a new broadcast receiver for OrderUpdate events published by CONNECTION module
    /// when orders are filled, canceled, or updated.
    ///
    /// # Returns
    ///
    /// Returns a `broadcast::Receiver<OrderUpdate>` for receiving order updates.
    pub fn subscribe_order_update(&self) -> broadcast::Receiver<OrderUpdate> {
        self.order_update_tx.subscribe()
    }
    
    /// Subscribe to position update events.
    ///
    /// Returns a new broadcast receiver for PositionUpdate events published by CONNECTION module
    /// when positions are opened, closed, or updated.
    ///
    /// # Returns
    ///
    /// Returns a `broadcast::Receiver<PositionUpdate>` for receiving position updates.
    pub fn subscribe_position_update(&self) -> broadcast::Receiver<PositionUpdate> {
        self.position_update_tx.subscribe()
    }
    
    /// Subscribe to balance update events.
    ///
    /// Returns a new broadcast receiver for BalanceUpdate events published by BALANCE module
    /// when USDT or USDC balances change.
    ///
    /// # Returns
    ///
    /// Returns a `broadcast::Receiver<BalanceUpdate>` for receiving balance updates.
    pub fn subscribe_balance_update(&self) -> broadcast::Receiver<BalanceUpdate> {
        self.balance_update_tx.subscribe()
    }
    
    /// Get health statistics for monitoring event bus status.
    ///
    /// Returns receiver counts for all event channels, which can be used to detect if modules
    /// have crashed or if there are no subscribers for critical events.
    ///
    /// # Returns
    ///
    /// Returns an `EventBusHealth` struct containing receiver counts for each event channel.
    /// A count of 0 indicates no active subscribers, which may indicate a module has panicked.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # let event_bus = crate::event_bus::EventBus::new();
    /// let health = event_bus.health_stats();
    /// if health.order_update_receivers == 0 {
    ///     eprintln!("Warning: No OrderUpdate subscribers!");
    /// }
    /// ```
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

/// Health statistics for event bus monitoring
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

