use crate::types::{
    BalanceSnapshot, BalanceStore, OrderState, OrderUpdate, PositionState, PositionUpdate,
    SharedState,
};
use std::sync::{Arc, Mutex};


impl SharedState {
    pub fn new() -> Self {
        Self {
            balance: Arc::new(Mutex::new(BalanceStore::default())),
            order_state: Arc::new(Mutex::new(OrderState::default())),
            position_state: Arc::new(Mutex::new(PositionState::default())),
        }
    }

    pub fn set_open_order(&self, open: bool) {
        if let Ok(mut state) = self.order_state.lock() {
            state.has_open_order = open;
        }
    }

    pub fn has_open_order(&self) -> bool {
        self.order_state
            .lock()
            .map(|state| state.has_open_order)
            .unwrap_or(false)
    }

    pub fn has_open_position(&self) -> bool {
        self.position_state
            .lock()
            .map(|state| state.has_open_position)
            .unwrap_or(false)
    }

    pub fn apply_balance_snapshot(&self, snap: &BalanceSnapshot) {
        if let Ok(mut balance) = self.balance.lock() {
            match snap.asset.as_str() {
                "USDT" => balance.usdt = snap.free,
                "USDC" => balance.usdc = snap.free,
                _ => {}
            }
        }
    }

    pub fn apply_order_update(&self, update: &OrderUpdate) {
        let is_active = !matches!(
            update.status,
            crate::types::OrderStatus::Canceled
                | crate::types::OrderStatus::Filled
                | crate::types::OrderStatus::Rejected
        );
        self.set_open_order(is_active);
    }

    pub fn apply_position_update(&self, update: &PositionUpdate) {
        if let Ok(mut state) = self.position_state.lock() {
            state.has_open_position = !update.is_closed;
            state.last_position = Some(update.clone());
        }
    }

    pub fn current_position(&self) -> Option<PositionUpdate> {
        self.position_state
            .lock()
            .ok()
            .and_then(|state| state.last_position.clone())
    }
}
