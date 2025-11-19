use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use chrono::{DateTime, Utc};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;

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
pub(crate) struct FileConfig {
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
    pub trending: Option<FileTrending>,
    #[serde(default)]
    pub binance: Option<FileBinance>,
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
    pub warmup_min_ticks: Option<usize>,
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
}


#[derive(Clone)]
pub struct Connection {
    pub(crate) config: crate::BotConfig,
    pub(crate) http: Client,
    pub(crate) rate_limiter: Arc<tokio::sync::Semaphore>,
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



#[derive(Default)]
pub(crate) struct LiqState {
    pub entries: VecDeque<LiqEntry>,
    pub long_sum: f64,
    pub short_sum: f64,
    pub open_interest: f64,
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
}

#[derive(Debug, Default, Clone)]
pub struct OrderState {
    pub has_open_order: bool,
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

    // Risk & maliyet
    pub fee_bps_round_trip: f64,   // round-trip toplam taker fee bps (örn: 8 = 0.08%)
    pub max_holding_bars: usize,   // maksimum bar sayısı (örn: 48 => 4 saat @5m)
}

// =======================
//  Binance Futures Client
// =======================

pub struct FuturesClient {
    pub(crate) http: Client,
    pub(crate) base_url: Url,
}









