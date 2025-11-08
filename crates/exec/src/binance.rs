//location: /crates/exec/src/binance.rs
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use bot_core::types::*;
use dashmap::DashMap;
use hmac::{Hmac, Mac};
use once_cell::sync::Lazy;
use reqwest::{Client, RequestBuilder, Response};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::{Decimal, RoundingStrategy};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use sha2::Sha256;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{info, warn};
use urlencoding::encode;

use super::{Venue, VenueOrder};

#[derive(Clone, Debug)]
pub struct SymbolRules {
    pub tick_size: Decimal,
    pub step_size: Decimal,
    pub price_precision: usize,
    pub qty_precision: usize,
    pub min_notional: Decimal,
}

#[derive(Deserialize)]
#[serde(tag = "filterType")]
#[allow(non_snake_case)]
enum FutFilter {
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
    #[serde(default)]
    filters: Vec<FutFilter>,
    #[serde(rename = "pricePrecision", default)]
    price_precision: Option<usize>,
    #[serde(rename = "quantityPrecision", default)]
    qty_precision: Option<usize>,
}

static FUT_RULES: Lazy<DashMap<String, Arc<SymbolRules>>> = Lazy::new(|| DashMap::new());

fn str_dec<S: AsRef<str>>(s: S) -> Decimal {
    Decimal::from_str_radix(s.as_ref(), 10).unwrap_or(Decimal::ZERO)
}

fn scale_from_step(step: Decimal) -> usize {
    if step.is_zero() {
        return 8; // Default precision
    }
    // Eğer step 1 veya daha büyükse, precision 0 olmalı
    if step >= Decimal::ONE {
        return 0;
    }
    // tick_size veya step_size'dan precision hesapla
    // Decimal'in scale() metodu internal scale'i döner (trailing zero'lar dahil)
    // Bu bizim için doğru precision'ı verir
    let scale = step.scale() as usize;
    scale
}

fn rules_from_fut_symbol(sym: FutExchangeSymbol) -> SymbolRules {
    let mut tick = Decimal::ZERO;
    let mut step = Decimal::ZERO;
    let mut min_notional = Decimal::ZERO;

    for f in sym.filters {
        match f {
            FutFilter::PriceFilter { tickSize } => tick = str_dec(tickSize),
            FutFilter::LotSize { stepSize } => step = str_dec(stepSize),
            FutFilter::MinNotional { notional } => min_notional = str_dec(notional),
            FutFilter::Other => {}
        }
    }

    let p_prec = sym.price_precision.unwrap_or_else(|| scale_from_step(tick));
    let q_prec = sym.qty_precision.unwrap_or_else(|| scale_from_step(step));

    SymbolRules {
        tick_size: if tick.is_zero() {
            Decimal::new(1, 2)
        } else {
            tick
        },
        step_size: if step.is_zero() {
            Decimal::new(1, 3)
        } else {
            step
        },
        price_precision: p_prec,
        qty_precision: q_prec,
        min_notional,
    }
}

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
        // SystemTime::now() her zaman UNIX_EPOCH'den sonra olduğu için unwrap güvenlidir
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time is before UNIX epoch")
            .as_millis() as u64
    }
    fn sign(&self, qs: &str) -> String {
        // secret_key boş olsa bile new_from_slice başarılı olur (boş key ile imza üretir)
        // Ancak yine de expect ile açık hale getiriyoruz
        let mut mac = Hmac::<Sha256>::new_from_slice(self.secret_key.as_bytes())
            .expect("HMAC key initialization failed");
        mac.update(qs.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }
}

// ---- USDT-M Futures ----

#[derive(Clone)]
pub struct BinanceFutures {
    pub base: String, // e.g. https://fapi.binance.com
    pub common: BinanceCommon,
    pub price_tick: Decimal,
    pub qty_step: Decimal,
    pub price_precision: usize,
    pub qty_precision: usize,
}

#[derive(Deserialize)]
struct OrderBookTop {
    bids: Vec<(String, String)>,
    asks: Vec<(String, String)>,
}

#[derive(Deserialize)]
struct FutPlacedOrder {
    #[serde(rename = "orderId")]
    order_id: u64,
    #[serde(rename = "clientOrderId")]
    client_order_id: Option<String>,
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
    #[serde(rename = "lastFundingRate")]
    #[serde(default)]
    last_funding_rate: Option<String>,
    #[serde(rename = "nextFundingTime")]
    #[serde(default)]
    next_funding_time: Option<u64>,
}

impl BinanceFutures {
    /// Per-symbol metadata (tick_size, step_size) alır, fallback olarak global değerleri kullanır
    pub async fn rules_for(&self, sym: &str) -> Result<Arc<SymbolRules>> {
        if let Some(r) = FUT_RULES.get(sym) {
            return Ok(r.clone());
        }

        let url = format!("{}/fapi/v1/exchangeInfo?symbol={}", self.base, encode(sym));
        match send_json::<FutExchangeInfo>(self.common.client.get(url)).await {
            Ok(info) => {
                let sym_rec = info
                    .symbols
                    .into_iter()
                    .next()
                    .ok_or_else(|| anyhow!("symbol info missing"))?;
                let rules = Arc::new(rules_from_fut_symbol(sym_rec));
                FUT_RULES.insert(sym.to_string(), rules.clone());
                Ok(rules)
            }
            Err(err) => {
                warn!(error = ?err, %sym, "failed to fetch futures symbol rules, using fallbacks");
                Ok(Arc::new(SymbolRules {
                    tick_size: self.price_tick,
                    step_size: self.qty_step,
                    price_precision: self.price_precision,
                    qty_precision: self.qty_precision,
                    min_notional: Decimal::ZERO,
                }))
            }
        }
    }

    pub async fn symbol_assets(&self, sym: &str) -> Result<(String, String)> {
        let url = format!("{}/fapi/v1/exchangeInfo?symbol={}", self.base, encode(sym));
        let info: FutExchangeInfo = send_json(self.common.client.get(url)).await?;
        let sym = info
            .symbols
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("symbol info missing"))?;
        Ok((sym.base_asset, sym.quote_asset))
    }

    pub async fn symbol_metadata(&self) -> Result<Vec<SymbolMeta>> {
        let url = format!("{}/fapi/v1/exchangeInfo", self.base);
        let info: FutExchangeInfo = send_json(self.common.client.get(url)).await?;
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
        let balances: Vec<FutBalance> = send_json(
            self.common
                .client
                .get(url)
                .header("X-MBX-APIKEY", &self.common.api_key),
        )
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
        let orders: Vec<FutOpenOrder> = send_json(
            self.common
                .client
                .get(url)
                .header("X-MBX-APIKEY", &self.common.api_key),
        )
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
        let mut positions: Vec<FutPosition> = send_json(
            self.common
                .client
                .get(url)
                .header("X-MBX-APIKEY", &self.common.api_key),
        )
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

    pub async fn fetch_premium_index(&self, sym: &str) -> Result<(Px, Option<f64>, Option<u64>)> {
        let url = format!("{}/fapi/v1/premiumIndex?symbol={}", self.base, sym);
        let premium: PremiumIndex = send_json(self.common.client.get(url)).await?;
        let mark = Decimal::from_str_radix(&premium.mark_price, 10)?;
        let funding_rate = premium
            .last_funding_rate
            .as_deref()
            .and_then(|rate| rate.parse::<f64>().ok());
        let next_time = premium.next_funding_time.filter(|ts| *ts > 0);
        Ok((Px(mark), funding_rate, next_time))
    }

    pub async fn fetch_mark_price(&self, sym: &str) -> Result<Px> {
        let (mark, _, _) = self.fetch_premium_index(sym).await?;
        Ok(mark)
    }

    /// Close position with reduceOnly guarantee and verification
    ///
    /// KRİTİK: Futures için pozisyon kapatma garantisi:
    /// 1. reduceOnly=true ile market order gönder
    /// 2. Pozisyon tam olarak kapatıldığını doğrula
    /// 3. Kısmi kapatma durumunda retry yap
    /// 4. Leverage ile uyumlu olduğundan emin ol
    pub async fn flatten_position(&self, sym: &str) -> Result<()> {
        // İlk pozisyon kontrolü
        let initial_pos = self.fetch_position(sym).await?;
        let initial_qty = initial_pos.qty.0;

        if initial_qty.is_zero() {
            // Pozisyon zaten kapalı
            return Ok(());
        }

        let initial_side = if initial_qty.is_sign_positive() {
            Side::Sell // Long pozisyon → Sell ile kapat
        } else {
            Side::Buy // Short pozisyon → Buy ile kapat
        };

        let rules = self.rules_for(sym).await?;
        let initial_qty_abs = quantize_decimal(initial_qty.abs(), rules.step_size);

        if initial_qty_abs <= Decimal::ZERO {
            warn!(
                symbol = %sym,
                original_qty = %initial_qty,
                "quantized position size is zero, skipping close"
            );
            return Ok(());
        }

        // KRİTİK: Pozisyon kapatma retry mekanizması (kısmi kapatma durumunda)
        let max_attempts = 3;

        for attempt in 0..max_attempts {
            // Mevcut pozisyonu kontrol et (retry durumunda pozisyon değişmiş olabilir)
            let current_pos = self.fetch_position(sym).await?;
            let current_qty = current_pos.qty.0;

            if current_qty.is_zero() {
                // Pozisyon tamamen kapatıldı
                if attempt > 0 {
                    info!(
                        symbol = %sym,
                        attempts = attempt + 1,
                        "position fully closed after retry"
                    );
                }
                return Ok(());
            }

            // Kalan pozisyon miktarını hesapla
            let remaining_qty = quantize_decimal(current_qty.abs(), rules.step_size);

            if remaining_qty <= Decimal::ZERO {
                return Ok(());
            }

            // Side belirleme (pozisyon yönüne göre)
            let side = if current_qty.is_sign_positive() {
                Side::Sell // Long → Sell
            } else {
                Side::Buy // Short → Buy
            };

            let qty_str = format_decimal_fixed(remaining_qty, rules.qty_precision);

            // KRİTİK: reduceOnly=true ve type=MARKET garantisi
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
                "type=MARKET".to_string(), // Post-only değil, market order
                format!("quantity={}", qty_str),
                "reduceOnly=true".to_string(), // KRİTİK: Yeni pozisyon açmayı önle
                format!("timestamp={}", BinanceCommon::ts()),
                format!("recvWindow={}", self.common.recv_window_ms),
            ];

            let qs = params.join("&");
            let sig = self.common.sign(&qs);
            let url = format!("{}/fapi/v1/order?{}&signature={}", self.base, qs, sig);

            // Emir gönder
            match send_void(
                self.common
                    .client
                    .post(&url)
                    .header("X-MBX-APIKEY", &self.common.api_key),
            )
            .await
            {
                Ok(_) => {
                    // KRİTİK DÜZELTME: Exchange'in işlemesi için bekleme eklendi
                    // Market order gönderildikten sonra exchange'in işlemesi için zaman gerekir
                    // Hemen kontrol etmek yanlış sonuçlara yol açabilir (pozisyon henüz kapanmamış olabilir)
                    tokio::time::sleep(Duration::from_millis(500)).await; // Exchange işlemesi için 500ms bekle

                    let verify_pos = self.fetch_position(sym).await?;
                    let verify_qty = verify_pos.qty.0;

                    if verify_qty.is_zero() {
                        // Pozisyon tamamen kapatıldı
                        info!(
                            symbol = %sym,
                            attempt = attempt + 1,
                            initial_qty = %initial_qty,
                            "position fully closed and verified"
                        );
                        return Ok(());
                    } else {
                        // Kısmi kapatma - retry yap
                        let remaining_pct = (verify_qty.abs() / initial_qty.abs()
                            * Decimal::from(100))
                        .to_f64()
                        .unwrap_or(0.0);

                        warn!(
                            symbol = %sym,
                            attempt = attempt + 1,
                            initial_qty = %initial_qty,
                            remaining_qty = %verify_qty,
                            remaining_pct = remaining_pct,
                            "position partially closed, retrying..."
                        );

                        if attempt < max_attempts - 1 {
                            // Son deneme değilse devam et
                            continue;
                        } else {
                            // Son denemede hala pozisyon varsa hata döndür
                            return Err(anyhow::anyhow!(
                                "Failed to fully close position after {} attempts. Initial: {}, Remaining: {}",
                                max_attempts,
                                initial_qty,
                                verify_qty
                            ));
                        }
                    }
                }
                Err(e) => {
                    if attempt < max_attempts - 1 {
                        warn!(
                            symbol = %sym,
                            attempt = attempt + 1,
                            error = %e,
                            "failed to close position, retrying..."
                        );
                        // KRİTİK DÜZELTME: Retry öncesi bekleme eklendi
                        // Hızlı retry'ler exchange'i overload edebilir
                        tokio::time::sleep(Duration::from_millis(500)).await; // Retry öncesi 500ms bekle
                        continue;
                    } else {
                        return Err(e);
                    }
                }
            }
        }

        // Buraya gelmemeli (yukarıdaki return'ler ile çıkılmalı)
        Err(anyhow::anyhow!("Unexpected error in flatten_position"))
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
        // Futures için clientOrderId olmadan eski davranış
        self.place_limit_with_client_id(sym, side, px, qty, tif, "").await.map(|(order_id, _)| order_id)
    }
    
    async fn place_limit_with_client_id(
        &self,
        sym: &str,
        side: Side,
        px: Px,
        qty: Qty,
        tif: Tif,
        client_order_id: &str,
    ) -> Result<(String, Option<String>)> {
        let s_side = match side {
            Side::Buy => "BUY",
            Side::Sell => "SELL",
        };
        let tif_str = match tif {
            Tif::PostOnly => "GTX",
            Tif::Gtc => "GTC",
            Tif::Ioc => "IOC",
        };

        let rules = self.rules_for(sym).await?;

        // KRİTİK DÜZELTME: Precision'ı önce hesapla, sonra quantize et
        // Precision'ı tick_size ve step_size ile uyumlu hale getir
        // Exchange'in verdiği precision yanlış olabilir, bu yüzden tick_size'dan hesaplanan precision'ı öncelikli kullan
        let price_prec_from_tick = scale_from_step(rules.tick_size);
        let qty_prec_from_step = scale_from_step(rules.step_size);
        // tick_size'dan hesaplanan precision'ı kullan (exchange'in verdiği precision yanlış olabilir)
        // Ama eğer exchange'in verdiği precision daha küçükse, onu kullan (daha güvenli)
        let price_precision = price_prec_from_tick.min(rules.price_precision);
        let qty_precision = qty_prec_from_step.min(rules.qty_precision);

        // Quantize: step_size'a göre floor
        let price_quantized = quantize_decimal(px.0, rules.tick_size);
        let qty_quantized = quantize_decimal(qty.0.abs(), rules.step_size);

        // KRİTİK: Quantize sonrası precision'a göre normalize et (internal precision'ı temizle)
        // Bu, "Precision is over the maximum" hatasını önler
        let price = price_quantized
            .round_dp_with_strategy(price_precision as u32, RoundingStrategy::ToZero);
        let qty =
            qty_quantized.round_dp_with_strategy(qty_precision as u32, RoundingStrategy::ToZero);

        let notional = price * qty;
        if !rules.min_notional.is_zero() && notional < rules.min_notional {
            return Err(anyhow!(
                "below min notional after clamps ({} < {})",
                notional,
                rules.min_notional
            ));
        }

        // Format: precision'a göre string'e çevir
        let price_str = format_decimal_fixed(price, price_precision);
        let qty_str = format_decimal_fixed(qty, qty_precision);

        let mut params = vec![
            format!("symbol={}", sym),
            format!("side={}", s_side),
            "type=LIMIT".to_string(),
            format!("timeInForce={}", tif_str),
            format!("price={}", price_str),
            format!("quantity={}", qty_str),
            format!("timestamp={}", BinanceCommon::ts()),
            format!("recvWindow={}", self.common.recv_window_ms),
            "newOrderRespType=RESULT".to_string(),
        ];
        
        // ClientOrderId ekle (idempotency için) - sadece boş değilse
        if !client_order_id.is_empty() {
            // Binance: max 36 karakter, alphanumeric
            if client_order_id.len() <= 36 && client_order_id.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
                params.push(format!("newClientOrderId={}", client_order_id));
            } else {
                warn!(
                    %sym,
                    client_order_id = client_order_id,
                    "invalid clientOrderId format (max 36 chars, alphanumeric), skipping"
                );
            }
        }
        let qs = params.join("&");
        let sig = self.common.sign(&qs);
        let url = format!("{}/fapi/v1/order?{}&signature={}", self.base, qs, sig);

        let order: FutPlacedOrder = send_json(
            self.common
                .client
                .post(url)
                .header("X-MBX-APIKEY", &self.common.api_key),
        )
        .await?;

        info!(
            %sym,
            ?side,
            price = %price,
            qty = %qty,
            tif = ?tif,
            order_id = order.order_id,
            client_order_id = ?order.client_order_id,
            "futures place_limit ok"
        );
        Ok((order.order_id.to_string(), order.client_order_id))
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

        send_void(
            self.common
                .client
                .delete(url)
                .header("X-MBX-APIKEY", &self.common.api_key),
        )
        .await?;
        Ok(())
    }

    async fn best_prices(&self, sym: &str) -> Result<(Px, Px)> {
        let url = format!("{}/fapi/v1/depth?symbol={}&limit=5", self.base, encode(sym));
        let d: OrderBookTop = send_json(self.common.client.get(url)).await?;
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

pub fn quantize_decimal(value: Decimal, step: Decimal) -> Decimal {
    // KRİTİK DÜZELTME: Edge case'ler için ek kontroller
    if step.is_zero() || step.is_sign_negative() {
        return value;
    }

    let ratio = value / step;
    let floored = ratio.floor();
    let result = floored * step;

    // Decimal her zaman finite'dir, bu yüzden direkt döndür
    result
}

fn format_decimal_fixed(value: Decimal, precision: usize) -> String {
    // KRİTİK DÜZELTME: Edge case'ler için ek kontroller
    // Precision overflow kontrolü (max 28 decimal places)
    let precision = precision.min(28);
    let scale = precision as u32;

    // Decimal her zaman finite'dir, bu yüzden direkt işle

    // ÖNEMLİ: Precision hatasını önlemek için önce quantize, sonra format
    // normalize() trailing zero'ları kaldırır, bu precision hatasına yol açabilir
    // Bu yüzden trailing zero'ları korumalıyız
    // ÖNCE: Decimal'i doğru scale'e truncate et (precision hatasını önlemek için)
    // Her zaman round_dp_with_strategy kullan çünkü set_scale güvenilir değil
    let truncated = value.round_dp_with_strategy(scale, RoundingStrategy::ToZero);

    // Trailing zero'ları koru: precision kadar decimal place göster
    if scale == 0 {
        truncated.to_string()
    } else {
        let s = truncated.to_string();
        if let Some(dot_pos) = s.find('.') {
            let current_decimals = s.len() - dot_pos - 1;
            if current_decimals < scale as usize {
                // Eksik trailing zero'ları ekle
                format!("{}{}", s, "0".repeat(scale as usize - current_decimals))
            } else if current_decimals > scale as usize {
                // Fazla decimal varsa kes (precision hatasını önle)
                // String'i kes - kesinlikle precision'dan fazla basamak gösterme
                let integer_part = &s[..dot_pos];
                let decimal_part = &s[dot_pos + 1..];
                let truncated_decimal = &decimal_part[..scale as usize];
                format!("{}.{}", integer_part, truncated_decimal)
            } else {
                s
            }
        } else {
            // Nokta yoksa ekle ve trailing zero ekle
            format!("{}.{}", s, "0".repeat(scale as usize))
        }
    }
}

async fn ensure_success(resp: Response) -> Result<Response> {
    let status = resp.status();
    if status.is_success() {
        Ok(resp)
    } else {
        let body = resp.text().await.unwrap_or_default();
        tracing::error!(%status, %body, "binance api error");
        Err(anyhow!("binance api error: {} - {}", status, body))
    }
}

async fn send_json<T>(builder: RequestBuilder) -> Result<T>
where
    T: DeserializeOwned,
{
    let resp = builder.send().await?;
    let resp = ensure_success(resp).await?;
    Ok(resp.json().await?)
}

async fn send_void(builder: RequestBuilder) -> Result<()> {
    let resp = builder.send().await?;
    ensure_success(resp).await?;
    Ok(())
}

#[allow(dead_code)]
fn quantize_f64(x: f64, step: f64) -> f64 {
    if step <= 0.0 || !x.is_finite() || !step.is_finite() {
        return x;
    }
    (x / step).floor() * step
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_quantize_price() {
        let price = dec!(0.2593620616072499999728690579);
        let tick = dec!(0.001);
        let result = quantize_decimal(price, tick);
        assert_eq!(result, dec!(0.259));

        let result = quantize_decimal(price, dec!(0.1));
        assert_eq!(result, dec!(0.2));
    }

    #[test]
    fn test_quantize_qty() {
        let qty = dec!(76.4964620386307103672152152);
        let step = dec!(0.001);
        let result = quantize_decimal(qty, step);
        assert_eq!(result, dec!(76.496));
    }

    #[test]
    fn test_quantize_f64() {
        assert_eq!(quantize_f64(0.2593, 0.1), 0.2);
        assert_eq!(quantize_f64(76.4964, 0.001), 76.496);
    }

    #[test]
    fn test_format_decimal_fixed() {
        assert_eq!(format_decimal_fixed(dec!(0.123456), 3), "0.123");
        assert_eq!(format_decimal_fixed(dec!(5), 0), "5");
        // format_decimal_fixed trailing zero'ları korur (precision kadar)
        assert_eq!(format_decimal_fixed(dec!(1.2000), 4), "1.2000");
        assert_eq!(format_decimal_fixed(dec!(0.00000001), 8), "0.00000001");

        // Yüksek fiyatlı semboller için testler (BNBUSDC gibi)
        assert_eq!(format_decimal_fixed(dec!(950.649470), 2), "950.64");
        assert_eq!(format_decimal_fixed(dec!(950.649470), 3), "950.649");
        assert_eq!(format_decimal_fixed(dec!(956.370530), 2), "956.37");
        assert_eq!(format_decimal_fixed(dec!(956.370530), 3), "956.370");

        // Fazla precision'ı kesme testi
        assert_eq!(format_decimal_fixed(dec!(202.129776525), 2), "202.12");
        assert_eq!(format_decimal_fixed(dec!(202.129776525), 3), "202.129");
        assert_eq!(format_decimal_fixed(dec!(0.08082180550260300), 4), "0.0808");
        assert_eq!(
            format_decimal_fixed(dec!(0.08082180550260300), 5),
            "0.08082"
        );

        // Integer precision testi
        assert_eq!(format_decimal_fixed(dec!(100.5), 0), "100");
        assert_eq!(format_decimal_fixed(dec!(1000), 0), "1000");
    }

    #[test]
    fn test_scale_from_step() {
        // tick_size'dan precision hesaplama testleri
        assert_eq!(scale_from_step(dec!(0.1)), 1);
        assert_eq!(scale_from_step(dec!(0.01)), 2);
        assert_eq!(scale_from_step(dec!(0.001)), 3);
        assert_eq!(scale_from_step(dec!(0.0001)), 4);
        assert_eq!(scale_from_step(dec!(1)), 0);
        assert_eq!(scale_from_step(dec!(10)), 0);
        assert_eq!(scale_from_step(dec!(0.000001)), 6);
    }
}
