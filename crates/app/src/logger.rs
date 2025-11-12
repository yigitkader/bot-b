//location: /crates/app/src/logger.rs
// Structured JSON logging system for trading events

use crate::types::*;
use crate::exchange::BinanceFutures;
use crate::exec::Venue;
use crate::utils::{rate_limit_guard, update_fill_rate_on_cancel};
use tracing::{info, warn};
use rust_decimal::prelude::ToPrimitive;
use serde::Serialize;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

// ============================================================================
// Log Event Types
// ============================================================================

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event_type")]
pub enum LogEvent {
    #[serde(rename = "order_created")]
    OrderCreated {
        timestamp: u64,
        symbol: String,
        order_id: String,
        side: String,
        price: f64,
        quantity: f64,
        notional_usd: f64,
        reason: String,
        tif: String,
    },
    #[serde(rename = "order_filled")]
    OrderFilled {
        timestamp: u64,
        symbol: String,
        order_id: String,
        side: String,
        price: f64,
        quantity: f64,
        notional_usd: f64,
        is_maker: bool,
        new_inventory: f64,
        fill_rate: f64,
    },
    #[serde(rename = "order_canceled")]
    OrderCanceled {
        timestamp: u64,
        symbol: String,
        order_id: String,
        reason: String,
        fill_rate: f64,
    },
    #[serde(rename = "position_opened")]
    PositionOpened {
        timestamp: u64,
        symbol: String,
        side: String, // "long" or "short"
        entry_price: f64,
        quantity: f64,
        notional_usd: f64,
        leverage: u32,
        reason: String,
    },
    #[serde(rename = "position_updated")]
    PositionUpdated {
        timestamp: u64,
        symbol: String,
        side: String,
        entry_price: f64,
        quantity: f64,
        mark_price: f64,
        notional_usd: f64,
        unrealized_pnl: f64,
        unrealized_pnl_pct: f64,
        leverage: u32,
    },
    #[serde(rename = "position_closed")]
    PositionClosed {
        timestamp: u64,
        symbol: String,
        side: String,
        entry_price: f64,
        exit_price: f64,
        quantity: f64,
        realized_pnl: f64,
        realized_pnl_pct: f64,
        leverage: u32,
        reason: String,
    },
    #[serde(rename = "pnl_summary")]
    PnlSummary {
        timestamp: u64,
        period: String, // "hourly", "daily", etc.
        trade_count: u32,
        profitable_trade_count: u32,
        losing_trade_count: u32,
        total_profit: f64,
        total_loss: f64,
        net_pnl: f64,
        largest_win: f64,
        largest_loss: f64,
        total_fees: f64,
    },
    #[serde(rename = "trade_completed")]
    TradeCompleted {
        timestamp: u64,
        symbol: String,
        side: String,
        entry_price: f64,
        exit_price: f64,
        quantity: f64,
        notional_usd: f64,
        realized_pnl: f64,
        realized_pnl_pct: f64,
        fees: f64,
        net_profit: f64,
        is_profitable: bool,
        leverage: u32,
    },
    #[serde(rename = "trade_rejected")]
    TradeRejected {
        timestamp: u64,
        symbol: String,
        reason: String,
        spread_bps: f64,
        position_size_usd: f64,
        min_spread_bps: f64,
    },
}

// ============================================================================
// Logger Implementation
// ============================================================================

pub struct JsonLogger {
    file_path: PathBuf,
    file: Arc<Mutex<std::fs::File>>,
    enabled: bool,
}

impl JsonLogger {
    /// Create a new JSON logger
    pub fn new(log_file: &str) -> Result<Self, std::io::Error> {
        let path = PathBuf::from(log_file);
        
        // Create parent directories if they don't exist
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        
        Ok(Self {
            file_path: path,
            file: Arc::new(Mutex::new(file)),
            enabled: true,
        })
    }
    
    /// Disable logging
    pub fn disable(&mut self) {
        self.enabled = false;
    }
    
    /// Enable logging
    pub fn enable(&mut self) {
        self.enabled = true;
    }
    
    /// Get current timestamp in milliseconds
    fn timestamp_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }
    
    /// Write event to JSON file
    fn write_event(&self, event: &LogEvent) -> Result<(), std::io::Error> {
        if !self.enabled {
            return Ok(());
        }
        
        let json = serde_json::to_string(event)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        
        let mut file = self.file.lock().unwrap();
        writeln!(file, "{}", json)?;
        file.flush()?;
        
        Ok(())
    }
    
    // ============================================================================
    // Order Event Logging
    // ============================================================================
    
    /// Log order creation
    pub fn log_order_created(
        &self,
        symbol: &str,
        order_id: &str,
        side: Side,
        price: Px,
        qty: Qty,
        reason: &str,
        tif: &str,
    ) {
        let notional = price.0.to_f64().unwrap_or(0.0) * qty.0.to_f64().unwrap_or(0.0);
        let event = LogEvent::OrderCreated {
            timestamp: Self::timestamp_ms(),
            symbol: symbol.to_string(),
            order_id: order_id.to_string(),
            side: format!("{:?}", side),
            price: price.0.to_f64().unwrap_or(0.0),
            quantity: qty.0.to_f64().unwrap_or(0.0),
            notional_usd: notional,
            reason: reason.to_string(),
            tif: tif.to_string(),
        };
        
        if let Err(e) = self.write_event(&event) {
            eprintln!("Failed to log order_created: {}", e);
        }
    }
    
    /// Log order fill
    pub fn log_order_filled(
        &self,
        symbol: &str,
        order_id: &str,
        side: Side,
        price: Px,
        qty: Qty,
        is_maker: bool,
        new_inventory: Qty,
        fill_rate: f64,
    ) {
        let notional = price.0.to_f64().unwrap_or(0.0) * qty.0.to_f64().unwrap_or(0.0);
        let event = LogEvent::OrderFilled {
            timestamp: Self::timestamp_ms(),
            symbol: symbol.to_string(),
            order_id: order_id.to_string(),
            side: format!("{:?}", side),
            price: price.0.to_f64().unwrap_or(0.0),
            quantity: qty.0.to_f64().unwrap_or(0.0),
            notional_usd: notional,
            is_maker,
            new_inventory: new_inventory.0.to_f64().unwrap_or(0.0),
            fill_rate,
        };
        
        if let Err(e) = self.write_event(&event) {
            eprintln!("Failed to log order_filled: {}", e);
        }
    }
    
    /// Log order cancellation
    pub fn log_order_canceled(
        &self,
        symbol: &str,
        order_id: &str,
        reason: &str,
        fill_rate: f64,
    ) {
        let event = LogEvent::OrderCanceled {
            timestamp: Self::timestamp_ms(),
            symbol: symbol.to_string(),
            order_id: order_id.to_string(),
            reason: reason.to_string(),
            fill_rate,
        };
        
        if let Err(e) = self.write_event(&event) {
            eprintln!("Failed to log order_canceled: {}", e);
        }
    }
    
    // ============================================================================
    // Position Event Logging
    // ============================================================================
    
    /// Log position opened
    pub fn log_position_opened(
        &self,
        symbol: &str,
        side: &str, // "long" or "short"
        entry_price: Px,
        qty: Qty,
        leverage: u32,
        reason: &str,
    ) {
        let notional = entry_price.0.to_f64().unwrap_or(0.0) * qty.0.to_f64().unwrap_or(0.0).abs();
        let event = LogEvent::PositionOpened {
            timestamp: Self::timestamp_ms(),
            symbol: symbol.to_string(),
            side: side.to_string(),
            entry_price: entry_price.0.to_f64().unwrap_or(0.0),
            quantity: qty.0.to_f64().unwrap_or(0.0),
            notional_usd: notional,
            leverage,
            reason: reason.to_string(),
        };
        
        if let Err(e) = self.write_event(&event) {
            eprintln!("Failed to log position_opened: {}", e);
        }
    }
    
    /// Log position update
    pub fn log_position_updated(
        &self,
        symbol: &str,
        side: &str,
        entry_price: Px,
        qty: Qty,
        mark_price: Px,
        leverage: u32,
    ) {
        let notional = entry_price.0.to_f64().unwrap_or(0.0) * qty.0.to_f64().unwrap_or(0.0).abs();
        let qty_f = qty.0.to_f64().unwrap_or(0.0);
        let entry_f = entry_price.0.to_f64().unwrap_or(0.0);
        let mark_f = mark_price.0.to_f64().unwrap_or(0.0);
        
        let unrealized_pnl = (mark_f - entry_f) * qty_f;
        let unrealized_pnl_pct = if entry_f > 0.0 {
            ((mark_f - entry_f) / entry_f) * 100.0
        } else {
            0.0
        };
        
        let event = LogEvent::PositionUpdated {
            timestamp: Self::timestamp_ms(),
            symbol: symbol.to_string(),
            side: side.to_string(),
            entry_price: entry_f,
            quantity: qty_f,
            mark_price: mark_f,
            notional_usd: notional,
            unrealized_pnl,
            unrealized_pnl_pct,
            leverage,
        };
        
        if let Err(e) = self.write_event(&event) {
            eprintln!("Failed to log position_updated: {}", e);
        }
    }
    
    /// Log position closed
    pub fn log_position_closed(
        &self,
        symbol: &str,
        side: &str,
        entry_price: Px,
        exit_price: Px,
        qty: Qty,
        leverage: u32,
        reason: &str,
    ) {
        let qty_f = qty.0.to_f64().unwrap_or(0.0);
        let entry_f = entry_price.0.to_f64().unwrap_or(0.0);
        let exit_f = exit_price.0.to_f64().unwrap_or(0.0);
        
        let realized_pnl = (exit_f - entry_f) * qty_f;
        let realized_pnl_pct = if entry_f > 0.0 {
            ((exit_f - entry_f) / entry_f) * 100.0
        } else {
            0.0
        };
        
        let event = LogEvent::PositionClosed {
            timestamp: Self::timestamp_ms(),
            symbol: symbol.to_string(),
            side: side.to_string(),
            entry_price: entry_f,
            exit_price: exit_f,
            quantity: qty_f,
            realized_pnl,
            realized_pnl_pct,
            leverage,
            reason: reason.to_string(),
        };
        
        if let Err(e) = self.write_event(&event) {
            eprintln!("Failed to log position_closed: {}", e);
        }
    }
    
    // ============================================================================
    // Trade Event Logging
    // ============================================================================
    
    /// Log completed trade
    pub fn log_trade_completed(
        &self,
        symbol: &str,
        side: &str,
        entry_price: Px,
        exit_price: Px,
        qty: Qty,
        fees: f64,
        leverage: u32,
    ) {
        let qty_f = qty.0.to_f64().unwrap_or(0.0);
        let entry_f = entry_price.0.to_f64().unwrap_or(0.0);
        let exit_f = exit_price.0.to_f64().unwrap_or(0.0);
        
        let notional = entry_f * qty_f.abs();
        let realized_pnl = (exit_f - entry_f) * qty_f;
        let realized_pnl_pct = if entry_f > 0.0 {
            ((exit_f - entry_f) / entry_f) * 100.0
        } else {
            0.0
        };
        
        let net_profit = realized_pnl - fees;
        let is_profitable = net_profit > 0.0;
        
        let event = LogEvent::TradeCompleted {
            timestamp: Self::timestamp_ms(),
            symbol: symbol.to_string(),
            side: side.to_string(),
            entry_price: entry_f,
            exit_price: exit_f,
            quantity: qty_f,
            notional_usd: notional,
            realized_pnl,
            realized_pnl_pct,
            fees,
            net_profit,
            is_profitable,
            leverage,
        };
        
        if let Err(e) = self.write_event(&event) {
            eprintln!("Failed to log trade_completed: {}", e);
        }
    }
    
    /// Log rejected trade
    pub fn log_trade_rejected(
        &self,
        symbol: &str,
        reason: &str,
        spread_bps: f64,
        position_size_usd: f64,
        min_spread_bps: f64,
    ) {
        let event = LogEvent::TradeRejected {
            timestamp: Self::timestamp_ms(),
            symbol: symbol.to_string(),
            reason: reason.to_string(),
            spread_bps,
            position_size_usd,
            min_spread_bps,
        };
        
        if let Err(e) = self.write_event(&event) {
            eprintln!("Failed to log trade_rejected: {}", e);
        }
    }
    
    /// Log PnL summary
    pub fn log_pnl_summary(
        &self,
        period: &str,
        trade_count: u32,
        profitable_trade_count: u32,
        losing_trade_count: u32,
        total_profit: f64,
        total_loss: f64,
        net_pnl: f64,
        largest_win: f64,
        largest_loss: f64,
        total_fees: f64,
    ) {
        let event = LogEvent::PnlSummary {
            timestamp: Self::timestamp_ms(),
            period: period.to_string(),
            trade_count,
            profitable_trade_count,
            losing_trade_count,
            total_profit,
            total_loss,
            net_pnl,
            largest_win,
            largest_loss,
            total_fees,
        };
        
        if let Err(e) = self.write_event(&event) {
            eprintln!("Failed to log pnl_summary: {}", e);
        }
    }
}

// Thread-safe logger wrapper
pub type SharedLogger = Arc<Mutex<JsonLogger>>;

/// Create a shared logger instance
pub fn create_logger(log_file: &str) -> Result<SharedLogger, std::io::Error> {
    let logger = JsonLogger::new(log_file)?;
    Ok(Arc::new(Mutex::new(logger)))
}


// ============================================================================
// Event Handler Module (from event_handler.rs)
// ============================================================================

/// Handle WebSocket reconnect sync for all symbols
pub async fn handle_reconnect_sync(
    venue: &BinanceFutures,
    states: &mut [SymbolState],
    cfg: &crate::config::AppCfg,
) {
    for state in states.iter_mut() {
        let current_pos = <BinanceFutures as Venue>::get_position(venue, &state.meta.symbol).await.ok();
        
        rate_limit_guard(3).await;
        if let Ok(api_orders) = <BinanceFutures as Venue>::get_open_orders(venue, &state.meta.symbol).await {
            let api_order_ids: std::collections::HashSet<String> = api_orders
                .iter()
                .map(|o| o.order_id.clone())
                .collect();
            
            let mut removed_orders = Vec::new();
            state.active_orders.retain(|order_id, order_info| {
                if !api_order_ids.contains(order_id) {
                    removed_orders.push(order_info.clone());
                    false
                } else {
                    true
                }
            });
            
            if !removed_orders.is_empty() {
                if let Some(pos) = current_pos {
                    let old_inv = state.inv.0;
                    state.inv = Qty(pos.qty.0);
                    state.last_inventory_update = Some(std::time::Instant::now());
                    
                    if old_inv != pos.qty.0 {
                        state.consecutive_no_fills = 0;
                        state.order_fill_rate = (state.order_fill_rate * 0.95 + 0.05).min(1.0);
                        info!(
                            symbol = %state.meta.symbol,
                            removed_orders = removed_orders.len(),
                            inv_change = %(pos.qty.0 - old_inv),
                            "reconnect sync: orders removed and inventory changed - likely filled"
                        );
                    } else {
                        update_fill_rate_on_cancel(state, cfg.internal.fill_rate_decrease_factor);
                        info!(
                            symbol = %state.meta.symbol,
                            removed_orders = removed_orders.len(),
                            "reconnect sync: orders removed but inventory unchanged - likely canceled"
                        );
                    }
                } else {
                    state.consecutive_no_fills = 0;
                    state.order_fill_rate = (state.order_fill_rate * cfg.internal.fill_rate_reconnect_factor + cfg.internal.fill_rate_reconnect_bonus).min(1.0);
                    warn!(
                        symbol = %state.meta.symbol,
                        removed_orders = removed_orders.len(),
                        "reconnect sync: orders removed but position unavailable, assuming filled"
                    );
                }
            }
        } else {
            warn!(symbol = %state.meta.symbol, "failed to sync orders after reconnect");
        }
    }
}

/// Handle order fill event with deduplication
pub fn handle_order_fill(
    state: &mut SymbolState,
    symbol: &str,
    order_id: &str,
    cumulative_filled_qty: Qty,
    _order_status: &str,
) -> bool {
    // Event ID bazlı duplicate kontrolü
    let event_timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let event_id = format!("{}-{}-{}", order_id, cumulative_filled_qty.0, event_timestamp);
    
    if state.processed_events.contains(&event_id) {
        warn!(
            %symbol,
            order_id = %order_id,
            cumulative_filled_qty = %cumulative_filled_qty.0,
            "duplicate fill event ignored"
        );
        return false;
    }
    
    // Legacy duplicate check
    let is_duplicate = state.active_orders.get(order_id)
        .map(|o| o.filled_qty.0 >= cumulative_filled_qty.0)
        .unwrap_or(false);
    
    if is_duplicate {
        warn!(
            %symbol,
            order_id = %order_id,
            "duplicate fill event ignored (legacy check)"
        );
        return false;
    }
    
    // Event ID'yi kaydet ve memory leak önle
    state.processed_events.insert(event_id);
    if state.processed_events.len() > 1000 {
        if state.last_event_cleanup
            .map(|t| t.elapsed().as_secs() > 3600)
            .unwrap_or(true)
        {
            state.processed_events.clear();
            state.last_event_cleanup = Some(std::time::Instant::now());
        } else if state.last_event_cleanup.is_none() {
            state.last_event_cleanup = Some(std::time::Instant::now());
        }
    }
    
    true
}


