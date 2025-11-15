//! Minimal ORDERING module stub used for the kata.
//!
//! The full project contains sophisticated logic for risk checks, retries and
//! order state management.  The tests bundled with this kata focus on the shared
//! state data structures, therefore a lightweight version of the module is
//! sufficient.  The stub exposes the same constructor and public methods so the
//! rest of the application can be wired together without pulling in the complex
//! production code.

use crate::config::AppCfg;
use crate::connection::{Connection, OrderCommand};
use crate::event_bus::EventBus;
use crate::state::SharedState;
use anyhow::Result;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// ORDERING module placeholder.
pub struct Ordering {
    #[allow(dead_code)]
    cfg: Arc<AppCfg>,
    connection: Arc<Connection>,
    #[allow(dead_code)]
    event_bus: Arc<EventBus>,
    #[allow(dead_code)]
    shutdown_flag: Arc<AtomicBool>,
    #[allow(dead_code)]
    shared_state: Arc<SharedState>,
}

impl Ordering {
    /// Create the ordering module stub.
    pub fn new(
        cfg: Arc<AppCfg>,
        connection: Arc<Connection>,
        event_bus: Arc<EventBus>,
        shutdown_flag: Arc<AtomicBool>,
        shared_state: Arc<SharedState>,
    ) -> Self {
        Self {
            cfg,
            connection,
            event_bus,
            shutdown_flag,
            shared_state,
        }
    }

    /// Start background processing.  The stub does not spawn workers but keeps
    /// the async API compatible with the original module.
    pub async fn start(&self) -> Result<()> {
        Ok(())
    }

    /// Helper used by tests or integration harnesses that still want to submit
    /// a command through the ordering facade.
    pub async fn send_order(&self, command: OrderCommand) -> Result<String> {
        self.connection.send_order(command).await
    }
}
