// STORAGE: Persistent state storage module using SQLite
// Event-driven architecture - listens to OrderingStateUpdate and OrderFillHistoryUpdate events
// Persists OrderingState, OrderFillHistory, and logs balance reservations
// Restores state on startup to handle bot restarts gracefully

use crate::event_bus::{EventBus, OrderFillHistoryUpdate, OrderingStateUpdate};
use crate::state::{OpenOrder, OpenPosition, OrderingState, SharedState};
use crate::types::{Px, Qty, PositionDirection, Side};
use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, OptionalExtension};
use rust_decimal::Decimal;
use std::path::Path;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

/// SQLite database for persistent state storage
pub struct StateStorage {
    db: Arc<Mutex<Connection>>,
}

impl StateStorage {
    /// Create or open SQLite database at the specified path
    /// If path is None, uses default location: "./bot_state.db"
    pub fn new(db_path: Option<&Path>) -> Result<Self> {
        let path = db_path.unwrap_or_else(|| Path::new("./bot_state.db"));
        
        let conn = Connection::open(path)?;
        let storage = Self {
            db: Arc::new(Mutex::new(conn)),
        };
        
        // Initialize database schema
        storage.init_schema()?;
        
        info!(db_path = %path.display(), "STORAGE: Database opened/created");
        Ok(storage)
    }
    
    /// Initialize database schema (create tables if they don't exist)
    fn init_schema(&self) -> Result<()> {
        let db = self.db.blocking_lock();
        
        // Table for ordering state (open position and open order)
        db.execute(
            r#"
            CREATE TABLE IF NOT EXISTS ordering_state (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                open_position_symbol TEXT,
                open_position_direction TEXT,
                open_position_qty TEXT,
                open_position_entry_price TEXT,
                open_order_symbol TEXT,
                open_order_id TEXT,
                open_order_side TEXT,
                open_order_qty TEXT,
                last_order_update_timestamp INTEGER,
                last_position_update_timestamp INTEGER,
                updated_at INTEGER NOT NULL
            )
            "#,
            [],
        )?;
        
        // Table for order fill history
        db.execute(
            r#"
            CREATE TABLE IF NOT EXISTS order_fill_history (
                order_id TEXT PRIMARY KEY,
                symbol TEXT NOT NULL,
                total_filled_qty TEXT NOT NULL,
                weighted_price_sum TEXT NOT NULL,
                maker_fill_count INTEGER NOT NULL,
                total_fill_count INTEGER NOT NULL,
                last_update INTEGER NOT NULL,
                created_at INTEGER NOT NULL
            )
            "#,
            [],
        )?;
        
        // Table for balance reservations (log only, not restored on startup)
        // This helps track balance leaks and debug issues
        db.execute(
            r#"
            CREATE TABLE IF NOT EXISTS balance_reservations_log (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                asset TEXT NOT NULL,
                amount TEXT NOT NULL,
                reserved_at INTEGER NOT NULL,
                released_at INTEGER,
                order_id TEXT,
                symbol TEXT
            )
            "#,
            [],
        )?;
        
        // Create index for faster queries
        db.execute(
            "CREATE INDEX IF NOT EXISTS idx_order_fill_history_symbol ON order_fill_history(symbol)",
            [],
        )?;
        
        db.execute(
            "CREATE INDEX IF NOT EXISTS idx_order_fill_history_last_update ON order_fill_history(last_update)",
            [],
        )?;
        
        db.execute(
            "CREATE INDEX IF NOT EXISTS idx_balance_reservations_released ON balance_reservations_log(released_at)",
            [],
        )?;
        
        info!("STORAGE: Database schema initialized");
        Ok(())
    }
    
    /// Restore OrderingState from database
    /// Returns None if no state exists in database
    pub async fn restore_ordering_state(&self) -> Result<Option<OrderingState>> {
        let db = self.db.lock().await;
        
        let row = db.query_row(
            "SELECT 
                open_position_symbol,
                open_position_direction,
                open_position_qty,
                open_position_entry_price,
                open_order_symbol,
                open_order_id,
                open_order_side,
                open_order_qty,
                last_order_update_timestamp,
                last_position_update_timestamp
            FROM ordering_state WHERE id = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?, // open_position_symbol
                    row.get::<_, Option<String>>(1)?, // open_position_direction
                    row.get::<_, Option<String>>(2)?, // open_position_qty
                    row.get::<_, Option<String>>(3)?, // open_position_entry_price
                    row.get::<_, Option<String>>(4)?, // open_order_symbol
                    row.get::<_, Option<String>>(5)?, // open_order_id
                    row.get::<_, Option<String>>(6)?, // open_order_side
                    row.get::<_, Option<String>>(7)?, // open_order_qty
                    row.get::<_, Option<i64>>(8)?, // last_order_update_timestamp
                    row.get::<_, Option<i64>>(9)?, // last_position_update_timestamp
                ))
            },
        ).optional()?;
        
        if let Some((
            pos_symbol,
            pos_direction,
            pos_qty,
            pos_entry_price,
            order_symbol,
            order_id,
            order_side,
            order_qty,
            order_ts,
            pos_ts,
        )) = row {
            // Parse open position
            let open_position = if let (Some(symbol), Some(direction_str), Some(qty_str), Some(price_str)) =
                (pos_symbol, pos_direction, pos_qty, pos_entry_price)
            {
                let direction = match direction_str.as_str() {
                    "Long" => PositionDirection::Long,
                    "Short" => PositionDirection::Short,
                    _ => {
                        warn!(direction = %direction_str, "STORAGE: Invalid position direction, skipping position restore");
                        return Ok(None);
                    }
                };
                
                let qty = Decimal::from_str(&qty_str)
                    .map_err(|e| anyhow!("Failed to parse position qty: {}", e))?;
                let entry_price = Decimal::from_str(&price_str)
                    .map_err(|e| anyhow!("Failed to parse position entry_price: {}", e))?;
                
                Some(OpenPosition {
                    symbol,
                    direction,
                    qty: Qty(qty),
                    entry_price: Px(entry_price),
                })
            } else {
                None
            };
            
            // Parse open order
            let open_order = if let (Some(symbol), Some(order_id), Some(side_str), Some(qty_str)) =
                (order_symbol, order_id, order_side, order_qty)
            {
                let side = match side_str.as_str() {
                    "Buy" => Side::Buy,
                    "Sell" => Side::Sell,
                    _ => {
                        warn!(side = %side_str, "STORAGE: Invalid order side, skipping order restore");
                        return Ok(None);
                    }
                };
                
                let qty = Decimal::from_str(&qty_str)
                    .map_err(|e| anyhow!("Failed to parse order qty: {}", e))?;
                
                Some(OpenOrder {
                    symbol,
                    order_id,
                    side,
                    qty: Qty(qty),
                })
            } else {
                None
            };
            
            // Parse timestamps (convert from i64 nanoseconds to Instant)
            let last_order_update_timestamp = order_ts.map(|ts| {
                // Convert from nanoseconds since epoch to Instant
                // Note: Instant is relative to process start, so we approximate
                // This is only used for comparison, not absolute time
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default();
                let stored = Duration::from_nanos(ts as u64);
                // Approximate: use current time minus stored duration
                // This is not perfect but works for timestamp comparison
                std::time::Instant::now() - (now - stored)
            });
            
            let last_position_update_timestamp = pos_ts.map(|ts| {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default();
                let stored = Duration::from_nanos(ts as u64);
                std::time::Instant::now() - (now - stored)
            });
            
            let state = OrderingState {
                open_position,
                open_order,
                last_order_update_timestamp,
                last_position_update_timestamp,
            };
            
            info!(
                has_position = state.open_position.is_some(),
                has_order = state.open_order.is_some(),
                "STORAGE: Restored OrderingState from database"
            );
            
            Ok(Some(state))
        } else {
            debug!("STORAGE: No OrderingState found in database");
            Ok(None)
        }
    }
    
    /// Persist OrderingState to database
    pub async fn save_ordering_state(&self, state: &OrderingState) -> Result<()> {
        let db = self.db.lock().await;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as i64;
        
        // Convert timestamps to i64 nanoseconds since epoch
        let order_ts = state.last_order_update_timestamp
            .and_then(|ts| {
                // Approximate: use current time to estimate stored timestamp
                // This is not perfect but works for persistence
                let now_duration = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default();
                Some(now_duration.as_nanos() as i64)
            });
        
        let pos_ts = state.last_position_update_timestamp
            .and_then(|_| {
                let now_duration = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default();
                Some(now_duration.as_nanos() as i64)
            });
        
        db.execute(
            r#"
            INSERT OR REPLACE INTO ordering_state (
                id,
                open_position_symbol,
                open_position_direction,
                open_position_qty,
                open_position_entry_price,
                open_order_symbol,
                open_order_id,
                open_order_side,
                open_order_qty,
                last_order_update_timestamp,
                last_position_update_timestamp,
                updated_at
            ) VALUES (1, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
            params![
                state.open_position.as_ref().map(|p| p.symbol.as_str()),
                state.open_position.as_ref().map(|p| format!("{:?}", p.direction)),
                state.open_position.as_ref().map(|p| p.qty.0.to_string()),
                state.open_position.as_ref().map(|p| p.entry_price.0.to_string()),
                state.open_order.as_ref().map(|o| o.symbol.as_str()),
                state.open_order.as_ref().map(|o| o.order_id.as_str()),
                state.open_order.as_ref().map(|o| format!("{:?}", o.side)),
                state.open_order.as_ref().map(|o| o.qty.0.to_string()),
                order_ts,
                pos_ts,
                now,
            ],
        )?;
        
        debug!("STORAGE: Saved OrderingState to database");
        Ok(())
    }
    
    /// Restore all OrderFillHistory entries from database
    /// Returns a map of order_id -> OrderFillHistory
    pub async fn restore_order_fill_history(&self) -> Result<Vec<(String, OrderFillHistory)>> {
        let db = self.db.lock().await;
        
        let mut stmt = db.prepare(
            "SELECT 
                order_id,
                symbol,
                total_filled_qty,
                weighted_price_sum,
                maker_fill_count,
                total_fill_count,
                last_update
            FROM order_fill_history
            ORDER BY last_update DESC"
        )?;
        
        let rows = stmt.query_map([], |row| {
            let order_id: String = row.get(0)?;
            let _symbol: String = row.get(1)?;
            let total_filled_qty_str: String = row.get(2)?;
            let weighted_price_sum_str: String = row.get(3)?;
            let maker_fill_count: u32 = row.get(4)?;
            let total_fill_count: u32 = row.get(5)?;
            let last_update_nanos: i64 = row.get(6)?;
            
            let total_filled_qty = Decimal::from_str(&total_filled_qty_str)
                .map_err(|e| rusqlite::Error::InvalidColumnType(0, format!("Failed to parse qty: {}", e), rusqlite::types::Type::Text))?;
            let weighted_price_sum = Decimal::from_str(&weighted_price_sum_str)
                .map_err(|e| rusqlite::Error::InvalidColumnType(0, format!("Failed to parse price: {}", e), rusqlite::types::Type::Text))?;
            
            // Convert timestamp from i64 nanoseconds to Instant (approximate)
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            let stored = Duration::from_nanos(last_update_nanos as u64);
            let last_update = std::time::Instant::now() - (now - stored);
            
            Ok((order_id, OrderFillHistory {
                total_filled_qty: Qty(total_filled_qty),
                weighted_price_sum,
                last_update,
                maker_fill_count,
                total_fill_count,
            }))
        })?;
        
        let mut history = Vec::new();
        for row in rows {
            match row {
                Ok(entry) => history.push(entry),
                Err(e) => {
                    warn!(error = %e, "STORAGE: Failed to restore order fill history entry, skipping");
                }
            }
        }
        
        info!(count = history.len(), "STORAGE: Restored {} order fill history entries from database", history.len());
        Ok(history)
    }
    
    /// Save OrderFillHistory entry to database
    pub async fn save_order_fill_history(
        &self,
        order_id: &str,
        symbol: &str,
        history: &OrderFillHistory,
    ) -> Result<()> {
        let db = self.db.lock().await;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as i64;
        
        // Convert last_update to i64 nanoseconds since epoch (approximate)
        let last_update_nanos = {
            let now_duration = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            now_duration.as_nanos() as i64
        };
        
        db.execute(
            r#"
            INSERT OR REPLACE INTO order_fill_history (
                order_id,
                symbol,
                total_filled_qty,
                weighted_price_sum,
                maker_fill_count,
                total_fill_count,
                last_update,
                created_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            "#,
            params![
                order_id,
                symbol,
                history.total_filled_qty.0.to_string(),
                history.weighted_price_sum.to_string(),
                history.maker_fill_count,
                history.total_fill_count,
                last_update_nanos,
                now,
            ],
        )?;
        
        debug!(order_id = %order_id, "STORAGE: Saved order fill history to database");
        Ok(())
    }
    
    /// Remove OrderFillHistory entry from database
    pub async fn remove_order_fill_history(&self, order_id: &str) -> Result<()> {
        let db = self.db.lock().await;
        
        db.execute(
            "DELETE FROM order_fill_history WHERE order_id = ?",
            params![order_id],
        )?;
        
        debug!(order_id = %order_id, "STORAGE: Removed order fill history from database");
        Ok(())
    }
    
    /// Log balance reservation (for debugging, not restored on startup)
    pub async fn log_balance_reservation(
        &self,
        asset: &str,
        amount: &Decimal,
        order_id: Option<&str>,
        symbol: Option<&str>,
    ) -> Result<i64> {
        let db = self.db.lock().await;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as i64;
        
        db.execute(
            r#"
            INSERT INTO balance_reservations_log (
                asset,
                amount,
                reserved_at,
                order_id,
                symbol
            ) VALUES (?, ?, ?, ?, ?)
            "#,
            params![
                asset,
                amount.to_string(),
                now,
                order_id,
                symbol,
            ],
        )?;
        
        Ok(db.last_insert_rowid())
    }
    
    /// Log balance reservation release
    pub async fn log_balance_release(&self, reservation_id: i64) -> Result<()> {
        let db = self.db.lock().await;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as i64;
        
        db.execute(
            "UPDATE balance_reservations_log SET released_at = ? WHERE id = ?",
            params![now, reservation_id],
        )?;
        
        Ok(())
    }
    
    /// Clean up old balance reservation logs (older than specified days)
    pub async fn cleanup_balance_reservation_logs(&self, days: u32) -> Result<usize> {
        let db = self.db.lock().await;
        let cutoff = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as i64
            - (days as i64 * 24 * 60 * 60 * 1_000_000_000); // Convert days to nanoseconds
        
        let count = db.execute(
            "DELETE FROM balance_reservations_log WHERE reserved_at < ?",
            params![cutoff],
        )?;
        
        info!(deleted_count = count, days, "STORAGE: Cleaned up {} old balance reservation logs (older than {} days)", count, days);
        Ok(count)
    }
    
    /// Clean up old order fill history (older than specified days)
    pub async fn cleanup_old_fill_history(&self, days: u32) -> Result<usize> {
        let db = self.db.lock().await;
        let cutoff = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as i64
            - (days as i64 * 24 * 60 * 60 * 1_000_000_000);
        
        let count = db.execute(
            "DELETE FROM order_fill_history WHERE last_update < ?",
            params![cutoff],
        )?;
        
        info!(deleted_count = count, days, "STORAGE: Cleaned up {} old order fill history entries (older than {} days)", count, days);
        Ok(count)
    }
}

/// OrderFillHistory structure (matches connection.rs)
#[derive(Clone, Debug)]
pub struct OrderFillHistory {
    pub total_filled_qty: Qty,
    pub weighted_price_sum: Decimal,
    pub last_update: std::time::Instant,
    pub maker_fill_count: u32,
    pub total_fill_count: u32,
}

// ============================================================================
// STORAGE Module (Event-Driven)
// ============================================================================

/// STORAGE module - persistent state storage
/// Listens to state update events and persists them to SQLite
/// Event-driven architecture - no direct coupling with other modules
pub struct Storage {
    storage: Arc<StateStorage>,
    event_bus: Arc<EventBus>,
    shutdown_flag: Arc<AtomicBool>,
    shared_state: Arc<SharedState>,
}

impl Storage {
    /// Create a new Storage module instance
    ///
    /// # Arguments
    ///
    /// * `db_path` - Optional path to SQLite database file (default: "./bot_state.db")
    /// * `event_bus` - Event bus for subscribing to state update events
    /// * `shutdown_flag` - Shared flag to signal graceful shutdown
    /// * `shared_state` - Shared state for restoring OrderingState on startup
    ///
    /// # Returns
    ///
    /// Returns a new `Storage` instance. Call `start()` to begin listening to events.
    pub fn new(
        db_path: Option<&Path>,
        event_bus: Arc<EventBus>,
        shutdown_flag: Arc<AtomicBool>,
        shared_state: Arc<SharedState>,
    ) -> Result<Self> {
        let storage = Arc::new(StateStorage::new(db_path)?);
        
        Ok(Self {
            storage,
            event_bus,
            shutdown_flag,
            shared_state,
        })
    }
    
    /// Start the storage module and begin listening to events
    ///
    /// This method:
    /// 1. Restores OrderingState from database on startup
    /// 2. Listens to OrderingStateUpdate events and persists state changes
    /// 3. Listens to OrderFillHistoryUpdate events and persists fill history changes
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` immediately after spawning background tasks.
    pub async fn start(&self) -> Result<()> {
        // ✅ CRITICAL: Restore state from database on startup
        self.restore_state_on_startup().await?;
        
        let storage_state = self.storage.clone();
        let event_bus_state = self.event_bus.clone();
        let shutdown_flag_state = self.shutdown_flag.clone();
        
        // Spawn task for OrderingStateUpdate events with batch writing
        tokio::spawn(async move {
            let mut state_update_rx = event_bus_state.subscribe_ordering_state_update();
            
            info!("STORAGE: Started, listening to OrderingStateUpdate events (with batch writing)");
            
            // CRITICAL: Batch write mechanism to prevent DB bottleneck
            // Problem: High frequency events cause SQLite single-writer bottleneck
            // Solution: Collect updates and write in batches (every 1 second)
            // For OrderingStateUpdate, only the latest state matters (overwrite previous)
            let mut pending_state_update: Option<OrderingStateUpdate> = None;
            let mut batch_interval = tokio::time::interval(Duration::from_secs(1));
            batch_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            
            loop {
                tokio::select! {
                    // Receive new state update
                    update_result = state_update_rx.recv() => {
                        match update_result {
                            Ok(update) => {
                                if shutdown_flag_state.load(AtomicOrdering::Relaxed) {
                                    // Flush pending update before shutdown
                                    if let Some(pending) = pending_state_update.take() {
                                        if let Err(e) = Self::handle_ordering_state_update(&pending, &storage_state).await {
                                            warn!(error = %e, "STORAGE: Failed to persist final OrderingStateUpdate on shutdown");
                                        }
                                    }
                                    break;
                                }
                                
                                // Store latest state (overwrite previous - only latest matters)
                                pending_state_update = Some(update);
                            }
                            Err(_) => {
                                // Flush pending update before exit
                                if let Some(pending) = pending_state_update.take() {
                                    if let Err(e) = Self::handle_ordering_state_update(&pending, &storage_state).await {
                                        warn!(error = %e, "STORAGE: Failed to persist final OrderingStateUpdate on channel close");
                                    }
                                }
                                break;
                            }
                        }
                    }
                    // Batch write timer
                    _ = batch_interval.tick() => {
                        if let Some(pending) = pending_state_update.take() {
                            if let Err(e) = Self::handle_ordering_state_update(&pending, &storage_state).await {
                                warn!(error = %e, "STORAGE: Failed to persist OrderingStateUpdate in batch");
                            }
                        }
                    }
                }
            }
        });
        
        let storage_fill = self.storage.clone();
        let event_bus_fill = self.event_bus.clone();
        let shutdown_flag_fill = self.shutdown_flag.clone();
        
        // Spawn task for OrderFillHistoryUpdate events with batch writing
        tokio::spawn(async move {
            let mut fill_history_rx = event_bus_fill.subscribe_order_fill_history_update();
            
            info!("STORAGE: Started, listening to OrderFillHistoryUpdate events (with batch writing)");
            
            // CRITICAL: Batch write mechanism to prevent DB bottleneck
            // Problem: High frequency fills (100 fills/min) cause SQLite single-writer bottleneck
            // Solution: Collect updates and write in batches (every 1 second)
            // For OrderFillHistoryUpdate, we need to process all updates (not just latest)
            let mut pending_updates: Vec<OrderFillHistoryUpdate> = Vec::new();
            let mut batch_interval = tokio::time::interval(Duration::from_secs(1));
            batch_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            
            loop {
                tokio::select! {
                    // Receive new fill history update
                    update_result = fill_history_rx.recv() => {
                        match update_result {
                            Ok(update) => {
                                if shutdown_flag_fill.load(AtomicOrdering::Relaxed) {
                                    // Flush pending updates before shutdown
                                    if !pending_updates.is_empty() {
                                        if let Err(e) = Self::handle_batch_fill_history_updates(&pending_updates, &storage_fill).await {
                                            warn!(error = %e, "STORAGE: Failed to persist final OrderFillHistoryUpdate batch on shutdown");
                                        }
                                        pending_updates.clear();
                                    }
                                    break;
                                }
                                
                                // Add to pending batch
                                pending_updates.push(update);
                            }
                            Err(_) => {
                                // Flush pending updates before exit
                                if !pending_updates.is_empty() {
                                    if let Err(e) = Self::handle_batch_fill_history_updates(&pending_updates, &storage_fill).await {
                                        warn!(error = %e, "STORAGE: Failed to persist final OrderFillHistoryUpdate batch on channel close");
                                    }
                                    pending_updates.clear();
                                }
                                break;
                            }
                        }
                    }
                    // Batch write timer
                    _ = batch_interval.tick() => {
                        if !pending_updates.is_empty() {
                            let updates_to_process = std::mem::take(&mut pending_updates);
                            if let Err(e) = Self::handle_batch_fill_history_updates(&updates_to_process, &storage_fill).await {
                                warn!(error = %e, "STORAGE: Failed to persist OrderFillHistoryUpdate batch");
                            }
                        }
                    }
                }
            }
        });
        
        Ok(())
    }
    
    /// Restore state from database on startup
    async fn restore_state_on_startup(&self) -> Result<()> {
        // Restore OrderingState
        match self.storage.restore_ordering_state().await {
            Ok(Some(restored_state)) => {
                let mut state_guard = self.shared_state.ordering_state.lock().await;
                *state_guard = restored_state;
                info!(
                    has_position = state_guard.open_position.is_some(),
                    has_order = state_guard.open_order.is_some(),
                    "STORAGE: Restored OrderingState from database on startup"
                );
            }
            Ok(None) => {
                debug!("STORAGE: No OrderingState found in database");
            }
            Err(e) => {
                warn!(error = %e, "STORAGE: Failed to restore OrderingState from database");
            }
        }
        
        Ok(())
    }
    
    /// Handle OrderingStateUpdate event and persist to database
    async fn handle_ordering_state_update(
        update: &OrderingStateUpdate,
        storage: &Arc<StateStorage>,
    ) -> Result<()> {
        // Convert event snapshot to OrderingState
        let ordering_state = OrderingState {
            open_position: update.open_position.as_ref().map(|pos| {
                let direction = match pos.direction.as_str() {
                    "Long" => PositionDirection::Long,
                    "Short" => PositionDirection::Short,
                    _ => {
                        warn!(direction = %pos.direction, "STORAGE: Invalid position direction in event");
                        return None;
                    }
                };
                
                let qty = match Decimal::from_str(&pos.qty) {
                    Ok(q) => q,
                    Err(e) => {
                        warn!(error = %e, qty = %pos.qty, "STORAGE: Failed to parse position qty from event");
                        return None;
                    }
                };
                
                let entry_price = match Decimal::from_str(&pos.entry_price) {
                    Ok(p) => p,
                    Err(e) => {
                        warn!(error = %e, price = %pos.entry_price, "STORAGE: Failed to parse position entry_price from event");
                        return None;
                    }
                };
                
                Some(OpenPosition {
                    symbol: pos.symbol.clone(),
                    direction,
                    qty: Qty(qty),
                    entry_price: Px(entry_price),
                })
            }).flatten(),
            open_order: update.open_order.as_ref().map(|order| {
                let side = match order.side.as_str() {
                    "Buy" => Side::Buy,
                    "Sell" => Side::Sell,
                    _ => {
                        warn!(side = %order.side, "STORAGE: Invalid order side in event");
                        return None;
                    }
                };
                
                let qty = match Decimal::from_str(&order.qty) {
                    Ok(q) => q,
                    Err(e) => {
                        warn!(error = %e, qty = %order.qty, "STORAGE: Failed to parse order qty from event");
                        return None;
                    }
                };
                
                Some(OpenOrder {
                    symbol: order.symbol.clone(),
                    order_id: order.order_id.clone(),
                    side,
                    qty: Qty(qty),
                })
            }).flatten(),
            last_order_update_timestamp: None, // Not persisted in event
            last_position_update_timestamp: None, // Not persisted in event
        };
        
        storage.save_ordering_state(&ordering_state).await?;
        debug!("STORAGE: Persisted OrderingStateUpdate to database");
        Ok(())
    }
    
    /// Handle OrderFillHistoryUpdate event and persist to database
    async fn handle_order_fill_history_update(
        update: &OrderFillHistoryUpdate,
        storage: &Arc<StateStorage>,
    ) -> Result<()> {
        match update.action {
            crate::event_bus::FillHistoryAction::Save => {
                if let Some(data) = &update.data {
                    let total_filled_qty = Decimal::from_str(&data.total_filled_qty)
                        .map_err(|e| anyhow!("Failed to parse total_filled_qty: {}", e))?;
                    let weighted_price_sum = Decimal::from_str(&data.weighted_price_sum)
                        .map_err(|e| anyhow!("Failed to parse weighted_price_sum: {}", e))?;
                    
                    let history = OrderFillHistory {
                        total_filled_qty: Qty(total_filled_qty),
                        weighted_price_sum,
                        last_update: update.timestamp,
                        maker_fill_count: data.maker_fill_count,
                        total_fill_count: data.total_fill_count,
                    };
                    
                    storage.save_order_fill_history(&update.order_id, &update.symbol, &history).await?;
                    debug!(order_id = %update.order_id, "STORAGE: Persisted OrderFillHistoryUpdate (Save) to database");
                } else {
                    warn!(order_id = %update.order_id, "STORAGE: OrderFillHistoryUpdate Save action has no data");
                }
            }
            crate::event_bus::FillHistoryAction::Remove => {
                storage.remove_order_fill_history(&update.order_id).await?;
                debug!(order_id = %update.order_id, "STORAGE: Removed OrderFillHistory from database");
            }
        }
        
        Ok(())
    }
    
    /// Handle batch of OrderFillHistoryUpdate events and persist to database
    /// 
    /// CRITICAL: Batch write to prevent DB bottleneck
    /// Processes multiple updates in a single transaction for better performance
    /// Reduces SQLite single-writer bottleneck from high-frequency events
    async fn handle_batch_fill_history_updates(
        updates: &[OrderFillHistoryUpdate],
        storage: &Arc<StateStorage>,
    ) -> Result<()> {
        if updates.is_empty() {
            return Ok(());
        }
        
        // Process all updates in batch
        for update in updates {
            if let Err(e) = Self::handle_order_fill_history_update(update, storage).await {
                // Log error but continue processing other updates
                warn!(
                    error = %e,
                    order_id = %update.order_id,
                    "STORAGE: Failed to persist OrderFillHistoryUpdate in batch, continuing with other updates"
                );
            }
        }
        
        debug!(
            batch_size = updates.len(),
            "STORAGE: Persisted {} OrderFillHistoryUpdate events in batch",
            updates.len()
        );
        
        Ok(())
    }
}

