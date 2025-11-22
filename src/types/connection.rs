use chrono::{DateTime, Utc};
use reqwest::{Client, Url};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::config::BotConfig;

pub struct RateLimiter {
    order_limiter: Arc<tokio::sync::Semaphore>,
    weight_tracker: Arc<tokio::sync::RwLock<WeightTracker>>,
}

struct WeightTracker {
    current_weight: u32,
    window_start: std::time::Instant,
}

impl RateLimiter {
    pub fn new() -> Self {
        let order_limiter = Arc::new(tokio::sync::Semaphore::new(10));
        let weight_tracker = Arc::new(tokio::sync::RwLock::new(WeightTracker {
            current_weight: 0,
            window_start: std::time::Instant::now(),
        }));

        let tracker_clone = weight_tracker.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
            loop {
                interval.tick().await;
                let mut tracker = tracker_clone.write().await;
                tracker.current_weight = 0;
                tracker.window_start = std::time::Instant::now();
            }
        });

        Self {
            order_limiter,
            weight_tracker,
        }
    }

    pub async fn acquire_order_permit(&self) -> anyhow::Result<tokio::sync::SemaphorePermit<'_>> {
        use anyhow::anyhow;
        let permit = self
            .order_limiter
            .acquire()
            .await
            .map_err(|_| anyhow!("order limiter closed"))?;

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        Ok(permit)
    }

    pub async fn acquire_weight(&self, weight: u32) -> anyhow::Result<()> {
        loop {
            {
                let mut tracker = self.weight_tracker.write().await;
                if tracker.window_start.elapsed().as_secs() >= 60 {
                    tracker.current_weight = 0;
                    tracker.window_start = std::time::Instant::now();
                }

                if tracker.current_weight + weight <= 1200 {
                    tracker.current_weight += weight;
                    return Ok(());
                }
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
    }
}

#[derive(Clone)]
pub struct Connection {
    pub(crate) config: BotConfig,
    pub(crate) http: Client,
    pub(crate) rate_limiter: Arc<RateLimiter>,
    pub(crate) server_time_offset: Arc<RwLock<i64>>,
    pub(crate) last_market_tick_ts: Arc<RwLock<Option<DateTime<Utc>>>>,
    pub(crate) last_ws_connection_ts: Arc<RwLock<Option<DateTime<Utc>>>>,
}

pub struct FuturesClient {
    pub(crate) http: Client,
    pub(crate) base_url: Url,
    pub(crate) api_key: Option<String>,
    pub(crate) api_secret: Option<String>,
    pub(crate) recv_window_ms: u64,
}

pub fn handle_broadcast_recv<T>(
    result: Result<T, tokio::sync::broadcast::error::RecvError>,
) -> Result<Option<T>, ()> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => Ok(None),
        Err(tokio::sync::broadcast::error::RecvError::Closed) => Err(()),
    }
}

