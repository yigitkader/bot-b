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
        client_order_id: Option<String>, // Idempotency için
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
                    client_order_id,
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
                    client_order_id,
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
