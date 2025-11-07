//location: /crates/app/src/main.rs

use anyhow::{anyhow, Result};
use bot_core::types::*;
use data::binance_ws::{UserDataStream, UserEvent, UserStreamKind};
use exec::binance::{BinanceCommon, BinanceFutures, BinanceSpot, SymbolMeta};
use exec::{decimal_places, Venue};
use risk::{RiskAction, RiskLimits};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::Deserialize;
use strategy::{Context, DynMm, DynMmCfg, Strategy};
use tokio::sync::mpsc;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use std::cmp::max;
use std::collections::HashMap;
use std::time::{Duration, Instant};

struct SymbolState {
    meta: SymbolMeta,
    inv: Qty,
    strategy: Box<dyn Strategy>,
    active_orders: HashMap<String, OrderInfo>,
    pnl_history: Vec<Decimal>,
    // --- eklendi: min notional öğrenme ve devre dışı bırakma ---
    min_notional_req: Option<f64>, // borsa min notional (quote cinsinden)
    disabled: bool,                // min_notional > max_usd_per_order => kalıcı disable
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

const PNL_HISTORY_MAX_LEN: usize = 1_024;

fn record_pnl_snapshot(history: &mut Vec<Decimal>, pos: &Position, mark_px: Px) {
    let pnl = (mark_px.0 - pos.entry.0) * pos.qty.0;
    let mut equity = Decimal::ONE + pnl;
    if equity <= Decimal::ZERO {
        // Keep the history strictly positive so drawdown math remains stable.
        equity = Decimal::new(1, 4);
    }
    history.push(equity);
    if history.len() > PNL_HISTORY_MAX_LEN {
        let excess = history.len() - PNL_HISTORY_MAX_LEN;
        history.drain(0..excess);
    }
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

/// USD-family stablecoin kontrolü (quote eşleştirme için)
/// Bot USDT, USDC, USD ve diğer USD-stablecoin'leri destekler
/// Her sembol kendi quote asset'ini kullanır (ADAUSDT → USDT, ADAUSDC → USDC)
fn is_usd_stable(asset: &str) -> bool {
    matches!(
        asset.to_uppercase().as_str(),
        "USD" | "USDT" | "USDC" | "BUSD" | "TUSD" | "FDUSD" | "USDA"
    )
}

#[tokio::main]
async fn main() -> Result<()> {
    // ---- LOG INIT ----
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .compact()
        .init();

    let cfg = load_cfg()?;
    if cfg.price_tick <= 0.0 {
        return Err(anyhow!("price_tick must be positive"));
    }
    if cfg.qty_step <= 0.0 {
        return Err(anyhow!("qty_step must be positive"));
    }
    if cfg.max_usd_per_order <= 0.0 {
        return Err(anyhow!("max_usd_per_order must be positive"));
    }
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
        base_size: Decimal::from_str_radix(&cfg.strategy.base_size, 10)
            .map_err(|e| anyhow!("invalid strategy.base_size: {}", e))?,
        inv_cap: Decimal::from_str_radix(
            cfg.strategy.inv_cap.as_deref().unwrap_or(&cfg.risk.inv_cap),
            10,
        )
        .map_err(|e| anyhow!("invalid strategy.inv_cap or risk.inv_cap: {}", e))?,
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
    let price_tick_dec = Decimal::from_f64_retain(cfg.price_tick).unwrap_or(Decimal::ZERO);
    let qty_step_dec = Decimal::from_f64_retain(cfg.qty_step).unwrap_or(Decimal::ZERO);
    let price_precision = decimal_places(price_tick_dec);
    let qty_precision = decimal_places(qty_step_dec);
    let venue = match cfg.mode.to_lowercase().as_str() {
        "spot" => V::Spot(BinanceSpot {
            base: cfg.binance.spot_base.clone(),
            common: common.clone(),
            price_tick: price_tick_dec,
            qty_step: qty_step_dec,
            price_precision,
            qty_precision,
        }),
        _ => V::Fut(BinanceFutures {
            base: cfg.binance.futures_base.clone(),
            common: common.clone(),
            price_tick: price_tick_dec,
            qty_step: qty_step_dec,
            price_precision,
            qty_precision,
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
            // --- quote eşleşmesi ya birebir ya da USD-stable grubu uyumu ---
            let exact_quote = meta.quote_asset.eq_ignore_ascii_case(&cfg.quote_asset);
            let group_quote = is_usd_stable(&cfg.quote_asset) && is_usd_stable(&meta.quote_asset);
            if !(exact_quote || group_quote) {
                warn!(
                    symbol = %sym,
                    quote_asset = %meta.quote_asset,
                    required_quote = %cfg.quote_asset,
                    "skipping configured symbol that is not in required quote group"
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

            // --- opsiyonel: başlangıçta bakiye tabanlı ön eleme ---
            let have_min = cfg.min_usd_per_order.unwrap_or(0.0);
            if mode_lower == "futures" {
                if let V::Fut(vtmp) = &venue {
                    let avail = vtmp.available_balance(&meta.quote_asset).await?.to_f64().unwrap_or(0.0);
                    if avail < have_min {
                        warn!(
                            symbol = %sym,
                            quote = %meta.quote_asset,
                            avail,
                            min_needed = have_min,
                            "skipping symbol at discovery: zero/low quote balance for futures wallet"
                        );
                        continue;
                    }
                }
            } else {
                if let V::Spot(vtmp) = &venue {
                    let q_free = vtmp.asset_free(&meta.quote_asset).await?.to_f64().unwrap_or(0.0);
                    let b_free = vtmp.asset_free(&meta.base_asset).await?.to_f64().unwrap_or(0.0);
                    if q_free < have_min && b_free < cfg.qty_step {
                        warn!(
                            symbol = %sym,
                            quote = %meta.quote_asset,
                            q_free,
                            b_free,
                            "skipping symbol at discovery: no usable balances (spot)"
                        );
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
        // --- auto-discover: USD-stable grup kuralı ---
        let want_group = is_usd_stable(&cfg.quote_asset);
        let mut auto: Vec<SymbolMeta> = metadata
            .iter()
            .filter(|m| {
                let match_quote = if want_group {
                    is_usd_stable(&m.quote_asset)
                } else {
                    m.quote_asset.eq_ignore_ascii_case(&cfg.quote_asset)
                };
                match_quote
                    && m.status.as_deref().map(|s| s == "TRADING").unwrap_or(true)
                    && (mode_lower != "futures"
                    || m.contract_type.as_deref().map(|ct| ct == "PERPETUAL").unwrap_or(false))
            })
            .cloned()
            .collect();
        
        // AKILLI SEÇİM: Hangi USD-stable asset'te daha fazla para varsa onu önceliklendir
        if want_group && mode_lower == "futures" {
            if let V::Fut(vtmp) = &venue {
                // USDT ve USDC bakiyelerini kontrol et
                let usdt_bal = vtmp.available_balance("USDT").await.ok()
                    .and_then(|b| b.to_f64())
                    .unwrap_or(0.0);
                let usdc_bal = vtmp.available_balance("USDC").await.ok()
                    .and_then(|b| b.to_f64())
                    .unwrap_or(0.0);
                
                // Hangi asset'te daha fazla para varsa, o asset'in sembollerini önceliklendir
                if usdt_bal >= 1.0 && usdt_bal > usdc_bal {
                    // USDT öncelikli
                    auto.sort_by(|a, b| {
                        let a_usdt = a.quote_asset.eq_ignore_ascii_case("USDT");
                        let b_usdt = b.quote_asset.eq_ignore_ascii_case("USDT");
                        match (a_usdt, b_usdt) {
                            (true, false) => std::cmp::Ordering::Less, // USDT önce
                            (false, true) => std::cmp::Ordering::Greater, // USDT önce
                            _ => a.symbol.cmp(&b.symbol),
                        }
                    });
                    info!(
                        count = auto.len(),
                        quote_asset = %cfg.quote_asset,
                        usdt_balance = usdt_bal,
                        usdc_balance = usdc_bal,
                        "auto-discovered symbols: USDT prioritized (more balance)"
                    );
                } else if usdc_bal >= 1.0 && usdc_bal > usdt_bal {
                    // USDC öncelikli
                    auto.sort_by(|a, b| {
                        let a_usdc = a.quote_asset.eq_ignore_ascii_case("USDC");
                        let b_usdc = b.quote_asset.eq_ignore_ascii_case("USDC");
                        match (a_usdc, b_usdc) {
                            (true, false) => std::cmp::Ordering::Less, // USDC önce
                            (false, true) => std::cmp::Ordering::Greater, // USDC önce
                            _ => a.symbol.cmp(&b.symbol),
                        }
                    });
                    info!(
                        count = auto.len(),
                        quote_asset = %cfg.quote_asset,
                        usdt_balance = usdt_bal,
                        usdc_balance = usdc_bal,
                        "auto-discovered symbols: USDC prioritized (more balance)"
                    );
                } else {
                    // Eşit veya ikisi de yetersiz, alfabetik sırala
                    auto.sort_by(|a, b| a.symbol.cmp(&b.symbol));
                    info!(
                        count = auto.len(),
                        quote_asset = %cfg.quote_asset,
                        usdt_balance = usdt_bal,
                        usdc_balance = usdc_bal,
                        "auto-discovered symbols: equal balance or insufficient, alphabetical order"
                    );
                }
            } else {
                auto.sort_by(|a, b| a.symbol.cmp(&b.symbol));
                info!(
                    count = auto.len(),
                    quote_asset = %cfg.quote_asset,
                    grouped = want_group,
                    "auto-discovered symbols for quote asset (group-aware)"
                );
            }
        } else {
            auto.sort_by(|a, b| a.symbol.cmp(&b.symbol));
            info!(
                count = auto.len(),
                quote_asset = %cfg.quote_asset,
                grouped = want_group,
                "auto-discovered symbols for quote asset (group-aware)"
            );
        }
        selected = auto;
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
            min_notional_req: None,
            disabled: false,
        });
    }

    let symbol_list: Vec<String> = states.iter().map(|s| s.meta.symbol.clone()).collect();
    info!(symbols = ?symbol_list, mode = %cfg.mode, "bot started with real Binance venue");

    let risk_limits = RiskLimits {
        inv_cap: Qty(Decimal::from_str_radix(&cfg.risk.inv_cap, 10)
            .map_err(|e| anyhow!("invalid risk.inv_cap: {}", e))?),
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
        buy_notional: f64,      // tek emir için USD üst sınır (örn 100)
        sell_notional: f64,     // tek emir için USD üst sınır
        sell_base: Option<f64>, // SPOT için base miktar üst sınırı
        buy_total: f64,         // toplam kullanılabilir quote USD (örn 140)
        sell_total_base: f64,   // toplam satılabilir base (SPOT)
    }

    let tick_ms = max(100, cfg.exec.cancel_replace_interval_ms);
    let min_usd_per_order = cfg.min_usd_per_order.unwrap_or(0.0);
    let mut interval = tokio::time::interval_at(
        tokio::time::Instant::now(),
        Duration::from_millis(tick_ms),
    );
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

            // --- kalıcı devre dışı kontrolü ---
            if state.disabled {
                info!(%symbol, "skipping symbol permanently (min_notional > max_usd_per_order)");
                continue;
            }

            // --- ERKEN BAKİYE KONTROLÜ: Bakiye yoksa gereksiz işlem yapma ---
            // Best prices çekmeden önce bakiye kontrolü yap, gereksiz API çağrılarını önle
            let has_balance = match &venue {
                V::Spot(v) => {
                    // Spot için: quote veya base bakiyesi yeterli mi?
                    let q_free = match v.asset_free(&quote_asset).await {
                        Ok(q) => q.to_f64().unwrap_or(0.0),
                        Err(_) => 0.0,
                    };
                    // HIZLI KONTROL: 1 USD'den azsa işlem yapma
                    if q_free < 1.0 {
                        // Base bakiyesi kontrolü (yaklaşık kontrol, gerçek fiyat bilinmiyor)
                        match v.asset_free(&base_asset).await {
                            Ok(b) => {
                                let b_free = b.to_f64().unwrap_or(0.0);
                                // Base varsa devam et (daha sonra gerçek fiyatla kontrol edilecek)
                                b_free >= cfg.qty_step
                            }
                            Err(_) => false,
                        }
                    } else {
                        // Quote bakiyesi yeterliyse devam et
                        q_free >= min_usd_per_order
                    }
                }
                V::Fut(v) => {
                    match v.available_balance(&quote_asset).await {
                        Ok(a) => {
                            let avail = a.to_f64().unwrap_or(0.0);
                            // HIZLI KONTROL: 1 USD'den azsa işlem yapma (gereksiz API çağrılarını önle)
                            if avail < 1.0 {
                                false // Bakiye çok düşük, skip
                            } else {
                                // Leverage ile toplam kullanılabilir miktar
                                let effective_leverage = cfg.leverage.unwrap_or(cfg.risk.max_leverage).max(1) as f64;
                                let total = avail * effective_leverage;
                                total >= min_usd_per_order
                            }
                        }
                        Err(_) => false,
                    }
                }
            };
            
            if !has_balance {
                // Bakiye yok, bu tick'i atla (best_prices, strateji, vs. çalıştırma)
                // Log sadece ilk birkaç kez veya periyodik olarak (spam önlemek için)
                continue;
            }

            // aktif emirleri iptal/temizle
            // Not: WebSocket event'leri bu noktada hala gelebilir, bu normaldir
            // çünkü event'ler clear() öncesi emirler için olabilir
            if !state.active_orders.is_empty() {
                let existing_orders: Vec<OrderInfo> =
                    state.active_orders.values().cloned().collect();
                // Clear'i iptal işlemlerinden önce yapıyoruz ki yeni emirler hemen eklenebilsin
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
                V::Spot(v) => match v.best_prices(&symbol).await {
                    Ok(prices) => prices,
                    Err(err) => {
                        warn!(%symbol, ?err, "failed to fetch best prices, skipping tick");
                        continue;
                    }
                },
                V::Fut(v) => match v.best_prices(&symbol).await {
                    Ok(prices) => prices,
                    Err(err) => {
                        warn!(%symbol, ?err, "failed to fetch best prices, skipping tick");
                        continue;
                    }
                },
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
                V::Spot(v) => match v.get_position(&symbol).await {
                    Ok(pos) => pos,
                    Err(err) => {
                        warn!(%symbol, ?err, "failed to fetch position, skipping tick");
                        continue;
                    }
                },
                V::Fut(v) => match v.get_position(&symbol).await {
                    Ok(pos) => pos,
                    Err(err) => {
                        warn!(%symbol, ?err, "failed to fetch position, skipping tick");
                        continue;
                    }
                },
            };

            let inv_diff = (state.inv.0 - pos.qty.0).abs();
            let reconcile_threshold = Decimal::new(1, 8);
            if inv_diff > reconcile_threshold {
                warn!(
                    %symbol,
                    ws_inv = %state.inv.0,
                    api_inv = %pos.qty.0,
                    diff = %inv_diff,
                    "inventory mismatch detected, syncing with API position"
                );
                state.inv = pos.qty;
            }

            let (mark_px, funding_rate, next_funding_time) = match &venue {
                V::Spot(v) => match v.mark_price(&symbol).await {
                    Ok(px) => (px, None, None),
                    Err(err) => {
                        warn!(%symbol, ?err, "failed to fetch mark price, skipping tick");
                        continue;
                    }
                },
                V::Fut(v) => match v.fetch_premium_index(&symbol).await {
                    Ok((mark, funding, next_time)) => (mark, funding, next_time),
                    Err(err) => {
                        warn!(%symbol, ?err, "failed to fetch premium index, skipping tick");
                        continue;
                    }
                },
            };

            record_pnl_snapshot(&mut state.pnl_history, &pos, mark_px);

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
                        if let Err(err) = v.cancel_all(&symbol).await {
                            warn!(%symbol, ?err, "failed to cancel all orders during halt");
                        }
                        if let Err(err) = v.close_position(&symbol).await {
                            warn!(%symbol, ?err, "failed to close position during halt");
                        }
                    }
                    V::Fut(v) => {
                        if let Err(err) = v.cancel_all(&symbol).await {
                            warn!(%symbol, ?err, "failed to cancel all orders during halt");
                        }
                        if let Err(err) = v.close_position(&symbol).await {
                            warn!(%symbol, ?err, "failed to close position during halt");
                        }
                    }
                }
                continue;
            }

            let ctx = Context {
                ob,
                sigma: 0.5,
                inv: state.inv,
                liq_gap_bps,
                funding_rate,
                next_funding_time,
                mark_price: mark_px, // Mark price stratejiye veriliyor
            };
            let mut quotes = state.strategy.on_tick(&ctx);
            info!(%symbol, ?quotes, ?risk_action, "strategy produced raw quotes");

            match risk_action {
                RiskAction::Reduce => {
                    let widen = Decimal::from_f64_retain(0.005).unwrap_or(Decimal::ZERO);
                    quotes.bid = quotes
                        .bid
                        .map(|(px, qty)| (Px(px.0 * (Decimal::ONE - widen)), qty));
                    quotes.ask = quotes
                        .ask
                        .map(|(px, qty)| (Px(px.0 * (Decimal::ONE + widen)), qty));
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

            // ---- CAP HESABI (sembolün kendi quote'u ile) ----
            let caps = match &venue {
                V::Spot(v) => {
                    let quote_free = match v.asset_free(&quote_asset).await {
                        Ok(q) => q.to_f64().unwrap_or(0.0),
                        Err(err) => {
                            warn!(%symbol, ?err, "failed to fetch quote asset balance, using zero");
                            0.0
                        }
                    };
                    let base_free = match v.asset_free(&base_asset).await {
                        Ok(b) => b.to_f64().unwrap_or(0.0),
                        Err(err) => {
                            warn!(%symbol, ?err, "failed to fetch base asset balance, using zero");
                            0.0
                        }
                    };
                    // Spot için: Tüm quote_free kullanılabilir, ama her emir max 100 USD
                    // Örnek: 20 USD varsa → buy_total=20 (tamamı kullanılır)
                    // Örnek: 100 USD varsa → buy_total=100 (tamamı kullanılır)
                    // Örnek: 200 USD varsa → buy_total=200, ama her emir max 100 USD
                    // Kalan 100 USD başka emirler için kullanılabilir (loop ile)
                    Caps {
                        buy_notional: cfg.max_usd_per_order.min(quote_free), // Her emir max 100 USD (veya mevcut bakiye, hangisi düşükse)
                        sell_notional: cfg.max_usd_per_order, // Her emir max 100 USD
                        sell_base: Some(base_free),
                        buy_total: quote_free, // Tüm bakiye kullanılabilir (20 varsa 20, 100 varsa 100, 200 varsa 200)
                        sell_total_base: base_free,
                    }
                }
                V::Fut(v) => {
                    let avail = match v.available_balance(&quote_asset).await {
                        Ok(a) => {
                            let avail_f64 = a.to_f64().unwrap_or(0.0);
                            // HIZLI KONTROL: 1 USD'den azsa işlem yapma
                            if avail_f64 < 1.0 {
                                // Bakiye çok düşük, bu sembolü skip et
                                0.0
                            } else if avail_f64 == 0.0 {
                                warn!(%symbol, quote_asset = %quote_asset, available_balance = %a, "available balance is zero or failed to convert to f64");
                                0.0
                            } else {
                                avail_f64
                            }
                        },
                        Err(err) => {
                            warn!(%symbol, quote_asset = %quote_asset, ?err, "failed to fetch available balance, using zero");
                            0.0
                        }
                    };
                    let risk_max_leverage = cfg.risk.max_leverage.max(1);
                    let requested_leverage = cfg.leverage.unwrap_or(risk_max_leverage);
                    let effective_leverage =
                        requested_leverage.max(1).min(risk_max_leverage) as f64;
                    
                    // ÖNEMLİ: Hesaptan giden para mantığı:
                    // - 20 USD varsa → 20 USD kullanılır (tamamı)
                    // - 100 USD varsa → 100 USD kullanılır (tamamı)
                    // - 200 USD varsa → 100 USD kullanılır (max limit), kalan 100 başka semboller için
                    // Leverage sadece pozisyon boyutunu belirler, hesaptan giden parayı etkilemez
                    // Örnek: 20 USD bakiye, 20x leverage → hesaptan 20 USD gider, pozisyon 400 USD olur
                    // Örnek: 200 USD bakiye, 20x leverage → hesaptan 100 USD gider (max limit), pozisyon 2000 USD olur
                    let max_usable_from_account = avail.min(cfg.max_usd_per_order);
                    
                    // Leverage ile açılan pozisyon boyutu (sadece bilgi amaçlı)
                    let position_size_with_leverage = max_usable_from_account * effective_leverage;
                    
                    // per_order_cap = margin (hesaptan giden para) = 100 USD
                    // per_order_notional = pozisyon boyutu = margin * leverage = 100 * 20 = 2000 USD
                    let per_order_cap_margin = cfg.max_usd_per_order;
                    let per_order_notional = per_order_cap_margin * effective_leverage;
                    info!(
                        %symbol,
                        quote_asset = %quote_asset,
                        available_balance = avail,
                        effective_leverage,
                        max_usable_from_account,
                        position_size_with_leverage,
                        per_order_limit_margin_usd = per_order_cap_margin,
                        per_order_limit_notional_usd = per_order_notional,
                        "calculated futures caps: max_usable_from_account is max USD that will leave your account, leverage only affects position size"
                    );
                    Caps {
                        buy_notional: per_order_notional,  // Her bid emri max 2000 USD pozisyon (100 USD margin * 20x)
                        sell_notional: per_order_notional, // Her ask emri max 2000 USD pozisyon (100 USD margin * 20x)
                        sell_base: None,
                        // buy_total: Hesaptan giden para (margin)
                        // - 20 USD varsa → 20 USD kullanılır (tamamı)
                        // - 100 USD varsa → 100 USD kullanılır (tamamı)
                        // - 200 USD varsa → 100 USD kullanılır (max limit), kalan 100 başka semboller için
                        buy_total: max_usable_from_account,
                        sell_total_base: 0.0,
                    }
                }
            };

            // Her taraf bağımsız: bid ve ask her biri max 100 USD kullanabilir
            // Toplam varlık paylaşılır (spent tracking ile)
            // Örnek: 300 USD varsa → bid için 100, ask için 100, kalan 100 ile ikinci bid yapılabilir
            // İki taraf için bölme yok, her taraf bağımsız max_usd_per_order'a kadar kullanabilir

            info!(
                %symbol,
                buy_notional = caps.buy_notional,
                sell_notional = caps.sell_notional,
                sell_base = ?caps.sell_base,
                buy_total = caps.buy_total,
                sell_total_base = caps.sell_total_base,
                "calculated order caps"
            );

            // --- min notional bilgisi varsa, kapasite bunun altındaysa tick'i atla ---
            if let Some(min_req) = state.min_notional_req {
                let buy_ok = caps.buy_notional >= min_req;
                let sell_ok = caps.sell_notional >= min_req;
                if !buy_ok && !sell_ok {
                    info!(
                        %symbol,
                        min_notional_req = min_req,
                        buy_notional = caps.buy_notional,
                        sell_notional = caps.sell_notional,
                        "skip tick: notional caps below exchange min_notional"
                    );
                    continue;
                }
            }

            // --- bakiye/min_emir hızlı kontrolü: gürültüyü kes ---
            let px_bid_f = bid.0.to_f64().unwrap_or(0.0);
            let px_ask_f = ask.0.to_f64().unwrap_or(0.0);
            let buy_cap_ok = caps.buy_notional >= min_usd_per_order;
            let mut sell_cap_ok = caps.sell_notional >= min_usd_per_order;
            if let Some(base_free) = caps.sell_base {
                let ref_px = if px_ask_f > 0.0 { px_ask_f } else { px_bid_f };
                if ref_px > 0.0 {
                    sell_cap_ok = sell_cap_ok && (base_free * ref_px >= min_usd_per_order);
                }
            }
            if !buy_cap_ok && !sell_cap_ok {
                info!(
                    %symbol,
                    buy_total = caps.buy_total,
                    sell_total_base = caps.sell_total_base,
                    min_usd_per_order,
                    "skip tick: zero/insufficient balance for this symbol"
                );
                continue;
            }
            if !buy_cap_ok {
                quotes.bid = None;
            }
            if !sell_cap_ok {
                quotes.ask = None;
            }

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
                        info!(
                            %symbol,
                            ?px,
                            original_qty = ?q,
                            qty_step = cfg.qty_step,
                            quantized_to_zero,
                            notional,
                            min_usd_per_order,
                            "skipping quote: qty too small after caps/quantization"
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
                        info!(
                            %symbol,
                            ?px,
                            original_qty = ?q,
                            sell_base = ?caps.sell_base,
                            qty_step = cfg.qty_step,
                            quantized_to_zero,
                            notional,
                            min_usd_per_order,
                            "skipping quote: qty too small after caps/quantization"
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
                    // ---- SPOT BID ----
                    if let Some((px, qty)) = quotes.bid {
                        info!(%symbol, ?px, ?qty, tif = ?tif, "placing spot bid order");
                        match v.place_limit(&symbol, Side::Buy, px, qty, tif).await {
                            Ok(order_id) => {
                                let info = OrderInfo { order_id: order_id.clone(), side: Side::Buy, price: px, qty, created_at: Instant::now() };
                                state.active_orders.insert(order_id, info);
                            }
                            Err(err) => {
                                warn!(%symbol, ?px, ?qty, tif = ?tif, ?err, "failed to place spot bid order");
                            }
                        }

                        // Kalan USD ile ikinci emir
                        let spent = (px.0.to_f64().unwrap_or(0.0)) * (qty.0.to_f64().unwrap_or(0.0));
                        let remaining = (caps.buy_total - spent).max(0.0);
                        if remaining >= min_usd_per_order && px.0 > Decimal::ZERO {
                            let qty2 = clamp_qty_by_usd(qty, px, remaining, cfg.qty_step);
                            if qty2.0 > Decimal::ZERO {
                                info!(%symbol, ?px, qty = ?qty2, tif = ?tif, remaining, "placing extra spot bid with leftover USD");
                                match v.place_limit(&symbol, Side::Buy, px, qty2, tif).await {
                                    Ok(order_id2) => {
                                        let info2 = OrderInfo { order_id: order_id2.clone(), side: Side::Buy, price: px, qty: qty2, created_at: Instant::now() };
                                        state.active_orders.insert(order_id2, info2);
                                    }
                                    Err(err) => {
                                        warn!(%symbol, ?px, qty = ?qty2, tif = ?tif, ?err, "failed to place extra spot bid order");
                                    }
                                }
                            }
                        }
                    }

                    // ---- SPOT ASK ----
                    if let Some((px, qty)) = quotes.ask {
                        info!(%symbol, ?px, ?qty, tif = ?tif, "placing spot ask order");
                        match v.place_limit(&symbol, Side::Sell, px, qty, tif).await {
                            Ok(order_id) => {
                                let info = OrderInfo { order_id: order_id.clone(), side: Side::Sell, price: px, qty, created_at: Instant::now() };
                                state.active_orders.insert(order_id, info);
                            }
                            Err(err) => {
                                warn!(%symbol, ?px, ?qty, tif = ?tif, ?err, "failed to place spot ask order");
                            }
                        }

                        // Kalan base ile ikinci ask (opsiyonel)
                        if let Some(base_total) = caps.sell_base {
                            let spent_base = qty.0.to_f64().unwrap_or(0.0);
                            let remaining_base = (base_total - spent_base).max(0.0);
                            let remaining_notional = remaining_base * px.0.to_f64().unwrap_or(0.0);
                            if remaining_notional >= min_usd_per_order && px.0 > Decimal::ZERO {
                                let qty2 = clamp_qty_by_base(qty, remaining_base, cfg.qty_step);
                                if qty2.0 > Decimal::ZERO {
                                    info!(%symbol, ?px, qty = ?qty2, tif = ?tif, remaining_base, "placing extra spot ask with leftover base");
                                    match v.place_limit(&symbol, Side::Sell, px, qty2, tif).await {
                                        Ok(order_id2) => {
                                            let info2 = OrderInfo { order_id: order_id2.clone(), side: Side::Sell, price: px, qty: qty2, created_at: Instant::now() };
                                            state.active_orders.insert(order_id2, info2);
                                        }
                                        Err(err) => {
                                            warn!(%symbol, ?px, qty = ?qty2, tif = ?tif, ?err, "failed to place extra spot ask order");
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                V::Fut(v) => {
                    // ---- FUTURES BID ----
                    // Her bid emri bağımsız, max 100 USD, toplam varlık paylaşılır
                    // ÖNEMLİ: total_spent_on_bids = hesaptan giden para (margin), pozisyon boyutu değil
                    // Örnek: 100 USD notional pozisyon, 20x leverage → hesaptan giden: 100/20 = 5 USD
                    // effective_leverage hesaplaması caps hesaplamasıyla aynı olmalı
                    let risk_max_leverage = cfg.risk.max_leverage.max(1);
                    let requested_leverage = cfg.leverage.unwrap_or(risk_max_leverage);
                    let effective_leverage = requested_leverage.max(1).min(risk_max_leverage) as f64;
                    let mut total_spent_on_bids = 0.0f64; // Hesaptan giden para (margin) toplamı
                    if let Some((px, qty)) = quotes.bid {
                        
                        // İlk bid emri (max 100 USD)
                        info!(%symbol, ?px, ?qty, tif = ?tif, "placing futures bid order");
                        match v.place_limit(&symbol, Side::Buy, px, qty, tif).await {
                            Ok(order_id) => {
                                let info = OrderInfo { order_id: order_id.clone(), side: Side::Buy, price: px, qty, created_at: Instant::now() };
                                state.active_orders.insert(order_id, info);
                                // Başarılı emir için spent hesapla (hesaptan giden para = margin)
                                // Notional (pozisyon boyutu) / leverage = hesaptan giden para
                                let notional = (px.0.to_f64().unwrap_or(0.0)) * (qty.0.to_f64().unwrap_or(0.0));
                                total_spent_on_bids = notional / effective_leverage;
                            }
                            Err(err) => {
                                let msg = err.to_string();
                                if msg.contains("below min notional after clamps") {
                                    let required_min = msg
                                        .split('<')
                                        .nth(1)
                                        .and_then(|s| s.split(')').next())
                                        .and_then(|s| s.trim().parse::<f64>().ok());
                                    if let Some(min_notional) = required_min {
                                        // --- öğren & gerekirse kalıcı disable ---
                                        state.min_notional_req = Some(min_notional);
                                        // >= kontrolü: min_notional (pozisyon boyutu) >= max_notional (margin * leverage) ise devre dışı
                                        let max_notional = cfg.max_usd_per_order * effective_leverage;
                                        if min_notional >= max_notional {
                                            state.disabled = true;
                                            warn!(%symbol, min_notional, max_notional, max_usd_per_order = cfg.max_usd_per_order, effective_leverage,
                                                  "disabling symbol: exchange min_notional >= max position size (margin * leverage)");
                                            continue;
                                        }
                                        // retry: min_notional'a göre miktarı büyüt (cap'e kadar)
                                        // order_cap = pozisyon boyutu (notional), margin değil
                                        // ÖNEMLİ: Fiyat ve miktar quantize edilecek, bu yüzden quantize edilmiş değerleri kullan
                                        let price_raw = px.0.to_f64().unwrap_or(0.0);
                                        let price_tick = cfg.price_tick;
                                        let step = cfg.qty_step;
                                        
                                        // Fiyatı quantize et (place_limit içinde yapılan işlem)
                                        let price_quantized = if price_tick > 0.0 {
                                            (price_raw / price_tick).floor() * price_tick
                                        } else {
                                            price_raw
                                        };
                                        
                                        let order_cap = caps.buy_notional; // Zaten notional (pozisyon boyutu)
                                        // MARGIN KONTROLÜ: Retry'da hesaplanan miktar için yeterli margin var mı?
                                        let available_margin = caps.buy_total - total_spent_on_bids; // Kalan margin
                                        let mut new_qty = 0.0f64;
                                        if price_quantized > 0.0 && step > 0.0 && available_margin > 0.0 {
                                            // Güvenli margin: min_notional'ın %10 fazlasını hedefle (quantize kayıpları için)
                                            let target_notional = min_notional * 1.10;
                                            let raw_qty = target_notional / price_quantized;
                                            new_qty = (raw_qty / step).floor() * step;
                                            
                                            // Cap kontrolü: max qty by cap (notional)
                                            let max_qty_by_cap = (order_cap / price_quantized / step).floor() * step;
                                            // Margin kontrolü: max qty by available margin
                                            let max_qty_by_margin = ((available_margin * effective_leverage) / price_quantized / step).floor() * step;
                                            let max_qty = max_qty_by_cap.min(max_qty_by_margin);
                                            if new_qty > max_qty { 
                                                new_qty = max_qty; 
                                            }
                                            
                                            // Min notional garantisi: quantize edilmiş fiyat ve miktarla kontrol et
                                            // Loop ile min_notional'ı geçene kadar step artır
                                            let mut attempts = 0;
                                            let max_attempts = 20; // Maksimum 20 step artır (daha fazla deneme)
                                            while attempts < max_attempts {
                                                let notional_after_quantize = new_qty * price_quantized;
                                                if notional_after_quantize >= min_notional {
                                                    // Margin kontrolü: Bu miktar için yeterli margin var mı?
                                                    let required_margin = notional_after_quantize / effective_leverage;
                                                    if required_margin <= available_margin {
                                                        break; // Yeterli notional ve margin
                                                    } else {
                                                        // Margin yetersiz, daha küçük miktar dene
                                                        break;
                                                    }
                                                }
                                                if new_qty + step > max_qty {
                                                    break; // Cap'e ulaştık
                                                }
                                                new_qty += step;
                                                attempts += 1;
                                            }
                                        }
                                        if new_qty > 0.0 {
                                            let retry_qty = Qty(rust_decimal::Decimal::from_f64_retain(new_qty).unwrap_or(rust_decimal::Decimal::ZERO));
                                            info!(%symbol, ?px, qty = ?retry_qty, tif = ?tif, min_notional, "retrying futures bid with exchange min notional");
                                            match v.place_limit(&symbol, Side::Buy, px, retry_qty, tif).await {
                                                Ok(order_id) => {
                                                    let info = OrderInfo { order_id: order_id.clone(), side: Side::Buy, price: px, qty: retry_qty, created_at: Instant::now() };
                                                    state.active_orders.insert(order_id, info);
                                                    // Retry başarılı oldu, spent güncelle (hesaptan giden para = margin)
                                                    let notional = (px.0.to_f64().unwrap_or(0.0)) * (retry_qty.0.to_f64().unwrap_or(0.0));
                                                    total_spent_on_bids = notional / effective_leverage;
                                                }
                                                Err(err2) => {
                                                    warn!(%symbol, ?px, qty = ?retry_qty, tif = ?tif, ?err2, "retry bid still failed");
                                                }
                                            }
                                        } else {
                                            warn!(%symbol, ?px, required_min = ?min_notional, cap = ?order_cap, "skip bid: insufficient balance for exchange min notional");
                                        }
                                    } else {
                                        warn!(%symbol, ?px, ?qty, ?err, "failed bid (min notional parse failed)");
                                    }
                                } else {
                                    warn!(%symbol, ?px, ?qty, tif = ?tif, ?err, "failed to place futures bid order");
                                }
                            }
                        }

                        // Kalan notional ile ikinci/üçüncü bid emirleri (her biri max 100 USD)
                        // Toplam varlık paylaşılır: 300 USD varsa → bid 100, ask 100, kalan 100 ile ikinci bid
                        let min_req_for_second = state.min_notional_req.unwrap_or(min_usd_per_order);
                        // Kalan bakiye varsa ve min_notional'a yetiyorsa, max 100 USD'lik ek emirler yap
                        loop {
                            let remaining = (caps.buy_total - total_spent_on_bids).max(0.0);
                            if remaining < min_req_for_second.max(min_usd_per_order) || px.0 <= Decimal::ZERO {
                                break; // Yetersiz bakiye veya geçersiz fiyat
                            }
                            
                            // Her ek emir max 100 USD (hesaptan giden para = margin)
                            // order_size = margin, notional = margin * leverage
                            let order_size_margin = remaining.min(cfg.max_usd_per_order);
                            let order_size_notional = order_size_margin * effective_leverage; // Pozisyon boyutu
                            let qty2 = clamp_qty_by_usd(qty, px, order_size_notional, cfg.qty_step);
                            let qty2_notional = (px.0.to_f64().unwrap_or(0.0)) * (qty2.0.to_f64().unwrap_or(0.0));
                            
                            if qty2.0 > Decimal::ZERO && qty2_notional >= min_req_for_second {
                                info!(%symbol, ?px, qty = ?qty2, tif = ?tif, remaining, order_size_margin, order_size_notional, min_notional = min_req_for_second, "placing extra futures bid with leftover notional");
                                match v.place_limit(&symbol, Side::Buy, px, qty2, tif).await {
                                    Ok(order_id2) => {
                                        let info2 = OrderInfo { order_id: order_id2.clone(), side: Side::Buy, price: px, qty: qty2, created_at: Instant::now() };
                                        state.active_orders.insert(order_id2, info2);
                                        // Spent güncelle (hesaptan giden para = margin)
                                        // Notional / leverage = hesaptan giden para
                                        total_spent_on_bids += qty2_notional / effective_leverage;
                                    }
                                    Err(err) => {
                                        warn!(%symbol, ?px, qty = ?qty2, tif = ?tif, ?err, "failed to place extra futures bid order");
                                        break; // Hata varsa döngüden çık
                                    }
                                }
                            } else {
                                break; // Yetersiz notional, döngüden çık
                            }
                        }
                    }

                    // ---- FUTURES ASK ----
                    // Her ask emri bağımsız, max 100 USD, toplam varlık paylaşılır (bid'lerden sonra kalan)
                    // ÖNEMLİ: total_spent_on_asks = hesaptan giden para (margin), pozisyon boyutu değil
                    // effective_leverage hesaplaması caps hesaplamasıyla aynı olmalı
                    if let Some((px, qty)) = quotes.ask {
                        let risk_max_leverage_ask = cfg.risk.max_leverage.max(1);
                        let requested_leverage_ask = cfg.leverage.unwrap_or(risk_max_leverage_ask);
                        let effective_leverage_ask = requested_leverage_ask.max(1).min(risk_max_leverage_ask) as f64;
                        let mut total_spent_on_asks = 0.0f64; // Hesaptan giden para (margin) toplamı
                        
                        // İlk ask emri (max 100 USD)
                        info!(%symbol, ?px, ?qty, tif = ?tif, "placing futures ask order");
                        match v.place_limit(&symbol, Side::Sell, px, qty, tif).await {
                            Ok(order_id) => {
                                let info = OrderInfo { order_id: order_id.clone(), side: Side::Sell, price: px, qty, created_at: Instant::now() };
                                state.active_orders.insert(order_id, info);
                                // Başarılı emir için spent hesapla (hesaptan giden para = margin)
                                // Notional (pozisyon boyutu) / leverage = hesaptan giden para
                                let notional = (px.0.to_f64().unwrap_or(0.0)) * (qty.0.to_f64().unwrap_or(0.0));
                                total_spent_on_asks = notional / effective_leverage_ask;
                            }
                            Err(err) => {
                                let msg = err.to_string();
                                if msg.contains("below min notional after clamps") {
                                    let required_min = msg
                                        .split('<')
                                        .nth(1)
                                        .and_then(|s| s.split(')').next())
                                        .and_then(|s| s.trim().parse::<f64>().ok());
                                    if let Some(min_notional) = required_min {
                                        // --- öğren & gerekirse kalıcı disable ---
                                        state.min_notional_req = Some(min_notional);
                                        // >= kontrolü: min_notional (pozisyon boyutu) >= max_notional (margin * leverage) ise devre dışı
                                        let max_notional = cfg.max_usd_per_order * effective_leverage_ask;
                                        if min_notional >= max_notional {
                                            state.disabled = true;
                                            warn!(%symbol, min_notional, max_notional, max_usd_per_order = cfg.max_usd_per_order, effective_leverage = effective_leverage_ask,
                                                  "disabling symbol: exchange min_notional >= max position size (margin * leverage)");
                                            continue;
                                        }
                                        // order_cap = pozisyon boyutu (notional), margin değil
                                        // ÖNEMLİ: Fiyat ve miktar quantize edilecek, bu yüzden quantize edilmiş değerleri kullan
                                        let price_raw = px.0.to_f64().unwrap_or(0.0);
                                        let price_tick = cfg.price_tick;
                                        let step = cfg.qty_step;
                                        
                                        // Fiyatı quantize et (place_limit içinde yapılan işlem)
                                        let price_quantized = if price_tick > 0.0 {
                                            (price_raw / price_tick).floor() * price_tick
                                        } else {
                                            price_raw
                                        };
                                        
                                        let order_cap = caps.sell_notional; // Zaten notional (pozisyon boyutu)
                                        // MARGIN KONTROLÜ: Retry'da hesaplanan miktar için yeterli margin var mı?
                                        let available_margin = caps.buy_total - total_spent_on_bids - total_spent_on_asks; // Kalan margin
                                        let mut new_qty = 0.0f64;
                                        if price_quantized > 0.0 && step > 0.0 && available_margin > 0.0 {
                                            // Güvenli margin: min_notional'ın %10 fazlasını hedefle (quantize kayıpları için)
                                            let target_notional = min_notional * 1.10;
                                            let raw_qty = target_notional / price_quantized;
                                            new_qty = (raw_qty / step).floor() * step;
                                            
                                            // Cap kontrolü: max qty by cap (notional)
                                            let max_qty_by_cap = (order_cap / price_quantized / step).floor() * step;
                                            // Margin kontrolü: max qty by available margin
                                            let max_qty_by_margin = ((available_margin * effective_leverage_ask) / price_quantized / step).floor() * step;
                                            let max_qty = max_qty_by_cap.min(max_qty_by_margin);
                                            if new_qty > max_qty { 
                                                new_qty = max_qty; 
                                            }
                                            
                                            // Min notional garantisi: quantize edilmiş fiyat ve miktarla kontrol et
                                            // Loop ile min_notional'ı geçene kadar step artır
                                            let mut attempts = 0;
                                            let max_attempts = 20; // Maksimum 20 step artır (daha fazla deneme)
                                            while attempts < max_attempts {
                                                let notional_after_quantize = new_qty * price_quantized;
                                                if notional_after_quantize >= min_notional {
                                                    // Margin kontrolü: Bu miktar için yeterli margin var mı?
                                                    let required_margin = notional_after_quantize / effective_leverage_ask;
                                                    if required_margin <= available_margin {
                                                        break; // Yeterli notional ve margin
                                                    } else {
                                                        // Margin yetersiz, daha küçük miktar dene
                                                        break;
                                                    }
                                                }
                                                if new_qty + step > max_qty {
                                                    break; // Cap'e ulaştık
                                                }
                                                new_qty += step;
                                                attempts += 1;
                                            }
                                        }
                                        if new_qty > 0.0 {
                                            let retry_qty = Qty(rust_decimal::Decimal::from_f64_retain(new_qty).unwrap_or(rust_decimal::Decimal::ZERO));
                                            info!(%symbol, ?px, qty = ?retry_qty, tif = ?tif, min_notional, "retrying futures ask with exchange min notional");
                                            match v.place_limit(&symbol, Side::Sell, px, retry_qty, tif).await {
                                                Ok(order_id) => {
                                                    let info = OrderInfo { order_id: order_id.clone(), side: Side::Sell, price: px, qty: retry_qty, created_at: Instant::now() };
                                                    state.active_orders.insert(order_id, info);
                                                    // Retry başarılı oldu, spent güncelle (hesaptan giden para = margin)
                                                    let notional = (px.0.to_f64().unwrap_or(0.0)) * (retry_qty.0.to_f64().unwrap_or(0.0));
                                                    total_spent_on_asks = notional / effective_leverage_ask;
                                                }
                                                Err(err2) => {
                                                    warn!(%symbol, ?px, qty = ?retry_qty, tif = ?tif, ?err2, "retry ask still failed");
                                                }
                                            }
                                        } else {
                                            warn!(%symbol, ?px, required_min = ?min_notional, cap = ?order_cap, "skip ask: insufficient balance for exchange min notional");
                                        }
                                    } else {
                                        warn!(%symbol, ?px, ?qty, ?err, "failed ask (min notional parse failed)");
                                    }
                                } else {
                                    warn!(%symbol, ?px, ?qty, tif = ?tif, ?err, "failed to place futures ask order");
                                }
                            }
                        }
                        
                        // Kalan notional ile ikinci/üçüncü ask emirleri (her biri max 100 USD)
                        // Ask'ler bid'lerden sonra kalan bakiyeyi kullanır
                        let min_req_for_second = state.min_notional_req.unwrap_or(min_usd_per_order);
                        loop {
                            // Bid'lerden sonra kalan bakiye
                            let remaining = (caps.buy_total - total_spent_on_bids - total_spent_on_asks).max(0.0);
                            if remaining < min_req_for_second.max(min_usd_per_order) || px.0 <= Decimal::ZERO {
                                break; // Yetersiz bakiye veya geçersiz fiyat
                            }
                            
                            // Her ek emir max 100 USD (hesaptan giden para = margin)
                            // order_size = margin, notional = margin * leverage
                            let order_size_margin = remaining.min(cfg.max_usd_per_order);
                            let order_size_notional = order_size_margin * effective_leverage_ask; // Pozisyon boyutu
                            let qty2 = clamp_qty_by_usd(qty, px, order_size_notional, cfg.qty_step);
                            let qty2_notional = (px.0.to_f64().unwrap_or(0.0)) * (qty2.0.to_f64().unwrap_or(0.0));
                            
                            if qty2.0 > Decimal::ZERO && qty2_notional >= min_req_for_second {
                                info!(%symbol, ?px, qty = ?qty2, tif = ?tif, remaining, order_size_margin, order_size_notional, min_notional = min_req_for_second, "placing extra futures ask with leftover notional");
                                match v.place_limit(&symbol, Side::Sell, px, qty2, tif).await {
                                    Ok(order_id2) => {
                                        let info2 = OrderInfo { order_id: order_id2.clone(), side: Side::Sell, price: px, qty: qty2, created_at: Instant::now() };
                                        state.active_orders.insert(order_id2, info2);
                                        // Spent güncelle (hesaptan giden para = margin)
                                        // Notional / leverage = hesaptan giden para
                                        total_spent_on_asks += qty2_notional / effective_leverage_ask;
                                    }
                                    Err(err) => {
                                        warn!(%symbol, ?px, qty = ?qty2, tif = ?tif, ?err, "failed to place extra futures ask order");
                                        break; // Hata varsa döngüden çık
                                    }
                                }
                            } else {
                                break; // Yetersiz notional, döngüden çık
                            }
                        }
                    }
                }
            }
        }
    }
}
