
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Px(pub Decimal);
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Qty(pub Decimal);
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct BookLevel {
    pub px: Px,
    pub qty: Qty,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OrderBook {
    pub best_bid: Option<BookLevel>,
    pub best_ask: Option<BookLevel>,
    pub top_bids: Option<Vec<BookLevel>>,
    pub top_asks: Option<Vec<BookLevel>>,
}
impl OrderBook {
    pub fn from_prices(bid: Px, ask: Px) -> Self {
        Self {
            best_bid: Some(BookLevel {
                px: bid,
                qty: Qty(Decimal::ONE),
            }),
            best_ask: Some(BookLevel {
                px: ask,
                qty: Qty(Decimal::ONE),
            }),
            top_bids: None,
            top_asks: None,
        }
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum PositionDirection {
    Long,
    Short,
}
impl PositionDirection {
    pub fn from_order_side(side: Side) -> Self {
        match side {
            Side::Buy => PositionDirection::Long,
            Side::Sell => PositionDirection::Short,
        }
    }
    pub fn from_qty_sign(qty: Decimal) -> Self {
        if qty.is_sign_positive() {
            PositionDirection::Long
        } else {
            PositionDirection::Short
        }
    }
    pub fn direction_and_abs_qty(qty: Qty) -> (Self, Qty) {
        let direction = Self::from_qty_sign(qty.0);
        let qty_abs = if qty.0.is_sign_negative() {
            Qty(-qty.0)
        } else {
            qty
        };
        (direction, qty_abs)
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum Tif {
    Gtc,
    Ioc,
    PostOnly,
}
#[derive(Debug, Clone)]
pub enum OrderCommand {
    Open {
        symbol: String,
        side: Side,
        price: Px,
        qty: Qty,
        tif: Tif,
    },
    Close {
        symbol: String,
        side: Side,
        price: Px,
        qty: Qty,
        tif: Tif,
    },
}
#[derive(Clone, Debug)]
pub struct VenueOrder {
    pub order_id: String,
    pub side: Side,
    pub price: Px,
    pub qty: Qty,
}
#[derive(Clone, Debug)]
pub struct Position {
    pub symbol: String,
    pub qty: Qty,
    pub entry: Px,
    pub leverage: u32,
    pub liq_px: Option<Px>,
}
#[derive(Clone, Debug)]
pub struct SymbolRules {
    pub tick_size: Decimal,
    pub step_size: Decimal,
    pub price_precision: usize,
    pub qty_precision: usize,
    pub min_notional: Decimal,
    pub max_leverage: Option<u32>,
}
#[derive(Clone, Debug)]
pub struct SymbolMeta {
    pub symbol: String,
    pub base_asset: String,
    pub quote_asset: String,
    pub status: Option<String>,
    pub contract_type: Option<String>,
}
#[derive(Clone, Copy, Debug)]
pub enum UserStreamKind {
    Futures,
}
#[derive(Debug, Clone)]
pub enum UserEvent {
    OrderFill {
        symbol: String,
        order_id: String,
        side: Side,
        qty: Qty,
        cumulative_filled_qty: Qty,
        order_qty: Option<Qty>,
        price: Px,
        is_maker: bool,
        order_status: String,
        commission: Decimal,  // For future fee tracking
    },
    OrderCanceled {
        symbol: String,
        order_id: String,
        client_order_id: Option<String>,
    },
    AccountUpdate {
        positions: Vec<AccountPosition>,
        balances: Vec<AccountBalance>,
    },
    Heartbeat,
}
#[derive(Debug, Clone)]
pub struct AccountPosition {
    pub symbol: String,
    pub position_amt: Decimal,
    pub entry_price: Decimal,
    pub leverage: u32,
    pub unrealized_pnl: Option<Decimal>,
}
#[derive(Debug, Clone)]
pub struct AccountBalance {
    pub asset: String,
    pub available_balance: Decimal,
}
#[derive(Debug, Clone)]
pub struct PriceUpdate {
    pub symbol: String,
    pub bid: Px,
    pub ask: Px,
    pub bid_qty: Qty,
    pub ask_qty: Qty,
}
#[derive(Clone, Debug, Serialize)]
pub struct MarketTick {
    pub symbol: String,
    pub bid: Px,
    pub ask: Px,
    pub mark_price: Option<Px>,
    pub volume: Option<Decimal>,
    #[serde(skip)]
    pub timestamp: Instant,
}
#[derive(Clone, Debug, Serialize)]
pub struct TradeSignal {
    pub symbol: String,
    pub side: Side,
    pub entry_price: Px,
    pub stop_loss_pct: Option<f64>,
    pub take_profit_pct: Option<f64>,
    pub spread_bps: f64,
    #[serde(skip)]
    pub spread_timestamp: Instant,
    #[serde(skip)]
    pub timestamp: Instant,
}
#[derive(Clone, Debug, Serialize)]
pub struct CloseRequest {
    pub symbol: String,
    pub position_id: Option<String>,
    pub reason: CloseReason,
    pub current_bid: Option<Px>,
    pub current_ask: Option<Px>,
    #[serde(skip)]
    pub timestamp: Instant,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum CloseReason {
    TakeProfit,
    StopLoss,
    Manual,
}
#[derive(Clone, Debug, Serialize)]
pub struct OrderUpdate {
    pub symbol: String,
    pub order_id: String,
    pub side: Side,
    pub last_fill_price: Px,
    pub average_fill_price: Px,
    pub qty: Qty,
    pub filled_qty: Qty,
    pub remaining_qty: Qty,
    pub status: OrderStatus,
    pub rejection_reason: Option<String>,
    pub is_maker: Option<bool>,
    #[serde(skip)]
    pub timestamp: Instant,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum OrderStatus {
    New,
    PartiallyFilled,
    Filled,
    Canceled,
    Expired,
    ExpiredInMatch,
    Rejected,
}
#[derive(Clone, Debug, Serialize)]
pub struct PositionUpdate {
    pub symbol: String,
    pub qty: Qty,
    pub entry_price: Px,
    pub leverage: u32,
    pub unrealized_pnl: Option<Decimal>,
    pub is_open: bool,
    pub liq_px: Option<Px>,
    #[serde(skip)]
    pub timestamp: Instant,
}
#[derive(Clone, Debug, Serialize)]
pub struct BalanceUpdate {
    pub usdt: Decimal,
    pub usdc: Decimal,
    #[serde(skip)]
    pub timestamp: Instant,
}
#[derive(Clone, Debug, Serialize)]
pub struct OrderingStateUpdate {
    pub open_position: Option<OpenPositionSnapshot>,
    pub open_order: Option<OpenOrderSnapshot>,
    #[serde(skip)]
    pub timestamp: Instant,
}
#[derive(Clone, Debug, Serialize)]
pub struct OpenPositionSnapshot {
    pub symbol: String,
    pub direction: String,
    pub qty: String,
    pub entry_price: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct OpenOrderSnapshot {
    pub symbol: String,
    pub order_id: String,
    pub side: String,
    pub qty: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct OrderFillHistoryUpdate {
    pub order_id: String,
    pub symbol: String,
    pub action: FillHistoryAction,
    pub data: Option<FillHistoryData>,
    #[serde(skip)]
    pub timestamp: Instant,
}
#[derive(Clone, Debug, Serialize)]
pub enum FillHistoryAction {
    Save,
    Remove,
}
#[derive(Clone, Debug, Serialize)]
pub struct FillHistoryData {
    pub total_filled_qty: String,
    pub weighted_price_sum: String,
    pub maker_fill_count: u32,
    pub total_fill_count: u32,
}
#[derive(Clone, Debug, Serialize)]
pub struct LogEvent {
    pub level: LogLevel,
    pub event_type: String,
    pub message: String,
    pub payload: Option<serde_json::Value>,
    #[serde(skip)]
    pub timestamp: Instant,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}
#[derive(Clone, Debug)]
pub struct OrderingState {
    pub open_position: Option<OpenPosition>,
    pub open_order: Option<OpenOrder>,
    pub last_order_update_timestamp: Option<Instant>,
    pub last_position_update_timestamp: Option<Instant>,
    pub reserved_margin: Decimal,
    pub circuit_breaker: CircuitBreakerState,
    pub order_timeouts: std::collections::HashMap<String, Instant>,
    // Track order creation times to detect quick cancellations
    pub order_creation_times: std::collections::HashMap<String, Instant>,
}
#[derive(Clone, Debug)]
pub struct CircuitBreakerState {
    pub reject_count: u32,
    pub last_trigger_time: Option<Instant>,
    pub is_open: bool,
    pub rejection_reasons: std::collections::HashMap<String, u32>,
}
impl OrderingState {
    pub fn new() -> Self {
        Self {
            open_position: None,
            open_order: None,
            last_order_update_timestamp: None,
            last_position_update_timestamp: None,
            reserved_margin: Decimal::ZERO,
            circuit_breaker: CircuitBreakerState {
                reject_count: 0,
                last_trigger_time: None,
                is_open: false,
                rejection_reasons: std::collections::HashMap::new(),
            },
            order_timeouts: std::collections::HashMap::new(),
            order_creation_times: std::collections::HashMap::new(),
        }
    }
}
impl CircuitBreakerState {
    pub fn new() -> Self {
        Self {
            reject_count: 0,
            last_trigger_time: None,
            is_open: false,
            rejection_reasons: std::collections::HashMap::new(),
        }
    }
    pub fn should_block(&self) -> bool {
        if !self.is_open {
            return false;
        }
        if let Some(last_trigger) = self.last_trigger_time {
            let elapsed = Instant::now().duration_since(last_trigger);
            if elapsed > Duration::from_secs(60) {
                return false;
            }
        }
        true
    }
    pub fn record_rejection(&mut self, reason: Option<&str>) -> bool {
        self.reject_count += 1;
        if let Some(reason_str) = reason {
            *self.rejection_reasons.entry(reason_str.to_string()).or_insert(0) += 1;
        }
        const REJECT_THRESHOLD: u32 = 5;
        if self.reject_count >= REJECT_THRESHOLD {
            self.is_open = true;
            self.last_trigger_time = Some(Instant::now());
            true
        } else {
            false
        }
    }
    pub fn reset(&mut self) {
        self.reject_count = 0;
        self.is_open = false;
        self.last_trigger_time = None;
    }
    pub fn reset_if_cooldown_passed(&mut self) {
        if let Some(last_trigger) = self.last_trigger_time {
            let elapsed = Instant::now().duration_since(last_trigger);
            if elapsed > Duration::from_secs(60) {
                self.reset();
            }
        }
    }
}
impl Default for OrderingState {
    fn default() -> Self {
        Self::new()
    }
}
#[derive(Clone, Debug)]
pub struct OpenPosition {
    pub symbol: String,
    pub direction: PositionDirection,
    pub qty: Qty,
    pub entry_price: Px,
}
#[derive(Clone, Debug)]
pub struct OpenOrder {
    pub symbol: String,
    pub order_id: String,
    pub side: Side,
    pub qty: Qty,
}
#[derive(Clone, Debug)]
pub struct BalanceStore {
    pub usdt: Decimal,
    pub usdc: Decimal,
    pub last_updated: Instant,
    pub reserved_usdt: Decimal,
    pub reserved_usdc: Decimal,
}
impl BalanceStore {
    pub fn new() -> Self {
        Self {
            usdt: Decimal::ZERO,
            usdc: Decimal::ZERO,
            last_updated: Instant::now(),
            reserved_usdt: Decimal::ZERO,
            reserved_usdc: Decimal::ZERO,
        }
    }
    pub fn available(&self, asset: &str) -> Decimal {
        let (total, reserved) = if asset.to_uppercase() == "USDT" {
            (self.usdt, self.reserved_usdt)
        } else {
            (self.usdc, self.reserved_usdc)
        };
        total - reserved
    }
    pub fn try_reserve(&mut self, asset: &str, amount: Decimal) -> bool {
        let asset_upper = asset.to_uppercase();
        let is_usdt = asset_upper == "USDT";
        let (total, reserved) = if is_usdt {
            (self.usdt, self.reserved_usdt)
        } else {
            (self.usdc, self.reserved_usdc)
        };
        let available = total - reserved;
        if available >= amount {
            if is_usdt {
                self.reserved_usdt += amount;
            } else {
                self.reserved_usdc += amount;
            }
            true
        } else {
            false
        }
    }
    pub fn release(&mut self, asset: &str, amount: Decimal) {
        if asset.to_uppercase() == "USDT" {
            self.reserved_usdt = (self.reserved_usdt - amount).max(Decimal::ZERO);
        } else {
            self.reserved_usdc = (self.reserved_usdc - amount).max(Decimal::ZERO);
        }
    }
}
impl Default for BalanceStore {
    fn default() -> Self {
        Self::new()
    }
}
#[derive(Deserialize)]
#[serde(tag = "filterType")]
#[allow(non_snake_case)]
pub enum FutFilter {
    #[serde(rename = "PRICE_FILTER")]
    PriceFilter { tickSize: String },
    #[serde(rename = "LOT_SIZE")]
    LotSize { stepSize: String },
    #[serde(rename = "MIN_NOTIONAL")]
    MinNotional { notional: String },
    #[serde(other)]
    Other,
}
#[derive(Deserialize)]
pub struct FutExchangeInfo {
    pub symbols: Vec<FutExchangeSymbol>,
}
#[derive(Deserialize)]
pub struct FutExchangeSymbol {
    pub symbol: String,
    #[serde(rename = "baseAsset")]
    pub base_asset: String,
    #[serde(rename = "quoteAsset")]
    pub quote_asset: String,
    #[serde(rename = "contractType")]
    pub contract_type: String,
    pub status: String,
    #[serde(default)]
    pub filters: Vec<FutFilter>,
    #[serde(rename = "pricePrecision", default)]
    pub price_precision: Option<usize>,
    #[serde(rename = "quantityPrecision", default)]
    pub qty_precision: Option<usize>,
}
#[derive(Clone, Debug)]
pub struct OrderFillHistory {
    pub total_filled_qty: Qty,
    pub weighted_price_sum: Decimal,
    pub last_update: Instant,
    pub maker_fill_count: u32,
    pub total_fill_count: u32,
}
#[derive(Clone, Debug)]
pub struct SymbolStats24h {
    pub symbol: String,
    pub price_change_percent: f64,
    pub volume: f64,
    pub quote_volume: f64,
    pub trades: u64,
    pub high_price: f64,
    pub low_price: f64,
}
#[derive(Clone, Debug)]
pub struct ScoredSymbol {
    pub symbol: String,
    pub score: f64,
    pub volatility: f64,
    pub volume: f64,
    pub trades: u64,
}
#[derive(Clone, Debug)]
pub struct PricePoint {
    pub timestamp: Instant,
    pub price: Decimal,
    pub volume: Option<Decimal>,
}
#[derive(Clone, Debug)]
pub struct SymbolState {
    pub symbol: String,
    pub prices: std::collections::VecDeque<PricePoint>,
    pub last_signal_time: Option<Instant>,
    pub last_position_close_time: Option<Instant>,
    pub last_position_direction: Option<PositionDirection>,
    pub tick_counter: u32,
    pub ema_9: Option<Decimal>,
    pub ema_21: Option<Decimal>,
    pub ema_55: Option<Decimal>,
    pub ema_55_history: std::collections::VecDeque<Decimal>,
    pub rsi_avg_gain: Option<Decimal>,
    pub rsi_avg_loss: Option<Decimal>,
    pub rsi_period_count: usize,
    pub last_analysis_time: Option<Instant>,
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TrendSignal {
    Long,
    Short,
}
#[derive(Clone, Debug)]
pub struct LastSignal {
    pub side: Side,
    pub timestamp: Instant,
}
#[derive(Clone, Debug)]
pub struct PositionInfo {
    pub symbol: String,
    pub qty: Qty,
    pub entry_price: Px,
    pub direction: PositionDirection,
    pub leverage: u32,
    pub stop_loss_pct: Option<f64>,
    pub take_profit_pct: Option<f64>,
    pub opened_at: Instant,
    pub is_maker: Option<bool>,
    pub close_requested: bool,
    pub liquidation_price: Option<Px>,
    pub trailing_stop_placed: bool,
}
