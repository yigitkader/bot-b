use crate::config::BotConfig;
use crate::types::ConnectionChannels;
use crate::types::{
    AccountBalance, AccountPosition, AccountUpdate, BalanceSnapshot, Connection,
    DepthEvent, DepthSnapshot, ExchangeInfoResponse, ForceOrderRecord,
    ForceOrderStreamEvent, ForceOrderStreamOrder, ForceOrderStreamWrapper, FuturesBalance,
    LeverageBracketResponse, LiqEntry, LiqState, ListenKeyResponse, MarkPriceEvent, MarketTick,
    NewOrderRequest, OpenInterestEvent, OpenInterestResponse, OrderStatus,
    OrderTradeUpdate, OrderUpdate, PositionRiskResponse, PositionUpdate, PremiumIndex, Side,
    SymbolPrecision,
};
use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use hex;
use hmac::{Hmac, Mac};
use log::{info, warn};
use reqwest::Client;
use serde::Deserialize;
use sha2::Sha256;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration as StdDuration, Instant};
use tokio::{
    sync::RwLock,
    time::{sleep, Duration},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use uuid::Uuid;
type HmacSha256 = Hmac<Sha256>;

/// Binance server time response structure
#[derive(Debug, Deserialize)]
struct ServerTimeResponse {
    #[serde(rename = "serverTime")]
    server_time: i64,
}

// Maximum WebSocket message size (1MB) - prevents DOS attacks and memory exhaustion
// Binance messages are typically < 10KB, but depth snapshots can be larger
const MAX_WS_MESSAGE_SIZE: usize = 1024 * 1024; // 1MB

impl Connection {
    fn check_message_size(size: usize, stream_name: &str) -> Result<()> {
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

    fn extract_text_from_message(
        msg: &Result<Message, tokio_tungstenite::tungstenite::Error>,
        stream_name: &str,
    ) -> Result<Option<String>> {
        match msg {
            Ok(Message::Text(txt)) => {
                Self::check_message_size(txt.len(), stream_name)?;
                Ok(Some(txt.clone()))
            }
            Ok(Message::Binary(bin)) => {
                Self::check_message_size(bin.len(), stream_name)?;
                Ok(String::from_utf8(bin.clone()).ok())
            }
            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) | Ok(Message::Frame(_)) => Ok(None),
            Ok(Message::Close(frame)) => {
                warn!("CONNECTION: {} WS closed: {:?}", stream_name, frame);
                Err(anyhow!("WebSocket closed"))
            }
            Err(err) => {
                warn!("CONNECTION: {} WS error: {}", stream_name, err);
                Err(anyhow!("WebSocket error: {}", err))
            }
        }
    }
    pub fn new(config: BotConfig) -> Self {
        let http = Client::builder()
            .user_agent("bot-b/0.1")
            .build()
            .expect("failed to build reqwest client");
        let rate_limiter = Arc::new(crate::types::RateLimiter::new());
        let server_time_offset = Arc::new(tokio::sync::RwLock::new(0i64));
        Self {
            config,
            http,
            rate_limiter,
            server_time_offset,
        }
    }

    pub fn quote_asset(&self) -> &str {
        &self.config.quote_asset
    }

    pub fn config(&self) -> &BotConfig {
        &self.config
    }

    pub async fn initialize_trading_settings(&self) -> Result<()> {
        self.ensure_credentials()?;
        let symbol = &self.config.symbol;
        info!("CONNECTION: Initializing trading settings for {}", symbol);

        if let Err(e) = self.sync_server_time().await {
            warn!("CONNECTION: Failed to sync server time on startup: {}. Using client time as fallback.", e);
        }

        self.set_margin_mode(symbol, self.config.use_isolated_margin).await?;
        self.set_leverage(symbol, self.config.leverage).await?;

        info!("CONNECTION: Trading settings initialized successfully");
        Ok(())
    }

    pub async fn fetch_snapshot(&self) -> Result<MarketTick> {
        let premium = self.fetch_premium_index().await?;
        let (symbol, price, funding_rate, ts) = premium.into_parts()?;
        let obi = self.fetch_depth_obi().await?;
        let (liq_long, liq_short) = self
            .fetch_liquidation_clusters()
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

    pub async fn run_market_ws(self: Arc<Self>, ch: ConnectionChannels) -> Result<()> {
        self.run_market_ws_with_depth_cache(ch, None).await
    }

    /// Market WebSocket with depth cache integration (TrendPlan.md)
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
                mark_stream
                    .consume_mark_price_stream(ch_mark, depth_for_mark, liq_for_mark)
                    .await
            });
            let depth_task = tokio::spawn(async move {
                depth_stream
                    .consume_depth_stream_with_cache(
                        depth_for_depth,
                        depth_cache_for_stream,
                        symbol,
                    )
                    .await
            });
            let liq_stream_task = tokio::spawn(async move {
                if let Err(err) = liq_stream_conn.run_liq_stream(liq_for_stream).await {
                    warn!("CONNECTION: liq stream exited: {err:?}");
                }
            });
            let liq_oi_task = tokio::spawn(async move {
                if let Err(err) = liq_oi_conn.run_open_interest_stream(liq_for_oi).await {
                    warn!("CONNECTION: open interest stream exited: {err:?}");
                }
            });

            let _ = tokio::join!(mark_task, depth_task, liq_stream_task, liq_oi_task);
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
                                match Self::extract_text_from_message(&msg, "user-data") {
                                    Ok(Some(txt)) => {
                                        if let Err(err) = self.handle_user_message(&txt, &ch).await
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

    fn calculate_limit_price(side: &Side, best_bid: f64, best_ask: f64, tick_size: f64) -> f64 {
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

    pub async fn calculate_order_price(&self, symbol: &str, side: &Side) -> Result<f64> {
        let depth = self.fetch_depth_snapshot().await?;

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

        let symbol_info = self.fetch_symbol_info(symbol).await?;
        let price = Self::calculate_limit_price(side, best_bid, best_ask, symbol_info.tick_size);

        Ok(price)
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

        self.set_margin_mode(&symbol, self.config.use_isolated_margin).await?;
        self.set_leverage(&symbol, self.config.leverage).await?;
        let symbol_info = self.fetch_symbol_info(&symbol).await?;
        let qty_rounded = Self::round_to_step_size(quantity, symbol_info.step_size);
        let qty_formatted = Self::format_quantity(qty_rounded, symbol_info.step_size);

        let side_str = match side {
            Side::Long => "BUY",
            Side::Short => "SELL",
        };

        let order_type = if reduce_only { "MARKET" } else { "LIMIT" };

        let mut params = vec![
            ("symbol".to_string(), symbol.clone()),
            ("side".to_string(), side_str.to_string()),
            ("type".to_string(), order_type.to_string()),
            ("quantity".to_string(), qty_formatted),
        ];

        if order_type == "LIMIT" {
            match self.fetch_depth_snapshot().await {
                Ok(depth) => {
                    let best_bid = depth.bids.first().and_then(|lvl| lvl[0].parse::<f64>().ok()).unwrap_or(0.0);
                    let best_ask = depth.asks.first().and_then(|lvl| lvl[0].parse::<f64>().ok()).unwrap_or(0.0);

                    if best_bid > 0.0 && best_ask > 0.0 {
                        let limit_price = Self::calculate_limit_price(&side, best_bid, best_ask, symbol_info.tick_size);
                        let price_formatted = if symbol_info.tick_size > 0.0 {
                            Self::format_quantity(limit_price, symbol_info.tick_size)
                        } else {
                            format!("{:.8}", limit_price)
                        };
                        params.push(("price".to_string(), price_formatted));
                        params.push(("timeInForce".to_string(), "GTC".to_string()));
                    } else {
                        warn!("CONNECTION: invalid bid/ask prices, falling back to market order");
                        params[2] = ("type".to_string(), "MARKET".to_string());
                    }
                }
                Err(err) => {
                    warn!("CONNECTION: failed to fetch depth snapshot for limit price: {err:?}, falling back to market order");
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

        self.signed_post("/fapi/v1/order", params).await?;
        info!(
            "CONNECTION: order sent (type={}, reduce_only={})",
            order_type, reduce_only
        );
        Ok(())
    }

    /// Fast order sender (minimal validation, uses cached data)
    /// Target: Signal → Fill in <50ms
    /// Eliminates redundant API calls by using pre-cached symbol info and depth data
    pub async fn send_order_fast(
        &self,
        order: &NewOrderRequest,
        symbol_info: &crate::types::SymbolPrecision,
        _best_bid: f64,
        _best_ask: f64,
    ) -> Result<()> {
        self.ensure_credentials()?;

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

        // Only essential validation
        let qty_rounded = Self::round_to_step_size(*quantity, symbol_info.step_size);
        let qty_formatted = Self::format_quantity(qty_rounded, symbol_info.step_size);

        let side_str = match side {
            Side::Long => "BUY",
            Side::Short => "SELL",
        };

        // Always use MARKET for speed (fastest execution)
        let mut params = vec![
            ("symbol".to_string(), symbol.clone()),
            ("side".to_string(), side_str.to_string()),
            ("type".to_string(), "MARKET".to_string()),
            ("quantity".to_string(), qty_formatted),
        ];

        if *reduce_only {
            params.push(("reduceOnly".into(), "true".into()));
        }
        if let Some(id) = client_order_id {
            params.push(("newClientOrderId".into(), id.clone()));
        }

        // Send order (single API call)
        self.signed_post("/fapi/v1/order", params).await?;
        
        Ok(())
    }

    pub async fn fetch_max_leverage(&self, symbol: &str) -> Result<f64> {
        let params = vec![("symbol".to_string(), symbol.to_string())];
        let response = self
            .signed_get("/fapi/v1/leverageBracket", params)
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

    pub async fn set_margin_mode(&self, symbol: &str, isolated: bool) -> Result<()> {
        self.ensure_credentials()?;

        let margin_type = if isolated { "ISOLATED" } else { "CROSSED" };
        let params = vec![
            ("symbol".to_string(), symbol.to_string()),
            ("marginType".to_string(), margin_type.to_string()),
        ];

        self.signed_post("/fapi/v1/marginType", params).await?;
        info!(
            "CONNECTION: margin mode set to {} for {}",
            margin_type, symbol
        );
        Ok(())
    }

    pub async fn set_leverage(&self, symbol: &str, leverage: f64) -> Result<()> {
        self.ensure_credentials()?;

        if leverage <= 0.0 {
            return Err(anyhow!("leverage must be positive"));
        }

        // Fetch maximum leverage from Binance for this symbol
        let max_leverage = self.fetch_max_leverage(symbol).await?;

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

        self.signed_post("/fapi/v1/leverage", params).await?;
        info!(
            "CONNECTION: leverage set to {}x for {} (max: {}x)",
            leverage, symbol, max_leverage
        );
        Ok(())
    }

    pub async fn fetch_symbol_info(&self, symbol: &str) -> Result<SymbolPrecision> {
        let url = format!("{}/fapi/v1/exchangeInfo", self.config.base_url);
        let response = self
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

    pub async fn fetch_balance(&self) -> Result<Vec<BalanceSnapshot>> {
        self.ensure_credentials()?;

        let response = self
            .signed_get("/fapi/v2/balance", Vec::<(String, String)>::new())
            .await?
            .json::<Vec<FuturesBalance>>()
            .await
            .context("failed to parse balance response")?;

        let critical_assets: Vec<&str> = vec!["USDT", "USDC", &self.config.quote_asset];

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
                        warn!("CONNECTION: failed to parse balance for asset {}: '{}' - {}", asset, record.available_balance, err);
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
                warn!(
                    "CONNECTION: critical asset {} not found in balance response (may have zero balance)",
                    critical
                );
            }
        }

        Ok(snapshots)
    }

    pub async fn fetch_position(&self, symbol: &str) -> Result<Option<PositionUpdate>> {
        self.ensure_credentials()?;

        let mut params = Vec::new();
        params.push(("symbol".to_string(), symbol.to_string()));

        let response = self
            .signed_get("/fapi/v2/positionRisk", params)
            .await?
            .json::<Vec<PositionRiskResponse>>()
            .await
            .context("failed to parse position risk response")?;

        // Find position for the requested symbol
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

    async fn consume_mark_price_stream(
        self: Arc<Self>,
        ch: ConnectionChannels,
        depth_state: Arc<RwLock<DepthState>>,
        liq_state: Arc<RwLock<LiqState>>,
    ) -> Result<()> {
        use tokio::sync::broadcast;

        let mut retry_delay = Duration::from_secs(1);
        loop {
            let url = self.mark_price_stream_url();
            match connect_async(&url).await {
                Ok((ws_stream, _)) => {
                    info!("CONNECTION: mark price stream connected ({url})");
                    retry_delay = Duration::from_secs(1);
                    let (_, mut read) = ws_stream.split();
                    let mut lag_monitor = ch.market_tx.subscribe();
                    while let Some(message) = read.next().await {
                        match Self::extract_text_from_message(&message, "mark-price") {
                            Ok(Some(txt)) => {
                                if let Some(mut tick) = self.parse_mark_price(&txt) {
                                    tick.obi = (depth_state.read().await).obi();
                                    let (liq_long, liq_short) = (liq_state.read().await).snapshot();
                                    tick.liq_long_cluster = liq_long;
                                    tick.liq_short_cluster = liq_short;
                                    populate_tick_with_depth(&depth_state, &mut tick).await;

                                    match lag_monitor.try_recv() {
                                        Ok(_) | Err(broadcast::error::TryRecvError::Empty) => {
                                            if ch.market_tx.send(tick).is_err() {
                                                warn!("CONNECTION: market_tx receiver dropped");
                                                break;
                                            }
                                        }
                                        Err(broadcast::error::TryRecvError::Lagged(n)) => {
                                            warn!("CONNECTION: Market tick lagged by {} messages", n);
                                            if let Ok(snapshot) = self.fetch_snapshot().await {
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

    async fn consume_depth_stream(
        self: Arc<Self>,
        depth_state: Arc<RwLock<DepthState>>,
    ) -> Result<()> {
        let symbol = self.config.symbol.clone();
        self.consume_depth_stream_with_cache(depth_state, None, symbol)
            .await
    }

    /// Depth stream with cache integration (TrendPlan.md)
    async fn consume_depth_stream_with_cache(
        self: Arc<Self>,
        depth_state: Arc<RwLock<DepthState>>,
        depth_cache: Option<Arc<crate::cache::DepthCache>>,
        symbol: String,
    ) -> Result<()> {
        let mut retry_delay = Duration::from_secs(1);
        loop {
            // Fetch snapshot before reconnecting to restore state
            let snapshot_result = self.fetch_depth_snapshot().await;
            if let Ok(snapshot) = snapshot_result {
                // ✅ CRITICAL: Update depth cache from snapshot FIRST (TrendPlan.md)
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
                
                // Then update depth state
                depth_state.write().await.reset_with_snapshot(snapshot);
                info!("CONNECTION: depth state restored from snapshot");
            } else {
                warn!("CONNECTION: failed to fetch depth snapshot, continuing with empty state");
            }

            let url = self.depth_stream_url();
            match connect_async(&url).await {
                Ok((ws_stream, _)) => {
                    info!("CONNECTION: depth stream connected ({url})");
                    retry_delay = Duration::from_secs(1);
                    let (_, mut read) = ws_stream.split();
                    while let Some(message) = read.next().await {
                        match Connection::extract_text_from_message(&message, "depth") {
                            Ok(Some(txt)) => {
                                if let Ok(event) = serde_json::from_str::<DepthEvent>(&txt) {
                                    depth_state.write().await.update(&event);
                                    
                                    // ✅ CRITICAL: Update depth cache from WebSocket event (TrendPlan.md)
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

    async fn run_liq_stream(self: Arc<Self>, liq_state: Arc<RwLock<LiqState>>) -> Result<()> {
        let stream_symbol = self.config.symbol.to_lowercase();
        let ws_url = format!(
            "{}/ws/{}@forceOrder",
            self.config.ws_base_url.trim_end_matches('/'),
            stream_symbol
        );

        loop {
            match connect_async(&ws_url).await {
                Ok((ws_stream, _)) => {
                    info!("CONNECTION: liquidation stream connected ({ws_url})");
                    let (_, mut read) = ws_stream.split();
                    while let Some(msg) = read.next().await {
                        match Self::extract_text_from_message(&msg, "liquidation") {
                            Ok(Some(txt)) => {
                                if let Err(err) =
                                    self.handle_force_order_message(&txt, &liq_state).await
                                {
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

    async fn handle_force_order_message(
        &self,
        payload: &str,
        liq_state: &Arc<RwLock<LiqState>>,
    ) -> Result<()> {
        if let Ok(wrapper) = serde_json::from_str::<ForceOrderStreamWrapper>(payload) {
            self.ingest_force_order(wrapper.data.order, liq_state)
                .await?;
        } else if let Ok(event) = serde_json::from_str::<ForceOrderStreamEvent>(payload) {
            self.ingest_force_order(event.order, liq_state).await?;
        }
        Ok(())
    }

    async fn ingest_force_order(
        &self,
        order: ForceOrderStreamOrder,
        liq_state: &Arc<RwLock<LiqState>>,
    ) -> Result<()> {
        if order.symbol != self.config.symbol {
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

    async fn run_open_interest_stream(
        self: Arc<Self>,
        liq_state: Arc<RwLock<LiqState>>,
    ) -> Result<()> {
        let mut retry_delay = Duration::from_secs(1);
        loop {
            let url = self.open_interest_stream_url();
            match connect_async(&url).await {
                Ok((ws_stream, _)) => {
                    info!("CONNECTION: open interest stream connected ({url})");
                    retry_delay = Duration::from_secs(1);
                    let (_, mut read) = ws_stream.split();
                    while let Some(message) = read.next().await {
                        match Self::extract_text_from_message(&message, "open-interest") {
                            Ok(Some(txt)) => {
                                if let Ok(event) = serde_json::from_str::<OpenInterestEvent>(&txt) {
                                    if event.symbol == self.config.symbol {
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

    fn open_interest_stream_url(&self) -> String {
        let base = self.config.ws_base_url.trim_end_matches('/');
        let symbol = self.config.symbol.to_lowercase();
        format!("{base}/ws/{symbol}@openInterest")
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
        let keep_alive_interval_secs = 25 * 60;
        let mut interval = tokio::time::interval(Duration::from_secs(keep_alive_interval_secs));
        interval.tick().await;

        loop {
            interval.tick().await;
            let mut retry_delay = Duration::from_secs(10);

            for attempt in 1..=3 {
                match self
                    .http
                    .put(&url)
                    .query(&[("listenKey", key)])
                    .header("X-MBX-APIKEY", &self.config.api_key)
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
                    if let Some(update) = pos.to_position_update(self.config.leverage) {
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

    /// ✅ CRITICAL FIX: Build Combined Stream URL for multiple symbols (TrendPlan.md - Action Plan)
    /// This reduces WebSocket connections from N (one per symbol) to 1 (combined stream)
    /// Format: /stream?streams=btcusdt@kline_5m/ethusdt@kline_5m/...
    /// Binance limit: Combined streams can handle up to 200 streams per connection
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
            bid_depth_usd: None,
            ask_depth_usd: None,
        })
    }

    async fn fetch_liquidation_clusters(&self) -> Result<(Option<f64>, Option<f64>)> {
        let params = vec![
            ("symbol".to_string(), self.config.symbol.clone()),
            ("limit".to_string(), "100".to_string()),
            ("autoCloseType".to_string(), "LIQUIDATION".to_string()),
        ];
        let records = self
            .signed_get("/fapi/v1/forceOrders", params)
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

        let oi = self.fetch_open_interest().await.unwrap_or(0.0);
        if oi <= 0.0 {
            return Ok((None, None));
        }

        let normalize = |value: f64| if value > 0.0 { Some(value / oi) } else { None };
        Ok((normalize(long_total), normalize(short_total)))
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

    async fn fetch_depth_snapshot(&self) -> Result<DepthSnapshot> {
        let url = format!("{}/fapi/v1/depth", self.config.base_url);
        let snapshot = self
            .http
            .get(&url)
            .query(&[("symbol", self.config.symbol.as_str()), ("limit", "20")])
            .send()
            .await?
            .error_for_status()?
            .json::<DepthSnapshot>()
            .await?;
        Ok(snapshot)
    }

    async fn fetch_depth_obi(&self) -> Result<Option<f64>> {
        let snapshot = self.fetch_depth_snapshot().await?;

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

    async fn fetch_premium_index(&self) -> Result<PremiumIndex> {
        let url = format!("{}/fapi/v1/premiumIndex", self.config.base_url);
        let resp = self
            .http
            .get(&url)
            .query(&[("symbol", self.config.symbol.as_str())])
            .send()
            .await?
            .error_for_status()?
            .json::<PremiumIndex>()
            .await?;
        Ok(resp)
    }

    async fn fetch_server_time_offset(&self) -> Result<i64> {
        let url = format!("{}/fapi/v1/time", self.config.base_url);
        let client_time_before = Utc::now().timestamp_millis();
        let resp = self
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

    pub async fn sync_server_time(&self) -> Result<()> {
        let offset = self
            .fetch_server_time_offset()
            .await
            .context("Failed to fetch Binance server time")?;

        let mut offset_guard = self.server_time_offset.write().await;
        *offset_guard = offset;

        info!(
            "CONNECTION: Server time synced - offset: {}ms (server {} client)",
            offset,
            if offset >= 0 { "ahead of" } else { "behind" }
        );

        Ok(())
    }

    fn get_synced_timestamp(&self) -> i64 {
        let offset = self.server_time_offset.try_read().map(|guard| *guard).unwrap_or(0);
        Utc::now().timestamp_millis() + offset
    }

    fn get_endpoint_weight(&self, path: &str) -> u32 {
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

    fn is_order_endpoint(&self, path: &str) -> bool {
        matches!(
            path,
            "/fapi/v1/order" | "/fapi/v1/marginType" | "/fapi/v1/leverage"
        )
    }

    async fn signed_get(&self, path: &str, params: Vec<(String, String)>) -> Result<reqwest::Response> {
        let weight = self.get_endpoint_weight(path);
        self.rate_limiter.acquire_weight(weight).await?;
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

    async fn signed_post(&self, path: &str, params: Vec<(String, String)>) -> Result<reqwest::Response> {
        let _order_permit = if self.is_order_endpoint(path) {
            Some(self.rate_limiter.acquire_order_permit().await?)
        } else {
            None
        };

        let weight = self.get_endpoint_weight(path);
        self.rate_limiter.acquire_weight(weight).await?;
        let body = self.sign_params(params)?;
        let url = format!("{}{}", self.config.base_url, path);
        let response_future = self
            .http
            .post(&url)
            .header("X-MBX-APIKEY", &self.config.api_key)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body)
            .send();
        drop(_order_permit);
        Ok(response_future.await?.error_for_status()?)
    }

    fn sign_params(&self, mut params: Vec<(String, String)>) -> Result<String> {
        let timestamp = self.get_synced_timestamp();
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

impl PremiumIndex {
    fn mark_price(&self) -> Result<f64> {
        self.mark_price_str
            .parse::<f64>()
            .context("failed to parse mark price")
    }

    fn funding_rate(&self) -> Option<f64> {
        self.last_funding_rate
            .as_ref()
            .and_then(|v| v.parse::<f64>().ok())
    }

    fn into_parts(self) -> Result<(String, f64, Option<f64>, DateTime<Utc>)> {
        let price = self.mark_price()?;
        let funding = self.funding_rate();
        let ts = DateTime::<Utc>::from_timestamp_millis(self.event_time_ms as i64)
            .unwrap_or_else(|| Utc::now());
        Ok((self.symbol, price, funding, ts))
    }
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
        // Merge bids: update or remove levels based on delta event
        for [price_str, qty_str] in &event.bids {
            let Ok(price) = price_str.parse::<f64>() else {
                continue;
            };
            let Ok(qty) = qty_str.parse::<f64>() else {
                continue;
            };

            if qty == 0.0 {
                self.bids.retain(|(p, _)| *p != price);
            } else {
                if let Some(level) = self.bids.iter_mut().find(|(p, _)| *p == price) {
                    level.1 = qty;
                } else {
                    self.bids.push((price, qty));
                }
            }
        }

        self.bids.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        self.bids.truncate(self.depth_limit);

        for [price_str, qty_str] in &event.asks {
            let Ok(price) = price_str.parse::<f64>() else { continue };
            let Ok(qty) = qty_str.parse::<f64>() else { continue };

            if qty == 0.0 {
                self.asks.retain(|(p, _)| *p != price);
            } else {
                if let Some(level) = self.asks.iter_mut().find(|(p, _)| *p == price) {
                    level.1 = qty;
                } else {
                    self.asks.push((price, qty));
                }
            }
        }

        self.asks.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        self.asks.truncate(self.depth_limit);

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

    fn reset_with_snapshot(&mut self, snapshot: DepthSnapshot) {
        self.bids = snapshot
            .bids
            .iter()
            .take(self.depth_limit)
            .filter_map(|lvl| {
                let price = lvl[0].parse::<f64>().ok()?;
                let qty = lvl[1].parse::<f64>().ok()?;
                Some((price, qty))
            })
            .collect();
        self.asks = snapshot
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

    fn best_bid(&self) -> Option<(f64, f64)> {
        self.bids.first().copied()
    }

    fn best_ask(&self) -> Option<(f64, f64)> {
        self.asks.first().copied()
    }

    fn depth_notional_usd(&self, is_bid: bool, levels: usize) -> Option<f64> {
        let book = if is_bid { &self.bids } else { &self.asks };
        if book.is_empty() {
            return None;
        }
        let mut total = 0.0;
        for (price, qty) in book.iter().take(levels.max(1)) {
            total += price * qty;
        }
        if total > 0.0 {
            Some(total)
        } else {
            None
        }
    }
}

impl LiqState {
    pub(crate) fn new(window_secs: u64) -> Self {
        Self {
            entries: VecDeque::new(),
            long_sum: 0.0,
            short_sum: 0.0,
            open_interest: 0.0,
            window_secs,
        }
    }

    fn record(&mut self, side: &str, notional: f64) {
        if self.open_interest <= f64::EPSILON || notional <= 0.0 {
            return;
        }
        let ratio = notional / self.open_interest;
        let now = Instant::now();
        self.prune(now);

        let is_long_cluster = side.eq_ignore_ascii_case("SELL");
        self.entries.push_back(LiqEntry {
            ts: now,
            ratio,
            is_long_cluster,
        });
        if is_long_cluster {
            self.long_sum += ratio;
        } else {
            self.short_sum += ratio;
        }
    }

    fn set_open_interest(&mut self, value: f64) {
        if value > 0.0 {
            self.open_interest = value;
        }
    }

    fn snapshot(&self) -> (Option<f64>, Option<f64>) {
        (
            if self.long_sum > 0.0 {
                Some(self.long_sum)
            } else {
                None
            },
            if self.short_sum > 0.0 {
                Some(self.short_sum)
            } else {
                None
            },
        )
    }

    fn prune(&mut self, now: Instant) {
        while let Some(entry) = self.entries.front() {
            let age = now
                .checked_duration_since(entry.ts)
                .unwrap_or(StdDuration::from_secs(self.window_secs + 1));

            if age > StdDuration::from_secs(self.window_secs) {
                let entry = self.entries.pop_front().unwrap();
                if entry.is_long_cluster {
                    self.long_sum = (self.long_sum - entry.ratio).max(0.0);
                } else {
                    self.short_sum = (self.short_sum - entry.ratio).max(0.0);
                }
            } else {
                break;
            }
        }
    }
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

impl AccountPosition {
    fn to_position_update(&self, leverage: f64) -> Option<PositionUpdate> {
        let size = self.position_amount.parse::<f64>().ok()?;
        let entry = self.entry_price.parse::<f64>().ok().unwrap_or(0.0);
        let pnl = self.unrealized_pnl.parse::<f64>().ok().unwrap_or(0.0);
        let side = if size >= 0.0 { Side::Long } else { Side::Short };

        Some(PositionUpdate {
            position_id: PositionUpdate::position_id(&self.symbol, side), // Unique ID from symbol+side+timestamp
            symbol: self.symbol.clone(),
            side,
            entry_price: entry,
            size: size.abs(),
            leverage,
            unrealized_pnl: pnl,
            ts: Utc::now(),
            is_closed: size == 0.0,
        })
    }
}
