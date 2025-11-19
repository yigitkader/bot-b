use crate::types::{
    BalanceSnapshot, BalanceStore, OrderState, OrderUpdate, PositionState, PositionUpdate,
    SharedState,
};
use log::warn;
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
            // Note: set_open_order is used for manual state changes, not from updates
            // So we don't update last_order here
            if !open {
                // If setting to false, clear order_sent_at
                state.order_sent_at = None;
            }
        }
    }
    
    /// Mark that an order was sent (for timeout tracking)
    pub fn mark_order_sent(&self) {
        if let Ok(mut state) = self.order_state.lock() {
            state.order_sent_at = Some(chrono::Utc::now());
        }
    }
    
    /// Check if order was sent but no update received (timeout check)
    pub fn check_order_timeout(&self, timeout_secs: u64) -> bool {
        if let Ok(state) = self.order_state.lock() {
            if let Some(sent_at) = state.order_sent_at {
                let elapsed = chrono::Utc::now() - sent_at;
                if elapsed.num_seconds() > timeout_secs as i64 {
                    return true;
                }
            }
        }
        false
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
                "USDT" => {
                    // Check for stale updates
                    if let Some(last_ts) = balance.last_usdt_update {
                        if snap.ts < last_ts {
                            warn!(
                                "STATE: stale USDT balance update ignored (update: {:?}, last: {:?})",
                                snap.ts, last_ts
                            );
                            return;
                        }
                    }
                    balance.usdt = snap.free;
                    balance.last_usdt_update = Some(snap.ts);
                }
                "USDC" => {
                    // Check for stale updates
                    if let Some(last_ts) = balance.last_usdc_update {
                        if snap.ts < last_ts {
                            warn!(
                                "STATE: stale USDC balance update ignored (update: {:?}, last: {:?})",
                                snap.ts, last_ts
                            );
                            return;
                        }
                    }
                    balance.usdc = snap.free;
                    balance.last_usdc_update = Some(snap.ts);
                }
                _ => {}
            }
        }
    }

    pub fn apply_order_update(&self, update: &OrderUpdate) {
        if let Ok(mut state) = self.order_state.lock() {
            // Check for stale updates
            if let Some(last) = &state.last_order {
                if update.ts < last.ts {
                    warn!(
                        "STATE: stale order update ignored (update: {:?}, last: {:?})",
                        update.ts, last.ts
                    );
                    return;
                }
            }
            
            let is_active = !matches!(
                update.status,
                crate::types::OrderStatus::Canceled
                    | crate::types::OrderStatus::Filled
                    | crate::types::OrderStatus::Rejected
            );
            state.has_open_order = is_active;
            state.last_order = Some(update.clone());
            // Clear order_sent_at when we receive an update
            if !is_active {
                state.order_sent_at = None;
            }
        }
    }

    pub fn apply_position_update(&self, update: &PositionUpdate) {
        if let Ok(mut state) = self.position_state.lock() {
            // Check for stale updates
            if let Some(last) = &state.last_position {
                if update.ts < last.ts {
                    warn!(
                        "STATE: stale position update ignored (update: {:?}, last: {:?})",
                        update.ts, last.ts
                    );
                    return;
                }
            }
            
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

    /// Get quote balance (USDT + USDC) in USD
    pub fn get_quote_balance(&self) -> f64 {
        self.balance
            .lock()
            .map(|balance| balance.usdt + balance.usdc)
            .unwrap_or(0.0)
    }
}
