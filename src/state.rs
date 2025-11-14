// Shared state for the trading bot
// Small common state: openPosition, openOrder, BalanceStore
// ORDERING uses this for "single position/order" guarantee
// BALANCE updates this
// TRENDING/ORDERING can read from this

use crate::types::{Px, Qty, Side};
use rust_decimal::Decimal;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{Mutex, RwLock};

// ============================================================================
// Ordering State (for ORDERING module)
// ============================================================================

/// Global state for ORDERING module
/// Guarantees single position/order at a time
#[derive(Clone, Debug)]
pub struct OrderingState {
    pub open_position: Option<OpenPosition>,
    pub open_order: Option<OpenOrder>,
}

#[derive(Clone, Debug)]
pub struct OpenPosition {
    pub symbol: String,
    pub side: Side,
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
#[derive(Clone, Debug)]
pub struct BalanceStore {
    pub usdt: Decimal,
    pub usdc: Decimal,
    pub last_updated: Instant,
}

impl BalanceStore {
    pub fn new() -> Self {
        Self {
            usdt: Decimal::ZERO,
            usdc: Decimal::ZERO,
            last_updated: Instant::now(),
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

