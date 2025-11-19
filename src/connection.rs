use crate::config::BotConfig;
use crate::event_bus::ConnectionChannels;
use crate::types::{BalanceSnapshot, MarketTick, OrderStatus, OrderUpdate, PositionUpdate, Side};
use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use hex;
use hmac::{Hmac, Mac};
use log::{info, warn};
use reqwest::Client;
use serde::Deserialize;
use sha2::Sha256;
use std::sync::Arc;
use tokio::{
    sync::RwLock,
    time::{interval, sleep, Duration},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use uuid::Uuid;
type HmacSha256 = Hmac<Sha256>;

#[derive(Clone)]
pub struct Connection {
    config: BotConfig,
    http: Client,
}

#[derive(Debug)]
pub struct NewOrderRequest {
    pub symbol: String,
    pub side: Side,
    pub quantity: f64,
    pub reduce_only: bool,
    pub client_order_id: Option<String>,
}

impl Connection {
    pub fn new(config: BotConfig) -> Self {
        let http = Client::builder()
            .user_agent("bot-b/0.1")
            .build()
            .expect("failed to build reqwest client");
        Self { config, http }
    }

    #[allow(dead_code)]
    pub fn quote_asset(&self) -> &str {
        &self.config.quote_asset
    }

    pub async fn run_market_ws(self: Arc<Self>, ch: ConnectionChannels) -> Result<()> {
        let depth_state = Arc::new(RwLock::new(DepthState::new(20)));
        let liq_state = Arc::new(RwLock::new(LiqState::default()));

        loop {
            let mark_stream = self.clone();
            let depth_stream = self.clone();
            let liq_stream = self.clone();
            let ch_mark = ch.clone();
            let depth_for_mark = depth_state.clone();
            let depth_for_depth = depth_state.clone();
            let liq_for_mark = liq_state.clone();
            let liq_for_task = liq_state.clone();

            let mark_task = tokio::spawn(async move {
                mark_stream
                    .consume_mark_price_stream(ch_mark, depth_for_mark, liq_for_mark)
                    .await
            });
            let depth_task =
                tokio::spawn(
                    async move { depth_stream.consume_depth_stream(depth_for_depth).await },
                );
            let liq_task = tokio::spawn(async move {
                if let Err(err) = liq_stream.run_liq_oi_pollers(liq_for_task).await {
                    warn!("CONNECTION: liq/OI poller exited: {err:?}");
                }
            });

            let _ = tokio::join!(mark_task, depth_task, liq_task);
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

    pub async fn send_order(&self, order: NewOrderRequest) -> Result<()> {
        self.ensure_credentials()?;

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

        let side = match side {
            Side::Long => "BUY",
            Side::Short => "SELL",
        };

        let mut params = vec![
            ("symbol".to_string(), symbol),
            ("side".to_string(), side.to_string()),
            ("type".to_string(), "MARKET".to_string()),
            ("quantity".to_string(), format!("{:.6}", quantity)),
        ];

        if reduce_only {
            params.push(("reduceOnly".into(), "true".into()));
        }
        if let Some(id) = client_order_id {
            params.push(("newClientOrderId".into(), id));
        }

        self.signed_post("/fapi/v1/order", params).await?;
        info!("CONNECTION: order sent (reduce_only={})", reduce_only);
        Ok(())
    }

    pub async fn fetch_balance(&self) -> Result<Vec<BalanceSnapshot>> {
        self.ensure_credentials()?;

        let response = self
            .signed_get("/fapi/v2/balance", Vec::<(String, String)>::new())
            .await?
            .json::<Vec<FuturesBalance>>()
            .await
            .context("failed to parse balance response")?;

        let snapshots = response
            .into_iter()
            .filter_map(|record| {
                record
                    .available_balance
                    .parse::<f64>()
                    .ok()
                    .map(|free| BalanceSnapshot {
                        asset: record.asset,
                        free,
                        ts: Utc::now(),
                    })
            })
            .collect();

        Ok(snapshots)
    }

    async fn consume_mark_price_stream(
        self: Arc<Self>,
        ch: ConnectionChannels,
        depth_state: Arc<RwLock<DepthState>>,
        liq_state: Arc<RwLock<LiqState>>,
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
                                    let (liq_long, liq_short) = (liq_state.read().await).snapshot();
                                    tick.liq_long_cluster = liq_long;
                                    tick.liq_short_cluster = liq_short;
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
                                        let (liq_long, liq_short) =
                                            (liq_state.read().await).snapshot();
                                        tick.liq_long_cluster = liq_long;
                                        tick.liq_short_cluster = liq_short;
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

    async fn run_liq_oi_pollers(self: Arc<Self>, liq_state: Arc<RwLock<LiqState>>) -> Result<()> {
        let mut ticker = interval(Duration::from_secs(5));
        loop {
            ticker.tick().await;
            match self.fetch_liquidation_clusters().await {
                Ok((long, short)) => {
                    liq_state.write().await.update(long, short);
                }
                Err(err) => warn!("CONNECTION: liq snapshot fetch failed: {err:?}"),
            }
        }
        #[allow(unreachable_code)]
        Ok(())
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

    async fn fetch_liquidation_clusters(&self) -> Result<(Option<f64>, Option<f64>)> {
        let url = format!("{}/fapi/v1/liquidationOrders", self.config.base_url);
        let response = self
            .http
            .get(&url)
            .query(&[("symbol", self.config.symbol.as_str()), ("limit", "100")])
            .send()
            .await?
            .error_for_status()?
            .json::<Vec<LiquidationOrder>>()
            .await?;

        let mut long_total = 0.0;
        let mut short_total = 0.0;

        for order in response {
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

        let oi = self.fetch_open_interest().await.unwrap_or(0.0);
        let normalize = |value: f64| if oi > 0.0 { value / oi } else { value };

        let long_cluster = if long_total > 0.0 {
            Some(normalize(long_total))
        } else {
            None
        };
        let short_cluster = if short_total > 0.0 {
            Some(normalize(short_total))
        } else {
            None
        };

        Ok((long_cluster, short_cluster))
    }

    async fn fetch_open_interest(&self) -> Result<f64> {
        let url = format!("{}/fapi/v1/openInterest", self.config.base_url);
        let resp = self
            .http
            .get(&url)
            .query(&[("symbol", self.config.symbol.as_str())])
            .send()
            .await?
            .error_for_status()?
            .json::<OpenInterestResponse>()
            .await?;

        resp.open_interest
            .parse::<f64>()
            .context("failed to parse open interest")
    }

    async fn signed_get(
        &self,
        path: &str,
        params: Vec<(String, String)>,
    ) -> Result<reqwest::Response> {
        let query = self.sign_params(params)?;
        let url = format!("{}{}?{}", self.config.base_url, path, query);
        let response = self
            .http
            .get(&url)
            .header("X-MBX-APIKEY", &self.config.api_key)
            .send()
            .await?
            .error_for_status()?;
        Ok(response)
    }

    async fn signed_post(
        &self,
        path: &str,
        params: Vec<(String, String)>,
    ) -> Result<reqwest::Response> {
        let body = self.sign_params(params)?;
        let url = format!("{}{}", self.config.base_url, path);
        let response = self
            .http
            .post(&url)
            .header("X-MBX-APIKEY", &self.config.api_key)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await?
            .error_for_status()?;
        Ok(response)
    }

    fn sign_params(&self, mut params: Vec<(String, String)>) -> Result<String> {
        let timestamp = Utc::now().timestamp_millis();
        params.push(("timestamp".into(), timestamp.to_string()));
        if self.config.recv_window_ms > 0 {
            params.push(("recvWindow".into(), self.config.recv_window_ms.to_string()));
        }
        let query = serde_urlencoded::to_string(&params)?;
        let mut mac = HmacSha256::new_from_slice(self.config.api_secret.as_bytes())
            .map_err(|err| anyhow!("failed to init signer: {err}"))?;
        mac.update(query.as_bytes());
        let signature = hex::encode(mac.finalize().into_bytes());
        Ok(format!("{query}&signature={signature}"))
    }

    fn ensure_credentials(&self) -> Result<()> {
        if self.config.api_key.is_empty() || self.config.api_secret.is_empty() {
            Err(anyhow!("Binance API key/secret required"))
        } else {
            Ok(())
        }
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

#[derive(Debug, Deserialize)]
struct LiquidationOrder {
    #[serde(rename = "side")]
    side: String,
    #[serde(rename = "avgPrice")]
    avg_price: Option<String>,
    #[serde(rename = "price")]
    price: Option<String>,
    #[serde(rename = "executedQty")]
    executed_qty: Option<String>,
    #[serde(rename = "origQty")]
    orig_qty: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenInterestResponse {
    #[serde(rename = "openInterest")]
    open_interest: String,
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

#[derive(Default)]
struct LiqState {
    liq_long: Option<f64>,
    liq_short: Option<f64>,
}

impl LiqState {
    fn update(&mut self, long: Option<f64>, short: Option<f64>) {
        self.liq_long = long;
        self.liq_short = short;
    }

    fn snapshot(&self) -> (Option<f64>, Option<f64>) {
        (self.liq_long, self.liq_short)
    }
}

#[derive(Debug, Deserialize)]
struct ListenKeyResponse {
    #[serde(rename = "listenKey")]
    listen_key: String,
}

#[derive(Debug, Deserialize)]
struct FuturesBalance {
    #[serde(rename = "asset")]
    asset: String,
    #[serde(rename = "availableBalance")]
    available_balance: String,
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
