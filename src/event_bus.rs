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
    pub stop_loss_pct: Option<f64>,   // Optional stop loss percentage
    pub take_profit_pct: Option<f64>, // Optional take profit percentage
    /// Spread in basis points when signal was generated (for validation at order placement)
    pub spread_bps: f64,
    /// Timestamp when spread was measured (for staleness check)
    #[serde(skip)]
    pub spread_timestamp: Instant, // Not serialized, only for runtime use
    #[serde(skip)]
    pub timestamp: Instant, // Not serialized, only for runtime use
}

/// Close request event - published by FOLLOW_ORDERS
///
/// ⚠️ DESIGN LIMITATION: position_id is currently not used
/// The position_id field exists but is ignored in the current implementation.
/// Positions are always closed by symbol only, not by specific position_id.
/// This is intentional for one-way mode (hedge_mode=false) where each symbol has only one position.
///
/// FUTURE: If hedge mode support is added with multiple positions per symbol, position_id
/// should be used to close specific positions. Until then, position_id is reserved for future use.
#[derive(Clone, Debug, Serialize)]
pub struct CloseRequest {
    pub symbol: String,
    /// Optional position identifier - currently NOT USED (reserved for future hedge mode support)
    /// In the current implementation, positions are closed by symbol only.
    /// If provided, a warning will be logged but the position_id will be ignored.
    pub position_id: Option<String>,
    pub reason: CloseReason,
    /// Current bid and ask prices from the market tick that triggered this close request
    /// If provided, these prices will be used instead of fetching fresh prices (reduces slippage)
    /// If None, prices will be fetched when processing the close request
    pub current_bid: Option<Px>,
    pub current_ask: Option<Px>,
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
    /// True if all fills were maker orders, None if unknown
    /// Used for commission calculation: if all maker, use maker commission; otherwise taker
    /// Post-only orders are typically maker, but can become taker if they cross the spread
    pub is_maker: Option<bool>,
    #[serde(skip)]
    pub timestamp: Instant, // Not serialized, only for runtime use
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
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
    pub is_open: bool,                   // true if position is open, false if closed
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

/// Ordering state update event - published by ORDERING
/// Published when OrderingState changes (order placed, filled, canceled, position opened/closed)
/// STORAGE module listens to this event to persist state
#[derive(Clone, Debug, Serialize)]
pub struct OrderingStateUpdate {
    pub open_position: Option<OpenPositionSnapshot>,
    pub open_order: Option<OpenOrderSnapshot>,
    #[serde(skip)]
    pub timestamp: Instant, // Not serialized, only for runtime use
}

/// Snapshot of open position for persistence
#[derive(Clone, Debug, Serialize)]
pub struct OpenPositionSnapshot {
    pub symbol: String,
    pub direction: String,   // "Long" or "Short"
    pub qty: String,         // Decimal as string
    pub entry_price: String, // Decimal as string
}

/// Snapshot of open order for persistence
#[derive(Clone, Debug, Serialize)]
pub struct OpenOrderSnapshot {
    pub symbol: String,
    pub order_id: String,
    pub side: String, // "Buy" or "Sell"
    pub qty: String,  // Decimal as string
}

/// Order fill history update event - published by CONNECTION
/// Published when order fill history changes (new fill, order closed)
/// STORAGE module listens to this event to persist fill history
#[derive(Clone, Debug, Serialize)]
pub struct OrderFillHistoryUpdate {
    pub order_id: String,
    pub symbol: String,
    pub action: FillHistoryAction,
    pub data: Option<FillHistoryData>, // None for Remove action
    #[serde(skip)]
    pub timestamp: Instant, // Not serialized, only for runtime use
}

#[derive(Clone, Debug, Serialize)]
pub enum FillHistoryAction {
    Save,   // Save or update fill history
    Remove, // Remove fill history (order closed)
}

/// Fill history data for persistence
#[derive(Clone, Debug, Serialize)]
pub struct FillHistoryData {
    pub total_filled_qty: String,   // Decimal as string
    pub weighted_price_sum: String, // Decimal as string
    pub maker_fill_count: u32,
    pub total_fill_count: u32,
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

    pub ordering_state_update_tx: broadcast::Sender<OrderingStateUpdate>,

    pub order_fill_history_update_tx: broadcast::Sender<OrderFillHistoryUpdate>,

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
        let (ordering_state_update_tx, _) = broadcast::channel(1000); // Buffer for state updates
        let (order_fill_history_update_tx, _) = broadcast::channel(1000); // Buffer for fill history updates
        let (log_event_tx, _) = mpsc::unbounded_channel();

        Self {
            market_tick_tx,
            trade_signal_tx,
            close_request_tx,
            order_update_tx,
            position_update_tx,
            balance_update_tx,
            ordering_state_update_tx,
            order_fill_history_update_tx,
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

    /// Subscribe to ordering state update events.
    ///
    /// Returns a new broadcast receiver for OrderingStateUpdate events published by ORDERING module
    /// when OrderingState changes (order placed, filled, canceled, position opened/closed).
    /// STORAGE module listens to this event to persist state.
    ///
    /// # Returns
    ///
    /// Returns a `broadcast::Receiver<OrderingStateUpdate>` for receiving state updates.
    pub fn subscribe_ordering_state_update(&self) -> broadcast::Receiver<OrderingStateUpdate> {
        self.ordering_state_update_tx.subscribe()
    }

    /// Subscribe to order fill history update events.
    ///
    /// Returns a new broadcast receiver for OrderFillHistoryUpdate events published by CONNECTION module
    /// when order fill history changes (new fill, order closed).
    /// STORAGE module listens to this event to persist fill history.
    ///
    /// # Returns
    ///
    /// Returns a `broadcast::Receiver<OrderFillHistoryUpdate>` for receiving fill history updates.
    pub fn subscribe_order_fill_history_update(
        &self,
    ) -> broadcast::Receiver<OrderFillHistoryUpdate> {
        self.order_fill_history_update_tx.subscribe()
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
            ordering_state_update_receivers: self.ordering_state_update_tx.receiver_count(),
            order_fill_history_update_receivers: self.order_fill_history_update_tx.receiver_count(),
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
    pub ordering_state_update_receivers: usize,
    pub order_fill_history_update_receivers: usize,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}
