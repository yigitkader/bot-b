// LOGGING: Event logging module
// Listens to all events from event bus and logs them
// Includes JsonLogger for structured JSON logging

use crate::event_bus::EventBus;
use crate::types::*;
use anyhow::Result;
use rust_decimal::prelude::ToPrimitive;
use serde::Serialize;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tracing::{info, warn};

// ============================================================================
// JsonLogger Implementation (from logger.rs)
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
        period: String,
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

/// Async-safe logger - channel-based, non-blocking
pub struct JsonLogger {
    event_tx: mpsc::Sender<LogEvent>,
}

impl JsonLogger {
    pub fn new(log_file: &str) -> Result<(Self, tokio::task::JoinHandle<()>), std::io::Error> {
        let path = PathBuf::from(log_file);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let file_path = path.clone();
        const CHANNEL_BUFFER_SIZE: usize = 1000;
        let (event_tx, mut event_rx) = mpsc::channel::<LogEvent>(CHANNEL_BUFFER_SIZE);

        let task_handle = tokio::spawn(async move {
            let mut file = match OpenOptions::new()
                .create(true)
                .append(true)
                .open(&file_path)
            {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("Failed to open log file {}: {}", file_path.display(), e);
                    return;
                }
            };

            while let Some(event) = event_rx.recv().await {
                match serde_json::to_string(&event) {
                    Ok(json) => {
                        if let Err(e) = writeln!(file, "{}", json) {
                            eprintln!("Failed to write log event: {}", e);
                        } else if let Err(e) = file.flush() {
                            eprintln!("Failed to flush log file: {}", e);
                        }
                    }
                    Err(e) => {
                        eprintln!("Failed to serialize log event: {}", e);
                    }
                }
            }

            let _ = file.flush();
        });

        Ok((Self { event_tx }, task_handle))
    }

    fn timestamp_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    fn send_event(&self, event: LogEvent) {
        match self.event_tx.try_send(event) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {}
            Err(mpsc::error::TrySendError::Closed(_)) => {
                eprintln!("Failed to send log event: channel closed");
            }
        }
    }

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
        self.send_event(event);
    }

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
        self.send_event(event);
    }

    pub fn log_order_canceled(&self, symbol: &str, order_id: &str, reason: &str, fill_rate: f64) {
        let event = LogEvent::OrderCanceled {
            timestamp: Self::timestamp_ms(),
            symbol: symbol.to_string(),
            order_id: order_id.to_string(),
            reason: reason.to_string(),
            fill_rate,
        };
        self.send_event(event);
    }

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
        self.send_event(event);
    }

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
        self.send_event(event);
    }

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
        self.send_event(event);
    }

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
        self.send_event(event);
    }

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
        self.send_event(event);
    }
}

pub type SharedLogger = Arc<JsonLogger>;

/// Create a shared logger instance with background task
pub fn create_logger(
    log_file: &str,
) -> Result<(SharedLogger, tokio::task::JoinHandle<()>), std::io::Error> {
    let (logger, task_handle) = JsonLogger::new(log_file)?;
    Ok((Arc::new(logger), task_handle))
}

// ============================================================================
// LOGGING Module - Event Bus Listener
// ============================================================================

/// LOGGING module - logs all events from event bus
pub struct Logging {
    event_bus: Arc<EventBus>,
    json_logger: SharedLogger,
    shutdown_flag: Arc<AtomicBool>,
}

impl Logging {
    /// Create a new Logging module instance.
    ///
    /// The Logging module subscribes to all events from the event bus and logs them using
    /// structured JSON logging. It runs in the background and does not block other modules.
    ///
    /// # Arguments
    ///
    /// * `event_bus` - Event bus for subscribing to all event types
    /// * `json_logger` - Shared JSON logger instance for structured logging
    /// * `shutdown_flag` - Shared flag to signal graceful shutdown
    ///
    /// # Returns
    ///
    /// Returns a new `Logging` instance. Call `start()` to begin logging events.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # let event_bus = Arc::new(crate::event_bus::EventBus::new());
    /// # let (json_logger, _) = crate::logging::create_logger("logs/events.json")?;
    /// # let shutdown_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    /// let logging = Logging::new(event_bus, json_logger, shutdown_flag);
    /// logging.start().await?;
    /// ```
    pub fn new(
        event_bus: Arc<EventBus>,
        json_logger: SharedLogger,
        shutdown_flag: Arc<AtomicBool>,
    ) -> Self {
        Self {
            event_bus,
            json_logger,
            shutdown_flag,
        }
    }

    /// Start the logging service and begin logging all events.
    ///
    /// This method spawns background tasks that subscribe to all event types and log them:
    /// - TradeSignal events
    /// - OrderUpdate events (with structured JSON logging)
    /// - PositionUpdate events
    /// - CloseRequest events
    /// - BalanceUpdate events
    /// - MarketTick events (throttled to reduce log volume)
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` immediately after spawning background tasks. Tasks will continue
    /// running until `shutdown_flag` is set to true.
    ///
    /// # Behavior
    ///
    /// - All events are logged with structured JSON format
    /// - MarketTick events are throttled (logged every 1000 ticks per symbol) to reduce volume
    /// - Handles broadcast channel lagging gracefully (logs warnings when events are missed)
    ///
    /// # Example
    ///
    /// ```no_run
    /// # let logging = crate::logging::Logging::new(todo!(), todo!(), todo!());
    /// logging.start().await?;
    /// // Logging service is now active
    /// ```
    pub async fn start(&self) -> Result<()> {
        let event_bus = self.event_bus.clone();
        let json_logger = self.json_logger.clone();
        let shutdown_flag = self.shutdown_flag.clone();
        
        // Spawn task for TradeSignal events
        let event_bus_trade = event_bus.clone();
        let shutdown_flag_trade = shutdown_flag.clone();
        tokio::spawn(async move {
            let mut trade_signal_rx = event_bus_trade.subscribe_trade_signal();
            
            info!("LOGGING: Started, listening to TradeSignal events");
            
            loop {
                match trade_signal_rx.recv().await {
                    Ok(signal) => {
                        if shutdown_flag_trade.load(AtomicOrdering::Relaxed) {
                            break;
                        }
                        
                        info!(
                            symbol = %signal.symbol,
                            side = ?signal.side,
                            entry_price = %signal.entry_price.0,
                            size = %signal.size.0,
                            leverage = signal.leverage,
                            "LOGGING: TradeSignal received"
                        );
                    }
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        warn!(
                            missed_events = missed,
                            "LOGGING: TradeSignal receiver lagged, {} events missed",
                            missed
                        );
                        // Continue processing - don't break on lag
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        warn!("LOGGING: TradeSignal channel closed");
                        break;
                    }
                }
            }
        });
        
        // Spawn task for OrderUpdate events
        let json_logger_order = json_logger.clone();
        let event_bus_order = event_bus.clone();
        let shutdown_flag_order = shutdown_flag.clone();
        tokio::spawn(async move {
            let mut order_update_rx = event_bus_order.subscribe_order_update();
            
            info!("LOGGING: Started, listening to OrderUpdate events");
            
            loop {
                match order_update_rx.recv().await {
                    Ok(update) => {
                        if shutdown_flag_order.load(AtomicOrdering::Relaxed) {
                            break;
                        }
                        
                        match update.status {
                            crate::event_bus::OrderStatus::Filled => {
                                // log_order_filled parameters:
                                // - qty: filled quantity (cumulative filled qty)
                                // - new_inventory: remaining quantity (remaining_qty)
                                // - price: average fill price (weighted average of all fills)
                                json_logger_order.log_order_filled(
                                    &update.symbol,
                                    &update.order_id,
                                    update.side,
                                    update.average_fill_price, // Average fill price (weighted average)
                                    update.filled_qty, // Cumulative filled qty
                                    false,
                                    update.remaining_qty, // Remaining qty (not order_qty)
                                    1.0,
                                );
                            }
                            crate::event_bus::OrderStatus::Canceled => {
                                json_logger_order.log_order_canceled(
                                    &update.symbol,
                                    &update.order_id,
                                    "Order canceled",
                                    1.0,
                                );
                            }
                            crate::event_bus::OrderStatus::Expired | crate::event_bus::OrderStatus::ExpiredInMatch => {
                                json_logger_order.log_order_canceled(
                                    &update.symbol,
                                    &update.order_id,
                                    "Order expired",
                                    1.0,
                                );
                            }
                            crate::event_bus::OrderStatus::Rejected => {
                                json_logger_order.log_order_canceled(
                                    &update.symbol,
                                    &update.order_id,
                                    "Order rejected",
                                    1.0,
                                );
                            }
                            _ => {
                                info!(
                                    symbol = %update.symbol,
                                    order_id = %update.order_id,
                                    status = ?update.status,
                                    "LOGGING: OrderUpdate received"
                                );
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        warn!(
                            missed_events = missed,
                            "LOGGING: OrderUpdate receiver lagged, {} events missed (log gap possible)",
                            missed
                        );
                        // Continue processing - don't break on lag
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        warn!("LOGGING: OrderUpdate channel closed");
                        break;
                    }
                }
            }
        });
        
        // Spawn task for PositionUpdate events
        let json_logger_pos = json_logger.clone();
        let event_bus_pos = event_bus.clone();
        let shutdown_flag_pos = shutdown_flag.clone();
        tokio::spawn(async move {
            let mut position_update_rx = event_bus_pos.subscribe_position_update();
            
            info!("LOGGING: Started, listening to PositionUpdate events");
            
            loop {
                match position_update_rx.recv().await {
                    Ok(update) => {
                        if shutdown_flag_pos.load(AtomicOrdering::Relaxed) {
                            break;
                        }
                        
                        if update.is_open {
                            let side_str = if update.qty.0.is_sign_positive() {
                                "Buy"
                            } else {
                                "Sell"
                            };
                            
                            json_logger_pos.log_position_updated(
                                &update.symbol,
                                side_str,
                                update.entry_price,
                                update.qty,
                                update.entry_price,
                                update.leverage,
                            );
                        } else {
                            info!(
                                symbol = %update.symbol,
                                "LOGGING: Position closed"
                            );
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        warn!(
                            missed_events = missed,
                            "LOGGING: PositionUpdate receiver lagged, {} events missed (log gap possible)",
                            missed
                        );
                        // Continue processing - don't break on lag
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        warn!("LOGGING: PositionUpdate channel closed");
                        break;
                    }
                }
            }
        });
        
        // Spawn task for CloseRequest events
        let event_bus_close = event_bus.clone();
        let shutdown_flag_close = shutdown_flag.clone();
        tokio::spawn(async move {
            let mut close_request_rx = event_bus_close.subscribe_close_request();
            
            info!("LOGGING: Started, listening to CloseRequest events");
            
            loop {
                match close_request_rx.recv().await {
                    Ok(request) => {
                        if shutdown_flag_close.load(AtomicOrdering::Relaxed) {
                            break;
                        }
                        
                        info!(
                            symbol = %request.symbol,
                            reason = ?request.reason,
                            "LOGGING: CloseRequest received"
                        );
                    }
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        warn!(
                            missed_events = missed,
                            "LOGGING: CloseRequest receiver lagged, {} events missed",
                            missed
                        );
                        // Continue processing - don't break on lag
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        warn!("LOGGING: CloseRequest channel closed");
                        break;
                    }
                }
            }
        });
        
        // Spawn task for BalanceUpdate events
        let event_bus_balance = event_bus.clone();
        let shutdown_flag_balance = shutdown_flag.clone();
        tokio::spawn(async move {
            let mut balance_update_rx = event_bus_balance.subscribe_balance_update();
            
            info!("LOGGING: Started, listening to BalanceUpdate events");
            
            loop {
                match balance_update_rx.recv().await {
                    Ok(update) => {
                        if shutdown_flag_balance.load(AtomicOrdering::Relaxed) {
                            break;
                        }
                        
                        info!(
                            usdt = %update.usdt,
                            usdc = %update.usdc,
                            "LOGGING: BalanceUpdate received"
                        );
                    }
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        warn!(
                            missed_events = missed,
                            "LOGGING: BalanceUpdate receiver lagged, {} events missed",
                            missed
                        );
                        // Continue processing - don't break on lag
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        warn!("LOGGING: BalanceUpdate channel closed");
                        break;
                    }
                }
            }
        });
        
        // Spawn task for MarketTick events
        // NOTE: MarketTick events are very frequent (hundreds per second per symbol)
        // We use per-symbol counters and higher threshold to reduce log pollution
        let event_bus_tick = event_bus.clone();
        let shutdown_flag_tick = shutdown_flag.clone();
        tokio::spawn(async move {
            let mut market_tick_rx = event_bus_tick.subscribe_market_tick();
            use std::collections::HashMap;
            use tokio::sync::RwLock;
            let tick_counts: Arc<RwLock<HashMap<String, u64>>> = Arc::new(RwLock::new(HashMap::new()));
            const LOG_INTERVAL: u64 = 1000; // Log every 1000 ticks per symbol (much less frequent)
            
            loop {
                match market_tick_rx.recv().await {
                    Ok(tick) => {
                        if shutdown_flag_tick.load(AtomicOrdering::Relaxed) {
                            break;
                        }
                        
                        // Per-symbol counter to avoid log spam with multiple symbols
                        let should_log = {
                            let mut counts = tick_counts.write().await;
                            let count = counts.entry(tick.symbol.clone()).or_insert(0);
                            *count += 1;
                            if *count >= LOG_INTERVAL {
                                *count = 0; // Reset counter
                                true
                            } else {
                                false
                            }
                        };
                        
                        if should_log {
                            info!(
                                symbol = %tick.symbol,
                                bid = %tick.bid.0,
                                ask = %tick.ask.0,
                                "LOGGING: MarketTick (logged every {} ticks per symbol)",
                                LOG_INTERVAL
                            );
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        warn!(
                            missed_events = missed,
                            "LOGGING: MarketTick receiver lagged, {} events missed (log gap possible)",
                            missed
                        );
                        // Continue processing - don't break on lag
                        // MarketTick events are very frequent, so lagging is more common
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        warn!("LOGGING: MarketTick channel closed");
                        break;
                    }
                }
            }
        });
        
        Ok(())
    }
}
