
use crate::types::{AccountBalance, AccountPosition, PriceUpdate, Px, Qty, Side, UserEvent, UserStreamKind};
use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};
use reqwest::Client;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::Value;
use std::str::FromStr;
use std::time::Instant;
use tokio::time::{timeout, Duration};
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message, WebSocketStream};
use tracing::{debug, error, info, warn};
pub type WsStream = WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
#[derive(Deserialize)]
struct ListenKeyResp {
    #[serde(rename = "listenKey")]
    listen_key: String,
}
pub struct UserDataStream {
    client: Client,
    base: String,
    api_key: String,
    kind: UserStreamKind,
    listen_key: String,
    ws: WsStream,
    last_keep_alive: Instant,
    on_reconnect: Option<Box<dyn Fn() + Send + Sync>>,
}
pub struct MarketDataStream {
    ws: WsStream,
}
impl MarketDataStream {
    pub async fn connect(symbols: &[String]) -> Result<Self> {
        let streams: Vec<String> = symbols
            .iter()
            .map(|s| format!("{}@bookTicker", s.to_lowercase()))
            .collect();
        let stream_param = streams.join("/");
        let url = format!("wss://fstream.binance.com/stream?streams={}", stream_param);
        info!(url = %url, symbol_count = symbols.len(), "connecting to market data websocket");
        let (ws, _) = connect_async(&url).await?;
        info!("connected to market data websocket");
        Ok(Self {
            ws,
        })
    }
    pub async fn next_price_update(&mut self) -> Result<PriceUpdate> {
        loop {
            match timeout(Duration::from_secs(300), self.ws.next()).await {
                Ok(Some(msg)) => {
                    let msg = msg.map_err(|e| match e {
                        WsError::ConnectionClosed | WsError::AlreadyClosed => {
                            anyhow!("market data stream closed")
                        }
                        other => anyhow!(other),
                    })?;
                    match msg {
                        Message::Ping(payload) => {
                            self.ws.send(Message::Pong(payload)).await?;
                            continue;
                        }
                        Message::Pong(_) => continue,
                        Message::Text(txt) => {
                            if txt.is_empty() {
                                continue;
                            }
                            let value: Value = serde_json::from_str(&txt)?;
                            let stream_name = value.get("stream")
                                .and_then(|s| s.as_str())
                                .ok_or_else(|| anyhow!("missing stream field"))?;
                            let symbol = stream_name
                                .split('@')
                                .next()
                                .ok_or_else(|| anyhow!("invalid stream name"))?
                                .to_uppercase();
                            let data = value.get("data")
                                .ok_or_else(|| anyhow!("missing data field"))?;
                            let bid_str = data.get("b")
                                .and_then(|v| v.as_str())
                                .ok_or_else(|| anyhow!("missing bid price"))?;
                            let ask_str = data.get("a")
                                .and_then(|v| v.as_str())
                                .ok_or_else(|| anyhow!("missing ask price"))?;
                            let bid_qty_str = data.get("B")
                                .and_then(|v| v.as_str())
                                .unwrap_or("0");
                            let ask_qty_str = data.get("A")
                                .and_then(|v| v.as_str())
                                .unwrap_or("0");
                            let bid = Decimal::from_str(bid_str)?;
                            let ask = Decimal::from_str(ask_str)?;
                            let bid_qty = Decimal::from_str(bid_qty_str).unwrap_or(Decimal::ZERO);
                            let ask_qty = Decimal::from_str(ask_qty_str).unwrap_or(Decimal::ZERO);
                            return Ok(PriceUpdate {
                                symbol,
                                bid: Px(bid),
                                ask: Px(ask),
                                bid_qty: Qty(bid_qty),
                                ask_qty: Qty(ask_qty),
                            });
                        }
                        Message::Binary(_) => continue,
                        Message::Close(_) => return Err(anyhow!("market data stream closed")),
                        Message::Frame(_) => continue,
                    }
                }
                Ok(None) => return Err(anyhow!("market data stream terminated")),
                Err(_) => {
                    warn!("market data websocket timeout, reconnecting");
                    return Err(anyhow!("market data stream timeout"));
                }
            }
        }
    }
}
impl UserDataStream {
#[inline]
fn ws_url_for(_kind: UserStreamKind, listen_key: &str) -> String {
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
    async fn reconnect_ws_without_new_key(&mut self) -> Result<()> {
        let url = Self::ws_url_for(self.kind, &self.listen_key);
        let (ws, _) = connect_async(&url).await?;
        self.ws = ws;
        self.last_keep_alive = Instant::now();
        if let Some(ref callback) = self.on_reconnect {
            callback();
        }
        info!(%url, "reconnected user data websocket (same listen key)");
        Ok(())
    }
    async fn reconnect_ws(&mut self) -> Result<()> {
        match self.reconnect_ws_without_new_key().await {
            Ok(()) => {
                return Ok(());
            }
            Err(e) => {
                warn!(error = %e, "reconnect with existing listen key failed, creating new listen key");
            }
        }
        let new_key =
            Self::create_listen_key(&self.client, &self.base, &self.api_key, self.kind).await?;
        self.listen_key = new_key;
        let url = Self::ws_url_for(self.kind, &self.listen_key);
        let (ws, _) = connect_async(&url).await?;
        self.ws = ws;
        self.last_keep_alive = Instant::now();
        if let Some(ref callback) = self.on_reconnect {
            callback();
            info!(%url, "reconnected user data websocket (new listen key), callback triggered");
        }
        info!(%url, "reconnected user data websocket (new listen key)");
        Ok(())
    }
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
        let listen_key = Self::create_listen_key(&client, &base, api_key, kind).await?;
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
    async fn keep_alive(&mut self) -> Result<()> {
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
        let new_key =
            Self::create_listen_key(&self.client, &self.base, &self.api_key, self.kind).await?;
        self.listen_key = new_key;
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
            match timeout(Duration::from_secs(90), self.ws.next()).await {
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
                    warn!("websocket timeout (90s), reconnecting");
                    self.reconnect_ws().await?;
                }
            }
        }
    }
fn map_event(value: &Value) -> Result<Option<UserEvent>> {
        let event_type = value.get("e").and_then(Value::as_str).unwrap_or_default();
        match event_type {
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
                let last_filled_qty = Self::parse_decimal(value, "l");
                let cumulative_filled_qty = Self::parse_decimal(value, "z");
                let order_qty = Self::parse_decimal(value, "q");
                let price = Self::parse_decimal(value, "L");
                let side =
                    Self::parse_side(value.get("S").and_then(Value::as_str).unwrap_or("SELL"));
                let is_maker = value.get("m").and_then(Value::as_bool).unwrap_or(false);
                let commission = Self::parse_decimal(value, "n");
                return Ok(Some(UserEvent::OrderFill {
                    symbol,
                    order_id,
                    side,
                    qty: Qty(last_filled_qty),
                    cumulative_filled_qty: Qty(cumulative_filled_qty),
                    order_qty: Some(Qty(order_qty)),
                    price: Px(price),
                    is_maker,
                    order_status: status.to_string(),
                    commission,
                }));
            }
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
                let last_filled_qty = Self::parse_decimal(data, "l");
                let cumulative_filled_qty = Self::parse_decimal(data, "z");
                let order_qty = Self::parse_decimal(data, "q");
                let price = Self::parse_decimal(data, "L");
                let side =
                    Self::parse_side(data.get("S").and_then(Value::as_str).unwrap_or("SELL"));
                let is_maker = data.get("m").and_then(Value::as_bool).unwrap_or(false);
                let commission = Self::parse_decimal(data, "n");
                return Ok(Some(UserEvent::OrderFill {
                    symbol,
                    order_id,
                    side,
                    qty: Qty(last_filled_qty),
                    cumulative_filled_qty: Qty(cumulative_filled_qty),
                    order_qty: Some(Qty(order_qty)),
                    price: Px(price),
                    is_maker,
                    order_status: status.to_string(),
                    commission,
                }));
            }
            "ACCOUNT_UPDATE" => {
                let mut positions = Vec::new();
                let mut balances = Vec::new();
                if let Some(positions_data) = value.get("a").and_then(|v| v.get("P")) {
                    if let Some(positions_array) = positions_data.as_array() {
                        for pos_data in positions_array {
                            if let (Some(symbol), Some(amt), Some(entry)) = (
                                pos_data.get("s").and_then(Value::as_str),
                                pos_data.get("pa").and_then(Value::as_str),
                                pos_data.get("ep").and_then(Value::as_str),
                            ) {
                                let position_amt = Decimal::from_str(amt).unwrap_or(Decimal::ZERO);
                                let entry_price = Decimal::from_str(entry).unwrap_or(Decimal::ZERO);
                                let leverage = pos_data
                                    .get("l")
                                    .and_then(Value::as_str)
                                    .and_then(|s| s.parse::<u32>().ok())
                                    .unwrap_or(1);
                                let unrealized_pnl = pos_data
                                    .get("up")
                                    .and_then(Value::as_str)
                                    .and_then(|s| Decimal::from_str(s).ok());
                                positions.push(AccountPosition {
                                    symbol: symbol.to_string(),
                                    position_amt,
                                    entry_price,
                                    leverage,
                                    unrealized_pnl,
                                });
                            }
                        }
                    }
                }
                if let Some(balances_data) = value.get("a").and_then(|v| v.get("B")) {
                    if let Some(balances_array) = balances_data.as_array() {
                        for bal_data in balances_array {
                            if let (Some(asset), Some(available)) = (
                                bal_data.get("a").and_then(Value::as_str),
                                bal_data.get("wb").and_then(Value::as_str),
                            ) {
                                let available_balance = Decimal::from_str(available).unwrap_or(Decimal::ZERO);
                                if asset == "USDT" || asset == "USDC" {
                                    balances.push(AccountBalance {
                                        asset: asset.to_string(),
                                        available_balance,
                                    });
                                }
                            }
                        }
                    }
                }
                if !positions.is_empty() || !balances.is_empty() {
                    return Ok(Some(UserEvent::AccountUpdate { positions, balances }));
                }
            }
            _ => {}
        }
        Ok(Some(UserEvent::Heartbeat))
    }
}
