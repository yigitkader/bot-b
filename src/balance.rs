
use crate::connection::Connection;
use crate::event_bus::{BalanceUpdate, EventBus};
use crate::state::SharedState;
use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::time::Duration;
use tracing::{debug, info, warn};
pub struct Balance {
    connection: Arc<Connection>,
    event_bus: Arc<EventBus>,
    shutdown_flag: Arc<AtomicBool>,
    shared_state: Arc<SharedState>,
}
impl Balance {
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
    pub async fn start(&self) -> Result<()> {
        if let Err(e) = self.fetch_and_update_balance_with_retry().await {
            warn!(
                error = %e,
                "BALANCE: failed to fetch initial balance after retries, will wait for WebSocket updates"
            );
        }
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
                        {
                            let mut store = balance_store.write().await;
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
                        let delay_secs = INITIAL_DELAY_SECS * (1 << attempt);
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
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("Failed to fetch balance after {} attempts", MAX_RETRIES)))
    }
    async fn fetch_and_update_balance(&self) -> Result<()> {
        let usdt_balance = self.connection.fetch_balance("USDT").await?;
        let usdc_balance = self.connection.fetch_balance("USDC").await?;
        let timestamp = Instant::now();
        let balance_update = BalanceUpdate {
            usdt: usdt_balance,
            usdc: usdc_balance,
            timestamp,
        };
        let receiver_count = self.event_bus.balance_update_tx.receiver_count();
        if receiver_count == 0 {
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
        {
            let mut store = self.shared_state.balance_store.write().await;
            if store.last_updated <= timestamp {
                store.usdt = usdt_balance;
                store.usdc = usdc_balance;
                store.last_updated = timestamp;
            } else {
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
