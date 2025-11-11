//location: /crates/app/src/config_simple.rs
// Simplified Config - Only essential fields from config.yaml

use anyhow::{anyhow, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct BinanceCfg {
    pub api_key: String,
    pub secret_key: String,
    #[serde(default = "default_recv_window")]
    pub recv_window_ms: u64,
    pub futures_base: String,
    #[serde(default = "default_hedge_mode")]
    pub hedge_mode: bool,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct WebsocketCfg {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_ws_reconnect_delay")]
    pub reconnect_delay_ms: u64,
    #[serde(default = "default_ws_ping_interval")]
    pub ping_interval_ms: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AppCfg {
    #[serde(default)]
    pub symbols: Vec<String>,
    #[serde(default = "default_quote_asset")]
    pub quote_asset: String,
    #[serde(default = "default_min_quote_balance_usd")]
    pub min_quote_balance_usd: f64,
    pub max_usd_per_order: f64,
    #[serde(default)]
    pub min_usd_per_order: Option<f64>,
    pub leverage: Option<u32>,
    pub price_tick: f64,
    pub qty_step: f64,
    pub binance: BinanceCfg,
    #[serde(default)]
    pub websocket: WebsocketCfg,
}

fn default_recv_window() -> u64 { 5000 }
fn default_hedge_mode() -> bool { false }
fn default_quote_asset() -> String { "USDC".to_string() }
fn default_min_quote_balance_usd() -> f64 { 1.0 }
fn default_ws_reconnect_delay() -> u64 { 5000 }
fn default_ws_ping_interval() -> u64 { 30000 }

/// Load configuration from config.yaml
pub fn load_config() -> Result<AppCfg> {
    let args: Vec<String> = std::env::args().collect();
    let path = args
        .windows(2)
        .find_map(|w| {
            if w[0] == "--config" {
                Some(w[1].clone())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "./config.yaml".to_string());
    
    let content = std::fs::read_to_string(&path)?;
    let cfg: AppCfg = serde_yaml::from_str(&content)?;
    
    validate_config(&cfg)?;
    Ok(cfg)
}

/// Validate configuration values
fn validate_config(cfg: &AppCfg) -> Result<()> {
    if cfg.price_tick <= 0.0 {
        return Err(anyhow!("price_tick must be positive"));
    }
    if cfg.qty_step <= 0.0 {
        return Err(anyhow!("qty_step must be positive"));
    }
    if cfg.max_usd_per_order <= 0.0 {
        return Err(anyhow!("max_usd_per_order must be positive"));
    }
    
    if cfg.binance.api_key.trim().is_empty() {
        return Err(anyhow!("binance.api_key is required"));
    }
    if cfg.binance.secret_key.trim().is_empty() {
        return Err(anyhow!("binance.secret_key is required"));
    }
    
    Ok(())
}

