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

    // --- POSITION STATE ---

    pub fn set_pending_position_meta(&self, symbol: &str, meta: PositionMeta) {
        if let Ok(mut state) = self.position_state.lock() {
            state.pending_meta.insert(symbol.to_string(), meta);
        }
    }

    pub fn clear_pending_position_meta(&self, symbol: &str) {
        if let Ok(mut state) = self.position_state.lock() {
            state.pending_meta.remove(symbol);
        }
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
        if let Ok(mut state) = self.order_state.lock() {
            if open {
                state.open_orders.insert(symbol.to_string());
            } else {
                state.open_orders.remove(symbol);
                // Order kapandıysa sent_at bilgisini de temizle
                state.order_sent_at.remove(symbol);
            }
        }
    }

    pub fn mark_order_sent(&self, symbol: &str) {
        if let Ok(mut state) = self.order_state.lock() {
            state.order_sent_at.insert(symbol.to_string(), chrono::Utc::now());
        }
    }

    pub fn clear_order_sent(&self, symbol: &str) {
        if let Ok(mut state) = self.order_state.lock() {
            state.order_sent_at.remove(symbol);
        }
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

    pub fn apply_position_update(&self, update: &PositionUpdate) {
        if let Ok(mut state) = self.position_state.lock() {
            let symbol = &update.symbol;

            // Check for stale updates
            if let Some(last) = state.active_positions.get(symbol) {
                if update.ts < last.ts {
                    // Eski veri, yoksay
                    return;
                }
            }

            if update.is_closed {
                // Pozisyon kapandı: State'den temizle
                state.active_positions.remove(symbol);
                state.active_meta.remove(symbol);
                state.pending_meta.remove(symbol);
            } else {
                // Yeni veya güncel pozisyon
                // Eğer pending meta varsa, active'e taşı
                if !state.active_positions.contains_key(symbol) {
                    if let Some(meta) = state.pending_meta.remove(symbol) {
                        state.active_meta.insert(symbol.clone(), meta);
                    }
                }
                state.active_positions.insert(symbol.clone(), update.clone());
            }
        }
    }

    pub fn apply_order_update(&self, update: &OrderUpdate) {
        if let Ok(mut state) = self.order_state.lock() {
            let symbol = &update.symbol;
            
            // Stale check
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
        }
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

        if let Ok(mut eq_state) = self.equity_state.lock() {
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
        }
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
    
    pub fn get_daily_drawdown_pct(&self) -> f64 {
        if let Ok(eq) = self.equity_state.lock() {
            if eq.daily_peak > 0.0 { (eq.daily_peak - eq.current_equity) / eq.daily_peak } else { 0.0 }
        } else { 0.0 }
    }
    
    pub fn get_weekly_drawdown_pct(&self) -> f64 {
        if let Ok(eq) = self.equity_state.lock() {
            if eq.weekly_peak > 0.0 { (eq.weekly_peak - eq.current_equity) / eq.weekly_peak } else { 0.0 }
        } else { 0.0 }
    }
}
