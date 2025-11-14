// Configuration structures and loading logic
// Clean architecture - minimal config for event-driven modules

use anyhow::{anyhow, Result};
use serde::Deserialize;

// ============================================================================
// Configuration Structures
// ============================================================================

#[derive(Debug, Deserialize, Clone, Default)]
pub struct RiskCfg {
    #[serde(default = "default_max_leverage")]
    pub max_leverage: u32,
    #[serde(default = "default_use_isolated_margin")]
    pub use_isolated_margin: bool,
    #[serde(default = "default_max_position_notional_usd")]
    pub max_position_notional_usd: f64,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct TrendingCfg {
    #[serde(default = "default_min_spread_bps")]
    pub min_spread_bps: f64,
    #[serde(default = "default_max_spread_bps")]
    pub max_spread_bps: f64,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct ExecCfg {
    #[serde(default = "default_tif")]
    pub tif: String,
    #[serde(default = "default_default_leverage")]
    pub default_leverage: u32,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct WebsocketCfg {
    #[serde(default = "default_ws_enabled")]
    pub enabled: bool,
    #[serde(default = "default_ws_reconnect_delay")]
    pub reconnect_delay_ms: u64,
    #[serde(default = "default_ws_ping_interval")]
    pub ping_interval_ms: u64,
}

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

#[derive(Debug, Deserialize, Clone)]
pub struct AppCfg {
    #[serde(default)]
    pub symbol: Option<String>,
    #[serde(default)]
    pub symbols: Vec<String>,
    #[serde(default = "default_auto_discover_quote")]
    pub auto_discover_quote: bool,
    #[serde(default = "default_quote_asset")]
    pub quote_asset: String,
    #[serde(default = "default_allow_usdt_quote")]
    pub allow_usdt_quote: bool,
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default = "default_max_usd_per_order")]
    pub max_usd_per_order: f64,
    #[serde(default = "default_min_usd_per_order")]
    pub min_usd_per_order: f64,
    #[serde(default = "default_min_quote_balance_usd")]
    pub min_quote_balance_usd: f64,
    #[serde(default)]
    pub leverage: Option<u32>,
    #[serde(default = "default_price_tick")]
    pub price_tick: f64,
    #[serde(default = "default_qty_step")]
    pub qty_step: f64,
    #[serde(default = "default_take_profit_pct")]
    pub take_profit_pct: f64,
    #[serde(default = "default_stop_loss_pct")]
    pub stop_loss_pct: f64,
    pub binance: BinanceCfg,
    #[serde(default)]
    pub risk: RiskCfg,
    #[serde(default)]
    pub trending: TrendingCfg,
    #[serde(default)]
    pub exec: ExecCfg,
    #[serde(default)]
    pub websocket: WebsocketCfg,
}

// ============================================================================
// Default Value Functions
// ============================================================================

fn default_max_leverage() -> u32 {
    50
}

fn default_use_isolated_margin() -> bool {
    true
}

fn default_max_position_notional_usd() -> f64 {
    1000.0
}

fn default_min_spread_bps() -> f64 {
    5.0
}

fn default_max_spread_bps() -> f64 {
    200.0
}

fn default_tif() -> String {
    "post_only".to_string()
}

fn default_default_leverage() -> u32 {
    20
}

fn default_ws_enabled() -> bool {
    true
}

fn default_ws_reconnect_delay() -> u64 {
    5_000
}

fn default_ws_ping_interval() -> u64 {
    30_000
}

fn default_recv_window() -> u64 {
    5_000
}

fn default_hedge_mode() -> bool {
    false
}

fn default_auto_discover_quote() -> bool {
    true
}

fn default_quote_asset() -> String {
    "USDC".to_string()
}

fn default_allow_usdt_quote() -> bool {
    true
}

fn default_mode() -> String {
    "futures".to_string()
}

fn default_max_usd_per_order() -> f64 {
    100.0
}

fn default_min_usd_per_order() -> f64 {
    10.0
}

fn default_min_quote_balance_usd() -> f64 {
    1.0
}

fn default_price_tick() -> f64 {
    0.001
}

fn default_qty_step() -> f64 {
    0.001
}

fn default_take_profit_pct() -> f64 {
    5.0
}

fn default_stop_loss_pct() -> f64 {
    2.0
}

// ============================================================================
// Configuration Loading
// ============================================================================

/// Load configuration from file or command line arguments
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

    // Quote asset validation: Only USDC or USDT accepted
    let quote_upper = cfg.quote_asset.to_uppercase();
    if quote_upper != "USDC" && quote_upper != "USDT" {
        return Err(anyhow!(
            "quote_asset must be either 'USDC' or 'USDT', got '{}'. Only USDC and USDT are supported.",
            cfg.quote_asset
        ));
    }

    // API key validation
    if cfg.binance.api_key.trim().is_empty() {
        return Err(anyhow!(
            "binance.api_key is required but is empty. Please set your API key in config.yaml"
        ));
    }
    if cfg.binance.secret_key.trim().is_empty() {
        return Err(anyhow!(
            "binance.secret_key is required but is empty. Please set your secret key in config.yaml"
        ));
    }

    // API key format check (Binance API keys are typically 64 characters)
    if cfg.binance.api_key.len() < 20 {
        return Err(anyhow!(
            "binance.api_key appears to be invalid (too short). Binance API keys are typically 64 characters long"
        ));
    }
    if cfg.binance.secret_key.len() < 20 {
        return Err(anyhow!(
            "binance.secret_key appears to be invalid (too short). Binance secret keys are typically 64 characters long"
        ));
    }

    Ok(())
}
