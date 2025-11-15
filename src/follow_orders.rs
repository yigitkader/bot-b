//! Minimal FOLLOW_ORDERS module stub.
//!
//! The original implementation consumed price streams and generated close
//! requests.  For the simplified training environment we only need the struct
//! skeleton so that other components can be wired together without pulling in
//! asynchronous workers.

use crate::config::AppCfg;
use crate::event_bus::EventBus;
use anyhow::Result;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

pub struct FollowOrders {
    #[allow(dead_code)]
    cfg: Arc<AppCfg>,
    #[allow(dead_code)]
    event_bus: Arc<EventBus>,
    #[allow(dead_code)]
    shutdown_flag: Arc<AtomicBool>,
}

impl FollowOrders {
    pub fn new(cfg: Arc<AppCfg>, event_bus: Arc<EventBus>, shutdown_flag: Arc<AtomicBool>) -> Self {
        Self {
            cfg,
            event_bus,
            shutdown_flag,
        }
    }

    pub async fn start(&self) -> Result<()> {
        Ok(())
    }
}
