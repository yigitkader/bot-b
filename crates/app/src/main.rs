//location: /crates/app/src/main.rs

use anyhow::{anyhow, Result};
use bot_core::types::*;
use data::binance_ws::{UserDataStream, UserEvent, UserStreamKind};
use exec::binance::{BinanceCommon, BinanceFutures, BinanceSpot, SymbolMeta};
use exec::Venue;
use risk::{RiskAction, RiskLimits};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::Deserialize;
use strategy::{Context, DynMm, DynMmCfg, Strategy};
use tokio::sync::mpsc;
use tracing::{info, warn};

use std::cmp::max;
use std::collections::HashMap;
use std::time::{Duration, Instant};

struct SymbolState {
    meta: SymbolMeta,
    inv: Qty,
    strategy: Box<dyn Strategy>,
    active_orders: HashMap<String, OrderInfo>,
    pnl_history: Vec<Decimal>,
}

#[derive(Clone, Debug)]
struct OrderInfo {
    order_id: String,
    side: Side,
    price: Px,
    qty: Qty,
    created_at: Instant,
}

fn compute_drawdown_bps(history: &[Decimal]) -> i64 {
    if history.is_empty() {
        return 0;
    }
    let mut peak = history[0];
    let mut max_drawdown = Decimal::ZERO;
    for value in history {
        if *value > peak {
            peak = *value;
        }
        if peak > Decimal::ZERO {
            let drawdown = ((*value - peak) / peak) * Decimal::from(10_000i32);
            if drawdown < max_drawdown {
                max_drawdown = drawdown;
            }
        }
    }
    max_drawdown.to_i64().unwrap_or(0)
}

#[derive(Debug, Deserialize)]
struct RiskCfg {
    inv_cap: String,
    min_liq_gap_bps: f64,
    dd_limit_bps: i64,
    max_leverage: u32,
}

#[derive(Debug, Deserialize)]
struct StratCfg {
    r#type: String,
    a: f64,
    b: f64,
    base_size: String,
    #[serde(default)]
    inv_cap: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExecCfg {
    tif: String,
    venue: String,
    #[serde(default = "default_cancel_interval")]
    cancel_replace_interval_ms: u64,
    #[serde(default = "default_max_order_age")]
    max_order_age_ms: u64,
}

fn default_cancel_interval() -> u64 {
    1_000
}

fn default_max_order_age() -> u64 {
    10_000
}

#[derive(Debug, Deserialize, Default)]
struct WebsocketCfg {
    #[serde(default)]
    enabled: bool,
    #[serde(default = "default_ws_reconnect_delay")]
    reconnect_delay_ms: u64,
    #[serde(default = "default_ws_ping_interval")]
    ping_interval_ms: u64,
}

fn default_ws_reconnect_delay() -> u64 {
    5_000
}

fn default_ws_ping_interval() -> u64 {
    30_000
}

#[derive(Debug, Deserialize)]
struct BinanceCfg {
    spot_base: String,
    futures_base: String,
    api_key: String,
    secret_key: String,
    recv_window_ms: u64,
}

fn default_quote_asset() -> String {
    "USDC".to_string()
}

#[derive(Debug, Deserialize)]
struct AppCfg {
    #[serde(default)]
    symbol: Option<String>,
    #[serde(default)]
    symbols: Vec<String>,
    #[serde(default, alias = "auto_discover_usdt")]
    auto_discover_quote: bool,
    #[serde(default = "default_quote_asset")]
    quote_asset: String,
    mode: String,
    metrics_port: Option<u16>,
    max_usd_per_order: f64,
    #[serde(default)]
    min_usd_per_order: Option<f64>,
    leverage: Option<u32>,
    price_tick: f64,
    qty_step: f64,
    binance: BinanceCfg,
    risk: RiskCfg,
    strategy: StratCfg,
    exec: ExecCfg,
    #[serde(default)]
    websocket: WebsocketCfg,
}

fn load_cfg() -> Result<AppCfg> {
    let args: Vec<String> = std::env::args().collect();
    let path = args
        .windows(2)
        .find_map(|w| {
            if w[0] == "--config" {
                Some(w[1].clone())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "./config.yaml".to_string());
    let s = std::fs::read_to_string(&path)?;
    let cfg: AppCfg = serde_yaml::from_str(&s)?;
    Ok(cfg)
}

fn tif_from_cfg(s: &str) -> Tif {
    match s.to_lowercase().as_str() {
        "gtc" => Tif::Gtc,
        "ioc" => Tif::Ioc,
        "post_only" => Tif::PostOnly,
        _ => Tif::PostOnly,
    }
}

fn clamp_qty_by_usd(qty: Qty, px: Px, max_usd: f64, qty_step: f64) -> Qty {
    let p = px.0.to_f64().unwrap_or(0.0);
    if p <= 0.0 || max_usd <= 0.0 {
        return Qty(Decimal::ZERO);
    }
    let max_qty = (max_usd / p).floor_div_step(qty_step);
    let wanted = qty.0.to_f64().unwrap_or(0.0);
    let q = wanted.min(max_qty);
    Qty(Decimal::from_f64_retain(q).unwrap_or(Decimal::ZERO))
}

fn clamp_qty_by_base(qty: Qty, max_base: f64, qty_step: f64) -> Qty {
    if max_base <= 0.0 {
        return Qty(Decimal::ZERO);
    }
    let max_qty = max_base.floor_div_step(qty_step);
    let wanted = qty.0.to_f64().unwrap_or(0.0);
    let q = wanted.min(max_qty);
    Qty(Decimal::from_f64_retain(q).unwrap_or(Decimal::ZERO))
}

// küçük helper: floor step
trait FloorStep {
    fn floor_div_step(self, step: f64) -> f64;
}
impl FloorStep for f64 {
    fn floor_div_step(self, step: f64) -> f64 {
        if step <= 0.0 {
            return self;
        }
        (self / step).floor() * step
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let cfg = load_cfg()?;
    if let Some(port) = cfg.metrics_port {
        monitor::init_prom(port);
    }

    info!(
        inv_cap = %cfg.risk.inv_cap,
        min_liq_gap_bps = cfg.risk.min_liq_gap_bps,
        dd_limit_bps = cfg.risk.dd_limit_bps,
        max_leverage = cfg.risk.max_leverage,
        quote_asset = %cfg.quote_asset,
        tif = %cfg.exec.tif,
        venue = %cfg.exec.venue,
        leverage = ?cfg.leverage,
        "configuration loaded"
    );

    if cfg.exec.venue.to_lowercase() != "binance" {
        warn!(venue = %cfg.exec.venue, "unsupported venue in config, defaulting to Binance");
    }

    // Strategy
    let dyn_cfg = DynMmCfg {
        a: cfg.strategy.a,
        b: cfg.strategy.b,
        base_size: Decimal::from_str_radix(&cfg.strategy.base_size, 10).unwrap(),
        inv_cap: Decimal::from_str_radix(
            cfg.strategy.inv_cap.as_deref().unwrap_or(&cfg.risk.inv_cap),
            10,
        )
        .unwrap(),
    };
    let mode_lower = cfg.mode.to_lowercase();
    let strategy_name = cfg.strategy.r#type.clone();
    let build_strategy = |symbol: &str| -> Box<dyn Strategy> {
        match strategy_name.as_str() {
            "dyn_mm" => Box::new(DynMm::from(dyn_cfg.clone())),
            other => {
                warn!(symbol = %symbol, strategy = %other, "unknown strategy type, defaulting dyn_mm");
                Box::new(DynMm::from(dyn_cfg.clone()))
            }
        }
    };

    // Venue seçimi
    let tif = tif_from_cfg(&cfg.exec.tif);
    let client = reqwest::Client::builder().build()?;
    let common = BinanceCommon {
        client,
        api_key: cfg.binance.api_key.clone(),
        secret_key: cfg.binance.secret_key.clone(),
        recv_window_ms: cfg.binance.recv_window_ms,
    };

    enum V {
        Spot(BinanceSpot),
        Fut(BinanceFutures),
    }
    let venue = match cfg.mode.to_lowercase().as_str() {
        "spot" => V::Spot(BinanceSpot {
            base: cfg.binance.spot_base.clone(),
            common: common.clone(),
            price_tick: cfg.price_tick,
            qty_step: cfg.qty_step,
        }),
        _ => V::Fut(BinanceFutures {
            base: cfg.binance.futures_base.clone(),
            common: common.clone(),
            price_tick: cfg.price_tick,
            qty_step: cfg.qty_step,
        }),
    };

    let metadata = match &venue {
        V::Spot(v) => v.symbol_metadata().await?,
        V::Fut(v) => v.symbol_metadata().await?,
    };

    let mut requested: Vec<String> = cfg.symbols.clone();
    if let Some(sym) = cfg.symbol.clone() {
        requested.push(sym);
    }

    let mut normalized = Vec::new();
    for sym in requested {
        let s = sym.trim().to_uppercase();
        if s.is_empty() {
            continue;
        }
        if !normalized.iter().any(|existing: &String| existing == &s) {
            normalized.push(s);
        }
    }

    let mut selected: Vec<SymbolMeta> = Vec::new();
    for sym in &normalized {
        if let Some(meta) = metadata.iter().find(|m| &m.symbol == sym) {
            if !meta.quote_asset.eq_ignore_ascii_case(&cfg.quote_asset) {
                warn!(
                    symbol = %sym,
                    quote_asset = %meta.quote_asset,
                    required_quote = %cfg.quote_asset,
                    "skipping configured symbol that is not quoted in required asset"
                );
                continue;
            }
            if let Some(status) = meta.status.as_deref() {
                if status != "TRADING" {
                    warn!(symbol = %sym, status, "skipping configured symbol that is not trading");
                    continue;
                }
            }
            if mode_lower == "futures" {
                match meta.contract_type.as_deref() {
                    Some("PERPETUAL") => {}
                    Some(other) => {
                        warn!(symbol = %sym, contract_type = %other, "skipping non-perpetual futures symbol");
                        continue;
                    }
                    None => {
                        warn!(symbol = %sym, "skipping futures symbol with missing contract type metadata");
                        continue;
                    }
                }
            }
            selected.push(meta.clone());
        } else {
            warn!(symbol = %sym, "configured symbol not found on venue");
        }
    }

    if selected.is_empty() && cfg.auto_discover_quote {
        selected = metadata
            .iter()
            .filter(|m| {
                m.quote_asset.eq_ignore_ascii_case(&cfg.quote_asset)
                    && m.status.as_deref().map(|s| s == "TRADING").unwrap_or(true)
                    && (mode_lower != "futures"
                        || m.contract_type
                            .as_deref()
                            .map(|ct| ct == "PERPETUAL")
                            .unwrap_or(false))
            })
            .cloned()
            .collect();
        selected.sort_by(|a, b| a.symbol.cmp(&b.symbol));
        info!(
            count = selected.len(),
            quote_asset = %cfg.quote_asset,
            "auto-discovered symbols for quote asset"
        );
    }

    if selected.is_empty() {
        return Err(anyhow!(
            "no eligible symbols resolved for required quote asset"
        ));
    }

    let mut states: Vec<SymbolState> = Vec::new();
    let mut symbol_index: HashMap<String, usize> = HashMap::new();
    for meta in selected {
        info!(
            symbol = %meta.symbol,
            base_asset = %meta.base_asset,
            quote_asset = %meta.quote_asset,
            mode = %cfg.mode,
            "bot initialized assets"
        );
        let strategy = build_strategy(&meta.symbol);
        let idx = states.len();
        symbol_index.insert(meta.symbol.clone(), idx);
        states.push(SymbolState {
            meta,
            inv: Qty(Decimal::ZERO),
            strategy,
            active_orders: HashMap::new(),
            pnl_history: Vec::new(),
        });
    }

    let symbol_list: Vec<String> = states.iter().map(|s| s.meta.symbol.clone()).collect();
    info!(symbols = ?symbol_list, mode = %cfg.mode, "bot started with real Binance venue");

    let risk_limits = RiskLimits {
        inv_cap: Qty(Decimal::from_str_radix(&cfg.risk.inv_cap, 10)?),
        min_liq_gap_bps: cfg.risk.min_liq_gap_bps,
        dd_limit_bps: cfg.risk.dd_limit_bps,
        max_leverage: cfg.risk.max_leverage,
    };

    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    if cfg.websocket.enabled {
        let client = common.client.clone();
        let api_key = cfg.binance.api_key.clone();
        let spot_base = cfg.binance.spot_base.clone();
        let futures_base = cfg.binance.futures_base.clone();
        let reconnect_delay = Duration::from_millis(cfg.websocket.reconnect_delay_ms);
        let tx = event_tx.clone();
        let kind = match &venue {
            V::Spot(_) => UserStreamKind::Spot,
            V::Fut(_) => UserStreamKind::Futures,
        };
        info!(
            reconnect_delay_ms = cfg.websocket.reconnect_delay_ms,
            ping_interval_ms = cfg.websocket.ping_interval_ms,
            ?kind,
            "launching user data stream task"
        );
        tokio::spawn(async move {
            let base = match kind {
                UserStreamKind::Spot => spot_base,
                UserStreamKind::Futures => futures_base,
            };
            loop {
                match UserDataStream::connect(client.clone(), &base, &api_key, kind).await {
                    Ok(mut stream) => {
                        info!(?kind, "connected to Binance user data stream");
                        while let Ok(event) = stream.next_event().await {
                            if tx.send(event).is_err() {
                                break;
                            }
                        }
                        warn!("user data stream reader exited, retrying");
                    }
                    Err(err) => {
                        warn!(?err, "failed to connect user data stream");
                    }
                }
                tokio::time::sleep(reconnect_delay).await;
            }
        });
    }

    struct Caps {
        buy_notional: f64,
        sell_notional: f64,
        sell_base: Option<f64>,
    }

    let tick_ms = max(100, cfg.exec.cancel_replace_interval_ms);
    let min_usd_per_order = cfg.min_usd_per_order.unwrap_or(0.0);
    let mut interval = tokio::time::interval(Duration::from_millis(tick_ms));
    loop {
        interval.tick().await;

        while let Ok(event) = event_rx.try_recv() {
            match event {
                UserEvent::OrderFill {
                    symbol,
                    order_id,
                    side,
                    qty,
                    ..
                } => {
                    if let Some(idx) = symbol_index.get(&symbol) {
                        let state = &mut states[*idx];
                        let mut inv = state.inv.0;
                        if side == Side::Buy {
                            inv += qty.0;
                        } else {
                            inv -= qty.0;
                        }
                        state.inv = Qty(inv);
                        state.active_orders.remove(&order_id);
                    }
                }
                UserEvent::OrderCanceled { symbol, order_id } => {
                    if let Some(idx) = symbol_index.get(&symbol) {
                        let state = &mut states[*idx];
                        state.active_orders.remove(&order_id);
                    }
                }
                _ => {}
            }
        }

        for state in states.iter_mut() {
            let symbol = state.meta.symbol.clone();
            let base_asset = state.meta.base_asset.clone();
            let quote_asset = state.meta.quote_asset.clone();

            if !state.active_orders.is_empty() {
                let existing_orders: Vec<OrderInfo> =
                    state.active_orders.values().cloned().collect();
                state.active_orders.clear();
                for order in existing_orders {
                    let age_ms = order.created_at.elapsed().as_millis() as u64;
                    let stale = age_ms > cfg.exec.max_order_age_ms;
                    if stale {
                        warn!(
                            %symbol,
                            order_id = %order.order_id,
                            side = ?order.side,
                            price = ?order.price,
                            qty = ?order.qty,
                            age_ms,
                            max_age = cfg.exec.max_order_age_ms,
                            "canceling stale order"
                        );
                    } else {
                        info!(
                            %symbol,
                            order_id = %order.order_id,
                            side = ?order.side,
                            price = ?order.price,
                            qty = ?order.qty,
                            age_ms,
                            "canceling active order before refresh"
                        );
                    }
                    match &venue {
                        V::Spot(v) => {
                            if let Err(err) = v.cancel(&order.order_id, &symbol).await {
                                warn!(%symbol, order_id = %order.order_id, ?err, "failed to cancel existing spot order");
                            }
                        }
                        V::Fut(v) => {
                            if let Err(err) = v.cancel(&order.order_id, &symbol).await {
                                warn!(%symbol, order_id = %order.order_id, ?err, "failed to cancel existing futures order");
                            }
                        }
                    }
                }
            }

            let (bid, ask) = match &venue {
                V::Spot(v) => v.best_prices(&symbol).await?,
                V::Fut(v) => v.best_prices(&symbol).await?,
            };
            info!(%symbol, ?bid, ?ask, "fetched best prices");
            let ob = OrderBook {
                best_bid: Some(BookLevel {
                    px: bid,
                    qty: Qty(Decimal::from(1)),
                }),
                best_ask: Some(BookLevel {
                    px: ask,
                    qty: Qty(Decimal::from(1)),
                }),
            };

            let pos = match &venue {
                V::Spot(v) => v.get_position(&symbol).await?,
                V::Fut(v) => v.get_position(&symbol).await?,
            };
            state.inv = pos.qty;

            let mark_px = match &venue {
                V::Spot(v) => v.mark_price(&symbol).await?,
                V::Fut(v) => v.mark_price(&symbol).await?,
            };

            let liq_gap_bps = if let Some(liq_px) = pos.liq_px {
                let mark = mark_px.0.to_f64().unwrap_or(0.0);
                let liq = liq_px.0.to_f64().unwrap_or(0.0);
                if mark > 0.0 {
                    ((mark - liq).abs() / mark) * 10_000.0
                } else {
                    9_999.0
                }
            } else {
                9_999.0
            };

            let dd_bps = compute_drawdown_bps(&state.pnl_history);
            let risk_action = risk::check_risk(&pos, state.inv, liq_gap_bps, dd_bps, &risk_limits);

            if matches!(risk_action, RiskAction::Halt) {
                warn!(%symbol, "risk halt triggered, cancelling and flattening");
                match &venue {
                    V::Spot(v) => {
                        v.cancel_all(&symbol).await?;
                        v.close_position(&symbol).await?;
                    }
                    V::Fut(v) => {
                        v.cancel_all(&symbol).await?;
                        v.close_position(&symbol).await?;
                    }
                }
                continue;
            }

            let ctx = Context {
                ob,
                sigma: 0.5,
                inv: state.inv,
                liq_gap_bps,
                funding_rate: None,
                next_funding_time: None,
            };
            let mut quotes = state.strategy.on_tick(&ctx);
            info!(%symbol, ?quotes, ?risk_action, "strategy produced raw quotes");

            match risk_action {
                RiskAction::Reduce => {
                    quotes.bid = quotes.bid.map(|(px, qty)| {
                        let reduced = qty.0 / Decimal::from(2u32);
                        let reduced = if reduced > Decimal::ZERO {
                            reduced
                        } else {
                            Decimal::ZERO
                        };
                        (px, Qty(reduced))
                    });
                    quotes.ask = quotes.ask.map(|(px, qty)| {
                        let reduced = qty.0 / Decimal::from(2u32);
                        let reduced = if reduced > Decimal::ZERO {
                            reduced
                        } else {
                            Decimal::ZERO
                        };
                        (px, Qty(reduced))
                    });
                }
                RiskAction::Widen => {
                    let widen = Decimal::from_f64_retain(0.001).unwrap_or(Decimal::ZERO);
                    quotes.bid = quotes
                        .bid
                        .map(|(px, qty)| (Px(px.0 * (Decimal::ONE - widen)), qty));
                    quotes.ask = quotes
                        .ask
                        .map(|(px, qty)| (Px(px.0 * (Decimal::ONE + widen)), qty));
                }
                RiskAction::Ok => {}
                RiskAction::Halt => {}
            }

            let caps = match &venue {
                V::Spot(v) => {
                    let quote_free = v.asset_free(&quote_asset).await?.to_f64().unwrap_or(0.0);
                    let base_free = v.asset_free(&base_asset).await?.to_f64().unwrap_or(0.0);
                    Caps {
                        buy_notional: cfg.max_usd_per_order.min(quote_free),
                        sell_notional: cfg.max_usd_per_order,
                        sell_base: Some(base_free),
                    }
                }
                V::Fut(v) => {
                    let avail = v
                        .available_balance(&quote_asset)
                        .await?
                        .to_f64()
                        .unwrap_or(0.0);
                    let risk_max_leverage = cfg.risk.max_leverage.max(1);
                    let requested_leverage = cfg.leverage.unwrap_or(risk_max_leverage);
                    let effective_leverage =
                        requested_leverage.max(1).min(risk_max_leverage) as f64;
                    let cap = cfg.max_usd_per_order.min(avail * effective_leverage);
                    Caps {
                        buy_notional: cap,
                        sell_notional: cap,
                        sell_base: None,
                    }
                }
            };
            info!(
                %symbol,
                buy_notional = caps.buy_notional,
                sell_notional = caps.sell_notional,
                sell_base = ?caps.sell_base,
                "calculated order caps"
            );

            let qty_step_dec = Decimal::from_f64_retain(cfg.qty_step).unwrap_or(Decimal::ZERO);

            if let Some((px, q)) = quotes.bid {
                if px.0 <= Decimal::ZERO {
                    warn!(%symbol, ?px, "dropping bid quote with non-positive price");
                    quotes.bid = None;
                } else {
                    let nq = clamp_qty_by_usd(q, px, caps.buy_notional, cfg.qty_step);
                    let quantized_to_zero = qty_step_dec > Decimal::ZERO
                        && nq.0 < qty_step_dec
                        && nq.0 != Decimal::ZERO;
                    let notional = px.0.to_f64().unwrap_or(0.0) * nq.0.to_f64().unwrap_or(0.0);
                    if nq.0 == Decimal::ZERO
                        || quantized_to_zero
                        || (min_usd_per_order > 0.0 && notional < min_usd_per_order)
                    {
                        warn!(
                            %symbol,
                            ?px,
                            original_qty = ?q,
                            qty_step = cfg.qty_step,
                            quantized_to_zero,
                            notional,
                            min_usd_per_order,
                            "dropping bid quote after clamps produced zero quantity"
                        );
                        quotes.bid = None;
                    } else {
                        quotes.bid = Some((px, nq));
                        info!(%symbol, ?px, original_qty = ?q, clamped_qty = ?nq, "prepared bid quote");
                    }
                }
            } else {
                info!(%symbol, "no bid quote generated for this tick");
            }
            if let Some((px, q)) = quotes.ask {
                if px.0 <= Decimal::ZERO {
                    warn!(%symbol, ?px, "dropping ask quote with non-positive price");
                    quotes.ask = None;
                } else {
                    let mut nq = clamp_qty_by_usd(q, px, caps.sell_notional, cfg.qty_step);
                    if let Some(max_base) = caps.sell_base {
                        nq = clamp_qty_by_base(nq, max_base, cfg.qty_step);
                    }
                    let quantized_to_zero = qty_step_dec > Decimal::ZERO
                        && nq.0 < qty_step_dec
                        && nq.0 != Decimal::ZERO;
                    let notional = px.0.to_f64().unwrap_or(0.0) * nq.0.to_f64().unwrap_or(0.0);
                    if nq.0 == Decimal::ZERO
                        || quantized_to_zero
                        || (min_usd_per_order > 0.0 && notional < min_usd_per_order)
                    {
                        warn!(
                            %symbol,
                            ?px,
                            original_qty = ?q,
                            sell_base = ?caps.sell_base,
                            qty_step = cfg.qty_step,
                            quantized_to_zero,
                            notional,
                            min_usd_per_order,
                            "dropping ask quote after clamps produced zero quantity"
                        );
                        quotes.ask = None;
                    } else {
                        quotes.ask = Some((px, nq));
                        info!(%symbol, ?px, original_qty = ?q, clamped_qty = ?nq, "prepared ask quote");
                    }
                }
            } else {
                info!(%symbol, "no ask quote generated for this tick");
            }

            match &venue {
                V::Spot(v) => {
                    if let Some((px, qty)) = quotes.bid {
                        info!(%symbol, ?px, ?qty, tif = ?tif, "placing spot bid order");
                        match v.place_limit(&symbol, Side::Buy, px, qty, tif).await {
                            Ok(order_id) => {
                                let info = OrderInfo {
                                    order_id: order_id.clone(),
                                    side: Side::Buy,
                                    price: px,
                                    qty,
                                    created_at: Instant::now(),
                                };
                                state.active_orders.insert(order_id, info);
                            }
                            Err(err) => {
                                warn!(%symbol, ?px, ?qty, tif = ?tif, ?err, "failed to place spot bid order");
                            }
                        }
                    }
                    if let Some((px, qty)) = quotes.ask {
                        info!(%symbol, ?px, ?qty, tif = ?tif, "placing spot ask order");
                        match v.place_limit(&symbol, Side::Sell, px, qty, tif).await {
                            Ok(order_id) => {
                                let info = OrderInfo {
                                    order_id: order_id.clone(),
                                    side: Side::Sell,
                                    price: px,
                                    qty,
                                    created_at: Instant::now(),
                                };
                                state.active_orders.insert(order_id, info);
                            }
                            Err(err) => {
                                warn!(%symbol, ?px, ?qty, tif = ?tif, ?err, "failed to place spot ask order");
                            }
                        }
                    }
                }
                V::Fut(v) => {
                    if let Some((px, qty)) = quotes.bid {
                        info!(%symbol, ?px, ?qty, tif = ?tif, "placing futures bid order");
                        match v.place_limit(&symbol, Side::Buy, px, qty, tif).await {
                            Ok(order_id) => {
                                let info = OrderInfo {
                                    order_id: order_id.clone(),
                                    side: Side::Buy,
                                    price: px,
                                    qty,
                                    created_at: Instant::now(),
                                };
                                state.active_orders.insert(order_id, info);
                            }
                            Err(err) => {
                                warn!(%symbol, ?px, ?qty, tif = ?tif, ?err, "failed to place futures bid order");
                            }
                        }
                    }
                    if let Some((px, qty)) = quotes.ask {
                        info!(%symbol, ?px, ?qty, tif = ?tif, "placing futures ask order");
                        match v.place_limit(&symbol, Side::Sell, px, qty, tif).await {
                            Ok(order_id) => {
                                let info = OrderInfo {
                                    order_id: order_id.clone(),
                                    side: Side::Sell,
                                    price: px,
                                    qty,
                                    created_at: Instant::now(),
                                };
                                state.active_orders.insert(order_id, info);
                            }
                            Err(err) => {
                                warn!(%symbol, ?px, ?qty, tif = ?tif, ?err, "failed to place futures ask order");
                            }
                        }
                    }
                }
            }
        }
    }
}
