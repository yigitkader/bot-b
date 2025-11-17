// Configuration structures and loading logic
// Clean architecture - minimal config for event-driven modules

use anyhow::{anyhow, Result};
use serde::Deserialize;

// ============================================================================
// Configuration Structures
// ============================================================================

#[derive(Debug, Deserialize, Clone, Default)]
pub struct RiskCfg {
    /// Maximum allowed leverage (validation only, not used as default)
    /// Used to validate that leverage and exec.default_leverage don't exceed this limit
    #[serde(default = "default_max_leverage")]
    pub max_leverage: u32,
    #[serde(default = "default_use_isolated_margin")]
    pub use_isolated_margin: bool,
    #[serde(default = "default_max_position_notional_usd")]
    pub max_position_notional_usd: f64,
    /// Maker commission rate (percentage, e.g., 0.02 for 0.02%)
    /// Maker orders add liquidity to the order book (post-only orders)
    #[serde(default = "default_maker_commission_pct")]
    pub maker_commission_pct: f64,
    /// Taker commission rate (percentage, e.g., 0.04 for 0.04%)
    /// Taker orders remove liquidity from the order book (market orders, IOC orders)
    /// Default: 0.04% (Binance futures standard taker fee)
    #[serde(default = "default_taker_commission_pct")]
    pub taker_commission_pct: f64,
    /// Minimum commission buffer in USD (safety margin for small positions)
    #[serde(default = "default_min_commission_buffer_usd")]
    pub min_commission_buffer_usd: f64,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct TrendingCfg {
    #[serde(default = "default_min_spread_bps")]
    pub min_spread_bps: f64,
    #[serde(default = "default_max_spread_bps")]
    pub max_spread_bps: f64,
    #[serde(default = "default_signal_cooldown_seconds")]
    pub signal_cooldown_seconds: u64,
    /// High-frequency trading mode: allows signals without volume confirmation
    /// When true, trend signals are generated even if volume doesn't confirm (more aggressive)
    #[serde(default = "default_hft_mode")]
    pub hft_mode: bool,
    /// Require volume confirmation for trend signals
    /// When true, signals are blocked if volume doesn't confirm the trend
    /// When false (and hft_mode=true), volume confirmation is optional
    #[serde(default = "default_require_volume_confirmation")]
    pub require_volume_confirmation: bool,
    /// Enable trailing stop loss (places trailing stop when TP threshold is reached)
    /// When true, trailing stop order is placed when take profit threshold is reached
    /// Trailing stop follows price movement to lock in profits
    #[serde(default = "default_use_trailing_stop")]
    pub use_trailing_stop: bool,
    /// Trailing stop callback rate (0.001 = 0.1%)
    /// Binance expects percentage (0.1 not 0.001)
    /// Example: 0.001 = 0.1% callback rate
    #[serde(default = "default_trailing_stop_callback_rate")]
    pub trailing_stop_callback_rate: f64,
    /// RSI minimum average loss threshold (prevents division by very small numbers)
    #[serde(default = "default_rsi_min_avg_loss")]
    pub rsi_min_avg_loss: f64,
    /// ATR period for volatility calculation
    #[serde(default = "default_atr_period")]
    pub atr_period: usize,
    /// Low volatility threshold (ATR < this = ranging market)
    #[serde(default = "default_low_volatility_threshold")]
    pub low_volatility_threshold: f64,
    /// High volatility threshold (ATR > this = volatile market)
    #[serde(default = "default_high_volatility_threshold")]
    pub high_volatility_threshold: f64,
    /// Base volatility percentage for normalization (2% = 0.02)
    #[serde(default = "default_base_volatility")]
    pub base_volatility: f64,
    /// Base minimum score threshold for signal generation
    #[serde(default = "default_base_min_score")]
    pub base_min_score: f64,
    /// Regime multiplier for trending markets
    #[serde(default = "default_regime_multiplier_trending")]
    pub regime_multiplier_trending: f64,
    /// Regime multiplier for ranging markets
    #[serde(default = "default_regime_multiplier_ranging")]
    pub regime_multiplier_ranging: f64,
    /// Regime multiplier for volatile markets
    #[serde(default = "default_regime_multiplier_volatile")]
    pub regime_multiplier_volatile: f64,
    /// Regime multiplier for unknown markets
    #[serde(default = "default_regime_multiplier_unknown")]
    pub regime_multiplier_unknown: f64,
    /// Volume multiplier for HFT mode (1.1 = 1.1x average volume)
    #[serde(default = "default_volume_multiplier_hft")]
    pub volume_multiplier_hft: f64,
    /// Volume multiplier for normal mode (1.3 = 1.3x average volume)
    #[serde(default = "default_volume_multiplier_normal")]
    pub volume_multiplier_normal: f64,
    /// Trend strength threshold for HFT mode (0.5 = 50% alignment required)
    #[serde(default = "default_trend_threshold_hft")]
    pub trend_threshold_hft: f64,
    /// Trend strength threshold for normal mode (0.7 = 70% alignment required)
    #[serde(default = "default_trend_threshold_normal")]
    pub trend_threshold_normal: f64,
    /// Score multiplier for weak trends with volume (1.1 = 10% higher threshold)
    #[serde(default = "default_weak_trend_score_multiplier")]
    pub weak_trend_score_multiplier: f64,
    /// RSI lower bound for long signals
    #[serde(default = "default_rsi_lower_long")]
    pub rsi_lower_long: f64,
    /// RSI upper bound for long signals
    #[serde(default = "default_rsi_upper_long")]
    pub rsi_upper_long: f64,
    /// RSI lower bound for short signals
    #[serde(default = "default_rsi_lower_short")]
    pub rsi_lower_short: f64,
    /// RSI upper bound for short signals
    #[serde(default = "default_rsi_upper_short")]
    pub rsi_upper_short: f64,
    /// Score weight for short-term EMA alignment
    #[serde(default = "default_ema_short_score")]
    pub ema_short_score: f64,
    /// Score weight for mid-term EMA alignment
    #[serde(default = "default_ema_mid_score")]
    pub ema_mid_score: f64,
    /// Score weight for slope strength
    #[serde(default = "default_slope_score")]
    pub slope_score: f64,
    /// Score weight for RSI confirmation
    #[serde(default = "default_rsi_score")]
    pub rsi_score: f64,
    /// Minimum analysis interval in milliseconds (throttling)
    #[serde(default = "default_min_analysis_interval_ms")]
    pub min_analysis_interval_ms: u64,
    /// Default spread quality when spread range is zero
    #[serde(default = "default_default_spread_quality")]
    pub default_spread_quality: f64,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct ExecCfg {
    #[serde(default = "default_tif")]
    pub tif: String,
    /// Default leverage to use when leverage (top-level) is not set
    /// Used as fallback: cfg.leverage.unwrap_or(cfg.exec.default_leverage)
    #[serde(default = "default_default_leverage")]
    pub default_leverage: u32,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct WebsocketCfg {
    #[serde(default = "default_ws_reconnect_delay")]
    pub reconnect_delay_ms: u64,
    #[serde(default = "default_ws_ping_interval")]
    pub ping_interval_ms: u64,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct InternalCfg {
    /// Maximum position size buffer multiplier (default: 1.5)
    #[serde(default = "default_max_position_size_buffer")]
    pub max_position_size_buffer: f64,
    /// Opportunity mode position multiplier (default: 1.2)
    #[serde(default = "default_opportunity_mode_position_multiplier")]
    pub opportunity_mode_position_multiplier: f64,
    /// Opportunity mode soft limit ratio (default: 0.8 = 80%)
    #[serde(default = "default_opportunity_mode_soft_limit_ratio")]
    pub opportunity_mode_soft_limit_ratio: f64,
    /// Opportunity mode medium limit ratio (default: 0.9 = 90%)
    #[serde(default = "default_opportunity_mode_medium_limit_ratio")]
    pub opportunity_mode_medium_limit_ratio: f64,
    /// Opportunity mode hard limit ratio (default: 1.0 = 100%)
    #[serde(default = "default_opportunity_mode_hard_limit_ratio")]
    pub opportunity_mode_hard_limit_ratio: f64,
    /// PnL alert interval in seconds (default: 60)
    #[serde(default = "default_pnl_alert_interval_sec")]
    pub pnl_alert_interval_sec: u64,
    /// PnL alert threshold for positive PnL (default: 0.1 = 10%)
    #[serde(default = "default_pnl_alert_threshold_positive")]
    pub pnl_alert_threshold_positive: f64,
    /// PnL alert threshold for negative PnL (default: -0.05 = -5%)
    #[serde(default = "default_pnl_alert_threshold_negative")]
    pub pnl_alert_threshold_negative: f64,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct EventBusCfg {
    /// Buffer size for MarketTick events (high frequency, needs larger buffer)
    /// 
    /// **CRITICAL**: MarketTick events are high-frequency and can cause buffer overflow if subscribers are slow.
    /// 
    /// Buffer size calculation:
    /// - 100 symbols × 1 tick/sec = 100 events/sec
    /// - 1000 buffer = ~10 seconds (may be insufficient if subscribers lag)
    /// - 10000 buffer = ~100 seconds (recommended for high-frequency trading)
    /// 
    /// **Warning**: When buffer is full, new events are dropped (RecvError::Lagged).
    /// Subscribers should handle lag gracefully and process events quickly.
    /// 
    /// **Recommendation**: 
    /// - Normal operation: 5000-10000
    /// - High-frequency trading: 10000-20000
    /// - Very slow subscribers: Consider increasing further or optimizing subscriber performance
    #[serde(default = "default_market_tick_buffer")]
    pub market_tick_buffer: usize,
    /// Buffer size for TradeSignal events
    #[serde(default = "default_trade_signal_buffer")]
    pub trade_signal_buffer: usize,
    /// Buffer size for CloseRequest events
    #[serde(default = "default_default_event_buffer")]
    pub close_request_buffer: usize,
    /// Buffer size for OrderUpdate events
    #[serde(default = "default_default_event_buffer")]
    pub order_update_buffer: usize,
    /// Buffer size for PositionUpdate events
    #[serde(default = "default_default_event_buffer")]
    pub position_update_buffer: usize,
    /// Buffer size for BalanceUpdate events
    #[serde(default = "default_default_event_buffer")]
    pub balance_update_buffer: usize,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct DynamicSymbolSelection {
    /// Enable dynamic symbol selection (automatic ranking and rotation)
    #[serde(default = "default_dynamic_symbol_selection_enabled")]
    pub enabled: bool,
    /// Maximum number of symbols to track simultaneously (CPU limit)
    #[serde(default = "default_max_symbols")]
    pub max_symbols: usize,
    /// Rotation interval in minutes (how often to re-rank and update symbols)
    #[serde(default = "default_rotation_interval_minutes")]
    pub rotation_interval_minutes: u64,
    /// Minimum volatility percentage (24h price change) to consider a symbol
    #[serde(default = "default_min_volatility_pct")]
    pub min_volatility_pct: f64,
    /// Minimum quote volume (USD) to consider a symbol (likidity filter)
    #[serde(default = "default_min_quote_volume")]
    pub min_quote_volume: f64,
    /// Minimum number of trades in 24h to consider a symbol
    #[serde(default = "default_min_trades_24h")]
    pub min_trades_24h: u64,
    /// Weight for volatility in opportunity score calculation
    #[serde(default = "default_volatility_weight")]
    pub volatility_weight: f64,
    /// Weight for volume in opportunity score calculation
    #[serde(default = "default_volume_weight")]
    pub volume_weight: f64,
    /// Weight for trades count in opportunity score calculation
    #[serde(default = "default_trades_weight")]
    pub trades_weight: f64,
    /// Weight for spread in opportunity score calculation
    #[serde(default = "default_spread_weight")]
    pub spread_weight: f64,
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

impl Default for BinanceCfg {
    fn default() -> Self {
        Self {
            api_key: "test_api_key".to_string(),
            secret_key: "test_secret_key".to_string(),
            recv_window_ms: default_recv_window(),
            futures_base: "https://fapi.binance.com".to_string(),
            hedge_mode: default_hedge_mode(),
        }
    }
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
    /// Maximum margin per trade (USD) - legacy, use min_margin_usd/max_margin_usd instead
    /// Kept for backward compatibility, but will be clamped to [min_margin_usd, max_margin_usd]
    #[serde(default = "default_max_usd_per_order")]
    pub max_usd_per_order: f64,
    /// Minimum margin per trade (USD) - legacy, use min_margin_usd instead
    #[serde(default = "default_min_usd_per_order")]
    pub min_usd_per_order: f64,
    /// Minimum margin per trade (USD) - dynamic margin range lower bound
    /// Used for dynamic margin calculation: margin will be clamped to [min_margin_usd, max_margin_usd]
    #[serde(default = "default_min_margin_usd")]
    pub min_margin_usd: f64,
    /// Maximum margin per trade (USD) - dynamic margin range upper bound
    /// Used for dynamic margin calculation: margin will be clamped to [min_margin_usd, max_margin_usd]
    #[serde(default = "default_max_margin_usd")]
    pub max_margin_usd: f64,
    /// Margin calculation strategy: "fixed" | "dynamic" | "trend_based" | "balance_based" | "max_balance"
    /// - "fixed": Always use max_usd_per_order (or clamped to [min_margin_usd, max_margin_usd])
    /// - "dynamic" | "trend_based": Scale margin based on trend strength, using available balance as base
    /// - "balance_based" | "max_balance": Use all available balance (up to max_margin_usd)
    ///   Since we only have one position at a time, we can use all available funds (up to max_margin_usd limit)
    #[serde(default = "default_margin_strategy")]
    pub margin_strategy: String,
    #[serde(default = "default_min_quote_balance_usd")]
    pub min_quote_balance_usd: f64,
    /// Explicit leverage setting (optional)
    /// If set, this value is used. Otherwise, exec.default_leverage is used.
    /// Must not exceed risk.max_leverage (validated in validate_config)
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
    #[serde(default)]
    pub event_bus: EventBusCfg,
    #[serde(default)]
    pub dynamic_symbol_selection: DynamicSymbolSelection,
    #[serde(default)]
    pub internal: InternalCfg,
}

impl Default for AppCfg {
    fn default() -> Self {
        Self {
            symbol: None,
            symbols: Vec::new(),
            auto_discover_quote: default_auto_discover_quote(),
            quote_asset: default_quote_asset(),
            allow_usdt_quote: default_allow_usdt_quote(),
            max_usd_per_order: default_max_usd_per_order(),
            min_usd_per_order: default_min_usd_per_order(),
            min_margin_usd: default_min_margin_usd(),
            max_margin_usd: default_max_margin_usd(),
            margin_strategy: default_margin_strategy(),
            min_quote_balance_usd: default_min_quote_balance_usd(),
            leverage: None,
            price_tick: default_price_tick(),
            qty_step: default_qty_step(),
            take_profit_pct: default_take_profit_pct(),
            stop_loss_pct: default_stop_loss_pct(),
            binance: BinanceCfg::default(),
            risk: RiskCfg::default(),
            trending: TrendingCfg::default(),
            exec: ExecCfg::default(),
            websocket: WebsocketCfg::default(),
            event_bus: EventBusCfg::default(),
            dynamic_symbol_selection: DynamicSymbolSelection::default(),
            internal: InternalCfg::default(),
        }
    }
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

fn default_maker_commission_pct() -> f64 {
    0.02 // 0.02% (Binance futures standard maker fee)
}

fn default_taker_commission_pct() -> f64 {
    0.04 // 0.04% (Binance futures standard taker fee)
}

fn default_min_spread_bps() -> f64 {
    5.0
}

fn default_max_spread_bps() -> f64 {
    200.0
}

fn default_signal_cooldown_seconds() -> u64 {
    5 // Default: 5 seconds cooldown between signals for same symbol (optimized for high-frequency trading)
}

fn default_hft_mode() -> bool {
    true // Default: HFT mode enabled for high-frequency trading
}

fn default_require_volume_confirmation() -> bool {
    false // Default: Volume confirmation not required in HFT mode
}

fn default_use_trailing_stop() -> bool {
    false // Default: Trailing stop disabled (can be enabled in config)
}

fn default_trailing_stop_callback_rate() -> f64 {
    0.001 // Default: 0.1% callback rate (0.001 = 0.1%)
}

fn default_rsi_min_avg_loss() -> f64 {
    0.0001 // Default: 0.0001 (prevents division by very small numbers)
}

fn default_atr_period() -> usize {
    14 // Default: 14 periods for ATR calculation
}

fn default_low_volatility_threshold() -> f64 {
    0.5 // Default: 0.5% ATR = ranging market
}

fn default_high_volatility_threshold() -> f64 {
    2.0 // Default: 2.0% ATR = volatile market
}

fn default_base_volatility() -> f64 {
    0.02 // Default: 2% base volatility for normalization
}

fn default_base_min_score() -> f64 {
    3.5 // Default: Base minimum score threshold
}

fn default_regime_multiplier_trending() -> f64 {
    0.95 // Default: Trending markets use 95% of base score
}

fn default_regime_multiplier_ranging() -> f64 {
    1.1 // Default: Ranging markets use 110% of base score
}

fn default_regime_multiplier_volatile() -> f64 {
    1.15 // Default: Volatile markets use 115% of base score
}

fn default_regime_multiplier_unknown() -> f64 {
    1.0 // Default: Unknown markets use 100% of base score
}

fn default_volume_multiplier_hft() -> f64 {
    1.1 // Default: HFT mode requires 1.1x average volume
}

fn default_volume_multiplier_normal() -> f64 {
    1.3 // Default: Normal mode requires 1.3x average volume
}

fn default_trend_threshold_hft() -> f64 {
    0.5 // Default: HFT mode requires 50% trend alignment
}

fn default_trend_threshold_normal() -> f64 {
    0.7 // Default: Normal mode requires 70% trend alignment
}

fn default_weak_trend_score_multiplier() -> f64 {
    1.1 // Default: Weak trends with volume require 10% higher score
}

fn default_rsi_lower_long() -> f64 {
    55.0 // Default: RSI lower bound for long signals
}

fn default_rsi_upper_long() -> f64 {
    70.0 // Default: RSI upper bound for long signals
}

fn default_rsi_lower_short() -> f64 {
    25.0 // Default: RSI lower bound for short signals
}

fn default_rsi_upper_short() -> f64 {
    50.0 // Default: RSI upper bound for short signals
}

fn default_ema_short_score() -> f64 {
    2.5 // Default: Score weight for short-term EMA alignment
}

fn default_ema_mid_score() -> f64 {
    2.0 // Default: Score weight for mid-term EMA alignment
}

fn default_slope_score() -> f64 {
    1.5 // Default: Score weight for slope strength
}

fn default_rsi_score() -> f64 {
    1.5 // Default: Score weight for RSI confirmation
}

fn default_min_analysis_interval_ms() -> u64 {
    100 // Default: 100ms minimum interval (10 analyses per second max)
}

fn default_default_spread_quality() -> f64 {
    0.5 // Default: 0.5 = medium spread quality when range is zero
}

fn default_min_commission_buffer_usd() -> f64 {
    0.5 // Default: 0.5 USD minimum commission buffer
}

fn default_min_margin_usd() -> f64 {
    10.0 // Minimum margin: 10 USD
}

fn default_max_margin_usd() -> f64 {
    100.0 // Maximum margin: 100 USD
}

fn default_margin_strategy() -> String {
    "fixed".to_string() // Default: Fixed margin (use max_usd_per_order)
}

fn default_tif() -> String {
    "post_only".to_string()
}

fn default_default_leverage() -> u32 {
    20
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

fn default_market_tick_buffer() -> usize {
    // ✅ CRITICAL: MarketTick events are high-frequency (100 symbols × 1 tick/sec = 100 events/sec)
    // Buffer size calculation:
    // - 100 events/sec × 10 seconds = 1000 events (minimum for normal operation)
    // - 100 events/sec × 100 seconds = 10000 events (recommended for high-frequency trading)
    // - With slow subscribers or network delays, buffer can fill up quickly
    // - When buffer is full, new events are dropped (RecvError::Lagged)
    // - Larger buffer prevents event loss during subscriber lag spikes
    //
    // Recommendation: Use 10000 for high-frequency trading, 5000 for normal operation
    // This provides ~100 seconds of buffer at 100 events/sec, or ~50 seconds at 200 events/sec
    10000 // Increased from 1000 to handle high-frequency events and subscriber lag
}

fn default_trade_signal_buffer() -> usize {
    1000
}

fn default_default_event_buffer() -> usize {
    1000
}

fn default_dynamic_symbol_selection_enabled() -> bool {
    false // Default: disabled (use manual symbols or auto_discover_quote)
}

fn default_max_symbols() -> usize {
    30 // Default: 30 symbols max (CPU limit)
}

fn default_rotation_interval_minutes() -> u64 {
    15 // Default: rotate every 15 minutes
}

fn default_min_volatility_pct() -> f64 {
    1.5 // Default: minimum 1.5% daily volatility
}

fn default_min_quote_volume() -> f64 {
    1_000_000.0 // Default: minimum 1M USD volume
}

fn default_min_trades_24h() -> u64 {
    1000 // Default: minimum 1000 trades in 24h
}

fn default_volatility_weight() -> f64 {
    2.0 // Default: volatility is 2x important
}

fn default_volume_weight() -> f64 {
    1.0 // Default: volume weight
}

fn default_trades_weight() -> f64 {
    0.5 // Default: trades weight
}

fn default_spread_weight() -> f64 {
    1.0 // Default: spread weight
}

// ============================================================================
// Configuration Loading
// ============================================================================

/// Load application configuration from file or command line arguments.
///
/// This function loads the configuration from a YAML file. The file path can be specified
/// via command line argument `--config <path>`, or it defaults to `./config.yaml`.
///
/// # Returns
///
/// Returns `Ok(AppCfg)` if the configuration file is found and valid, or `Err` if:
/// - The file cannot be read
/// - The YAML is invalid or cannot be deserialized
/// - Configuration validation fails (see `validate_config`)
///
/// # Configuration File Format
///
/// The configuration file should be in YAML format and include:
/// - `binance`: API keys and exchange settings
/// - `symbols` or `symbol`: Trading symbols
/// - `risk`: Risk management parameters (leverage, margin type)
/// - `trending`: Trend analysis parameters
/// - `exec`: Execution parameters (TIF, leverage)
/// - `event_bus`: Event bus buffer sizes
///
/// # Example
///
/// ```no_run
/// use crate::config::load_config;
///
/// // Load from default path (./config.yaml)
/// let cfg = load_config()?;
///
/// // Or specify path via command line:
/// // cargo run -- --config /path/to/config.yaml
/// ```
///
/// # Errors
///
/// Common errors include:
/// - Missing or invalid API keys
/// - Invalid leverage settings
/// - Missing required fields
/// - File I/O errors
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

    // Leverage validation
    // ✅ FIX: Removed max_leverage limit check
    // User requirement: Each coin can have different max leverage (100x, 125x, etc.)
    // Coin's max leverage comes from exchange (rules.max_leverage) and is always correct
    // Config leverage is only used as fallback when coin doesn't have max leverage info
    // Therefore, we should not limit config leverage - exchange will enforce correct limits
    if let Some(leverage) = cfg.leverage {
        if leverage == 0 {
            return Err(anyhow!("leverage must be greater than 0"));
        }
        // Removed: leverage > cfg.risk.max_leverage check
        // Reason: Coin's max leverage (from exchange) may exceed config max_leverage
        // Exchange will enforce correct leverage limits per symbol
    }
    
    // CRITICAL: Cross margin mode validation
    // TP/SL PnL calculation in follow_orders.rs assumes isolated margin
    // Cross margin uses shared account equity, which requires different PnL calculation
    // Formula for isolated: PnL% = PriceChange% × Leverage
    // Formula for cross: PnL% = (PriceChange% × PositionNotional) / TotalAccountEquity
    // Using isolated formula with cross margin causes incorrect TP/SL triggers:
    // - Premature or delayed TP/SL triggers
    // - Incorrect risk management
    // - Potential financial losses
    if !cfg.risk.use_isolated_margin {
        return Err(anyhow!(
            "CRITICAL: Cross margin mode is NOT supported for TP/SL. \
             PnL calculation in follow_orders.rs assumes isolated margin. \
             Cross margin requires different PnL calculation formula that accounts for shared account equity. \
             Please set risk.use_isolated_margin: true in config.yaml"
        ));
    }
    
    // Also validate exec.default_leverage
    // ✅ FIX: Removed max_leverage limit check for default_leverage
    // User requirement: Each coin can have different max leverage (100x, 125x, etc.)
    // Default leverage is only used as fallback when coin doesn't have max leverage info
    // Exchange will enforce correct leverage limits per symbol
    if cfg.exec.default_leverage == 0 {
        return Err(anyhow!("exec.default_leverage must be greater than 0"));
    }
    // Removed: default_leverage > cfg.risk.max_leverage check
    // Reason: Coin's max leverage (from exchange) may exceed config max_leverage
    // Exchange will enforce correct leverage limits per symbol

    // Take profit percentage validation
    if cfg.take_profit_pct <= 0.0 {
        return Err(anyhow!("take_profit_pct must be greater than 0"));
    }
    if cfg.take_profit_pct >= 100.0 {
        return Err(anyhow!("take_profit_pct must be less than 100"));
    }

    // Stop loss percentage validation
    if cfg.stop_loss_pct <= 0.0 {
        return Err(anyhow!("stop_loss_pct must be greater than 0"));
    }
    if cfg.stop_loss_pct >= cfg.take_profit_pct {
        return Err(anyhow!(
            "stop_loss_pct ({}) must be less than take_profit_pct ({})",
            cfg.stop_loss_pct,
            cfg.take_profit_pct
        ));
    }

    // Order size validation (legacy)
    if cfg.max_usd_per_order <= cfg.min_usd_per_order {
        return Err(anyhow!(
            "max_usd_per_order ({}) must be greater than min_usd_per_order ({})",
            cfg.max_usd_per_order,
            cfg.min_usd_per_order
        ));
    }
    if cfg.min_usd_per_order <= 0.0 {
        return Err(anyhow!("min_usd_per_order must be greater than 0"));
    }

    // Dynamic margin range validation
    if cfg.min_margin_usd <= 0.0 {
        return Err(anyhow!("min_margin_usd must be greater than 0"));
    }
    if cfg.max_margin_usd <= cfg.min_margin_usd {
        return Err(anyhow!(
            "max_margin_usd ({}) must be greater than min_margin_usd ({})",
            cfg.max_margin_usd,
            cfg.min_margin_usd
        ));
    }
    // Clamp max_usd_per_order to [min_margin_usd, max_margin_usd] range
    if cfg.max_usd_per_order < cfg.min_margin_usd || cfg.max_usd_per_order > cfg.max_margin_usd {
        return Err(anyhow!(
            "max_usd_per_order ({}) must be within [min_margin_usd ({}), max_margin_usd ({})] range",
            cfg.max_usd_per_order,
            cfg.min_margin_usd,
            cfg.max_margin_usd
        ));
    }
    // Validate margin strategy
    let valid_strategies = ["fixed", "dynamic", "trend_based", "balance_based", "max_balance"];
    if !valid_strategies.contains(&cfg.margin_strategy.as_str()) {
        return Err(anyhow!(
            "margin_strategy must be one of: 'fixed', 'dynamic', 'trend_based', 'balance_based', 'max_balance', got '{}'",
            cfg.margin_strategy
        ));
    }

    // Minimum quote balance validation
    if cfg.min_quote_balance_usd <= 0.0 {
        return Err(anyhow!("min_quote_balance_usd must be greater than 0"));
    }

    // Note: min_quote_balance_usd is NOT required to be >= max_margin_usd
    // For small balances (< max_margin_usd), we use dynamic threshold:
    // - Minimum balance = min_margin_usd + commission buffer (e.g., 10 + 5 = 15 USD)
    // - The system will use all available balance (up to max_margin_usd) for margin
    // - Example: 20 USD balance → use 20 USD margin (all available, up to max 100 USD)
    // This allows trading with any balance >= min_margin_usd + commission buffer

    // ✅ CRITICAL: Validate that profitable trades are possible
    // For a trade to be profitable, take_profit_pct must exceed stop_loss_pct + total costs
    // Total costs = entry commission + exit commission + spread + slippage + funding rate
    // 
    // Mathematical model:
    // - Entry commission: taker_commission_pct (worst case: market order)
    // - Exit commission (TP): taker_commission_pct (on exit price)
    // - Exit commission (SL): taker_commission_pct (on exit price)
    // - Spread cost: average spread (bps) / 10000 (convert to percentage)
    // - Slippage: estimated 0.01% for liquid pairs (conservative)
    // - Funding rate: estimated 0.01% per 8 hours (perpetual futures)
    //   For HFT, average trade duration is short (< 1 hour), but we include it for safety
    //
    // Worst case total cost = 2 * taker_commission_pct + spread_pct + slippage_pct + funding_rate_pct
    // We need: take_profit_pct > stop_loss_pct + total_cost
    //
    // For HFT with 0.5$ profit target:
    // - Margin: 10-100 USD
    // - Leverage: 100x (max)
    // - Notional: 1000-10000 USD
    // - Target profit: 0.5 USD = 0.05% of 1000 USD notional (worst case)
    // - This means TP% must be at least 0.05% + costs
    let total_commission_pct = 2.0 * cfg.risk.taker_commission_pct;
    
    // Estimate spread cost: average spread in bps / 10000
    // Default: assume 0.02% spread (2 bps) for major pairs
    let avg_spread_bps = (cfg.trending.min_spread_bps + cfg.trending.max_spread_bps) / 2.0;
    let spread_pct = avg_spread_bps / 10000.0;
    
    // Slippage estimate: 0.01% for liquid pairs (conservative)
    const SLIPPAGE_PCT: f64 = 0.01;
    
    // Funding rate estimate: 0.01% per 8 hours (typical for perpetual futures)
    // For HFT, average trade duration is short (< 1 hour), but we include it for safety
    // Conservative estimate: assume 1 hour average trade duration = 0.01% / 8 = 0.00125%
    // For simplicity, use 0.01% as worst case (if trade lasts full 8 hours)
    const ESTIMATED_FUNDING_RATE_PCT: f64 = 0.01; // 0.01% per 8 hours (worst case)
    
    // Total cost = commission + spread + slippage + funding rate
    let total_cost_pct = total_commission_pct + spread_pct + SLIPPAGE_PCT + ESTIMATED_FUNDING_RATE_PCT;
    
    // Minimum required TP = SL% + total costs
    let min_required_tp = cfg.stop_loss_pct + total_cost_pct;
    
    if cfg.take_profit_pct <= min_required_tp {
        return Err(anyhow!(
            "take_profit_pct ({}) must be greater than stop_loss_pct ({}) + total costs ({}). \
             Total costs breakdown: commission={}%, spread={}%, slippage={}%, funding_rate={}%. \
             Current: {} <= {}. Profitable trades would be impossible. \
             For HFT with 0.5$ profit target, consider: TP% >= {}%",
            cfg.take_profit_pct,
            cfg.stop_loss_pct,
            total_cost_pct,
            total_commission_pct,
            spread_pct,
            SLIPPAGE_PCT,
            ESTIMATED_FUNDING_RATE_PCT,
            cfg.take_profit_pct,
            min_required_tp,
            min_required_tp
        ));
    }
    
    // Additional validation for HFT strategy (0.5$ profit target)
    // With 100x leverage and 10-100 USD margin:
    // - Min notional: 10 * 100 = 1000 USD
    // - Max notional: 100 * 100 = 10000 USD
    // - Target profit: 0.5 USD
    // - Min profit %: 0.5 / 10000 = 0.005% (worst case, max notional)
    // - Max profit %: 0.5 / 1000 = 0.05% (best case, min notional)
    // 
    // For consistency, TP% should be at least 0.05% to achieve 0.5$ profit on min notional
    // But this is too small - we need TP% to cover costs + profit
    // Recommended: TP% >= 0.1% for HFT strategy (allows 0.5$ profit after costs)
    const MIN_HFT_TP_PCT: f64 = 0.1;
    if cfg.take_profit_pct < MIN_HFT_TP_PCT {
        eprintln!(
            "⚠️  WARNING: take_profit_pct ({}) is very small for HFT strategy (target: 0.5$ profit). \
             Recommended: TP% >= {}% to ensure profitability after costs. \
             Current TP% may not achieve 0.5$ profit target consistently.",
            cfg.take_profit_pct,
            MIN_HFT_TP_PCT
        );
    }

    // Validate trending spread configuration
    if cfg.trending.min_spread_bps < 0.0 {
        return Err(anyhow!("trending.min_spread_bps must be non-negative"));
    }
    if cfg.trending.max_spread_bps <= 0.0 {
        return Err(anyhow!("trending.max_spread_bps must be greater than 0"));
    }
    if cfg.trending.min_spread_bps > cfg.trending.max_spread_bps {
        return Err(anyhow!(
            "trending.min_spread_bps ({}) must be less than or equal to trending.max_spread_bps ({})",
            cfg.trending.min_spread_bps,
            cfg.trending.max_spread_bps
        ));
    }

    // CRITICAL: Validate hedge mode configuration
    // Hedge mode support is incomplete and causes system failures
    // 
    // Problem: Current implementation cannot handle hedge mode correctly
    // - Position struct only supports single position per symbol (one qty, one entry)
    // - TP/SL tracking is symbol-based, not position-side-based
    // - If both LONG and SHORT positions exist, only one is tracked (the other is lost)
    // - flatten_position closes ALL positions for the symbol (both LONG and SHORT)
    // - Position tracking is incomplete - LONG and SHORT should be tracked separately
    // 
    // This causes:
    // - Incorrect TP/SL triggers (only one position tracked)
    // - Unintended position closures (both LONG and SHORT closed when closing one)
    // - Position data loss (one position ignored)
    // - System instability and potential financial losses
    // 
    // Full hedge mode support requires:
    // - Position struct to support multiple positions per symbol (LONG and SHORT separately)
    // - Separate TP/SL tracking for LONG and SHORT positions
    // - position_id-based closing (CloseRequest.position_id)
    // - Separate position tracking in ORDERING state
    // - HashMap<(String, PositionSide), PositionInfo> for TP/SL tracking
    if cfg.binance.hedge_mode {
        return Err(anyhow!(
            "CRITICAL: Hedge mode (hedge_mode=true) is NOT supported. \
             Current implementation cannot handle hedge mode correctly and will cause system failures. \
             \
             Limitations: \
             - Position struct only supports single position per symbol \
             - TP/SL tracking is symbol-based, not position-side-based \
             - If both LONG and SHORT positions exist, only one is tracked \
             - flatten_position closes ALL positions (both LONG and SHORT) \
             \
             Please set binance.hedge_mode: false in config.yaml \
             \
             Full hedge mode support requires significant architectural changes: \
             - Position struct to support multiple positions per symbol \
             - Separate TP/SL tracking for LONG and SHORT \
             - position_id-based closing \
             - Separate position tracking in ORDERING state"
        ));
    }

    // ⚠️ CRITICAL: Validate margin mode configuration
    // TP/SL PnL calculation assumes isolated margin - warn if cross margin is used
    if !cfg.risk.use_isolated_margin {
        eprintln!("⚠️  WARNING: Cross margin mode (use_isolated_margin=false) is enabled.");
        eprintln!("   - TP/SL PnL calculation assumes isolated margin");
        eprintln!("   - TP/SL trigger levels will be INCORRECT with cross margin");
        eprintln!("   - This can lead to premature or delayed TP/SL triggers");
        eprintln!("   - This can lead to financial losses");
        eprintln!("   RECOMMENDATION: Use use_isolated_margin=true for correct TP/SL behavior.");
    }

    // ✅ CRITICAL: Validate signal size consistency
    // TRENDING generates signals using: notional = max_usd_per_order * leverage
    // ORDERING validates: notional <= max_position_notional_usd
    // These must be consistent to avoid signals being systematically rejected
    let leverage = cfg.leverage.unwrap_or(cfg.exec.default_leverage);
    let expected_notional = cfg.max_usd_per_order * leverage as f64;
    if expected_notional > cfg.risk.max_position_notional_usd {
        return Err(anyhow!(
            "Config mismatch: TRENDING will generate signals with notional {} (max_usd_per_order {} * leverage {}) but ORDERING limit is {}. Signals will be systematically rejected. Please ensure max_usd_per_order * leverage <= risk.max_position_notional_usd",
            expected_notional,
            cfg.max_usd_per_order,
            leverage,
            cfg.risk.max_position_notional_usd
        ));
    }

    Ok(())
}

// ============================================================================
// Internal Config Default Functions
// ============================================================================

fn default_max_position_size_buffer() -> f64 {
    1.5
}

fn default_opportunity_mode_position_multiplier() -> f64 {
    1.2
}

fn default_opportunity_mode_soft_limit_ratio() -> f64 {
    0.8
}

fn default_opportunity_mode_medium_limit_ratio() -> f64 {
    0.9
}

fn default_opportunity_mode_hard_limit_ratio() -> f64 {
    1.0
}

fn default_pnl_alert_interval_sec() -> u64 {
    60
}

fn default_pnl_alert_threshold_positive() -> f64 {
    0.1 // 10%
}

fn default_pnl_alert_threshold_negative() -> f64 {
    -0.05 // -5%
}
