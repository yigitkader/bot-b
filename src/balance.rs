//! Minimal BALANCE module stub used in the kata.
//!
//! The production version synchronised WebSocket updates and persisted history.
//! For testing the shared state machinery we only require a very small subset of
//! the behaviour: fetch balances once and update the shared `BalanceStore` so
//! other components can observe deterministic values.

use crate::connection::Connection;
use crate::event_bus::EventBus;
use crate::state::SharedState;
use anyhow::Result;
use rust_decimal::Decimal;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

pub struct Balance {
    connection: Arc<Connection>,
    #[allow(dead_code)]
    event_bus: Arc<EventBus>,
    #[allow(dead_code)]
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
        let usdt = self
            .connection
            .fetch_balance("USDT")
            .await
            .unwrap_or(Decimal::ZERO);
        let usdc = self
            .connection
            .fetch_balance("USDC")
            .await
            .unwrap_or(Decimal::ZERO);

        {
            let mut store = self.shared_state.balance_store.write().await;
            store.usdt = usdt;
            store.usdc = usdc;
        }

        Ok(())
    }
}
