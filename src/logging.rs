
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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tracing::{info, warn};
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event_type")]
pub enum LogEvent {
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
        cancellation_duration_ms: Option<u64>, // Time from order creation to cancellation
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
}
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
            .unwrap_or_else(|_| {
                warn!("System time is before UNIX epoch, using fallback timestamp");
                Duration::from_secs(0)
            })
            .as_millis() as u64
    }
    fn instant_to_timestamp_ms(instant: Instant) -> u64 {
        // Convert Instant to SystemTime by using elapsed time from a reference point
        // This is approximate but sufficient for logging purposes
        let now = SystemTime::now();
        let elapsed = instant.elapsed();
        now.duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| Duration::from_secs(0))
            .checked_sub(elapsed)
            .unwrap_or_else(|| Duration::from_secs(0))
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
        event_timestamp: Option<Instant>,
    ) {
        let notional = price.0.to_f64().unwrap_or(0.0) * qty.0.to_f64().unwrap_or(0.0);
        let timestamp_ms = event_timestamp
            .map(|ts| Self::instant_to_timestamp_ms(ts))
            .unwrap_or_else(|| Self::timestamp_ms());
        let event = LogEvent::OrderFilled {
            timestamp: timestamp_ms,
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
    pub fn log_order_canceled(&self, symbol: &str, order_id: &str, reason: &str, fill_rate: f64, event_timestamp: Option<Instant>, cancellation_duration_ms: Option<u64>) {
        let timestamp_ms = event_timestamp
            .map(|ts| Self::instant_to_timestamp_ms(ts))
            .unwrap_or_else(|| Self::timestamp_ms());
        let event = LogEvent::OrderCanceled {
            timestamp: timestamp_ms,
            symbol: symbol.to_string(),
            order_id: order_id.to_string(),
            reason: reason.to_string(),
            fill_rate,
            cancellation_duration_ms,
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
        event_timestamp: Option<Instant>,
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
        let timestamp_ms = event_timestamp
            .map(|ts| Self::instant_to_timestamp_ms(ts))
            .unwrap_or_else(|| Self::timestamp_ms());
        let event = LogEvent::PositionUpdated {
            timestamp: timestamp_ms,
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
}
pub type SharedLogger = Arc<JsonLogger>;
pub fn create_logger(
    log_file: &str,
) -> Result<(SharedLogger, tokio::task::JoinHandle<()>), std::io::Error> {
    let (logger, task_handle) = JsonLogger::new(log_file)?;
    Ok((Arc::new(logger), task_handle))
}
pub struct Logging {
    event_bus: Arc<EventBus>,
    json_logger: SharedLogger,
    shutdown_flag: Arc<AtomicBool>,
}
impl Logging {
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
    pub async fn start(&self) -> Result<()> {
        let event_bus = self.event_bus.clone();
        let json_logger = self.json_logger.clone();
        let shutdown_flag = self.shutdown_flag.clone();
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
                        let signal_age_ms = signal.timestamp.elapsed().as_millis();
                        info!(
                            symbol = %signal.symbol,
                            side = ?signal.side,
                            entry_price = %signal.entry_price.0,
                            signal_age_ms = signal_age_ms,
                            "LOGGING: TradeSignal received"
                        );
                    }
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        warn!(
                            missed_events = missed,
                            "LOGGING: TradeSignal receiver lagged, {} events missed",
                            missed
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        warn!("LOGGING: TradeSignal channel closed");
                        break;
                    }
                }
            }
        });
        let json_logger_order = json_logger.clone();
        let event_bus_order = event_bus.clone();
        let shutdown_flag_order = shutdown_flag.clone();
        tokio::spawn(async move {
            use std::collections::HashMap;
            use tokio::sync::RwLock;
            let order_creation_times_local: Arc<RwLock<HashMap<String, Instant>>> = Arc::new(RwLock::new(HashMap::new()));
            let mut order_update_rx = event_bus_order.subscribe_order_update();
            info!("LOGGING: Started, listening to OrderUpdate events");
            loop {
                match order_update_rx.recv().await {
                    Ok(update) => {
                        if shutdown_flag_order.load(AtomicOrdering::Relaxed) {
                            break;
                        }
                        match update.status {
                            crate::event_bus::OrderStatus::New => {
                                // Track order creation time
                                let mut times = order_creation_times_local.write().await;
                                times.insert(update.order_id.clone(), update.timestamp);
                                info!(
                                    symbol = %update.symbol,
                                    order_id = %update.order_id,
                                    "LOGGING: Order created (New status)"
                                );
                            }
                            crate::event_bus::OrderStatus::Filled => {
                                // Clean up order creation time tracking
                                let mut times = order_creation_times_local.write().await;
                                times.remove(&update.order_id);
                                json_logger_order.log_order_filled(
                                    &update.symbol,
                                    &update.order_id,
                                    update.side,
                                    update.average_fill_price,
                                    update.filled_qty,
                                    false,
                                    update.remaining_qty,
                                    1.0,
                                    Some(update.timestamp),
                                );
                            }
                            crate::event_bus::OrderStatus::Canceled => {
                                // Calculate cancellation duration
                                let times = order_creation_times_local.read().await;
                                let order_creation_time = times.get(&update.order_id);
                                let cancellation_duration_ms = order_creation_time
                                    .map(|creation_time| {
                                        update.timestamp.duration_since(*creation_time).as_millis() as u64
                                    });
                                drop(times);
                                let mut times = order_creation_times_local.write().await;
                                times.remove(&update.order_id);
                                
                                // Log order cancellation with cancellation duration
                                json_logger_order.log_order_canceled(
                                    &update.symbol,
                                    &update.order_id,
                                    "Order canceled",
                                    1.0,
                                    Some(update.timestamp),
                                    cancellation_duration_ms,
                                );
                                
                                // Log to console with visibility
                                if let Some(duration_ms) = cancellation_duration_ms {
                                    if duration_ms < 5000 {
                                        warn!(
                                            symbol = %update.symbol,
                                            order_id = %update.order_id,
                                            cancellation_duration_ms = duration_ms,
                                            "LOGGING: ⚠️ Order canceled quickly after placement ({}ms) - check ORDERING logs for details",
                                            duration_ms
                                        );
                                    } else {
                                        info!(
                                            symbol = %update.symbol,
                                            order_id = %update.order_id,
                                            cancellation_duration_ms = duration_ms,
                                            "LOGGING: Order canceled (was open for {}ms)",
                                            duration_ms
                                        );
                                    }
                                } else {
                                    info!(
                                        symbol = %update.symbol,
                                        order_id = %update.order_id,
                                        "LOGGING: Order canceled (creation time not tracked)"
                                    );
                                }
                            }
                            crate::event_bus::OrderStatus::Expired | crate::event_bus::OrderStatus::ExpiredInMatch => {
                                // Calculate expiration duration
                                let times = order_creation_times_local.read().await;
                                let order_creation_time = times.get(&update.order_id);
                                let expiration_duration_ms = order_creation_time
                                    .map(|creation_time| {
                                        update.timestamp.duration_since(*creation_time).as_millis() as u64
                                    });
                                drop(times);
                                let mut times = order_creation_times_local.write().await;
                                times.remove(&update.order_id);
                                
                                json_logger_order.log_order_canceled(
                                    &update.symbol,
                                    &update.order_id,
                                    "Order expired",
                                    1.0,
                                    Some(update.timestamp),
                                    expiration_duration_ms,
                                );
                            }
                            crate::event_bus::OrderStatus::Rejected => {
                                // Calculate rejection duration
                                let times = order_creation_times_local.read().await;
                                let order_creation_time = times.get(&update.order_id);
                                let rejection_duration_ms = order_creation_time
                                    .map(|creation_time| {
                                        update.timestamp.duration_since(*creation_time).as_millis() as u64
                                    });
                                drop(times);
                                let mut times = order_creation_times_local.write().await;
                                times.remove(&update.order_id);
                                
                                json_logger_order.log_order_canceled(
                                    &update.symbol,
                                    &update.order_id,
                                    "Order rejected",
                                    1.0,
                                    Some(update.timestamp),
                                    rejection_duration_ms,
                                );
                                
                                if let Some(duration_ms) = rejection_duration_ms {
                                    warn!(
                                        symbol = %update.symbol,
                                        order_id = %update.order_id,
                                        rejection_duration_ms = duration_ms,
                                        rejection_reason = ?update.rejection_reason,
                                        "LOGGING: Order rejected ({}ms after placement)",
                                        duration_ms
                                    );
                                }
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
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        warn!("LOGGING: OrderUpdate channel closed");
                        break;
                    }
                }
            }
        });
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
                                Some(update.timestamp),
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
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        warn!("LOGGING: PositionUpdate channel closed");
                        break;
                    }
                }
            }
        });
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
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        warn!("LOGGING: CloseRequest channel closed");
                        break;
                    }
                }
            }
        });
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
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        warn!("LOGGING: BalanceUpdate channel closed");
                        break;
                    }
                }
            }
        });
        let event_bus_tick = event_bus.clone();
        let shutdown_flag_tick = shutdown_flag.clone();
        tokio::spawn(async move {
            let mut market_tick_rx = event_bus_tick.subscribe_market_tick();
            use std::collections::HashMap;
            use tokio::sync::RwLock;
            let tick_counts: Arc<RwLock<HashMap<String, u64>>> = Arc::new(RwLock::new(HashMap::new()));
            let last_seen: Arc<RwLock<HashMap<String, Instant>>> = Arc::new(RwLock::new(HashMap::new()));
            const LOG_INTERVAL: u64 = 1000;
            const CLEANUP_INTERVAL_SECS: u64 = 3600;
            let tick_counts_cleanup = tick_counts.clone();
            let last_seen_cleanup = last_seen.clone();
            let shutdown_flag_cleanup = shutdown_flag_tick.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(CLEANUP_INTERVAL_SECS)).await;
                    if shutdown_flag_cleanup.load(AtomicOrdering::Relaxed) {
                        break;
                    }
                    let now = Instant::now();
                    let mut counts = tick_counts_cleanup.write().await;
                    let mut last_seen_guard = last_seen_cleanup.write().await;
                    counts.retain(|symbol, _| {
                        last_seen_guard.get(symbol)
                            .map(|ts| now.duration_since(*ts).as_secs() < CLEANUP_INTERVAL_SECS)
                            .unwrap_or(false)
                    });
                    last_seen_guard.retain(|_, ts| {
                        now.duration_since(*ts).as_secs() < CLEANUP_INTERVAL_SECS
                    });
                }
            });
            loop {
                match market_tick_rx.recv().await {
                    Ok(tick) => {
                        if shutdown_flag_tick.load(AtomicOrdering::Relaxed) {
                            break;
                        }
                        {
                            let mut last_seen_guard = last_seen.write().await;
                            last_seen_guard.insert(tick.symbol.clone(), Instant::now());
                        }
                        let should_log = {
                            let mut counts = tick_counts.write().await;
                            let count = counts.entry(tick.symbol.clone()).or_insert(0);
                            *count += 1;
                            if *count >= LOG_INTERVAL {
                                *count = 0;
                                true
                            } else {
                                false
                            }
                        };
                        if should_log {
                            let tick_age_ms = tick.timestamp.elapsed().as_millis();
                            info!(
                                symbol = %tick.symbol,
                                bid = %tick.bid.0,
                                ask = %tick.ask.0,
                                tick_age_ms = tick_age_ms,
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
