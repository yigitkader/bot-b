//location: /crates/exec/src/binance.rs
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use bot_core::types::*;
use hmac::{Hmac, Mac};
use reqwest::Client;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::Deserialize;
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};
use urlencoding::encode;

use super::{Venue, VenueOrder};

// ---- Ortak ----

#[derive(Clone, Debug)]
pub struct SymbolMeta {
    pub symbol: String,
    pub base_asset: String,
    pub quote_asset: String,
    pub status: Option<String>,
    pub contract_type: Option<String>,
}

#[derive(Clone)]
pub struct BinanceCommon {
    pub client: Client,
    pub api_key: String,
    pub secret_key: String,
    pub recv_window_ms: u64,
}

impl BinanceCommon {
    fn ts() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }
    fn sign(&self, qs: &str) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(self.secret_key.as_bytes()).unwrap();
        mac.update(qs.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }
}

// ---- SPOT ----

#[derive(Clone)]
pub struct BinanceSpot {
    pub base: String, // e.g. https://api.binance.com
    pub common: BinanceCommon,
    pub price_tick: f64,
    pub qty_step: f64,
}

#[derive(Deserialize)]
struct BookTickerSpot {
    #[serde(rename = "bidPrice")]
    bid_price: String,
    #[serde(rename = "askPrice")]
    ask_price: String,
}

#[derive(Deserialize)]
struct SpotExchangeInfo {
    symbols: Vec<SpotExchangeSymbol>,
}

#[derive(Deserialize)]
struct SpotExchangeSymbol {
    symbol: String,
    #[serde(rename = "baseAsset")]
    base_asset: String,
    #[serde(rename = "quoteAsset")]
    quote_asset: String,
    status: String,
}

#[derive(Deserialize)]
struct SpotPlacedOrder {
    #[serde(rename = "orderId")]
    order_id: u64,
}

#[derive(Deserialize)]
struct SpotOpenOrder {
    #[serde(rename = "orderId")]
    order_id: u64,
    #[serde(rename = "price")]
    price: String,
    #[serde(rename = "origQty")]
    orig_qty: String,
    #[serde(rename = "side")]
    side: String,
}

#[derive(Deserialize)]
struct SpotTrade {
    #[serde(rename = "id")]
    id: i64,
    #[serde(rename = "qty")]
    qty: String,
    #[serde(rename = "price")]
    price: String,
    #[serde(rename = "isBuyer")]
    is_buyer: bool,
    #[serde(rename = "time")]
    time: u64,
}

impl BinanceSpot {
    pub async fn symbol_assets(&self, sym: &str) -> Result<(String, String)> {
        let url = format!("{}/api/v3/exchangeInfo?symbol={}", self.base, encode(sym));
        let info: SpotExchangeInfo = self
            .common
            .client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let sym = info
            .symbols
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("symbol info missing"))?;
        Ok((sym.base_asset, sym.quote_asset))
    }

    pub async fn symbol_metadata(&self) -> Result<Vec<SymbolMeta>> {
        let url = format!("{}/api/v3/exchangeInfo", self.base);
        let info: SpotExchangeInfo = self
            .common
            .client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(info
            .symbols
            .into_iter()
            .map(|s| SymbolMeta {
                symbol: s.symbol,
                base_asset: s.base_asset,
                quote_asset: s.quote_asset,
                status: Some(s.status),
                contract_type: None,
            })
            .collect())
    }

    pub async fn asset_free(&self, asset: &str) -> Result<Decimal> {
        #[derive(Deserialize)]
        struct SpotBalance {
            asset: String,
            free: String,
        }
        #[derive(Deserialize)]
        struct SpotAccountInfo {
            balances: Vec<SpotBalance>,
        }

        let qs = format!(
            "timestamp={}&recvWindow={}",
            BinanceCommon::ts(),
            self.common.recv_window_ms
        );
        let sig = self.common.sign(&qs);
        let url = format!("{}/api/v3/account?{}&signature={}", self.base, qs, sig);
        let info: SpotAccountInfo = self
            .common
            .client
            .get(url)
            .header("X-MBX-APIKEY", &self.common.api_key)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let bal = info.balances.into_iter().find(|b| b.asset == asset);
        let amt = match bal {
            Some(b) => Decimal::from_str_radix(&b.free, 10)?,
            None => Decimal::ZERO,
        };
        Ok(amt)
    }

    pub async fn fetch_open_orders(&self, sym: &str) -> Result<Vec<VenueOrder>> {
        let qs = format!(
            "symbol={}&timestamp={}&recvWindow={}",
            sym,
            BinanceCommon::ts(),
            self.common.recv_window_ms
        );
        let sig = self.common.sign(&qs);
        let url = format!("{}/api/v3/openOrders?{}&signature={}", self.base, qs, sig);
        let orders: Vec<SpotOpenOrder> = self
            .common
            .client
            .get(url)
            .header("X-MBX-APIKEY", &self.common.api_key)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let mut res = Vec::new();
        for o in orders {
            let price = Decimal::from_str_radix(&o.price, 10)?;
            let qty = Decimal::from_str_radix(&o.orig_qty, 10)?;
            let side = if o.side.eq_ignore_ascii_case("buy") {
                Side::Buy
            } else {
                Side::Sell
            };
            res.push(VenueOrder {
                order_id: o.order_id.to_string(),
                side,
                price: Px(price),
                qty: Qty(qty),
            });
        }
        Ok(res)
    }

    pub async fn get_fills_since(&self, symbol: &str, from_id: Option<i64>) -> Result<Vec<Fill>> {
        let mut params = vec![
            format!("symbol={}", symbol),
            format!("timestamp={}", BinanceCommon::ts()),
            format!("recvWindow={}", self.common.recv_window_ms),
        ];
        if let Some(id) = from_id {
            params.push(format!("fromId={}", id));
        }
        let qs = params.join("&");
        let sig = self.common.sign(&qs);
        let url = format!("{}/api/v3/myTrades?{}&signature={}", self.base, qs, sig);
        let trades: Vec<SpotTrade> = self
            .common
            .client
            .get(url)
            .header("X-MBX-APIKEY", &self.common.api_key)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let mut fills = Vec::new();
        for t in trades {
            let qty = Decimal::from_str_radix(&t.qty, 10)?;
            let price = Decimal::from_str_radix(&t.price, 10)?;
            let side = if t.is_buyer { Side::Buy } else { Side::Sell };
            fills.push(Fill {
                id: t.id,
                symbol: symbol.to_string(),
                side,
                qty: Qty(qty),
                price: Px(price),
                timestamp: t.time,
            });
        }
        Ok(fills)
    }

    pub async fn fetch_position(&self, symbol: &str) -> Result<Position> {
        let (base, _) = self.symbol_assets(symbol).await?;
        let qty = self.asset_free(&base).await?;
        Ok(Position {
            symbol: symbol.to_string(),
            qty: Qty(qty),
            entry: Px(Decimal::ZERO),
            leverage: 1,
            liq_px: None,
        })
    }

    pub async fn fetch_mark_price(&self, sym: &str) -> Result<Px> {
        let (bid, ask) = self.best_prices(sym).await?;
        let mid = (bid.0 + ask.0) / Decimal::from(2);
        Ok(Px(mid))
    }

    pub async fn flatten_position(&self, symbol: &str) -> Result<()> {
        let pos = self.fetch_position(symbol).await?;
        let qty_f = pos.qty.0.to_f64().unwrap_or(0.0);
        if qty_f.abs() < f64::EPSILON {
            return Ok(());
        }
        let side = if qty_f > 0.0 { Side::Sell } else { Side::Buy };
        let qty = quantize(qty_f.abs(), self.qty_step);
        if qty <= 0.0 {
            warn!(%symbol, original_qty = qty_f, "quantized position size is zero, skipping close");
            return Ok(());
        }

        let params = vec![
            format!("symbol={}", symbol),
            format!(
                "side={}",
                if matches!(side, Side::Buy) {
                    "BUY"
                } else {
                    "SELL"
                }
            ),
            "type=MARKET".to_string(),
            format!("quantity={}", qty),
            format!("timestamp={}", BinanceCommon::ts()),
            format!("recvWindow={}", self.common.recv_window_ms),
        ];
        let qs = params.join("&");
        let sig = self.common.sign(&qs);
        let url = format!("{}/api/v3/order?{}&signature={}", self.base, qs, sig);
        self.common
            .client
            .post(url)
            .header("X-MBX-APIKEY", &self.common.api_key)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

#[async_trait]
impl Venue for BinanceSpot {
    async fn place_limit(
        &self,
        sym: &str,
        side: Side,
        px: Px,
        qty: Qty,
        tif: Tif,
    ) -> Result<String> {
        let s_side = match side {
            Side::Buy => "BUY",
            Side::Sell => "SELL",
        };
        let (order_type, tif_str) = match tif {
            Tif::PostOnly => ("LIMIT_MAKER", None), // Post-only SPOT
            Tif::Gtc => ("LIMIT", Some("GTC")),
            Tif::Ioc => ("LIMIT", Some("IOC")),
        };

        let price = quantize(px.0.to_f64().unwrap_or(0.0), self.price_tick);
        let qty = quantize(qty.0.to_f64().unwrap_or(0.0), self.qty_step);

        let mut params = vec![
            format!("symbol={}", sym),
            format!("side={}", s_side),
            format!("type={}", order_type),
            format!("price={}", price),
            format!("quantity={}", qty),
            format!("timestamp={}", BinanceCommon::ts()),
            format!("recvWindow={}", self.common.recv_window_ms),
            "newOrderRespType=RESULT".to_string(),
        ];
        if let Some(t) = tif_str {
            params.push(format!("timeInForce={}", t));
        }

        let qs = params.join("&");
        let sig = self.common.sign(&qs);
        let url = format!("{}/api/v3/order?{}&signature={}", self.base, qs, sig);

        let order: SpotPlacedOrder = self
            .common
            .client
            .post(url)
            .header("X-MBX-APIKEY", &self.common.api_key)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        info!(%sym, ?side, %price, %qty, tif = ?tif, order_id = order.order_id, "spot place_limit ok");
        Ok(order.order_id.to_string())
    }

    async fn cancel(&self, order_id: &str, sym: &str) -> Result<()> {
        let qs = format!(
            "symbol={}&orderId={}&timestamp={}&recvWindow={}",
            sym,
            order_id,
            BinanceCommon::ts(),
            self.common.recv_window_ms
        );
        let sig = self.common.sign(&qs);
        let url = format!("{}/api/v3/order?{}&signature={}", self.base, qs, sig);

        self.common
            .client
            .delete(url)
            .header("X-MBX-APIKEY", &self.common.api_key)
            .send()
            .await?
            .error_for_status()?;

        Ok(())
    }

    async fn best_prices(&self, sym: &str) -> Result<(Px, Px)> {
        let url = format!(
            "{}/api/v3/ticker/bookTicker?symbol={}",
            self.base,
            encode(sym)
        );
        let t: BookTickerSpot = self
            .common
            .client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        use rust_decimal::Decimal;
        let bid = Px(Decimal::from_str_radix(&t.bid_price, 10)?);
        let ask = Px(Decimal::from_str_radix(&t.ask_price, 10)?);
        Ok((bid, ask))
    }

    async fn get_open_orders(&self, sym: &str) -> Result<Vec<VenueOrder>> {
        self.fetch_open_orders(sym).await
    }

    async fn get_position(&self, sym: &str) -> Result<Position> {
        self.fetch_position(sym).await
    }

    async fn mark_price(&self, sym: &str) -> Result<Px> {
        self.fetch_mark_price(sym).await
    }

    async fn close_position(&self, sym: &str) -> Result<()> {
        self.flatten_position(sym).await
    }
}

// ---- USDT-M Futures ----

#[derive(Clone)]
pub struct BinanceFutures {
    pub base: String, // e.g. https://fapi.binance.com
    pub common: BinanceCommon,
    pub price_tick: f64,
    pub qty_step: f64,
}

#[derive(Deserialize)]
struct OrderBookTop {
    bids: Vec<(String, String)>,
    asks: Vec<(String, String)>,
}

#[derive(Deserialize)]
struct FutExchangeInfo {
    symbols: Vec<FutExchangeSymbol>,
}

#[derive(Deserialize)]
struct FutExchangeSymbol {
    symbol: String,
    #[serde(rename = "baseAsset")]
    base_asset: String,
    #[serde(rename = "quoteAsset")]
    quote_asset: String,
    #[serde(rename = "contractType")]
    contract_type: String,
    status: String,
}

#[derive(Deserialize)]
struct FutPlacedOrder {
    #[serde(rename = "orderId")]
    order_id: u64,
}

#[derive(Deserialize)]
struct FutOpenOrder {
    #[serde(rename = "orderId")]
    order_id: u64,
    #[serde(rename = "price")]
    price: String,
    #[serde(rename = "origQty")]
    orig_qty: String,
    #[serde(rename = "side")]
    side: String,
}

#[derive(Deserialize)]
struct FutPosition {
    #[serde(rename = "symbol")]
    symbol: String,
    #[serde(rename = "positionAmt")]
    position_amt: String,
    #[serde(rename = "entryPrice")]
    entry_price: String,
    #[serde(rename = "leverage")]
    leverage: String,
    #[serde(rename = "liquidationPrice")]
    liquidation_price: String,
}

#[derive(Deserialize)]
struct PremiumIndex {
    #[serde(rename = "markPrice")]
    mark_price: String,
}

impl BinanceFutures {
    pub async fn symbol_assets(&self, sym: &str) -> Result<(String, String)> {
        let url = format!("{}/fapi/v1/exchangeInfo?symbol={}", self.base, encode(sym));
        let info: FutExchangeInfo = self
            .common
            .client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let sym = info
            .symbols
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("symbol info missing"))?;
        Ok((sym.base_asset, sym.quote_asset))
    }

    pub async fn symbol_metadata(&self) -> Result<Vec<SymbolMeta>> {
        let url = format!("{}/fapi/v1/exchangeInfo", self.base);
        let info: FutExchangeInfo = self
            .common
            .client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(info
            .symbols
            .into_iter()
            .map(|s| SymbolMeta {
                symbol: s.symbol,
                base_asset: s.base_asset,
                quote_asset: s.quote_asset,
                status: Some(s.status),
                contract_type: Some(s.contract_type),
            })
            .collect())
    }

    pub async fn available_balance(&self, asset: &str) -> Result<Decimal> {
        #[derive(Deserialize)]
        struct FutBalance {
            asset: String,
            #[serde(rename = "availableBalance")]
            available_balance: String,
        }

        let qs = format!(
            "timestamp={}&recvWindow={}",
            BinanceCommon::ts(),
            self.common.recv_window_ms
        );
        let sig = self.common.sign(&qs);
        let url = format!("{}/fapi/v2/balance?{}&signature={}", self.base, qs, sig);
        let balances: Vec<FutBalance> = self
            .common
            .client
            .get(url)
            .header("X-MBX-APIKEY", &self.common.api_key)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let bal = balances.into_iter().find(|b| b.asset == asset);
        let amt = match bal {
            Some(b) => Decimal::from_str_radix(&b.available_balance, 10)?,
            None => Decimal::ZERO,
        };
        Ok(amt)
    }

    pub async fn fetch_open_orders(&self, sym: &str) -> Result<Vec<VenueOrder>> {
        let qs = format!(
            "symbol={}&timestamp={}&recvWindow={}",
            sym,
            BinanceCommon::ts(),
            self.common.recv_window_ms
        );
        let sig = self.common.sign(&qs);
        let url = format!("{}/fapi/v1/openOrders?{}&signature={}", self.base, qs, sig);
        let orders: Vec<FutOpenOrder> = self
            .common
            .client
            .get(url)
            .header("X-MBX-APIKEY", &self.common.api_key)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let mut res = Vec::new();
        for o in orders {
            let price = Decimal::from_str_radix(&o.price, 10)?;
            let qty = Decimal::from_str_radix(&o.orig_qty, 10)?;
            let side = if o.side.eq_ignore_ascii_case("buy") {
                Side::Buy
            } else {
                Side::Sell
            };
            res.push(VenueOrder {
                order_id: o.order_id.to_string(),
                side,
                price: Px(price),
                qty: Qty(qty),
            });
        }
        Ok(res)
    }

    pub async fn fetch_position(&self, sym: &str) -> Result<Position> {
        let qs = format!(
            "symbol={}&timestamp={}&recvWindow={}",
            sym,
            BinanceCommon::ts(),
            self.common.recv_window_ms
        );
        let sig = self.common.sign(&qs);
        let url = format!(
            "{}/fapi/v2/positionRisk?{}&signature={}",
            self.base, qs, sig
        );
        let mut positions: Vec<FutPosition> = self
            .common
            .client
            .get(url)
            .header("X-MBX-APIKEY", &self.common.api_key)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let pos = positions
            .drain(..)
            .find(|p| p.symbol.eq_ignore_ascii_case(sym))
            .ok_or_else(|| anyhow!("position not found for symbol"))?;
        let qty = Decimal::from_str_radix(&pos.position_amt, 10)?;
        let entry = Decimal::from_str_radix(&pos.entry_price, 10)?;
        let leverage = pos.leverage.parse::<u32>().unwrap_or(1);
        let liq = Decimal::from_str_radix(&pos.liquidation_price, 10).unwrap_or(Decimal::ZERO);
        let liq_px = if liq > Decimal::ZERO {
            Some(Px(liq))
        } else {
            None
        };
        Ok(Position {
            symbol: sym.to_string(),
            qty: Qty(qty),
            entry: Px(entry),
            leverage,
            liq_px,
        })
    }

    pub async fn fetch_mark_price(&self, sym: &str) -> Result<Px> {
        let url = format!("{}/fapi/v1/premiumIndex?symbol={}", self.base, sym);
        let premium: PremiumIndex = self
            .common
            .client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let mark = Decimal::from_str_radix(&premium.mark_price, 10)?;
        Ok(Px(mark))
    }

    pub async fn flatten_position(&self, sym: &str) -> Result<()> {
        let pos = self.fetch_position(sym).await?;
        let qty_f = pos.qty.0.to_f64().unwrap_or(0.0);
        if qty_f.abs() < f64::EPSILON {
            return Ok(());
        }
        let side = if qty_f > 0.0 { Side::Sell } else { Side::Buy };
        let qty = quantize(qty_f.abs(), self.qty_step);
        if qty <= 0.0 {
            warn!(symbol = %sym, original_qty = qty_f, "quantized position size is zero, skipping close");
            return Ok(());
        }
        let params = vec![
            format!("symbol={}", sym),
            format!(
                "side={}",
                if matches!(side, Side::Buy) {
                    "BUY"
                } else {
                    "SELL"
                }
            ),
            "type=MARKET".to_string(),
            format!("quantity={}", qty),
            "reduceOnly=true".to_string(),
            format!("timestamp={}", BinanceCommon::ts()),
            format!("recvWindow={}", self.common.recv_window_ms),
        ];
        let qs = params.join("&");
        let sig = self.common.sign(&qs);
        let url = format!("{}/fapi/v1/order?{}&signature={}", self.base, qs, sig);
        self.common
            .client
            .post(url)
            .header("X-MBX-APIKEY", &self.common.api_key)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

#[async_trait]
impl Venue for BinanceFutures {
    async fn place_limit(
        &self,
        sym: &str,
        side: Side,
        px: Px,
        qty: Qty,
        tif: Tif,
    ) -> Result<String> {
        let s_side = match side {
            Side::Buy => "BUY",
            Side::Sell => "SELL",
        };
        let tif_str = match tif {
            Tif::PostOnly => "GTX",
            Tif::Gtc => "GTC",
            Tif::Ioc => "IOC",
        };

        let price = quantize(px.0.to_f64().unwrap_or(0.0), self.price_tick);
        let qty = quantize(qty.0.to_f64().unwrap_or(0.0), self.qty_step);

        let params = vec![
            format!("symbol={}", sym),
            format!("side={}", s_side),
            "type=LIMIT".to_string(),
            format!("timeInForce={}", tif_str),
            format!("price={}", price),
            format!("quantity={}", qty),
            format!("timestamp={}", BinanceCommon::ts()),
            format!("recvWindow={}", self.common.recv_window_ms),
            "newOrderRespType=RESULT".to_string(),
        ];
        let qs = params.join("&");
        let sig = self.common.sign(&qs);
        let url = format!("{}/fapi/v1/order?{}&signature={}", self.base, qs, sig);

        let order: FutPlacedOrder = self
            .common
            .client
            .post(url)
            .header("X-MBX-APIKEY", &self.common.api_key)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        info!(%sym, ?side, %price, %qty, tif = ?tif, order_id = order.order_id, "futures place_limit ok");
        Ok(order.order_id.to_string())
    }

    async fn cancel(&self, order_id: &str, sym: &str) -> Result<()> {
        let qs = format!(
            "symbol={}&orderId={}&timestamp={}&recvWindow={}",
            sym,
            order_id,
            BinanceCommon::ts(),
            self.common.recv_window_ms
        );
        let sig = self.common.sign(&qs);
        let url = format!("{}/fapi/v1/order?{}&signature={}", self.base, qs, sig);

        self.common
            .client
            .delete(url)
            .header("X-MBX-APIKEY", &self.common.api_key)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    async fn best_prices(&self, sym: &str) -> Result<(Px, Px)> {
        let url = format!("{}/fapi/v1/depth?symbol={}&limit=5", self.base, encode(sym));
        let d: OrderBookTop = self
            .common
            .client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        use rust_decimal::Decimal;
        let best_bid = d.bids.get(0).ok_or_else(|| anyhow!("no bid"))?.0.clone();
        let best_ask = d.asks.get(0).ok_or_else(|| anyhow!("no ask"))?.0.clone();
        Ok((
            Px(Decimal::from_str_radix(&best_bid, 10)?),
            Px(Decimal::from_str_radix(&best_ask, 10)?),
        ))
    }

    async fn get_open_orders(&self, sym: &str) -> Result<Vec<VenueOrder>> {
        self.fetch_open_orders(sym).await
    }

    async fn get_position(&self, sym: &str) -> Result<Position> {
        self.fetch_position(sym).await
    }

    async fn mark_price(&self, sym: &str) -> Result<Px> {
        self.fetch_mark_price(sym).await
    }

    async fn close_position(&self, sym: &str) -> Result<()> {
        self.flatten_position(sym).await
    }
}

// ---- helpers ----

fn quantize(x: f64, step: f64) -> f64 {
    if step <= 0.0 {
        return x;
    }
    let rounded = (x / step).floor() * step;
    if rounded > 0.0 && rounded < step {
        return 0.0;
    }
    rounded
}
