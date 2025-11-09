//location: /crates/app/src/config.rs
// Configuration structures and loading logic

use anyhow::{anyhow, Result};
// Removed unused import
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
    #[serde(default = "default_slippage_bps_reserve")]
    pub slippage_bps_reserve: f64, // Slipaj tamponu (bps) - spread hesaplamasından çıkarılır
    #[serde(default = "default_use_isolated_margin")]
    pub use_isolated_margin: bool, // Isolated margin kullan (default: true)
    #[serde(default = "default_max_open_chunks_per_symbol_per_side")]
    pub max_open_chunks_per_symbol_per_side: usize, // Sembol başına yönde maksimum açık chunk sayısı
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
    pub min_24h_volume_usd: Option<f64>,
    #[serde(default)]
    pub min_book_depth_usd: Option<f64>,
    #[serde(default)]
    pub opportunity_size_multiplier: Option<f64>,
    #[serde(default)]
    pub strong_trend_multiplier: Option<f64>,
    // Profit guarantee ve fee ayarları
    #[serde(default)]
    pub min_profit_usd: Option<f64>, // Minimum kar hedefi (USD) - default: 0.50
    #[serde(default)]
    pub maker_fee_rate: Option<f64>, // Maker fee oranı (default: 0.0002 = 2 bps)
    #[serde(default)]
    pub taker_fee_rate: Option<f64>, // Taker fee oranı (default: 0.0004 = 4 bps)
    // Not: slippage_bps_reserve artık RiskCfg altında (risk.slippage_bps_reserve)
    
    // TP ve time-box ayarları
    #[serde(default)]
    pub tp_timebox_secs: Option<u64>, // Time-box süresi (saniye) - default: 20
    #[serde(default)]
    pub position_close_cooldown_ms: Option<u64>, // Pozisyon kapatma cooldown (ms) - default: 500
    
    // Long/Short seçimi ayarları
    #[serde(default)]
    pub direction_cooldown_secs: Option<u64>, // Yön değişikliği cooldown (saniye) - default: 60
    #[serde(default)]
    pub direction_signal_strength_threshold: Option<f64>, // Sinyal gücü farkı eşiği - default: 0.2
    #[serde(default)]
    pub direction_min_signal_strength: Option<f64>, // Minimum sinyal gücü (emir yerleştirme için) - default: 0.3
    #[serde(default)]
    pub orderbook_imbalance_long_threshold: Option<f64>, // Long için imbalance threshold (bid_vol/ask_vol) - default: 1.2
    #[serde(default)]
    pub orderbook_imbalance_short_threshold: Option<f64>, // Short için imbalance threshold (bid_vol/ask_vol) - default: 0.83
}

#[derive(Debug, Deserialize)]
pub struct ExecCfg {
    pub tif: String,
    pub venue: String,
    #[serde(default = "default_cancel_interval")]
    pub cancel_replace_interval_ms: u64,
    #[serde(default = "default_max_order_age")]
    pub max_order_age_ms: u64,
    #[serde(default)]
    pub default_leverage: Option<u32>, // Default leverage (futures için) - her sembol için set edilir
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
    #[allow(dead_code)]
    pub rate_limiter_safety_factor: f64,
    #[serde(default = "default_rate_limiter_min_interval_ms")]
    #[allow(dead_code)]
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
    #[serde(default = "default_min_quote_life_ms")]
    pub min_quote_life_ms: u64,
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
    #[serde(default = "default_opportunity_mode_position_multiplier")]
    pub opportunity_mode_position_multiplier: f64,
    #[serde(default = "default_opportunity_mode_leverage_reduction")]
    pub opportunity_mode_leverage_reduction: f64,
    #[serde(default = "default_opportunity_mode_soft_limit_ratio")]
    pub opportunity_mode_soft_limit_ratio: f64, // Soft limit: Yeni emirleri durdur (örn: 0.8 = %80)
    #[serde(default = "default_opportunity_mode_medium_limit_ratio")]
    pub opportunity_mode_medium_limit_ratio: f64, // Medium limit: Mevcut emirleri azalt (örn: 0.9 = %90)
    #[serde(default = "default_opportunity_mode_hard_limit_ratio")]
    pub opportunity_mode_hard_limit_ratio: f64, // Hard limit: Force-close (örn: 1.0 = %100)
    #[serde(default = "default_min_risk_reward_ratio")]
    pub min_risk_reward_ratio: f64,
    #[serde(default = "default_take_profit_position_size_threshold")]
    pub take_profit_position_size_threshold: f64,
    #[serde(default = "default_initial_fill_rate")]
    pub initial_fill_rate: f64,
    #[serde(default = "default_no_fill_threshold_sec")]
    pub no_fill_threshold_sec: f64,
    #[serde(default = "default_fill_rate_decrease_on_no_fill")]
    pub fill_rate_decrease_on_no_fill: f64,
    #[serde(default = "default_min_fill_rate")]
    pub min_fill_rate: f64,
    #[serde(default = "default_spread_widen_factor")]
    pub spread_widen_factor: f64,
    #[serde(default = "default_min_tick_interval_ms")]
    pub min_tick_interval_ms: u64,
    #[serde(default = "default_symbol_discovery_retry_interval_sec")]
    pub symbol_discovery_retry_interval_sec: u64,
    #[serde(default = "default_progress_log_first_n_symbols")]
    pub progress_log_first_n_symbols: usize,
    #[serde(default = "default_progress_log_interval")]
    pub progress_log_interval: usize,
    #[serde(default = "default_debug_symbol_count")]
    pub debug_symbol_count: usize,
    #[serde(default = "default_max_stale_orders_threshold")]
    pub max_stale_orders_threshold: usize,
    #[serde(default = "default_fill_rate_partial_fill_factor")]
    pub fill_rate_partial_fill_factor: f64,
    #[serde(default = "default_fill_rate_partial_fill_bonus")]
    pub fill_rate_partial_fill_bonus: f64,
    #[serde(default = "default_fill_rate_reconnect_factor")]
    pub fill_rate_reconnect_factor: f64,
    #[serde(default = "default_fill_rate_reconnect_bonus")]
    pub fill_rate_reconnect_bonus: f64,
}

#[derive(Debug, Deserialize)]
pub struct BinanceCfg {
    pub api_key: String,
    pub secret_key: String,
    #[serde(default = "default_recv_window")]
    pub recv_window_ms: u64,
    pub futures_base: String,
    #[serde(default = "default_hedge_mode")]
    pub hedge_mode: bool, // Hedge mode (dual-side position) açık mı? Default: false (one-way mode)
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
    #[serde(default = "default_allow_usdt_quote")]
    pub allow_usdt_quote: bool,
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
    #[serde(default = "default_flash_crash_recovery_window_ms")]
    pub flash_crash_recovery_window_ms: u64,
    #[serde(default = "default_flash_crash_recovery_min_points")]
    pub flash_crash_recovery_min_points: usize,
    #[serde(default = "default_flash_crash_recovery_min_ratio")]
    pub flash_crash_recovery_min_ratio: f64,
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
    #[serde(default = "default_confidence_min_threshold")]
    pub confidence_min_threshold: f64,
    #[serde(default = "default_default_confidence")]
    pub default_confidence: f64,
    #[serde(default = "default_min_confidence_value")]
    pub min_confidence_value: f64,
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
fn default_slippage_bps_reserve() -> f64 { 2.0 } // Default: 2 bps slippage reserve
fn default_use_isolated_margin() -> bool { true } // Default: isolated margin kullan
fn default_max_open_chunks_per_symbol_per_side() -> usize { 5 } // Default: sembol başına yönde max 5 chunk
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
fn default_order_price_change_threshold() -> f64 { 0.001 }
fn default_min_quote_life_ms() -> u64 { 1500 }
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
fn default_max_position_size_buffer() -> f64 { 10.0 }
fn default_opportunity_mode_position_multiplier() -> f64 { 2.0 }
fn default_opportunity_mode_leverage_reduction() -> f64 { 0.5 }
fn default_opportunity_mode_soft_limit_ratio() -> f64 { 0.95 } // %95'te yeni emirleri durdur
fn default_opportunity_mode_medium_limit_ratio() -> f64 { 1.0 } // %100'de mevcut emirleri azalt (hard limit)
fn default_opportunity_mode_hard_limit_ratio() -> f64 { 1.0 } // %100'de force-close
fn default_min_risk_reward_ratio() -> f64 { 2.0 }
fn default_take_profit_position_size_threshold() -> f64 { 100.0 }

// TP ve time-box default'ları
fn default_tp_timebox_secs() -> u64 { 30 }
fn default_position_close_cooldown_ms() -> u64 { 500 }

// Long/Short seçimi default'ları
fn default_direction_cooldown_secs() -> u64 { 60 }
fn default_direction_signal_strength_threshold() -> f64 { 0.2 }
fn default_direction_min_signal_strength() -> f64 { 0.3 }
fn default_orderbook_imbalance_long_threshold() -> f64 { 1.2 }
fn default_orderbook_imbalance_short_threshold() -> f64 { 0.83 }
fn default_initial_fill_rate() -> f64 { 0.5 }
fn default_no_fill_threshold_sec() -> f64 { 5.0 }
fn default_fill_rate_decrease_on_no_fill() -> f64 { 0.90 }
fn default_min_fill_rate() -> f64 { 0.1 }
fn default_spread_widen_factor() -> f64 { 0.001 }
fn default_recv_window() -> u64 { 5_000 }
fn default_hedge_mode() -> bool { false } // Default: one-way mode (hedge mode kapalı)
fn default_quote_asset() -> String { "USDC".to_string() }
fn default_allow_usdt_quote() -> bool { true }
fn default_min_quote_balance_usd() -> f64 { 1.0 }
fn default_manipulation_volume_ratio_threshold() -> f64 { 5.0 }
fn default_manipulation_time_threshold_ms() -> u64 { 2000 }
fn default_manipulation_price_history_min_len() -> usize { 3 }
fn default_manipulation_price_history_max_len() -> usize { 200 }
fn default_flash_crash_recovery_window_ms() -> u64 { 30000 }
fn default_flash_crash_recovery_min_points() -> usize { 10 }
fn default_flash_crash_recovery_min_ratio() -> f64 { 0.3 }
fn default_confidence_price_drop_max() -> f64 { 500.0 }
fn default_confidence_volume_ratio_min() -> f64 { 5.0 }
fn default_confidence_volume_ratio_max() -> f64 { 10.0 }
fn default_confidence_spread_min() -> f64 { 50.0 }
fn default_confidence_spread_max() -> f64 { 150.0 }
fn default_confidence_bonus_multiplier() -> f64 { 0.3 }
fn default_confidence_max_multiplier() -> f64 { 1.5 }
fn default_confidence_min_threshold() -> f64 { 0.70 } // 0.75 → 0.70: False positive azalt, gerçek fırsatları kaçırma
fn default_default_confidence() -> f64 { 0.7 }
fn default_min_confidence_value() -> f64 { 0.5 }
fn default_min_tick_interval_ms() -> u64 { 100 }
fn default_symbol_discovery_retry_interval_sec() -> u64 { 30 }
fn default_progress_log_first_n_symbols() -> usize { 10 }
fn default_progress_log_interval() -> usize { 50 }
fn default_debug_symbol_count() -> usize { 5 }
fn default_max_stale_orders_threshold() -> usize { 10 }
fn default_fill_rate_partial_fill_factor() -> f64 { 0.98 }
fn default_fill_rate_partial_fill_bonus() -> f64 { 0.02 }
fn default_fill_rate_reconnect_factor() -> f64 { 0.95 }
fn default_fill_rate_reconnect_bonus() -> f64 { 0.05 }
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
    
    // Quote asset validasyonu: Sadece USDC veya USDT kabul edilir
    let quote_upper = cfg.quote_asset.to_uppercase();
    if quote_upper != "USDC" && quote_upper != "USDT" {
        return Err(anyhow!("quote_asset must be either 'USDC' or 'USDT', got '{}'. Only USDC and USDT are supported.", cfg.quote_asset));
    }
    
    // API key validasyonu
    if cfg.binance.api_key.trim().is_empty() {
        return Err(anyhow!("binance.api_key is required but is empty. Please set your API key in config.yaml"));
    }
    if cfg.binance.secret_key.trim().is_empty() {
        return Err(anyhow!("binance.secret_key is required but is empty. Please set your secret key in config.yaml"));
    }
    
    // API key format kontrolü (Binance API key'leri genellikle 64 karakter)
    if cfg.binance.api_key.len() < 20 {
        return Err(anyhow!("binance.api_key appears to be invalid (too short). Binance API keys are typically 64 characters long"));
    }
    if cfg.binance.secret_key.len() < 20 {
        return Err(anyhow!("binance.secret_key appears to be invalid (too short). Binance secret keys are typically 64 characters long"));
    }
    
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;

    fn create_test_config() -> AppCfg {
        AppCfg {
            symbol: None,
            symbols: vec![],
            auto_discover_quote: true,
            quote_asset: "USDC".to_string(),
            allow_usdt_quote: true,
            mode: "futures".to_string(),
            metrics_port: Some(9000),
            max_usd_per_order: 100.0,
            min_usd_per_order: Some(20.0),
            min_quote_balance_usd: 1.0,
            leverage: Some(3),
            price_tick: 0.001,
            qty_step: 0.001,
            binance: BinanceCfg {
                futures_base: "https://fapi.binance.com".to_string(),
                api_key: "6WkI51B0dbz9gbtRm5PphsWYFmr3oaoagJECl0UiGX13ySnLxXlhuEoR5brk74ZU".to_string(),
                secret_key: "8N3PxGEp1CTrirTbmkM42mHaNTo5jbyamUFgatfeueHHkH5RBsDgGLCh3b6Ki3nw".to_string(),
                recv_window_ms: 5000,
            },
            risk: RiskCfg {
                inv_cap: "0.50".to_string(),
                min_liq_gap_bps: 300.0,
                dd_limit_bps: 2000,
                max_leverage: 20,
                slippage_bps_reserve: 2.0,
            },
            strategy: StratCfg {
                r#type: "dyn_mm".to_string(),
                a: 100.0,
                b: 35.0,
                base_size: "20.0".to_string(),
                inv_cap: None,
                min_spread_bps: Some(30.0),
                max_spread_bps: Some(200.0),
                spread_arbitrage_min_bps: Some(5.0),
                spread_arbitrage_max_bps: Some(100.0),
                strong_trend_bps: Some(100.0),
                momentum_strong_bps: Some(50.0),
                trend_bias_multiplier: Some(1.0),
                adverse_selection_threshold_on: Some(0.6),
                adverse_selection_threshold_off: Some(0.4),
                opportunity_threshold_on: Some(0.5),
                opportunity_threshold_off: Some(0.2),
                price_jump_threshold_bps: Some(500.0),
                fake_breakout_threshold_bps: Some(100.0),
                liquidity_drop_threshold: Some(0.5),
                inventory_threshold_ratio: Some(0.10),
                volatility_coefficient: Some(0.5),
                ofi_coefficient: Some(0.5),
                min_liquidity_required: Some(0.001),
                min_24h_volume_usd: Some(0.0),
                min_book_depth_usd: Some(0.0),
                opportunity_size_multiplier: Some(1.05),
                strong_trend_multiplier: Some(1.0),
            },
            exec: ExecCfg {
                tif: "post_only".to_string(),
                venue: "binance".to_string(),
                cancel_replace_interval_ms: 1500,
                max_order_age_ms: 10000,
            },
            websocket: WebsocketCfg {
                enabled: true,
                reconnect_delay_ms: 5000,
                ping_interval_ms: 30000,
            },
            internal: InternalCfg::default(),
            strategy_internal: StrategyInternalCfg::default(),
        }
    }

    #[test]
    fn test_validate_config_valid() {
        let cfg = create_test_config();
        assert!(validate_config(&cfg).is_ok());
    }

    #[test]
    fn test_validate_config_invalid_price_tick() {
        let mut cfg = create_test_config();
        cfg.price_tick = 0.0;
        assert!(validate_config(&cfg).is_err());
        
        cfg.price_tick = -1.0;
        assert!(validate_config(&cfg).is_err());
    }

    #[test]
    fn test_validate_config_invalid_qty_step() {
        let mut cfg = create_test_config();
        cfg.qty_step = 0.0;
        assert!(validate_config(&cfg).is_err());
        
        cfg.qty_step = -1.0;
        assert!(validate_config(&cfg).is_err());
    }

    #[test]
    fn test_validate_config_invalid_max_usd_per_order() {
        let mut cfg = create_test_config();
        cfg.max_usd_per_order = 0.0;
        assert!(validate_config(&cfg).is_err());
        
        cfg.max_usd_per_order = -1.0;
        assert!(validate_config(&cfg).is_err());
    }

    #[test]
    fn test_validate_config_invalid_quote_asset() {
        let mut cfg = create_test_config();
        cfg.quote_asset = "BUSD".to_string();
        assert!(validate_config(&cfg).is_err());
        
        cfg.quote_asset = "BTC".to_string();
        assert!(validate_config(&cfg).is_err());
        
        cfg.quote_asset = "".to_string();
        assert!(validate_config(&cfg).is_err());
        
        cfg.quote_asset = "EUR".to_string();
        assert!(validate_config(&cfg).is_err());
    }

    #[test]
    fn test_validate_config_valid_quote_asset_usdc() {
        let mut cfg = create_test_config();
        cfg.quote_asset = "USDC".to_string();
        assert!(validate_config(&cfg).is_ok());
        
        cfg.quote_asset = "usdc".to_string();
        assert!(validate_config(&cfg).is_ok());
        
        cfg.quote_asset = "UsDc".to_string();
        assert!(validate_config(&cfg).is_ok());
    }

    #[test]
    fn test_validate_config_valid_quote_asset_usdt() {
        let mut cfg = create_test_config();
        cfg.quote_asset = "USDT".to_string();
        assert!(validate_config(&cfg).is_ok());
        
        cfg.quote_asset = "usdt".to_string();
        assert!(validate_config(&cfg).is_ok());
        
        cfg.quote_asset = "UsDt".to_string();
        assert!(validate_config(&cfg).is_ok());
    }

    #[test]
    fn test_validate_config_empty_api_key() {
        let mut cfg = create_test_config();
        cfg.binance.api_key = "".to_string();
        assert!(validate_config(&cfg).is_err());
        
        cfg.binance.api_key = "   ".to_string();
        assert!(validate_config(&cfg).is_err());
    }

    #[test]
    fn test_validate_config_empty_secret_key() {
        let mut cfg = create_test_config();
        cfg.binance.secret_key = "".to_string();
        assert!(validate_config(&cfg).is_err());
        
        cfg.binance.secret_key = "   ".to_string();
        assert!(validate_config(&cfg).is_err());
    }

    #[test]
    fn test_validate_config_short_api_key() {
        let mut cfg = create_test_config();
        cfg.binance.api_key = "short".to_string();
        assert!(validate_config(&cfg).is_err());
        
        cfg.binance.api_key = "1234567890123456789".to_string(); // 19 chars
        assert!(validate_config(&cfg).is_err());
    }

    #[test]
    fn test_validate_config_short_secret_key() {
        let mut cfg = create_test_config();
        cfg.binance.secret_key = "short".to_string();
        assert!(validate_config(&cfg).is_err());
        
        cfg.binance.secret_key = "1234567890123456789".to_string(); // 19 chars
        assert!(validate_config(&cfg).is_err());
    }

    #[test]
    fn test_validate_config_valid_api_keys() {
        let mut cfg = create_test_config();
        // 20+ character keys should pass
        cfg.binance.api_key = "12345678901234567890".to_string(); // 20 chars
        cfg.binance.secret_key = "12345678901234567890".to_string(); // 20 chars
        assert!(validate_config(&cfg).is_ok());
    }
}

