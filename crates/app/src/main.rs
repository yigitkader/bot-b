use anyhow::{anyhow, Result};
use bot_core::types::*;
use exec::binance::{BinanceCommon, BinanceFutures, BinanceSpot, SymbolMeta};
use exec::Venue;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::Deserialize;
use strategy::{Context, DynMm, DynMmCfg, Strategy};
use tracing::{info, warn};

struct SymbolState {
    meta: SymbolMeta,
    inv: Qty,
    strategy: Box<dyn Strategy>,
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
}

#[derive(Debug, Deserialize)]
struct BinanceCfg {
    spot_base: String,
    futures_base: String,
    api_key: String,
    secret_key: String,
    recv_window_ms: u64,
}

#[derive(Debug, Deserialize)]
struct AppCfg {
    #[serde(default)]
    symbol: Option<String>,
    #[serde(default)]
    symbols: Vec<String>,
    #[serde(default)]
    auto_discover_usdt: bool,
    mode: String,
    metrics_port: Option<u16>,
    max_usd_per_order: f64,
    leverage: Option<u32>,
    price_tick: f64,
    qty_step: f64,
    binance: BinanceCfg,
    risk: RiskCfg,
    strategy: StratCfg,
    exec: ExecCfg,
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
            if !meta.quote_asset.eq_ignore_ascii_case("USDT") {
                warn!(symbol = %sym, quote_asset = %meta.quote_asset, "skipping configured symbol that is not quoted in USDT");
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

    if selected.is_empty() && cfg.auto_discover_usdt {
        selected = metadata
            .iter()
            .filter(|m| {
                m.quote_asset.eq_ignore_ascii_case("USDT")
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
        info!(count = selected.len(), "auto-discovered USDT symbols");
    }

    if selected.is_empty() {
        return Err(anyhow!(
            "no eligible USDT symbols resolved from configuration"
        ));
    }

    let mut states: Vec<SymbolState> = Vec::new();
    for meta in selected {
        info!(
            symbol = %meta.symbol,
            base_asset = %meta.base_asset,
            quote_asset = %meta.quote_asset,
            mode = %cfg.mode,
            "bot initialized assets"
        );
        let strategy = build_strategy(&meta.symbol);
        states.push(SymbolState {
            meta,
            inv: Qty(Decimal::ZERO),
            strategy,
        });
    }

    let symbol_list: Vec<String> = states.iter().map(|s| s.meta.symbol.clone()).collect();
    info!(symbols = ?symbol_list, mode = %cfg.mode, "bot started with real Binance venue");

    struct Caps {
        buy_notional: f64,
        sell_notional: f64,
        sell_base: Option<f64>,
    }

    let mut interval = tokio::time::interval(std::time::Duration::from_millis(1200));
    loop {
        interval.tick().await;

        for state in states.iter_mut() {
            let symbol = state.meta.symbol.clone();
            let base_asset = state.meta.base_asset.clone();
            let quote_asset = state.meta.quote_asset.clone();

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

            let ctx = Context {
                ob,
                sigma: 0.5,
                inv: state.inv,
                liq_gap_bps: cfg.risk.min_liq_gap_bps.max(0.0),
            };
            let mut quotes = state.strategy.on_tick(&ctx);
            info!(%symbol, ?quotes, "strategy produced raw quotes");

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

            if let Some((px, q)) = quotes.bid {
                let nq = clamp_qty_by_usd(q, px, caps.buy_notional, cfg.qty_step);
                quotes.bid = Some((px, nq));
                info!(%symbol, ?px, original_qty = ?q, clamped_qty = ?nq, "prepared bid quote");
            } else {
                info!(%symbol, "no bid quote generated for this tick");
            }
            if let Some((px, q)) = quotes.ask {
                let mut nq = clamp_qty_by_usd(q, px, caps.sell_notional, cfg.qty_step);
                if let Some(max_base) = caps.sell_base {
                    nq = clamp_qty_by_base(nq, max_base, cfg.qty_step);
                }
                quotes.ask = Some((px, nq));
                info!(%symbol, ?px, original_qty = ?q, clamped_qty = ?nq, "prepared ask quote");
            } else {
                info!(%symbol, "no ask quote generated for this tick");
            }

            match &venue {
                V::Spot(v) => {
                    if let Some((px, qty)) = quotes.bid {
                        info!(%symbol, ?px, ?qty, tif = ?tif, "placing spot bid order");
                        if let Err(err) = v.place_limit(&symbol, Side::Buy, px, qty, tif).await {
                            warn!(%symbol, ?px, ?qty, tif = ?tif, ?err, "failed to place spot bid order");
                        }
                    }
                    if let Some((px, qty)) = quotes.ask {
                        info!(%symbol, ?px, ?qty, tif = ?tif, "placing spot ask order");
                        if let Err(err) = v.place_limit(&symbol, Side::Sell, px, qty, tif).await {
                            warn!(%symbol, ?px, ?qty, tif = ?tif, ?err, "failed to place spot ask order");
                        }
                    }
                }
                V::Fut(v) => {
                    if let Some((px, qty)) = quotes.bid {
                        info!(%symbol, ?px, ?qty, tif = ?tif, "placing futures bid order");
                        if let Err(err) = v.place_limit(&symbol, Side::Buy, px, qty, tif).await {
                            warn!(%symbol, ?px, ?qty, tif = ?tif, ?err, "failed to place futures bid order");
                        }
                    }
                    if let Some((px, qty)) = quotes.ask {
                        info!(%symbol, ?px, ?qty, tif = ?tif, "placing futures ask order");
                        if let Err(err) = v.place_limit(&symbol, Side::Sell, px, qty, tif).await {
                            warn!(%symbol, ?px, ?qty, tif = ?tif, ?err, "failed to place futures ask order");
                        }
                    }
                }
            }
        }
    }
}
