use anyhow::Result;
use bot_core::types::*;
use exec::{Venue};
use exec::binance::{BinanceCommon, BinanceSpot, BinanceFutures};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::Deserialize;
use strategy::{DynMm, DynMmCfg, Strategy, Context};
use tracing::{info, warn};

#[derive(Debug, Deserialize)]
struct RiskCfg { inv_cap: String, min_liq_gap_bps: f64, dd_limit_bps: i64, max_leverage: u32 }

#[derive(Debug, Deserialize)]
struct StratCfg { r#type: String, a: f64, b: f64, base_size: String, #[serde(default)] inv_cap: Option<String> }

#[derive(Debug, Deserialize)]
struct ExecCfg { tif: String, venue: String }

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
    symbol: String,          // e.g. "BTCUSDT"
    mode: String,            // "spot" | "futures"
    metrics_port: Option<u16>,
    max_usd_per_order: f64,
    leverage: Option<u32>,   // only futures (not set here programmatically)
    price_tick: f64,
    qty_step: f64,
    binance: BinanceCfg,
    risk: RiskCfg,
    strategy: StratCfg,
    exec: ExecCfg,
}

fn load_cfg() -> Result<AppCfg> {
    let args: Vec<String> = std::env::args().collect();
    let path = args.windows(2).find_map(|w| if w[0] == "--config" { Some(w[1].clone()) } else { None })
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
    if p <= 0.0 || max_usd <= 0.0 { return Qty(Decimal::ZERO); }
    let max_qty = (max_usd / p).floor_div_step(qty_step);
    let wanted = qty.0.to_f64().unwrap_or(0.0);
    let q = wanted.min(max_qty);
    Qty(Decimal::from_f64_retain(q).unwrap_or(Decimal::ZERO))
}

fn clamp_qty_by_base(qty: Qty, max_base: f64, qty_step: f64) -> Qty {
    if max_base <= 0.0 { return Qty(Decimal::ZERO); }
    let max_qty = max_base.floor_div_step(qty_step);
    let wanted = qty.0.to_f64().unwrap_or(0.0);
    let q = wanted.min(max_qty);
    Qty(Decimal::from_f64_retain(q).unwrap_or(Decimal::ZERO))
}

// küçük helper: floor step
trait FloorStep { fn floor_div_step(self, step: f64) -> f64; }
impl FloorStep for f64 {
    fn floor_div_step(self, step: f64) -> f64 {
        if step <= 0.0 { return self; }
        (self / step).floor() * step
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let cfg = load_cfg()?;
    if let Some(port) = cfg.metrics_port { monitor::init_prom(port); }

    // Strategy
    let dyn_cfg = DynMmCfg {
        a: cfg.strategy.a,
        b: cfg.strategy.b,
        base_size: Decimal::from_str_radix(&cfg.strategy.base_size, 10).unwrap(),
        inv_cap: Decimal::from_str_radix(cfg.strategy.inv_cap.as_deref().unwrap_or(&cfg.risk.inv_cap), 10).unwrap()
    };
    let mut strat: Box<dyn Strategy> = match cfg.strategy.r#type.as_str() {
        "dyn_mm" => Box::new(DynMm::from(dyn_cfg)),
        other => { warn!(%other, "unknown strategy type, defaulting dyn_mm"); Box::new(DynMm::from(dyn_cfg)) }
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

    enum V { Spot(BinanceSpot), Fut(BinanceFutures) }
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
        })
    };

    let (base_asset, quote_asset) = match &venue {
        V::Spot(v) => v.symbol_assets(&cfg.symbol).await?,
        V::Fut(v) => v.symbol_assets(&cfg.symbol).await?,
    };
    info!(symbol = %cfg.symbol, base_asset = %base_asset, quote_asset = %quote_asset, mode = %cfg.mode, "bot initialized assets");

    let inv = Qty(Decimal::ZERO);
    let symbol = cfg.symbol.clone();
    info!(%symbol, mode = %cfg.mode, "bot started with real Binance venue");

    let mut interval = tokio::time::interval(std::time::Duration::from_millis(1200));
    loop {
        interval.tick().await;

        // Gerçek en iyi fiyatları çek
        let (bid, ask) = match &venue {
            V::Spot(v) => v.best_prices(&symbol).await?,
            V::Fut(v)  => v.best_prices(&symbol).await?,
        };
        let ob = OrderBook {
            best_bid: Some(BookLevel { px: bid, qty: Qty(Decimal::from(1)) }),
            best_ask: Some(BookLevel { px: ask, qty: Qty(Decimal::from(1)) }),
        };

        let ctx = Context { ob, sigma: 0.5, inv, liq_gap_bps: 800.0 };
        let mut quotes = strat.on_tick(&ctx);

        // max_usd_per_order kuralı + hesap bakiyesi
        struct Caps { buy_notional: f64, sell_notional: f64, sell_base: Option<f64> }
        let caps = match &venue {
            V::Spot(v) => {
                let quote_free = v.asset_free(&quote_asset).await?.to_f64().unwrap_or(0.0);
                let base_free = v.asset_free(&base_asset).await?.to_f64().unwrap_or(0.0);
                Caps { buy_notional: cfg.max_usd_per_order.min(quote_free), sell_notional: cfg.max_usd_per_order, sell_base: Some(base_free) }
            }
            V::Fut(v) => {
                let avail = v.available_balance(&quote_asset).await?.to_f64().unwrap_or(0.0);
                let cap = cfg.max_usd_per_order.min(avail);
                Caps { buy_notional: cap, sell_notional: cap, sell_base: None }
            }
        };

        if let Some((px, q)) = quotes.bid {
            let nq = clamp_qty_by_usd(q, px, caps.buy_notional, cfg.qty_step);
            quotes.bid = Some((px, nq));
        }
        if let Some((px, q)) = quotes.ask {
            let mut nq = clamp_qty_by_usd(q, px, caps.sell_notional, cfg.qty_step);
            if let Some(max_base) = caps.sell_base { nq = clamp_qty_by_base(nq, max_base, cfg.qty_step); }
            quotes.ask = Some((px, nq));
        }

        // emirleri gönder
        match &venue {
            V::Spot(v) => {
                if let Some((px, qty)) = quotes.bid { let _ = v.place_limit(&symbol, Side::Buy,  px, qty, tif).await; }
                if let Some((px, qty)) = quotes.ask { let _ = v.place_limit(&symbol, Side::Sell, px, qty, tif).await; }
            }
            V::Fut(v) => {
                if let Some((px, qty)) = quotes.bid { let _ = v.place_limit(&symbol, Side::Buy,  px, qty, tif).await; }
                if let Some((px, qty)) = quotes.ask { let _ = v.place_limit(&symbol, Side::Sell, px, qty, tif).await; }
            }
        }
    }
}
