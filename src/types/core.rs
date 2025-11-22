use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum Side {
    Long,
    Short,
}

impl Side {
    pub fn to_binance_str(&self) -> &'static str {
        match self {
            Side::Long => "BUY",
            Side::Short => "SELL",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OrderStatus {
    New,
    PartiallyFilled,
    Filled,
    Canceled,
    Rejected,
}

#[derive(Debug, Clone)]
pub struct NewOrderRequest {
    pub symbol: String,
    pub side: Side,
    pub quantity: f64,
    pub reduce_only: bool,
    pub client_order_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SymbolPrecision {
    pub step_size: f64,
    pub min_quantity: f64,
    pub max_quantity: f64,
    pub tick_size: f64,
    pub min_price: f64,
    pub max_price: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candle {
    pub open_time: DateTime<Utc>,
    pub close_time: DateTime<Utc>,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

#[derive(Debug, Clone)]
pub struct FundingRate {
    pub _symbol: String,
    pub funding_rate: String,
    pub funding_time: i64,
}

#[derive(Debug)]
pub struct OpenInterestPoint {
    pub timestamp: DateTime<Utc>,
    pub open_interest: f64,
}

#[derive(Debug)]
pub struct LongShortRatioPoint {
    pub timestamp: DateTime<Utc>,
    pub long_short_ratio: f64,
    pub long_account_pct: f64,
    pub short_account_pct: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrendDirection {
    Up,
    Down,
    Flat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalSide {
    Long,
    Short,
    Flat,
}

#[derive(Debug, Clone)]
pub struct SignalContext {
    pub ema_fast: f64,
    pub ema_slow: f64,
    pub rsi: f64,
    pub atr: f64,
    pub funding_rate: f64,
    pub open_interest: f64,
    pub long_short_ratio: f64,
}

#[derive(Debug, Clone)]
pub struct EnhancedSignalContext {
    pub ema_fast: f64,
    pub ema_slow: f64,
    pub rsi: f64,
    pub atr: f64,
    pub bid_ask_spread_bps: f64,
    pub orderbook_imbalance: f64,
    pub top_5_bid_depth_usd: f64,
    pub top_5_ask_depth_usd: f64,
    pub volume_ma_20: f64,
    pub volume_ratio: f64,
    pub buy_volume_ratio: f64,
    pub macd: f64,
    pub macd_signal: f64,
    pub stochastic_k: f64,
    pub stochastic_d: f64,
    pub atr_percentile: f64,
    pub bollinger_width: f64,
    pub price_vs_bb_upper: f64,
    pub price_vs_bb_lower: f64,
    pub funding_rate: f64,
    pub open_interest: f64,
    pub long_short_ratio: f64,
    pub trend_1m: TrendDirection,
    pub trend_5m: TrendDirection,
    pub trend_15m: TrendDirection,
    pub trend_1h: TrendDirection,
    pub nearest_support_distance: f64,
    pub nearest_resistance_distance: f64,
    pub support_strength: f64,
    pub resistance_strength: f64,
    pub has_real_orderbook_data: bool,
    pub has_real_volume_data: bool,
}

#[derive(Debug, Clone)]
pub struct Signal {
    pub time: DateTime<Utc>,
    pub price: f64,
    pub side: SignalSide,
    pub ctx: SignalContext,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionSide {
    Long,
    Short,
    Flat,
}

#[derive(Debug, Clone)]
pub struct Trade {
    pub entry_time: DateTime<Utc>,
    pub exit_time: DateTime<Utc>,
    pub side: PositionSide,
    pub entry_price: f64,
    pub exit_price: f64,
    pub pnl_pct: f64,
    pub win: bool,
}

#[derive(Debug, Clone)]
pub struct BacktestResult {
    pub trades: Vec<Trade>,
    pub total_trades: usize,
    pub win_trades: usize,
    pub loss_trades: usize,
    pub win_rate: f64,
    pub total_pnl_pct: f64,
    pub avg_pnl_pct: f64,
    pub avg_r: f64,
    pub total_signals: usize,
    pub long_signals: usize,
    pub short_signals: usize,
}

#[derive(Debug)]
pub struct AdvancedBacktestResult {
    pub trades: Vec<Trade>,
    pub total_trades: usize,
    pub win_trades: usize,
    pub loss_trades: usize,
    pub win_rate: f64,
    pub total_pnl_pct: f64,
    pub avg_pnl_pct: f64,
    pub avg_r: f64,
    pub max_drawdown_pct: f64,
    pub max_consecutive_losses: usize,
    pub sharpe_ratio: f64,
    pub sortino_ratio: f64,
    pub profit_factor: f64,
    pub recovery_factor: f64,
    pub avg_trade_duration_hours: f64,
    pub kelly_criterion: f64,
    pub best_hour_of_day: Option<u32>,
    pub worst_hour_of_day: Option<u32>,
    pub longest_drawdown_duration_hours: f64,
    pub current_drawdown_pct: f64,
}

#[derive(Debug, Clone)]
pub struct AlgoConfig {
    pub rsi_trend_long_min: f64,
    pub rsi_trend_short_max: f64,
    pub funding_extreme_pos: f64,
    pub funding_extreme_neg: f64,
    pub lsr_crowded_long: f64,
    pub lsr_crowded_short: f64,
    pub long_min_score: usize,
    pub short_min_score: usize,
    pub fee_bps_round_trip: f64,
    pub max_holding_bars: usize,
    pub slippage_bps: f64,
    pub min_volume_ratio: f64,
    pub max_volatility_pct: f64,
    pub max_price_change_5bars_pct: f64,
    pub enable_signal_quality_filter: bool,
    pub enable_enhanced_scoring: bool,
    pub enhanced_score_excellent: f64,
    pub enhanced_score_good: f64,
    pub enhanced_score_marginal: f64,
    pub atr_stop_loss_multiplier: f64,
    pub atr_take_profit_multiplier: f64,
    pub min_holding_bars: usize,
    pub hft_mode: bool,
    pub base_min_score: f64,
    pub trend_threshold_hft: f64,
    pub trend_threshold_normal: f64,
    pub weak_trend_score_multiplier: f64,
    pub regime_multiplier_trending: f64,
    pub regime_multiplier_ranging: f64,
    pub enable_order_flow: bool,
}
