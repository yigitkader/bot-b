// BALANCE: Balance tracking (USDT/USDC)
// Only reads and maintains USDT/USDC balance
// Provides availableBalance to other modules via shared state

use crate::connection::Connection;
use crate::event_bus::{BalanceUpdate, EventBus};
use crate::state::{BalanceStore, SharedState};
use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tracing::{info, warn};

/// BALANCE module - balance tracking
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

    /// Get shared balance store
    pub fn balance_store(&self) -> Arc<tokio::sync::RwLock<BalanceStore>> {
        self.shared_state.balance_store.clone()
    }

    /// Start balance service
    /// Listens to BalanceUpdate events from CONNECTION (WebSocket stream)
    /// Falls back to REST API only on startup and if WebSocket fails
    pub async fn start(&self) -> Result<()> {
        // Fetch balance immediately on startup (REST API fallback)
        if let Err(e) = self.fetch_and_update_balance().await {
            warn!(error = %e, "BALANCE: failed to fetch initial balance, will wait for WebSocket updates");
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
                        
                        // Update shared state from WebSocket event
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

    /// Fetch and update balance immediately
    async fn fetch_and_update_balance(&self) -> Result<()> {
        let usdt_balance = self.connection.fetch_balance("USDT").await?;
        let usdc_balance = self.connection.fetch_balance("USDC").await?;
        
        // Update shared state
        {
            let mut store = self.shared_state.balance_store.write().await;
            store.usdt = usdt_balance;
            store.usdc = usdc_balance;
            store.last_updated = Instant::now();
        }
        
        // Publish BalanceUpdate event
        let balance_update = BalanceUpdate {
            usdt: usdt_balance,
            usdc: usdc_balance,
            timestamp: Instant::now(),
        };
        
        let _ = self.event_bus.balance_update_tx.send(balance_update);
        {
            info!(
                usdt = %usdt_balance,
                usdc = %usdc_balance,
                "BALANCE: Initial balance fetched"
            );
        }
        
        Ok(())
    }
}

