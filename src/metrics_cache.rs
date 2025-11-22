// ✅ ADIM 4: Cache mekanizması (TrendPlan.md)
// Funding/OI/LSR için background task - REST çağrılarını azaltmak için

use anyhow::Result;
use chrono::{DateTime, Utc};
use log::{info, warn};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::types::{FundingRate, FuturesClient, LongShortRatioPoint, OpenInterestPoint};

#[derive(Debug, Clone)]
pub struct LatestMetrics {
    pub symbol: String,
    pub funding_rate: Option<f64>,
    pub open_interest: Option<f64>,
    pub long_short_ratio: Option<f64>,
    pub last_update: DateTime<Utc>,
}

pub struct MetricsCache {
    client: FuturesClient,
    cache: Arc<RwLock<HashMap<String, LatestMetrics>>>,
    update_interval_secs: u64,
    symbols: Arc<RwLock<Vec<String>>>,
}

impl MetricsCache {
    pub fn new(update_interval_secs: u64) -> Self {
        Self {
            client: FuturesClient::new(),
            cache: Arc::new(RwLock::new(HashMap::new())),
            update_interval_secs,
            symbols: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Set symbols to track
    pub async fn set_symbols(&self, symbols: Vec<String>) {
        let mut cached = self.symbols.write().await;
        *cached = symbols;
    }

    /// ✅ FIX: Pre-warm cache for new symbols (TrendPlan.md - Action Plan)
    /// SymbolScanner yeni coinler bulup rotasyon yaptığında,
    /// MetricsCache'in o yeni coinler için hemen veri çekmesi (pre-warm) gerekir.
    /// Bu, ilk API çağrısında cache miss olmasını önler.
    /// 
    /// ⚠️ CRITICAL FIX: Check cache, not just tracked symbols list
    /// Cache'de olmayan symbol'leri warmup et (ilk warmup için tüm symbol'ler warmup edilir)
    pub async fn pre_warm_symbols(&self, symbols: &[String]) -> Result<()> {
        // ✅ FIX: Check cache instead of tracked symbols list
        // Cache'de olmayan symbol'leri warmup et
        let cache = self.cache.read().await;
        let symbols_to_warmup: Vec<String> = symbols
            .iter()
            .filter(|s| !cache.contains_key(*s))
            .cloned()
            .collect();
        drop(cache);

        if symbols_to_warmup.is_empty() {
            info!("METRICS_CACHE: All symbols already in cache, skipping warmup");
            return Ok(());
        }

        info!(
            "METRICS_CACHE: Pre-warming {} symbols (not in cache): {:?}",
            symbols_to_warmup.len(),
            symbols_to_warmup
        );

        let mut success_count = 0;
        let mut error_count = 0;

        for symbol in &symbols_to_warmup {
            match self.update_symbol_metrics(symbol).await {
                Ok(_) => success_count += 1,
                Err(err) => {
                    error_count += 1;
                    warn!("METRICS_CACHE: Failed to pre-warm {}: {}", symbol, err);
                }
            }

            // Small delay to avoid rate limiting
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }

        info!(
            "METRICS_CACHE: Pre-warm completed: {} success, {} errors",
            success_count, error_count
        );

        // ✅ FIX: Update tracked symbols list (if not already set)
        // set_symbols() zaten çağrılmış olabilir, bu yüzden sadece eksik olanları ekle
        let mut tracked = self.symbols.write().await;
        for symbol in &symbols_to_warmup {
            if !tracked.contains(symbol) {
                tracked.push(symbol.clone());
            }
        }

        Ok(())
    }

    /// Get latest metrics for a symbol
    pub async fn get_metrics(&self, symbol: &str) -> Option<LatestMetrics> {
        let cache = self.cache.read().await;
        cache.get(symbol).cloned()
    }

    /// Update metrics for a single symbol
    async fn update_symbol_metrics(&self, symbol: &str) -> Result<()> {
        let futures_period = "5m"; // Default period

        // Fetch funding rate (latest)
        let funding_rates = self.client.fetch_funding_rates(symbol, 1).await?;
        let funding_rate = funding_rates
            .first()
            .map(|f| f.funding_rate.parse::<f64>().unwrap_or(0.0));

        // Fetch open interest (latest)
        let oi_hist = self
            .client
            .fetch_open_interest_hist(symbol, futures_period, 1)
            .await?;
        let open_interest = oi_hist.first().map(|o| o.open_interest);

        // Fetch long/short ratio (latest)
        let lsr_hist = self
            .client
            .fetch_top_long_short_ratio(symbol, futures_period, 1)
            .await?;
        let long_short_ratio = lsr_hist.first().map(|l| l.long_short_ratio);

        // Update cache
        let mut cache = self.cache.write().await;
        cache.insert(
            symbol.to_string(),
            LatestMetrics {
                symbol: symbol.to_string(),
                funding_rate,
                open_interest,
                long_short_ratio,
                last_update: Utc::now(),
            },
        );

        Ok(())
    }

    /// Update metrics for all tracked symbols
    pub async fn update_all_metrics(&self) -> Result<()> {
        let symbols = self.symbols.read().await.clone();

        if symbols.is_empty() {
            return Ok(());
        }

        info!(
            "METRICS_CACHE: Updating metrics for {} symbols",
            symbols.len()
        );

        let mut success_count = 0;
        let mut error_count = 0;

        for symbol in &symbols {
            match self.update_symbol_metrics(symbol).await {
                Ok(_) => success_count += 1,
                Err(err) => {
                    error_count += 1;
                    warn!("METRICS_CACHE: Failed to update {}: {}", symbol, err);
                }
            }

            // Small delay to avoid rate limiting
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }

        info!(
            "METRICS_CACHE: Update completed: {} success, {} errors",
            success_count, error_count
        );

        Ok(())
    }

    /// Background task: Periodically update metrics
    pub async fn run_update_task(&self) {
        let mut interval =
            tokio::time::interval(tokio::time::Duration::from_secs(self.update_interval_secs));

        // Initial update
        if let Err(err) = self.update_all_metrics().await {
            warn!("METRICS_CACHE: Initial update failed: {}", err);
        }

        loop {
            interval.tick().await;

            if let Err(err) = self.update_all_metrics().await {
                warn!("METRICS_CACHE: Periodic update failed: {}", err);
            }
        }
    }

    /// Get funding rate from cache
    pub async fn get_funding_rate(&self, symbol: &str) -> Option<f64> {
        self.get_metrics(symbol).await?.funding_rate
    }

    /// Get open interest from cache
    pub async fn get_open_interest(&self, symbol: &str) -> Option<f64> {
        self.get_metrics(symbol).await?.open_interest
    }

    /// Get long/short ratio from cache
    pub async fn get_long_short_ratio(&self, symbol: &str) -> Option<f64> {
        self.get_metrics(symbol).await?.long_short_ratio
    }

    /// Get all funding rates (for build_signal_contexts compatibility)
    pub async fn get_funding_rates(&self, symbol: &str, _limit: u32) -> Result<Vec<FundingRate>> {
        // Return cached funding rate as a single-item vector
        // This maintains compatibility with existing code
        if let Some(rate) = self.get_funding_rate(symbol).await {
            Ok(vec![FundingRate {
                _symbol: symbol.to_string(),
                funding_rate: rate.to_string(),
                funding_time: Utc::now().timestamp_millis(),
            }])
        } else {
            // Fallback: fetch from API if cache miss
            self.client.fetch_funding_rates(symbol, _limit).await
        }
    }

    /// Get open interest history (for build_signal_contexts compatibility)
    pub async fn get_open_interest_hist(
        &self,
        symbol: &str,
        period: &str,
        _limit: u32,
    ) -> Result<Vec<OpenInterestPoint>> {
        // Return cached OI as a single-item vector
        if let Some(oi) = self.get_open_interest(symbol).await {
            Ok(vec![OpenInterestPoint {
                timestamp: Utc::now(),
                open_interest: oi,
            }])
        } else {
            // Fallback: fetch from API if cache miss
            self.client
                .fetch_open_interest_hist(symbol, period, _limit)
                .await
        }
    }

    /// Get long/short ratio history (for build_signal_contexts compatibility)
    pub async fn get_top_long_short_ratio(
        &self,
        symbol: &str,
        period: &str,
        _limit: u32,
    ) -> Result<Vec<LongShortRatioPoint>> {
        // Return cached LSR as a single-item vector
        if let Some(lsr) = self.get_long_short_ratio(symbol).await {
            // Cache only stores ratio, not account percentages
            // Fetch from API to get complete data
            match self.client
                .fetch_top_long_short_ratio(symbol, period, _limit)
                .await
            {
                Ok(mut points) => {
                    // Update with cached ratio if available
                    if let Some(last_point) = points.last_mut() {
                        last_point.long_short_ratio = lsr;
                    }
                    Ok(points)
                }
                Err(_) => {
                    // If API fails, return cached ratio with neutral account percentages
                    // This is a fallback, but we prefer real API data
                    Ok(vec![LongShortRatioPoint {
                        timestamp: Utc::now(),
                        long_short_ratio: lsr,
                        long_account_pct: 50.0, // Neutral fallback (not from cache)
                        short_account_pct: 50.0, // Neutral fallback (not from cache)
                    }])
                }
            }
        } else {
            // No cache - fetch from API
            self.client
                .fetch_top_long_short_ratio(symbol, period, _limit)
                .await
        }
    }
}
