use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::time::Instant;

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

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ForceOrderRecord {
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
pub struct DepthSnapshot {
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

pub struct LiqState {
    pub entries: VecDeque<LiqEntry>,
    pub long_sum: f64,
    pub short_sum: f64,
    pub open_interest: f64,
    pub window_secs: u64,
}

pub struct LiqEntry {
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

#[derive(Debug, Deserialize)]
pub struct KlineEvent {
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
pub struct CombinedStreamEvent {
    pub stream: String,
    pub data: KlineEvent,
}

#[derive(Debug, Deserialize)]
pub struct KlineData {
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

#[derive(Debug, Deserialize)]
pub struct OpenInterestHistPoint {
    #[serde(rename = "symbol")]
    pub _symbol: String,
    #[serde(rename = "sumOpenInterest")]
    pub sum_open_interest: String,
    #[serde(rename = "sumOpenInterestValue")]
    pub _sum_open_interest_value: String,
    pub timestamp: i64,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ServerTimeResponse {
    #[serde(rename = "serverTime")]
    pub server_time: i64,
}

impl PremiumIndex {
    pub(crate) fn mark_price(&self) -> anyhow::Result<f64> {
        self.mark_price_str
            .parse::<f64>()
            .map_err(|e| anyhow::anyhow!("failed to parse mark price: {}", e))
    }

    pub(crate) fn funding_rate(&self) -> Option<f64> {
        self.last_funding_rate
            .as_ref()
            .and_then(|v| v.parse::<f64>().ok())
    }

    pub(crate) fn into_parts(self) -> anyhow::Result<(String, f64, Option<f64>, chrono::DateTime<chrono::Utc>)> {
        use chrono::Utc;
        let price = self.mark_price()?;
        let funding = self.funding_rate();
        let ts = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(self.event_time_ms as i64)
            .unwrap_or_else(|| Utc::now());
        Ok((self.symbol, price, funding, ts))
    }
}

impl OrderTradeUpdate {
    pub(crate) fn into_order_update(self) -> crate::types::OrderUpdate {
        use crate::types::core::{OrderStatus, Side};
        use crate::types::OrderUpdate;
        use chrono::Utc;
        use uuid::Uuid;

        let side = if self.order.side == "BUY" {
            Side::Long
        } else {
            Side::Short
        };
        let status = match self.order.status.as_str() {
            "NEW" => OrderStatus::New,
            "PARTIALLY_FILLED" => OrderStatus::PartiallyFilled,
            "FILLED" => OrderStatus::Filled,
            "CANCELED" => OrderStatus::Canceled,
            "REJECTED" => OrderStatus::Rejected,
            _ => OrderStatus::New,
        };

        OrderUpdate {
            order_id: Uuid::new_v4(),
            symbol: self.order.symbol,
            side,
            status,
            filled_qty: self.order.filled_qty.parse().unwrap_or(0.0),
            ts: chrono::DateTime::<chrono::Utc>::from_timestamp_millis(self.order.event_time as i64)
                .unwrap_or_else(|| Utc::now()),
        }
    }
}

impl AccountBalance {
    pub(crate) fn to_balance_snapshot(&self) -> Option<crate::types::BalanceSnapshot> {
        use crate::types::BalanceSnapshot;
        use chrono::Utc;
        let free = self.wallet_balance.parse().ok()?;
        Some(BalanceSnapshot {
            asset: self.asset.clone(),
            free,
            ts: Utc::now(),
        })
    }
}

impl AccountPosition {
    pub(crate) fn to_position_update(&self, leverage: f64) -> Option<crate::types::PositionUpdate> {
        use crate::types::core::Side;
        use crate::types::PositionUpdate;
        use chrono::Utc;
        let size = self.position_amount.parse::<f64>().ok()?;
        let entry = self.entry_price.parse::<f64>().ok().unwrap_or(0.0);
        let pnl = self.unrealized_pnl.parse::<f64>().ok().unwrap_or(0.0);
        let side = if size >= 0.0 { Side::Long } else { Side::Short };

        Some(PositionUpdate {
            position_id: PositionUpdate::position_id(&self.symbol, side),
            symbol: self.symbol.clone(),
            side,
            entry_price: entry,
            size: size.abs(),
            leverage,
            unrealized_pnl: pnl,
            ts: Utc::now(),
            is_closed: size == 0.0,
        })
    }
}

pub struct DepthState {
    pub(crate) bids: Vec<(f64, f64)>,
    pub(crate) asks: Vec<(f64, f64)>,
    depth_limit: usize,
    obi: Option<f64>,
}

impl DepthState {
    pub(crate) fn new(depth_limit: usize) -> Self {
        Self {
            bids: Vec::new(),
            asks: Vec::new(),
            depth_limit,
            obi: None,
        }
    }

    pub(crate) fn update(&mut self, event: &DepthEvent) {
        for [price_str, qty_str] in &event.bids {
            let Ok(price) = price_str.parse::<f64>() else {
                continue;
            };
            let Ok(qty) = qty_str.parse::<f64>() else {
                continue;
            };

            if qty == 0.0 {
                self.bids.retain(|(p, _)| *p != price);
            } else {
                if let Some(level) = self.bids.iter_mut().find(|(p, _)| *p == price) {
                    level.1 = qty;
                } else {
                    self.bids.push((price, qty));
                }
            }
        }

        self.bids.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        self.bids.truncate(self.depth_limit);

        for [price_str, qty_str] in &event.asks {
            let Ok(price) = price_str.parse::<f64>() else { continue };
            let Ok(qty) = qty_str.parse::<f64>() else { continue };

            if qty == 0.0 {
                self.asks.retain(|(p, _)| *p != price);
            } else {
                if let Some(level) = self.asks.iter_mut().find(|(p, _)| *p == price) {
                    level.1 = qty;
                } else {
                    self.asks.push((price, qty));
                }
            }
        }

        self.asks.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        self.asks.truncate(self.depth_limit);

        let bid_sum: f64 = self.bids.iter().map(|(_, qty)| qty).sum();
        let ask_sum: f64 = self.asks.iter().map(|(_, qty)| qty).sum();
        if bid_sum > 0.0 && ask_sum > 0.0 {
            self.obi = Some(bid_sum / ask_sum);
        } else {
            self.obi = None;
        }
    }

    pub(crate) fn obi(&self) -> Option<f64> {
        self.obi
    }

    pub(crate) fn reset_with_snapshot(&mut self, snapshot: DepthSnapshot) {
        self.bids = snapshot
            .bids
            .iter()
            .take(self.depth_limit)
            .filter_map(|lvl| {
                let price = lvl[0].parse::<f64>().ok()?;
                let qty = lvl[1].parse::<f64>().ok()?;
                Some((price, qty))
            })
            .collect();
        self.asks = snapshot
            .asks
            .iter()
            .take(self.depth_limit)
            .filter_map(|lvl| {
                let price = lvl[0].parse::<f64>().ok()?;
                let qty = lvl[1].parse::<f64>().ok()?;
                Some((price, qty))
            })
            .collect();

        let bid_sum: f64 = self.bids.iter().map(|(_, qty)| qty).sum();
        let ask_sum: f64 = self.asks.iter().map(|(_, qty)| qty).sum();
        if bid_sum > 0.0 && ask_sum > 0.0 {
            self.obi = Some(bid_sum / ask_sum);
        } else {
            self.obi = None;
        }
    }

    pub(crate) fn best_bid(&self) -> Option<(f64, f64)> {
        self.bids.first().copied()
    }

    pub(crate) fn best_ask(&self) -> Option<(f64, f64)> {
        self.asks.first().copied()
    }

    pub(crate) fn depth_notional_usd(&self, is_bid: bool, levels: usize) -> Option<f64> {
        let book = if is_bid { &self.bids } else { &self.asks };
        if book.is_empty() {
            return None;
        }
        let mut total = 0.0;
        for (price, qty) in book.iter().take(levels.max(1)) {
            total += price * qty;
        }
        if total > 0.0 {
            Some(total)
        } else {
            None
        }
    }
}

impl LiqState {
    pub(crate) fn new(window_secs: u64) -> Self {
        Self {
            entries: VecDeque::new(),
            long_sum: 0.0,
            short_sum: 0.0,
            open_interest: 0.0,
            window_secs,
        }
    }

    pub(crate) fn record(&mut self, side: &str, notional: f64) {
        if self.open_interest <= f64::EPSILON || notional <= 0.0 {
            return;
        }
        let ratio = notional / self.open_interest;
        let now = Instant::now();
        self.prune(now);

        let is_long_cluster = side.eq_ignore_ascii_case("SELL");
        self.entries.push_back(LiqEntry {
            ts: now,
            ratio,
            is_long_cluster,
        });
        if is_long_cluster {
            self.long_sum += ratio;
        } else {
            self.short_sum += ratio;
        }
    }

    pub(crate) fn set_open_interest(&mut self, value: f64) {
        if value > 0.0 {
            self.open_interest = value;
        }
    }

    pub(crate) fn snapshot(&self) -> (Option<f64>, Option<f64>) {
        (
            if self.long_sum > 0.0 {
                Some(self.long_sum)
            } else {
                None
            },
            if self.short_sum > 0.0 {
                Some(self.short_sum)
            } else {
                None
            },
        )
    }

    fn prune(&mut self, now: Instant) {
        use std::time::Duration;
        while let Some(entry) = self.entries.front() {
            let age = now
                .checked_duration_since(entry.ts)
                .unwrap_or(Duration::from_secs(self.window_secs + 1));

            if age > Duration::from_secs(self.window_secs) {
                let entry = self.entries.pop_front().unwrap();
                if entry.is_long_cluster {
                    self.long_sum = (self.long_sum - entry.ratio).max(0.0);
                } else {
                    self.short_sum = (self.short_sum - entry.ratio).max(0.0);
                }
            } else {
                break;
            }
        }
    }
}

