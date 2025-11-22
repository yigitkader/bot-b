use crate::types::{
    events::{BalanceSnapshot, OrderUpdate, PositionUpdate},
    state::{BalanceStore, EquityState, OrderState, PositionMeta, PositionState, SharedState},
};
use chrono::Datelike;
use log::warn;
use std::sync::{Arc, Mutex};

macro_rules! with_lock {
    ($lock:expr, |$state:ident| $body:block) => {
        if let Ok(mut $state) = $lock.lock() {
            $body
        }
    };
}

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

    // --- POSITION STATE ---

    pub fn set_pending_position_meta(&self, symbol: &str, meta: PositionMeta) {
        with_lock!(self.position_state, |state| {
            state.pending_meta.insert(symbol.to_string(), meta);
        });
    }

    pub fn clear_pending_position_meta(&self, symbol: &str) {
        with_lock!(self.position_state, |state| {
            state.pending_meta.remove(symbol);
        });
    }

    pub fn current_position_meta(&self, symbol: &str) -> Option<PositionMeta> {
        self.position_state
            .lock()
            .ok()
            .and_then(|state| state.active_meta.get(symbol).cloned())
    }

    pub fn current_position(&self, symbol: &str) -> Option<PositionUpdate> {
        self.position_state
            .lock()
            .ok()
            .and_then(|state| state.active_positions.get(symbol).cloned())
    }

    pub fn has_open_position(&self, symbol: &str) -> bool {
        self.position_state
            .lock()
            .map(|state| state.active_positions.contains_key(symbol))
            .unwrap_or(false)
    }

    /// Get all active positions (for emergency close scenarios)
    pub fn get_all_active_positions(&self) -> Vec<PositionUpdate> {
        self.position_state
            .lock()
            .ok()
            .map(|state| state.active_positions.values().cloned().collect())
            .unwrap_or_default()
    }

    // --- ORDER STATE ---

    pub fn set_open_order(&self, symbol: &str, open: bool) {
        with_lock!(self.order_state, |state| {
            if open {
                state.open_orders.insert(symbol.to_string());
            } else {
                state.open_orders.remove(symbol);
                state.order_sent_at.remove(symbol);
            }
        });
    }

    pub fn mark_order_sent(&self, symbol: &str) {
        with_lock!(self.order_state, |state| {
            state.order_sent_at.insert(symbol.to_string(), chrono::Utc::now());
        });
    }

    pub fn clear_order_sent(&self, symbol: &str) {
        with_lock!(self.order_state, |state| {
            state.order_sent_at.remove(symbol);
        });
    }

    pub fn check_order_timeout(&self, timeout_secs: u64) -> Vec<String> {
        let mut timed_out_symbols = Vec::new();
        if let Ok(state) = self.order_state.lock() {
            let now = chrono::Utc::now();
            for (symbol, sent_at) in &state.order_sent_at {
                let elapsed = now - *sent_at;
                if elapsed.num_seconds() > timeout_secs as i64 {
                    timed_out_symbols.push(symbol.clone());
                }
            }
        }
        timed_out_symbols
    }

    pub fn has_open_order(&self, symbol: &str) -> bool {
        self.order_state
            .lock()
            .map(|state| state.open_orders.contains(symbol))
            .unwrap_or(false)
    }

    pub fn apply_balance_snapshot(&self, snap: &BalanceSnapshot) {
        with_lock!(self.balance, |balance| {
            match snap.asset.as_str() {
                "USDT" => {
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
        });
    }

    pub fn apply_position_update(&self, update: &PositionUpdate) {
        with_lock!(self.position_state, |state| {
            let symbol = &update.symbol;

            if let Some(last) = state.active_positions.get(symbol) {
                if update.ts < last.ts {
                    return;
                }
            }

            if update.is_closed {
                state.active_positions.remove(symbol);
                state.active_meta.remove(symbol);
                state.pending_meta.remove(symbol);
            } else {
                if !state.active_positions.contains_key(symbol) {
                    if let Some(meta) = state.pending_meta.remove(symbol) {
                        state.active_meta.insert(symbol.clone(), meta);
                    }
                }
                state.active_positions.insert(symbol.clone(), update.clone());
            }
        });
    }

    pub fn apply_order_update(&self, update: &OrderUpdate) {
        with_lock!(self.order_state, |state| {
            let symbol = &update.symbol;
            
            if let Some(last) = state.last_orders.get(symbol) {
                if update.ts < last.ts { return; }
            }

            let is_active = !matches!(
                update.status,
                crate::types::OrderStatus::Canceled
                    | crate::types::OrderStatus::Filled
                    | crate::types::OrderStatus::Rejected
            );

            if is_active {
                state.open_orders.insert(symbol.clone());
            } else {
                state.open_orders.remove(symbol);
                state.order_sent_at.remove(symbol);
            }
            state.last_orders.insert(symbol.clone(), update.clone());
        });
    }

    /// Get quote balance (USDT + USDC) in USD
    pub fn get_quote_balance(&self) -> f64 {
        self.balance
            .lock()
            .map(|balance| balance.usdt + balance.usdc)
            .unwrap_or(0.0)
    }

    // --- BALANCE & EQUITY (Aynı kalabilir, globaldir) ---
    
    /// Update equity (balance + unrealized PnL from positions)
    /// Note: This simple implementation only adds the incoming single update.
    /// In a real portfolio, total PnL from all open positions should be calculated.
    /// However, for risk management purposes, real-time equity tracking may be sufficient.
    /// In an advanced version, all active_positions should be scanned and total PnL calculated.
    /// For now, we keep it simple.
    pub fn update_equity(&self, unrealized_pnl: f64) {
        use chrono::Utc;
        let balance = self.get_quote_balance();
        let equity = balance + unrealized_pnl;

        with_lock!(self.equity_state, |eq_state| {
            let now = Utc::now();
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
            if equity > eq_state.peak_equity { eq_state.peak_equity = equity; }
            if equity > eq_state.daily_peak { eq_state.daily_peak = equity; }
            if equity > eq_state.weekly_peak { eq_state.weekly_peak = equity; }
        });
    }

    /// Get current equity
    pub fn get_equity(&self) -> f64 {
        self.equity_state
            .lock()
            .map(|eq| eq.current_equity)
            .unwrap_or(0.0)
    }

    // Diğer getter metodları (get_equity, get_drawdown_pct vs) aynı kalabilir.
    pub fn get_drawdown_pct(&self) -> f64 {
        self.equity_state
            .lock()
            .map(|eq_state| {
                if eq_state.peak_equity > 0.0 {
                    (eq_state.peak_equity - eq_state.current_equity) / eq_state.peak_equity
                } else {
                    0.0
                }
            })
            .unwrap_or(0.0)
    }
    
    pub fn get_daily_drawdown_pct(&self) -> f64 {
        self.equity_state
            .lock()
            .map(|eq| if eq.daily_peak > 0.0 { (eq.daily_peak - eq.current_equity) / eq.daily_peak } else { 0.0 })
            .unwrap_or(0.0)
    }
    
    pub fn get_weekly_drawdown_pct(&self) -> f64 {
        self.equity_state
            .lock()
            .map(|eq| if eq.weekly_peak > 0.0 { (eq.weekly_peak - eq.current_equity) / eq.weekly_peak } else { 0.0 })
            .unwrap_or(0.0)
    }
}
