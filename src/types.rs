// Domain types for the trading bot
// All types and enums are centralized here for single source of truth

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::time::Instant;

// ============================================================================
// Core Domain Types
// ============================================================================

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Px(pub Decimal);

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Qty(pub Decimal);

/// Order side - represents the direction of an order (BUY or SELL)
/// 
/// **IMPORTANT**: This is for ORDER sides, NOT position directions!
/// - Use `Side::Buy` / `Side::Sell` for placing orders
/// - Use `PositionDirection::Long` / `PositionDirection::Short` for positions
/// 
/// In Binance:
/// - Long position: positionAmt > 0 (opened with BUY order)
/// - Short position: positionAmt < 0 (opened with SELL order)
/// 
/// When storing positions, use `PositionDirection` and store absolute qty values.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}

/// Position direction - represents whether a position is long or short
/// This is separate from Side (order side) to avoid confusion
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum PositionDirection {
    /// Long position (qty > 0) - profit when price goes up
    Long,
    /// Short position (qty < 0) - profit when price goes down
    Short,
}

impl PositionDirection {
    /// Convert from order side to position direction
    /// Buy order opens Long position, Sell order opens Short position
    pub fn from_order_side(side: Side) -> Self {
        match side {
            Side::Buy => PositionDirection::Long,
            Side::Sell => PositionDirection::Short,
        }
    }
    
    /// Determine position direction from quantity sign
    /// Positive qty = Long, Negative qty = Short
    pub fn from_qty_sign(qty: Decimal) -> Self {
        if qty.is_sign_positive() {
            PositionDirection::Long
        } else {
            PositionDirection::Short
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum Tif {
    Gtc,
    Ioc,
    PostOnly,
}

// ============================================================================
// Connection Module Types
// ============================================================================

/// Order command for ORDERING module
#[derive(Debug, Clone)]
pub enum OrderCommand {
    Open {
        symbol: String,
        side: Side,
        price: Px,
        qty: Qty,
        tif: Tif,
    },
    Close {
        symbol: String,
        side: Side,
        price: Px,
        qty: Qty,
        tif: Tif,
    },
}

/// Internal order type (used within connection module)
#[derive(Clone, Debug)]
pub struct VenueOrder {
    pub order_id: String,
    pub side: Side,
    pub price: Px,
    pub qty: Qty,
}

/// Internal position type (used within connection module)
#[derive(Clone, Debug)]
pub struct Position {
    pub symbol: String,
    pub qty: Qty,
    pub entry: Px,
    pub leverage: u32,
    pub liq_px: Option<Px>,
}

/// Symbol trading rules (tick size, step size, precision, etc.)
#[derive(Clone, Debug)]
pub struct SymbolRules {
    pub tick_size: Decimal,
    pub step_size: Decimal,
    pub price_precision: usize,
    pub qty_precision: usize,
    pub min_notional: Decimal,
    /// Maximum leverage allowed for this symbol (from exchange)
    /// None if not available (will use config max_leverage instead)
    pub max_leverage: Option<u32>,
}

/// Symbol metadata from exchange
#[derive(Clone, Debug)]
pub struct SymbolMeta {
    pub symbol: String,
    pub base_asset: String,
    pub quote_asset: String,
    pub status: Option<String>,
    pub contract_type: Option<String>,
}

/// User data stream kind
#[derive(Clone, Copy, Debug)]
pub enum UserStreamKind {
    Futures,
}

/// User data stream events
#[derive(Debug, Clone)]
pub enum UserEvent {
    OrderFill {
        symbol: String,
        order_id: String,
        side: Side,
        qty: Qty,                   // Last executed qty (incremental, this fill only)
        cumulative_filled_qty: Qty, // Cumulative filled qty (total filled so far)
        order_qty: Option<Qty>,    // Original order quantity (from "q" field, may be missing)
        price: Px,
        is_maker: bool,       // true = maker, false = taker
        order_status: String, // Order status: NEW, PARTIALLY_FILLED, FILLED, CANCELED, etc.
        commission: Decimal,  // Real commission from executionReport "n" field
    },
    OrderCanceled {
        symbol: String,
        order_id: String,
        client_order_id: Option<String>, // For idempotency
    },
    AccountUpdate {
        // Position updates
        positions: Vec<AccountPosition>,
        // Balance updates
        balances: Vec<AccountBalance>,
    },
    Heartbeat,
}

/// Account position from WebSocket
#[derive(Debug, Clone)]
pub struct AccountPosition {
    pub symbol: String,
    pub position_amt: Decimal,
    pub entry_price: Decimal,
    pub leverage: u32,
    pub unrealized_pnl: Option<Decimal>,
}

/// Account balance from WebSocket
#[derive(Debug, Clone)]
pub struct AccountBalance {
    pub asset: String,
    pub available_balance: Decimal,
}

/// Market data price update from WebSocket (@bookTicker stream)
#[derive(Debug, Clone)]
pub struct PriceUpdate {
    pub symbol: String,
    pub bid: Px,
    pub ask: Px,
    pub bid_qty: Qty,
    pub ask_qty: Qty,
}

// ============================================================================
// Event Bus Types
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
    /// Spread in basis points when signal was generated (for validation at order placement)
    pub spread_bps: f64,
    /// Timestamp when spread was measured (for staleness check)
    #[serde(skip)]
    pub spread_timestamp: Instant, // Not serialized, only for runtime use
    #[serde(skip)]
    pub timestamp: Instant, // Not serialized, only for runtime use
}

/// Close request event - published by FOLLOW_ORDERS
#[derive(Clone, Debug, Serialize)]
pub struct CloseRequest {
    pub symbol: String,
    /// Optional position identifier - currently NOT USED (reserved for future hedge mode support)
    pub position_id: Option<String>,
    pub reason: CloseReason,
    /// Current bid and ask prices from the market tick that triggered this close request
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
    pub average_fill_price: Px,
    /// Total order quantity (original order size)
    pub qty: Qty,
    /// Cumulative filled quantity (total filled so far across all fills)
    pub filled_qty: Qty,
    /// Remaining quantity to be filled (qty - filled_qty)
    pub remaining_qty: Qty,
    pub status: OrderStatus,
    /// True if all fills were maker orders, None if unknown
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

/// Ordering state update event - published by ORDERING
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
    pub direction: String, // "Long" or "Short"
    pub qty: String, // Decimal as string
    pub entry_price: String, // Decimal as string
}

/// Snapshot of open order for persistence
#[derive(Clone, Debug, Serialize)]
pub struct OpenOrderSnapshot {
    pub symbol: String,
    pub order_id: String,
    pub side: String, // "Buy" or "Sell"
    pub qty: String, // Decimal as string
}

/// Order fill history update event - published by CONNECTION
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
    pub total_filled_qty: String, // Decimal as string
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
// State Types
// ============================================================================

/// Global state for ORDERING module
#[derive(Clone, Debug)]
pub struct OrderingState {
    pub open_position: Option<OpenPosition>,
    pub open_order: Option<OpenOrder>,
    /// Timestamp of the last OrderUpdate event that modified state
    pub last_order_update_timestamp: Option<Instant>,
    /// Timestamp of the last PositionUpdate event that modified state
    pub last_position_update_timestamp: Option<Instant>,
}

impl OrderingState {
    pub fn new() -> Self {
        Self {
            open_position: None,
            open_order: None,
            last_order_update_timestamp: None,
            last_position_update_timestamp: None,
        }
    }
}

impl Default for OrderingState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug)]
pub struct OpenPosition {
    pub symbol: String,
    /// Position direction (Long or Short) - separate from order side to avoid confusion
    pub direction: PositionDirection,
    /// Position quantity - always positive (absolute value)
    pub qty: Qty,
    pub entry_price: Px,
}

#[derive(Clone, Debug)]
pub struct OpenOrder {
    pub symbol: String,
    pub order_id: String,
    pub side: Side,
    pub qty: Qty,
}

/// Shared balance store
#[derive(Clone, Debug)]
pub struct BalanceStore {
    pub usdt: Decimal,
    pub usdc: Decimal,
    pub last_updated: Instant,
    /// Reserved balance (USDT) - balance reserved for pending orders
    pub reserved_usdt: Decimal,
    /// Reserved balance (USDC) - balance reserved for pending orders
    pub reserved_usdc: Decimal,
}

impl BalanceStore {
    pub fn new() -> Self {
        Self {
            usdt: Decimal::ZERO,
            usdc: Decimal::ZERO,
            last_updated: Instant::now(),
            reserved_usdt: Decimal::ZERO,
            reserved_usdc: Decimal::ZERO,
        }
    }
    
    /// Get available balance (total - reserved) for an asset
    pub fn available(&self, asset: &str) -> Decimal {
        let (total, reserved) = if asset.to_uppercase() == "USDT" {
            (self.usdt, self.reserved_usdt)
        } else {
            (self.usdc, self.reserved_usdc)
        };
        total - reserved
    }
    
    /// Reserve balance atomically (returns true if reservation successful, false if insufficient)
    pub fn try_reserve(&mut self, asset: &str, amount: Decimal) -> bool {
        let asset_upper = asset.to_uppercase();
        let is_usdt = asset_upper == "USDT";
        
        let (total, reserved) = if is_usdt {
            (self.usdt, self.reserved_usdt)
        } else {
            (self.usdc, self.reserved_usdc)
        };
        
        let available = total - reserved;
        if available >= amount {
            if is_usdt {
                self.reserved_usdt += amount;
            } else {
                self.reserved_usdc += amount;
            }
            true
        } else {
            false
        }
    }
    
    /// Release reserved balance
    pub fn release(&mut self, asset: &str, amount: Decimal) {
        if asset.to_uppercase() == "USDT" {
            self.reserved_usdt = (self.reserved_usdt - amount).max(Decimal::ZERO);
        } else {
            self.reserved_usdc = (self.reserved_usdc - amount).max(Decimal::ZERO);
        }
    }
}

impl Default for BalanceStore {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Connection Module Types
// ============================================================================

/// Binance futures filter type
#[derive(Deserialize)]
#[serde(tag = "filterType")]
#[allow(non_snake_case)]
pub enum FutFilter {
    #[serde(rename = "PRICE_FILTER")]
    PriceFilter { tickSize: String },
    #[serde(rename = "LOT_SIZE")]
    LotSize { stepSize: String },
    #[serde(rename = "MIN_NOTIONAL")]
    MinNotional { notional: String },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
pub struct FutExchangeInfo {
    pub symbols: Vec<FutExchangeSymbol>,
}

#[derive(Deserialize)]
pub struct FutExchangeSymbol {
    pub symbol: String,
    #[serde(rename = "baseAsset")]
    pub base_asset: String,
    #[serde(rename = "quoteAsset")]
    pub quote_asset: String,
    #[serde(rename = "contractType")]
    pub contract_type: String,
    pub status: String,
    #[serde(default)]
    pub filters: Vec<FutFilter>,
    #[serde(rename = "pricePrecision", default)]
    pub price_precision: Option<usize>,
    #[serde(rename = "quantityPrecision", default)]
    pub qty_precision: Option<usize>,
}

/// Order fill history (internal to connection module)
#[derive(Clone, Debug)]
pub struct OrderFillHistory {
    pub total_filled_qty: Qty,
    pub weighted_price_sum: Decimal, // Sum of (price * qty) for all fills
    pub last_update: Instant,        // Timestamp of last update (for cleanup)
    /// Number of fills that were maker orders (for commission calculation)
    pub maker_fill_count: u32,
    /// Total number of fills for this order
    pub total_fill_count: u32,
}

// ============================================================================
// Trending Module Types
// ============================================================================

/// Price point for trend analysis
#[derive(Clone, Debug)]
pub struct PricePoint {
    pub timestamp: Instant,
    pub price: Decimal,
    pub volume: Option<Decimal>,
}

/// Symbol state for trend analysis
#[derive(Clone, Debug)]
pub struct SymbolState {
    pub symbol: String,
    pub prices: std::collections::VecDeque<PricePoint>,
    pub last_signal_time: Option<Instant>,
    /// Timestamp of last position close for this symbol
    pub last_position_close_time: Option<Instant>,
    /// Direction of last closed position for this symbol
    pub last_position_direction: Option<PositionDirection>,
    /// Tick counter for sampling (event flood prevention)
    pub tick_counter: u32,
}

/// Trend signal direction
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TrendSignal {
    Long,   // Uptrend - buy signal
    Short,  // Downtrend - sell signal
}

/// Last signal information for cooldown and direction checking
#[derive(Clone, Debug)]
pub struct LastSignal {
    pub side: Side,
    pub timestamp: Instant,
}

// ============================================================================
// Follow Orders Module Types
// ============================================================================

/// Position information for TP/SL tracking
#[derive(Clone, Debug)]
pub struct PositionInfo {
    pub symbol: String,
    pub qty: Qty,
    pub entry_price: Px,
    /// Position direction (Long or Short) - clearer than using Side
    pub direction: PositionDirection,
    pub leverage: u32,
    pub stop_loss_pct: Option<f64>,
    pub take_profit_pct: Option<f64>,
    pub opened_at: Instant,
    pub is_maker: Option<bool>,
}

// ============================================================================
// Storage Module Types
// ============================================================================

/// OrderFillHistory structure for storage (matches connection.rs)
#[derive(Clone, Debug)]
pub struct StorageOrderFillHistory {
    pub total_filled_qty: Qty,
    pub weighted_price_sum: Decimal,
    pub last_update: Instant,
    pub maker_fill_count: u32,
    pub total_fill_count: u32,
}
