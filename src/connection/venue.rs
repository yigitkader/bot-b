
use crate::types::{
    FutExchangeInfo, FutExchangeSymbol, FutFilter, Position, PriceUpdate, Px, Qty, Side,
    SymbolMeta, SymbolRules, Tif, VenueOrder,
};
use rust_decimal::{Decimal, RoundingStrategy};
use std::str::FromStr;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use dashmap::DashMap;
use hmac::{Hmac, Mac};
use once_cell::sync::Lazy;
use reqwest::{Client, RequestBuilder, Response};
use rust_decimal::prelude::ToPrimitive;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use sha2::Sha256;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::time::Duration;
use tracing::{error, info, warn};
use urlencoding::encode;
#[async_trait]
pub trait Venue: Send + Sync {
    async fn place_limit_with_client_id(
        &self,
        sym: &str,
        side: Side,
        px: Px,
        qty: Qty,
        tif: Tif,
        client_order_id: &str,
    ) -> Result<(String, Option<String>)>;
    async fn cancel(&self, order_id: &str, sym: &str) -> Result<()>;
    async fn best_prices(&self, sym: &str) -> Result<(Px, Px)>;
    async fn get_open_orders(&self, sym: &str) -> Result<Vec<VenueOrder>>;
    async fn get_position(&self, sym: &str) -> Result<Position>;
    async fn get_all_positions(&self) -> Result<Vec<(String, Position)>>;
    async fn place_trailing_stop_order(
        &self,
        symbol: &str,
        activation_price: Px,
        callback_rate: f64,
        quantity: Qty,
    ) -> Result<String>;
    async fn available_balance(&self, asset: &str) -> Result<Decimal>;
}
pub static FUT_RULES: Lazy<DashMap<String, Arc<SymbolRules>>> = Lazy::new(|| DashMap::new());
pub static PRICE_CACHE: Lazy<DashMap<String, PriceUpdate>> = Lazy::new(|| DashMap::new());
pub static POSITION_CACHE: Lazy<DashMap<String, Position>> = Lazy::new(|| DashMap::new());
pub static BALANCE_CACHE: Lazy<DashMap<String, Decimal>> = Lazy::new(|| DashMap::new());
pub static OPEN_ORDERS_CACHE: Lazy<DashMap<String, Vec<VenueOrder>>> = Lazy::new(|| DashMap::new());
fn str_dec<S: AsRef<str>>(s: S) -> Decimal {
    let value = s.as_ref();
    Decimal::from_str(value).unwrap_or_else(|err| {
        warn!(input = value, ?err, "failed to parse decimal from string");
        Decimal::ZERO
    })
}
fn scale_from_step(step: Decimal) -> usize {
    if step.is_zero() {
        return 8;
    }
    if step >= Decimal::ONE {
        return 0;
    }
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
    let final_tick = if tick.is_zero() {
        tracing::warn!(symbol = %sym.symbol, "tickSize is zero, using fallback 0.01");
        Decimal::new(1, 2)
    } else {
        tick
    };
    let final_step = if step.is_zero() {
        tracing::warn!(symbol = %sym.symbol, "stepSize is zero, using fallback 0.001");
        Decimal::new(1, 3)
    } else {
        step
    };
    tracing::debug!(
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
        max_leverage: None,
    }
}
#[derive(Clone)]
pub struct BinanceCommon {
    pub client: Arc<Client>,
    pub api_key: String,
    pub secret_key: String,
    pub recv_window_ms: u64,
}
impl BinanceCommon {
fn ts() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| {
                warn!("System time is before UNIX epoch, using fallback timestamp");
                Duration::from_secs(0)
            })
            .as_millis() as u64
    }
fn sign(&self, qs: &str) -> String {
        let mut mac = match Hmac::<Sha256>::new_from_slice(self.secret_key.as_bytes()) {
            Ok(mac) => mac,
            Err(e) => {
                error!(
                    error = %e,
                    secret_key_length = self.secret_key.len(),
                    "CRITICAL: HMAC key initialization failed, returning empty signature (API call will fail)"
                );
                return String::new();
            }
        };
        mac.update(qs.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }
}
#[derive(Clone)]
pub struct BinanceFutures {
    pub base: String,
    pub common: BinanceCommon,
    pub hedge_mode: bool,
}
#[derive(Deserialize)]
struct OrderBookTop {
    bids: Vec<(String, String)>,
    asks: Vec<(String, String)>,
}
#[derive(Deserialize)]
pub(crate) struct FutPlacedOrder {
#[serde(rename = "orderId")]
    pub(crate) order_id: u64,
#[serde(rename = "clientOrderId")]
#[allow(dead_code)]
    pub(crate) client_order_id: Option<String>,
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
    position_side: Option<String>,
#[serde(rename = "marginType", default)]
    margin_type: String,
}
impl BinanceFutures {
pub fn from_config(
        binance_cfg: &crate::config::BinanceCfg,
    ) -> Result<Self> {
        let base = if binance_cfg.futures_base.contains("testnet") {
            "https://testnet.binancefuture.com".to_string()
        } else {
            binance_cfg.futures_base.clone()
        };
        let common = BinanceCommon {
            client: Arc::new(Client::new()),
            api_key: binance_cfg.api_key.clone(),
            secret_key: binance_cfg.secret_key.clone(),
            recv_window_ms: binance_cfg.recv_window_ms,
        };
        Ok(BinanceFutures {
            base,
            common,
            hedge_mode: binance_cfg.hedge_mode,
        })
    }
    pub async fn set_leverage(&self, sym: &str, leverage: u32) -> Result<u32> {
        let leverage_candidates = if leverage > 100 {
            vec![100, 75, 50, 25, 20, 10, 5, 1]
        } else if leverage > 50 {
            vec![leverage, 50, 25, 20, 10, 5, 1]
        } else if leverage > 25 {
            vec![leverage, 25, 20, 10, 5, 1]
        } else if leverage > 10 {
            vec![leverage, 20, 10, 5, 1]
        } else {
            vec![leverage, 5, 1]
        };
        let mut last_error = None;
        for &lev in &leverage_candidates {
            let params = vec![
                format!("symbol={}", sym),
                format!("leverage={}", lev),
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
                    if lev < leverage {
                        warn!(
                            symbol = %sym,
                            desired_leverage = leverage,
                            actual_leverage = lev,
                            "Leverage set to lower value than desired (symbol max leverage may be lower)"
                        );
                    } else {
                        info!(symbol = %sym, leverage = lev, "Leverage set successfully");
                    }
                    return Ok(lev);
                }
                Err(e) => {
                    last_error = Some(e);
                    if let Some(ref error) = last_error {
                        let error_str = error.to_string().to_lowercase();
                        if error_str.contains("leverage") && (error_str.contains("not valid") || error_str.contains("-4028")) {
                            tracing::debug!(
                                symbol = %sym,
                                attempted_leverage = lev,
                                "Leverage {}x not valid for symbol, trying lower value",
                                lev
                            );
                            continue;
                        } else {
                            warn!(symbol = %sym, leverage = lev, error = ?last_error, "Failed to set leverage (non-leverage error)");
                            return Err(last_error.unwrap());
                        }
                    } else {
                        return Err(anyhow!("Unknown error setting leverage"));
                    }
                }
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow!("All leverage values failed for symbol {}", sym)))
    }
    pub async fn set_position_side_dual(&self, dual: bool) -> Result<()> {
        let params = vec![
            format!("dualSidePosition={}", if dual { "true" } else { "false" }),
            format!("timestamp={}", BinanceCommon::ts()),
            format!("recvWindow={}", self.common.recv_window_ms),
        ];
        let qs = params.join("&");
        let sig = self.common.sign(&qs);
        let url = format!(
            "{}/fapi/v1/positionSide/dual?{}&signature={}",
            self.base, qs, sig
        );
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
                let error_str = e.to_string();
                if error_str.contains("-4059") || error_str.contains("No need to change position side") {
                    info!(
                        dual_side = dual,
                        "position side mode already set correctly (no change needed)"
                    );
                    Ok(())
                } else {
                    warn!(dual_side = dual, error = %e, "failed to set position side mode");
                    Err(e)
                }
            }
        }
    }
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
                let error_str = e.to_string();
                if error_str.contains("-4046") || error_str.contains("No need to change margin type") {
                    info!(
                        %sym,
                        margin_type = %margin_type,
                        "margin type already set correctly (no change needed)"
                    );
                    Ok(())
                } else {
                    warn!(%sym, margin_type = %margin_type, error = %e, "failed to set margin type");
                    Err(e)
                }
            }
        }
    }
    pub async fn fetch_max_leverage(&self, sym: &str) -> Option<u32> {
        fn deserialize_u32_flexible<'de, D>(deserializer: D) -> Result<u32, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            use serde::de::{self, Visitor};
            use std::fmt;
            struct U32FlexibleVisitor;
            impl<'de> Visitor<'de> for U32FlexibleVisitor {
                type Value = u32;
                fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                    formatter.write_str("a u32 or string representation of u32")
                }
                fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
                where
                    E: de::Error,
                {
                    Ok(value as u32)
                }
                fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
                where
                    E: de::Error,
                {
                    value.parse::<u32>().map_err(de::Error::custom)
                }
            }
            deserializer.deserialize_any(U32FlexibleVisitor)
        }
        #[derive(Deserialize)]
        struct LeverageBracket {
            #[serde(deserialize_with = "deserialize_u32_flexible")]
            bracket: u32,
            #[serde(rename = "initialLeverage", deserialize_with = "deserialize_u32_flexible")]
            initial_leverage: u32,
            #[serde(rename = "notionalCap")]
            notional_cap: String,
            #[serde(rename = "notionalFloor")]
            notional_floor: String,
            #[serde(rename = "maintMarginRatio")]
            maint_margin_ratio: String,
        }
        #[derive(Deserialize)]
        struct LeverageBracketsResponse {
            symbol: String,
            brackets: Vec<LeverageBracket>,
        }
        let qs = format!(
            "timestamp={}&recvWindow={}",
            BinanceCommon::ts(),
            self.common.recv_window_ms
        );
        let sig = self.common.sign(&qs);
        let url = format!("{}/fapi/v1/leverageBrackets?{}&signature={}", self.base, qs, sig);
        let resp = match self.common
            .client
            .get(&url)
            .header("X-MBX-APIKEY", &self.common.api_key)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    symbol = %sym,
                    "Failed to send leverage brackets request (network error)"
                );
                return None;
            }
        };
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            tracing::debug!(
                error = %status,
                body = %body,
                symbol = %sym,
                "Leverage brackets API returned error status"
            );
            return None;
        }
        match resp.json::<serde_json::Value>().await {
            Ok(json) => {
                let json_str = serde_json::to_string(&json).unwrap_or_default();
                let preview = if json_str.len() > 500 {
                    format!("{}...", &json_str[..500])
                } else {
                    json_str.clone()
                };
                tracing::debug!(
                    symbol = %sym,
                    response_preview = %preview,
                    "Leverage brackets API response received"
                );
                let brackets_result: Result<Vec<LeverageBracketsResponse>, _> = if json.is_array() {
                    serde_json::from_value::<Vec<LeverageBracketsResponse>>(json.clone())
                        .or_else(|_| {
                            serde_json::from_value::<Vec<LeverageBracket>>(json.clone())
                                .map(|brackets| vec![LeverageBracketsResponse {
                                    symbol: sym.to_string(),
                                    brackets,
                                }])
                        })
                        .or_else(|_| {
                            if let Some(arr) = json.as_array() {
                                let mut all_brackets = Vec::new();
                                for item in arr {
                                    if let Some(obj) = item.as_object() {
                                        if let (Some(symbol_val), Some(brackets_val)) = (obj.get("symbol"), obj.get("brackets")) {
                                            if let (Some(symbol_str), Some(brackets_arr)) = (symbol_val.as_str(), brackets_val.as_array()) {
                                                let mut brackets = Vec::new();
                                                for bracket_item in brackets_arr {
                                                    if let Some(bracket_obj) = bracket_item.as_object() {
                                                        let initial_leverage = bracket_obj.get("initialLeverage")
                                                            .and_then(|v| {
                                                                v.as_u64()
                                                                    .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
                                                            });
                                                        if let Some(lev) = initial_leverage {
                                                            let bracket_num = bracket_obj.get("bracket")
                                                                .and_then(|v| {
                                                                    v.as_u64()
                                                                        .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
                                                                })
                                                                .unwrap_or(0) as u32;
                                                            brackets.push(LeverageBracket {
                                                                bracket: bracket_num,
                                                                initial_leverage: lev as u32,
                                                                notional_cap: bracket_obj.get("notionalCap").and_then(|v| v.as_str()).unwrap_or("0").to_string(),
                                                                notional_floor: bracket_obj.get("notionalFloor").and_then(|v| v.as_str()).unwrap_or("0").to_string(),
                                                                maint_margin_ratio: bracket_obj.get("maintMarginRatio").and_then(|v| v.as_str()).unwrap_or("0").to_string(),
                                                            });
                                                        }
                                                    }
                                                }
                                                if !brackets.is_empty() {
                                                    all_brackets.push(LeverageBracketsResponse {
                                                        symbol: symbol_str.to_string(),
                                                        brackets,
                                                    });
                                                }
                                            }
                                        }
                                    }
                                }
                                if !all_brackets.is_empty() {
                                    Ok(all_brackets)
                                } else {
                                    serde_json::from_value::<Vec<LeverageBracketsResponse>>(serde_json::json!([]))
                                }
                            } else {
                                serde_json::from_value::<Vec<LeverageBracketsResponse>>(serde_json::json!({}))
                            }
                        })
                } else {
                    serde_json::from_value::<LeverageBracketsResponse>(json)
                        .map(|r| vec![r])
                };
                match brackets_result {
                    Ok(brackets_vec) => {
                        let symbol_brackets = match brackets_vec.iter()
                            .find(|b| b.symbol.eq_ignore_ascii_case(sym))
                            .or_else(|| brackets_vec.first())
                        {
                            Some(b) => b,
                            None => {
                                warn!(symbol = %sym, "No brackets found in response");
                                return None;
                            }
                        };
                        let max_lev = symbol_brackets.brackets
                            .iter()
                            .map(|b| b.initial_leverage)
                            .max()
                            .unwrap_or(1);
                        tracing::debug!(
                            symbol = %sym,
                            max_leverage = max_lev,
                            "Fetched max leverage from Binance leverage brackets"
                        );
                        Some(max_lev)
                    }
                    Err(e) => {
                        tracing::debug!(
                            error = %e,
                            symbol = %sym,
                            response_preview = %preview,
                            "Failed to parse leverage brackets response (format may have changed)"
                        );
                        None
                    }
                }
            }
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    symbol = %sym,
                    "Failed to parse leverage brackets response as JSON"
                );
                None
            }
        }
    }
    pub async fn rules_for(&self, sym: &str) -> Result<Arc<SymbolRules>> {
        if let Some(r) = FUT_RULES.get(sym) {
            return Ok(r.clone());
        }
    const MAX_RETRIES: u32 = 2;
    const INITIAL_BACKOFF_MS: u64 = 100;
        let mut last_error = None;
        for attempt in 0..=MAX_RETRIES {
            let url = format!("{}/fapi/v1/exchangeInfo?symbol={}", self.base, encode(sym));
            match send_json::<FutExchangeInfo>(self.common.client.get(url)).await {
                Ok(info) => {
                    let sym_rec = info
                        .symbols
                        .into_iter()
                        .next()
                        .ok_or_else(|| anyhow!("symbol info missing"))?;
                    if let Some(existing) = FUT_RULES.get(sym) {
                        return Ok(existing.clone());
                    }
                    let max_leverage = self.fetch_max_leverage(sym).await;
                    let mut rules = rules_from_fut_symbol(sym_rec);
                    rules.max_leverage = max_leverage;
                    let rules = Arc::new(rules);
                    FUT_RULES.insert(sym.to_string(), rules.clone());
                    return Ok(rules);
                }
                Err(err) => {
                    last_error = Some(err);
                    if attempt < MAX_RETRIES {
                        let backoff = crate::utils::exponential_backoff(attempt, INITIAL_BACKOFF_MS, 2);
                        let backoff_ms = backoff.as_millis() as u64;
                        warn!(
                            error = ?last_error,
                            %sym,
                            attempt = attempt + 1,
                            max_retries = MAX_RETRIES + 1,
                            backoff_ms,
                            "failed to fetch futures symbol rules, retrying with exponential backoff..."
                        );
                        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                        continue;
                    }
                }
            }
        }
        error!(
            error = ?last_error,
            %sym,
            "CRITICAL: failed to fetch futures symbol rules after {} retries, cannot use global fallback (would cause LOT_SIZE/PRICE_FILTER errors)",
            MAX_RETRIES + 1
        );
        Err(anyhow!(
            "failed to fetch symbol rules for {} after {} retries: {}",
            sym,
            MAX_RETRIES + 1,
            last_error
                .map(|e| e.to_string())
                .unwrap_or_else(|| "unknown error".to_string())
        ))
    }
pub fn refresh_rules_for(&self, sym: &str) {
        FUT_RULES.remove(sym);
        info!(symbol = %sym, "CONNECTION: Rules cache invalidated for symbol");
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
        if let Some(balance) = BALANCE_CACHE.get(asset) {
            return Ok(*balance);
        }
        tracing::debug!(
            asset = %asset,
            "BALANCE_CACHE empty, falling back to REST API (normal during startup - WebSocket will populate cache)"
        );
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
            Some(b) => {
                let balance = Decimal::from_str(&b.available_balance)?;
                BALANCE_CACHE.insert(asset.to_string(), balance);
                balance
            }
            None => Decimal::ZERO,
        };
        Ok(amt)
    }
    pub async fn fetch_open_orders(&self, sym: &str) -> Result<Vec<VenueOrder>> {
        if let Some(orders) = OPEN_ORDERS_CACHE.get(sym) {
            return Ok(orders.clone());
        }
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
            let price = Decimal::from_str(&o.price)?;
            let qty = Decimal::from_str(&o.orig_qty)?;
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
        if !res.is_empty() {
            OPEN_ORDERS_CACHE.insert(sym.to_string(), res.clone());
        }
        Ok(res)
    }
    pub async fn fetch_all_positions(&self) -> Result<Vec<(String, Position)>> {
        let qs = format!(
            "timestamp={}&recvWindow={}",
            BinanceCommon::ts(),
            self.common.recv_window_ms
        );
        let sig = self.common.sign(&qs);
        let url = format!(
            "{}/fapi/v2/positionRisk?{}&signature={}",
            self.base, qs, sig
        );
        let positions: Vec<FutPosition> = send_json(
            self.common
                .client
                .get(url)
                .header("X-MBX-APIKEY", &self.common.api_key),
        )
        .await?;
        let mut result = Vec::new();
        for pos in positions {
            let qty = Decimal::from_str(&pos.position_amt).unwrap_or(Decimal::ZERO);
            if !qty.is_zero() {
                let symbol = pos.symbol.clone();
                let entry = Decimal::from_str(&pos.entry_price)?;
                let leverage = pos.leverage.parse::<u32>().unwrap_or(1);
                let liq = Decimal::from_str(&pos.liquidation_price).unwrap_or(Decimal::ZERO);
                let liq_px = if liq > Decimal::ZERO {
                    Some(Px(liq))
                } else {
                    None
                };
                let position = Position {
                    symbol: symbol.clone(),
                    qty: Qty(qty),
                    entry: Px(entry),
                    leverage,
                    liq_px,
                };
                result.push((symbol, position));
            }
        }
        Ok(result)
    }
    pub async fn place_trailing_stop_order(
        &self,
        sym: &str,
        activation_price: Px,
        callback_rate: f64,
        qty: Qty,
    ) -> Result<String> {
        let rules = self.rules_for(sym).await?;
        let effective_price_precision = if rules.tick_size.is_zero() {
            rules.price_precision
        } else {
            rules.price_precision.min(rules.tick_size.scale() as usize)
        };
        let effective_qty_precision = if rules.step_size.is_zero() {
            rules.qty_precision
        } else {
            rules.qty_precision.min(rules.step_size.scale() as usize)
        };
        let qty_str = crate::utils::format_decimal_fixed(qty.0, effective_qty_precision);
        let activation_price_str = crate::utils::format_decimal_fixed(activation_price.0, effective_price_precision);
        let callback_rate_pct = callback_rate * 100.0;
        let mut params = vec![
            format!("symbol={}", sym),
            "type=TRAILING_STOP_MARKET".to_string(),
            format!("activationPrice={}", activation_price_str),
            format!("callbackRate={}", callback_rate_pct),
            format!("quantity={}", qty_str),
            "reduceOnly=true".to_string(),
            format!("timestamp={}", BinanceCommon::ts()),
            format!("recvWindow={}", self.common.recv_window_ms),
        ];
        if self.hedge_mode {
            let position_side = if qty.0.is_sign_positive() {
                "LONG"
            } else {
                "SHORT"
            };
            params.push(format!("positionSide={}", position_side));
        }
        let qs = params.join("&");
        let sig = self.common.sign(&qs);
        let url = format!("{}/fapi/v1/order?{}&signature={}", self.base, qs, sig);
        let resp = ensure_success(self.common.client.post(&url).send().await?).await?;
        let order: FutPlacedOrder = resp.json().await?;
        Ok(order.order_id.to_string())
    }
    pub async fn fetch_position(&self, sym: &str) -> Result<Position> {
        if let Some(position) = POSITION_CACHE.get(sym) {
            return Ok(position.clone());
        }
        tracing::debug!(
            symbol = %sym,
            "POSITION_CACHE empty, falling back to REST API (normal during startup - WebSocket will populate cache)"
        );
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
        let positions: Vec<FutPosition> = send_json(
            self.common
                .client
                .get(url)
                .header("X-MBX-APIKEY", &self.common.api_key),
        )
            .await?;
        let matching_positions: Vec<&FutPosition> = positions
            .iter()
            .filter(|p| p.symbol.eq_ignore_ascii_case(sym))
            .collect();
        if matching_positions.is_empty() {
            return Err(anyhow!("position not found for symbol"));
        }
        if !self.hedge_mode {
            for pos in &matching_positions {
                if let Some(ref ps) = pos.position_side {
                    if ps == "LONG" || ps == "SHORT" {
                        warn!(
                            symbol = %sym,
                            position_side = %ps,
                            hedge_mode = self.hedge_mode,
                            "WARNING: positionSide is '{}' but hedge_mode is false - possible API inconsistency",
                            ps
                        );
                    }
                }
            }
            let net_qty: Decimal = matching_positions
                .iter()
                .map(|p| Decimal::from_str(&p.position_amt).unwrap_or(Decimal::ZERO))
                .sum();
            let pos = matching_positions[0];
            let qty = Decimal::from_str(&pos.position_amt)?;
            let entry = Decimal::from_str(&pos.entry_price)?;
            let leverage = pos.leverage.parse::<u32>().unwrap_or(1);
            let liq = Decimal::from_str(&pos.liquidation_price).unwrap_or(Decimal::ZERO);
            let liq_px = if liq > Decimal::ZERO {
                Some(Px(liq))
            } else {
                None
            };
            if matching_positions.len() > 1 {
                warn!(
                    symbol = %sym,
                    positions_count = matching_positions.len(),
                    net_qty = %net_qty,
                    hedge_mode = self.hedge_mode,
                    "WARNING: Multiple positions found in one-way mode, using net position"
                );
                let position = Position {
                    symbol: sym.to_string(),
                    qty: Qty(net_qty),
                    entry: Px(entry),
                    leverage,
                    liq_px,
                };
                POSITION_CACHE.insert(sym.to_string(), position.clone());
                Ok(position)
            } else {
                let position = Position {
                    symbol: sym.to_string(),
                    qty: Qty(qty),
                    entry: Px(entry),
                    leverage,
                    liq_px,
                };
                POSITION_CACHE.insert(sym.to_string(), position.clone());
                Ok(position)
            }
        } else {
            error!(
                symbol = %sym,
                "CRITICAL: Hedge mode detected but NOT supported! Config validation should have prevented this. This indicates a configuration error or runtime config change."
            );
            let mut long_qty = Decimal::ZERO;
            let mut short_qty = Decimal::ZERO;
            let mut long_entry = Decimal::ZERO;
            let mut short_entry = Decimal::ZERO;
            let mut long_leverage = 1u32;
            let mut short_leverage = 1u32;
            let mut long_liq_px = None;
            let mut short_liq_px = None;
            for pos in matching_positions {
                let qty = Decimal::from_str(&pos.position_amt).unwrap_or(Decimal::ZERO);
                let entry = Decimal::from_str(&pos.entry_price).unwrap_or(Decimal::ZERO);
                let lev = pos.leverage.parse::<u32>().unwrap_or(1);
                let liq = Decimal::from_str(&pos.liquidation_price).unwrap_or(Decimal::ZERO);
                match pos.position_side.as_deref() {
                    Some("LONG") => {
                        long_qty = qty;
                        long_entry = entry;
                        long_leverage = lev;
                        if liq > Decimal::ZERO {
                            long_liq_px = Some(Px(liq));
                        }
                    }
                    Some("SHORT") => {
                        short_qty = qty.abs();
                        short_entry = entry;
                        short_leverage = lev;
                        if liq > Decimal::ZERO {
                            short_liq_px = Some(Px(liq));
                        }
                    }
                    Some("BOTH") | None => {
                        warn!(
                            symbol = %sym,
                            position_side = ?pos.position_side,
                            hedge_mode = self.hedge_mode,
                            "WARNING: positionSide is 'BOTH' or None in hedge mode - possible API inconsistency"
                        );
                        long_qty = qty.max(Decimal::ZERO);
                        short_qty = (-qty).max(Decimal::ZERO);
                        long_entry = entry;
                        short_entry = entry;
                        long_leverage = lev;
                        short_leverage = lev;
                        if liq > Decimal::ZERO {
                            long_liq_px = Some(Px(liq));
                            short_liq_px = Some(Px(liq));
                        }
                    }
                    Some(other) => {
                        warn!(
                            symbol = %sym,
                            position_side = %other,
                            hedge_mode = self.hedge_mode,
                            "WARNING: Unknown positionSide value in hedge mode"
                        );
                    }
                }
            }
            if long_qty > Decimal::ZERO && short_qty > Decimal::ZERO {
                return Err(anyhow!(
                    "CRITICAL: Both LONG and SHORT positions exist for symbol {} in hedge mode. \
                     Current implementation cannot handle multiple positions per symbol. \
                     SHORT position (qty={}, entry={}) will be lost if we return LONG position. \
                     This would cause TP/SL to fail for the untracked SHORT position. \
                     \
                     Hedge mode is NOT supported. Please set binance.hedge_mode: false in config.yaml. \
                     \
                     If you need hedge mode, full support requires: \
                     - Position struct redesign to support multiple positions per symbol \
                     - Separate TP/SL tracking for LONG and SHORT \
                     - position_id-based closing \
                     - Separate position tracking in ORDERING state",
                    sym,
                    short_qty,
                    short_entry
                ));
            }
            let (final_qty, final_entry, final_leverage, final_liq_px) = if long_qty > Decimal::ZERO {
                (long_qty, long_entry, long_leverage, long_liq_px)
            } else if short_qty > Decimal::ZERO {
                (short_qty, short_entry, short_leverage, short_liq_px)
            } else {
                (Decimal::ZERO, Decimal::ZERO, 1u32, None)
            };
            let position = Position {
                symbol: sym.to_string(),
                qty: Qty(final_qty),
                entry: Px(final_entry),
                leverage: final_leverage,
                liq_px: final_liq_px,
            };
            POSITION_CACHE.insert(sym.to_string(), position.clone());
            Ok(position)
        }
    }
    pub async fn flatten_position(&self, sym: &str, use_market_only: bool) -> Result<()> {
        let initial_pos = match self.fetch_position(sym).await {
            Ok(pos) => pos,
            Err(e) => {
                if crate::utils::is_position_not_found_error(&e) {
                    info!(symbol = %sym, "position already closed (manual intervention detected), skipping close");
                    return Ok(());
                }
                return Err(e);
            }
        };
        let initial_qty = initial_pos.qty.0;
        if initial_qty.is_zero() {
            info!(symbol = %sym, "position already closed (zero quantity), skipping close");
            return Ok(());
        }
        let rules = self.rules_for(sym).await?;
        let initial_qty_abs = crate::utils::quantize_decimal(initial_qty.abs(), rules.step_size);
        if initial_qty_abs <= Decimal::ZERO {
            warn!(
                symbol = %sym,
                original_qty = %initial_qty,
                "quantized position size is zero, skipping close"
            );
            return Ok(());
        }
        let max_attempts = 3;
        #[allow(unused_assignments)]
        let mut limit_fallback_attempted = false;
        let mut growth_event_count = 0u32;
    const MAX_RETRIES_ON_GROWTH: u32 = 8;
        for attempt in 0..max_attempts {
            if limit_fallback_attempted {
                return Err(anyhow::anyhow!(
                    "LIMIT fallback already attempted and failed, cannot proceed with retry. Position may need manual intervention."
                ));
            }
            let current_pos = match self.fetch_position(sym).await {
                Ok(pos) => pos,
                Err(e) => {
                    if crate::utils::is_position_not_found_error(&e) {
                        info!(symbol = %sym, attempt, "position already closed during retry (manual intervention detected)");
                        return Ok(());
                    }
                    return Err(e);
                }
            };
            let current_qty = current_pos.qty.0;
            const MAX_POSITION_MULTIPLIER: f64 = 1.5;
            let early_position_multiplier = if initial_qty.abs() > Decimal::ZERO {
                current_qty.abs() / initial_qty.abs()
            } else {
                Decimal::ZERO
            };
            let early_multiplier_f64 = early_position_multiplier.to_f64().unwrap_or(0.0);
            if early_multiplier_f64 > MAX_POSITION_MULTIPLIER {
                tracing::error!(
                    symbol = %sym,
                    attempt = attempt + 1,
                    initial_qty = %initial_qty,
                    current_qty = %current_qty,
                    position_multiplier = early_multiplier_f64,
                    max_multiplier = MAX_POSITION_MULTIPLIER,
                    "POSITION CONTROL LOST (EARLY CHECK): Position is {:.2}x initial size (exceeds {:.2}x limit) before close attempt. Aborting immediately to prevent infinite loop and excessive risk.",
                    early_multiplier_f64,
                    MAX_POSITION_MULTIPLIER
                );
                return Err(anyhow!(
                    "Position control lost: position is {:.2}x initial size ({}) before close attempt, exceeding {:.2}x limit. Aborting immediately. Manual intervention required.",
                    early_multiplier_f64,
                    current_qty,
                    MAX_POSITION_MULTIPLIER
                ));
            }
            if current_qty.is_zero() {
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
            let remaining_qty = crate::utils::quantize_decimal(current_qty.abs(), rules.step_size);
            if remaining_qty <= Decimal::ZERO {
                return Ok(());
            }
            let side = if current_qty.is_sign_positive() {
                Side::Sell
            } else {
                Side::Buy
            };
            let effective_qty_precision = if rules.step_size.is_zero() {
                rules.qty_precision
            } else {
                rules.qty_precision.min(rules.step_size.scale() as usize)
            };
            let qty_str = crate::utils::format_decimal_fixed(remaining_qty, effective_qty_precision);
            let position_side = if self.hedge_mode {
                if current_qty.is_sign_positive() {
                    Some("LONG")
                } else {
                    Some("SHORT")
                }
            } else {
                None
            };
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
                "type=MARKET".to_string(),
                format!("quantity={}", qty_str),
                "reduceOnly=true".to_string(),
                format!("timestamp={}", BinanceCommon::ts()),
                format!("recvWindow={}", self.common.recv_window_ms),
            ];
            if let Some(pos_side) = position_side {
                params.push(format!("positionSide={}", pos_side));
            }
            let qs = params.join("&");
            let sig = self.common.sign(&qs);
            let url = format!("{}/fapi/v1/order?{}&signature={}", self.base, qs, sig);
            match send_void(
                self.common
                    .client
                    .post(&url)
                    .header("X-MBX-APIKEY", &self.common.api_key),
            )
                .await
            {
                Ok(_) => {
                    tokio::time::sleep(Duration::from_millis(1000)).await;
                    let verify_pos = if let Some(position) = POSITION_CACHE.get(sym) {
                        position.clone()
                    } else {
                        tracing::debug!(
                            symbol = %sym,
                            "POSITION_CACHE empty during position verification, falling back to REST API"
                        );
                        self.fetch_position(sym).await?
                    };
                    let verify_qty = verify_pos.qty.0;
                    if verify_qty.is_zero() {
                        info!(
                            symbol = %sym,
                            attempt = attempt + 1,
                            initial_qty = %initial_qty,
                            "position fully closed and verified"
                        );
                        return Ok(());
                    } else {
                        let position_grew_from_attempt = verify_qty.abs() > current_qty.abs();
                        let position_grew_from_initial = verify_qty.abs() > initial_qty.abs();
                        const MAX_POSITION_MULTIPLIER: f64 = 1.5;
                        let position_multiplier = if initial_qty.abs() > Decimal::ZERO {
                            verify_qty.abs() / initial_qty.abs()
                        } else {
                            Decimal::ZERO
                        };
                        let multiplier_f64 = position_multiplier.to_f64().unwrap_or(0.0);
                        if multiplier_f64 > MAX_POSITION_MULTIPLIER {
                            tracing::error!(
                                symbol = %sym,
                                attempt = attempt + 1,
                                initial_qty = %initial_qty,
                                current_qty_at_attempt = %current_qty,
                                verify_qty = %verify_qty,
                                position_multiplier = multiplier_f64,
                                max_multiplier = MAX_POSITION_MULTIPLIER,
                                "POSITION CONTROL LOST: Position grew to {:.2}x initial size (exceeds {:.2}x limit). Aborting immediately to prevent infinite loop and excessive risk.",
                                multiplier_f64,
                                MAX_POSITION_MULTIPLIER
                            );
                            return Err(anyhow!(
                                "Position control lost: position grew to {:.2}x initial size ({}), exceeding {:.2}x limit. Aborting immediately. Manual intervention required.",
                                multiplier_f64,
                                verify_qty,
                                MAX_POSITION_MULTIPLIER
                            ));
                        }
                        if position_grew_from_attempt || position_grew_from_initial {
                            let growth_from_initial = if initial_qty.abs() > Decimal::ZERO {
                                crate::utils::decimal_to_f64(
                                    crate::utils::calculate_percentage(verify_qty.abs() - initial_qty.abs(), initial_qty.abs())
                                )
                            } else {
                                0.0
                            };
                        const MAX_ACCEPTABLE_GROWTH_PCT: f64 = 10.0;
                            if growth_from_initial > MAX_ACCEPTABLE_GROWTH_PCT {
                                growth_event_count += 1;
                                const CRITICAL_GROWTH_THRESHOLD: u32 = 3;
                                if growth_event_count > CRITICAL_GROWTH_THRESHOLD && growth_event_count <= MAX_RETRIES_ON_GROWTH {
                                    tracing::error!(
                                        symbol = %sym,
                                        attempt = attempt + 1,
                                        growth_events = growth_event_count,
                                        initial_qty = %initial_qty,
                                        current_qty_at_attempt = %current_qty,
                                        verify_qty = %verify_qty,
                                        growth_pct = growth_from_initial,
                                        "CRITICAL: Position growth events exceeded threshold ({}). Position is growing faster than we can close. Using aggressive MARKET close.",
                                        CRITICAL_GROWTH_THRESHOLD
                                    );
                                }
                                if growth_event_count > MAX_RETRIES_ON_GROWTH {
                                    tracing::error!(
                                        symbol = %sym,
                                        attempt = attempt + 1,
                                        growth_events = growth_event_count,
                                        initial_qty = %initial_qty,
                                        current_qty_at_attempt = %current_qty,
                                        verify_qty = %verify_qty,
                                        growth_pct = growth_from_initial,
                                        max_growth_retries = MAX_RETRIES_ON_GROWTH,
                                        "POSITION GROWTH ABORT: Position grew {}% during close (exceeds {}% threshold) after {} growth events. Aborting to prevent infinite loop.",
                                        growth_from_initial,
                                        MAX_ACCEPTABLE_GROWTH_PCT,
                                        growth_event_count
                                    );
                                    return Err(anyhow!(
                                        "Position grew {}% during close (exceeds {}% threshold), aborting after {} growth events to prevent infinite loop. Position may need manual intervention.",
                                        growth_from_initial,
                                        MAX_ACCEPTABLE_GROWTH_PCT,
                                        growth_event_count
                                    ));
                                }
                                tracing::warn!(
                                    symbol = %sym,
                                    attempt = attempt + 1,
                                    growth_events = growth_event_count,
                                    max_growth_retries = MAX_RETRIES_ON_GROWTH,
                                    initial_qty = %initial_qty,
                                    current_qty_at_attempt = %current_qty,
                                    verify_qty = %verify_qty,
                                    growth_pct = growth_from_initial,
                                    grew_from_attempt = position_grew_from_attempt,
                                    grew_from_initial = position_grew_from_initial,
                                    "POSITION GROWTH DETECTED: Position grew {}% during close (exceeds {}% threshold). Continuing with retry (attempt {}/{}) - this may be normal in volatile markets with multiple simultaneous fills.",
                                    growth_from_initial,
                                    MAX_ACCEPTABLE_GROWTH_PCT,
                                    growth_event_count,
                                    MAX_RETRIES_ON_GROWTH
                                );
                            } else {
                                tracing::warn!(
                                    symbol = %sym,
                                    attempt = attempt + 1,
                                    initial_qty = %initial_qty,
                                    current_qty_at_attempt = %current_qty,
                                    verify_qty = %verify_qty,
                                    growth_pct = growth_from_initial,
                                    grew_from_attempt = position_grew_from_attempt,
                                    grew_from_initial = position_grew_from_initial,
                                    "POSITION GROWTH DETECTED: Position grew {}% during close (within acceptable {}% threshold). Continuing with retry - this may be normal in volatile markets.",
                                    growth_from_initial,
                                    MAX_ACCEPTABLE_GROWTH_PCT
                                );
                            }
                        }
                        let closed_amount = current_qty.abs() - verify_qty.abs();
                        let close_ratio = if current_qty.abs() > Decimal::ZERO {
                            closed_amount / current_qty.abs()
                        } else {
                            Decimal::ZERO
                        };
                        let remaining_pct = if initial_qty.abs() > Decimal::ZERO {
                            crate::utils::decimal_to_f64(
                                crate::utils::calculate_percentage(verify_qty.abs(), initial_qty.abs())
                            )
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
                            continue;
                        } else {
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
                    if crate::utils::is_position_already_closed_error(&e) {
                        info!(
                            symbol = %sym,
                            attempt = attempt + 1,
                            error = %e,
                            "position already closed (manual intervention or already closed), treating as success"
                        );
                        return Ok(());
                    }
                    if use_market_only {
                        warn!(
                            symbol = %sym,
                            attempt = attempt + 1,
                            error = %e,
                            remaining_qty = %remaining_qty,
                            "MARKET reduce-only failed in fast close mode, retrying..."
                        );
                        if attempt < max_attempts - 1 {
                            tokio::time::sleep(Duration::from_millis(500)).await;
                            continue;
                        } else {
                            return Err(anyhow::anyhow!(
                                "Failed to close position with MARKET reduce-only after {} attempts: {}",
                                max_attempts,
                                e
                            ));
                        }
                    }
                    if crate::utils::is_min_notional_error(&e) && !limit_fallback_attempted
                    {
                        warn!(symbol = %sym, "MIN_NOTIONAL error detected in flatten_position, invalidating rules cache");
                        self.refresh_rules_for(sym);
                        let rules = match self.rules_for(sym).await {
                            Ok(fresh_rules) => fresh_rules,
                            Err(e2) => {
                                warn!(symbol = %sym, error = %e2, "failed to refresh rules after MIN_NOTIONAL error, using existing rules");
                                rules.clone()
                            }
                        };
                        let (best_bid, best_ask) = {
                            if let Some(price_update) = PRICE_CACHE.get(sym) {
                                (price_update.bid, price_update.ask)
                            } else {
                                match self.best_prices(sym).await {
                                    Ok(prices) => prices,
                                    Err(e2) => {
                                        warn!(symbol = %sym, error = %e2, "failed to fetch best prices for dust check (WebSocket cache empty and REST API failed)");
                                        let assumed_min_price = Decimal::new(1, 2);
                                        let dust_threshold = rules.min_notional / assumed_min_price;
                                        warn!(
                                            symbol = %sym,
                                            min_notional = %rules.min_notional,
                                            assumed_price = %assumed_min_price,
                                            conservative_threshold = %dust_threshold,
                                            "Price fetch failed, using very conservative dust threshold (assumes price=0.01)"
                                        );
                                        if remaining_qty < dust_threshold {
                                            info!(
                                                symbol = %sym,
                                                remaining_qty = %remaining_qty,
                                                dust_threshold = %dust_threshold,
                                                min_notional = %rules.min_notional,
                                                "MIN_NOTIONAL error: remaining qty is dust (below threshold), considering position closed without LIMIT fallback"
                                            );
                                            return Ok(());
                                        }
                                        return Err(anyhow!(
                                            "MIN_NOTIONAL error and failed to fetch prices for dust check: {}",
                                            e2
                                        ));
                                    }
                                }
                            }
                        };
                        let current_price = if matches!(side, Side::Buy) {
                            best_ask.0
                        } else {
                            best_bid.0
                        };
                        let dust_threshold = crate::utils::calculate_dust_threshold(rules.min_notional, current_price);
                        if current_price.is_zero() {
                            warn!(
                                symbol = %sym,
                                min_notional = %rules.min_notional,
                                dust_threshold = %dust_threshold,
                                "Price is zero, using very conservative dust threshold (assumes price=0.01)"
                            );
                        }
                        if remaining_qty < dust_threshold {
                            info!(
                                symbol = %sym,
                                remaining_qty = %remaining_qty,
                                dust_threshold = %dust_threshold,
                                min_notional = %rules.min_notional,
                                "MIN_NOTIONAL error: remaining qty is dust (below threshold), considering position closed without LIMIT fallback"
                            );
                            return Ok(());
                        }
                        limit_fallback_attempted = true;
                        warn!(
                            symbol = %sym,
                            attempt = attempt + 1,
                            error = %e,
                            remaining_qty = %remaining_qty,
                            min_notional = %rules.min_notional,
                            "MIN_NOTIONAL error in reduce-only market close, trying limit reduce-only fallback (one-time)"
                        );
                        let tick_size = rules.tick_size;
                        let limit_price = if matches!(side, Side::Buy) {
                            best_bid.0 + tick_size
                        } else {
                            best_ask.0 - tick_size
                        };
                        let limit_price_quantized = crate::utils::quantize_decimal(limit_price, tick_size);
                        let effective_price_precision = if rules.tick_size.is_zero() {
                            rules.price_precision
                        } else {
                            rules.price_precision.min(rules.tick_size.scale() as usize)
                        };
                        let limit_price_str =
                            crate::utils::format_decimal_fixed(limit_price_quantized, effective_price_precision);
                        let mut limit_params = vec![
                            format!("symbol={}", sym),
                            format!(
                                "side={}",
                                if matches!(side, Side::Buy) {
                                    "BUY"
                                } else {
                                    "SELL"
                                }
                            ),
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
                        let limit_url = format!(
                            "{}/fapi/v1/order?{}&signature={}",
                            self.base, limit_qs, limit_sig
                        );
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
                                return Ok(());
                            }
                            Err(e2) => {
                                warn!(
                                    symbol = %sym,
                                    error = %e2,
                                    "MIN_NOTIONAL fallback: limit reduce-only order also failed"
                                );
                                let current_price = if matches!(side, Side::Buy) {
                                    best_ask.0
                                } else {
                                    best_bid.0
                                };
                                let dust_threshold = crate::utils::calculate_dust_threshold(rules.min_notional, current_price);
                                if current_price.is_zero() {
                                    warn!(
                                        symbol = %sym,
                                        min_notional = %rules.min_notional,
                                        dust_threshold = %dust_threshold,
                                        "Price is zero, using very conservative dust threshold (assumes price=0.01)"
                                    );
                                }
                                if remaining_qty < dust_threshold {
                                    info!(
                                        symbol = %sym,
                                        remaining_qty = %remaining_qty,
                                        dust_threshold = %dust_threshold,
                                        min_notional = %rules.min_notional,
                                        "MIN_NOTIONAL: remaining qty is dust (below threshold), considering position closed after LIMIT fallback failed"
                                    );
                                    return Ok(());
                                }
                                limit_fallback_attempted = true;
                                warn!(
                                    symbol = %sym,
                                    market_error = %e,
                                    limit_error = %e2,
                                    remaining_qty = %remaining_qty,
                                    min_notional = %rules.min_notional,
                                    "MIN_NOTIONAL: LIMIT fallback failed, will exit retry loop to prevent infinite loop"
                                );
                                continue;
                            }
                        }
                    } else {
                        if attempt < max_attempts - 1 {
                            warn!(
                                symbol = %sym,
                                attempt = attempt + 1,
                                error = %e,
                                "failed to close position, retrying..."
                            );
                            tokio::time::sleep(Duration::from_millis(500)).await;
                            continue;
                        } else {
                            return Err(e);
                        }
                    }
                }
            }
        }
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
        let (price_str, qty_str, price_quantized, qty_quantized) =
            Self::validate_and_format_order_params(px, qty, &rules, sym)?;
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
        let mut current_price_str = price_str;
        let mut current_qty_str = qty_str;
    const MAX_RETRIES: u32 = 3;
    const INITIAL_BACKOFF_MS: u64 = 100;
        let mut last_error = None;
        let mut order_result: Option<FutPlacedOrder> = None;
        let base_client_order_id = if !client_order_id.is_empty() {
            client_order_id.to_string()
        } else {
            format!("{}", BinanceCommon::ts())
        };
        for attempt in 0..=MAX_RETRIES {
            if attempt > 0 {
                match self.query_order_by_client_id(sym, &base_client_order_id).await {
                    Ok(Some(existing_order)) => {
                        info!(
                            %sym,
                            base_client_order_id = %base_client_order_id,
                            existing_order_id = existing_order.order_id,
                            attempt,
                            "Previous order found with base clientOrderId, returning existing order to prevent duplicate"
                        );
                        return Ok((existing_order.order_id.to_string(), existing_order.client_order_id));
                    }
                    Ok(None) => {
                        tracing::debug!(
                            %sym,
                            base_client_order_id = %base_client_order_id,
                            attempt,
                            "No existing order found with base clientOrderId, proceeding with retry"
                        );
                    }
                    Err(e) => {
                        warn!(
                            %sym,
                            base_client_order_id = %base_client_order_id,
                            attempt,
                            error = %e,
                            "Failed to query order by clientOrderId, proceeding with retry (may create duplicate if order exists)"
                        );
                    }
                }
            }
            let unique_client_order_id = if attempt == 0 {
                base_client_order_id.clone()
            } else {
                let retry_timestamp = BinanceCommon::ts();
                let random_suffix = retry_timestamp % 1000000;
                format!("{}-{}-{}", base_client_order_id, retry_timestamp, random_suffix)
            };
            let mut attempt_params = vec![
                format!("symbol={}", sym),
                format!("side={}", s_side),
                "type=LIMIT".to_string(),
                format!("timeInForce={}", tif_str),
                format!("price={}", current_price_str),
                format!("quantity={}", current_qty_str),
                format!("timestamp={}", BinanceCommon::ts()),
                format!("recvWindow={}", self.common.recv_window_ms),
                "newOrderRespType=RESULT".to_string(),
            ];
            if self.hedge_mode {
                let position_side = match side {
                    Side::Buy => "LONG",
                    Side::Sell => "SHORT",
                };
                attempt_params.push(format!("positionSide={}", position_side));
            }
            if !unique_client_order_id.is_empty() {
                if unique_client_order_id.len() <= 36
                    && unique_client_order_id
                        .chars()
                        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
                {
                    attempt_params.push(format!("newClientOrderId={}", unique_client_order_id));
                } else {
                    warn!(
                        %sym,
                        client_order_id = unique_client_order_id,
                        attempt,
                        "invalid clientOrderId format (max 36 chars, alphanumeric), skipping"
                    );
                }
            }
            let retry_qs = attempt_params.join("&");
            let retry_sig = self.common.sign(&retry_qs);
            let retry_url = format!(
                "{}/fapi/v1/order?{}&signature={}",
                self.base, retry_qs, retry_sig
            );
            match self
                .common
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
                                break;
                            }
                            Err(e) => {
                                if attempt < MAX_RETRIES {
                                    let backoff = crate::utils::exponential_backoff(attempt, INITIAL_BACKOFF_MS, 3);
                                    tracing::warn!(error = %e, attempt = attempt + 1, backoff_ms = backoff.as_millis(), "json parse error, retrying");
                                    tokio::time::sleep(backoff).await;
                                    last_error = Some(anyhow!("json parse error: {}", e));
                                    continue;
                                } else {
                                    return Err(e.into());
                                }
                            }
                        }
                    } else {
                        let body = resp.text().await.unwrap_or_default();
                        let body_lower = body.to_lowercase();
                        let should_refresh_rules = body_lower.contains("precision is over")
                            || body_lower.contains("-1111")
                            || crate::utils::contains_min_notional(&body_lower);
                        if should_refresh_rules {
                            let error_type = if body_lower.contains("precision is over") || body_lower.contains("-1111") {
                                "precision error (-1111)"
                            } else {
                                "MIN_NOTIONAL error (-1013)"
                            };
                            warn!(%sym, error_type, "rules-related error detected, invalidating cache immediately");
                            self.refresh_rules_for(sym);
                            if attempt < MAX_RETRIES {
                                warn!(%sym, attempt = attempt + 1, error_type, "refreshing rules and retrying");
                                match self.rules_for(sym).await {
                                    Ok(new_rules) => {
                                        match Self::validate_and_format_order_params(
                                            px, qty, &new_rules, sym,
                                        ) {
                                            Ok((new_price_str, new_qty_str, _, _)) => {
                                                let backoff = crate::utils::exponential_backoff(attempt, INITIAL_BACKOFF_MS, 3);
                                                tokio::time::sleep(backoff).await;
                                                current_price_str = new_price_str;
                                                current_qty_str = new_qty_str;
                                                last_error = Some(anyhow!("{} error, retrying with refreshed rules", error_type));
                                                continue;
                                            }
                                            Err(e) => {
                                                error!(%sym, error = %e, "validation failed after rules refresh, giving up");
                                                return Err(anyhow!("{} error, validation failed after rules refresh: {}", error_type, e));
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        error!(%sym, error = %e, "failed to refresh rules, giving up");
                                        return Err(anyhow!(
                                            "{} error, failed to refresh rules: {}",
                                            error_type,
                                            e
                                        ));
                                    }
                                }
                            } else {
                                error!(%sym, attempt, error_type, "after max retries, symbol should be quarantined (cache invalidated)");
                                return Err(anyhow!(
                                    "binance api error: {} - {} ({}, max retries)",
                                    status,
                                    body,
                                    error_type
                                ));
                            }
                        }
                        if is_permanent_error(status.as_u16(), &body) {
                            tracing::error!(%status, %body, attempt, "permanent error, no retry");
                            return Err(anyhow!(
                                "binance api error: {} - {} (permanent)",
                                status,
                                body
                            ));
                        }
                        if is_transient_error(status.as_u16(), &body) && attempt < MAX_RETRIES {
                            let backoff = crate::utils::exponential_backoff(attempt, INITIAL_BACKOFF_MS, 3);
                            tracing::warn!(%status, %body, attempt = attempt + 1, backoff_ms = backoff.as_millis(), "transient error, retrying with exponential backoff (unique clientOrderId)");
                            tokio::time::sleep(backoff).await;
                            last_error = Some(anyhow!("binance api error: {} - {}", status, body));
                            continue;
                        } else {
                            tracing::error!(%status, %body, attempt, "error after retries");
                            return Err(anyhow!("binance api error: {} - {}", status, body));
                        }
                    }
                }
                Err(e) => {
                    if attempt < MAX_RETRIES {
                        let backoff = crate::utils::exponential_backoff(attempt, INITIAL_BACKOFF_MS, 3);
                        tracing::warn!(error = %e, attempt = attempt + 1, backoff_ms = backoff.as_millis(), "network error, retrying with exponential backoff (unique clientOrderId)");
                        tokio::time::sleep(backoff).await;
                        last_error = Some(e.into());
                        continue;
                    } else {
                        tracing::error!(error = %e, attempt, "network error after retries");
                        return Err(e.into());
                    }
                }
            }
        }
        let order = order_result
            .ok_or_else(|| last_error.unwrap_or_else(|| anyhow!("unknown error after retries")))?;
        info!(
            %sym,
            ?side,
            price_quantized = %price_quantized,
            qty_quantized = %qty_quantized,
            price_str = %current_price_str,
            qty_str = %current_qty_str,
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
        if let Some(price_update) = PRICE_CACHE.get(sym) {
            return Ok((price_update.bid, price_update.ask));
        }
        warn!(
            symbol = %sym,
            "PRICE_CACHE empty in best_prices, falling back to REST API (should be rare)"
        );
        let url = format!("{}/fapi/v1/depth?symbol={}&limit=5", self.base, encode(sym));
        let d: OrderBookTop = send_json(self.common.client.get(url)).await?;
    use rust_decimal::Decimal;
        let best_bid = d.bids.get(0).ok_or_else(|| anyhow!("no bid"))?.0.clone();
        let best_ask = d.asks.get(0).ok_or_else(|| anyhow!("no ask"))?.0.clone();
        Ok((
            Px(Decimal::from_str(&best_bid)?),
            Px(Decimal::from_str(&best_ask)?),
        ))
    }
    async fn get_open_orders(&self, sym: &str) -> Result<Vec<VenueOrder>> {
        self.fetch_open_orders(sym).await
    }
    async fn get_position(&self, sym: &str) -> Result<Position> {
        self.fetch_position(sym).await
    }
    async fn get_all_positions(&self) -> Result<Vec<(String, Position)>> {
        BinanceFutures::fetch_all_positions(self).await
    }
    async fn place_trailing_stop_order(
        &self,
        symbol: &str,
        activation_price: Px,
        callback_rate: f64,
        quantity: Qty,
    ) -> Result<String> {
        BinanceFutures::place_trailing_stop_order(self, symbol, activation_price, callback_rate, quantity).await
    }
    async fn available_balance(&self, asset: &str) -> Result<Decimal> {
        BinanceFutures::available_balance(self, asset).await
    }
}
impl BinanceFutures {
    pub async fn query_order_by_client_id(
        &self,
        sym: &str,
        client_order_id: &str,
    ) -> Result<Option<FutPlacedOrder>> {
        let qs = format!(
            "symbol={}&origClientOrderId={}&timestamp={}&recvWindow={}",
            sym,
            client_order_id,
            BinanceCommon::ts(),
            self.common.recv_window_ms
        );
        let sig = self.common.sign(&qs);
        let url = format!("{}/fapi/v1/order?{}&signature={}", self.base, qs, sig);
        match self
            .common
            .client
            .get(&url)
            .header("X-MBX-APIKEY", &self.common.api_key)
            .send()
            .await
        {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    match resp.json::<FutPlacedOrder>().await {
                        Ok(order) => Ok(Some(order)),
                        Err(e) => {
                            tracing::debug!(
                                %sym,
                                client_order_id = %client_order_id,
                                error = %e,
                                "Failed to parse order query response, treating as order not found"
                            );
                            Ok(None)
                        }
                    }
                } else {
                    let body = resp.text().await.unwrap_or_default();
                    let body_lower = body.to_lowercase();
                    if body_lower.contains("-2013") || body_lower.contains("order does not exist") {
                        Ok(None)
                    } else {
                        Err(anyhow!("binance api error querying order: {} - {}", status, body))
                    }
                }
            }
            Err(e) => {
                Err(e.into())
            }
        }
    }
    pub fn validate_and_format_order_params(
        px: Px,
        qty: Qty,
        rules: &SymbolRules,
        sym: &str,
    ) -> Result<(String, String, Decimal, Decimal)> {
        let price_precision_from_tick = if rules.tick_size.is_zero() {
            rules.price_precision
        } else {
            rules.tick_size.scale() as usize
        };
        let qty_precision_from_step = if rules.step_size.is_zero() {
            rules.qty_precision
        } else {
            rules.step_size.scale() as usize
        };
        let effective_price_precision = rules.price_precision.min(price_precision_from_tick);
        let effective_qty_precision = rules.qty_precision.min(qty_precision_from_step);
        let price_quantized = crate::utils::quantize_decimal(px.0, rules.tick_size);
        let qty_quantized = crate::utils::quantize_decimal(qty.0.abs(), rules.step_size);
        let price = price_quantized
            .round_dp_with_strategy(effective_price_precision as u32, RoundingStrategy::ToNegativeInfinity);
        let qty_rounded =
            qty_quantized.round_dp_with_strategy(effective_qty_precision as u32, RoundingStrategy::ToNegativeInfinity);
    let qty_rounded = if qty_rounded.is_zero() && !qty_quantized.is_zero() {
        let re_quantized = crate::utils::quantize_decimal(qty_quantized, rules.step_size);
        if re_quantized.is_zero() {
            return Err(anyhow!(
                "Quantity becomes zero after quantization: qty_quantized={}, step_size={}, effective_qty_precision={}",
                qty_quantized,
                rules.step_size,
                effective_qty_precision
            ));
        }
        re_quantized
    } else {
        qty_rounded
    };
        let price_str = crate::utils::format_decimal_fixed(price, effective_price_precision);
        let qty_str = crate::utils::format_decimal_fixed(qty_rounded, effective_qty_precision);
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
        if price_fractional > effective_price_precision {
            let error_msg = format!(
                "CRITICAL: price_str fractional digits ({}) > effective_price_precision ({}) for {}",
                price_fractional, effective_price_precision, sym
            );
            tracing::error!(%sym, price_str, effective_price_precision, price_fractional, %error_msg);
            return Err(anyhow!(error_msg));
        }
        if qty_fractional > effective_qty_precision {
            let error_msg = format!(
                "CRITICAL: qty_str fractional digits ({}) > effective_qty_precision ({}) for {}",
                qty_fractional, effective_qty_precision, sym
            );
            tracing::error!(%sym, qty_str, effective_qty_precision, qty_fractional, %error_msg);
            return Err(anyhow!(error_msg));
        }
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
async fn ensure_success(resp: Response) -> Result<Response> {
    let status = resp.status();
    if status.is_success() {
        Ok(resp)
    } else {
        if status == 429 {
            let retry_after = resp.headers()
                .get("retry-after")
                .and_then(|h| h.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok());
            if let Some(seconds) = retry_after {
                tracing::warn!(
                    status = %status,
                    retry_after_seconds = seconds,
                    "Binance API rate limit exceeded (429), waiting {} seconds before retry",
                    seconds
                );
                tokio::time::sleep(std::time::Duration::from_secs(seconds)).await;
            } else {
                tracing::warn!(
                    status = %status,
                    "Binance API rate limit exceeded (429), no Retry-After header, waiting 1 second"
                );
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
        let body = resp.text().await.unwrap_or_default();
        let is_benign_error = body.contains("-4046") || body.contains("-4059")
            || body.contains("No need to change margin type")
            || body.contains("No need to change position side");
        let is_leverage_error = body.contains("-4028") || body.contains("Leverage") && body.contains("not valid");
        if is_benign_error {
            tracing::debug!(%status, %body, "binance api response (benign - already set correctly)");
        } else if is_leverage_error {
            tracing::warn!(%status, %body, "binance api error (leverage not valid - will try lower)");
        } else if status == 429 {
        } else {
            tracing::error!(%status, %body, "binance api error");
        }
        Err(anyhow!("binance api error: {} - {}", status, body))
    }
}
fn is_transient_error(status: u16, _body: &str) -> bool {
    match status {
        408 => true,
        429 => true,
        500..=599 => true,
        400 => {
            false
        }
        _ => false,
    }
}
fn is_permanent_error(status: u16, body: &str) -> bool {
    if status == 400 {
        let body_lower = body.to_lowercase();
        if body_lower.contains("precision is over") || body_lower.contains("-1111") {
            return false;
        }
        if crate::utils::contains_min_notional(&body_lower) {
            return false;
        }
        body_lower.contains("invalid")
            || body_lower.contains("margin")
            || body_lower.contains("insufficient balance")
    } else {
        false
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
