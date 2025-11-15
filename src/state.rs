// Shared state for the trading bot
// Small common state: openPosition, openOrder, BalanceStore
// ORDERING uses this for "single position/order" guarantee
// BALANCE updates this
// TRENDING/ORDERING can read from this

use crate::types::{Px, Qty, Side, PositionDirection};
use rust_decimal::Decimal;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{Mutex, RwLock};

// ============================================================================
// Ordering State (for ORDERING module)
// ============================================================================

/// Global state for ORDERING module
/// Guarantees single position/order at a time
/// 
/// CRITICAL: Timestamp tracking prevents stale event updates
/// OrderUpdate and PositionUpdate events may arrive out of order or be stale
/// We track the last update timestamp to ensure we only apply newer updates
#[derive(Clone, Debug)]
pub struct OrderingState {
    pub open_position: Option<OpenPosition>,
    pub open_order: Option<OpenOrder>,
    /// Timestamp of the last OrderUpdate event that modified state
    /// Used to prevent stale OrderUpdate events from overwriting newer state
    pub last_order_update_timestamp: Option<Instant>,
    /// Timestamp of the last PositionUpdate event that modified state
    /// Used to prevent stale PositionUpdate events from overwriting newer state
    pub last_position_update_timestamp: Option<Instant>,
}

#[derive(Clone, Debug)]
pub struct OpenPosition {
    pub symbol: String,
    /// Position direction (Long or Short) - separate from order side to avoid confusion
    pub direction: PositionDirection,
    /// Position quantity - always positive (absolute value)
    /// Use direction to determine if it's long or short
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

// ============================================================================
// Balance Store (for BALANCE module)
// ============================================================================

/// Shared balance store
/// Updated by BALANCE module
/// Read by TRENDING/ORDERING modules
/// 
/// CRITICAL: Balance reservation system prevents double-spending race conditions
/// When placing an order, balance should be reserved atomically before sending
/// Reservation is released after order is placed (success or failure)
#[derive(Clone, Debug)]
pub struct BalanceStore {
    pub usdt: Decimal,
    pub usdc: Decimal,
    pub last_updated: Instant,
    /// Reserved balance (USDT) - balance reserved for pending orders
    /// This prevents double-spending when multiple threads check balance simultaneously
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
    /// This is the actual available balance that can be used for new orders
    pub fn available(&self, asset: &str) -> Decimal {
        let (total, reserved) = if asset.to_uppercase() == "USDT" {
            (self.usdt, self.reserved_usdt)
        } else {
            (self.usdc, self.reserved_usdc)
        };
        total - reserved
    }
    
    /// Reserve balance atomically (returns true if reservation successful, false if insufficient)
    /// This prevents double-spending race conditions
    /// CRITICAL: This method atomically checks available balance and reserves it in a single operation
    /// Do not call available() separately before this method - it's already included here
    pub fn try_reserve(&mut self, asset: &str, amount: Decimal) -> bool {
        // Atomic read + reserve: read total and reserved, check, and increment in one operation
        let (total, reserved) = if asset.to_uppercase() == "USDT" {
            (self.usdt, self.reserved_usdt)
        } else {
            (self.usdc, self.reserved_usdc)
        };
        
        let available = total - reserved;
        if available >= amount {
            if asset.to_uppercase() == "USDT" {
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
// Shared State Container
// ============================================================================

/// Container for all shared state
/// Provides thread-safe access to ordering state and balance store
pub struct SharedState {
    pub ordering_state: Arc<Mutex<OrderingState>>,
    pub balance_store: Arc<RwLock<BalanceStore>>,
}

impl SharedState {
    pub fn new() -> Self {
        Self {
            ordering_state: Arc::new(Mutex::new(OrderingState::new())),
            balance_store: Arc::new(RwLock::new(BalanceStore::new())),
        }
    }
}

impl Default for SharedState {
    fn default() -> Self {
        Self::new()
    }
}

