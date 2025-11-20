use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use chrono::{DateTime, Utc};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, RwLock};
use uuid::Uuid;
use anyhow::Result;

use tokio::sync::broadcast::{Receiver as BReceiver, Sender as BSender};
use tokio::sync::mpsc::{Receiver as MReceiver, Sender as MSender};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum Side {
    Long,
    Short,
}

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloseRequest {
    pub position_id: Uuid,
    pub reason: String,
    pub ts: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OrderStatus {
    New,
    PartiallyFilled,
    Filled,
    Canceled,
    Rejected,
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
    /// Generate a unique position ID from symbol, side, and timestamp
    /// Uses UUID v5 (SHA-1 based) with timestamp to ensure each position gets a unique ID
    /// 
    /// # Why timestamp?
    /// Without timestamp, the same symbol+side would always produce the same ID.
    /// This causes ID collision when:
    /// 1. A position closes
    /// 2. A new position opens immediately in the same direction
    /// 
    /// Adding timestamp ensures each position gets a unique ID while still being
    /// deterministic (useful for matching across WebSocket events and API calls).
    /// 
    /// # Note
    /// If two positions open in the same millisecond, they will have the same ID.
    /// This is extremely rare but possible. For production, consider using UUID v4 (random)
    /// if absolute uniqueness is required.
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
}

// fallback for warmup default referencing EMA slow period
#[derive(Debug, Default, Deserialize)]
pub struct FileConfig {
    #[serde(default)]
    pub symbols: Vec<String>,
    #[serde(default)]
    pub quote_asset: Option<String>,
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
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct FileRisk {
    #[serde(default)]
    pub use_isolated_margin: Option<bool>,
    #[serde(default)]
    pub max_position_notional_usd: Option<f64>,
    #[serde(default)]
    pub liq_window_secs: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
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

#[derive(Debug, Default, Deserialize)]
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
}

#[derive(Debug, Default, Deserialize)]
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
}


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
    pub use_isolated_margin: bool,
    pub min_margin_usd: f64,
    pub max_position_notional_usd: f64,
    pub min_quote_balance_usd: f64,
}


/// Rate limiter for Binance Futures API
/// - Order limiter: 10 orders/second
/// - Weight limiter: 1200 weight/minute
pub(crate) struct RateLimiter {
    order_limiter: Arc<tokio::sync::Semaphore>,
    weight_tracker: Arc<tokio::sync::RwLock<WeightTracker>>,
}

struct WeightTracker {
    current_weight: u32,
    window_start: std::time::Instant,
}

impl RateLimiter {
    pub fn new() -> Self {
        // Order limiter: 10 permits for 10 orders/second
        let order_limiter = Arc::new(tokio::sync::Semaphore::new(10));
        
        let weight_tracker = Arc::new(tokio::sync::RwLock::new(WeightTracker {
            current_weight: 0,
            window_start: std::time::Instant::now(),
        }));
        
        // Start weight reset task
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
    
    /// Acquire permit for order (10 orders/second limit)
    /// 
    /// CRITICAL: The permit MUST be held for the entire request duration to prevent rate limit bypass.
    /// The 100ms delay happens WHILE holding the permit (before returning it).
    /// Caller must keep the returned permit until the request completes.
    /// 
    /// Rate limit bypass prevention:
    /// - Sleep happens BEFORE returning permit (while permit is held)
    /// - Caller must hold permit during entire request
    /// - Permit is automatically released when dropped (at end of scope)
    pub async fn acquire_order_permit(&self) -> anyhow::Result<tokio::sync::SemaphorePermit<'_>> {
        use anyhow::anyhow;
        // Acquire permit first (this blocks if rate limit is exceeded)
        let permit = self.order_limiter.acquire().await
            .map_err(|_| anyhow!("order limiter closed"))?;
        
        // CRITICAL: Sleep while holding the permit to space out orders (10/sec = 100ms between)
        // This ensures the rate limit delay happens while the permit is held, preventing bypass
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        
        // Return permit - caller must hold it for entire request duration
        Ok(permit)
    }
    
    /// Acquire weight (1200 weight/minute limit)
    pub async fn acquire_weight(&self, weight: u32) -> anyhow::Result<()> {
        use anyhow::anyhow;
        
        loop {
            // Check if we need to reset weight window
            {
                let mut tracker = self.weight_tracker.write().await;
                if tracker.window_start.elapsed().as_secs() >= 60 {
                    tracker.current_weight = 0;
                    tracker.window_start = std::time::Instant::now();
                }
                
                // Check if we have enough weight available
                if tracker.current_weight + weight <= 1200 {
                    tracker.current_weight += weight;
                    return Ok(());
                }
            }
            
            // Wait a bit before retrying
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
    }
}

#[derive(Clone)]
pub struct Connection {
    pub(crate) config: crate::BotConfig,
    pub(crate) http: Client,
    pub(crate) rate_limiter: Arc<RateLimiter>,
    /// Server time offset in milliseconds (server_time - client_time)
    /// Used to sync with Binance server time for accurate timestamp generation
    /// Positive value means server is ahead of client
    pub(crate) server_time_offset: Arc<RwLock<i64>>,
}

#[derive(Debug)]
pub struct NewOrderRequest {
    pub symbol: String,
    pub side: Side,
    pub quantity: f64,
    pub reduce_only: bool,
    pub client_order_id: Option<String>,
}




#[derive(Debug, Deserialize)]
pub(crate) struct MarkPriceEvent {
    #[serde(rename = "s")]
    pub symbol: String,
    #[serde(rename = "p")]
    pub mark_price: String,
    #[serde(rename = "r")]
    pub funding_rate: Option<String>,
    #[serde(rename = "E")]
    pub event_time: u64,
}

#[derive(Debug, Deserialize)]
pub(crate) struct DepthEvent {
    #[serde(rename = "b")]
    pub bids: Vec<[String; 2]>,
    #[serde(rename = "a")]
    pub asks: Vec<[String; 2]>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ForceOrderRecord {
    #[serde(rename = "side")]
    pub side: String,
    #[serde(rename = "avgPrice")]
    pub avg_price: Option<String>,
    #[serde(rename = "price")]
    pub price: Option<String>,
    #[serde(rename = "executedQty")]
    pub executed_qty: Option<String>,
    #[serde(rename = "origQty")]
    pub orig_qty: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ForceOrderStreamWrapper {
    pub data: ForceOrderStreamEvent,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ForceOrderStreamEvent {
    #[serde(rename = "o")]
    pub order: ForceOrderStreamOrder,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ForceOrderStreamOrder {
    #[serde(rename = "s")]
    pub symbol: String,
    #[serde(rename = "S")]
    pub side: String,
    #[serde(rename = "p")]
    pub price: Option<String>,
    #[serde(rename = "ap")]
    pub avg_price: Option<String>,
    #[serde(rename = "q")]
    pub orig_qty: Option<String>,
    #[serde(rename = "l")]
    pub last_filled: Option<String>,
    #[serde(rename = "z")]
    pub executed_qty: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenInterestResponse {
    #[serde(rename = "openInterest")]
    pub open_interest: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenInterestEvent {
    #[serde(rename = "e")]
    pub event_type: String,
    #[serde(rename = "E")]
    pub event_time: u64,
    #[serde(rename = "s")]
    pub symbol: String,
    #[serde(rename = "o")]
    pub open_interest: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct DepthSnapshot {
    pub bids: Vec<[String; 2]>,
    pub asks: Vec<[String; 2]>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PremiumIndex {
    #[serde(rename = "symbol")]
    pub symbol: String,
    #[serde(rename = "markPrice")]
    pub mark_price_str: String,
    #[serde(rename = "lastFundingRate")]
    pub last_funding_rate: Option<String>,
    #[serde(rename = "time")]
    pub event_time_ms: u64,
}



pub(crate) struct LiqState {
    pub entries: VecDeque<LiqEntry>,
    pub long_sum: f64,
    pub short_sum: f64,
    pub open_interest: f64,
    pub window_secs: u64, // Aggregation window in seconds
}

pub(crate) struct LiqEntry {
    pub ts: Instant,
    pub ratio: f64,
    pub is_long_cluster: bool,
}


#[derive(Debug, Deserialize)]
pub(crate) struct ListenKeyResponse {
    #[serde(rename = "listenKey")]
    pub listen_key: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct FuturesBalance {
    #[serde(rename = "asset")]
    pub asset: String,
    #[serde(rename = "availableBalance")]
    pub available_balance: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OrderTradeUpdate {
    #[serde(rename = "o")]
    pub order: OrderPayload,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OrderPayload {
    #[serde(rename = "s")]
    pub symbol: String,
    #[serde(rename = "S")]
    pub side: String,
    #[serde(rename = "X")]
    pub status: String,
    #[serde(rename = "z")]
    pub filled_qty: String,
    #[serde(rename = "E")]
    pub event_time: u64,
}




#[derive(Debug, Deserialize)]
pub(crate) struct AccountUpdate {
    #[serde(rename = "a")]
    pub account: AccountPayload,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AccountPayload {
    #[serde(rename = "B")]
    pub balances: Vec<AccountBalance>,
    #[serde(rename = "P")]
    pub positions: Vec<AccountPosition>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AccountBalance {
    #[serde(rename = "a")]
    pub asset: String,
    #[serde(rename = "wb")]
    pub wallet_balance: String,
}




#[derive(Debug, Deserialize)]
pub(crate) struct AccountPosition {
    #[serde(rename = "s")]
    pub symbol: String,
    #[serde(rename = "pa")]
    pub position_amount: String,
    #[serde(rename = "ep")]
    pub entry_price: String,
    #[serde(rename = "cr")]
    pub unrealized_pnl: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PositionRiskResponse {
    #[serde(rename = "symbol")]
    pub symbol: String,
    #[serde(rename = "positionAmt")]
    pub position_amount: String,
    #[serde(rename = "entryPrice")]
    pub entry_price: String,
    #[serde(rename = "unRealizedProfit")]
    pub unrealized_profit: String,
    #[serde(rename = "leverage")]
    pub leverage: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct LeverageBracketResponse {
    #[serde(rename = "symbol")]
    pub symbol: String,
    #[serde(rename = "brackets")]
    pub brackets: Vec<LeverageBracket>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct LeverageBracket {
    #[serde(rename = "bracket")]
    pub bracket: u8,
    #[serde(rename = "initialLeverage")]
    pub initial_leverage: u8,
    #[serde(rename = "notionalCap")]
    pub notional_cap: String,
    #[serde(rename = "notionalFloor")]
    pub notional_floor: String,
    #[serde(rename = "maintMarginRatio")]
    pub maint_margin_ratio: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ExchangeInfoResponse {
    #[serde(rename = "symbols")]
    pub symbols: Vec<SymbolInfo>,
}

#[derive(Debug, Deserialize, Clone)]
pub(crate) struct SymbolInfo {
    #[serde(rename = "symbol")]
    pub symbol: String,
    #[serde(rename = "filters")]
    pub filters: Vec<Filter>,
}

#[derive(Debug, Clone)]
pub(crate) struct Filter {
    pub filter_type: String,
    pub data: serde_json::Value,
}

impl<'de> serde::Deserialize<'de> for Filter {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        let filter_type = value
            .get("filterType")
            .and_then(|v| v.as_str())
            .unwrap_or("UNKNOWN")
            .to_string();
        Ok(Filter {
            filter_type,
            data: value,
        })
    }
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

pub struct EventBus {
    pub(crate) market_tx: broadcast::Sender<MarketTick>,
    pub(crate) order_update_tx: broadcast::Sender<OrderUpdate>,
    pub(crate) position_update_tx: broadcast::Sender<PositionUpdate>,
    pub(crate) balance_tx: broadcast::Sender<BalanceSnapshot>,
    pub(crate) signal_tx: mpsc::Sender<TradeSignal>,
    pub(crate) signal_rx: Mutex<Option<mpsc::Receiver<TradeSignal>>>,
    pub(crate) close_tx: mpsc::Sender<CloseRequest>,
    pub(crate) close_rx: Mutex<Option<mpsc::Receiver<CloseRequest>>>,
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




#[derive(Debug, Default, Clone)]
pub struct BalanceStore {
    pub usdt: f64,
    pub usdc: f64,
    pub last_usdt_update: Option<chrono::DateTime<chrono::Utc>>,
    pub last_usdc_update: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Default, Clone)]
pub struct OrderState {
    pub has_open_order: bool,
    pub last_order: Option<OrderUpdate>,
    pub order_sent_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Default, Clone)]
pub struct PositionState {
    pub has_open_position: bool,
    pub last_position: Option<PositionUpdate>,
}

#[derive(Clone)]
pub struct SharedState {
    pub balance: Arc<Mutex<BalanceStore>>,
    pub order_state: Arc<Mutex<OrderState>>,
    pub position_state: Arc<Mutex<PositionState>>,
}


// =======================
//  Binance Futures Modelleri
// =======================

// KlineRaw removed - using serde_json::Value for flexible parsing

#[derive(Debug, Clone)]
pub struct Candle {
    pub open_time: DateTime<Utc>,
    pub close_time: DateTime<Utc>,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

#[derive(Debug, Deserialize)]
pub(crate) struct KlineEvent {
    #[serde(rename = "e")]
    pub event_type: String,
    #[serde(rename = "E")]
    pub event_time: u64,
    #[serde(rename = "s")]
    pub symbol: String,
    #[serde(rename = "k")]
    pub kline: KlineData,
}

#[derive(Debug, Deserialize)]
pub(crate) struct KlineData {
    #[serde(rename = "t")]
    pub open_time: i64,
    #[serde(rename = "T")]
    pub close_time: i64,
    #[serde(rename = "s")]
    pub symbol: String,
    #[serde(rename = "i")]
    pub interval: String,
    #[serde(rename = "o")]
    pub open: String,
    #[serde(rename = "c")]
    pub close: String,
    #[serde(rename = "h")]
    pub high: String,
    #[serde(rename = "l")]
    pub low: String,
    #[serde(rename = "v")]
    pub volume: String,
    #[serde(rename = "x")]
    pub is_closed: bool,
}

#[derive(Debug)]
pub(crate) struct FundingRate {
    pub _symbol: String,
    pub funding_rate: String,
    pub funding_time: i64,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenInterestHistPoint {
    #[serde(rename = "symbol")]
    pub _symbol: String,
    #[serde(rename = "sumOpenInterest")]
    pub sum_open_interest: String,
    #[serde(rename = "sumOpenInterestValue")]
    pub _sum_open_interest_value: String,
    pub timestamp: i64,
}

#[derive(Debug)]
pub struct OpenInterestPoint {
    pub timestamp: DateTime<Utc>,
    pub open_interest: f64,
}

// TopLongShortAccountRatioRaw removed - using serde_json::Value for flexible parsing

#[derive(Debug)]
pub struct LongShortRatioPoint {
    pub timestamp: DateTime<Utc>,
    pub long_short_ratio: f64,
    pub long_account_pct: f64,
    pub short_account_pct: f64,
}

// =======================
//  Sinyal Modeli
// =======================

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
    pub ema_fast: f64,         // EMA 21
    pub ema_slow: f64,         // EMA 55
    pub rsi: f64,              // RSI 14
    pub atr: f64,              // ATR 14
    pub funding_rate: f64,     // en yakın funding
    pub open_interest: f64,    // en yakın OI
    pub long_short_ratio: f64, // top trader long/short account ratio
}

#[derive(Debug, Clone)]
pub struct Signal {
    pub time: DateTime<Utc>,
    pub price: f64,
    pub side: SignalSide,
    pub ctx: SignalContext,
}

// =======================
//  Backtest Trade Model
// =======================

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
    pub pnl_pct: f64, // % olarak (0.01 => +1%)
    pub win: bool,
}

#[derive(Debug)]
pub struct BacktestResult {
    pub trades: Vec<Trade>,
    pub total_trades: usize,
    pub win_trades: usize,
    pub loss_trades: usize,
    pub win_rate: f64,
    pub total_pnl_pct: f64,
    pub avg_pnl_pct: f64,
    pub avg_r: f64, // Average Risk/Reward ratio (average win / average loss)
}

// =======================
//  Parametrik Config
// =======================

#[derive(Debug, Clone)]
pub struct AlgoConfig {
    // Sinyal eşiği
    pub rsi_trend_long_min: f64,   // trend yukarıdaysa min RSI (örn: 55)
    pub rsi_trend_short_max: f64,  // trend aşağıdaysa max RSI (örn: 45)
    pub funding_extreme_pos: f64,  // aşırı long seviye (örn: +0.01 = 0.01%)
    pub funding_extreme_neg: f64,  // aşırı short seviye (örn: -0.01)
    pub lsr_crowded_long: f64,     // longShortRatio > X => crowded long
    pub lsr_crowded_short: f64,    // longShortRatio < X => crowded short
    pub long_min_score: usize,     // LONG sinyal için minimum score (örn: 4)
    pub short_min_score: usize,    // SHORT sinyal için minimum score (örn: 4)

    // Risk & maliyet
    pub fee_bps_round_trip: f64,   // round-trip toplam taker fee bps (örn: 8 = 0.08%)
    pub max_holding_bars: usize,   // maksimum bar sayısı (örn: 48 => 4 saat @5m)
    
    // Execution simulation (backtest için)
    pub slippage_bps: f64,         // slippage in basis points (örn: 5 = 0.05%)
                                   // Production'da gerçek slippage var, backtest'te simüle etmek için
                                   // Default: 0 (optimistic backtest)
}

// =======================
//  Binance Futures Client
// =======================

pub struct FuturesClient {
    pub(crate) http: Client,
    pub(crate) base_url: Url,
}









