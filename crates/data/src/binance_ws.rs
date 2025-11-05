use anyhow::{anyhow, Result};
use bot_core::types::{Px, Qty, Side};
use futures_util::{SinkExt, StreamExt};
use reqwest::Client;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::Value;
use std::str::FromStr;
use tokio::time::{Duration, Instant};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message, WebSocketStream};
use tracing::{debug, info};

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
struct ListenKey {
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
    pub async fn connect(
        client: Client,
        base: &str,
        api_key: &str,
        kind: UserStreamKind,
    ) -> Result<Self> {
        let listen_endpoint = match kind {
            UserStreamKind::Spot => format!("{}/api/v3/userDataStream", base),
            UserStreamKind::Futures => format!("{}/fapi/v1/listenKey", base),
        };
        let listen: ListenKey = client
            .post(&listen_endpoint)
            .header("X-MBX-APIKEY", api_key)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let ws_url = match kind {
            UserStreamKind::Spot => {
                format!("wss://stream.binance.com:9443/ws/{}", listen.listen_key)
            }
            UserStreamKind::Futures => {
                format!("wss://fstream.binance.com/ws/{}", listen.listen_key)
            }
        };
        let (ws, _) = connect_async(&ws_url).await?;
        info!(%ws_url, "connected user data websocket");

        Ok(Self {
            client,
            base: base.to_string(),
            api_key: api_key.to_string(),
            kind,
            listen_key: listen.listen_key,
            ws,
            last_keep_alive: Instant::now(),
        })
    }

    async fn keep_alive(&mut self) -> Result<()> {
        if self.last_keep_alive.elapsed() < Duration::from_secs(60 * 25) {
            return Ok(());
        }
        let endpoint = match self.kind {
            UserStreamKind::Spot => format!(
                "{}/api/v3/userDataStream?listenKey={}",
                self.base, self.listen_key
            ),
            UserStreamKind::Futures => format!(
                "{}/fapi/v1/listenKey?listenKey={}",
                self.base, self.listen_key
            ),
        };
        self.client
            .put(&endpoint)
            .header("X-MBX-APIKEY", &self.api_key)
            .send()
            .await?
            .error_for_status()?;
        self.last_keep_alive = Instant::now();
        debug!("refreshed user data listen key");
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
        self.keep_alive().await?;
        while let Some(msg) = self.ws.next().await {
            let msg = msg.map_err(|e| match e {
                WsError::ConnectionClosed | WsError::AlreadyClosed => anyhow!("user stream closed"),
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
                    let data = value.get("data").cloned().unwrap_or(value);
                    if let Some(event) = Self::map_event(&data)? {
                        return Ok(event);
                    }
                }
                Message::Binary(_) => continue,
                Message::Close(_) => return Err(anyhow!("user stream closed")),
                Message::Frame(_) => continue,
            }
        }
        Err(anyhow!("user stream terminated"))
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
                let status = value.get("X").and_then(Value::as_str).unwrap_or_default();
                if status == "CANCELED" {
                    return Ok(Some(UserEvent::OrderCanceled { symbol, order_id }));
                }
                let exec_type = value.get("x").and_then(Value::as_str).unwrap_or_default();
                if exec_type != "TRADE" {
                    return Ok(Some(UserEvent::Heartbeat));
                }
                let qty = Self::parse_decimal(value, "l");
                let price = Self::parse_decimal(value, "L");
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
                let qty = Self::parse_decimal(data, "l");
                let price = Self::parse_decimal(data, "L");
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
