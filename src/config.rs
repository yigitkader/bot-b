use serde::Deserialize;
use std::fs;
use std::str::FromStr;

#[allow(dead_code)]
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
    pub max_position_usdt: f64,
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
}

impl BotConfig {
    pub fn from_env() -> Self {
        let file_cfg = FileConfig::load("config.yaml").unwrap_or_default();
        let binance_cfg = file_cfg.binance.as_ref();
        let trending_cfg = file_cfg.trending.as_ref();

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
            max_position_usdt: numeric_setting("BOT_MAX_POSITION_USDT", None, 50.0),
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
            warmup_min_ticks: Self::env_or("BOT_WARMUP_MIN_TICKS", EMA_SLOW_PERIOD + 10),
        }
    }

    pub fn trend_params(&self) -> TrendParams {
        TrendParams {
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

    fn env_or<T>(key: &str, default: T) -> T
    where
        T: FromStr,
    {
        std::env::var(key)
            .ok()
            .and_then(|v| v.parse::<T>().ok())
            .unwrap_or(default)
    }
}

#[derive(Debug, Clone)]
pub struct TrendParams {
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
}

// fallback for warmup default referencing EMA slow period
const EMA_SLOW_PERIOD: usize = 55;

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    #[serde(default)]
    symbols: Vec<String>,
    #[serde(default)]
    quote_asset: Option<String>,
    #[serde(default)]
    leverage: Option<f64>,
    #[serde(default)]
    take_profit_pct: Option<f64>,
    #[serde(default)]
    stop_loss_pct: Option<f64>,
    #[serde(default)]
    trending: Option<FileTrending>,
    #[serde(default)]
    binance: Option<FileBinance>,
}

#[derive(Debug, Default, Deserialize)]
struct FileBinance {
    #[serde(default)]
    futures_base: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    secret_key: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct FileTrending {
    #[serde(default)]
    rsi_lower_long: Option<f64>,
    #[serde(default)]
    rsi_upper_long: Option<f64>,
    #[serde(default)]
    signal_cooldown_seconds: Option<i64>,
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
