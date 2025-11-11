//location: /crates/app/src/logger.rs
// Structured JSON logging system for trading events

use crate::types::*;
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
    #[serde(rename = "pnl_summary")]
    PnLSummary {
        timestamp: u64,
        period: String, // "daily", "hourly", etc.
        total_trades: u64,
        profitable_trades: u64,
        losing_trades: u64,
        total_profit: f64,
        total_loss: f64,
        net_pnl: f64,
        win_rate: f64,
        avg_profit_per_trade: f64,
        avg_loss_per_trade: f64,
        largest_win: f64,
        largest_loss: f64,
        total_fees: f64,
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
    
    /// Log PnL summary (daily/hourly)
    pub fn log_pnl_summary(
        &self,
        period: &str,
        total_trades: u64,
        profitable_trades: u64,
        losing_trades: u64,
        total_profit: f64,
        total_loss: f64,
        net_pnl: f64,
        largest_win: f64,
        largest_loss: f64,
        total_fees: f64,
    ) {
        let win_rate = if total_trades > 0 {
            profitable_trades as f64 / total_trades as f64
        } else {
            0.0
        };
        
        let avg_profit_per_trade = if profitable_trades > 0 {
            total_profit / profitable_trades as f64
        } else {
            0.0
        };
        
        let avg_loss_per_trade = if losing_trades > 0 {
            total_loss / losing_trades as f64
        } else {
            0.0
        };
        
        let event = LogEvent::PnLSummary {
            timestamp: Self::timestamp_ms(),
            period: period.to_string(),
            total_trades,
            profitable_trades,
            losing_trades,
            total_profit,
            total_loss,
            net_pnl,
            win_rate,
            avg_profit_per_trade,
            avg_loss_per_trade,
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

