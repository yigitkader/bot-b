use crate::types::{
    BalanceSnapshot, BalanceStore, EquityState, OrderState, OrderUpdate, PositionMeta,
    PositionState, PositionUpdate, SharedState,
};
use chrono::Datelike;
use log::warn;
use std::sync::{Arc, Mutex};

impl SharedState {
    pub fn new() -> Self {
        use chrono::Utc;
        let now = Utc::now();
        let daily_reset = now.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc();
        let weekday_num = now.date_naive().weekday().num_days_from_monday() as i64;
        let weekly_reset = daily_reset - chrono::Duration::days(weekday_num);
        
        let initial_equity = EquityState {
            peak_equity: 0.0,
            current_equity: 0.0,
            daily_peak: 0.0,
            weekly_peak: 0.0,
            daily_reset_time: daily_reset,
            weekly_reset_time: weekly_reset,
        };

        Self {
            balance: Arc::new(Mutex::new(BalanceStore::default())),
            order_state: Arc::new(Mutex::new(OrderState::default())),
            position_state: Arc::new(Mutex::new(PositionState::default())),
            equity_state: Arc::new(Mutex::new(initial_equity)),
        }
    }

    pub fn set_pending_position_meta(&self, meta: PositionMeta) {
        if let Ok(mut state) = self.position_state.lock() {
            state.pending_meta = Some(meta);
        }
    }

    pub fn clear_pending_position_meta(&self) {
        if let Ok(mut state) = self.position_state.lock() {
            state.pending_meta = None;
        }
    }

    pub fn current_position_meta(&self) -> Option<PositionMeta> {
        self.position_state
            .lock()
            .ok()
            .and_then(|state| state.active_meta.clone())
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

    /// Clear order sent timestamp (used when order fails to send)
    pub fn clear_order_sent(&self) {
        if let Ok(mut state) = self.order_state.lock() {
            state.order_sent_at = None;
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
            // Check for duplicate or stale updates
            if let Some(last) = &state.last_position {
                // Check for duplicate using position_id + entry_price + size + is_closed combination
                // This is more reliable than timestamp because Binance may send the same event
                // multiple times with different timestamps during WebSocket reconnects
                if update.position_id == last.position_id
                    && (update.entry_price - last.entry_price).abs() < f64::EPSILON
                    && (update.size - last.size).abs() < f64::EPSILON
                    && update.is_closed == last.is_closed
                {
                    // Position ID, entry price, size, and closed status are identical
                    // This is definitely a duplicate update, ignore silently
                    // (common with broadcast channels or WebSocket reconnects where same message can be received multiple times)
                    return;
                }

                // Check for stale updates (older timestamp) - but allow if it's a different position
                if update.ts < last.ts {
                    // If it's the same position_id, it's definitely stale
                    if update.position_id == last.position_id {
                        warn!(
                            "STATE: stale position update ignored (update: {:?}, last: {:?}, position_id: {})",
                            update.ts, last.ts, update.position_id
                        );
                        return;
                    }
                    // Different position_id with older timestamp - could be from a different position
                    // Allow it but log a warning
                    warn!(
                        "STATE: position update with older timestamp but different position_id (update: {:?}, last: {:?}, update_id: {}, last_id: {})",
                        update.ts, last.ts, update.position_id, last.position_id
                    );
                }
            }

            let is_opening_new_position = if !update.is_closed {
                match &state.last_position {
                    None => true,
                    Some(last) => last.is_closed || last.position_id != update.position_id,
                }
            } else {
                false
            };

            if update.is_closed {
                state.active_meta = None;
                state.pending_meta = None;
            } else if is_opening_new_position {
                if let Some(meta) = state.pending_meta.take() {
                    state.active_meta = Some(meta);
                }
            }

            // Apply the update
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

    /// Update equity (balance + unrealized PnL from positions)
    pub fn update_equity(&self, unrealized_pnl: f64) {
        use chrono::Utc;
        let balance = self.get_quote_balance();
        let equity = balance + unrealized_pnl;

        if let Ok(mut eq_state) = self.equity_state.lock() {
            let now = Utc::now();
            
            // Reset daily/weekly peaks if needed
            if now >= eq_state.daily_reset_time + chrono::Duration::days(1) {
                eq_state.daily_peak = equity;
                eq_state.daily_reset_time = now.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc();
            }
            if now >= eq_state.weekly_reset_time + chrono::Duration::days(7) {
                eq_state.weekly_peak = equity;
                let weekday_num = now.date_naive().weekday().num_days_from_monday() as i64;
                eq_state.weekly_reset_time = eq_state.daily_reset_time - chrono::Duration::days(weekday_num);
            }

            eq_state.current_equity = equity;
            if equity > eq_state.peak_equity {
                eq_state.peak_equity = equity;
            }
            if equity > eq_state.daily_peak {
                eq_state.daily_peak = equity;
            }
            if equity > eq_state.weekly_peak {
                eq_state.weekly_peak = equity;
            }
        }
    }

    /// Get current equity
    pub fn get_equity(&self) -> f64 {
        self.equity_state
            .lock()
            .map(|eq| eq.current_equity)
            .unwrap_or(0.0)
    }

    /// Get current drawdown percentage
    pub fn get_drawdown_pct(&self) -> f64 {
        if let Ok(eq_state) = self.equity_state.lock() {
            if eq_state.peak_equity > 0.0 {
                (eq_state.peak_equity - eq_state.current_equity) / eq_state.peak_equity
            } else {
                0.0
            }
        } else {
            0.0
        }
    }

    /// Get daily drawdown percentage
    pub fn get_daily_drawdown_pct(&self) -> f64 {
        if let Ok(eq_state) = self.equity_state.lock() {
            if eq_state.daily_peak > 0.0 {
                (eq_state.daily_peak - eq_state.current_equity) / eq_state.daily_peak
            } else {
                0.0
            }
        } else {
            0.0
        }
    }

    /// Get weekly drawdown percentage
    pub fn get_weekly_drawdown_pct(&self) -> f64 {
        if let Ok(eq_state) = self.equity_state.lock() {
            if eq_state.weekly_peak > 0.0 {
                (eq_state.weekly_peak - eq_state.current_equity) / eq_state.weekly_peak
            } else {
                0.0
            }
        } else {
            0.0
        }
    }
}
