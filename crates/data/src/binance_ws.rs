//location: /crates/data/src/binance_ws.rs
use anyhow::{anyhow, Result};
use bot_core::types::{Px, Qty, Side};
use futures_util::{SinkExt, StreamExt};
use reqwest::Client;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::Value;
use std::str::FromStr;
use tokio::time::{timeout, Duration, Instant};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message, WebSocketStream};
use tracing::{debug, error, info, warn};

use tokio_tungstenite::tungstenite::Error as WsError;

pub type WsStream = WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Clone, Copy, Debug)]
pub enum UserStreamKind {
    Spot,
    Futures,
}

#[derive(Debug, Clone)]
pub enum UserEvent {
    OrderFill {
        symbol: String,
        order_id: String,
        side: Side,
        qty: Qty,
        price: Px,
    },
    OrderCanceled {
        symbol: String,
        order_id: String,
    },
    Heartbeat,
}

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
}

impl UserDataStream {
    #[inline]
    fn ws_url_for(kind: UserStreamKind, listen_key: &str) -> String {
        match kind {
            UserStreamKind::Spot => {
                // Spot user data
                format!("wss://stream.binance.com:9443/ws/{}", listen_key)
            }
            UserStreamKind::Futures => {
                // USDⓈ-M Futures user data
                format!("wss://fstream.binance.com/ws/{}", listen_key)
            }
        }
    }

    async fn create_listen_key(
        client: &Client,
        base: &str,
        api_key: &str,
        kind: UserStreamKind,
    ) -> Result<String> {
        let base = base.trim_end_matches('/');
        let endpoint = match kind {
            UserStreamKind::Spot => format!("{}/api/v3/userDataStream", base),
            UserStreamKind::Futures => format!("{}/fapi/v1/listenKey", base),
        };

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

        #[derive(Deserialize)]
        struct ListenKeyResp {
            #[serde(rename = "listenKey")]
            listen_key: String,
        }
        let lk: ListenKeyResp = resp.json().await?;
        info!(listen_key=%lk.listen_key, "listenKey created");
        Ok(lk.listen_key)
    }

    async fn keepalive_listen_key(&self, listen_key: &str) -> Result<()> {
        let base = self.base.trim_end_matches('/');
        let endpoint = match self.kind {
            UserStreamKind::Spot => {
                format!("{}/api/v3/userDataStream?listenKey={}", base, listen_key)
            }
            UserStreamKind::Futures => {
                format!("{}/fapi/v1/listenKey?listenKey={}", base, listen_key)
            }
        };

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
    async fn reconnect_ws(&mut self) -> Result<()> {
        let url = Self::ws_url_for(self.kind, &self.listen_key);
        let (ws, _) = connect_async(&url).await?;
        self.ws = ws;
        self.last_keep_alive = Instant::now();
        info!(%url, "reconnected user data websocket");
        Ok(())
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
            // SPOT executionReport
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
                let status = value.get("X").and_then(Value::as_str).unwrap_or_default();
                if status == "CANCELED" {
                    return Ok(Some(UserEvent::OrderCanceled { symbol, order_id }));
                }
                let exec_type = value.get("x").and_then(Value::as_str).unwrap_or_default();
                if exec_type != "TRADE" {
                    return Ok(Some(UserEvent::Heartbeat));
                }
                let qty = Self::parse_decimal(value, "l"); // last executed qty
                let price = Self::parse_decimal(value, "L"); // last executed price
                let side =
                    Self::parse_side(value.get("S").and_then(Value::as_str).unwrap_or("SELL"));
                return Ok(Some(UserEvent::OrderFill {
                    symbol,
                    order_id,
                    side,
                    qty: Qty(qty),
                    price: Px(price),
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
                let status = data.get("X").and_then(Value::as_str).unwrap_or_default();
                if status == "CANCELED" {
                    return Ok(Some(UserEvent::OrderCanceled { symbol, order_id }));
                }
                let exec_type = data.get("x").and_then(Value::as_str).unwrap_or_default();
                if exec_type != "TRADE" {
                    return Ok(Some(UserEvent::Heartbeat));
                }
                let qty = Self::parse_decimal(data, "l"); // last filled
                let price = Self::parse_decimal(data, "L"); // last price
                let side =
                    Self::parse_side(data.get("S").and_then(Value::as_str).unwrap_or("SELL"));
                return Ok(Some(UserEvent::OrderFill {
                    symbol,
                    order_id,
                    side,
                    qty: Qty(qty),
                    price: Px(price),
                }));
            }

            _ => {}
        }
        Ok(Some(UserEvent::Heartbeat))
    }
}
