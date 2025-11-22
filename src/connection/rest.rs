use crate::types::{
    api::{
        ExchangeInfoResponse, ForceOrderRecord, LeverageBracketResponse, OpenInterestResponse,
        PositionRiskResponse, PremiumIndex, DepthSnapshot, FuturesBalance,
    },
    connection::Connection,
    core::{NewOrderRequest, Side, SymbolPrecision},
    events::{BalanceSnapshot, MarketTick, PositionUpdate},
};
use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use hex;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use serde_urlencoded;

type HmacSha256 = Hmac<Sha256>;

pub fn calculate_limit_price(side: &Side, best_bid: f64, best_ask: f64, tick_size: f64) -> f64 {
    let price = match side {
        Side::Long => best_ask,
        Side::Short => best_bid,
    };
    if tick_size > 0.0 {
        (price / tick_size).round() * tick_size
    } else {
        price
    }
}

pub async fn calculate_order_price(conn: &Connection, symbol: &str, side: &Side) -> Result<f64> {
    let depth = fetch_depth_snapshot(conn).await?;

    let best_bid = depth
        .bids
        .first()
        .and_then(|lvl| lvl[0].parse::<f64>().ok())
        .ok_or_else(|| anyhow!("invalid best bid"))?;

    let best_ask = depth
        .asks
        .first()
        .and_then(|lvl| lvl[0].parse::<f64>().ok())
        .ok_or_else(|| anyhow!("invalid best ask"))?;

    let symbol_info = fetch_symbol_info(conn, symbol).await?;
    let price = calculate_limit_price(side, best_bid, best_ask, symbol_info.tick_size);

    Ok(price)
}

pub async fn send_order(conn: &Connection, order: NewOrderRequest) -> Result<()> {
    ensure_credentials(conn)?;

    let NewOrderRequest {
        symbol,
        side,
        quantity,
        reduce_only,
        client_order_id,
    } = order;

    if quantity <= 0.0 {
        return Err(anyhow!("order quantity must be positive"));
    }

    set_margin_mode(conn, &symbol, conn.config.use_isolated_margin).await?;
    set_leverage(conn, &symbol, conn.config.leverage).await?;
    let symbol_info = fetch_symbol_info(conn, &symbol).await?;
    let qty_rounded = round_to_step_size(quantity, symbol_info.step_size);
    let qty_formatted = format_quantity(qty_rounded, symbol_info.step_size);

    let side_str = side.to_binance_str();

    let order_type = if reduce_only { "MARKET" } else { "LIMIT" };

    let mut params = vec![
        ("symbol".to_string(), symbol.clone()),
        ("side".to_string(), side_str.to_string()),
        ("type".to_string(), order_type.to_string()),
        ("quantity".to_string(), qty_formatted),
    ];

    if order_type == "LIMIT" {
        match fetch_depth_snapshot(conn).await {
            Ok(depth) => {
                let best_bid = depth.bids.first().and_then(|lvl| lvl[0].parse::<f64>().ok()).unwrap_or(0.0);
                let best_ask = depth.asks.first().and_then(|lvl| lvl[0].parse::<f64>().ok()).unwrap_or(0.0);

                if best_bid > 0.0 && best_ask > 0.0 {
                    let limit_price = calculate_limit_price(&side, best_bid, best_ask, symbol_info.tick_size);
                    let price_formatted = if symbol_info.tick_size > 0.0 {
                        format_quantity(limit_price, symbol_info.tick_size)
                    } else {
                        format!("{:.8}", limit_price)
                    };
                    params.push(("price".to_string(), price_formatted));
                    params.push(("timeInForce".to_string(), "GTC".to_string()));
                } else {
                    log::warn!("CONNECTION: invalid bid/ask prices, falling back to market order");
                    params[2] = ("type".to_string(), "MARKET".to_string());
                }
            }
            Err(err) => {
                log::warn!("CONNECTION: failed to fetch depth snapshot for limit price: {err:?}, falling back to market order");
                params[2] = ("type".to_string(), "MARKET".to_string());
            }
        }
    }

    if reduce_only {
        params.push(("reduceOnly".into(), "true".into()));
    }
    if let Some(id) = client_order_id {
        params.push(("newClientOrderId".into(), id));
    }

    signed_post(conn, "/fapi/v1/order", params).await?;
    log::info!(
        "CONNECTION: order sent (type={}, reduce_only={})",
        order_type, reduce_only
    );
    Ok(())
}

pub async fn send_order_fast(
    conn: &Connection,
    order: &NewOrderRequest,
    symbol_info: &SymbolPrecision,
    _best_bid: f64,
    _best_ask: f64,
) -> Result<()> {
    ensure_credentials(conn)?;

    let NewOrderRequest {
        symbol,
        side,
        quantity,
        reduce_only,
        client_order_id,
    } = order;

    if *quantity <= 0.0 {
        return Err(anyhow!("order quantity must be positive"));
    }

    let qty_rounded = round_to_step_size(*quantity, symbol_info.step_size);
    let qty_formatted = format_quantity(qty_rounded, symbol_info.step_size);

    let mut params = vec![
        ("symbol".to_string(), symbol.clone()),
        ("side".to_string(), side.to_binance_str().to_string()),
        ("type".to_string(), "MARKET".to_string()),
        ("quantity".to_string(), qty_formatted),
    ];

    if *reduce_only {
        params.push(("reduceOnly".into(), "true".into()));
    }
    if let Some(id) = client_order_id {
        params.push(("newClientOrderId".into(), id.clone()));
    }

    signed_post(conn, "/fapi/v1/order", params).await?;
    
    Ok(())
}

pub async fn fetch_max_leverage(conn: &Connection, symbol: &str) -> Result<f64> {
    let params = vec![("symbol".to_string(), symbol.to_string())];
    let response = signed_get(conn, "/fapi/v1/leverageBracket", params)
        .await?
        .json::<Vec<LeverageBracketResponse>>()
        .await
        .context("failed to parse leverage bracket response")?;

    let bracket_response = response
        .into_iter()
        .find(|bracket| bracket.symbol == symbol)
        .ok_or_else(|| anyhow!("leverage bracket not found for symbol: {}", symbol))?;

    let max_leverage = bracket_response
        .brackets
        .iter()
        .map(|b| b.initial_leverage as f64)
        .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .ok_or_else(|| anyhow!("no leverage brackets found for symbol: {}", symbol))?;

    Ok(max_leverage)
}

pub async fn set_margin_mode(conn: &Connection, symbol: &str, isolated: bool) -> Result<()> {
    ensure_credentials(conn)?;

    let margin_type = if isolated { "ISOLATED" } else { "CROSSED" };
    let params = vec![
        ("symbol".to_string(), symbol.to_string()),
        ("marginType".to_string(), margin_type.to_string()),
    ];

    signed_post(conn, "/fapi/v1/marginType", params).await?;
    log::info!(
        "CONNECTION: margin mode set to {} for {}",
        margin_type, symbol
    );
    Ok(())
}

pub async fn set_leverage(conn: &Connection, symbol: &str, leverage: f64) -> Result<()> {
    ensure_credentials(conn)?;

    if leverage <= 0.0 {
        return Err(anyhow!("leverage must be positive"));
    }

    let max_leverage = fetch_max_leverage(conn, symbol).await?;

    if leverage > max_leverage {
        return Err(anyhow!(
            "leverage {}x exceeds maximum {}x for symbol {}",
            leverage,
            max_leverage,
            symbol
        ));
    }

    let params = vec![
        ("symbol".to_string(), symbol.to_string()),
        ("leverage".to_string(), leverage.to_string()),
    ];

    signed_post(conn, "/fapi/v1/leverage", params).await?;
    log::info!(
        "CONNECTION: leverage set to {}x for {} (max: {}x)",
        leverage, symbol, max_leverage
    );
    Ok(())
}

pub async fn fetch_symbol_info(conn: &Connection, symbol: &str) -> Result<SymbolPrecision> {
    let url = format!("{}/fapi/v1/exchangeInfo", conn.config.base_url);
    let response = conn
        .http
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .json::<ExchangeInfoResponse>()
        .await
        .context("failed to parse exchange info response")?;

    let symbol_info = response
        .symbols
        .into_iter()
        .find(|s| s.symbol == symbol)
        .ok_or_else(|| anyhow!("symbol not found in exchange info: {}", symbol))?;

    let mut step_size = 0.001;
    let mut min_quantity = 0.001;
    let mut max_quantity = f64::MAX;
    let mut tick_size = 0.01;
    let mut min_price = 0.0;
    let mut max_price = f64::MAX;

    for filter in symbol_info.filters {
        match filter.filter_type.as_str() {
            "LOT_SIZE" => {
                if let (Some(ss), Some(mq), Some(mqx)) = (
                    filter.data.get("stepSize").and_then(|v| v.as_str()),
                    filter.data.get("minQty").and_then(|v| v.as_str()),
                    filter.data.get("maxQty").and_then(|v| v.as_str()),
                ) {
                    step_size = ss.parse().unwrap_or(0.001);
                    min_quantity = mq.parse().unwrap_or(0.001);
                    max_quantity = mqx.parse().unwrap_or(f64::MAX);
                }
            }
            "PRICE_FILTER" => {
                if let (Some(ts), Some(mp), Some(mxp)) = (
                    filter.data.get("tickSize").and_then(|v| v.as_str()),
                    filter.data.get("minPrice").and_then(|v| v.as_str()),
                    filter.data.get("maxPrice").and_then(|v| v.as_str()),
                ) {
                    tick_size = ts.parse().unwrap_or(0.01);
                    min_price = mp.parse().unwrap_or(0.0);
                    max_price = mxp.parse().unwrap_or(f64::MAX);
                }
            }
            _ => {}
        }
    }

    Ok(SymbolPrecision {
        step_size,
        min_quantity,
        max_quantity,
        tick_size,
        min_price,
        max_price,
    })
}

pub fn round_to_step_size(quantity: f64, step_size: f64) -> f64 {
    if step_size <= 0.0 {
        return quantity;
    }
    (quantity / step_size).floor() * step_size
}

pub fn format_quantity(quantity: f64, step_size: f64) -> String {
    if step_size <= 0.0 {
        return format!("{:.8}", quantity);
    }
    let step_str = format!("{:.10}", step_size);
    let decimal_places = if let Some(dot_pos) = step_str.find('.') {
        let after_dot = &step_str[dot_pos + 1..];
        after_dot.trim_end_matches('0').len()
    } else {
        0
    };
    format!("{:.1$}", quantity, decimal_places.min(8))
}

pub async fn fetch_balance(conn: &Connection) -> Result<Vec<BalanceSnapshot>> {
    ensure_credentials(conn)?;

    let response = signed_get(conn, "/fapi/v2/balance", Vec::<(String, String)>::new())
        .await?
        .json::<Vec<FuturesBalance>>()
        .await
        .context("failed to parse balance response")?;

    let critical_assets: Vec<&str> = vec!["USDT", "USDC", &conn.config.quote_asset];

    let mut snapshots = Vec::new();
    let mut critical_parse_errors = Vec::new();

    for record in response {
        let asset = &record.asset;
        let is_critical = critical_assets
            .iter()
            .any(|&critical| asset.eq_ignore_ascii_case(critical));

        match record.available_balance.parse::<f64>() {
            Ok(free) => {
                snapshots.push(BalanceSnapshot {
                    asset: record.asset,
                    free,
                    ts: Utc::now(),
                });
            }
            Err(err) => {
                if is_critical {
                    critical_parse_errors.push(format!(
                        "Failed to parse {} balance: '{}' - {}",
                        asset, record.available_balance, err
                    ));
                } else {
                    log::warn!("CONNECTION: failed to parse balance for asset {}: '{}' - {}", asset, record.available_balance, err);
                }
            }
        }
    }

    if !critical_parse_errors.is_empty() {
        let error_msg = format!(
            "Critical balance parse failures: {}",
            critical_parse_errors.join("; ")
        );
        return Err(anyhow!(error_msg));
    }

    let snapshot_assets: Vec<&str> = snapshots.iter().map(|s| s.asset.as_str()).collect();
    for critical in &critical_assets {
        if !snapshot_assets
            .iter()
            .any(|&asset| asset.eq_ignore_ascii_case(critical))
        {
            log::warn!(
                "CONNECTION: critical asset {} not found in balance response (may have zero balance)",
                critical
            );
        }
    }

    Ok(snapshots)
}

pub async fn fetch_position(conn: &Connection, symbol: &str) -> Result<Option<PositionUpdate>> {
    ensure_credentials(conn)?;

    let mut params = Vec::new();
    params.push(("symbol".to_string(), symbol.to_string()));

    let response = signed_get(conn, "/fapi/v2/positionRisk", params)
        .await?
        .json::<Vec<PositionRiskResponse>>()
        .await
        .context("failed to parse position risk response")?;

    let position = response.into_iter().find(|p| p.symbol == symbol);

    match position {
        Some(pos) => {
            let size = pos.position_amount.parse::<f64>().unwrap_or(0.0);
            if size == 0.0 {
                return Ok(None);
            }

            let entry_price = pos.entry_price.parse::<f64>().unwrap_or(0.0);
            let unrealized_pnl = pos.unrealized_profit.parse::<f64>().unwrap_or(0.0);
            let leverage = pos.leverage.parse::<f64>().unwrap_or(0.0);
            let side = if size >= 0.0 { Side::Long } else { Side::Short };
            Ok(Some(PositionUpdate {
                position_id: PositionUpdate::position_id(&pos.symbol, side),
                symbol: pos.symbol,
                side,
                entry_price,
                size: size.abs(),
                leverage,
                unrealized_pnl,
                ts: Utc::now(),
                is_closed: false,
            }))
        }
        None => Ok(None),
    }
}

pub(crate) async fn fetch_liquidation_clusters(conn: &Connection) -> Result<(Option<f64>, Option<f64>)> {
    let params = vec![
        ("symbol".to_string(), conn.config.symbol.clone()),
        ("limit".to_string(), "100".to_string()),
        ("autoCloseType".to_string(), "LIQUIDATION".to_string()),
    ];
    let records = signed_get(conn, "/fapi/v1/forceOrders", params)
        .await?
        .json::<Vec<ForceOrderRecord>>()
        .await?;

    let mut long_total = 0.0;
    let mut short_total = 0.0;

    for order in records {
        let qty = order
            .executed_qty
            .as_deref()
            .or_else(|| order.orig_qty.as_deref())
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0);
        let price = order
            .avg_price
            .as_deref()
            .or_else(|| order.price.as_deref())
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0);
        let notional = qty * price;
        match order.side.as_str() {
            "SELL" => long_total += notional,
            "BUY" => short_total += notional,
            _ => {}
        }
    }

    let oi = fetch_open_interest(conn).await.unwrap_or(0.0);
    if oi <= 0.0 {
        return Ok((None, None));
    }

    let normalize = |value: f64| if value > 0.0 { Some(value / oi) } else { None };
    Ok((normalize(long_total), normalize(short_total)))
}

pub(crate) async fn fetch_open_interest(conn: &Connection) -> Result<f64> {
    let url = format!("{}/fapi/v1/openInterest", conn.config.base_url);
    let resp = conn
        .http
        .get(&url)
        .query(&[("symbol", conn.config.symbol.as_str())])
        .send()
        .await?
        .error_for_status()?
        .json::<OpenInterestResponse>()
        .await?;

    resp.open_interest
        .parse::<f64>()
        .context("failed to parse open interest")
}

pub(crate) async fn fetch_depth_snapshot(conn: &Connection) -> Result<DepthSnapshot> {
    let url = format!("{}/fapi/v1/depth", conn.config.base_url);
    let snapshot = conn
        .http
        .get(&url)
        .query(&[("symbol", conn.config.symbol.as_str()), ("limit", "20")])
        .send()
        .await?
        .error_for_status()?
        .json::<DepthSnapshot>()
        .await?;
    Ok(snapshot)
}

pub(crate) async fn fetch_depth_obi(conn: &Connection) -> Result<Option<f64>> {
    let snapshot = fetch_depth_snapshot(conn).await?;

    let bid_sum: f64 = snapshot
        .bids
        .iter()
        .filter_map(|lvl| lvl[1].parse::<f64>().ok())
        .sum();
    let ask_sum: f64 = snapshot
        .asks
        .iter()
        .filter_map(|lvl| lvl[1].parse::<f64>().ok())
        .sum();
    if bid_sum > 0.0 && ask_sum > 0.0 {
        Ok(Some(bid_sum / ask_sum))
    } else {
        Ok(None)
    }
}

pub(crate) async fn fetch_premium_index(conn: &Connection) -> Result<PremiumIndex> {
    let url = format!("{}/fapi/v1/premiumIndex", conn.config.base_url);
    let resp = conn
        .http
        .get(&url)
        .query(&[("symbol", conn.config.symbol.as_str())])
        .send()
        .await?
        .error_for_status()?
        .json::<PremiumIndex>()
        .await?;
    Ok(resp)
}

async fn fetch_server_time_offset(conn: &Connection) -> Result<i64> {
    use serde::Deserialize;
    #[derive(Debug, Deserialize)]
    struct ServerTimeResponse {
        #[serde(rename = "serverTime")]
        server_time: i64,
    }
    let url = format!("{}/fapi/v1/time", conn.config.base_url);
    let client_time_before = Utc::now().timestamp_millis();
    let resp = conn
        .http
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .json::<ServerTimeResponse>()
        .await?;
    let client_time_after = Utc::now().timestamp_millis();
    let client_time_midpoint = (client_time_before + client_time_after) / 2;
    Ok(resp.server_time - client_time_midpoint)
}

pub async fn sync_server_time(conn: &Connection) -> Result<()> {
    let offset = fetch_server_time_offset(conn)
        .await
        .context("Failed to fetch Binance server time")?;

    let mut offset_guard = conn.server_time_offset.write().await;
    *offset_guard = offset;

    log::info!(
        "CONNECTION: Server time synced - offset: {}ms (server {} client)",
        offset,
        if offset >= 0 { "ahead of" } else { "behind" }
    );

    Ok(())
}

fn get_synced_timestamp(conn: &Connection) -> i64 {
    let offset = conn.server_time_offset.try_read().map(|guard| *guard).unwrap_or(0);
    Utc::now().timestamp_millis() + offset
}

fn get_endpoint_weight(_conn: &Connection, path: &str) -> u32 {
    match path {
        "/fapi/v1/order" => 1,
        "/fapi/v1/marginType" => 1,
        "/fapi/v1/leverage" => 1,
        "/fapi/v1/leverageBracket" => 1,
        "/fapi/v2/balance" => 5,
        "/fapi/v2/positionRisk" => 5,
        "/fapi/v1/forceOrders" => 20,
        "/fapi/v1/exchangeInfo" => 1,
        "/fapi/v1/depth" => 1,
        "/fapi/v1/premiumIndex" => 1,
        "/fapi/v1/openInterest" => 1,
        "/fapi/v1/time" => 1,
        _ => 1,
    }
}

fn is_order_endpoint(_conn: &Connection, path: &str) -> bool {
    matches!(
        path,
        "/fapi/v1/order" | "/fapi/v1/marginType" | "/fapi/v1/leverage"
    )
}

async fn signed_get(conn: &Connection, path: &str, params: Vec<(String, String)>) -> Result<reqwest::Response> {
    let weight = get_endpoint_weight(conn, path);
    conn.rate_limiter.acquire_weight(weight).await?;
    let query = sign_params(conn, params)?;
    let url = format!("{}{}?{}", conn.config.base_url, path, query);
    let response = conn
        .http
        .get(&url)
        .header("X-MBX-APIKEY", &conn.config.api_key)
        .send()
        .await?
        .error_for_status()?;

    Ok(response)
}

async fn signed_post(conn: &Connection, path: &str, params: Vec<(String, String)>) -> Result<reqwest::Response> {
    let _order_permit = if is_order_endpoint(conn, path) {
        Some(conn.rate_limiter.acquire_order_permit().await?)
    } else {
        None
    };

    let weight = get_endpoint_weight(conn, path);
    conn.rate_limiter.acquire_weight(weight).await?;
    let body = sign_params(conn, params)?;
    let url = format!("{}{}", conn.config.base_url, path);
    let response_future = conn
        .http
        .post(&url)
        .header("X-MBX-APIKEY", &conn.config.api_key)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send();
    drop(_order_permit);
    Ok(response_future.await?.error_for_status()?)
}

fn sign_params(conn: &Connection, mut params: Vec<(String, String)>) -> Result<String> {
    let timestamp = get_synced_timestamp(conn);
    params.push(("timestamp".into(), timestamp.to_string()));
    if conn.config.recv_window_ms > 0 {
        params.push(("recvWindow".into(), conn.config.recv_window_ms.to_string()));
    }
    let query = serde_urlencoded::to_string(&params)?;
    let mut mac = HmacSha256::new_from_slice(conn.config.api_secret.as_bytes())
        .map_err(|err| anyhow!("failed to init signer: {err}"))?;
    mac.update(query.as_bytes());
    let signature = hex::encode(mac.finalize().into_bytes());
    Ok(format!("{query}&signature={signature}"))
}

pub(crate) fn ensure_credentials(conn: &Connection) -> Result<()> {
    if conn.config.api_key.is_empty() || conn.config.api_secret.is_empty() {
        Err(anyhow!("Binance API key/secret required"))
    } else {
        Ok(())
    }
}

pub async fn fetch_snapshot(conn: &Connection) -> Result<MarketTick> {
    use crate::types::events::MarketTick;
    let premium = fetch_premium_index(conn).await?;
    let (symbol, price, funding_rate, ts) = premium.into_parts()?;
    let obi = fetch_depth_obi(conn).await?;
    let (liq_long, liq_short) = fetch_liquidation_clusters(conn)
        .await
        .unwrap_or((None, None));

    Ok(MarketTick {
        symbol,
        price,
        bid: price,
        ask: price,
        volume: 0.0,
        ts,
        obi,
        funding_rate,
        liq_long_cluster: liq_long,
        liq_short_cluster: liq_short,
        bid_depth_usd: None,
        ask_depth_usd: None,
    })
}

