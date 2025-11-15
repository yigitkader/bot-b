// Domain types for the trading bot
// Core domain types only - no abstractions, just data structures

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

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
    
    /// Convert to order side for closing
    /// Long position closes with Sell order, Short position closes with Buy order
    pub fn to_close_side(self) -> Side {
        match self {
            PositionDirection::Long => Side::Sell,
            PositionDirection::Short => Side::Buy,
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
