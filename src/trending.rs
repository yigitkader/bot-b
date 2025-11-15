//! Minimal TRENDING module stub used for tests.
//!
//! The production project ran complex analytics in this module.  For the
//! purposes of the exercises we only need a lightweight container that exposes
//! the same constructor and `start` method so the rest of the crate builds.  All
//! heavy lifting happens elsewhere in the tests.

use crate::config::AppCfg;
use crate::connection::Connection;
use crate::event_bus::EventBus;
use crate::state::SharedState;
use anyhow::Result;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// Lightweight implementation that keeps references to the shared resources so
/// other modules can instantiate it exactly like the production version.
pub struct Trending {
    #[allow(dead_code)]
    cfg: Arc<AppCfg>,
    #[allow(dead_code)]
    event_bus: Arc<EventBus>,
    #[allow(dead_code)]
    shutdown_flag: Arc<AtomicBool>,
    #[allow(dead_code)]
    shared_state: Arc<SharedState>,
    #[allow(dead_code)]
    connection: Arc<Connection>,
}

impl Trending {
    /// Create a new stubbed instance.
    pub fn new(
        cfg: Arc<AppCfg>,
        event_bus: Arc<EventBus>,
        shutdown_flag: Arc<AtomicBool>,
        shared_state: Arc<SharedState>,
        connection: Arc<Connection>,
    ) -> Self {
        Self {
            cfg,
            event_bus,
            shutdown_flag,
            shared_state,
            connection,
        }
    }

    /// Start the module.  The stub does not spawn background tasks; it simply
    /// mirrors the original signature so callers can await the future.
    pub async fn start(&self) -> Result<()> {
        Ok(())
    }
}
