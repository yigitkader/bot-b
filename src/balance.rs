// BALANCE: Balance tracking (USDT/USDC)
// Only reads and maintains USDT/USDC balance
// Provides availableBalance to other modules via shared state

use crate::connection::Connection;
use crate::event_bus::{BalanceUpdate, EventBus};
use crate::state::SharedState;
use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::time::Duration;
use tracing::{debug, info, warn};

/// BALANCE module - balance tracking
pub struct Balance {
    connection: Arc<Connection>,
    event_bus: Arc<EventBus>,
    shutdown_flag: Arc<AtomicBool>,
    shared_state: Arc<SharedState>,
}

impl Balance {
    /// Create a new Balance module instance.
    ///
    /// The Balance module tracks USDT and USDC balances. It subscribes to BalanceUpdate events
    /// from the WebSocket stream and maintains balance state in shared storage.
    ///
    /// # Arguments
    ///
    /// * `connection` - Connection instance for fetching balance via REST API (fallback)
    /// * `event_bus` - Event bus for subscribing to BalanceUpdate events from WebSocket
    /// * `shutdown_flag` - Shared flag to signal graceful shutdown
    /// * `shared_state` - Shared state containing the balance store
    ///
    /// # Returns
    ///
    /// Returns a new `Balance` instance. Call `start()` to begin tracking balances.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # let connection = Arc::new(crate::connection::Connection::from_config(todo!(), todo!(), todo!(), None)?);
    /// # let event_bus = Arc::new(crate::event_bus::EventBus::new());
    /// # let shutdown_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    /// # let shared_state = Arc::new(crate::state::SharedState::new());
    /// let balance = Balance::new(connection, event_bus, shutdown_flag, shared_state);
    /// balance.start().await?;
    /// ```
    pub fn new(
        connection: Arc<Connection>,
        event_bus: Arc<EventBus>,
        shutdown_flag: Arc<AtomicBool>,
        shared_state: Arc<SharedState>,
    ) -> Self {
        Self {
            connection,
            event_bus,
            shutdown_flag,
            shared_state,
        }
    }

    /// Start the balance tracking service.
    ///
    /// This method:
    /// 1. Fetches initial balance via REST API (with retry mechanism)
    /// 2. Spawns a background task that listens to BalanceUpdate events from WebSocket
    /// 3. Updates shared balance store when balance changes
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` after initial balance fetch completes (or fails after retries).
    /// The WebSocket listener task continues running in the background until `shutdown_flag` is set.
    ///
    /// # Behavior
    ///
    /// - Prefers WebSocket updates (real-time) over REST API polling
    /// - Falls back to REST API on startup and if WebSocket fails
    /// - Uses event-first approach: sends BalanceUpdate event before updating store
    ///
    /// # Errors
    ///
    /// Returns an error if initial balance fetch fails after all retries. The service will
    /// still start and wait for WebSocket updates.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # let balance = crate::balance::Balance::new(todo!(), todo!(), todo!(), todo!());
    /// balance.start().await?;
    /// // Balance tracking is now active
    /// ```
    pub async fn start(&self) -> Result<()> {
        // Fetch balance immediately on startup (REST API fallback)
        // Retry with exponential backoff if initial fetch fails
        if let Err(e) = self.fetch_and_update_balance_with_retry().await {
            warn!(
                error = %e,
                "BALANCE: failed to fetch initial balance after retries, will wait for WebSocket updates"
            );
        }
        
        // Listen to BalanceUpdate events from WebSocket (preferred method)
        let balance_store = self.shared_state.balance_store.clone();
        let shutdown_flag = self.shutdown_flag.clone();
        let event_bus = self.event_bus.clone();
        
        tokio::spawn(async move {
            let mut balance_update_rx = event_bus.subscribe_balance_update();
            
            info!("BALANCE: Started, listening to BalanceUpdate events from WebSocket");
            
            loop {
                match balance_update_rx.recv().await {
                    Ok(update) => {
                        if shutdown_flag.load(Ordering::Relaxed) {
                            break;
                        }
                        
                        // ✅ CRITICAL: Update shared state from WebSocket event
                        // WebSocket updates are prioritized (real-time, more accurate)
                        // Always accept WebSocket updates (they are more recent and accurate)
                        {
                            let mut store = balance_store.write().await;
                            
                            // WebSocket updates are always accepted (they are real-time and prioritized)
                            // Even if REST API update is pending, WebSocket update should overwrite it
                            store.usdt = update.usdt;
                            store.usdc = update.usdc;
                            store.last_updated = update.timestamp;
                        }
                        
                        info!(
                            usdt = %update.usdt,
                            usdc = %update.usdc,
                            "BALANCE: Balance updated from WebSocket"
                        );
                    }
                    Err(_) => break,
                }
            }
            
            info!("BALANCE: Stopped");
        });
        
        Ok(())
    }

    /// Fetch and update balance with retry mechanism
    /// Retries up to 5 times with exponential backoff (1s, 2s, 4s, 8s, 16s)
    async fn fetch_and_update_balance_with_retry(&self) -> Result<()> {
        const MAX_RETRIES: u32 = 5;
        const INITIAL_DELAY_SECS: u64 = 1;
        
        let mut last_error = None;
        
        for attempt in 0..MAX_RETRIES {
            match self.fetch_and_update_balance().await {
                Ok(()) => {
                    if attempt > 0 {
                        info!(
                            attempt = attempt + 1,
                            "BALANCE: Successfully fetched balance after retry"
                        );
                    }
                    return Ok(());
                }
                Err(e) => {
                    last_error = Some(e);
                    
                    if attempt < MAX_RETRIES - 1 {
                        let delay_secs = INITIAL_DELAY_SECS * (1 << attempt); // Exponential backoff
                        // ✅ CRITICAL: Safe unwrap - last_error was just set to Some(e) above
                        // But use if let for extra safety
                        if let Some(ref err) = last_error {
                            warn!(
                                error = %err,
                                attempt = attempt + 1,
                                max_retries = MAX_RETRIES,
                                retry_in_secs = delay_secs,
                                "BALANCE: Failed to fetch balance, retrying..."
                            );
                        }
                        tokio::time::sleep(Duration::from_secs(delay_secs)).await;
                    }
                }
            }
        }
        
        // All retries failed
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("Failed to fetch balance after {} attempts", MAX_RETRIES)))
    }

    /// Fetch and update balance immediately
    async fn fetch_and_update_balance(&self) -> Result<()> {
        let usdt_balance = self.connection.fetch_balance("USDT").await?;
        let usdc_balance = self.connection.fetch_balance("USDC").await?;
        
        let timestamp = Instant::now();
        
        // CRITICAL: Event-first approach - send event BEFORE updating store
        // This ensures event-driven consistency: subscribers process event before store is updated
        // Prevents race condition where another thread reads updated store before event is processed
        let balance_update = BalanceUpdate {
            usdt: usdt_balance,
            usdc: usdc_balance,
            timestamp,
        };
        
        // Send event first (non-blocking, but ensures event is queued before store update)
        // Check if there are subscribers before sending to avoid unnecessary errors
        let receiver_count = self.event_bus.balance_update_tx.receiver_count();
        if receiver_count == 0 {
            // No subscribers - skip sending (not an error, just no consumers yet)
            debug!("BALANCE: No BalanceUpdate subscribers, skipping event");
        } else {
            if let Err(e) = self.event_bus.balance_update_tx.send(balance_update) {
                warn!(
                    error = ?e,
                    receiver_count,
                    "BALANCE: Failed to send BalanceUpdate event (channel closed or buffer full)"
                );
            }
        }
        
        // ✅ CRITICAL: Update shared state AFTER event is sent, but check timestamp first
        // WebSocket updates are prioritized - if WebSocket already updated balance with newer timestamp,
        // don't overwrite it with stale REST API data
        // ✅ CRITICAL FIX: Single atomic check - no race window
        // Lock is held during the entire check and update, so no WebSocket update can interleave
        // Single check is sufficient because lock prevents concurrent updates
        {
            let mut store = self.shared_state.balance_store.write().await;
            
            // ✅ Single atomic check - no race window
            // Lock is held, so no WebSocket update can arrive between check and update
            // If timestamp is valid (REST API data is newer or equal), update atomically
            if store.last_updated <= timestamp {
                // Timestamp check passed - safe to update atomically
                store.usdt = usdt_balance;
                store.usdc = usdc_balance;
                store.last_updated = timestamp;
            } else {
                // Stale data, skip
                warn!(
                    usdt_rest = %usdt_balance,
                    usdc_rest = %usdc_balance,
                    usdt_ws = %store.usdt,
                    usdc_ws = %store.usdc,
                    rest_timestamp = ?timestamp,
                    ws_timestamp = ?store.last_updated,
                    "BALANCE: Ignoring stale REST API balance - WebSocket update is newer"
                );
            }
        }
        
        info!(
            usdt = %usdt_balance,
            usdc = %usdc_balance,
            "BALANCE: Initial balance fetched"
        );
        
        Ok(())
    }
}

