use crate::types::{
    api::{DepthEvent, DepthState, ForceOrderStreamEvent, ForceOrderStreamOrder, ForceOrderStreamWrapper, LiqState, MarkPriceEvent, OpenInterestEvent},
    connection::Connection,
    events::{ConnectionChannels, MarketTick},
};
use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use log::{info, warn};
use std::sync::Arc;
use tokio::{
    sync::{broadcast, RwLock},
    time::{sleep, Duration},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};

pub(crate) async fn consume_mark_price_stream(
    conn: Arc<Connection>,
    ch: ConnectionChannels,
    depth_state: Arc<RwLock<DepthState>>,
    liq_state: Arc<RwLock<LiqState>>,
) -> Result<()> {
    let mut retry_delay = Duration::from_secs(1);
    loop {
        let url = mark_price_stream_url(&conn);
        match connect_async(&url).await {
            Ok((ws_stream, _)) => {
                info!("CONNECTION: mark price stream connected ({url})");
                conn.update_ws_connection_timestamp().await;
                retry_delay = Duration::from_secs(1);
                let (_, mut read) = ws_stream.split();
                let mut lag_monitor = ch.market_tx.subscribe();
                while let Some(message) = read.next().await {
                    match extract_text_from_message(&message, "mark-price") {
                        Ok(Some(txt)) => {
                            if let Some(mut tick) = parse_mark_price(&txt) {
                                tick.obi = (depth_state.read().await).obi();
                                let (liq_long, liq_short) = (liq_state.read().await).snapshot();
                                tick.liq_long_cluster = liq_long;
                                tick.liq_short_cluster = liq_short;
                                populate_tick_with_depth(&depth_state, &mut tick).await;

                                conn.update_market_tick_timestamp(tick.ts).await;
                                
                                match lag_monitor.try_recv() {
                                    Ok(_) | Err(broadcast::error::TryRecvError::Empty) => {
                                        if ch.market_tx.send(tick).is_err() {
                                            warn!("CONNECTION: market_tx receiver dropped");
                                            break;
                                        }
                                    }
                                    Err(broadcast::error::TryRecvError::Lagged(n)) => {
                                        warn!("CONNECTION: Market tick lagged by {} messages", n);
                                        use crate::connection::rest;
                                        if let Ok(snapshot) = rest::fetch_snapshot(&conn).await {
                                            let mut recovered_tick = snapshot;
                                            recovered_tick.obi = (depth_state.read().await).obi();
                                            let (liq_long, liq_short) = (liq_state.read().await).snapshot();
                                            recovered_tick.liq_long_cluster = liq_long;
                                            recovered_tick.liq_short_cluster = liq_short;
                                            populate_tick_with_depth(&depth_state, &mut recovered_tick).await;
                                            if ch.market_tx.send(recovered_tick).is_err() {
                                                warn!("CONNECTION: market_tx receiver dropped");
                                                break;
                                            }
                                        }
                                        if ch.market_tx.send(tick).is_err() {
                                            warn!("CONNECTION: market_tx receiver dropped");
                                            break;
                                        }
                                    }
                                    Err(broadcast::error::TryRecvError::Closed) => {
                                        warn!("CONNECTION: market_tx channel closed");
                                        break;
                                    }
                                }
                            }
                        }
                        Ok(None) => {}
                        Err(_) => break,
                    }
                }
            }
            Err(err) => warn!("CONNECTION: mark price connect error: {err:?}"),
        }
        info!(
            "CONNECTION: mark price reconnecting in {}s",
            retry_delay.as_secs()
        );
        sleep(retry_delay).await;
        retry_delay = (retry_delay * 2).min(Duration::from_secs(60));
    }
}

pub(crate) async fn consume_depth_stream_with_cache(
    conn: Arc<Connection>,
    depth_state: Arc<RwLock<DepthState>>,
    depth_cache: Option<Arc<crate::cache::DepthCache>>,
    symbol: String,
) -> Result<()> {
    let mut retry_delay = Duration::from_secs(1);
    loop {
        use crate::connection::rest;
        let snapshot_result = rest::fetch_depth_snapshot(&conn).await;
        if let Ok(snapshot) = snapshot_result {
            if let Some(cache) = &depth_cache {
                let best_bid = snapshot
                    .bids
                    .first()
                    .and_then(|lvl| lvl[0].parse::<f64>().ok())
                    .unwrap_or(0.0);
                let best_ask = snapshot
                    .asks
                    .first()
                    .and_then(|lvl| lvl[0].parse::<f64>().ok())
                    .unwrap_or(0.0);
                if best_bid > 0.0 && best_ask > 0.0 {
                    cache.update(symbol.clone(), best_bid, best_ask).await;
                }
            }
            
            depth_state.write().await.reset_with_snapshot(snapshot);
            info!("CONNECTION: depth state restored from snapshot");
        } else {
            warn!("CONNECTION: failed to fetch depth snapshot, continuing with empty state");
        }

        let url = depth_stream_url(&conn);
        match connect_async(&url).await {
            Ok((ws_stream, _)) => {
                info!("CONNECTION: depth stream connected ({url})");
                conn.update_ws_connection_timestamp().await;
                retry_delay = Duration::from_secs(1);
                let (_, mut read) = ws_stream.split();
                while let Some(message) = read.next().await {
                    match extract_text_from_message(&message, "depth") {
                        Ok(Some(txt)) => {
                            if let Ok(event) = serde_json::from_str::<DepthEvent>(&txt) {
                                depth_state.write().await.update(&event);
                                
                                if let Some(cache) = &depth_cache {
                                    let best_bid = event
                                        .bids
                                        .first()
                                        .and_then(|lvl| lvl[0].parse::<f64>().ok())
                                        .unwrap_or(0.0);
                                    let best_ask = event
                                        .asks
                                        .first()
                                        .and_then(|lvl| lvl[0].parse::<f64>().ok())
                                        .unwrap_or(0.0);
                                    if best_bid > 0.0 && best_ask > 0.0 {
                                        cache.update(symbol.clone(), best_bid, best_ask).await;
                                    }
                                }
                            }
                        }
                        Ok(None) => {}
                        Err(_) => break,
                    }
                }
            }
            Err(err) => warn!("CONNECTION: depth connect error: {err:?}"),
        }
        info!(
            "CONNECTION: depth reconnecting in {}s",
            retry_delay.as_secs()
        );
        sleep(retry_delay).await;
        retry_delay = (retry_delay * 2).min(Duration::from_secs(60));
    }
}

pub(crate) async fn run_liq_stream(conn: Arc<Connection>, liq_state: Arc<RwLock<LiqState>>) -> Result<()> {
    let stream_symbol = conn.config.symbol.to_lowercase();
    let ws_url = format!(
        "{}/ws/{}@forceOrder",
        conn.config.ws_base_url.trim_end_matches('/'),
        stream_symbol
    );

    loop {
        match connect_async(&ws_url).await {
            Ok((ws_stream, _)) => {
                info!("CONNECTION: liquidation stream connected ({ws_url})");
                let (_, mut read) = ws_stream.split();
                while let Some(msg) = read.next().await {
                    match extract_text_from_message(&msg, "liquidation") {
                        Ok(Some(txt)) => {
                            if let Err(err) = handle_force_order_message(&conn, &txt, &liq_state).await {
                                warn!("CONNECTION: liquidation message handle error: {err:?}");
                            }
                        }
                        Ok(None) => {}
                        Err(_) => break,
                    }
                }
            }
            Err(err) => warn!("CONNECTION: liq stream connect error: {err:?}"),
        }
        sleep(Duration::from_secs(2)).await;
    }
}

pub(crate) async fn run_open_interest_stream(conn: Arc<Connection>, liq_state: Arc<RwLock<LiqState>>) -> Result<()> {
    let mut retry_delay = Duration::from_secs(1);
    loop {
        let url = open_interest_stream_url(&conn);
        match connect_async(&url).await {
            Ok((ws_stream, _)) => {
                info!("CONNECTION: open interest stream connected ({url})");
                retry_delay = Duration::from_secs(1);
                let (_, mut read) = ws_stream.split();
                while let Some(message) = read.next().await {
                    match extract_text_from_message(&message, "open-interest") {
                        Ok(Some(txt)) => {
                            if let Ok(event) = serde_json::from_str::<OpenInterestEvent>(&txt) {
                                if event.symbol == conn.config.symbol {
                                    if let Ok(oi_value) = event.open_interest.parse::<f64>() {
                                        liq_state.write().await.set_open_interest(oi_value);
                                    }
                                }
                            }
                        }
                        Ok(None) => {}
                        Err(_) => break,
                    }
                }
            }
            Err(err) => warn!("CONNECTION: open interest connect error: {err:?}"),
        }
        info!(
            "CONNECTION: open interest reconnecting in {}s",
            retry_delay.as_secs()
        );
        sleep(retry_delay).await;
        retry_delay = (retry_delay * 2).min(Duration::from_secs(60));
    }
}

pub(crate) async fn create_listen_key(conn: &Connection) -> Result<String> {
    use crate::types::api::ListenKeyResponse;
    let url = format!("{}/fapi/v1/listenKey", conn.config.base_url);
    let resp = conn
        .http
        .post(&url)
        .header("X-MBX-APIKEY", &conn.config.api_key)
        .send()
        .await?
        .error_for_status()?
        .json::<ListenKeyResponse>()
        .await?;
    Ok(resp.listen_key)
}

pub(crate) async fn keep_alive_listen_key(conn: Arc<Connection>, key: &str) {
    let url = format!("{}/fapi/v1/listenKey", conn.config.base_url);
    let keep_alive_interval_secs = 25 * 60;
    let mut interval = tokio::time::interval(Duration::from_secs(keep_alive_interval_secs));
    interval.tick().await;

    loop {
        interval.tick().await;
        let mut retry_delay = Duration::from_secs(10);

        for attempt in 1..=3 {
            match conn
                .http
                .put(&url)
                .query(&[("listenKey", key)])
                .header("X-MBX-APIKEY", &conn.config.api_key)
                .send()
                .await
                .and_then(|resp| resp.error_for_status())
            {
                Ok(_) => {
                    break;
                }
                Err(err) => {
                    if attempt < 3 {
                        warn!("CONNECTION: listen key keep-alive failed (attempt {}/3): {err:?}, retrying in {}s", attempt, retry_delay.as_secs());
                        sleep(retry_delay).await;
                        retry_delay = retry_delay * 2;
                    } else {
                        warn!("CONNECTION: listen key keep-alive failed after 3 attempts: {err:?}, stream may disconnect");
                    }
                }
            }
        }
    }
}

pub(crate) fn mark_price_stream_url(conn: &Connection) -> String {
    let base = conn.config.ws_base_url.trim_end_matches('/');
    let symbol = conn.config.symbol.to_lowercase();
    format!("{base}/ws/{symbol}@markPrice@1s")
}

pub(crate) fn depth_stream_url(conn: &Connection) -> String {
    let base = conn.config.ws_base_url.trim_end_matches('/');
    let symbol = conn.config.symbol.to_lowercase();
    format!("{base}/ws/{symbol}@depth20@100ms")
}

pub(crate) fn open_interest_stream_url(conn: &Connection) -> String {
    let base = conn.config.ws_base_url.trim_end_matches('/');
    let symbol = conn.config.symbol.to_lowercase();
    format!("{base}/ws/{symbol}@openInterest")
}

pub(crate) fn extract_text_from_message(msg: &Result<Message, tokio_tungstenite::tungstenite::Error>, stream_name: &str) -> Result<Option<String>> {
    use crate::connection;
    let msg_size = msg.as_ref().map(|m| {
        match m {
            Message::Text(txt) => txt.len(),
            Message::Binary(bin) => bin.len(),
            _ => 0,
        }
    }).unwrap_or(0);
    connection::check_message_size(msg_size, stream_name)?;
    
    match msg {
        Ok(Message::Text(txt)) => Ok(Some(txt.clone())),
        Ok(Message::Binary(_)) => Ok(None),
        Ok(Message::Ping(_) | Message::Pong(_) | Message::Close(_) | Message::Frame(_)) => Ok(None),
        Err(e) => Err(anyhow!("WebSocket error on {}: {e:?}", stream_name)),
    }
}

fn parse_mark_price(payload: &str) -> Option<MarketTick> {
    let event: MarkPriceEvent = serde_json::from_str(payload).ok()?;
    let price = event.mark_price.parse::<f64>().ok()?;
    let funding_rate = event.funding_rate.and_then(|v| v.parse().ok());
    let event_ts = DateTime::<Utc>::from_timestamp_millis(event.event_time as i64)
        .unwrap_or_else(|| Utc::now());

    Some(MarketTick {
        symbol: event.symbol,
        price,
        bid: price,
        ask: price,
        volume: 0.0,
        ts: event_ts,
        obi: None,
        funding_rate,
        liq_long_cluster: None,
        liq_short_cluster: None,
        bid_depth_usd: None,
        ask_depth_usd: None,
    })
}

async fn handle_force_order_message(
    conn: &Connection,
    payload: &str,
    liq_state: &Arc<RwLock<LiqState>>,
) -> Result<()> {
    if let Ok(wrapper) = serde_json::from_str::<ForceOrderStreamWrapper>(payload) {
        ingest_force_order(conn, wrapper.data.order, liq_state).await?;
    } else if let Ok(event) = serde_json::from_str::<ForceOrderStreamEvent>(payload) {
        ingest_force_order(conn, event.order, liq_state).await?;
    }
    Ok(())
}

async fn ingest_force_order(
    conn: &Connection,
    order: ForceOrderStreamOrder,
    liq_state: &Arc<RwLock<LiqState>>,
) -> Result<()> {
    if order.symbol != conn.config.symbol {
        return Ok(());
    }
    let qty = order
        .last_filled
        .as_deref()
        .or_else(|| order.executed_qty.as_deref())
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
    if notional <= 0.0 {
        return Ok(());
    }
    liq_state.write().await.record(&order.side, notional);
    Ok(())
}

async fn populate_tick_with_depth(depth_state: &Arc<RwLock<DepthState>>, tick: &mut MarketTick) {
    let depth = depth_state.read().await;
    if let Some((price, _)) = depth.best_bid() {
        tick.bid = price;
    }
    if let Some((price, _)) = depth.best_ask() {
        tick.ask = price;
    }
    tick.bid_depth_usd = depth.depth_notional_usd(true, 10);
    tick.ask_depth_usd = depth.depth_notional_usd(false, 10);
}

pub(crate) async fn handle_user_message(conn: &Connection, payload: &str, ch: &ConnectionChannels) -> Result<()> {
    use crate::types::api::{AccountUpdate, OrderTradeUpdate};
    let event: serde_json::Value = serde_json::from_str(payload)?;
    match event.get("e").and_then(|v| v.as_str()) {
        Some("ORDER_TRADE_UPDATE") => {
            let order: OrderTradeUpdate = serde_json::from_value(event)?;
            let update = order.into_order_update();
            if ch.order_update_tx.send(update).is_err() {
                warn!("CONNECTION: order update receiver dropped");
            }
            Ok(())
        }
        Some("ACCOUNT_UPDATE") => {
            let account: AccountUpdate = serde_json::from_value(event)?;
            for bal in account.account.balances.iter() {
                if let Some(snapshot) = bal.to_balance_snapshot() {
                    if ch.balance_tx.send(snapshot.clone()).is_err() {
                        warn!("CONNECTION: balance receiver dropped");
                    }
                }
            }
            for pos in account.account.positions.iter() {
                if let Some(update) = pos.to_position_update(conn.config.leverage) {
                    if ch.position_update_tx.send(update.clone()).is_err() {
                        warn!("CONNECTION: position receiver dropped");
                    }
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

