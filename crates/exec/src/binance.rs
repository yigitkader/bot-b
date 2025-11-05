use anyhow::{anyhow, Result};
use async_trait::async_trait;
use bot_core::types::*;
use hmac::{Hmac, Mac};
use reqwest::Client;
use serde::Deserialize;
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::info;
use urlencoding::encode;
use rust_decimal::prelude::ToPrimitive;


use super::Venue;

// ---- Ortak ----

#[derive(Clone)]
pub struct BinanceCommon {
    pub client: Client,
    pub api_key: String,
    pub secret_key: String,
    pub recv_window_ms: u64,
}

impl BinanceCommon {
    fn ts() -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
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
    pub base: String,          // e.g. https://api.binance.com
    pub common: BinanceCommon,
    pub price_tick: f64,
    pub qty_step: f64,
}

#[derive(Deserialize)]
struct BookTickerSpot { bidPrice: String, askPrice: String }

#[async_trait]
impl Venue for BinanceSpot {
    async fn place_limit(&self, sym: &str, side: Side, px: Px, qty: Qty, tif: Tif) -> Result<String> {
        let s_side = match side { Side::Buy => "BUY", Side::Sell => "SELL" };
        let (order_type, tif_str) = match tif {
            Tif::PostOnly => ("LIMIT_MAKER", None),                // Post-only SPOT
            Tif::Gtc      => ("LIMIT", Some("GTC")),
            Tif::Ioc      => ("LIMIT", Some("IOC")),
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
        if let Some(t) = tif_str { params.push(format!("timeInForce={}", t)); }

        let qs = params.join("&");
        let sig = self.common.sign(&qs);
        let url = format!("{}/api/v3/order?{}&signature={}", self.base, qs, sig);

        let res = self.common.client
            .post(url)
            .header("X-MBX-APIKEY", &self.common.api_key)
            .send().await?
            .error_for_status()?
            .text().await?;

        info!(%sym, ?side, %price, %qty, tif = ?tif, "spot place_limit ok");
        // Dönen JSON içinde orderId var; burada pars etmeyip raw dönebiliriz
        Ok(res)
    }

    async fn cancel(&self, order_id: &str, sym: &str) -> Result<()> {
        let qs = format!(
            "symbol={}&orderId={}&timestamp={}&recvWindow={}",
            sym, order_id, BinanceCommon::ts(), self.common.recv_window_ms
        );
        let sig = self.common.sign(&qs);
        let url = format!("{}/api/v3/order?{}&signature={}", self.base, qs, sig);

        self.common.client
            .delete(url)
            .header("X-MBX-APIKEY", &self.common.api_key)
            .send().await?
            .error_for_status()?;

        Ok(())
    }

    async fn best_prices(&self, sym: &str) -> Result<(Px, Px)> {
        let url = format!("{}/api/v3/ticker/bookTicker?symbol={}", self.base, encode(sym));
        let t: BookTickerSpot = self.common.client.get(url).send().await?.error_for_status()?.json().await?;
        use rust_decimal::Decimal;
        let bid = Px(Decimal::from_str_radix(&t.bidPrice, 10)?);
        let ask = Px(Decimal::from_str_radix(&t.askPrice, 10)?);
        Ok((bid, ask))
    }
}

// ---- USDT-M Futures ----

#[derive(Clone)]
pub struct BinanceFutures {
    pub base: String,         // e.g. https://fapi.binance.com
    pub common: BinanceCommon,
    pub price_tick: f64,
    pub qty_step: f64,
}

#[derive(Deserialize)]
struct OrderBookTop { bids: Vec<(String,String)>, asks: Vec<(String,String)> }

#[async_trait]
impl Venue for BinanceFutures {
    async fn place_limit(&self, sym: &str, side: Side, px: Px, qty: Qty, tif: Tif) -> Result<String> {
        let s_side = match side { Side::Buy => "BUY", Side::Sell => "SELL" };
        let tif_str = match tif {
            Tif::PostOnly => "GTX",   // Post-only Futures
            Tif::Gtc      => "GTC",
            Tif::Ioc      => "IOC",
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

        let res = self.common.client
            .post(url)
            .header("X-MBX-APIKEY", &self.common.api_key)
            .send().await?
            .error_for_status()?
            .text().await?;

        info!(%sym, ?side, %price, %qty, tif = ?tif, "futures place_limit ok");
        Ok(res)
    }

    async fn cancel(&self, order_id: &str, sym: &str) -> Result<()> {
        let qs = format!(
            "symbol={}&orderId={}&timestamp={}&recvWindow={}",
            sym, order_id, BinanceCommon::ts(), self.common.recv_window_ms
        );
        let sig = self.common.sign(&qs);
        let url = format!("{}/fapi/v1/order?{}&signature={}", self.base, qs, sig);

        self.common.client
            .delete(url)
            .header("X-MBX-APIKEY", &self.common.api_key)
            .send().await?
            .error_for_status()?;
        Ok(())
    }

    async fn best_prices(&self, sym: &str) -> Result<(Px, Px)> {
        let url = format!("{}/fapi/v1/depth?symbol={}&limit=5", self.base, encode(sym));
        let d: OrderBookTop = self.common.client.get(url).send().await?.error_for_status()?.json().await?;
        use rust_decimal::Decimal;
        let best_bid = d.bids.get(0).ok_or_else(|| anyhow!("no bid"))?.0.clone();
        let best_ask = d.asks.get(0).ok_or_else(|| anyhow!("no ask"))?.0.clone();
        Ok((Px(Decimal::from_str_radix(&best_bid, 10)?), Px(Decimal::from_str_radix(&best_ask, 10)?)))
    }
}

// ---- helpers ----

fn quantize(x: f64, step: f64) -> f64 {
    if step <= 0.0 { return x; }
    (x / step).floor() * step
}
