pub mod api;
pub mod core;

pub use api::{
    DepthState, LiqState, LiqEntry, ForceOrderRecord, DepthSnapshot, OpenInterestHistPoint,
    KlineData, KlineEvent, CombinedStreamEvent,
};
// Connection types
use chrono::{DateTime, Utc};
use reqwest::{Client, Url};
use std::sync::Arc;
use tokio::sync::RwLock;
use crate::config::BotConfig;

pub struct RateLimiter {
    order_limiter: Arc<tokio::sync::Semaphore>,
    weight_tracker: Arc<tokio::sync::RwLock<WeightTracker>>,
}

struct WeightTracker {
    current_weight: u32,
    window_start: std::time::Instant,
}

impl RateLimiter {
    pub fn new() -> Self {
        let order_limiter = Arc::new(tokio::sync::Semaphore::new(10));
        let weight_tracker = Arc::new(tokio::sync::RwLock::new(WeightTracker {
            current_weight: 0,
            window_start: std::time::Instant::now(),
        }));

        let tracker_clone = weight_tracker.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
            loop {
                interval.tick().await;
                let mut tracker = tracker_clone.write().await;
                tracker.current_weight = 0;
                tracker.window_start = std::time::Instant::now();
            }
        });

        Self {
            order_limiter,
            weight_tracker,
        }
    }

    pub async fn acquire_order_permit(&self) -> anyhow::Result<tokio::sync::SemaphorePermit<'_>> {
        use anyhow::anyhow;
        let permit = self
            .order_limiter
            .acquire()
            .await
            .map_err(|_| anyhow!("order limiter closed"))?;

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        Ok(permit)
    }

    pub async fn acquire_weight(&self, weight: u32) -> anyhow::Result<()> {
        loop {
            {
                let mut tracker = self.weight_tracker.write().await;
                if tracker.window_start.elapsed().as_secs() >= 60 {
                    tracker.current_weight = 0;
                    tracker.window_start = std::time::Instant::now();
                }

                if tracker.current_weight + weight <= 1200 {
                    tracker.current_weight += weight;
                    return Ok(());
                }
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
    }
}

#[derive(Clone)]
pub struct Connection {
    pub(crate) config: BotConfig,
    pub(crate) http: Client,
    pub(crate) rate_limiter: Arc<RateLimiter>,
    pub(crate) server_time_offset: Arc<RwLock<i64>>,
    pub(crate) last_market_tick_ts: Arc<RwLock<Option<DateTime<Utc>>>>,
    pub(crate) last_ws_connection_ts: Arc<RwLock<Option<DateTime<Utc>>>>,
}

pub struct FuturesClient {
    pub(crate) http: Client,
    pub(crate) base_url: Url,
    pub(crate) api_key: Option<String>,
    pub(crate) api_secret: Option<String>,
    pub(crate) recv_window_ms: u64,
}

pub fn handle_broadcast_recv<T>(
    result: Result<T, tokio::sync::broadcast::error::RecvError>,
) -> Result<Option<T>, ()> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => Ok(None),
        Err(tokio::sync::broadcast::error::RecvError::Closed) => Err(()),
    }
}
pub use core::{
    AlgoConfig, AdvancedBacktestResult, BacktestResult, Candle, EnhancedSignalContext,
    FundingRate, LongShortRatioPoint, NewOrderRequest, OpenInterestPoint, PositionSide,
    Signal, SignalContext, SignalSide, Side, OrderStatus, SymbolPrecision, Trade, TrendDirection,
};
// Event bus types
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;
use tokio::sync::broadcast::{Receiver as BReceiver, Sender as BSender};
use tokio::sync::mpsc::{Receiver as MReceiver, Sender as MSender};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketTick {
    pub symbol: String,
    pub price: f64,
    pub bid: f64,
    pub ask: f64,
    pub volume: f64,
    pub ts: DateTime<Utc>,
    pub obi: Option<f64>,
    pub funding_rate: Option<f64>,
    pub liq_long_cluster: Option<f64>,
    pub liq_short_cluster: Option<f64>,
    pub bid_depth_usd: Option<f64>,
    pub ask_depth_usd: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeSignal {
    pub id: Uuid,
    pub symbol: String,
    pub side: Side,
    pub entry_price: f64,
    pub leverage: f64,
    pub size_usdt: f64,
    pub ts: DateTime<Utc>,
    pub atr_value: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloseRequest {
    pub position_id: Uuid,
    pub reason: String,
    pub ts: DateTime<Utc>,
    pub partial_close_percentage: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderUpdate {
    pub order_id: Uuid,
    pub symbol: String,
    pub side: Side,
    pub status: OrderStatus,
    pub filled_qty: f64,
    pub ts: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PositionUpdate {
    pub position_id: Uuid,
    pub symbol: String,
    pub side: Side,
    pub entry_price: f64,
    pub size: f64,
    pub leverage: f64,
    pub unrealized_pnl: f64,
    pub ts: DateTime<Utc>,
    pub is_closed: bool,
}

impl PositionUpdate {
    pub fn position_id(symbol: &str, side: Side) -> Uuid {
        let timestamp = chrono::Utc::now().timestamp_millis();
        let name = format!("{}:{:?}:{}", symbol, side, timestamp);
        Uuid::new_v5(&Uuid::NAMESPACE_OID, name.as_bytes())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BalanceSnapshot {
    pub asset: String,
    pub free: f64,
    pub ts: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolRotationEvent {
    pub new_symbols: Vec<String>,
    pub removed_symbols: Vec<String>,
    pub all_symbols: Vec<String>,
    pub ts: DateTime<Utc>,
}

pub struct EventBus {
    pub(crate) market_tx: broadcast::Sender<MarketTick>,
    pub(crate) order_update_tx: broadcast::Sender<OrderUpdate>,
    pub(crate) position_update_tx: broadcast::Sender<PositionUpdate>,
    pub(crate) balance_tx: broadcast::Sender<BalanceSnapshot>,
    pub(crate) signal_tx: mpsc::Sender<TradeSignal>,
    pub(crate) signal_rx: Mutex<Option<mpsc::Receiver<TradeSignal>>>,
    pub(crate) close_tx: mpsc::Sender<CloseRequest>,
    pub(crate) close_rx: Mutex<Option<mpsc::Receiver<CloseRequest>>>,
    pub(crate) rotation_tx: broadcast::Sender<SymbolRotationEvent>,
}

impl Clone for EventBus {
    fn clone(&self) -> Self {
        Self {
            market_tx: self.market_tx.clone(),
            order_update_tx: self.order_update_tx.clone(),
            position_update_tx: self.position_update_tx.clone(),
            balance_tx: self.balance_tx.clone(),
            signal_tx: self.signal_tx.clone(),
            signal_rx: Mutex::new(None),
            close_tx: self.close_tx.clone(),
            close_rx: Mutex::new(None),
            rotation_tx: self.rotation_tx.clone(),
        }
    }
}

pub struct TrendingChannels {
    pub market_rx: BReceiver<MarketTick>,
    pub signal_tx: MSender<TradeSignal>,
}

#[derive(Debug)]
pub struct OrderingChannels {
    pub signal_rx: MReceiver<TradeSignal>,
    pub close_rx: MReceiver<CloseRequest>,
    pub order_update_rx: BReceiver<OrderUpdate>,
    pub position_update_rx: BReceiver<PositionUpdate>,
}

#[derive(Debug)]
pub struct FollowChannels {
    pub market_rx: BReceiver<MarketTick>,
    pub position_update_rx: BReceiver<PositionUpdate>,
    pub close_tx: MSender<CloseRequest>,
}

#[derive(Clone)]
pub struct BalanceChannels {
    pub balance_tx: BSender<BalanceSnapshot>,
}

#[derive(Debug)]
pub struct LoggingChannels {
    pub market_rx: BReceiver<MarketTick>,
    pub order_update_rx: BReceiver<OrderUpdate>,
    pub position_update_rx: BReceiver<PositionUpdate>,
    pub balance_rx: BReceiver<BalanceSnapshot>,
    pub signal_rx: MReceiver<TradeSignal>,
}

#[derive(Clone)]
pub struct ConnectionChannels {
    pub market_tx: BSender<MarketTick>,
    pub order_update_tx: BSender<OrderUpdate>,
    pub position_update_tx: BSender<PositionUpdate>,
    pub balance_tx: BSender<BalanceSnapshot>,
}

#[derive(Debug)]
pub struct RotationChannels {
    pub rotation_rx: BReceiver<SymbolRotationEvent>,
}
// State types
use std::collections::{HashMap, HashSet};

#[derive(Debug, Default, Clone)]
pub struct BalanceStore {
    pub usdt: f64,
    pub usdc: f64,
    pub last_usdt_update: Option<DateTime<Utc>>,
    pub last_usdc_update: Option<DateTime<Utc>>,
}

#[derive(Debug, Default, Clone)]
pub struct OrderState {
    pub open_orders: HashSet<String>,
    pub last_orders: HashMap<String, OrderUpdate>,
    pub order_sent_at: HashMap<String, DateTime<Utc>>,
}

#[derive(Debug, Default, Clone)]
pub struct PositionState {
    pub active_positions: HashMap<String, PositionUpdate>,
    pub pending_meta: HashMap<String, PositionMeta>,
    pub active_meta: HashMap<String, PositionMeta>,
}

#[derive(Debug, Default, Clone)]
pub struct EquityState {
    pub peak_equity: f64,
    pub current_equity: f64,
    pub daily_peak: f64,
    pub weekly_peak: f64,
    pub daily_reset_time: DateTime<Utc>,
    pub weekly_reset_time: DateTime<Utc>,
}

#[derive(Clone)]
pub struct SharedState {
    pub balance: Arc<Mutex<BalanceStore>>,
    pub order_state: Arc<Mutex<OrderState>>,
    pub position_state: Arc<Mutex<PositionState>>,
    pub equity_state: Arc<Mutex<EquityState>>,
}

#[derive(Debug, Default, Clone)]
pub struct PositionMeta {
    pub atr_at_entry: Option<f64>,
}

// EventBus implementation
impl EventBus {
    pub fn new(
        market_tick_buffer: usize,
        trade_signal_buffer: usize,
        close_request_buffer: usize,
        order_update_buffer: usize,
        position_update_buffer: usize,
        balance_update_buffer: usize,
    ) -> Self {
        let (market_tx, _) = broadcast::channel(market_tick_buffer);
        let (order_update_tx, _) = broadcast::channel(order_update_buffer);
        let (position_update_tx, _) = broadcast::channel(position_update_buffer);
        let (balance_tx, _) = broadcast::channel(balance_update_buffer);
        let (signal_tx, signal_rx) = mpsc::channel(trade_signal_buffer);
        let (close_tx, close_rx) = mpsc::channel(close_request_buffer);
        let (rotation_tx, _) = broadcast::channel(10);

        Self {
            market_tx,
            order_update_tx,
            position_update_tx,
            balance_tx,
            signal_tx,
            signal_rx: Mutex::new(Some(signal_rx)),
            close_tx,
            close_rx: Mutex::new(Some(close_rx)),
            rotation_tx,
        }
    }

    pub fn trending_channels(&self) -> TrendingChannels {
        TrendingChannels {
            market_rx: self.market_tx.subscribe(),
            signal_tx: self.signal_tx.clone(),
        }
    }

    pub fn ordering_channels(&self) -> OrderingChannels {
        OrderingChannels {
            signal_rx: Self::take_receiver(&self.signal_rx, "TradeSignal"),
            close_rx: Self::take_receiver(&self.close_rx, "CloseRequest"),
            order_update_rx: self.order_update_tx.subscribe(),
            position_update_rx: self.position_update_tx.subscribe(),
        }
    }

    pub fn follow_channels(&self) -> FollowChannels {
        FollowChannels {
            market_rx: self.market_tx.subscribe(),
            position_update_rx: self.position_update_tx.subscribe(),
            close_tx: self.close_tx.clone(),
        }
    }

    pub fn balance_channels(&self) -> BalanceChannels {
        BalanceChannels {
            balance_tx: self.balance_tx.clone(),
        }
    }

    pub fn logging_channels(&self) -> LoggingChannels {
        LoggingChannels {
            market_rx: self.market_tx.subscribe(),
            order_update_rx: self.order_update_tx.subscribe(),
            position_update_rx: self.position_update_tx.subscribe(),
            balance_rx: self.balance_tx.subscribe(),
            signal_rx: Self::take_receiver(&self.signal_rx, "TradeSignal"),
        }
    }

    pub fn market_receiver(&self) -> BReceiver<MarketTick> {
        self.market_tx.subscribe()
    }

    pub fn connection_channels(&self) -> ConnectionChannels {
        ConnectionChannels {
            market_tx: self.market_tx.clone(),
            order_update_tx: self.order_update_tx.clone(),
            position_update_tx: self.position_update_tx.clone(),
            balance_tx: self.balance_tx.clone(),
        }
    }

    pub fn rotation_channels(&self) -> RotationChannels {
        RotationChannels {
            rotation_rx: self.rotation_tx.subscribe(),
        }
    }

    pub fn rotation_sender(&self) -> broadcast::Sender<SymbolRotationEvent> {
        self.rotation_tx.clone()
    }

    fn take_receiver<T>(slot: &Mutex<Option<mpsc::Receiver<T>>>, name: &str) -> mpsc::Receiver<T> {
        slot.lock()
            .expect("receiver mutex poisoned")
            .take()
            .unwrap_or_else(|| panic!("{name} receiver already taken"))
    }
}

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


