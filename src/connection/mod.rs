pub mod rest;
pub mod websocket;

use crate::config::BotConfig;
use crate::types::{
    api::{DepthState, LiqState},
    {Connection, RateLimiter, ConnectionChannels, MarketTick},
};
use anyhow::{anyhow, Result};
use chrono::Utc;
use futures::StreamExt;
use log::{info, warn};
use reqwest::Client;
use std::sync::Arc;
use tokio::{
    sync::RwLock,
    time::{sleep, Duration},
};
use tokio_tungstenite::connect_async;

const MAX_WS_MESSAGE_SIZE: usize = 1024 * 1024;

pub(crate) fn check_message_size(size: usize, stream_name: &str) -> Result<()> {
    if size > MAX_WS_MESSAGE_SIZE {
        return Err(anyhow!(
            "WebSocket message too large: {} bytes (max: {} bytes) on {} stream - possible DOS attack",
            size,
            MAX_WS_MESSAGE_SIZE,
            stream_name
        ));
    }
    Ok(())
}

impl Connection {
    pub fn new(config: BotConfig) -> Self {
        let http = Client::builder()
            .user_agent("bot-b/0.1")
            .build()
            .expect("failed to build reqwest client");
        let rate_limiter = Arc::new(RateLimiter::new());
        let server_time_offset = Arc::new(tokio::sync::RwLock::new(0i64));
        let last_market_tick_ts = Arc::new(tokio::sync::RwLock::new(None));
        let last_ws_connection_ts = Arc::new(tokio::sync::RwLock::new(None));
        Self {
            config,
            http,
            rate_limiter,
            server_time_offset,
            last_market_tick_ts,
            last_ws_connection_ts,
        }
    }

    pub async fn check_websocket_health(&self) -> (bool, i64) {
        let now = Utc::now();
        let last_tick = self.last_market_tick_ts.read().await;
        
        if let Some(last_ts) = *last_tick {
            let elapsed = now.signed_duration_since(last_ts).num_seconds();
            let stale_threshold = self.config.market_tick_stale_tolerance_secs;
            (elapsed > stale_threshold, elapsed)
        } else {
            (true, i64::MAX)
        }
    }

    pub(crate) async fn update_market_tick_timestamp(&self, ts: chrono::DateTime<Utc>) {
        *self.last_market_tick_ts.write().await = Some(ts);
    }

    pub(crate) async fn update_ws_connection_timestamp(&self) {
        *self.last_ws_connection_ts.write().await = Some(Utc::now());
    }

    pub fn quote_asset(&self) -> &str {
        &self.config.quote_asset
    }

    pub fn config(&self) -> &BotConfig {
        &self.config
    }

    pub async fn initialize_trading_settings(&self) -> Result<()> {
        use crate::connection::rest;
        rest::ensure_credentials(self)?;
        let symbol = &self.config.symbol;
        info!("CONNECTION: Initializing trading settings for {}", symbol);

        if let Err(e) = rest::sync_server_time(self).await {
            warn!("CONNECTION: Failed to sync server time on startup: {}. Using client time as fallback.", e);
        }

        rest::set_margin_mode(self, symbol, self.config.use_isolated_margin).await?;
        rest::set_leverage(self, symbol, self.config.leverage).await?;

        info!("CONNECTION: Trading settings initialized successfully");
        Ok(())
    }

    pub async fn fetch_snapshot(&self) -> Result<MarketTick> {
        use crate::connection::rest;
        rest::fetch_snapshot(self).await
    }

    pub async fn fetch_balance(&self) -> Result<Vec<crate::types::BalanceSnapshot>> {
        use crate::connection::rest;
        rest::fetch_balance(self).await
    }

    pub async fn fetch_position(&self, symbol: &str) -> Result<Option<crate::types::PositionUpdate>> {
        use crate::connection::rest;
        rest::fetch_position(self, symbol).await
    }

    pub async fn fetch_symbol_info(&self, symbol: &str) -> Result<crate::types::core::SymbolPrecision> {
        use crate::connection::rest;
        rest::fetch_symbol_info(self, symbol).await
    }

    pub async fn calculate_order_price(&self, symbol: &str, side: &crate::types::core::Side) -> Result<f64> {
        use crate::connection::rest;
        rest::calculate_order_price(self, symbol, side).await
    }

    pub async fn send_order(&self, order: crate::types::core::NewOrderRequest) -> Result<()> {
        use crate::connection::rest;
        rest::send_order(self, order).await
    }

    pub async fn send_order_fast(
        &self,
        order: &crate::types::core::NewOrderRequest,
        symbol_info: &crate::types::core::SymbolPrecision,
        best_bid: f64,
        best_ask: f64,
    ) -> Result<()> {
        use crate::connection::rest;
        rest::send_order_fast(self, order, symbol_info, best_bid, best_ask).await
    }

    pub async fn run_market_ws(self: Arc<Self>, ch: ConnectionChannels) -> Result<()> {
        self.run_market_ws_with_depth_cache(ch, None).await
    }

    pub async fn run_market_ws_with_depth_cache(
        self: Arc<Self>,
        ch: ConnectionChannels,
        depth_cache: Option<Arc<crate::cache::DepthCache>>,
    ) -> Result<()> {
        let depth_state = Arc::new(RwLock::new(DepthState::new(20)));
        let liq_state = Arc::new(RwLock::new(LiqState::new(self.config.liq_window_secs)));

        loop {
            let mark_stream = self.clone();
            let depth_stream = self.clone();
            let liq_stream_conn = self.clone();
            let liq_oi_conn = self.clone();
            let ch_mark = ch.clone();
            let depth_for_mark = depth_state.clone();
            let depth_for_depth = depth_state.clone();
            let depth_cache_for_stream = depth_cache.clone();
            let symbol = self.config.symbol.clone();
            let liq_for_mark = liq_state.clone();
            let liq_for_stream = liq_state.clone();
            let liq_for_oi = liq_state.clone();

            let mark_task = tokio::spawn(async move {
                use crate::connection::websocket;
                websocket::consume_mark_price_stream(mark_stream, ch_mark, depth_for_mark, liq_for_mark)
                    .await
            });
            let depth_task = tokio::spawn(async move {
                use crate::connection::websocket;
                websocket::consume_depth_stream_with_cache(
                    depth_stream,
                    depth_for_depth,
                    depth_cache_for_stream,
                    symbol,
                )
                    .await
            });
            let liq_stream_task = tokio::spawn(async move {
                use crate::connection::websocket;
                if let Err(err) = websocket::run_liq_stream(liq_stream_conn, liq_for_stream).await {
                    warn!("CONNECTION: liq stream exited: {err:?}");
                }
            });
            let liq_oi_task = tokio::spawn(async move {
                use crate::connection::websocket;
                if let Err(err) = websocket::run_open_interest_stream(liq_oi_conn, liq_for_oi).await {
                    warn!("CONNECTION: open interest stream exited: {err:?}");
                }
            });

            let _ = tokio::join!(mark_task, depth_task, liq_stream_task, liq_oi_task);
            warn!("CONNECTION: market streams stopped. Reconnecting soon...");
            sleep(Duration::from_secs(2)).await;
        }
    }

    pub async fn run_user_ws(self: Arc<Self>, ch: ConnectionChannels) -> Result<()> {
        use crate::connection::websocket;
        loop {
            match websocket::create_listen_key(&self).await {
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
                        websocket::keep_alive_listen_key(keepalive_conn, &key_clone).await;
                    });

                    match connect_async(&ws_url).await {
                        Ok((mut ws_stream, _)) => {
                            info!("CONNECTION: user-data WS connected");
                            while let Some(msg) = ws_stream.next().await {
                                match websocket::extract_text_from_message(&msg, "user-data") {
                                    Ok(Some(txt)) => {
                                        if let Err(err) = websocket::handle_user_message(&self, &txt, &ch).await
                                        {
                                            warn!("CONNECTION: user-data handle error: {err:?}");
                                            break;
                                        }
                                    }
                                    Ok(None) => {}
                                    Err(_) => break,
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

    pub fn build_combined_stream_url(symbols: &[String], stream_type: &str, interval: Option<&str>) -> String {
        let base = "wss://fstream.binance.com";
        let streams: Vec<String> = symbols
            .iter()
            .map(|s| {
                let symbol_lower = s.to_lowercase();
                if let Some(interval) = interval {
                    format!("{}@{}@{}", symbol_lower, stream_type, interval)
                } else {
                    format!("{}@{}", symbol_lower, stream_type)
                }
            })
            .collect();
        format!("{}/stream?streams={}", base, streams.join("/"))
    }
}

// Re-export rest module functions
pub use rest::{
    calculate_order_price, fetch_balance, fetch_max_leverage, fetch_position, fetch_snapshot,
    fetch_symbol_info, format_quantity, round_to_step_size, send_order, send_order_fast,
    set_leverage, set_margin_mode, sync_server_time,
};

