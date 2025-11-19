use serde::Deserialize;
use std::fs;
use std::str::FromStr;
use crate::types::{FileConfig, TrendParams};

#[derive(Debug, Clone, Deserialize)]
pub struct BotConfig {
    pub api_key: String,
    pub api_secret: String,
    pub base_url: String,
    pub ws_base_url: String,
    pub symbol: String,
    pub quote_asset: String,
    pub leverage: f64,
    pub tp_percent: f64,
    pub sl_percent: f64,
    pub position_size_quote: f64,
    pub ema_fast_period: usize,
    pub ema_slow_period: usize,
    pub rsi_period: usize,
    pub atr_period: usize,
    pub rsi_long_min: f64,
    pub rsi_short_max: f64,
    pub atr_rising_factor: f64,
    pub obi_long_min: f64,
    pub obi_short_max: f64,
    pub funding_max_for_long: f64,
    pub funding_min_for_short: f64,
    pub long_min_score: usize,
    pub short_min_score: usize,
    pub signal_cooldown_secs: i64,
    pub warmup_min_ticks: usize,
    pub recv_window_ms: u64,
    // Liquidation cluster thresholds (normalized ratio: liquidation_notional / open_interest)
    // Typical values: 0.0001 (0.01%) to 0.01 (1%) of OI
    // Higher values indicate more significant liquidation pressure
    pub liq_long_cluster_min: Option<f64>,
    pub liq_short_cluster_min: Option<f64>,
}

impl BotConfig {
    pub fn from_env() -> Self {
        let file_cfg = FileConfig::load("config.yaml").unwrap_or_default();
        let binance_cfg = file_cfg.binance.as_ref();
        let trending_cfg = file_cfg.trending.as_ref();
        let ema_fast_period = numeric_setting(
            "BOT_EMA_FAST_PERIOD",
            trending_cfg.and_then(|t| t.ema_fast_period),
            21usize,
        );
        let ema_slow_period = numeric_setting(
            "BOT_EMA_SLOW_PERIOD",
            trending_cfg.and_then(|t| t.ema_slow_period),
            55usize,
        );
        let rsi_period = numeric_setting(
            "BOT_RSI_PERIOD",
            trending_cfg.and_then(|t| t.rsi_period),
            14usize,
        );
        let atr_period = numeric_setting(
            "BOT_ATR_PERIOD",
            trending_cfg.and_then(|t| t.atr_period),
            14usize,
        );
        let warmup_min_ticks = numeric_setting(
            "BOT_WARMUP_MIN_TICKS",
            trending_cfg.and_then(|t| t.warmup_min_ticks),
            ema_slow_period + 10,
        );

        Self {
            api_key: string_setting(
                "BINANCE_API_KEY",
                binance_cfg.and_then(|b| b.api_key.clone()),
                "",
            ),
            api_secret: string_setting(
                "BINANCE_API_SECRET",
                binance_cfg.and_then(|b| b.secret_key.clone()),
                "",
            ),
            base_url: string_setting(
                "BINANCE_BASE_URL",
                binance_cfg.and_then(|b| b.futures_base.clone()),
                "https://fapi.binance.com",
            ),
            ws_base_url: string_setting("BINANCE_WS_URL", None, "wss://fstream.binance.com"),
            symbol: string_setting("BOT_SYMBOL", file_cfg.symbols.first().cloned(), "BTCUSDT"),
            quote_asset: string_setting("BOT_QUOTE_ASSET", file_cfg.quote_asset.clone(), "USDT"),
            leverage: numeric_setting("BOT_LEVERAGE", file_cfg.leverage, 10.0),
            tp_percent: numeric_setting("BOT_TP_PERCENT", file_cfg.take_profit_pct, 1.0),
            sl_percent: numeric_setting("BOT_SL_PERCENT", file_cfg.stop_loss_pct, 0.5),
            position_size_quote: numeric_setting_with_alias(
                "BOT_POSITION_SIZE_QUOTE",
                Some("BOT_MAX_POSITION_USDT"),
                file_cfg.max_usd_per_order,
                50.0,
            ),
            ema_fast_period,
            ema_slow_period,
            rsi_period,
            atr_period,
            rsi_long_min: numeric_setting(
                "BOT_RSI_LONG_MIN",
                trending_cfg.and_then(|t| t.rsi_lower_long),
                55.0,
            ),
            rsi_short_max: numeric_setting(
                "BOT_RSI_SHORT_MAX",
                trending_cfg.and_then(|t| t.rsi_upper_long),
                45.0,
            ),
            atr_rising_factor: numeric_setting("BOT_ATR_RISING_FACTOR", None, 1.02),
            obi_long_min: numeric_setting("BOT_OBI_LONG_MIN", None, 1.15),
            obi_short_max: numeric_setting("BOT_OBI_SHORT_MAX", None, 0.85),
            funding_max_for_long: numeric_setting("BOT_FUNDING_MAX_FOR_LONG", None, 0.0),
            funding_min_for_short: numeric_setting("BOT_FUNDING_MIN_FOR_SHORT", None, 0.0),
            long_min_score: numeric_setting("BOT_LONG_MIN_SCORE", None, 4usize),
            short_min_score: numeric_setting("BOT_SHORT_MIN_SCORE", None, 4usize),
            signal_cooldown_secs: numeric_setting(
                "BOT_SIGNAL_COOLDOWN_SECS",
                trending_cfg.and_then(|t| t.signal_cooldown_seconds),
                30i64,
            ),
            warmup_min_ticks,
            recv_window_ms: numeric_setting(
                "BINANCE_RECV_WINDOW_MS",
                binance_cfg.and_then(|b| b.recv_window_ms),
                5000u64,
            ),
            use_isolated_margin: bool_setting(
                "BOT_USE_ISOLATED_MARGIN",
                file_cfg.risk.as_ref().and_then(|r| r.use_isolated_margin),
                false,
            ),
            min_margin_usd: numeric_setting(
                "BOT_MIN_MARGIN_USD",
                file_cfg.min_margin_usd,
                10.0,
            ),
            max_position_notional_usd: numeric_setting(
                "BOT_MAX_POSITION_NOTIONAL_USD",
                file_cfg.risk.as_ref().and_then(|r| r.max_position_notional_usd),
                30000.0,
            ),
            min_quote_balance_usd: numeric_setting(
                "BOT_MIN_QUOTE_BALANCE_USD",
                file_cfg.min_quote_balance_usd,
                20.0,
            ),
            // Liquidation cluster thresholds (ratio: liquidation_notional / open_interest)
            // Default: None (no filtering). Typical values: 0.0001 (0.01%) to 0.01 (1%)
            // Example: 0.001 means liquidations must be >= 0.1% of OI to be considered significant
            liq_long_cluster_min: std::env::var("BOT_LIQ_LONG_CLUSTER_MIN")
                .ok()
                .and_then(|v| v.parse::<f64>().ok()),
            liq_short_cluster_min: std::env::var("BOT_LIQ_SHORT_CLUSTER_MIN")
                .ok()
                .and_then(|v| v.parse::<f64>().ok()),
        }
    }

    pub fn trend_params(&self) -> TrendParams {
        TrendParams {
            ema_fast_period: self.ema_fast_period,
            ema_slow_period: self.ema_slow_period,
            rsi_period: self.rsi_period,
            atr_period: self.atr_period,
            leverage: self.leverage,
            position_size_quote: self.position_size_quote,
            rsi_long_min: self.rsi_long_min,
            rsi_short_max: self.rsi_short_max,
            atr_rising_factor: self.atr_rising_factor,
            obi_long_min: self.obi_long_min,
            obi_short_max: self.obi_short_max,
            funding_max_for_long: self.funding_max_for_long,
            funding_min_for_short: self.funding_min_for_short,
            long_min_score: self.long_min_score,
            short_min_score: self.short_min_score,
            signal_cooldown_secs: self.signal_cooldown_secs,
            warmup_min_ticks: self.warmup_min_ticks,
        }
    }
}



impl FileConfig {
    fn load(path: &str) -> Option<Self> {
        let content = fs::read_to_string(path).ok()?;
        serde_yaml::from_str(&content).ok()
    }
}

fn string_setting(env_key: &str, file_value: Option<String>, default: &str) -> String {
    if let Ok(val) = std::env::var(env_key) {
        if !val.is_empty() {
            return val;
        }
    }
    file_value.unwrap_or_else(|| default.to_string())
}

fn numeric_setting<T>(env_key: &str, file_value: Option<T>, default: T) -> T
where
    T: FromStr + Copy,
{
    std::env::var(env_key)
        .ok()
        .and_then(|v| v.parse::<T>().ok())
        .or(file_value)
        .unwrap_or(default)
}

fn numeric_setting_with_alias<T>(
    primary_env: &str,
    alias_env: Option<&str>,
    file_value: Option<T>,
    default: T,
) -> T
where
    T: FromStr + Copy,
{
    if let Some(val) = std::env::var(primary_env)
        .ok()
        .and_then(|v| v.parse::<T>().ok())
    {
        return val;
    }
    if let Some(alias) = alias_env {
        if let Some(val) = std::env::var(alias).ok().and_then(|v| v.parse::<T>().ok()) {
            return val;
        }
    }
    file_value.unwrap_or(default)
}

fn bool_setting(env_key: &str, file_value: Option<bool>, default: bool) -> bool {
    if let Ok(val) = std::env::var(env_key) {
        if let Ok(parsed) = val.parse::<bool>() {
            return parsed;
        }
        // Also support "true"/"false" strings
        match val.to_lowercase().as_str() {
            "true" | "1" | "yes" => return true,
            "false" | "0" | "no" => return false,
            _ => {}
        }
    }
    file_value.unwrap_or(default)
}
