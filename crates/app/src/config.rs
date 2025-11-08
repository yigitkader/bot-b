//location: /crates/app/src/config.rs
// Configuration structures and loading logic

use anyhow::{anyhow, Result};
use rust_decimal::Decimal;
use serde::Deserialize;

// ============================================================================
// Configuration Structures
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct RiskCfg {
    pub inv_cap: String,
    pub min_liq_gap_bps: f64,
    pub dd_limit_bps: i64,
    pub max_leverage: u32,
}

#[derive(Debug, Deserialize)]
pub struct StratCfg {
    pub r#type: String,
    pub a: f64,
    pub b: f64,
    pub base_size: String,
    #[serde(default)]
    pub inv_cap: Option<String>,
    // Spread ve Fiyatlama Eşikleri
    #[serde(default)]
    pub min_spread_bps: Option<f64>,
    #[serde(default)]
    pub max_spread_bps: Option<f64>,
    #[serde(default)]
    pub spread_arbitrage_min_bps: Option<f64>,
    #[serde(default)]
    pub spread_arbitrage_max_bps: Option<f64>,
    // Trend Takibi Eşikleri
    #[serde(default)]
    pub strong_trend_bps: Option<f64>,
    #[serde(default)]
    pub momentum_strong_bps: Option<f64>,
    #[serde(default)]
    pub trend_bias_multiplier: Option<f64>,
    // Adverse Selection Eşikleri
    #[serde(default)]
    pub adverse_selection_threshold_on: Option<f64>,
    #[serde(default)]
    pub adverse_selection_threshold_off: Option<f64>,
    // Fırsat Modu Eşikleri
    #[serde(default)]
    pub opportunity_threshold_on: Option<f64>,
    #[serde(default)]
    pub opportunity_threshold_off: Option<f64>,
    // Manipülasyon Tespit Eşikleri
    #[serde(default)]
    pub price_jump_threshold_bps: Option<f64>,
    #[serde(default)]
    pub fake_breakout_threshold_bps: Option<f64>,
    #[serde(default)]
    pub liquidity_drop_threshold: Option<f64>,
    // Envanter Yönetimi
    #[serde(default)]
    pub inventory_threshold_ratio: Option<f64>,
    // Adaptif Spread Katsayıları
    #[serde(default)]
    pub volatility_coefficient: Option<f64>,
    #[serde(default)]
    pub ofi_coefficient: Option<f64>,
    // Diğer
    #[serde(default)]
    pub min_liquidity_required: Option<f64>,
    #[serde(default)]
    pub opportunity_size_multiplier: Option<f64>,
    #[serde(default)]
    pub strong_trend_multiplier: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub struct ExecCfg {
    pub tif: String,
    pub venue: String,
    #[serde(default = "default_cancel_interval")]
    pub cancel_replace_interval_ms: u64,
    #[serde(default = "default_max_order_age")]
    pub max_order_age_ms: u64,
}

#[derive(Debug, Deserialize, Default)]
pub struct WebsocketCfg {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_ws_reconnect_delay")]
    pub reconnect_delay_ms: u64,
    #[serde(default = "default_ws_ping_interval")]
    pub ping_interval_ms: u64,
}

#[derive(Debug, Deserialize, Default)]
pub struct InternalCfg {
    #[serde(default = "default_pnl_history_max_len")]
    pub pnl_history_max_len: usize,
    #[serde(default = "default_position_size_history_max_len")]
    pub position_size_history_max_len: usize,
    #[serde(default = "default_max_symbols_per_tick")]
    pub max_symbols_per_tick: usize,
    #[serde(default = "default_rate_limiter_safety_factor")]
    pub rate_limiter_safety_factor: f64,
    #[serde(default = "default_rate_limiter_min_interval_ms")]
    pub rate_limiter_min_interval_ms: u64,
    #[serde(default = "default_order_sync_interval_sec")]
    pub order_sync_interval_sec: u64,
    #[serde(default = "default_cancel_stagger_delay_ms")]
    pub cancel_stagger_delay_ms: u64,
    #[serde(default = "default_fill_rate_increase_factor")]
    pub fill_rate_increase_factor: f64,
    #[serde(default = "default_fill_rate_increase_bonus")]
    pub fill_rate_increase_bonus: f64,
    #[serde(default = "default_fill_rate_decrease_factor")]
    pub fill_rate_decrease_factor: f64,
    #[serde(default = "default_fill_rate_slow_decrease_factor")]
    pub fill_rate_slow_decrease_factor: f64,
    #[serde(default = "default_fill_rate_slow_decrease_bonus")]
    pub fill_rate_slow_decrease_bonus: f64,
    #[serde(default = "default_order_price_distance_with_position")]
    pub order_price_distance_with_position: f64,
    #[serde(default = "default_order_price_distance_no_position")]
    pub order_price_distance_no_position: f64,
    #[serde(default = "default_order_price_change_threshold")]
    pub order_price_change_threshold: f64,
    #[serde(default = "default_inventory_reconcile_threshold")]
    pub inventory_reconcile_threshold: String,
    #[serde(default = "default_position_qty_threshold")]
    pub position_qty_threshold: String,
    #[serde(default = "default_pnl_alert_interval_sec")]
    pub pnl_alert_interval_sec: u64,
    #[serde(default = "default_pnl_alert_threshold_positive")]
    pub pnl_alert_threshold_positive: f64,
    #[serde(default = "default_pnl_alert_threshold_negative")]
    pub pnl_alert_threshold_negative: f64,
    #[serde(default = "default_take_profit_threshold_small")]
    pub take_profit_threshold_small: f64,
    #[serde(default = "default_take_profit_threshold_large")]
    pub take_profit_threshold_large: f64,
    #[serde(default = "default_take_profit_time_threshold_ms")]
    pub take_profit_time_threshold_ms: u64,
    #[serde(default = "default_take_profit_min_profit_threshold")]
    pub take_profit_min_profit_threshold: f64,
    #[serde(default = "default_trailing_stop_peak_threshold_large")]
    pub trailing_stop_peak_threshold_large: f64,
    #[serde(default = "default_trailing_stop_peak_threshold_medium")]
    pub trailing_stop_peak_threshold_medium: f64,
    #[serde(default = "default_trailing_stop_drawdown_large")]
    pub trailing_stop_drawdown_large: f64,
    #[serde(default = "default_trailing_stop_drawdown_medium")]
    pub trailing_stop_drawdown_medium: f64,
    #[serde(default = "default_trailing_stop_drawdown_small")]
    pub trailing_stop_drawdown_small: f64,
    #[serde(default = "default_trailing_stop_min_peak")]
    pub trailing_stop_min_peak: f64,
    #[serde(default = "default_stop_loss_threshold")]
    pub stop_loss_threshold: f64,
    #[serde(default = "default_stop_loss_time_threshold_ms")]
    pub stop_loss_time_threshold_ms: u64,
    #[serde(default = "default_stop_loss_trend_threshold")]
    pub stop_loss_trend_threshold: f64,
    #[serde(default = "default_max_position_size_buffer")]
    pub max_position_size_buffer: f64,
}

#[derive(Debug, Deserialize)]
pub struct BinanceCfg {
    pub api_key: String,
    pub secret_key: String,
    #[serde(default = "default_recv_window")]
    pub recv_window_ms: u64,
    pub spot_base: String,
    pub futures_base: String,
}

#[derive(Debug, Deserialize)]
pub struct AppCfg {
    #[serde(default)]
    pub symbol: Option<String>,
    #[serde(default)]
    pub symbols: Vec<String>,
    #[serde(default, alias = "auto_discover_usdt")]
    pub auto_discover_quote: bool,
    #[serde(default = "default_quote_asset")]
    pub quote_asset: String,
    pub mode: String,
    #[serde(default)]
    pub metrics_port: Option<u16>,
    pub max_usd_per_order: f64,
    #[serde(default)]
    pub min_usd_per_order: Option<f64>,
    #[serde(default = "default_min_quote_balance_usd")]
    pub min_quote_balance_usd: f64,
    pub leverage: Option<u32>,
    pub price_tick: f64,
    pub qty_step: f64,
    pub binance: BinanceCfg,
    pub risk: RiskCfg,
    pub strategy: StratCfg,
    pub exec: ExecCfg,
    #[serde(default)]
    pub websocket: WebsocketCfg,
    #[serde(default)]
    pub internal: InternalCfg,
    #[serde(default)]
    pub strategy_internal: StrategyInternalCfg,
}

#[derive(Debug, Deserialize, Default)]
pub struct StrategyInternalCfg {
    #[serde(default = "default_manipulation_volume_ratio_threshold")]
    pub manipulation_volume_ratio_threshold: f64,
    #[serde(default = "default_manipulation_time_threshold_ms")]
    pub manipulation_time_threshold_ms: u64,
    #[serde(default = "default_manipulation_price_history_min_len")]
    pub manipulation_price_history_min_len: usize,
    #[serde(default = "default_manipulation_price_history_max_len")]
    pub manipulation_price_history_max_len: usize,
    #[serde(default = "default_confidence_price_drop_max")]
    pub confidence_price_drop_max: f64,
    #[serde(default = "default_confidence_volume_ratio_min")]
    pub confidence_volume_ratio_min: f64,
    #[serde(default = "default_confidence_volume_ratio_max")]
    pub confidence_volume_ratio_max: f64,
    #[serde(default = "default_confidence_spread_min")]
    pub confidence_spread_min: f64,
    #[serde(default = "default_confidence_spread_max")]
    pub confidence_spread_max: f64,
    #[serde(default = "default_confidence_bonus_multiplier")]
    pub confidence_bonus_multiplier: f64,
    #[serde(default = "default_confidence_max_multiplier")]
    pub confidence_max_multiplier: f64,
    #[serde(default = "default_trend_analysis_min_history")]
    pub trend_analysis_min_history: usize,
    #[serde(default = "default_trend_analysis_threshold_negative")]
    pub trend_analysis_threshold_negative: f64,
    #[serde(default = "default_trend_analysis_threshold_strong_negative")]
    pub trend_analysis_threshold_strong_negative: f64,
}

// ============================================================================
// Default Value Functions
// ============================================================================

fn default_cancel_interval() -> u64 { 1_000 }
fn default_max_order_age() -> u64 { 10_000 }
fn default_ws_reconnect_delay() -> u64 { 5_000 }
fn default_ws_ping_interval() -> u64 { 30_000 }
fn default_pnl_history_max_len() -> usize { 1024 }
fn default_position_size_history_max_len() -> usize { 100 }
fn default_max_symbols_per_tick() -> usize { 8 }
fn default_rate_limiter_safety_factor() -> f64 { 0.8 }
fn default_rate_limiter_min_interval_ms() -> u64 { 1000 }
fn default_order_sync_interval_sec() -> u64 { 2 }
fn default_cancel_stagger_delay_ms() -> u64 { 50 }
fn default_fill_rate_increase_factor() -> f64 { 0.95 }
fn default_fill_rate_increase_bonus() -> f64 { 0.05 }
fn default_fill_rate_decrease_factor() -> f64 { 0.98 }
fn default_fill_rate_slow_decrease_factor() -> f64 { 0.995 }
fn default_fill_rate_slow_decrease_bonus() -> f64 { 0.005 }
fn default_order_price_distance_with_position() -> f64 { 0.01 }
fn default_order_price_distance_no_position() -> f64 { 0.005 }
fn default_order_price_change_threshold() -> f64 { 0.0001 }
fn default_inventory_reconcile_threshold() -> String { "0.00000001".to_string() }
fn default_position_qty_threshold() -> String { "0.00000001".to_string() }
fn default_pnl_alert_interval_sec() -> u64 { 10 }
fn default_pnl_alert_threshold_positive() -> f64 { 0.05 }
fn default_pnl_alert_threshold_negative() -> f64 { -0.03 }
fn default_take_profit_threshold_small() -> f64 { 0.01 }
fn default_take_profit_threshold_large() -> f64 { 0.05 }
fn default_take_profit_time_threshold_ms() -> u64 { 600_000 }
fn default_take_profit_min_profit_threshold() -> f64 { 0.10 }
fn default_trailing_stop_peak_threshold_large() -> f64 { 100.0 }
fn default_trailing_stop_peak_threshold_medium() -> f64 { 20.0 }
fn default_trailing_stop_drawdown_large() -> f64 { 0.03 }
fn default_trailing_stop_drawdown_medium() -> f64 { 0.015 }
fn default_trailing_stop_drawdown_small() -> f64 { 0.01 }
fn default_trailing_stop_min_peak() -> f64 { 0.01 }
fn default_stop_loss_threshold() -> f64 { -0.005 }
fn default_stop_loss_time_threshold_ms() -> u64 { 120_000 }
fn default_stop_loss_trend_threshold() -> f64 { -0.2 }
fn default_max_position_size_buffer() -> f64 { 5.0 }
fn default_recv_window() -> u64 { 5_000 }
fn default_quote_asset() -> String { "USDC".to_string() }
fn default_min_quote_balance_usd() -> f64 { 1.0 }
fn default_manipulation_volume_ratio_threshold() -> f64 { 5.0 }
fn default_manipulation_time_threshold_ms() -> u64 { 2000 }
fn default_manipulation_price_history_min_len() -> usize { 3 }
fn default_manipulation_price_history_max_len() -> usize { 200 }
fn default_confidence_price_drop_max() -> f64 { 500.0 }
fn default_confidence_volume_ratio_min() -> f64 { 5.0 }
fn default_confidence_volume_ratio_max() -> f64 { 10.0 }
fn default_confidence_spread_min() -> f64 { 50.0 }
fn default_confidence_spread_max() -> f64 { 150.0 }
fn default_confidence_bonus_multiplier() -> f64 { 0.3 }
fn default_confidence_max_multiplier() -> f64 { 1.5 }
fn default_trend_analysis_min_history() -> usize { 10 }
fn default_trend_analysis_threshold_negative() -> f64 { -0.15 }
fn default_trend_analysis_threshold_strong_negative() -> f64 { -0.20 }

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
    Ok(())
}

