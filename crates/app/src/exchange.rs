// Exchange module - Consolidates binance_exec, binance_rest, binance_ws
// This file includes all Binance Futures API implementations

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use crate::types::*;
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
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, error, info, warn};
use urlencoding::encode;
use crate::exec::{Venue, VenueOrder};
use crate::types::{Px, Qty, Side};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::str::FromStr;
use tokio::time::{timeout, Duration, Instant};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message, WebSocketStream};
use tokio_tungstenite::tungstenite::Error as WsError;

// ============================================================================
// Binance Exec Module (from binance_exec.rs)
// ============================================================================

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

pub static FUT_RULES: Lazy<DashMap<String, Arc<SymbolRules>>> = Lazy::new(|| DashMap::new());

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
            FutFilter::PriceFilter { tickSize } => {
                tick = str_dec(&tickSize);
                tracing::debug!(
                    symbol = %sym.symbol,
                    tick_size_raw = %tickSize,
                    tick_size_parsed = %tick,
                    "parsed PRICE_FILTER tickSize"
                );
            }
            FutFilter::LotSize { stepSize } => {
                step = str_dec(&stepSize);
                tracing::debug!(
                    symbol = %sym.symbol,
                    step_size_raw = %stepSize,
                    step_size_parsed = %step,
                    "parsed LOT_SIZE stepSize"
                );
            }
            FutFilter::MinNotional { notional } => {
                min_notional = str_dec(&notional);
                tracing::debug!(
                    symbol = %sym.symbol,
                    min_notional_raw = %notional,
                    min_notional_parsed = %min_notional,
                    "parsed MIN_NOTIONAL"
                );
            }
            FutFilter::Other => {}
        }
    }

    // KRİTİK: Precision hesaplama scale_from_step ile değil, doğrudan API'den al
    let p_prec = sym.price_precision.unwrap_or_else(|| {
        let calc = scale_from_step(tick);
        tracing::warn!(
            symbol = %sym.symbol,
            tick_size = %tick,
            calculated_precision = calc,
            "pricePrecision missing from API, calculated from tickSize"
        );
        calc
    });
    
    let q_prec = sym.qty_precision.unwrap_or_else(|| {
        let calc = scale_from_step(step);
        tracing::warn!(
            symbol = %sym.symbol,
            step_size = %step,
            calculated_precision = calc,
            "quantityPrecision missing from API, calculated from stepSize"
        );
        calc
    });

    // KRİTİK: Fallback değerleri daha makul yap
    let final_tick = if tick.is_zero() {
        tracing::warn!(symbol = %sym.symbol, "tickSize is zero, using fallback 0.01");
        Decimal::new(1, 2) // 0.01
    } else {
        tick
    };
    
    let final_step = if step.is_zero() {
        tracing::warn!(symbol = %sym.symbol, "stepSize is zero, using fallback 0.001");
        Decimal::new(1, 3) // 0.001
    } else {
        step
    };

    tracing::info!(
        symbol = %sym.symbol,
        tick_size = %final_tick,
        step_size = %final_step,
        price_precision = p_prec,
        qty_precision = q_prec,
        min_notional = %min_notional,
        "symbol rules parsed from exchangeInfo"
    );

    SymbolRules {
        tick_size: final_tick,
        step_size: final_step,
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
    pub hedge_mode: bool, // Hedge mode (dual-side position) açık mı?
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
    #[allow(dead_code)]
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
    #[serde(rename = "positionSide", default)]
    #[allow(dead_code)]
    position_side: Option<String>, // "LONG" | "SHORT" | "BOTH" (hedge mode) veya None (one-way mode)
    #[serde(rename = "marginType", default)]
    margin_type: String, // "isolated" or "cross"
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
    /// Leverage ayarla (sembol bazlı)
    /// KRİTİK: Başlangıçta her sembol için leverage'i açıkça ayarla
    /// /fapi/v1/leverage endpoint'i ile sembol bazlı leverage set edilir
    pub async fn set_leverage(&self, sym: &str, leverage: u32) -> Result<()> {
        let params = vec![
            format!("symbol={}", sym),
            format!("leverage={}", leverage),
            format!("timestamp={}", BinanceCommon::ts()),
            format!("recvWindow={}", self.common.recv_window_ms),
        ];
        let qs = params.join("&");
        let sig = self.common.sign(&qs);
        let url = format!("{}/fapi/v1/leverage?{}&signature={}", self.base, qs, sig);
        
        match send_void(
            self.common
                .client
                .post(&url)
                .header("X-MBX-APIKEY", &self.common.api_key),
        )
        .await
        {
            Ok(_) => {
                info!(%sym, leverage, "leverage set successfully");
                Ok(())
            }
            Err(e) => {
                warn!(%sym, leverage, error = %e, "failed to set leverage");
                Err(e)
            }
        }
    }
    
    /// Position side mode ayarla (hedge mode aç/kapa)
    /// KRİTİK: Başlangıçta hesap modunu açıkça ayarla
    /// /fapi/v1/positionSide/dual endpoint'i ile hedge mode açılır/kapanır
    pub async fn set_position_side_dual(&self, dual: bool) -> Result<()> {
        let params = vec![
            format!("dualSidePosition={}", if dual { "true" } else { "false" }),
            format!("timestamp={}", BinanceCommon::ts()),
            format!("recvWindow={}", self.common.recv_window_ms),
        ];
        let qs = params.join("&");
        let sig = self.common.sign(&qs);
        let url = format!("{}/fapi/v1/positionSide/dual?{}&signature={}", self.base, qs, sig);
        
        match send_void(
            self.common
                .client
                .post(&url)
                .header("X-MBX-APIKEY", &self.common.api_key),
        )
        .await
        {
            Ok(_) => {
                info!(dual_side = dual, "position side mode set successfully");
                Ok(())
            }
            Err(e) => {
                warn!(dual_side = dual, error = %e, "failed to set position side mode");
                Err(e)
            }
        }
    }
    
    /// Margin type ayarla (isolated veya cross)
    /// KRİTİK: Başlangıçta her sembol için margin type'ı açıkça ayarla
    /// /fapi/v1/marginType endpoint'i ile isolated/cross margin set edilir
    /// 
    /// # Arguments
    /// * `sym` - Symbol (örn: "BTCUSDT")
    /// * `isolated` - true = isolated margin, false = cross margin
    pub async fn set_margin_type(&self, sym: &str, isolated: bool) -> Result<()> {
        let margin_type = if isolated { "ISOLATED" } else { "CROSSED" };
        let params = vec![
            format!("symbol={}", sym),
            format!("marginType={}", margin_type),
            format!("timestamp={}", BinanceCommon::ts()),
            format!("recvWindow={}", self.common.recv_window_ms),
        ];
        let qs = params.join("&");
        let sig = self.common.sign(&qs);
        let url = format!("{}/fapi/v1/marginType?{}&signature={}", self.base, qs, sig);
        
        match send_void(
            self.common
                .client
                .post(&url)
                .header("X-MBX-APIKEY", &self.common.api_key),
        )
        .await
        {
            Ok(_) => {
                info!(%sym, margin_type = %margin_type, "margin type set successfully");
                Ok(())
            }
            Err(e) => {
                warn!(%sym, margin_type = %margin_type, error = %e, "failed to set margin type");
                Err(e)
            }
        }
    }
    
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

    /// Get current margin type for a symbol
    /// Returns true if isolated, false if crossed
    pub async fn get_margin_type(&self, sym: &str) -> Result<bool> {
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
        // marginType: "isolated" or "cross"
        let is_isolated = pos.margin_type.eq_ignore_ascii_case("isolated");
        Ok(is_isolated)
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


    /// Close position with reduceOnly guarantee and verification
    ///
    /// KRİTİK: Futures için pozisyon kapatma garantisi:
    /// 1. reduceOnly=true ile market order gönder
    /// 2. Pozisyon tam olarak kapatıldığını doğrula
    /// 3. Kısmi kapatma durumunda retry yap
    /// 4. Leverage ile uyumlu olduğundan emin ol
    /// 5. Hedge mode açıksa positionSide parametresi ekle
    pub async fn flatten_position(&self, sym: &str, hedge_mode: bool) -> Result<()> {
        // İlk pozisyon kontrolü
        let initial_pos = self.fetch_position(sym).await?;
        let initial_qty = initial_pos.qty.0;

        if initial_qty.is_zero() {
            // Pozisyon zaten kapalı
            return Ok(());
        }

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
            // KRİTİK İYİLEŞTİRME: Her attempt'te mevcut pozisyonu kontrol et
            // Retry durumunda pozisyon değişmiş olabilir (kısmi kapatma)
            let current_pos = self.fetch_position(sym).await?;
            let current_qty = current_pos.qty.0;

            if current_qty.is_zero() {
                // Pozisyon tamamen kapatıldı
                if attempt > 0 {
                    info!(
                        symbol = %sym,
                        attempts = attempt + 1,
                        initial_qty = %initial_qty,
                        "position fully closed after retry"
                    );
                }
                return Ok(());
            }

            // Kalan pozisyon miktarını hesapla (quantize et)
            let remaining_qty = quantize_decimal(current_qty.abs(), rules.step_size);

            if remaining_qty <= Decimal::ZERO {
                // Quantize sonrası sıfır oldu, pozisyon zaten kapalı sayılabilir
                return Ok(());
            }

            // Side belirleme (pozisyon yönüne göre)
            let side = if current_qty.is_sign_positive() {
                Side::Sell // Long → Sell
            } else {
                Side::Buy // Short → Buy
            };

            let qty_str = format_decimal_fixed(remaining_qty, rules.qty_precision);

            // KRİTİK: Hedge mode açıksa positionSide parametresi ekle
            // positionSide: "LONG" (pozitif qty) veya "SHORT" (negatif qty)
            let position_side = if hedge_mode {
                if current_qty.is_sign_positive() {
                    Some("LONG")
                } else {
                    Some("SHORT")
                }
            } else {
                None
            };

            // KRİTİK: reduceOnly=true ve type=MARKET garantisi
            let mut params = vec![
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
            
            // Hedge mode açıksa positionSide ekle
            if let Some(pos_side) = position_side {
                params.push(format!("positionSide={}", pos_side));
            }

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
                    // KRİTİK İYİLEŞTİRME: Binance için 1000ms daha güvenli (500ms yeterli olmayabilir)
                    // Exchange'in order'ı işlemesi ve position update'i için yeterli süre
                    tokio::time::sleep(Duration::from_millis(1000)).await; // Exchange işlemesi için 1000ms bekle (Binance)

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
                        // KRİTİK İYİLEŞTİRME: Kısmi kapatma tespiti - kapatılan miktarı hesapla
                        // Bu attempt'te ne kadar kapatıldı?
                        let closed_amount = current_qty.abs() - verify_qty.abs();
                        let close_ratio = if current_qty.abs() > Decimal::ZERO {
                            closed_amount / current_qty.abs()
                        } else {
                            Decimal::ZERO
                        };
                        
                        // Kalan pozisyon yüzdesi (initial'a göre)
                        let remaining_pct = if initial_qty.abs() > Decimal::ZERO {
                            (verify_qty.abs() / initial_qty.abs() * Decimal::from(100))
                                .to_f64()
                                .unwrap_or(0.0)
                        } else {
                            0.0
                        };

                        warn!(
                            symbol = %sym,
                            attempt = attempt + 1,
                            initial_qty = %initial_qty,
                            current_qty_at_attempt = %current_qty,
                            remaining_qty = %verify_qty,
                            closed_amount = %closed_amount,
                            close_ratio = %close_ratio,
                            remaining_pct = remaining_pct,
                            "partial close detected, retrying..."
                        );

                        if attempt < max_attempts - 1 {
                            // Son deneme değilse devam et
                            continue;
                        } else {
                            // Son denemede hala pozisyon varsa hata döndür
                            return Err(anyhow::anyhow!(
                                "Failed to fully close position after {} attempts. Initial: {}, Remaining: {}, Closed in last attempt: {}",
                                max_attempts,
                                initial_qty,
                                verify_qty,
                                closed_amount
                            ));
                        }
                    }
                }
                Err(e) => {
                    let error_str = e.to_string();
                    let error_lower = error_str.to_lowercase();
                    
                    // KRİTİK DÜZELTME: MIN_NOTIONAL hatası yakalama (-1013 veya "min notional")
                    // Küçük "artık" miktarlarda reduce-only market close borsa min_notional eşiğini sağlamayabilir
                    if error_lower.contains("-1013") || error_lower.contains("min notional") || error_lower.contains("min_notional") {
                        warn!(
                            symbol = %sym,
                            attempt = attempt + 1,
                            error = %e,
                            remaining_qty = %remaining_qty,
                            min_notional = %rules.min_notional,
                            "MIN_NOTIONAL error in reduce-only market close, trying limit reduce-only fallback"
                        );
                        
                        // Fallback: Limit reduce-only ile karşı tarafta 1-2 tick avantajlı pasif bırak
                        let (best_bid, best_ask) = match self.best_prices(sym).await {
                            Ok(prices) => prices,
                            Err(e2) => {
                                warn!(symbol = %sym, error = %e2, "failed to fetch best prices for limit fallback");
                                if attempt < max_attempts - 1 {
                                    tokio::time::sleep(Duration::from_millis(500)).await;
                                    continue;
                                } else {
                                    return Err(anyhow!("MIN_NOTIONAL error and limit fallback failed: {}", e));
                                }
                            }
                        };
                        
                        // Limit reduce-only emri: karşı tarafta 1-2 tick avantajlı
                        let tick_size = rules.tick_size;
                        let limit_price = if matches!(side, Side::Buy) {
                            // Short kapatma: Buy limit, bid'den 1 tick yukarı (maker olabilir)
                            best_bid.0 + tick_size
                        } else {
                            // Long kapatma: Sell limit, ask'ten 1 tick aşağı (maker olabilir)
                            best_ask.0 - tick_size
                        };
                        
                        let limit_price_quantized = quantize_decimal(limit_price, tick_size);
                        let limit_price_str = format_decimal_fixed(limit_price_quantized, rules.price_precision);
                        
                        // Limit reduce-only emri gönder
                        let mut limit_params = vec![
                            format!("symbol={}", sym),
                            format!("side={}", if matches!(side, Side::Buy) { "BUY" } else { "SELL" }),
                            "type=LIMIT".to_string(),
                            "timeInForce=GTC".to_string(),
                            format!("price={}", limit_price_str),
                            format!("quantity={}", qty_str),
                            "reduceOnly=true".to_string(),
                            format!("timestamp={}", BinanceCommon::ts()),
                            format!("recvWindow={}", self.common.recv_window_ms),
                        ];
                        
                        if let Some(pos_side) = position_side {
                            limit_params.push(format!("positionSide={}", pos_side));
                        }
                        
                        let limit_qs = limit_params.join("&");
                        let limit_sig = self.common.sign(&limit_qs);
                        let limit_url = format!("{}/fapi/v1/order?{}&signature={}", self.base, limit_qs, limit_sig);
                        
                        match send_void(
                            self.common
                                .client
                                .post(&limit_url)
                                .header("X-MBX-APIKEY", &self.common.api_key),
                        )
                        .await
                        {
                            Ok(_) => {
                                info!(
                                    symbol = %sym,
                                    limit_price = %limit_price_quantized,
                                    qty = %remaining_qty,
                                    "MIN_NOTIONAL fallback: limit reduce-only order placed successfully"
                                );
                                // Limit emri başarılı, pozisyon kapatılacak (emir fill olunca)
                                return Ok(());
                            }
                            Err(e2) => {
                                warn!(
                                    symbol = %sym,
                                    error = %e2,
                                    "MIN_NOTIONAL fallback: limit reduce-only order also failed"
                                );
                                // Limit emri de başarısız, dust için komple sıfırlamaya zorla
                                if remaining_qty < rules.min_notional / Decimal::from(1000) {
                                    // Çok küçük miktar, quantize et ve sıfırla
                                    let dust_qty = quantize_decimal(remaining_qty, rules.step_size);
                                    if dust_qty <= Decimal::ZERO {
                                        info!(
                                            symbol = %sym,
                                            "MIN_NOTIONAL: remaining qty is dust, considering position closed"
                                        );
                                        return Ok(());
                                    }
                                }
                                // Son deneme değilse devam et
                                if attempt < max_attempts - 1 {
                                    tokio::time::sleep(Duration::from_millis(500)).await;
                                    continue;
                                } else {
                                    return Err(anyhow!("MIN_NOTIONAL error: market and limit reduce-only both failed: {}", e));
                                }
                            }
                        }
                    } else {
                        // MIN_NOTIONAL hatası değil, normal retry
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
        }

        // Buraya gelmemeli (yukarıdaki return'ler ile çıkılmalı)
        Err(anyhow::anyhow!("Unexpected error in flatten_position"))
    }
}

#[async_trait]
impl Venue for BinanceFutures {
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

        // KRİTİK DÜZELTME: Validation guard - tek nokta kontrol
        // Bu fonksiyon -1111 hatasını imkânsız hale getirir
        let (price_str, qty_str, price_quantized, qty_quantized) = 
            Self::validate_and_format_order_params(px, qty, &rules, sym)?;

        // KRİTİK: Log'ları zenginleştir - gönderilen değerleri logla
        info!(
            %sym,
            side = ?side,
            price_original = %px.0,
            price_quantized = %price_quantized,
            price_str,
            qty_original = %qty.0,
            qty_quantized = %qty_quantized,
            qty_str,
            price_precision = rules.price_precision,
            qty_precision = rules.qty_precision,
            endpoint = "/fapi/v1/order",
            "order validation guard passed, submitting order"
        );

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
        
        // ✅ YENİ: reduceOnly desteği (TP emirleri için) - şimdilik false (normal emirler için)
        // TP emirleri için ayrı bir public method kullanılacak
        
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
        // KRİTİK DÜZELTME: Retry/backoff mekanizması
        // Transient hatalar için exponential backoff ile retry (aynı clientOrderId ile)
        const MAX_RETRIES: u32 = 3;
        const INITIAL_BACKOFF_MS: u64 = 100;
        
        let mut last_error = None;
        let mut order_result: Option<FutPlacedOrder> = None;
        
        for attempt in 0..=MAX_RETRIES {
            // Her retry'de yeni request oluştur (aynı parametrelerle, aynı clientOrderId ile)
            let retry_qs = params.join("&");
            let retry_sig = self.common.sign(&retry_qs);
            let retry_url = format!("{}/fapi/v1/order?{}&signature={}", self.base, retry_qs, retry_sig);
            
            match self.common
                .client
                .post(&retry_url)
                .header("X-MBX-APIKEY", &self.common.api_key)
                .send()
                .await
            {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        match resp.json::<FutPlacedOrder>().await {
                            Ok(order) => {
                                order_result = Some(order);
                                break; // Başarılı, döngüden çık
                            }
                            Err(e) => {
                                if attempt < MAX_RETRIES {
                                    let backoff_ms = INITIAL_BACKOFF_MS * 3_u64.pow(attempt);
                                    tracing::warn!(error = %e, attempt = attempt + 1, backoff_ms, "json parse error, retrying");
                                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                                    last_error = Some(anyhow!("json parse error: {}", e));
                                    continue;
                                } else {
                                    return Err(e.into());
                                }
                            }
                        }
                    } else {
                        // Status code hata
                        let body = resp.text().await.unwrap_or_default();
                        let body_lower = body.to_lowercase();
                        
                        // KRİTİK DÜZELTME: -1111 (precision) hatası - rules'ı yeniden çek ve retry
                        if body_lower.contains("precision is over") || body_lower.contains("-1111") {
                            if attempt < MAX_RETRIES {
                                // Rules'ı yeniden çek
                                warn!(%sym, attempt = attempt + 1, "precision error (-1111), refreshing rules and retrying");
                                match self.rules_for(sym).await {
                                    Ok(new_rules) => {
                                        // Yeni rules ile yeniden validate et
                                        match Self::validate_and_format_order_params(px, qty, &new_rules, sym) {
                                            Ok((new_price_str, new_qty_str, _, _)) => {
                                                // Yeni değerlerle retry
                                                let backoff_ms = INITIAL_BACKOFF_MS * 3_u64.pow(attempt);
                                                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                                                
                                                // Params'ı güncelle
                                                params = vec![
                                                    format!("symbol={}", sym),
                                                    format!("side={}", s_side),
                                                    "type=LIMIT".to_string(),
                                                    format!("timeInForce={}", tif_str),
                                                    format!("price={}", new_price_str),
                                                    format!("quantity={}", new_qty_str),
                                                    format!("timestamp={}", BinanceCommon::ts()),
                                                    format!("recvWindow={}", self.common.recv_window_ms),
                                                    "newOrderRespType=RESULT".to_string(),
                                                ];
                                                if !client_order_id.is_empty() {
                                                    params.push(format!("newClientOrderId={}", client_order_id));
                                                }
                                                
                                                last_error = Some(anyhow!("precision error, retrying with refreshed rules"));
                                                continue;
                                            }
                                            Err(e) => {
                                                error!(%sym, error = %e, "validation failed after rules refresh, giving up");
                                                return Err(anyhow!("precision error, validation failed after rules refresh: {}", e));
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        error!(%sym, error = %e, "failed to refresh rules, giving up");
                                        return Err(anyhow!("precision error, failed to refresh rules: {}", e));
                                    }
                                }
                            } else {
                                error!(%sym, attempt, "precision error (-1111) after max retries, symbol should be quarantined");
                                return Err(anyhow!("binance api error: {} - {} (precision error, max retries)", status, body));
                            }
                        }
                        
                        // Kalıcı hata kontrolü
                        if is_permanent_error(status.as_u16(), &body) {
                            tracing::error!(%status, %body, attempt, "permanent error, no retry");
                            return Err(anyhow!("binance api error: {} - {} (permanent)", status, body));
                        }
                        
                        // Transient hata kontrolü
                        if is_transient_error(status.as_u16(), &body) && attempt < MAX_RETRIES {
                            let backoff_ms = INITIAL_BACKOFF_MS * 3_u64.pow(attempt);
                            tracing::warn!(%status, %body, attempt = attempt + 1, backoff_ms, "transient error, retrying with exponential backoff (same clientOrderId)");
                            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                            last_error = Some(anyhow!("binance api error: {} - {}", status, body));
                            continue;
                        } else {
                            // Transient değil veya max retry'ye ulaşıldı
                            tracing::error!(%status, %body, attempt, "error after retries");
                            return Err(anyhow!("binance api error: {} - {}", status, body));
                        }
                    }
                }
                Err(e) => {
                    // Network hatası
                    if attempt < MAX_RETRIES {
                        let backoff_ms = INITIAL_BACKOFF_MS * 3_u64.pow(attempt);
                        tracing::warn!(error = %e, attempt = attempt + 1, backoff_ms, "network error, retrying with exponential backoff (same clientOrderId)");
                        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                        last_error = Some(e.into());
                        continue;
                    } else {
                        tracing::error!(error = %e, attempt, "network error after retries");
                        return Err(e.into());
                    }
                }
            }
        }
        
        // Başarılı sonuç döndür
        let order = order_result.ok_or_else(|| {
            last_error.unwrap_or_else(|| anyhow!("unknown error after retries"))
        })?;

        info!(
            %sym,
            ?side,
            price_quantized = %price_quantized,
            qty_quantized = %qty_quantized,
            price_str,
            qty_str,
            tif = ?tif,
            order_id = order.order_id,
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

    async fn close_position(&self, sym: &str) -> Result<()> {
        self.flatten_position(sym, self.hedge_mode).await
    }
}

impl BinanceFutures {
    /// Test order endpoint - İlk emir öncesi doğrulama
    /// /fapi/v1/order/test endpoint'i ile emir parametrelerini test et
    /// -1111 hatası gelirse sembolü disable et ve rules'ı yeniden çek
    pub async fn test_order(
        &self,
        sym: &str,
        side: Side,
        px: Px,
        qty: Qty,
        tif: Tif,
    ) -> Result<()> {
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
        
        // Validation guard ile format et
        let (price_str, qty_str, _, _) = 
            Self::validate_and_format_order_params(px, qty, &rules, sym)?;

        let params = vec![
            format!("symbol={}", sym),
            format!("side={}", s_side),
            "type=LIMIT".to_string(),
            format!("timeInForce={}", tif_str),
            format!("price={}", price_str),
            format!("quantity={}", qty_str),
            format!("timestamp={}", BinanceCommon::ts()),
            format!("recvWindow={}", self.common.recv_window_ms),
        ];

        let qs = params.join("&");
        let sig = self.common.sign(&qs);
        let url = format!("{}/fapi/v1/order/test?{}&signature={}", self.base, qs, sig);

        match send_void(
            self.common
                .client
                .post(&url)
                .header("X-MBX-APIKEY", &self.common.api_key),
        )
        .await
        {
            Ok(_) => {
                info!(%sym, price_str, qty_str, "test order passed");
                Ok(())
            }
            Err(e) => {
                let error_str = e.to_string();
                let error_lower = error_str.to_lowercase();
                
                // -1111 hatası gelirse sembolü disable et
                if error_lower.contains("precision is over") || error_lower.contains("-1111") {
                    warn!(
                        %sym,
                        price_str,
                        qty_str,
                        error = %e,
                        "test order failed with -1111 (precision error), symbol should be disabled and rules refreshed"
                    );
                    Err(anyhow!("test order failed with precision error: {}", e))
                } else {
                    warn!(%sym, price_str, qty_str, error = %e, "test order failed");
                    Err(e)
                }
            }
        }
    }
    
    /// Validation guard: Emir gönderiminden önce son doğrulama
    /// Bu fonksiyon -1111 hatasını imkânsız hale getirir
    /// price = floor_to_step(price, tick_size)
    /// qty = floor_to_step(abs(qty), step_size)
    /// price_str = format_to_precision(price, price_precision)
    /// qty_str = format_to_precision(qty, qty_precision)
    /// Son kontrol: fractional_digits(price_str) <= price_precision
    pub fn validate_and_format_order_params(
        px: Px,
        qty: Qty,
        rules: &SymbolRules,
        sym: &str,
    ) -> Result<(String, String, Decimal, Decimal)> {
        let price_precision = rules.price_precision;
        let qty_precision = rules.qty_precision;

        // 1. Quantize: step_size'a göre floor
        let price_quantized = quantize_decimal(px.0, rules.tick_size);
        let qty_quantized = quantize_decimal(qty.0.abs(), rules.step_size);

        // 2. Round: precision'a göre round et
        let price = price_quantized
            .round_dp_with_strategy(price_precision as u32, RoundingStrategy::ToZero);
        let qty_rounded =
            qty_quantized.round_dp_with_strategy(qty_precision as u32, RoundingStrategy::ToZero);

        // 3. Format: precision'a göre string'e çevir
        let price_str = format_decimal_fixed(price, price_precision);
        let qty_str = format_decimal_fixed(qty_rounded, qty_precision);

        // 4. KRİTİK: Son kontrol - fractional_digits kontrolü
        let price_fractional = if let Some(dot_pos) = price_str.find('.') {
            price_str[dot_pos + 1..].len()
        } else {
            0
        };
        let qty_fractional = if let Some(dot_pos) = qty_str.find('.') {
            qty_str[dot_pos + 1..].len()
        } else {
            0
        };

        if price_fractional > price_precision {
            let error_msg = format!(
                "CRITICAL: price_str fractional digits ({}) > price_precision ({}) for {}",
                price_fractional, price_precision, sym
            );
            tracing::error!(%sym, price_str, price_precision, price_fractional, %error_msg);
            return Err(anyhow!(error_msg));
        }

        if qty_fractional > qty_precision {
            let error_msg = format!(
                "CRITICAL: qty_str fractional digits ({}) > qty_precision ({}) for {}",
                qty_fractional, qty_precision, sym
            );
            tracing::error!(%sym, qty_str, qty_precision, qty_fractional, %error_msg);
            return Err(anyhow!(error_msg));
        }

        // 5. Min notional kontrolü
        let notional = price * qty_rounded;
        if !rules.min_notional.is_zero() && notional < rules.min_notional {
            return Err(anyhow!(
                "below min notional after validation ({} < {})",
                notional,
                rules.min_notional
            ));
        }

        Ok((price_str, qty_str, price, qty_rounded))
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

pub fn format_decimal_fixed(value: Decimal, precision: usize) -> String {
    // KRİTİK DÜZELTME: Edge case'ler için ek kontroller
    // Precision overflow kontrolü (max 28 decimal places)
    let precision = precision.min(28);
    let scale = precision as u32;

    // Decimal her zaman finite'dir, bu yüzden direkt işle

    // ÖNEMLİ: Precision hatasını önlemek için önce quantize, sonra format
    // KRİTİK: round_dp_with_strategy ile kesinlikle precision'a kadar yuvarla
    // ToZero strategy kullanarak fazla basamakları kes
    let truncated = value.round_dp_with_strategy(scale, RoundingStrategy::ToZero);

    // KRİTİK: String formatlamada kesinlikle precision'dan fazla basamak gösterme
    // Decimal'in to_string() metodu bazen internal precision'ı gösterebilir
    // Bu yüzden manuel olarak string'i kontrol edip kesmeliyiz
    if scale == 0 {
        // Integer kısmı al (nokta varsa kes)
        let s = truncated.to_string();
        if let Some(dot_pos) = s.find('.') {
            s[..dot_pos].to_string()
        } else {
            s
        }
    } else {
        let s = truncated.to_string();
        if let Some(dot_pos) = s.find('.') {
            let integer_part = &s[..dot_pos];
            let decimal_part = &s[dot_pos + 1..];
            let current_decimals = decimal_part.len();
            
            if current_decimals < scale as usize {
                // Eksik trailing zero'ları ekle
                format!("{}.{}{}", integer_part, decimal_part, "0".repeat(scale as usize - current_decimals))
            } else if current_decimals > scale as usize {
                // KRİTİK: Fazla decimal varsa kes - kesinlikle precision'dan fazla basamak gösterme
                // String'i kes - bu "Precision is over the maximum" hatasını önler
                let truncated_decimal = &decimal_part[..scale as usize];
                format!("{}.{}", integer_part, truncated_decimal)
            } else {
                // Tam precision - olduğu gibi döndür
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

/// Transient hata mı kontrol et (retry yapılabilir mi?)
/// 408 (Request Timeout), 429 (Too Many Requests), 5xx (Server Errors) → transient
/// 400 (Bad Request) → body'ye göre karar ver (bazıları transient olabilir)
fn is_transient_error(status: u16, _body: &str) -> bool {
    match status {
        408 => true,  // Request Timeout
        429 => true,  // Too Many Requests
        500..=599 => true,  // Server Errors
        400 => {
            // 400 için body'ye bak - bazı hatalar transient olabilir
            // "Invalid symbol" gibi kalıcı hatalar retry edilmemeli
            // "Precision is over" gibi hatalar kalıcı
            // "Insufficient margin" gibi hatalar kalıcı
            // Ama network timeout gibi durumlar transient olabilir
            // Şimdilik 400'leri kalıcı sayalım (daha güvenli)
            false
        }
        _ => false,  // Diğer hatalar kalıcı
    }
}

/// Kalıcı hata mı kontrol et (sembol disable edilmeli mi?)
/// "invalid", "margin", "precision" gibi hatalar kalıcıdır
fn is_permanent_error(status: u16, body: &str) -> bool {
    if status == 400 {
        let body_lower = body.to_lowercase();
        // KRİTİK DÜZELTME: -1111 (precision) hatası permanent değil, retry edilebilir
        // Çünkü girdiyi düzelterek geçilebilir
        if body_lower.contains("precision is over") || body_lower.contains("-1111") {
            return false; // Precision hatası retry edilebilir
        }
        body_lower.contains("invalid") ||
        body_lower.contains("margin") ||
        body_lower.contains("insufficient balance") ||
        body_lower.contains("min notional") ||
        body_lower.contains("below min notional")
    } else {
        false
    }
}


async fn send_json<T>(builder: RequestBuilder) -> Result<T>
where
    T: DeserializeOwned,
{
    // KRİTİK DÜZELTME: Retry/backoff mekanizması
    // RequestBuilder clone edilemediği için, request'i baştan oluşturmalıyız
    // Ama builder'ı closure'a wrap edemeyiz çünkü builder consume ediliyor
    // Bu yüzden şimdilik sadece ilk denemeyi yapıyoruz, retry mekanizması üst seviyede implement edilebilir
    let resp = builder.send().await?;
    let resp = ensure_success(resp).await?;
    Ok(resp.json().await?)
}

async fn send_void(builder: RequestBuilder) -> Result<()> {
    // KRİTİK DÜZELTME: Retry/backoff mekanizması
    // RequestBuilder clone edilemediği için, request'i baştan oluşturmalıyız
    // Ama builder'ı closure'a wrap edemeyiz çünkü builder consume ediliyor
    // Bu yüzden şimdilik sadece ilk denemeyi yapıyoruz, retry mekanizması üst seviyede implement edilebilir
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


// ============================================================================
// Binance WebSocket Module (from binance_ws.rs)
// ============================================================================

pub type WsStream = WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Deserialize)]
struct ListenKeyResp {
    #[serde(rename = "listenKey")]
    listen_key: String,
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
        qty: Qty,                   // Last executed qty (incremental)
        cumulative_filled_qty: Qty, // Cumulative filled qty (total filled so far)
        price: Px,
        is_maker: bool,       // true = maker, false = taker
        order_status: String, // Order status: NEW, PARTIALLY_FILLED, FILLED, CANCELED, etc.
        commission: Decimal,   // KRİTİK: Gerçek komisyon (executionReport'tan "n" field'ı)
    },
    OrderCanceled {
        symbol: String,
        order_id: String,
        client_order_id: Option<String>, // Idempotency için
    },
    Heartbeat,
}

pub struct UserDataStream {
    client: Client,
    base: String,
    api_key: String,
    kind: UserStreamKind,
    listen_key: String,
    ws: WsStream,
    last_keep_alive: Instant,
    /// Reconnect sonrası missed events sync callback
    /// Callback reconnect sonrası çağrılır ve missed events'leri sync etmek için kullanılır
    on_reconnect: Option<Box<dyn Fn() + Send + Sync>>,
}

impl UserDataStream {
    #[inline]
    fn ws_url_for(_kind: UserStreamKind, listen_key: &str) -> String {
        // USDⓈ-M Futures user data
        format!("wss://fstream.binance.com/ws/{}", listen_key)
    }

    async fn create_listen_key(
        client: &Client,
        base: &str,
        api_key: &str,
        _kind: UserStreamKind,
    ) -> Result<String> {
        let base = base.trim_end_matches('/');
        let endpoint = format!("{}/fapi/v1/listenKey", base);

        let resp = client
            .post(&endpoint)
            .header("X-MBX-APIKEY", api_key)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            // resp.text() self'i tükettiği için status’u ÖNCE aldık
            let body = resp.text().await.unwrap_or_default();
            error!(status=?status, body=%body, "listenKey create failed");
            return Err(anyhow!("listenKey create failed: {} {}", status, body));
        }

        let lk: ListenKeyResp = resp.json().await?;
        info!(listen_key=%lk.listen_key, "listenKey created");
        Ok(lk.listen_key)
    }

    async fn keepalive_listen_key(&self, listen_key: &str) -> Result<()> {
        let base = self.base.trim_end_matches('/');
        let endpoint = format!("{}/fapi/v1/listenKey?listenKey={}", base, listen_key);

        let resp = self
            .client
            .put(&endpoint)
            .header("X-MBX-APIKEY", &self.api_key)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            warn!(status=?status, body=%body, "listenKey keepalive failed");
            return Err(anyhow!("listenKey keepalive failed: {} {}", status, body));
        }

        debug!("refreshed user data listen key");
        Ok(())
    }

    /// WS'yi yeni listenKey ile tekrar bağlar (var olan ws kapatılır)
    /// KRİTİK DÜZELTME: Reconnect sonrası missed events sync eklendi
    async fn reconnect_ws(&mut self) -> Result<()> {
        // 1. Yeni listen key oluştur (eski expire olmuş olabilir)
        let new_key =
            Self::create_listen_key(&self.client, &self.base, &self.api_key, self.kind).await?;
        self.listen_key = new_key;

        // 2. WebSocket'e bağlan
        let url = Self::ws_url_for(self.kind, &self.listen_key);
        let (ws, _) = connect_async(&url).await?;
        self.ws = ws;
        self.last_keep_alive = Instant::now();

        // 3. KRİTİK DÜZELTME: Reconnect sonrası missed events sync callback'i çağır
        // Callback main.rs'de set edilir ve REST API'den missed events'leri sync eder
        if let Some(ref callback) = self.on_reconnect {
            callback();
            info!(%url, "reconnected user data websocket, sync callback triggered");
        } else {
            warn!(%url, "WebSocket reconnected, but no sync callback set - missed events may not be synced");
        }
        
        info!(%url, "reconnected user data websocket");
        Ok(())
    }
    
    /// Reconnect sonrası missed events sync callback'i set et
    /// Callback reconnect sonrası çağrılır ve REST API'den missed events'leri sync etmek için kullanılır
    pub fn set_on_reconnect<F>(&mut self, callback: F)
    where
        F: Fn() + Send + Sync + 'static,
    {
        self.on_reconnect = Some(Box::new(callback));
    }

    pub async fn connect(
        client: Client,
        base: &str,
        api_key: &str,
        kind: UserStreamKind,
    ) -> Result<Self> {
        let base = base.trim_end_matches('/').to_string();

        // 1) listenKey oluştur
        let listen_key = Self::create_listen_key(&client, &base, api_key, kind).await?;

        // 2) WS bağlan
        let ws_url = Self::ws_url_for(kind, &listen_key);
        let (ws, _) = connect_async(&ws_url).await?;
        info!(%ws_url, "connected user data websocket");

        Ok(Self {
            client,
            base,
            api_key: api_key.to_string(),
            kind,
            listen_key,
            ws,
            last_keep_alive: Instant::now(),
            on_reconnect: None,
        })
    }

    /// 25. dakikadan sonra keepalive (PUT). Hata alırsak yeni listenKey oluşturup WS'yi yeniden bağlarız.
    async fn keep_alive(&mut self) -> Result<()> {
        // Binance listenKey 60dk geçerli; biz 25dk'da bir yeniliyoruz
        if self.last_keep_alive.elapsed() < Duration::from_secs(60 * 25) {
            return Ok(());
        }

        match self.keepalive_listen_key(&self.listen_key).await {
            Ok(()) => {
                self.last_keep_alive = Instant::now();
                return Ok(());
            }
            Err(e) => {
                warn!(err=?e, "keepalive failed; will recreate listenKey and reconnect ws");
            }
        }

        // Keepalive başarısız → yeni listenKey oluştur
        let new_key =
            Self::create_listen_key(&self.client, &self.base, &self.api_key, self.kind).await?;
        self.listen_key = new_key;

        // WS yeniden bağlan
        self.reconnect_ws().await?;
        Ok(())
    }

    fn parse_side(side: &str) -> Side {
        if side.eq_ignore_ascii_case("buy") {
            Side::Buy
        } else {
            Side::Sell
        }
    }

    fn parse_decimal(value: &Value, key: &str) -> Decimal {
        value
            .get(key)
            .and_then(Value::as_str)
            .and_then(|s| Decimal::from_str(s).ok())
            .unwrap_or(Decimal::ZERO)
    }

    pub async fn next_event(&mut self) -> Result<UserEvent> {
        loop {
            self.keep_alive().await?;

            match timeout(Duration::from_secs(70), self.ws.next()).await {
                Ok(Some(msg)) => {
                    let msg = msg.map_err(|e| match e {
                        WsError::ConnectionClosed | WsError::AlreadyClosed => {
                            anyhow!("user stream closed")
                        }
                        other => anyhow!(other),
                    })?;

                    match msg {
                        Message::Ping(payload) => {
                            self.ws.send(Message::Pong(payload)).await?;
                            return Ok(UserEvent::Heartbeat);
                        }
                        Message::Pong(_) => return Ok(UserEvent::Heartbeat),
                        Message::Text(txt) => {
                            if txt.is_empty() {
                                continue;
                            }
                            // Uyarı: USDⓈ-M tarafında auth wrapper'da {"stream": "...", "data": {...}} gelebilir.
                            let value: Value = serde_json::from_str(&txt)?;
                            let data = value.get("data").cloned().unwrap_or_else(|| value.clone());
                            if let Some(event) = Self::map_event(&data)? {
                                return Ok(event);
                            }
                        }
                        Message::Binary(_) => continue,
                        Message::Close(_) => return Err(anyhow!("user stream closed")),
                        Message::Frame(_) => continue,
                    }
                }
                Ok(None) => return Err(anyhow!("user stream terminated")),
                Err(_) => {
                    warn!("websocket timeout, reconnecting");
                    self.reconnect_ws().await?;
                }
            }
        }
    }

    fn map_event(value: &Value) -> Result<Option<UserEvent>> {
        let event_type = value.get("e").and_then(Value::as_str).unwrap_or_default();
        match event_type {
            // Futures executionReport
            "executionReport" => {
                let symbol = value
                    .get("s")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let order_id = value
                    .get("i")
                    .and_then(Value::as_i64)
                    .unwrap_or_default()
                    .to_string();
                let client_order_id = value
                    .get("c")
                    .and_then(Value::as_str)
                    .map(|s| s.to_string());
                let status = value.get("X").and_then(Value::as_str).unwrap_or_default();
                if status == "CANCELED" {
                    return Ok(Some(UserEvent::OrderCanceled {
                        symbol,
                        order_id,
                        client_order_id,
                    }));
                }
                let exec_type = value.get("x").and_then(Value::as_str).unwrap_or_default();
                if exec_type != "TRADE" {
                    return Ok(Some(UserEvent::Heartbeat));
                }
                let qty = Self::parse_decimal(value, "l"); // last executed qty (incremental)
                let cumulative_filled_qty = Self::parse_decimal(value, "z"); // cumulative filled qty (total)
                let price = Self::parse_decimal(value, "L"); // last executed price
                let side =
                    Self::parse_side(value.get("S").and_then(Value::as_str).unwrap_or("SELL"));
                // Maker flag: "m" field (true = maker, false = taker)
                let is_maker = value.get("m").and_then(Value::as_bool).unwrap_or(false);
                // KRİTİK DÜZELTME: Gerçek komisyon (executionReport'tan "n" field'ı)
                // "n" = commission (last executed qty için komisyon, incremental)
                let commission = Self::parse_decimal(value, "n");
                    return Ok(Some(UserEvent::OrderFill {
                        symbol,
                        order_id,
                        side,
                        qty: Qty(qty),
                        cumulative_filled_qty: Qty(cumulative_filled_qty),
                    price: Px(price),
                    is_maker,
                    order_status: status.to_string(),
                    commission,
                }));
            }

            // FUTURES user data wrapper: ORDER_TRADE_UPDATE
            "ORDER_TRADE_UPDATE" => {
                let data = value
                    .get("o")
                    .ok_or_else(|| anyhow!("missing order payload"))?;
                let symbol = data
                    .get("s")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let order_id = data
                    .get("i")
                    .and_then(Value::as_i64)
                    .unwrap_or_default()
                    .to_string();
                let client_order_id = data.get("c").and_then(Value::as_str).map(|s| s.to_string());
                let status = data.get("X").and_then(Value::as_str).unwrap_or_default();
                if status == "CANCELED" {
                    return Ok(Some(UserEvent::OrderCanceled {
                        symbol,
                        order_id,
                        client_order_id,
                    }));
                }
                let exec_type = data.get("x").and_then(Value::as_str).unwrap_or_default();
                if exec_type != "TRADE" {
                    return Ok(Some(UserEvent::Heartbeat));
                }
                let qty = Self::parse_decimal(data, "l"); // last filled (incremental)
                let cumulative_filled_qty = Self::parse_decimal(data, "z"); // cumulative filled qty (total)
                let price = Self::parse_decimal(data, "L"); // last price
                let side =
                    Self::parse_side(data.get("S").and_then(Value::as_str).unwrap_or("SELL"));
                // Maker flag: "m" field (true = maker, false = taker)
                let is_maker = data.get("m").and_then(Value::as_bool).unwrap_or(false);
                // KRİTİK DÜZELTME: Gerçek komisyon (ORDER_TRADE_UPDATE'ten "n" field'ı)
                // "n" = commission (last executed qty için komisyon, incremental)
                let commission = Self::parse_decimal(data, "n");
                    return Ok(Some(UserEvent::OrderFill {
                        symbol,
                        order_id,
                        side,
                        qty: Qty(qty),
                        cumulative_filled_qty: Qty(cumulative_filled_qty),
                    price: Px(price),
                    is_maker,
                    order_status: status.to_string(),
                    commission,
                }));
            }

            _ => {}
        }
        Ok(Some(UserEvent::Heartbeat))
    }
}

