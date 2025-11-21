// ✅ CRITICAL FIX: Fast Order Execution (TrendPlan.md)
// Pre-cached symbol information and depth data to eliminate API calls during order execution
// Target: Signal → Fill in <50ms (currently ~500-1000ms)

use crate::types::Connection;
use chrono::Utc;
use log::info;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Pre-cached symbol information
/// Eliminates API calls during order execution
#[derive(Clone)]
pub struct SymbolInfoCache {
    cache: Arc<RwLock<HashMap<String, CachedSymbolInfo>>>,
}

#[derive(Clone)]
pub struct CachedSymbolInfo {
    pub step_size: f64,
    pub min_quantity: f64,
    pub max_quantity: f64,
    pub tick_size: f64,
    pub leverage: f64,
    pub margin_mode_set: bool,
    pub last_update: chrono::DateTime<chrono::Utc>,
}

impl SymbolInfoCache {
    pub fn new() -> Self {
        Self {
            cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Warm up cache on startup (do this ONCE, not per order!)
    pub async fn warmup(&self, symbols: &[String], connection: &Connection) {
        info!("CACHE: Warming up symbol info cache for {} symbols", symbols.len());
        for symbol in symbols {
            if let Ok(info) = connection.fetch_symbol_info(symbol).await {
                let mut cache = self.cache.write().await;
                cache.insert(
                    symbol.clone(),
                    CachedSymbolInfo {
                        step_size: info.step_size,
                        min_quantity: info.min_quantity,
                        max_quantity: info.max_quantity,
                        tick_size: info.tick_size,
                        leverage: 20.0, // Default
                        margin_mode_set: false,
                        last_update: Utc::now(),
                    },
                );
                info!("CACHE: Cached symbol info for {}", symbol);
            } else {
                log::warn!("CACHE: Failed to fetch symbol info for {}", symbol);
            }
        }
        info!("CACHE: Symbol info cache warmup complete");
    }

    /// Get cached info (no API call!)
    pub async fn get(&self, symbol: &str) -> Option<CachedSymbolInfo> {
        let cache = self.cache.read().await;
        cache.get(symbol).cloned()
    }

    /// Update leverage in cache (after set_leverage API call)
    pub async fn update_leverage(&self, symbol: &str, leverage: f64) {
        let mut cache = self.cache.write().await;
        if let Some(info) = cache.get_mut(symbol) {
            info.leverage = leverage;
            info.last_update = Utc::now();
        }
    }

    /// Mark margin mode as set
    pub async fn mark_margin_mode_set(&self, symbol: &str) {
        let mut cache = self.cache.write().await;
        if let Some(info) = cache.get_mut(symbol) {
            info.margin_mode_set = true;
            info.last_update = Utc::now();
        }
    }

    /// Check if margin mode is set (to avoid redundant API calls)
    pub async fn is_margin_mode_set(&self, symbol: &str) -> bool {
        let cache = self.cache.read().await;
        cache.get(symbol).map(|info| info.margin_mode_set).unwrap_or(false)
    }

    /// Get cached leverage
    pub async fn get_leverage(&self, symbol: &str) -> Option<f64> {
        let cache = self.cache.read().await;
        cache.get(symbol).map(|info| info.leverage)
    }
}

/// Real-time depth cache from WebSocket
/// Eliminates API calls for depth data
#[derive(Clone)]
pub struct DepthCache {
    depths: Arc<RwLock<HashMap<String, BestPrices>>>,
}

#[derive(Clone)]
struct BestPrices {
    best_bid: f64,
    best_ask: f64,
    last_update: std::time::Instant,
}

impl DepthCache {
    pub fn new() -> Self {
        Self {
            depths: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Update from WebSocket depth event
    pub async fn update(&self, symbol: String, bid: f64, ask: f64) {
        let mut depths = self.depths.write().await;
        depths.insert(
            symbol,
            BestPrices {
                best_bid: bid,
                best_ask: ask,
                last_update: std::time::Instant::now(),
            },
        );
    }

    /// Get best prices (no API call!)
    pub async fn get_best_prices(&self, symbol: &str) -> Option<(f64, f64)> {
        let depths = self.depths.read().await;
        let prices = depths.get(symbol)?;
        
        // Check if data is fresh (<1 second old)
        if prices.last_update.elapsed().as_millis() < 1000 {
            Some((prices.best_bid, prices.best_ask))
        } else {
            None // Stale data
        }
    }

    /// Check if depth data is available and fresh
    pub async fn is_available(&self, symbol: &str) -> bool {
        self.get_best_prices(symbol).await.is_some()
    }
}

