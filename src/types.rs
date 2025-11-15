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
