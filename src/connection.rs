use crate::config::BotConfig;
use crate::event_bus::ConnectionChannels;
use crate::types::{BalanceSnapshot, MarketTick, OrderStatus, OrderUpdate, PositionUpdate, Side};
use anyhow::Result;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use log::{info, warn};
use reqwest::Client;
use serde::Deserialize;
use std::sync::Arc;
use tokio::{
    sync::RwLock,
    time::{sleep, Duration},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use uuid::Uuid;

#[derive(Clone)]
pub struct Connection {
    config: BotConfig,
    http: Client,
}

impl Connection {
    pub fn new(config: BotConfig) -> Self {
        let http = Client::builder()
            .user_agent("bot-b/0.1")
            .build()
            .expect("failed to build reqwest client");
        Self { config, http }
    }

    pub fn quote_asset(&self) -> &str {
        &self.config.quote_asset
    }

    pub async fn run_market_ws(self: Arc<Self>, ch: ConnectionChannels) -> Result<()> {
        let depth_state = Arc::new(RwLock::new(DepthState::new(20)));

        loop {
            let mark_stream = self.clone();
            let depth_stream = self.clone();
            let ch_mark = ch.clone();
            let depth_for_mark = depth_state.clone();
            let depth_for_depth = depth_state.clone();

            let mark_task = tokio::spawn(async move {
                mark_stream
                    .consume_mark_price_stream(ch_mark, depth_for_mark)
                    .await
            });
            let depth_task =
                tokio::spawn(
                    async move { depth_stream.consume_depth_stream(depth_for_depth).await },
                );

            let _ = tokio::join!(mark_task, depth_task);
            warn!("CONNECTION: market streams stopped. Reconnecting soon...");
            sleep(Duration::from_secs(2)).await;
        }
    }

    pub async fn run_user_ws(self: Arc<Self>, ch: ConnectionChannels) -> Result<()> {
        loop {
            match self.create_listen_key().await {
                Ok(listen_key) => {
                    let ws_url = format!(
                        "{}/ws/{}",
                        self.config.ws_base_url.trim_end_matches('/'),
                        listen_key
                    );
                    info!("CONNECTION: user-data WS connecting");

                    let keepalive_conn = self.clone();
                    let key_clone = listen_key.clone();
                    let keepalive = tokio::spawn(async move {
                        keepalive_conn.keep_alive_listen_key(&key_clone).await;
                    });

                    match connect_async(&ws_url).await {
                        Ok((mut ws_stream, _)) => {
                            info!("CONNECTION: user-data WS connected");
                            while let Some(msg) = ws_stream.next().await {
                                match msg {
                                    Ok(Message::Text(txt)) => {
                                        if let Err(err) = self.handle_user_message(&txt, &ch).await
                                        {
                                            warn!("CONNECTION: user-data handle error: {err:?}");
                                            break;
                                        }
                                    }
                                    Ok(Message::Binary(bin)) => {
                                        if let Ok(txt) = String::from_utf8(bin) {
                                            if let Err(err) =
                                                self.handle_user_message(&txt, &ch).await
                                            {
                                                warn!(
                                                    "CONNECTION: user-data handle error: {err:?}"
                                                );
                                                break;
                                            }
                                        }
                                    }
                                    Ok(Message::Ping(_))
                                    | Ok(Message::Pong(_))
                                    | Ok(Message::Frame(_)) => {}
                                    Ok(Message::Close(frame)) => {
                                        warn!("CONNECTION: user-data WS closed: {:?}", frame);
                                        break;
                                    }
                                    Err(err) => {
                                        warn!("CONNECTION: user-data WS error: {err}");
                                        break;
                                    }
                                }
                            }
                        }
                        Err(err) => warn!("CONNECTION: user-data WS connect error: {err:?}"),
                    }

                    keepalive.abort();
                }
                Err(err) => {
                    warn!("CONNECTION: listen key error: {err:?}");
                    sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }

    pub async fn send_order(&self, description: String) -> Result<()> {
        info!("CONNECTION: send_order -> {}", description);
        Ok(())
    }

    pub async fn fetch_balance(&self) -> Result<Vec<BalanceSnapshot>> {
        let snapshot = BalanceSnapshot {
            asset: "USDT".into(),
            free: 1_000.0,
            ts: Utc::now(),
        };
        Ok(vec![snapshot])
    }

    async fn consume_mark_price_stream(
        self: Arc<Self>,
        ch: ConnectionChannels,
        depth_state: Arc<RwLock<DepthState>>,
    ) -> Result<()> {
        let mut retry_delay = Duration::from_secs(1);
        loop {
            let url = self.mark_price_stream_url();
            match connect_async(&url).await {
                Ok((ws_stream, _)) => {
                    info!("CONNECTION: mark price stream connected ({url})");
                    retry_delay = Duration::from_secs(1);
                    let (_, mut read) = ws_stream.split();
                    while let Some(message) = read.next().await {
                        match message {
                            Ok(Message::Text(txt)) => {
                                if let Some(mut tick) = self.parse_mark_price(&txt) {
                                    tick.obi = (depth_state.read().await).obi();
                                    if ch.market_tx.send(tick).is_err() {
                                        warn!("CONNECTION: market_tx receiver dropped");
                                        break;
                                    }
                                }
                            }
                            Ok(Message::Binary(bin)) => {
                                if let Ok(txt) = String::from_utf8(bin) {
                                    if let Some(mut tick) = self.parse_mark_price(&txt) {
                                        tick.obi = (depth_state.read().await).obi();
                                        if ch.market_tx.send(tick).is_err() {
                                            warn!("CONNECTION: market_tx receiver dropped");
                                            break;
                                        }
                                    }
                                }
                            }
                            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) | Ok(Message::Frame(_)) => {
                            }
                            Ok(Message::Close(frame)) => {
                                warn!("CONNECTION: mark price stream closed: {:?}", frame);
                                break;
                            }
                            Err(err) => {
                                warn!("CONNECTION: mark price stream error: {err}");
                                break;
                            }
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

    async fn consume_depth_stream(
        self: Arc<Self>,
        depth_state: Arc<RwLock<DepthState>>,
    ) -> Result<()> {
        let mut retry_delay = Duration::from_secs(1);
        loop {
            let url = self.depth_stream_url();
            match connect_async(&url).await {
                Ok((ws_stream, _)) => {
                    info!("CONNECTION: depth stream connected ({url})");
                    retry_delay = Duration::from_secs(1);
                    let (_, mut read) = ws_stream.split();
                    while let Some(message) = read.next().await {
                        match message {
                            Ok(Message::Text(txt)) => {
                                if let Ok(event) = serde_json::from_str::<DepthEvent>(&txt) {
                                    depth_state.write().await.update(&event);
                                }
                            }
                            Ok(Message::Binary(bin)) => {
                                if let Ok(txt) = String::from_utf8(bin) {
                                    if let Ok(event) = serde_json::from_str::<DepthEvent>(&txt) {
                                        depth_state.write().await.update(&event);
                                    }
                                }
                            }
                            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) | Ok(Message::Frame(_)) => {
                            }
                            Ok(Message::Close(frame)) => {
                                warn!("CONNECTION: depth stream closed: {:?}", frame);
                                break;
                            }
                            Err(err) => {
                                warn!("CONNECTION: depth stream error: {err}");
                                break;
                            }
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

    async fn create_listen_key(&self) -> Result<String> {
        let url = format!("{}/fapi/v1/listenKey", self.config.base_url);
        let resp = self
            .http
            .post(&url)
            .header("X-MBX-APIKEY", &self.config.api_key)
            .send()
            .await?
            .error_for_status()?
            .json::<ListenKeyResponse>()
            .await?;
        Ok(resp.listen_key)
    }

    async fn keep_alive_listen_key(&self, key: &str) {
        let url = format!("{}/fapi/v1/listenKey", self.config.base_url);
        let mut interval = tokio::time::interval(Duration::from_secs(30 * 60));
        loop {
            interval.tick().await;
            if let Err(err) = self
                .http
                .put(&url)
                .query(&[("listenKey", key)])
                .header("X-MBX-APIKEY", &self.config.api_key)
                .send()
                .await
                .and_then(|resp| resp.error_for_status())
            {
                warn!("CONNECTION: listen key keep-alive failed: {err:?}");
                break;
            }
        }
    }

    async fn handle_user_message(&self, payload: &str, ch: &ConnectionChannels) -> Result<()> {
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
                    if let Some(update) = pos.to_position_update() {
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

    fn mark_price_stream_url(&self) -> String {
        let base = self.config.ws_base_url.trim_end_matches('/');
        let symbol = self.config.symbol.to_lowercase();
        format!("{base}/ws/{symbol}@markPrice@1s")
    }

    fn depth_stream_url(&self) -> String {
        let base = self.config.ws_base_url.trim_end_matches('/');
        let symbol = self.config.symbol.to_lowercase();
        format!("{base}/ws/{symbol}@depth20@100ms")
    }

    fn parse_mark_price(&self, payload: &str) -> Option<MarketTick> {
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
        })
    }
}

#[derive(Debug, Deserialize)]
struct MarkPriceEvent {
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "p")]
    mark_price: String,
    #[serde(rename = "r")]
    funding_rate: Option<String>,
    #[serde(rename = "E")]
    event_time: u64,
}

#[derive(Debug, Deserialize)]
struct DepthEvent {
    #[serde(rename = "b")]
    bids: Vec<[String; 2]>,
    #[serde(rename = "a")]
    asks: Vec<[String; 2]>,
}

struct DepthState {
    bids: Vec<(f64, f64)>,
    asks: Vec<(f64, f64)>,
    depth_limit: usize,
    obi: Option<f64>,
}

impl DepthState {
    fn new(depth_limit: usize) -> Self {
        Self {
            bids: Vec::new(),
            asks: Vec::new(),
            depth_limit,
            obi: None,
        }
    }

    fn update(&mut self, event: &DepthEvent) {
        self.bids = event
            .bids
            .iter()
            .take(self.depth_limit)
            .filter_map(|lvl| {
                let price = lvl[0].parse::<f64>().ok()?;
                let qty = lvl[1].parse::<f64>().ok()?;
                Some((price, qty))
            })
            .collect();
        self.asks = event
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

    fn obi(&self) -> Option<f64> {
        self.obi
    }
}

#[derive(Debug, Deserialize)]
struct ListenKeyResponse {
    #[serde(rename = "listenKey")]
    listen_key: String,
}

#[derive(Debug, Deserialize)]
struct OrderTradeUpdate {
    #[serde(rename = "o")]
    order: OrderPayload,
}

#[derive(Debug, Deserialize)]
struct OrderPayload {
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "S")]
    side: String,
    #[serde(rename = "X")]
    status: String,
    #[serde(rename = "z")]
    filled_qty: String,
    #[serde(rename = "E")]
    event_time: u64,
}

impl OrderTradeUpdate {
    fn into_order_update(self) -> OrderUpdate {
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
            ts: DateTime::<Utc>::from_timestamp_millis(self.order.event_time as i64)
                .unwrap_or_else(|| Utc::now()),
        }
    }
}

#[derive(Debug, Deserialize)]
struct AccountUpdate {
    #[serde(rename = "a")]
    account: AccountPayload,
}

#[derive(Debug, Deserialize)]
struct AccountPayload {
    #[serde(rename = "B")]
    balances: Vec<AccountBalance>,
    #[serde(rename = "P")]
    positions: Vec<AccountPosition>,
}

#[derive(Debug, Deserialize)]
struct AccountBalance {
    #[serde(rename = "a")]
    asset: String,
    #[serde(rename = "wb")]
    wallet_balance: String,
}

impl AccountBalance {
    fn to_balance_snapshot(&self) -> Option<BalanceSnapshot> {
        let free = self.wallet_balance.parse().ok()?;
        Some(BalanceSnapshot {
            asset: self.asset.clone(),
            free,
            ts: Utc::now(),
        })
    }
}

#[derive(Debug, Deserialize)]
struct AccountPosition {
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "pa")]
    position_amount: String,
    #[serde(rename = "ep")]
    entry_price: String,
    #[serde(rename = "cr")]
    unrealized_pnl: String,
}

impl AccountPosition {
    fn to_position_update(&self) -> Option<PositionUpdate> {
        let size = self.position_amount.parse::<f64>().ok()?;
        let entry = self.entry_price.parse::<f64>().ok().unwrap_or(0.0);
        let pnl = self.unrealized_pnl.parse::<f64>().ok().unwrap_or(0.0);
        let side = if size >= 0.0 { Side::Long } else { Side::Short };

        Some(PositionUpdate {
            position_id: Uuid::new_v4(),
            symbol: self.symbol.clone(),
            side,
            entry_price: entry,
            size: size.abs(),
            leverage: 0.0,
            unrealized_pnl: pnl,
            ts: Utc::now(),
            is_closed: size == 0.0,
        })
    }
}
