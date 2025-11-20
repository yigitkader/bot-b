use crate::types::{MarketTick, Side};
use chrono::{DateTime, Utc};
use log::{debug, warn};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::broadcast::Receiver as BReceiver;
use tokio::sync::RwLock;

const IMPACT_COEFFICIENT_BPS: f64 = 50.0;

#[derive(Debug, Clone, Default)]
pub struct SlippageStats {
    pub sample_count: usize,
    pub avg_spread_bps: f64,
    pub p95_spread_bps: f64,
    pub max_spread_bps: f64,
    pub avg_bid_depth_usd: f64,
    pub avg_ask_depth_usd: f64,
    pub last_update: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug)]
struct SlippageSample {
    spread_bps: f64,
    bid_depth_usd: f64,
    ask_depth_usd: f64,
    ts: DateTime<Utc>,
}

pub struct SlippageTracker {
    window: usize,
    samples: RwLock<VecDeque<SlippageSample>>,
    stats: RwLock<SlippageStats>,
}

impl SlippageTracker {
    pub fn new(window: usize) -> Arc<Self> {
        Arc::new(Self {
            window,
            samples: RwLock::new(VecDeque::with_capacity(window)),
            stats: RwLock::new(SlippageStats::default()),
        })
    }

    pub async fn ingest_tick(&self, tick: &MarketTick) {
        if tick.bid <= 0.0 || tick.ask <= 0.0 || tick.ask <= tick.bid {
            return;
        }

        let spread_bps = ((tick.ask - tick.bid) / ((tick.ask + tick.bid) / 2.0)) * 10_000.0;
        let bid_depth_usd = tick.bid_depth_usd.unwrap_or(0.0);
        let ask_depth_usd = tick.ask_depth_usd.unwrap_or(0.0);

        let sample = SlippageSample {
            spread_bps,
            bid_depth_usd,
            ask_depth_usd,
            ts: tick.ts,
        };

        {
            let mut samples = self.samples.write().await;
            samples.push_back(sample);
            if samples.len() > self.window {
                samples.pop_front();
            }
            let stats = Self::compute_stats(&samples);
            *self.stats.write().await = stats;
        }
    }

    pub async fn estimate_for_order(&self, side: Side, notional_usd: f64) -> f64 {
        let stats = self.stats.read().await.clone();
        if stats.sample_count == 0 || notional_usd <= 0.0 {
            return 0.0;
        }

        let spread_component = stats
            .p95_spread_bps
            .max(stats.avg_spread_bps)
            .max(stats.max_spread_bps * 0.5);

        let depth = match side {
            Side::Long => stats.avg_ask_depth_usd,
            Side::Short => stats.avg_bid_depth_usd,
        };

        let depth_component = if depth > 0.0 {
            let ratio = (notional_usd / depth).min(5.0);
            ratio * IMPACT_COEFFICIENT_BPS
        } else {
            0.0
        };

        spread_component + depth_component
    }

    pub async fn current_stats(&self) -> SlippageStats {
        self.stats.read().await.clone()
    }

    fn compute_stats(samples: &VecDeque<SlippageSample>) -> SlippageStats {
        if samples.is_empty() {
            return SlippageStats::default();
        }

        let mut spread_values: Vec<f64> = Vec::with_capacity(samples.len());
        let mut spread_sum = 0.0;
        let mut max_spread: f64 = 0.0;
        let mut bid_depth_sum = 0.0;
        let mut ask_depth_sum = 0.0;

        for sample in samples {
            spread_values.push(sample.spread_bps);
            spread_sum += sample.spread_bps;
            max_spread = max_spread.max(sample.spread_bps);
            bid_depth_sum += sample.bid_depth_usd;
            ask_depth_sum += sample.ask_depth_usd;
        }

        spread_values.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let idx = ((spread_values.len() as f64 * 0.95).ceil() as usize).saturating_sub(1);
        let p95_spread = *spread_values
            .get(idx.min(spread_values.len() - 1))
            .unwrap_or(&spread_values[spread_values.len() - 1]);

        SlippageStats {
            sample_count: samples.len(),
            avg_spread_bps: spread_sum / samples.len() as f64,
            p95_spread_bps: p95_spread,
            max_spread_bps: max_spread,
            avg_bid_depth_usd: bid_depth_sum / samples.len() as f64,
            avg_ask_depth_usd: ask_depth_sum / samples.len() as f64,
            last_update: samples.back().map(|s| s.ts),
        }
    }
}

pub async fn run_slippage_tracker(
    tracker: Arc<SlippageTracker>,
    mut market_rx: BReceiver<MarketTick>,
) {
    loop {
        match market_rx.recv().await {
            Ok(tick) => {
                tracker.ingest_tick(&tick).await;
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                warn!(
                    "SLIPPAGE: lagged by {} market ticks, tracker stats may be stale",
                    count
                );
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                warn!("SLIPPAGE: market channel closed, stopping tracker");
                break;
            }
        }
    }
    debug!("SLIPPAGE: tracker task exiting");
}
