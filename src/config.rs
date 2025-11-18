
use anyhow::{anyhow, Result};
use serde::Deserialize;
#[derive(Debug, Deserialize, Clone, Default)]
pub struct RiskCfg {
    #[serde(default = "default_max_leverage")]
    #[allow(dead_code)]
    pub max_leverage: u32,
    #[serde(default = "default_use_isolated_margin")]
    pub use_isolated_margin: bool,
    #[serde(default = "default_max_position_notional_usd")]
    pub max_position_notional_usd: f64,
    #[serde(default = "default_maker_commission_pct")]
    pub maker_commission_pct: f64,
    #[serde(default = "default_taker_commission_pct")]
    pub taker_commission_pct: f64,
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
    #[serde(default = "default_hft_mode")]
    pub hft_mode: bool,
    #[serde(default = "default_require_volume_confirmation")]
    pub require_volume_confirmation: bool,
    #[serde(default = "default_use_trailing_stop")]
    pub use_trailing_stop: bool,
    #[serde(default = "default_trailing_stop_callback_rate")]
    pub trailing_stop_callback_rate: f64,
    #[serde(default = "default_rsi_min_avg_loss")]
    pub rsi_min_avg_loss: f64,
    #[serde(default = "default_atr_period")]
    pub atr_period: usize,
    #[serde(default = "default_atr_sl_multiplier")]
    pub atr_sl_multiplier: f64,
    #[serde(default = "default_atr_tp_multiplier")]
    pub atr_tp_multiplier: f64,
    #[serde(default = "default_low_volatility_threshold")]
    pub low_volatility_threshold: f64,
    #[serde(default = "default_high_volatility_threshold")]
    pub high_volatility_threshold: f64,
    #[serde(default = "default_base_volatility")]
    pub base_volatility: f64,
    #[serde(default = "default_base_min_score")]
    pub base_min_score: f64,
    #[serde(default = "default_regime_multiplier_trending")]
    pub regime_multiplier_trending: f64,
    #[serde(default = "default_regime_multiplier_ranging")]
    pub regime_multiplier_ranging: f64,
    #[serde(default = "default_regime_multiplier_volatile")]
    pub regime_multiplier_volatile: f64,
    #[serde(default = "default_regime_multiplier_unknown")]
    pub regime_multiplier_unknown: f64,
    #[serde(default = "default_volume_multiplier_hft")]
    pub volume_multiplier_hft: f64,
    #[serde(default = "default_volume_multiplier_normal")]
    pub volume_multiplier_normal: f64,
    #[serde(default = "default_trend_threshold_hft")]
    pub trend_threshold_hft: f64,
    #[serde(default = "default_trend_threshold_normal")]
    pub trend_threshold_normal: f64,
    #[serde(default = "default_weak_trend_score_multiplier")]
    pub weak_trend_score_multiplier: f64,
    #[serde(default = "default_rsi_lower_long")]
    pub rsi_lower_long: f64,
    #[serde(default = "default_rsi_upper_long")]
    pub rsi_upper_long: f64,
    #[serde(default = "default_rsi_lower_short")]
    #[allow(dead_code)]
    pub rsi_lower_short: f64,
    #[serde(default = "default_rsi_upper_short")]
    pub rsi_upper_short: f64,
    #[serde(default = "default_ema_short_score")]
    pub ema_short_score: f64,
    #[serde(default = "default_ema_mid_score")]
    pub ema_mid_score: f64,
    #[serde(default = "default_slope_score")]
    pub slope_score: f64,
    #[serde(default = "default_rsi_score")]
    pub rsi_score: f64,
    #[serde(default = "default_min_analysis_interval_ms")]
    pub min_analysis_interval_ms: u64,
    #[serde(default = "default_default_spread_quality")]
    pub default_spread_quality: f64,
}
#[derive(Debug, Deserialize, Clone, Default)]
pub struct ExecCfg {
    #[serde(default = "default_tif")]
    pub tif: String,
    #[serde(default = "default_default_leverage")]
    pub default_leverage: u32,
    #[serde(default = "default_min_profit_usd")]
    pub min_profit_usd: f64,
    #[serde(default = "default_max_position_duration_sec")]
    pub max_position_duration_sec: f64,
    #[serde(default = "default_max_loss_duration_sec")]
    pub max_loss_duration_sec: f64,
    #[serde(default = "default_time_weighted_threshold_early")]
    pub time_weighted_threshold_early: f64,
    #[serde(default = "default_time_weighted_threshold_normal")]
    pub time_weighted_threshold_normal: f64,
    #[serde(default = "default_time_weighted_threshold_mid")]
    pub time_weighted_threshold_mid: f64,
    #[serde(default = "default_time_weighted_threshold_late")]
    pub time_weighted_threshold_late: f64,
    #[serde(default = "default_trailing_stop_threshold_ratio")]
    pub trailing_stop_threshold_ratio: f64,
    #[serde(default = "default_max_loss_threshold_ratio")]
    pub max_loss_threshold_ratio: f64,
    #[serde(default = "default_stop_loss_threshold_ratio")]
    pub stop_loss_threshold_ratio: f64,
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
    #[serde(default = "default_max_position_size_buffer")]
    pub max_position_size_buffer: f64,
    #[serde(default = "default_opportunity_mode_position_multiplier")]
    pub opportunity_mode_position_multiplier: f64,
    #[serde(default = "default_opportunity_mode_soft_limit_ratio")]
    pub opportunity_mode_soft_limit_ratio: f64,
    #[serde(default = "default_opportunity_mode_medium_limit_ratio")]
    pub opportunity_mode_medium_limit_ratio: f64,
    #[serde(default = "default_opportunity_mode_hard_limit_ratio")]
    pub opportunity_mode_hard_limit_ratio: f64,
    #[serde(default = "default_pnl_alert_interval_sec")]
    #[allow(dead_code)]
    pub pnl_alert_interval_sec: u64,
    #[serde(default = "default_pnl_alert_threshold_positive")]
    #[allow(dead_code)]
    pub pnl_alert_threshold_positive: f64,
    #[serde(default = "default_pnl_alert_threshold_negative")]
    #[allow(dead_code)]
    pub pnl_alert_threshold_negative: f64,
}
#[derive(Debug, Deserialize, Clone, Default)]
pub struct EventBusCfg {
    #[serde(default = "default_market_tick_buffer")]
    pub market_tick_buffer: usize,
    #[serde(default = "default_trade_signal_buffer")]
    pub trade_signal_buffer: usize,
    #[serde(default = "default_default_event_buffer")]
    pub close_request_buffer: usize,
    #[serde(default = "default_default_event_buffer")]
    pub order_update_buffer: usize,
    #[serde(default = "default_default_event_buffer")]
    pub position_update_buffer: usize,
    #[serde(default = "default_default_event_buffer")]
    pub balance_update_buffer: usize,
}
#[derive(Debug, Deserialize, Clone, Default)]
pub struct DynamicSymbolSelection {
    #[serde(default = "default_dynamic_symbol_selection_enabled")]
    pub enabled: bool,
    #[serde(default = "default_max_symbols")]
    pub max_symbols: usize,
    #[serde(default = "default_rotation_interval_minutes")]
    pub rotation_interval_minutes: u64,
    #[serde(default = "default_min_volatility_pct")]
    pub min_volatility_pct: f64,
    #[serde(default = "default_min_quote_volume")]
    pub min_quote_volume: f64,
    #[serde(default = "default_min_trades_24h")]
    pub min_trades_24h: u64,
    #[serde(default = "default_volatility_weight")]
    pub volatility_weight: f64,
    #[serde(default = "default_volume_weight")]
    pub volume_weight: f64,
    #[serde(default = "default_trades_weight")]
    pub trades_weight: f64,
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
impl ::std::default::Default for BinanceCfg {
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
    #[serde(default = "default_max_usd_per_order")]
    pub max_usd_per_order: f64,
    #[serde(default = "default_min_usd_per_order")]
    pub min_usd_per_order: f64,
    #[serde(default = "default_min_margin_usd")]
    pub min_margin_usd: f64,
    #[serde(default = "default_max_margin_usd")]
    pub max_margin_usd: f64,
    #[serde(default = "default_margin_strategy")]
    pub margin_strategy: String,
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
    #[serde(default)]
    pub event_bus: EventBusCfg,
    #[serde(default)]
    pub dynamic_symbol_selection: DynamicSymbolSelection,
    #[serde(default)]
    pub internal: InternalCfg,
}
impl ::std::default::Default for AppCfg {
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
    0.02
}
fn default_taker_commission_pct() -> f64 {
    0.04
}
fn default_min_spread_bps() -> f64 {
    5.0
}
fn default_max_spread_bps() -> f64 {
    200.0
}
fn default_signal_cooldown_seconds() -> u64 {
    5
}
fn default_hft_mode() -> bool {
    true
}
fn default_require_volume_confirmation() -> bool {
    false
}
fn default_use_trailing_stop() -> bool {
    false
}
fn default_trailing_stop_callback_rate() -> f64 {
    0.001
}
fn default_rsi_min_avg_loss() -> f64 {
    0.0001
}
fn default_atr_period() -> usize {
    14
}
fn default_atr_sl_multiplier() -> f64 {
    3.0
}
fn default_atr_tp_multiplier() -> f64 {
    6.0
}
fn default_low_volatility_threshold() -> f64 {
    0.5
}
fn default_high_volatility_threshold() -> f64 {
    2.0
}
fn default_base_volatility() -> f64 {
    0.02
}
fn default_base_min_score() -> f64 {
    3.5
}
fn default_regime_multiplier_trending() -> f64 {
    0.95
}
fn default_regime_multiplier_ranging() -> f64 {
    1.1
}
fn default_regime_multiplier_volatile() -> f64 {
    1.15
}
fn default_regime_multiplier_unknown() -> f64 {
    1.0
}
fn default_volume_multiplier_hft() -> f64 {
    1.1
}
fn default_volume_multiplier_normal() -> f64 {
    1.3
}
fn default_trend_threshold_hft() -> f64 {
    0.5
}
fn default_trend_threshold_normal() -> f64 {
    0.7
}
fn default_weak_trend_score_multiplier() -> f64 {
    1.1
}
fn default_rsi_lower_long() -> f64 {
    55.0
}
fn default_rsi_upper_long() -> f64 {
    70.0
}
fn default_rsi_lower_short() -> f64 {
    25.0
}
fn default_rsi_upper_short() -> f64 {
    50.0
}
fn default_ema_short_score() -> f64 {
    2.5
}
fn default_ema_mid_score() -> f64 {
    2.0
}
fn default_slope_score() -> f64 {
    1.5
}
fn default_rsi_score() -> f64 {
    1.5
}
fn default_min_analysis_interval_ms() -> u64 {
    100
}
fn default_default_spread_quality() -> f64 {
    0.5
}
fn default_min_commission_buffer_usd() -> f64 {
    0.5
}
fn default_min_margin_usd() -> f64 {
    10.0
}
fn default_max_margin_usd() -> f64 {
    100.0
}
fn default_margin_strategy() -> String {
    "fixed".to_string()
}
fn default_tif() -> String {
    "post_only".to_string()
}
fn default_default_leverage() -> u32 {
    20
}
fn default_min_profit_usd() -> f64 {
    0.50
}
fn default_max_position_duration_sec() -> f64 {
    300.0
}
fn default_max_loss_duration_sec() -> f64 {
    120.0
}
fn default_time_weighted_threshold_early() -> f64 {
    0.6
}
fn default_time_weighted_threshold_normal() -> f64 {
    1.0
}
fn default_time_weighted_threshold_mid() -> f64 {
    0.4
}
fn default_time_weighted_threshold_late() -> f64 {
    0.2
}
fn default_trailing_stop_threshold_ratio() -> f64 {
    0.5
}
fn default_max_loss_threshold_ratio() -> f64 {
    2.0
}
fn default_stop_loss_threshold_ratio() -> f64 {
    0.8
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
    10000
}
fn default_trade_signal_buffer() -> usize {
    1000
}
fn default_default_event_buffer() -> usize {
    1000
}
fn default_dynamic_symbol_selection_enabled() -> bool {
    false
}
fn default_max_symbols() -> usize {
    30
}
fn default_rotation_interval_minutes() -> u64 {
    15
}
fn default_min_volatility_pct() -> f64 {
    1.5
}
fn default_min_quote_volume() -> f64 {
    1_000_000.0
}
fn default_min_trades_24h() -> u64 {
    1000
}
fn default_volatility_weight() -> f64 {
    2.0
}
fn default_volume_weight() -> f64 {
    1.0
}
fn default_trades_weight() -> f64 {
    0.5
}
fn default_spread_weight() -> f64 {
    1.0
}
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
    let quote_upper = cfg.quote_asset.to_uppercase();
    if quote_upper != "USDC" && quote_upper != "USDT" {
        return Err(anyhow!(
            "quote_asset must be either 'USDC' or 'USDT', got '{}'. Only USDC and USDT are supported.",
            cfg.quote_asset
        ));
    }
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
    if let Some(leverage) = cfg.leverage {
        if leverage == 0 {
            return Err(anyhow!("leverage must be greater than 0"));
        }
    }
    if !cfg.risk.use_isolated_margin {
        return Err(anyhow!(
            "CRITICAL: Cross margin mode is NOT supported for TP/SL. \
             PnL calculation in follow_orders.rs assumes isolated margin. \
             Cross margin requires different PnL calculation formula that accounts for shared account equity. \
             Please set risk.use_isolated_margin: true in config.yaml"
        ));
    }
    if cfg.exec.default_leverage == 0 {
        return Err(anyhow!("exec.default_leverage must be greater than 0"));
    }
    if cfg.take_profit_pct <= 0.0 {
        return Err(anyhow!("take_profit_pct must be greater than 0"));
    }
    if cfg.take_profit_pct >= 100.0 {
        return Err(anyhow!("take_profit_pct must be less than 100"));
    }
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
    if cfg.max_usd_per_order < cfg.min_margin_usd || cfg.max_usd_per_order > cfg.max_margin_usd {
        return Err(anyhow!(
            "max_usd_per_order ({}) must be within [min_margin_usd ({}), max_margin_usd ({})] range",
            cfg.max_usd_per_order,
            cfg.min_margin_usd,
            cfg.max_margin_usd
        ));
    }
    let valid_strategies = ["fixed", "dynamic", "trend_based", "balance_based", "max_balance"];
    if !valid_strategies.contains(&cfg.margin_strategy.as_str()) {
        return Err(anyhow!(
            "margin_strategy must be one of: 'fixed', 'dynamic', 'trend_based', 'balance_based', 'max_balance', got '{}'",
            cfg.margin_strategy
        ));
    }
    if cfg.min_quote_balance_usd <= 0.0 {
        return Err(anyhow!("min_quote_balance_usd must be greater than 0"));
    }
    let total_commission_pct = 2.0 * cfg.risk.taker_commission_pct;
    let avg_spread_bps = (cfg.trending.min_spread_bps + cfg.trending.max_spread_bps) / 2.0;
    let spread_pct = avg_spread_bps / 10000.0;
    const SLIPPAGE_PCT: f64 = 0.01;
    const ESTIMATED_FUNDING_RATE_PCT: f64 = 0.01;
    let total_cost_pct = total_commission_pct + spread_pct + SLIPPAGE_PCT + ESTIMATED_FUNDING_RATE_PCT;
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
    if !cfg.risk.use_isolated_margin {
        eprintln!("⚠️  WARNING: Cross margin mode (use_isolated_margin=false) is enabled.");
        eprintln!("   - TP/SL PnL calculation assumes isolated margin");
        eprintln!("   - TP/SL trigger levels will be INCORRECT with cross margin");
        eprintln!("   - This can lead to premature or delayed TP/SL triggers");
        eprintln!("   - This can lead to financial losses");
        eprintln!("   RECOMMENDATION: Use use_isolated_margin=true for correct TP/SL behavior.");
    }
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
    0.1
}
fn default_pnl_alert_threshold_negative() -> f64 {
    -0.05
}
