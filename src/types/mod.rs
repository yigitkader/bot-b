pub mod api;
pub mod connection;
pub mod core;
pub mod events;
pub mod state;

pub use api::{
    DepthState, LiqState, LiqEntry, ForceOrderRecord, DepthSnapshot, OpenInterestHistPoint,
    KlineData, KlineEvent, CombinedStreamEvent,
};
pub use connection::{Connection, FuturesClient, RateLimiter, handle_broadcast_recv};
pub use core::{
    AlgoConfig, AdvancedBacktestResult, BacktestResult, Candle, EnhancedSignalContext,
    FundingRate, LongShortRatioPoint, NewOrderRequest, OpenInterestPoint, PositionSide,
    Signal, SignalContext, SignalSide, Side, OrderStatus, SymbolPrecision, Trade, TrendDirection,
};
pub use events::{
    BalanceChannels, BalanceSnapshot, CloseRequest, ConnectionChannels, EventBus,
    FollowChannels, LoggingChannels, MarketTick, OrderingChannels, OrderUpdate,
    PositionUpdate, RotationChannels, SymbolRotationEvent, TradeSignal, TrendingChannels,
};
pub use state::{
    BalanceStore, EquityState, OrderState, PositionMeta, PositionState, SharedState,
};

use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct TrendParams {
    pub ema_fast_period: usize,
    pub ema_slow_period: usize,
    pub rsi_period: usize,
    pub atr_period: usize,
    pub leverage: f64,
    pub position_size_quote: f64,
    pub rsi_long_min: f64,
    pub rsi_short_max: f64,
    pub atr_rising_factor: f64,
    pub atr_sl_multiplier: f64,
    pub atr_tp_multiplier: f64,
    pub obi_long_min: f64,
    pub obi_short_max: f64,
    pub funding_max_for_long: f64,
    pub funding_min_for_short: f64,
    pub long_min_score: usize,
    pub short_min_score: usize,
    pub signal_cooldown_secs: i64,
    pub warmup_min_ticks: usize,
    pub hft_mode: bool,
    pub base_min_score: f64,
    pub trend_threshold_hft: f64,
    pub trend_threshold_normal: f64,
    pub weak_trend_score_multiplier: f64,
    pub regime_multiplier_trending: f64,
    pub regime_multiplier_ranging: f64,
    pub enable_enhanced_scoring: bool,
    pub enhanced_score_excellent: f64,
    pub enhanced_score_good: f64,
    pub enhanced_score_marginal: f64,
    pub enable_order_flow: bool,
    pub fee_bps_round_trip: f64,
    pub max_holding_bars: usize,
    pub slippage_bps: f64,
    pub min_holding_bars: usize,
    pub min_volume_ratio: f64,
    pub max_volatility_pct: f64,
    pub max_price_change_5bars_pct: f64,
    pub enable_signal_quality_filter: bool,
    pub market_tick_stale_tolerance_secs: i64,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct FileConfig {
    #[serde(default)]
    pub symbols: Vec<String>,
    #[serde(default)]
    pub quote_asset: Option<String>,
    #[serde(default)]
    pub auto_discover_quote: Option<bool>,
    #[serde(default)]
    pub allow_usdt_quote: Option<bool>,
    #[serde(default)]
    pub leverage: Option<f64>,
    #[serde(default)]
    pub take_profit_pct: Option<f64>,
    #[serde(default)]
    pub stop_loss_pct: Option<f64>,
    #[serde(default)]
    pub max_usd_per_order: Option<f64>,
    #[serde(default)]
    pub min_margin_usd: Option<f64>,
    #[serde(default)]
    pub min_quote_balance_usd: Option<f64>,
    #[serde(default)]
    pub trending: Option<FileTrending>,
    #[serde(default)]
    pub binance: Option<FileBinance>,
    #[serde(default)]
    pub risk: Option<FileRisk>,
    #[serde(default)]
    pub event_bus: Option<FileEventBus>,
    #[serde(default)]
    pub dynamic_symbol_selection: Option<FileDynamicSymbolSelection>,
    #[serde(default)]
    pub paper_trading: Option<FilePaperTrading>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct FilePaperTrading {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub log_file: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct FileDynamicSymbolSelection {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub max_symbols: Option<usize>,
    #[serde(default)]
    pub rotation_interval_minutes: Option<u64>,
    #[serde(default)]
    pub min_volatility_pct: Option<f64>,
    #[serde(default)]
    pub min_quote_volume: Option<f64>,
    #[serde(default)]
    pub min_trades_24h: Option<u64>,
    #[serde(default)]
    pub volatility_weight: Option<f64>,
    #[serde(default)]
    pub volume_weight: Option<f64>,
    #[serde(default)]
    pub trades_weight: Option<f64>,
    #[serde(default)]
    pub spread_weight: Option<f64>,
    #[serde(default)]
    pub min_balance_threshold: Option<f64>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct FileRisk {
    #[serde(default)]
    pub use_isolated_margin: Option<bool>,
    #[serde(default)]
    pub max_position_notional_usd: Option<f64>,
    #[serde(default)]
    pub liq_window_secs: Option<u64>,
    #[serde(default)]
    pub correlation_threshold: Option<f64>,
    #[serde(default)]
    pub max_correlated_positions: Option<usize>,
    #[serde(default)]
    pub max_correlated_notional_pct: Option<f64>,
    #[serde(default)]
    pub max_position_per_symbol_pct: Option<f64>,
    #[serde(default)]
    pub correlation_cache_update_interval_secs: Option<u64>,
    #[serde(default)]
    pub correlation_cache_retention_minutes: Option<u64>,
    #[serde(default)]
    pub max_daily_drawdown_pct: Option<f64>,
    #[serde(default)]
    pub max_weekly_drawdown_pct: Option<f64>,
    #[serde(default)]
    pub risk_per_trade_pct: Option<f64>,
    #[serde(default)]
    pub ws_disconnect_close_positions: Option<bool>,
    #[serde(default)]
    pub ws_disconnect_timeout_secs: Option<i64>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct FileBinance {
    #[serde(default)]
    pub futures_base: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub secret_key: Option<String>,
    #[serde(default)]
    pub recv_window_ms: Option<u64>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct FileTrending {
    #[serde(default)]
    pub rsi_lower_long: Option<f64>,
    #[serde(default)]
    pub rsi_upper_long: Option<f64>,
    #[serde(default)]
    pub signal_cooldown_seconds: Option<i64>,
    #[serde(default)]
    pub ema_fast_period: Option<usize>,
    #[serde(default)]
    pub ema_slow_period: Option<usize>,
    #[serde(default)]
    pub rsi_period: Option<usize>,
    #[serde(default)]
    pub atr_period: Option<usize>,
    #[serde(default)]
    pub atr_sl_multiplier: Option<f64>,
    #[serde(default)]
    pub atr_tp_multiplier: Option<f64>,
    #[serde(default)]
    pub warmup_min_ticks: Option<usize>,
    #[serde(default)]
    pub hft_mode: Option<bool>,
    #[serde(default)]
    pub base_min_score: Option<f64>,
    #[serde(default)]
    pub trend_threshold_hft: Option<f64>,
    #[serde(default)]
    pub trend_threshold_normal: Option<f64>,
    #[serde(default)]
    pub weak_trend_score_multiplier: Option<f64>,
    #[serde(default)]
    pub regime_multiplier_trending: Option<f64>,
    #[serde(default)]
    pub regime_multiplier_ranging: Option<f64>,
    #[serde(default)]
    pub enable_enhanced_scoring: Option<bool>,
    #[serde(default)]
    pub enhanced_score_excellent: Option<f64>,
    #[serde(default)]
    pub enhanced_score_good: Option<f64>,
    #[serde(default)]
    pub enhanced_score_marginal: Option<f64>,
    #[serde(default)]
    pub enable_order_flow: Option<bool>,
    #[serde(default)]
    pub market_tick_stale_tolerance_secs: Option<i64>,
    #[serde(default)]
    pub fee_bps_round_trip: Option<f64>,
    #[serde(default)]
    pub max_holding_bars: Option<usize>,
    #[serde(default)]
    pub slippage_bps: Option<f64>,
    #[serde(default)]
    pub min_holding_bars: Option<usize>,
    #[serde(default)]
    pub min_volume_ratio: Option<f64>,
    #[serde(default)]
    pub max_volatility_pct: Option<f64>,
    #[serde(default)]
    pub max_price_change_5bars_pct: Option<f64>,
    #[serde(default)]
    pub enable_signal_quality_filter: Option<bool>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct FileEventBus {
    #[serde(default)]
    pub market_tick_buffer: Option<usize>,
    #[serde(default)]
    pub trade_signal_buffer: Option<usize>,
    #[serde(default)]
    pub close_request_buffer: Option<usize>,
    #[serde(default)]
    pub order_update_buffer: Option<usize>,
    #[serde(default)]
    pub position_update_buffer: Option<usize>,
    #[serde(default)]
    pub balance_update_buffer: Option<usize>,
    #[serde(default)]
    pub metrics_cache_update_interval_secs: Option<u64>,
}


