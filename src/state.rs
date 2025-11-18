
pub use crate::types::{BalanceStore, OpenOrder, OpenPosition, OrderingState};

use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};


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

