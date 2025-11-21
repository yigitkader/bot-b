use crate::types::{FileConfig, TrendParams};
use serde::Deserialize;
use std::fs;
use std::str::FromStr;

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
    pub atr_sl_multiplier: f64, // ATR multiplier for stop loss (dynamic SL = ATR * multiplier)
    pub atr_tp_multiplier: f64, // ATR multiplier for take profit (dynamic TP = ATR * multiplier)
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
    // Liquidation cluster aggregation window (seconds)
    // Lower values (10-20s) for high volatility markets
    // Higher values (30-60s) for stable markets
    pub liq_window_secs: u64,
    // Risk management fields
    pub use_isolated_margin: bool,
    pub min_margin_usd: f64,
    pub max_position_notional_usd: f64,
    pub min_quote_balance_usd: f64,
    pub max_slippage_bps: f64,
    // ✅ ADIM 2: Config.yaml parametreleri (TrendPlan.md)
    pub hft_mode: bool,
    pub base_min_score: f64,
    pub trend_threshold_hft: f64,
    pub trend_threshold_normal: f64,
    pub weak_trend_score_multiplier: f64,
    pub regime_multiplier_trending: f64,
    pub regime_multiplier_ranging: f64,
    // Enhanced Signal Scoring (TrendPlan.md)
    pub enable_enhanced_scoring: bool,
    pub enhanced_score_excellent: f64,
    pub enhanced_score_good: f64,
    pub enhanced_score_marginal: f64,
    // Order Flow Analysis (TrendPlan.md - Action Plan)
    pub enable_order_flow: bool, // Enable Order Flow analysis (requires real depth data)
    // Execution & Backtest Parameters (no hardcoded values)
    pub fee_bps_round_trip: f64, // Round-trip fee in basis points
    pub max_holding_bars: usize, // Maximum holding time in bars
    pub slippage_bps: f64, // Slippage in basis points
    pub min_holding_bars: usize, // Minimum holding time in bars
    // Signal Quality Filtering (configurable)
    pub min_volume_ratio: f64, // Minimum volume ratio vs 20-bar average
    pub max_volatility_pct: f64, // Maximum ATR volatility %
    pub max_price_change_5bars_pct: f64, // Max price change in 5 bars %
    pub enable_signal_quality_filter: bool, // Enable signal quality filtering
    // Paper Trading Mode (TrendPlan.md - Action Plan)
    pub paper_trading_enabled: bool,
    pub paper_trading_log_file: String, // Path to log file for virtual orders
    // WebSocket Interruption Tolerance (TrendPlan.md - Critical Warnings)
    pub market_tick_stale_tolerance_secs: i64, // Tolerance period for stale MarketTick (default: 30 seconds)
    // If MarketTick is stale but within tolerance, continue signal generation with MTF/Funding
    // but skip Order Flow and Liquidation strategies that require real-time data
    // WebSocket Disconnection Strategy (Plan.md - Veri Akışı Sağlamlığı)
    pub ws_disconnect_close_positions: bool, // Close positions when WebSocket disconnects for too long (default: false)
    pub ws_disconnect_timeout_secs: i64, // Timeout before closing positions on disconnect (default: 60 seconds)
    // If true, bot will close all open positions if WebSocket is disconnected for more than timeout_secs
    // This prevents trading with stale data during extended disconnections
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
            atr_sl_multiplier: numeric_setting(
                "BOT_ATR_SL_MULTIPLIER",
                trending_cfg.and_then(|t| t.atr_sl_multiplier),
                2.0, // Default: 2.0 (SL = 2 * ATR)
            ),
            atr_tp_multiplier: numeric_setting(
                "BOT_ATR_TP_MULTIPLIER",
                trending_cfg.and_then(|t| t.atr_tp_multiplier),
                4.0, // Default: 4.0 (TP = 4 * ATR)
            ),
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
                2500u64, // Default: 2.5s (recommended: 2000-3000ms for security)
            ),
            use_isolated_margin: bool_setting(
                "BOT_USE_ISOLATED_MARGIN",
                file_cfg.risk.as_ref().and_then(|r| r.use_isolated_margin),
                false,
            ),
            min_margin_usd: numeric_setting("BOT_MIN_MARGIN_USD", file_cfg.min_margin_usd, 10.0),
            max_position_notional_usd: numeric_setting(
                "BOT_MAX_POSITION_NOTIONAL_USD",
                file_cfg
                    .risk
                    .as_ref()
                    .and_then(|r| r.max_position_notional_usd),
                30000.0,
            ),
            min_quote_balance_usd: numeric_setting(
                "BOT_MIN_QUOTE_BALANCE_USD",
                file_cfg.min_quote_balance_usd,
                20.0,
            ),
            max_slippage_bps: numeric_setting("BOT_MAX_SLIPPAGE_BPS", None, 25.0),
            // Liquidation cluster thresholds (ratio: liquidation_notional / open_interest)
            // Default: None (no filtering). Typical values: 0.0001 (0.01%) to 0.01 (1%)
            // Example: 0.001 means liquidations must be >= 0.1% of OI to be considered significant
            liq_long_cluster_min: std::env::var("BOT_LIQ_LONG_CLUSTER_MIN")
                .ok()
                .and_then(|v| v.parse::<f64>().ok()),
            liq_short_cluster_min: std::env::var("BOT_LIQ_SHORT_CLUSTER_MIN")
                .ok()
                .and_then(|v| v.parse::<f64>().ok()),
            // Liquidation cluster aggregation window (seconds)
            // Lower values (10-20s) for high volatility markets
            // Higher values (30-60s) for stable markets
            liq_window_secs: numeric_setting(
                "BOT_LIQ_WINDOW_SECS",
                file_cfg.risk.as_ref().and_then(|r| r.liq_window_secs),
                30u64, // Default: 30 seconds
            ),
            // ✅ ADIM 2: Config.yaml parametreleri
            hft_mode: bool_setting("BOT_HFT_MODE", trending_cfg.and_then(|t| t.hft_mode), false),
            base_min_score: numeric_setting(
                "BOT_BASE_MIN_SCORE",
                trending_cfg.and_then(|t| t.base_min_score),
                6.5,
            ),
            trend_threshold_hft: numeric_setting(
                "BOT_TREND_THRESHOLD_HFT",
                trending_cfg.and_then(|t| t.trend_threshold_hft),
                0.5,
            ),
            trend_threshold_normal: numeric_setting(
                "BOT_TREND_THRESHOLD_NORMAL",
                trending_cfg.and_then(|t| t.trend_threshold_normal),
                0.6,
            ),
            weak_trend_score_multiplier: numeric_setting(
                "BOT_WEAK_TREND_SCORE_MULTIPLIER",
                trending_cfg.and_then(|t| t.weak_trend_score_multiplier),
                1.15,
            ),
            regime_multiplier_trending: numeric_setting(
                "BOT_REGIME_MULTIPLIER_TRENDING",
                trending_cfg.and_then(|t| t.regime_multiplier_trending),
                0.9,
            ),
            regime_multiplier_ranging: numeric_setting(
                "BOT_REGIME_MULTIPLIER_RANGING",
                trending_cfg.and_then(|t| t.regime_multiplier_ranging),
                1.15,
            ),
            // Enhanced Signal Scoring (TrendPlan.md)
            enable_enhanced_scoring: bool_setting(
                "BOT_ENABLE_ENHANCED_SCORING",
                trending_cfg.and_then(|t| t.enable_enhanced_scoring),
                false, // Default: disabled
            ),
            enhanced_score_excellent: numeric_setting(
                "BOT_ENHANCED_SCORE_EXCELLENT",
                trending_cfg.and_then(|t| t.enhanced_score_excellent),
                80.0,
            ),
            enhanced_score_good: numeric_setting(
                "BOT_ENHANCED_SCORE_GOOD",
                trending_cfg.and_then(|t| t.enhanced_score_good),
                65.0,
            ),
            enhanced_score_marginal: numeric_setting(
                "BOT_ENHANCED_SCORE_MARGINAL",
                trending_cfg.and_then(|t| t.enhanced_score_marginal),
                50.0,
            ),
            // Order Flow Analysis (TrendPlan.md - Action Plan)
            enable_order_flow: bool_setting(
                "BOT_ENABLE_ORDER_FLOW",
                trending_cfg.and_then(|t| t.enable_order_flow),
                true, // Default: enabled (can be disabled for backtest consistency)
            ),
            // Execution & Backtest Parameters (no hardcoded values)
            fee_bps_round_trip: numeric_setting(
                "BOT_FEE_BPS_ROUND_TRIP",
                trending_cfg.and_then(|t| t.fee_bps_round_trip),
                8.0, // Default: 8.0 bps = 0.08%
            ),
            max_holding_bars: numeric_setting(
                "BOT_MAX_HOLDING_BARS",
                trending_cfg.and_then(|t| t.max_holding_bars),
                48usize, // Default: 48 bars = 4 hours @5m
            ),
            slippage_bps: numeric_setting(
                "BOT_SLIPPAGE_BPS",
                trending_cfg.and_then(|t| t.slippage_bps),
                0.0, // Default: 0.0 (optimistic backtest)
            ),
            min_holding_bars: numeric_setting(
                "BOT_MIN_HOLDING_BARS",
                trending_cfg.and_then(|t| t.min_holding_bars),
                3usize, // Default: 3 bars = 15 minutes @5m
            ),
            // Signal Quality Filtering (configurable)
            min_volume_ratio: numeric_setting(
                "BOT_MIN_VOLUME_RATIO",
                trending_cfg.and_then(|t| t.min_volume_ratio),
                1.5, // Default: 1.5
            ),
            max_volatility_pct: numeric_setting(
                "BOT_MAX_VOLATILITY_PCT",
                trending_cfg.and_then(|t| t.max_volatility_pct),
                2.0, // Default: 2.0%
            ),
            max_price_change_5bars_pct: numeric_setting(
                "BOT_MAX_PRICE_CHANGE_5BARS_PCT",
                trending_cfg.and_then(|t| t.max_price_change_5bars_pct),
                3.0, // Default: 3.0%
            ),
            enable_signal_quality_filter: bool_setting(
                "BOT_ENABLE_SIGNAL_QUALITY_FILTER",
                trending_cfg.and_then(|t| t.enable_signal_quality_filter),
                true, // Default: enabled
            ),
            // Paper Trading Mode (TrendPlan.md - Action Plan)
            paper_trading_enabled: bool_setting(
                "BOT_PAPER_TRADING_ENABLED",
                file_cfg.paper_trading.as_ref().and_then(|pt| pt.enabled),
                false,
            ),
            paper_trading_log_file: string_setting(
                "BOT_PAPER_TRADING_LOG_FILE",
                file_cfg.paper_trading.as_ref().and_then(|pt| pt.log_file.clone()),
                "paper_trading_orders.log",
            ),
            // WebSocket Interruption Tolerance (TrendPlan.md - Critical Warnings)
            market_tick_stale_tolerance_secs: numeric_setting(
                "BOT_MARKET_TICK_STALE_TOLERANCE_SECS",
                trending_cfg.and_then(|t| t.market_tick_stale_tolerance_secs),
                30i64, // Default: 30 seconds
            ),
            // WebSocket Disconnection Strategy (Plan.md - Veri Akışı Sağlamlığı)
            ws_disconnect_close_positions: bool_setting(
                "BOT_WS_DISCONNECT_CLOSE_POSITIONS",
                file_cfg.risk.as_ref().and_then(|r| r.ws_disconnect_close_positions),
                false, // Default: false (don't auto-close positions on disconnect)
            ),
            ws_disconnect_timeout_secs: numeric_setting(
                "BOT_WS_DISCONNECT_TIMEOUT_SECS",
                file_cfg.risk.as_ref().and_then(|r| r.ws_disconnect_timeout_secs),
                60i64, // Default: 60 seconds
            ),
        }
        .validate()
    }

    fn validate(self) -> Self {
        use std::process;

        macro_rules! validate_positive {
            ($value:expr, $name:expr, $env_key:expr, $example:expr) => {
                if $value <= 0.0 {
                    eprintln!("ERROR: {} must be > 0, got: {}", $name, $value);
                    eprintln!("  Fix: Set {} to a positive value ({})", $env_key, $example);
                    process::exit(1);
                }
            };
        }

        macro_rules! validate_positive_int {
            ($value:expr, $name:expr, $env_key:expr) => {
                if $value == 0 {
                    eprintln!("ERROR: {} must be > 0, got: {}", $name, $value);
                    eprintln!("  Fix: Set {} to a positive value", $env_key);
                    process::exit(1);
                }
            };
        }

        macro_rules! validate_range {
            ($value:expr, $name:expr, $min:expr, $max:expr) => {
                if $value < $min || $value > $max {
                    eprintln!("ERROR: {} must be between {}-{}, got: {}", $name, $min, $max, $value);
                    process::exit(1);
                }
            };
        }

        macro_rules! validate_greater {
            ($a:expr, $b:expr, $name_a:expr, $name_b:expr, $env_key:expr) => {
                if $a <= $b {
                    eprintln!("ERROR: {} ({}) must be > {} ({}), got: {} <= {}", $name_a, $a, $name_b, $b, $a, $b);
                    eprintln!("  Fix: Set {} to a value greater than {}", $env_key, $name_b);
                    process::exit(1);
                }
            };
        }

        macro_rules! warn_if {
            ($condition:expr, $message:expr, $value:expr) => {
                if $condition {
                    eprintln!("WARNING: {} (value: {})", $message, $value);
                }
            };
        }

        validate_positive!(self.leverage, "leverage", "BOT_LEVERAGE", "e.g., 10.0");
        validate_positive!(self.tp_percent, "tp_percent", "BOT_TP_PERCENT", "e.g., 1.0 for 1%");
        validate_positive!(self.sl_percent, "sl_percent", "BOT_SL_PERCENT", "e.g., 0.5 for 0.5%");
        validate_positive!(self.position_size_quote, "position_size_quote", "BOT_POSITION_SIZE_QUOTE", "a positive value");
        validate_positive_int!(self.ema_fast_period, "ema_fast_period", "BOT_EMA_FAST_PERIOD");
        validate_positive_int!(self.rsi_period, "rsi_period", "BOT_RSI_PERIOD");
        validate_positive_int!(self.atr_period, "atr_period", "BOT_ATR_PERIOD");
        validate_positive!(self.obi_long_min, "obi_long_min", "BOT_OBI_LONG_MIN", "a positive value");
        validate_positive!(self.obi_short_max, "obi_short_max", "BOT_OBI_SHORT_MAX", "a positive value");
        validate_positive!(self.min_margin_usd, "min_margin_usd", "BOT_MIN_MARGIN_USD", "a positive value");
        validate_positive!(self.min_quote_balance_usd, "min_quote_balance_usd", "BOT_MIN_QUOTE_BALANCE_USD", "a positive value");
        validate_positive!(self.max_position_notional_usd, "max_position_notional_usd", "BOT_MAX_POSITION_NOTIONAL_USD", "a positive value");
        validate_positive_int!(self.liq_window_secs, "liq_window_secs", "BOT_LIQ_WINDOW_SECS");

        if self.sl_percent >= 100.0 {
            eprintln!("ERROR: sl_percent >= 100% will cause instant liquidation, got: {}%", self.sl_percent);
            eprintln!("  Fix: Set BOT_SL_PERCENT to a reasonable value (e.g., 0.5-5.0%)");
            process::exit(1);
        }

        validate_greater!(self.ema_slow_period, self.ema_fast_period, "ema_slow_period", "ema_fast_period", "BOT_EMA_SLOW_PERIOD");
        validate_range!(self.rsi_long_min, "rsi_long_min", 0.0, 100.0);
        validate_range!(self.rsi_short_max, "rsi_short_max", 0.0, 100.0);

        if self.recv_window_ms > 60000 {
            eprintln!("ERROR: recv_window_ms exceeds Binance maximum (60000ms), got: {}ms", self.recv_window_ms);
            eprintln!("  Fix: Set BINANCE_RECV_WINDOW_MS to <= 60000");
            process::exit(1);
        }

        warn_if!(self.tp_percent > 100.0, "tp_percent is very high, this may be intentional", self.tp_percent);
        warn_if!(self.rsi_long_min <= self.rsi_short_max, "rsi_long_min should be > rsi_short_max for proper signal generation", self.rsi_long_min);
        warn_if!(self.obi_short_max >= self.obi_long_min, "obi_short_max should be < obi_long_min for proper signal generation", self.obi_short_max);
        warn_if!(self.recv_window_ms < 1000, "recv_window_ms is very low, may cause order rejections due to network latency. Recommendation: Use 2000-3000ms for security and reliability", self.recv_window_ms);
        warn_if!(self.recv_window_ms > 5000, "recv_window_ms is large, increases replay attack risk. Recommendation: Use 2000-3000ms for better security", self.recv_window_ms);
        warn_if!(self.long_min_score == 0, "long_min_score is 0, all signals will pass (may be intentional)", self.long_min_score);
        warn_if!(self.short_min_score == 0, "short_min_score is 0, all signals will pass (may be intentional)", self.short_min_score);
        warn_if!(self.liq_window_secs < 5, "liq_window_secs is very low, may cause excessive pruning. Recommendation: Use at least 10 seconds for stable operation", self.liq_window_secs);
        warn_if!(self.liq_window_secs > 300, format!("liq_window_secs is very high ({} minutes), may miss short-term liquidation clusters. Recommendation: Use 10-60 seconds for optimal detection", self.liq_window_secs / 60), self.liq_window_secs);

        // ✅ CRITICAL: HFT Mode Commission Warning (Plan.md - HFT Modu Komisyonu)
        // HFT modu çok sık işlem açar. Binance Futures taker komisyonu (yaklaşık %0.04 - %0.05) kârı eritir.
        // fee_bps_round_trip mutlaka 8.0 (0.08%) veya üzeri tutulmalı.
        if self.hft_mode {
            if self.fee_bps_round_trip < 8.0 {
                eprintln!("⚠️  CRITICAL WARNING: HFT mode enabled but fee_bps_round_trip ({}) is below recommended 8.0 (0.08%)", self.fee_bps_round_trip);
                eprintln!("  HFT mode opens many trades. Binance Futures taker commission (~0.04-0.05%) will eat profits.");
                eprintln!("  Recommendation: Set fee_bps_round_trip to at least 8.0 for realistic backtest results.");
                eprintln!("  If your VIP level is low, consider disabling HFT mode or using higher fee settings.");
            }
            // Additional warning for very high frequency
            if self.signal_cooldown_secs < 10 {
                eprintln!("⚠️  WARNING: HFT mode with very low cooldown ({}s) will generate many trades.", self.signal_cooldown_secs);
                eprintln!("  Each trade pays commission. Ensure fee_bps_round_trip is realistic (>= 8.0).");
            }
        }
        warn_if!(self.fee_bps_round_trip < 5.0, "fee_bps_round_trip is very low, may cause over-optimistic backtest results. Recommendation: Use at least 8.0 (0.08%) for realistic results", self.fee_bps_round_trip);

        if self.signal_cooldown_secs < 0 {
            eprintln!("ERROR: signal_cooldown_secs must be >= 0, got: {}", self.signal_cooldown_secs);
            process::exit(1);
        }

        self
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
            atr_sl_multiplier: self.atr_sl_multiplier,
            atr_tp_multiplier: self.atr_tp_multiplier,
            obi_long_min: self.obi_long_min,
            obi_short_max: self.obi_short_max,
            funding_max_for_long: self.funding_max_for_long,
            funding_min_for_short: self.funding_min_for_short,
            long_min_score: self.long_min_score,
            short_min_score: self.short_min_score,
            signal_cooldown_secs: self.signal_cooldown_secs,
            warmup_min_ticks: self.warmup_min_ticks,
            // ✅ ADIM 2: Config.yaml parametreleri
            hft_mode: self.hft_mode,
            base_min_score: self.base_min_score,
            trend_threshold_hft: self.trend_threshold_hft,
            trend_threshold_normal: self.trend_threshold_normal,
            weak_trend_score_multiplier: self.weak_trend_score_multiplier,
            regime_multiplier_trending: self.regime_multiplier_trending,
            regime_multiplier_ranging: self.regime_multiplier_ranging,
            // Enhanced Signal Scoring (TrendPlan.md)
            enable_enhanced_scoring: self.enable_enhanced_scoring,
            enhanced_score_excellent: self.enhanced_score_excellent,
            enhanced_score_good: self.enhanced_score_good,
            enhanced_score_marginal: self.enhanced_score_marginal,
            // Order Flow Analysis (TrendPlan.md - Action Plan)
            enable_order_flow: self.enable_order_flow,
            // Execution & Backtest Parameters (no hardcoded values)
            fee_bps_round_trip: self.fee_bps_round_trip,
            max_holding_bars: self.max_holding_bars,
            slippage_bps: self.slippage_bps,
            min_holding_bars: self.min_holding_bars,
            // Signal Quality Filtering (configurable)
            min_volume_ratio: self.min_volume_ratio,
            max_volatility_pct: self.max_volatility_pct,
            max_price_change_5bars_pct: self.max_price_change_5bars_pct,
            enable_signal_quality_filter: self.enable_signal_quality_filter,
            // WebSocket Interruption Tolerance (TrendPlan.md - Critical Warnings)
            market_tick_stale_tolerance_secs: self.market_tick_stale_tolerance_secs,
        }
    }
}

impl FileConfig {
    pub fn load(path: &str) -> Option<Self> {
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
