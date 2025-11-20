// ✅ ADIM 3: Multi-symbol scanner modülü (TrendPlan.md)
// USDT/USDC universe için symbol discovery ve selection

use anyhow::Result;
use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::types::FuturesClient;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct SymbolMetrics {
    pub symbol: String,
    pub quote_asset: String,
    pub price: f64,
    pub price_change_24h_pct: f64,
    pub quote_volume_24h: f64,
    pub trades_24h: u64,
    pub bid_price: f64,
    pub ask_price: f64,
    pub spread_bps: f64, // Bid-ask spread in basis points
    pub volatility_pct: f64, // Price change % (absolute)
}

#[derive(Debug, Clone)]
pub struct SymbolScore {
    pub symbol: String,
    pub total_score: f64,
    pub volatility_score: f64,
    pub volume_score: f64,
    pub trades_score: f64,
    pub spread_score: f64,
}

#[derive(Debug, Clone)]
pub struct SymbolSelectionConfig {
    pub enabled: bool,
    pub max_symbols: usize,
    pub rotation_interval_minutes: u64,
    pub min_volatility_pct: f64,
    pub min_quote_volume: f64,
    pub min_trades_24h: u64,
    pub volatility_weight: f64,
    pub volume_weight: f64,
    pub trades_weight: f64,
    pub spread_weight: f64,
    pub allowed_quotes: Vec<String>, // ["USDT", "USDC"]
}

pub struct SymbolScanner {
    client: FuturesClient,
    config: SymbolSelectionConfig,
    selected_symbols: Arc<RwLock<Vec<String>>>,
    last_update: Arc<RwLock<Option<DateTime<Utc>>>>,
}

#[derive(Debug, Deserialize)]
struct ExchangeInfoResponse {
    symbols: Vec<SymbolInfo>,
}

#[derive(Debug, Deserialize)]
struct SymbolInfo {
    symbol: String,
    quoteAsset: String,
    status: String,
    contractType: String, // PERPETUAL, CURRENT_QUARTER, etc.
}

#[derive(Debug, Deserialize)]
struct Ticker24hr {
    symbol: String,
    priceChangePercent: String,
    quoteVolume: String,
    count: String, // 24h trades
    bidPrice: String,
    askPrice: String,
    lastPrice: String,
}

impl SymbolScanner {
    pub fn new(config: SymbolSelectionConfig) -> Self {
        Self {
            client: FuturesClient::new(),
            config,
            selected_symbols: Arc::new(RwLock::new(Vec::new())),
            last_update: Arc::new(RwLock::new(None)),
        }
    }

    /// Discover all USDT/USDC perpetual futures symbols
    pub async fn discover_symbols(&self) -> Result<Vec<String>> {
        let url = format!("{}/fapi/v1/exchangeInfo", self.client.base_url.as_str());
        let response = self.client.http.get(&url).send().await?;
        
        if !response.status().is_success() {
            anyhow::bail!("ExchangeInfo error: {}", response.text().await?);
        }

        let exchange_info: ExchangeInfoResponse = response.json().await?;
        
        let mut symbols = Vec::new();
        for symbol_info in exchange_info.symbols {
            // Filter: PERPETUAL futures only
            if symbol_info.contractType != "PERPETUAL" {
                continue;
            }
            
            // Filter: Only trading symbols
            if symbol_info.status != "TRADING" {
                continue;
            }
            
            // Filter: Allowed quote assets (USDT, USDC)
            if !self.config.allowed_quotes.contains(&symbol_info.quoteAsset) {
                continue;
            }
            
            symbols.push(symbol_info.symbol);
        }
        
        info!("SYMBOL_SCANNER: Discovered {} symbols", symbols.len());
        Ok(symbols)
    }

    /// Fetch 24h ticker data for symbols
    pub async fn fetch_ticker_24hr(&self, symbols: &[String]) -> Result<HashMap<String, SymbolMetrics>> {
        let url = format!("{}/fapi/v1/ticker/24hr", self.client.base_url.as_str());
        let response = self.client.http.get(&url).send().await?;
        
        if !response.status().is_success() {
            anyhow::bail!("Ticker24hr error: {}", response.text().await?);
        }

        let tickers: Vec<Ticker24hr> = response.json().await?;
        let mut metrics_map = HashMap::new();
        
        for ticker in tickers {
            // Only process requested symbols
            if !symbols.contains(&ticker.symbol) {
                continue;
            }
            
            let price = ticker.lastPrice.parse::<f64>().unwrap_or(0.0);
            let price_change_pct = ticker.priceChangePercent.parse::<f64>().unwrap_or(0.0);
            let quote_volume = ticker.quoteVolume.parse::<f64>().unwrap_or(0.0);
            let trades_24h = ticker.count.parse::<u64>().unwrap_or(0);
            let bid_price = ticker.bidPrice.parse::<f64>().unwrap_or(0.0);
            let ask_price = ticker.askPrice.parse::<f64>().unwrap_or(0.0);
            
            // Calculate spread in basis points
            let spread_bps = if price > 0.0 && bid_price > 0.0 && ask_price > 0.0 {
                ((ask_price - bid_price) / price) * 10000.0
            } else {
                10000.0 // Very high spread if invalid
            };
            
            // Extract quote asset from symbol (last 4 chars: USDT or USDC)
            let quote_asset = if ticker.symbol.ends_with("USDT") {
                "USDT".to_string()
            } else if ticker.symbol.ends_with("USDC") {
                "USDC".to_string()
            } else {
                continue; // Skip if not USDT/USDC
            };
            
            metrics_map.insert(
                ticker.symbol.clone(),
                SymbolMetrics {
                    symbol: ticker.symbol,
                    quote_asset,
                    price,
                    price_change_24h_pct: price_change_pct,
                    quote_volume_24h: quote_volume,
                    trades_24h,
                    bid_price,
                    ask_price,
                    spread_bps,
                    volatility_pct: price_change_pct.abs(),
                },
            );
        }
        
        Ok(metrics_map)
    }

    /// Score symbols based on config weights
    pub fn score_symbols(
        &self,
        metrics: &HashMap<String, SymbolMetrics>,
    ) -> Vec<SymbolScore> {
        if metrics.is_empty() {
            return Vec::new();
        }
        
        // Find max values for normalization
        let max_volatility = metrics
            .values()
            .map(|m| m.volatility_pct)
            .fold(0.0, f64::max);
        let max_volume = metrics
            .values()
            .map(|m| m.quote_volume_24h)
            .fold(0.0, f64::max);
        let max_trades = metrics
            .values()
            .map(|m| m.trades_24h as f64)
            .fold(0.0, f64::max);
        let min_spread = metrics
            .values()
            .map(|m| m.spread_bps)
            .fold(f64::MAX, f64::min);
        let max_spread = metrics
            .values()
            .map(|m| m.spread_bps)
            .fold(0.0, f64::max);
        
        let mut scores = Vec::new();
        
        for (symbol, metric) in metrics {
            // Apply minimum filters
            if metric.volatility_pct < self.config.min_volatility_pct {
                continue;
            }
            if metric.quote_volume_24h < self.config.min_quote_volume {
                continue;
            }
            if metric.trades_24h < self.config.min_trades_24h {
                continue;
            }
            
            // Normalize and score (0-1 range, higher is better)
            let volatility_score = if max_volatility > 0.0 {
                metric.volatility_pct / max_volatility
            } else {
                0.0
            };
            
            let volume_score = if max_volume > 0.0 {
                metric.quote_volume_24h / max_volume
            } else {
                0.0
            };
            
            let trades_score = if max_trades > 0.0 {
                metric.trades_24h as f64 / max_trades
            } else {
                0.0
            };
            
            // Spread: lower is better, so invert
            let spread_score = if max_spread > min_spread {
                1.0 - ((metric.spread_bps - min_spread) / (max_spread - min_spread))
            } else {
                1.0
            };
            
            // Weighted total score
            let total_score = 
                volatility_score * self.config.volatility_weight +
                volume_score * self.config.volume_weight +
                trades_score * self.config.trades_weight +
                spread_score * self.config.spread_weight;
            
            scores.push(SymbolScore {
                symbol: symbol.clone(),
                total_score,
                volatility_score,
                volume_score,
                trades_score,
                spread_score,
            });
        }
        
        // Sort by total score (descending)
        scores.sort_by(|a, b| b.total_score.partial_cmp(&a.total_score).unwrap());
        
        scores
    }

    /// Select top N symbols based on scoring
    pub async fn select_symbols(&self) -> Result<Vec<String>> {
        // Discover all symbols
        let all_symbols = self.discover_symbols().await?;
        
        // Fetch metrics
        let metrics = self.fetch_ticker_24hr(&all_symbols).await?;
        
        // Score symbols
        let scores = self.score_symbols(&metrics);
        
        // Select top N
        let selected: Vec<String> = scores
            .into_iter()
            .take(self.config.max_symbols)
            .map(|s| s.symbol)
            .collect();
        
        info!(
            "SYMBOL_SCANNER: Selected {} symbols from {} candidates",
            selected.len(),
            metrics.len()
        );
        
        // Update cache
        {
            let mut cached = self.selected_symbols.write().await;
            *cached = selected.clone();
        }
        {
            let mut last = self.last_update.write().await;
            *last = Some(Utc::now());
        }
        
        Ok(selected)
    }

    /// Get currently selected symbols (from cache)
    pub async fn get_selected_symbols(&self) -> Vec<String> {
        self.selected_symbols.read().await.clone()
    }

    /// Check if rotation is needed
    pub async fn should_rotate(&self) -> bool {
        if !self.config.enabled {
            return false;
        }
        
        let last = self.last_update.read().await;
        if let Some(last_update) = *last {
            let now = Utc::now();
            let elapsed_minutes = (now - last_update).num_minutes() as u64;
            elapsed_minutes >= self.config.rotation_interval_minutes
        } else {
            true // Never updated, need initial selection
        }
    }

    /// Background task: Periodically update symbol selection
    pub async fn run_rotation_task(&self) {
        let mut interval = tokio::time::interval(
            tokio::time::Duration::from_secs(self.config.rotation_interval_minutes * 60),
        );
        
        loop {
            interval.tick().await;
            
            if !self.config.enabled {
                continue;
            }
            
            match self.select_symbols().await {
                Ok(symbols) => {
                    info!("SYMBOL_SCANNER: Rotation completed, {} symbols selected", symbols.len());
                }
                Err(err) => {
                    warn!("SYMBOL_SCANNER: Rotation failed: {}", err);
                }
            }
        }
    }
}

// Helper function to create config from FileConfig
impl SymbolSelectionConfig {
    pub fn from_file_config(
        file_cfg: &crate::types::FileConfig,
        allowed_quotes: Vec<String>,
    ) -> Self {
        let selection_cfg = file_cfg.dynamic_symbol_selection.as_ref();
        
        Self {
            enabled: selection_cfg
                .and_then(|s| s.enabled)
                .unwrap_or(false),
            max_symbols: selection_cfg
                .and_then(|s| s.max_symbols)
                .unwrap_or(30),
            rotation_interval_minutes: selection_cfg
                .and_then(|s| s.rotation_interval_minutes)
                .unwrap_or(15),
            min_volatility_pct: selection_cfg
                .and_then(|s| s.min_volatility_pct)
                .unwrap_or(1.5),
            min_quote_volume: selection_cfg
                .and_then(|s| s.min_quote_volume)
                .unwrap_or(1_000_000.0),
            min_trades_24h: selection_cfg
                .and_then(|s| s.min_trades_24h)
                .unwrap_or(1000),
            volatility_weight: selection_cfg
                .and_then(|s| s.volatility_weight)
                .unwrap_or(2.0),
            volume_weight: selection_cfg
                .and_then(|s| s.volume_weight)
                .unwrap_or(1.0),
            trades_weight: selection_cfg
                .and_then(|s| s.trades_weight)
                .unwrap_or(0.5),
            spread_weight: selection_cfg
                .and_then(|s| s.spread_weight)
                .unwrap_or(1.0),
            allowed_quotes,
        }
    }
}

