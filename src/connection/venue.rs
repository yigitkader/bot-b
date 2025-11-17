// Venue trait and Binance Futures implementation
// Exchange API implementations (REST API calls, order placement, position management)
// All Binance-specific exchange logic

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

// ============================================================================
// Venue Trait
// ============================================================================

/// Exchange interface trait
/// Implemented by BinanceFutures
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
    /// Get all open positions from exchange
    /// Returns Vec<(symbol, position)> for all positions with non-zero qty
    async fn get_all_positions(&self) -> Result<Vec<(String, Position)>>;
    /// Place trailing stop market order
    /// activation_price: Price at which trailing stop activates
    /// callback_rate: Trailing stop callback rate (0.001 = 0.1%, Binance expects percentage 0.1)
    /// quantity: Position quantity to close
    async fn place_trailing_stop_order(
        &self,
        symbol: &str,
        activation_price: Px,
        callback_rate: f64, // 0.001 = 0.1%
        quantity: Qty,
    ) -> Result<String>;
    async fn available_balance(&self, asset: &str) -> Result<Decimal>;
}

// ============================================================================
// Binance Exchange Types (from types.rs)
// ============================================================================
// FutFilter, FutExchangeInfo, FutExchangeSymbol are imported from crate::types

pub static FUT_RULES: Lazy<DashMap<String, Arc<SymbolRules>>> = Lazy::new(|| DashMap::new());

/// Price cache from WebSocket market data stream
/// Thread-safe price storage - updated by WebSocket, read by main loop
///
/// CONCURRENT WRITE BEHAVIOR:
/// - DashMap.insert() is atomic and thread-safe
/// - If multiple streams subscribe to the same symbol (duplicate subscription),
///   concurrent writes are safe: last write wins (correct for price data - we want latest)
/// - No data corruption or race conditions, but may have unnecessary writes
/// - Each symbol should ideally be in only one stream chunk to avoid duplicate updates
pub static PRICE_CACHE: Lazy<DashMap<String, PriceUpdate>> = Lazy::new(|| DashMap::new());

/// Position cache from WebSocket user data stream (ACCOUNT_UPDATE events)
/// Thread-safe position storage - updated by WebSocket, read by main loop
/// Binance recommendation: Use WebSocket instead of REST API for real-time data
pub static POSITION_CACHE: Lazy<DashMap<String, Position>> = Lazy::new(|| DashMap::new());

/// Balance cache from WebSocket user data stream (ACCOUNT_UPDATE events)
/// Thread-safe balance storage - updated by WebSocket, read by main loop
/// Binance recommendation: Use WebSocket instead of REST API for real-time data
pub static BALANCE_CACHE: Lazy<DashMap<String, Decimal>> = Lazy::new(|| DashMap::new());

/// Open orders cache from WebSocket user data stream (ORDER_TRADE_UPDATE events)
/// Thread-safe open orders storage - updated by WebSocket, read by main loop
/// Binance recommendation: Use WebSocket instead of REST API for real-time data
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
        return 8; // Default precision
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

// Precision calculation: use API value directly, not scale_from_step
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

// Use more reasonable fallback values
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
        // For now, set to None (will use config max_leverage instead)
        max_leverage: None,
    }
}

// ---- Ortak ----

/// Binance API common configuration
///
/// Thread-safety and performance optimization
/// - `client: Arc<Client>`: reqwest::Client is thread-safe, wrapping in Arc avoids unnecessary overhead
/// - `sign()` function is thread-safe (uses immutable data, read-only)
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
            // Return empty signature - API call will fail gracefully
                return String::new();
            }
        };
        mac.update(qs.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }
}

// ---- USDT-M Futures ----

#[derive(Clone)]
pub struct BinanceFutures {
    pub base: String, // e.g. https://fapi.binance.com
    pub common: BinanceCommon,
    pub hedge_mode: bool, // Is hedge mode (dual-side position) enabled?
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
    position_side: Option<String>, // "LONG" | "SHORT" | "BOTH" (hedge mode) or None (one-way mode)
#[serde(rename = "marginType", default)]
    margin_type: String, // "isolated" or "cross"
}


impl BinanceFutures {
/// Create BinanceFutures from config
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

/// Set leverage (per symbol) with automatic step-down on error
/// If leverage is rejected, tries lower values (100 -> 75 -> 50 -> 25 -> 20 -> 10 -> 5 -> 1)
/// Returns the actual leverage value that was successfully set
/// Must be set explicitly for each symbol at startup
/// Uses /fapi/v1/leverage endpoint for per-symbol leverage
    pub async fn set_leverage(&self, sym: &str, leverage: u32) -> Result<u32> {
        // Try leverage values in descending order if initial attempt fails
        // Common valid leverage values: 1, 2, 3, 5, 10, 20, 25, 50, 75, 100
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
                    // ✅ CRITICAL: Safe unwrap - last_error is Some(e) just above
                    // But use if let for extra safety to prevent panic
                    if let Some(ref error) = last_error {
                        let error_str = error.to_string().to_lowercase();
                        if error_str.contains("leverage") && (error_str.contains("not valid") || error_str.contains("-4028")) {
                            // Continue to next lower leverage value
                            tracing::debug!(
                                symbol = %sym,
                                attempted_leverage = lev,
                                "Leverage {}x not valid for symbol, trying lower value",
                                lev
                            );
                            continue;
                        } else {
                            // Other error (network, auth, etc.) - return immediately
                            warn!(symbol = %sym, leverage = lev, error = ?last_error, "Failed to set leverage (non-leverage error)");
                            // Safe unwrap - last_error is Some(e) from above
                            return Err(last_error.unwrap());
                        }
                    } else {
                        // Should never happen, but defensive programming
                        return Err(anyhow!("Unknown error setting leverage"));
                    }
                }
            }
        }
        
        // All leverage values failed
        Err(last_error.unwrap_or_else(|| anyhow!("All leverage values failed for symbol {}", sym)))
    }

/// Set position side mode (enable/disable hedge mode)
/// Must be set explicitly at startup
/// Uses /fapi/v1/positionSide/dual endpoint to enable/disable hedge mode
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
                // ✅ CRITICAL: Binance returns -4059 "No need to change position side" when already set correctly
                // This is not an error - the position side is already in the desired state
                let error_str = e.to_string();
                if error_str.contains("-4059") || error_str.contains("No need to change position side") {
                    info!(
                        dual_side = dual,
                        "position side mode already set correctly (no change needed)"
                    );
                    Ok(()) // Treat as success
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
                // ✅ CRITICAL: Binance returns -4046 "No need to change margin type" when already set correctly
                // This is not an error - the margin type is already in the desired state
                let error_str = e.to_string();
                if error_str.contains("-4046") || error_str.contains("No need to change margin type") {
                    info!(
                        %sym,
                        margin_type = %margin_type,
                        "margin type already set correctly (no change needed)"
                    );
                    Ok(()) // Treat as success
                } else {
                    warn!(%sym, margin_type = %margin_type, error = %e, "failed to set margin type");
                    Err(e)
                }
            }
        }
    }

    /// Fetch max leverage for a symbol from Binance leverage brackets API
    /// Returns None if API call fails (will use safe fallback instead of config max_leverage)
    /// ✅ CRITICAL: This endpoint requires authentication (signature + API key)
    /// Public so it can be called directly when coin's max leverage is needed
    pub async fn fetch_max_leverage(&self, sym: &str) -> Option<u32> {
        // Helper function to deserialize u32 from string or number
        // Binance API returns bracket and initialLeverage as strings in some cases
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
            // ✅ CRITICAL FIX: bracket and initialLeverage can be string or number
            // Binance API returns them as strings in some cases
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
        
        // ✅ CRITICAL: This endpoint requires authentication
        // Build authenticated request with signature
        let qs = format!(
            "timestamp={}&recvWindow={}",
            BinanceCommon::ts(),
            self.common.recv_window_ms
        );
        let sig = self.common.sign(&qs);
        let url = format!("{}/fapi/v1/leverageBrackets?{}&signature={}", self.base, qs, sig);
        
        // Try to fetch leverage brackets with better error handling
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

        // Check HTTP status
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

        // Try to parse response - handle both single symbol and array responses
        match resp.json::<serde_json::Value>().await {
            Ok(json) => {
                // Log raw response for debugging (first 500 chars to avoid huge logs)
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
                
                // Handle different response formats:
                // 1. Array of LeverageBracketsResponse: [{"symbol": "...", "brackets": [...]}]
                // 2. Single object: {"symbol": "...", "brackets": [...]}
                // 3. Array of brackets directly: [{"bracket": ..., "initialLeverage": ...}]
                // 4. New format: brackets array might not have "bracket" field (optional)
                
                // Try different response formats with more flexible parsing
                let brackets_result: Result<Vec<LeverageBracketsResponse>, _> = if json.is_array() {
                    // Try to parse as array of LeverageBracketsResponse
                    serde_json::from_value::<Vec<LeverageBracketsResponse>>(json.clone())
                        .or_else(|_| {
                            // Try to parse as array of LeverageBracket (direct brackets) - wrap in LeverageBracketsResponse
                            serde_json::from_value::<Vec<LeverageBracket>>(json.clone())
                                .map(|brackets| vec![LeverageBracketsResponse {
                                    symbol: sym.to_string(),
                                    brackets,
                                }])
                        })
                        .or_else(|_| {
                            // Try flexible parsing: brackets might have different field names
                            // Parse manually from JSON value
                            if let Some(arr) = json.as_array() {
                                let mut all_brackets = Vec::new();
                                for item in arr {
                                    if let Some(obj) = item.as_object() {
                                        // Check if this is a symbol entry with brackets
                                        if let (Some(symbol_val), Some(brackets_val)) = (obj.get("symbol"), obj.get("brackets")) {
                                            if let (Some(symbol_str), Some(brackets_arr)) = (symbol_val.as_str(), brackets_val.as_array()) {
                                                let mut brackets = Vec::new();
                                                for bracket_item in brackets_arr {
                                                    if let Some(bracket_obj) = bracket_item.as_object() {
                                                        // Try to extract initialLeverage (required)
                                                        // ✅ CRITICAL FIX: initialLeverage can be string or number
                                                        // API returns "50" as string, not 50 as number
                                                        let initial_leverage = bracket_obj.get("initialLeverage")
                                                            .and_then(|v| {
                                                                // Try as number first
                                                                v.as_u64()
                                                                    // If not number, try as string and parse
                                                                    .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
                                                            });
                                                        
                                                        if let Some(lev) = initial_leverage {
                                                            // ✅ CRITICAL FIX: bracket can also be string or number
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
                                    // Return parse error - will be caught and logged below
                                    serde_json::from_value::<Vec<LeverageBracketsResponse>>(serde_json::json!([]))
                                }
                            } else {
                                // Return parse error - will be caught and logged below
                                serde_json::from_value::<Vec<LeverageBracketsResponse>>(serde_json::json!({}))
                            }
                        })
                } else {
                    // Try to parse as single LeverageBracketsResponse
                    serde_json::from_value::<LeverageBracketsResponse>(json)
                        .map(|r| vec![r])
                };

                match brackets_result {
                    Ok(brackets_vec) => {
                        // Find brackets for this symbol or use first if single symbol
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

                        // Max leverage is the highest initial_leverage in brackets
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
                        // Log full error and response for debugging
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
                // JSON parse error
                tracing::debug!(
                    error = %e,
                    symbol = %sym,
                    "Failed to parse leverage brackets response as JSON"
                );
                None
            }
        }
    }

/// Get per-symbol metadata (tick_size, step_size, max_leverage)
/// Does not use global fallback - real rules required for each symbol
/// Using fallback can cause LOT_SIZE and PRICE_FILTER errors
    pub async fn rules_for(&self, sym: &str) -> Result<Arc<SymbolRules>> {
    // Double-check locking pattern - prevent race condition
    // First check: Is it in cache?
        if let Some(r) = FUT_RULES.get(sym) {
            return Ok(r.clone());
        }

    // Geçici hata durumunda retry mekanizması (max 2 retry)
    const MAX_RETRIES: u32 = 2;
    const INITIAL_BACKOFF_MS: u64 = 100; // Exponential backoff başlangıç değeri
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

                // Double-check - another thread might have added it
                    if let Some(existing) = FUT_RULES.get(sym) {
                        return Ok(existing.clone());
                    }

                    // Fetch max leverage from leverage brackets API
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
                    // Use exponential backoff for rate limit handling
                        let backoff_ms = INITIAL_BACKOFF_MS * 2_u64.pow(attempt);
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

    // All retries failed - return error
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

/// Force refresh rules for a specific symbol by removing it from cache
/// Next call to rules_for() will fetch fresh rules from exchange
pub fn refresh_rules_for(&self, sym: &str) {
        FUT_RULES.remove(sym);
        info!(symbol = %sym, "CONNECTION: Rules cache invalidated for symbol");
    }

/// Refresh rules for all cached symbols
/// Removes all entries from cache, forcing fresh fetch on next access
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

    // Fallback to REST API only if cache is empty (e.g., on startup before WebSocket data arrives)
        // This is normal during startup, so log at DEBUG level instead of WARN
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
            // Update cache with REST API result (for startup sync)
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

    // Update cache with REST API result (for startup sync)
        if !res.is_empty() {
            OPEN_ORDERS_CACHE.insert(sym.to_string(), res.clone());
        }

        Ok(res)
    }

    /// Fetch all open positions from exchange
    /// Returns Vec<(symbol, position)> for all positions with non-zero qty
    /// Used for order monitoring fallback and position reconciliation
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
            
            // Only include positions with non-zero qty
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

    /// Place trailing stop market order
    /// activation_price: Price at which trailing stop activates
    /// callback_rate: Trailing stop callback rate (0.001 = 0.1%, Binance expects percentage 0.1)
    /// quantity: Position quantity to close
    pub async fn place_trailing_stop_order(
        &self,
        sym: &str,
        activation_price: Px,
        callback_rate: f64, // 0.001 = 0.1%
        qty: Qty,
    ) -> Result<String> {
        let rules = self.rules_for(sym).await?;
        // Calculate effective precision from tick_size/step_size (source of truth)
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
        
        // Binance expects callback rate as percentage (0.1 not 0.001)
        // Example: callback_rate = 0.001 (0.1%) → callback_rate_pct = 0.1
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
            // Determine position side from quantity sign
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

/// Get current margin type for a symbol
/// Returns true if isolated, false if crossed
    pub async fn fetch_position(&self, sym: &str) -> Result<Position> {
        if let Some(position) = POSITION_CACHE.get(sym) {
            return Ok(position.clone());
        }

        // This is normal during startup, so log at DEBUG level instead of WARN
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

    // In one-way mode, positionSide should be "BOTH" or None, not "LONG"/"SHORT"
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

        // Tek-yön modunda: Net pozisyonu hesapla (birden fazla pozisyon olmamalı ama kontrol edelim)
            let net_qty: Decimal = matching_positions
                .iter()
                .map(|p| Decimal::from_str(&p.position_amt).unwrap_or(Decimal::ZERO))
                .sum();

        // Tek-yön modunda sadece bir pozisyon olmalı (net pozisyon)
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

        // In one-way mode, if multiple positions exist, use net position
            if matching_positions.len() > 1 {
                warn!(
                    symbol = %sym,
                    positions_count = matching_positions.len(),
                    net_qty = %net_qty,
                    hedge_mode = self.hedge_mode,
                    "WARNING: Multiple positions found in one-way mode, using net position"
                );
            // Net pozisyonu kullan (LONG - SHORT)
                let position = Position {
                    symbol: sym.to_string(),
                    qty: Qty(net_qty),
                    entry: Px(entry), // İlk pozisyonun entry'si (net pozisyon için ortalama hesaplanabilir ama basit tutuyoruz)
                    leverage,
                    liq_px,
                };
            // Update cache with REST API result (for startup sync)
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
            // Update cache with REST API result (for startup sync)
                POSITION_CACHE.insert(sym.to_string(), position.clone());
                Ok(position)
            }
        } else {
            // ✅ CRITICAL: Hedge mode is NOT supported (config validation should prevent this)
            // This code should never execute if config validation is working correctly
            // But we keep it for defensive programming and clear error messages
            //
            // Problem: Position struct only supports single position per symbol
            // If both LONG and SHORT positions exist, only one can be returned
            // This causes:
            // - SHORT position loss (not tracked)
            // - TP/SL won't work for untracked position
            // - System instability and potential financial losses
            //
            // NOTE: Config validation (config.rs line 548) should prevent hedge_mode=true
            // If this code executes, it means config validation was bypassed or config changed at runtime
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

            // positionSide check - in hedge mode should be "LONG" or "SHORT"
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
                    // SHORT pozisyon için qty negatif olabilir, mutlak değerini al
                        short_qty = qty.abs();
                        short_entry = entry;
                        short_leverage = lev;
                        if liq > Decimal::ZERO {
                            short_liq_px = Some(Px(liq));
                        }
                    }
                    Some("BOTH") | None => {
                    // In hedge mode, "BOTH" or None is not expected, log warning
                        warn!(
                            symbol = %sym,
                            position_side = ?pos.position_side,
                            hedge_mode = self.hedge_mode,
                            "WARNING: positionSide is 'BOTH' or None in hedge mode - possible API inconsistency"
                        );
                    // Net pozisyonu hesapla (qty zaten net olabilir)
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

            // ✅ CRITICAL: If both LONG and SHORT positions exist, return error
            // This prevents silent data loss where SHORT position is ignored
            // SHORT position loss would cause TP/SL to fail for that position
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
            // Only LONG position exists
                (long_qty, long_entry, long_leverage, long_liq_px)
            } else if short_qty > Decimal::ZERO {
            // Only SHORT position exists
                (short_qty, short_entry, short_leverage, short_liq_px)
            } else {
            // No position (zero)
                (Decimal::ZERO, Decimal::ZERO, 1u32, None)
            };

            let position = Position {
                symbol: sym.to_string(),
                qty: Qty(final_qty),
                entry: Px(final_entry),
                leverage: final_leverage,
                liq_px: final_liq_px,
            };
        // Update cache with REST API result
            POSITION_CACHE.insert(sym.to_string(), position.clone());
            Ok(position)
        }
    }

    pub async fn flatten_position(&self, sym: &str, use_market_only: bool) -> Result<()> {
        let initial_pos = match self.fetch_position(sym).await {
            Ok(pos) => pos,
            Err(e) => {
                if is_position_not_found_error(&e) {
                    info!(symbol = %sym, "position already closed (manual intervention detected), skipping close");
                    return Ok(());
                }
                return Err(e);
            }
        };
        let initial_qty = initial_pos.qty.0;

        if initial_qty.is_zero() {
        // Pozisyon zaten kapalı
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
        // ✅ Note: limit_fallback_attempted is set in inner scopes but checked at loop start (line 1428)
        // Compiler may warn about unused assignments, but the variable is actually used
        #[allow(unused_assignments)]
        let mut limit_fallback_attempted = false;
        let mut growth_event_count = 0u32;
    const MAX_RETRIES_ON_GROWTH: u32 = 8; // Max retries allowed when position grows (increased from 2 to 8 to handle volatile markets with multiple simultaneous fills)

        for attempt in 0..max_attempts {
            if limit_fallback_attempted {
                return Err(anyhow::anyhow!(
                    "LIMIT fallback already attempted and failed, cannot proceed with retry. Position may need manual intervention."
                ));
            }

        // Check current position on each attempt
            let current_pos = match self.fetch_position(sym).await {
                Ok(pos) => pos,
                Err(e) => {
                // Only handle specific "position not found" errors
                    if is_position_not_found_error(&e) {
                        info!(symbol = %sym, attempt, "position already closed during retry (manual intervention detected)");
                        return Ok(());
                    }
                // Other errors (network, API errors, etc.) should be retried
                    return Err(e);
                }
            };
            let current_qty = current_pos.qty.0;

            // ✅ CRITICAL: Early absolute limit check before attempting to close
            // Check if position has grown beyond 1.5x initial before even trying to close
            // Position büyümesi infinite loop'a yol açabilir - 1.5x'den fazla büyürse abort et
            const MAX_POSITION_MULTIPLIER: f64 = 1.5; // 1.5x = 50% growth - abort threshold
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
            let remaining_qty = crate::utils::quantize_decimal(current_qty.abs(), rules.step_size);

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

            // Calculate effective precision from step_size (source of truth)
            let effective_qty_precision = if rules.step_size.is_zero() {
                rules.qty_precision
            } else {
                rules.qty_precision.min(rules.step_size.scale() as usize)
            };
            let qty_str = crate::utils::format_decimal_fixed(remaining_qty, effective_qty_precision);

        // Add positionSide parameter if hedge mode is enabled
            let position_side = if self.hedge_mode {
                if current_qty.is_sign_positive() {
                    Some("LONG")
                } else {
                    Some("SHORT")
                }
            } else {
                None
            };

        // Ensure reduceOnly=true and type=MARKET
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
                    tokio::time::sleep(Duration::from_millis(1000)).await; // Exchange işlemesi için 1000ms bekle (Binance)
                    let verify_pos = if let Some(position) = POSITION_CACHE.get(sym) {
                        position.clone()
                    } else {
                    // Fallback to REST API only if cache is empty (normal during startup)
                        // This is normal during startup, so log at DEBUG level instead of WARN
                        tracing::debug!(
                            symbol = %sym,
                            "POSITION_CACHE empty during position verification, falling back to REST API"
                        );
                        self.fetch_position(sym).await?
                    };
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
                        let position_grew_from_attempt = verify_qty.abs() > current_qty.abs();
                        let position_grew_from_initial = verify_qty.abs() > initial_qty.abs();

                        // ✅ CRITICAL: Absolute limit check - abort immediately if position > 1.5x initial
                        // Position büyümesi infinite loop'a yol açabilir - 1.5x'den fazla büyürse abort et
                        // This prevents infinite loops in volatile markets where position keeps growing
                        // Example: Position 0.5 BTC → NEW order fills +0.3 BTC → Position 0.8 BTC (1.6x) → ABORT
                        const MAX_POSITION_MULTIPLIER: f64 = 1.5; // 1.5x = 50% growth - abort threshold
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
                        // Position büyümesi tespit edildi - büyüme yüzdesini hesapla
                            let growth_from_initial = if initial_qty.abs() > Decimal::ZERO {
                                ((verify_qty.abs() - initial_qty.abs()) / initial_qty.abs() * Decimal::from(100))
                                    .to_f64()
                                    .unwrap_or(0.0)
                            } else {
                                0.0
                            };

                        const MAX_ACCEPTABLE_GROWTH_PCT: f64 = 10.0;

                        // Position growth detection with abort mechanism
                            if growth_from_initial > MAX_ACCEPTABLE_GROWTH_PCT {
                                growth_event_count += 1;

                            // ✅ CRITICAL: If growth events exceed threshold, log critical warning
                            // This indicates position is growing faster than we can close it
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

                            // Too many growth events - abort to prevent infinite loop
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

                            // ⚠️ WARNING: Position %10'dan fazla büyüdü - volatile market'te normal olabilir
                            // Aynı anda birden fazla fill olabilir, devam et (ama growth event sayısını artır)
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
                            // Position grew but below 10% threshold - may be normal in volatile markets
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

                    // Calculate how much was closed in this attempt
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
                    let _error_lower = error_str.to_lowercase();


                    if is_position_already_closed_error(&e) {
                        info!(
                            symbol = %sym,
                            attempt = attempt + 1,
                            error = %e,
                            "position already closed (manual intervention or already closed), treating as success"
                        );
                        return Ok(());
                    }

                    if use_market_only {
                    // Hızlı kapanış gereksiniminde: Retry yap veya hata döndür, LIMIT fallback yapma
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

                    if is_min_notional_error(&e) && !limit_fallback_attempted
                    {
                        // ✅ CRITICAL: Invalidate cache immediately on MIN_NOTIONAL error
                        // This ensures limit fallback uses fresh rules
                        warn!(symbol = %sym, "MIN_NOTIONAL error detected in flatten_position, invalidating rules cache");
                        self.refresh_rules_for(sym);
                        
                        // Re-fetch rules with fresh data
                        let rules = match self.rules_for(sym).await {
                            Ok(fresh_rules) => fresh_rules,
                            Err(e2) => {
                                warn!(symbol = %sym, error = %e2, "failed to refresh rules after MIN_NOTIONAL error, using existing rules");
                                // Continue with existing rules if refresh fails
                                rules.clone()
                            }
                        };
                        
                        let (best_bid, best_ask) = {
                        // Try WebSocket cache first (fastest, most up-to-date)
                            if let Some(price_update) = PRICE_CACHE.get(sym) {
                                (price_update.bid, price_update.ask)
                            } else {
                            // Fallback to REST API only if cache is empty (should be rare)
                                match self.best_prices(sym).await {
                                    Ok(prices) => prices,
                                    Err(e2) => {
                                        warn!(symbol = %sym, error = %e2, "failed to fetch best prices for dust check (WebSocket cache empty and REST API failed)");
                                        let assumed_min_price = Decimal::new(1, 2); // 0.01 (very conservative)
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
                                    // Price fetch failed but not dust - continue to LIMIT fallback
                                        return Err(anyhow!(
                                            "MIN_NOTIONAL error and failed to fetch prices for dust check: {}",
                                            e2
                                        ));
                                    }
                                }
                            }
                        };

                        let dust_threshold = {
                            let current_price = if matches!(side, Side::Buy) {
                            // Short position closing: use ask price (BUY orders execute at ask)
                                best_ask.0
                            } else {
                            // Long position closing: use bid price (SELL orders execute at bid)
                                best_bid.0
                            };

                            if !current_price.is_zero() {
                                rules.min_notional / current_price
                            } else {
                                let assumed_min_price = Decimal::new(1, 2); // 0.01 (very conservative)
                                let conservative_threshold = rules.min_notional / assumed_min_price;

                                warn!(
                                    symbol = %sym,
                                    min_notional = %rules.min_notional,
                                    assumed_price = %assumed_min_price,
                                    conservative_threshold = %conservative_threshold,
                                    "Price is zero, using very conservative dust threshold (assumes price=0.01)"
                                );

                                conservative_threshold
                            }
                        };

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

                        // ✅ Set flag to prevent retry - will be checked at loop start (line 1428)
                        limit_fallback_attempted = true; // LIMIT fallback'i işaretle (tekrar denenmeyecek)
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
                        // Calculate effective precision from tick_size (source of truth)
                        let effective_price_precision = if rules.tick_size.is_zero() {
                            rules.price_precision
                        } else {
                            rules.price_precision.min(rules.tick_size.scale() as usize)
                        };
                        let limit_price_str =
                            crate::utils::format_decimal_fixed(limit_price_quantized, effective_price_precision);

                    // Limit reduce-only emri gönder
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
                            // Limit emri başarılı, pozisyon kapatılacak (emir fill olunca)
                                return Ok(());
                            }
                            Err(e2) => {
                                warn!(
                                    symbol = %sym,
                                    error = %e2,
                                    "MIN_NOTIONAL fallback: limit reduce-only order also failed"
                                );

                            // LIMIT fallback failed, check if remaining qty is dust
                            // Recalculate dust threshold with current prices (may have changed)
                                let dust_threshold = {
                                    let current_price = if matches!(side, Side::Buy) {
                                    // Short position closing: use ask price (BUY orders execute at ask)
                                        best_ask.0
                                    } else {
                                    // Long position closing: use bid price (SELL orders execute at bid)
                                        best_bid.0
                                    };
                                // Calculate dust threshold as min_notional / price (more accurate)
                                    if !current_price.is_zero() {
                                        rules.min_notional / current_price
                                    } else {
                                    // Price is zero - use very conservative threshold
                                        let assumed_min_price = Decimal::new(1, 2); // 0.01 (very conservative)
                                        let conservative_threshold = rules.min_notional / assumed_min_price;

                                        warn!(
                                            symbol = %sym,
                                            min_notional = %rules.min_notional,
                                            assumed_price = %assumed_min_price,
                                            conservative_threshold = %conservative_threshold,
                                            "Price is zero, using very conservative dust threshold (assumes price=0.01)"
                                        );

                                        conservative_threshold
                                    }
                                };

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

                            // LIMIT fallback failed and not dust - set flag to prevent retry
                                limit_fallback_attempted = true;
                                warn!(
                                    symbol = %sym,
                                    market_error = %e,
                                    limit_error = %e2,
                                    remaining_qty = %remaining_qty,
                                    min_notional = %rules.min_notional,
                                    "MIN_NOTIONAL: LIMIT fallback failed, will exit retry loop to prevent infinite loop"
                                );
                            // Retry loop'un başındaki kontrolle çıkış yapılacak
                            // continue ile retry loop'un başına dön, orada limit_fallback_attempted kontrolü yapılacak
                                continue;
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
                        // Wait before retry to avoid overloading exchange
                            tokio::time::sleep(Duration::from_millis(500)).await;
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
            Tif::PostOnly => "GTX", // Binance GTX: Post-only, cross ederse otomatik iptal
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

        // Store validated price/qty strings for reuse in retry loop
        // These may be updated if rules refresh is needed
        let mut current_price_str = price_str;
        let mut current_qty_str = qty_str;

    const MAX_RETRIES: u32 = 3;
    const INITIAL_BACKOFF_MS: u64 = 100;

        let mut last_error = None;
        let mut order_result: Option<FutPlacedOrder> = None;

        // ✅ CRITICAL: Generate base clientOrderId for tracking
        // Each retry will append attempt number to ensure uniqueness
        // This prevents duplicate orders if first request times out but Binance received it
        let base_client_order_id = if !client_order_id.is_empty() {
            client_order_id.to_string()
        } else {
            // If no clientOrderId provided, generate one
            format!("{}", BinanceCommon::ts())
        };

        for attempt in 0..=MAX_RETRIES {
            // ✅ CRITICAL: Before retrying, check if previous order exists to prevent duplicates
            // Problem: If first request times out but Binance received it, retry would create duplicate order
            // Solution: Check if order with base clientOrderId exists before retrying
            if attempt > 0 {
                // Check if previous order exists (query by origClientOrderId)
                match self.query_order_by_client_id(sym, &base_client_order_id).await {
                    Ok(Some(existing_order)) => {
                        // Previous order exists - return it instead of creating duplicate
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
                        // Order doesn't exist - safe to retry
                        tracing::debug!(
                            %sym,
                            base_client_order_id = %base_client_order_id,
                            attempt,
                            "No existing order found with base clientOrderId, proceeding with retry"
                        );
                    }
                    Err(e) => {
                        // Query failed - log warning but continue with retry (better to retry than skip)
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

            // ✅ CRITICAL: Generate unique clientOrderId for each retry attempt
            // Use timestamp + random suffix for retries to ensure uniqueness
            // Format: "{base}-{timestamp}-{random}" (e.g., "1234567890-1234567890123-abc")
            let unique_client_order_id = if attempt == 0 {
                // First attempt: use original clientOrderId
                base_client_order_id.clone()
            } else {
                // Retry attempts: append timestamp + random suffix to ensure uniqueness
                // This is more unique than just "-1", "-2" and prevents collisions
                let retry_timestamp = BinanceCommon::ts();
                // Use last 6 digits of timestamp as random component (simple but effective)
                let random_suffix = retry_timestamp % 1000000;
                format!("{}-{}-{}", base_client_order_id, retry_timestamp, random_suffix)
            };

            // Build params with unique clientOrderId for this attempt
            // Use current_price_str and current_qty_str (may be updated after rules refresh)
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

            // Add unique clientOrderId for this attempt
            if !unique_client_order_id.is_empty() {
                // Binance: max 36 karakter, alphanumeric
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

            // Her retry'de yeni request oluştur (unique clientOrderId ile)
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
                        let body = resp.text().await.unwrap_or_default();
                        let body_lower = body.to_lowercase();
                        let should_refresh_rules = body_lower.contains("precision is over")
                            || body_lower.contains("-1111")
                            || body_lower.contains("-1013")
                            || body_lower.contains("min notional")
                            || body_lower.contains("min_notional")
                            || body_lower.contains("below min notional");

                        // ✅ CRITICAL: Invalidate cache immediately on rules-related errors
                        // This must happen regardless of retry attempts, as the cache is stale
                        if should_refresh_rules {
                            let error_type = if body_lower.contains("precision is over") || body_lower.contains("-1111") {
                                "precision error (-1111)"
                            } else {
                                "MIN_NOTIONAL error (-1013)"
                            };
                            
                            // Always invalidate cache when rules-related errors occur
                            warn!(%sym, error_type, "rules-related error detected, invalidating cache immediately");
                            self.refresh_rules_for(sym);

                            if attempt < MAX_RETRIES {
                                warn!(%sym, attempt = attempt + 1, error_type, "refreshing rules and retrying");

                            // Fresh rules çek
                                match self.rules_for(sym).await {
                                    Ok(new_rules) => {
                                    // Yeni rules ile yeniden validate et
                                        match Self::validate_and_format_order_params(
                                            px, qty, &new_rules, sym,
                                        ) {
                                            Ok((new_price_str, new_qty_str, _, _)) => {
                                            // Yeni değerlerle retry
                                                let backoff_ms =
                                                    INITIAL_BACKOFF_MS * 3_u64.pow(attempt);
                                                tokio::time::sleep(Duration::from_millis(
                                                    backoff_ms,
                                                ))
                                                    .await;

                                            // Update price/qty strings for next iteration
                                                // These will be used to build attempt_params in the next loop iteration
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

                    // Kalıcı hata kontrolü
                        if is_permanent_error(status.as_u16(), &body) {
                            tracing::error!(%status, %body, attempt, "permanent error, no retry");
                            return Err(anyhow!(
                                "binance api error: {} - {} (permanent)",
                                status,
                                body
                            ));
                        }

                    // Transient hata kontrolü
                        if is_transient_error(status.as_u16(), &body) && attempt < MAX_RETRIES {
                            let backoff_ms = INITIAL_BACKOFF_MS * 3_u64.pow(attempt);
                            tracing::warn!(%status, %body, attempt = attempt + 1, backoff_ms, "transient error, retrying with exponential backoff (unique clientOrderId)");
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
                        tracing::warn!(error = %e, attempt = attempt + 1, backoff_ms, "network error, retrying with exponential backoff (unique clientOrderId)");
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
    // Call BinanceFutures::available_balance method (not trait method to avoid recursion)
        BinanceFutures::available_balance(self, asset).await
    }
}

impl BinanceFutures {
    /// Query order by clientOrderId (origClientOrderId)
    /// Returns Some(order) if order exists, None if not found, Err on API error
    /// Used to check if previous order exists before retrying to prevent duplicates
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
                            // JSON parse error - treat as order not found
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
                    
                    // Check if error is "order not found" (-2013)
                    if body_lower.contains("-2013") || body_lower.contains("order does not exist") {
                        // Order doesn't exist - this is expected, return None
                        Ok(None)
                    } else {
                        // Other error - return error
                        Err(anyhow!("binance api error querying order: {} - {}", status, body))
                    }
                }
            }
            Err(e) => {
                // Network error - return error (caller will handle)
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
        // ✅ CRITICAL FIX: Calculate precision from tick_size/step_size instead of API precision
        // Problem: API precision might be incorrect or stale, causing "Precision is over" errors (-1111)
        // Solution: Always calculate precision from tick_size/step_size, which are the source of truth
        // Binance requires that formatted values match the precision implied by tick_size/step_size
        
        // Calculate price precision from tick_size (source of truth)
        let price_precision_from_tick = if rules.tick_size.is_zero() {
            rules.price_precision // Fallback to API precision if tick_size is zero
        } else {
            rules.tick_size.scale() as usize
        };
        
        // Calculate quantity precision from step_size (source of truth)
        let qty_precision_from_step = if rules.step_size.is_zero() {
            rules.qty_precision // Fallback to API precision if step_size is zero
        } else {
            rules.step_size.scale() as usize
        };
        
        // Use the minimum of API precision and calculated precision (safety)
        // This ensures we never exceed Binance's actual requirements
        let effective_price_precision = rules.price_precision.min(price_precision_from_tick);
        let effective_qty_precision = rules.qty_precision.min(qty_precision_from_step);

    // 1. Quantize: step_size'a göre floor
        let price_quantized = crate::utils::quantize_decimal(px.0, rules.tick_size);
        let qty_quantized = crate::utils::quantize_decimal(qty.0.abs(), rules.step_size);

    // 2. Round: precision'a göre round et
    // Use ToNegativeInfinity (floor) instead of ToZero for safety
        let price = price_quantized
            .round_dp_with_strategy(effective_price_precision as u32, RoundingStrategy::ToNegativeInfinity);
        let qty_rounded =
            qty_quantized.round_dp_with_strategy(effective_qty_precision as u32, RoundingStrategy::ToNegativeInfinity);

    // ✅ CRITICAL: Check if qty_rounded is zero after rounding
    // Problem: If qty_precision is too small, rounding can make quantity zero
    // Example: qty_quantized = 227197.118, qty_precision = 0 → qty_rounded = 227197 (OK)
    //          qty_quantized = 0.001, qty_precision = 0 → qty_rounded = 0 (BAD)
    // Solution: If qty_rounded is zero but qty_quantized is not, use qty_quantized instead
    // This prevents "below min notional after validation (0 < 100)" errors
    let qty_rounded = if qty_rounded.is_zero() && !qty_quantized.is_zero() {
        // qty_rounded is zero but qty_quantized is not - rounding precision issue
        // Use qty_quantized but ensure it's still quantized to step_size
        // Re-quantize to ensure it's still a multiple of step_size
        let re_quantized = crate::utils::quantize_decimal(qty_quantized, rules.step_size);
        if re_quantized.is_zero() {
            // Even re-quantized is zero - this is a real problem
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

    // 3. Format: precision'a göre string'e çevir
        let price_str = crate::utils::format_decimal_fixed(price, effective_price_precision);
        let qty_str = crate::utils::format_decimal_fixed(qty_rounded, effective_qty_precision);

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

/// Check if error indicates position not found (already closed)
///
/// Returns true if error message contains position not found indicators.
/// This is used to handle cases where position was manually closed or doesn't exist.
fn is_position_not_found_error(error: &anyhow::Error) -> bool {
    let error_str = error.to_string().to_lowercase();
    error_str.contains("position not found")
        || error_str.contains("no position")
        || error_str.contains("-2011") // Binance: Unknown order (position not found)
}

/// Check if error indicates MIN_NOTIONAL error
///
/// Returns true if error message contains MIN_NOTIONAL error indicators.
/// This is used to trigger LIMIT fallback for small position sizes.
fn is_min_notional_error(error: &anyhow::Error) -> bool {
    let error_str = error.to_string().to_lowercase();
    error_str.contains("-1013")
        || error_str.contains("min notional")
        || error_str.contains("min_notional")
}

/// Check if error indicates position already closed or invalid state
///
/// Returns true if error message indicates position is already closed or in invalid state.
/// This is used to treat certain errors as success (position already closed).
fn is_position_already_closed_error(error: &anyhow::Error) -> bool {
    let error_str = error.to_string().to_lowercase();
    error_str.contains("position not found")
        || error_str.contains("no position")
        || error_str.contains("position already closed")
        || error_str.contains("reduceonly")
        || error_str.contains("reduce only")
        || error_str.contains("-2011") // Binance: "Unknown order sent"
        || error_str.contains("-2019") // Binance: "Margin is insufficient"
        || error_str.contains("-2021") // Binance: "Order would immediately match"
}

// quantize_decimal and format_decimal_fixed are in utils.rs

async fn ensure_success(resp: Response) -> Result<Response> {
    let status = resp.status();
    if status.is_success() {
        Ok(resp)
    } else {
        // ✅ NEW: Handle 429 Too Many Requests with Retry-After header
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
                // Wait for retry-after duration
                tokio::time::sleep(std::time::Duration::from_secs(seconds)).await;
            } else {
                // No retry-after header, use default 1 second
                tracing::warn!(
                    status = %status,
                    "Binance API rate limit exceeded (429), no Retry-After header, waiting 1 second"
                );
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
        
        let body = resp.text().await.unwrap_or_default();
        
        // ✅ CRITICAL: Some Binance errors are not actual errors but informational messages
        // -4046: "No need to change margin type" - margin type already set correctly
        // -4059: "No need to change position side" - position side already set correctly
        // -4028: "Leverage X is not valid" - leverage not valid for symbol (expected, system will try lower)
        // These are handled as success or retry in their respective functions
        // Log them at appropriate level instead of ERROR to reduce log noise
        let is_benign_error = body.contains("-4046") || body.contains("-4059")
            || body.contains("No need to change margin type")
            || body.contains("No need to change position side");
        
        let is_leverage_error = body.contains("-4028") || body.contains("Leverage") && body.contains("not valid");
        
        if is_benign_error {
            tracing::debug!(%status, %body, "binance api response (benign - already set correctly)");
        } else if is_leverage_error {
            // Leverage errors are expected - system will automatically try lower leverage
            // Log as WARN instead of ERROR to reduce noise
            tracing::warn!(%status, %body, "binance api error (leverage not valid - will try lower)");
        } else if status == 429 {
            // 429 already logged above with retry-after info
            // Don't log again as ERROR
        } else {
            tracing::error!(%status, %body, "binance api error");
        }
        
        Err(anyhow!("binance api error: {} - {}", status, body))
    }
}

fn is_transient_error(status: u16, _body: &str) -> bool {
    match status {
        408 => true,       // Request Timeout
        429 => true,       // Too Many Requests
        500..=599 => true, // Server Errors
        400 => {
            false
        }
        _ => false, // Other errors are permanent
    }
}

/// Check if error is permanent (should symbol be disabled?)
/// Errors like "invalid", "margin", "precision" are permanent
fn is_permanent_error(status: u16, body: &str) -> bool {
    if status == 400 {
        let body_lower = body.to_lowercase();
        if body_lower.contains("precision is over") || body_lower.contains("-1111") {
            return false; // Precision error can be retried
        }
        if body_lower.contains("-1013")
            || body_lower.contains("min notional")
            || body_lower.contains("min_notional")
            || body_lower.contains("below min notional")
        {
            return false; // MIN_NOTIONAL hatası retry edilebilir (rules refresh ile)
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


// ============================================================================
// Binance WebSocket Module (from binance_ws.rs)
// ============================================================================

