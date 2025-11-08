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
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use std::cmp::max;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

// --- API RATE LIMIT KORUMASI: Binance limitleri ---
// Spot: 1200 req/min (20 req/sec)
// Futures: 2400 req/min (40 req/sec)
// KRİTİK: En güvenli limit kullan (Spot: 20 req/sec)
// Sliding window rate limiter: Son 1 saniyede kaç request yapıldığını takip et
use std::sync::Mutex;
use std::collections::VecDeque;

struct RateLimiter {
    requests: Mutex<VecDeque<Instant>>,
    max_requests_per_sec: u32,
    min_interval_ms: u64,
}

impl RateLimiter {
    fn new(max_requests_per_sec: u32) -> Self {
        // Güvenlik için %20 margin ekle (20 req/sec → 16 req/sec kullan)
        let safe_limit = (max_requests_per_sec as f64 * 0.8) as u32;
        let min_interval_ms = 1000 / safe_limit as u64;
        Self {
            requests: Mutex::new(VecDeque::new()),
            max_requests_per_sec: safe_limit,
            min_interval_ms,
        }
    }
    
    async fn wait_if_needed(&self) {
        loop {
            let now = Instant::now();
            let mut requests = self.requests.lock().unwrap();
            
            // Son 1 saniyede yapılan request'leri temizle
            let one_sec_ago = now.checked_sub(Duration::from_secs(1)).unwrap_or(Instant::now());
            while requests.front().map_or(false, |&t| t < one_sec_ago) {
                requests.pop_front();
            }
            
            // Eğer limit aşıldıysa bekle
            if requests.len() >= self.max_requests_per_sec as usize {
                if let Some(oldest) = requests.front().copied() {
                    let wait_time = oldest + Duration::from_secs(1);
                    if wait_time > now {
                        let sleep_duration = wait_time.duration_since(now);
                        drop(requests); // Lock'u bırak
                        tokio::time::sleep(sleep_duration).await;
                        continue; // Tekrar kontrol et
                    }
                }
            }
            
            // Minimum interval kontrolü (her request arasında minimum bekleme)
            if let Some(last) = requests.back() {
                let elapsed = now.duration_since(*last);
                if elapsed.as_millis() < self.min_interval_ms as u128 {
                    let wait = Duration::from_millis(self.min_interval_ms).saturating_sub(elapsed);
                    if wait.as_millis() > 0 {
                        drop(requests);
                        tokio::time::sleep(wait).await;
                        continue; // Tekrar kontrol et
                    }
                }
            }
            
            // Request'i kaydet ve çık
            requests.push_back(now);
            break;
        }
    }
}

// Global rate limiter (Spot limit kullan - en güvenli)
use std::sync::OnceLock;
static RATE_LIMITER: OnceLock<RateLimiter> = OnceLock::new();

fn get_rate_limiter() -> &'static RateLimiter {
    RATE_LIMITER.get_or_init(|| {
        RateLimiter::new(20) // Spot: 20 req/sec, güvenlik için 16 req/sec kullan
    })
}

async fn rate_limit_guard() {
    get_rate_limiter().wait_if_needed().await;
}

struct SymbolState {
    meta: SymbolMeta,
    inv: Qty,
    strategy: Box<dyn Strategy>,
    active_orders: HashMap<String, OrderInfo>,
    pnl_history: Vec<Decimal>,
    // --- eklendi: min notional öğrenme ve devre dışı bırakma ---
    min_notional_req: Option<f64>, // borsa min notional (quote cinsinden)
    disabled: bool,                // min_notional > max_usd_per_order => kalıcı disable
    // --- PER-SYMBOL METADATA: Exchange'den çekilen tick_size ve step_size ---
    symbol_rules: Option<std::sync::Arc<exec::binance::SymbolRules>>, // Per-symbol metadata (fallback: global cfg)
    // --- AKILLI TAKİP: Pozisyon ve emir durumu analizi ---
    last_position_check: Option<Instant>, // Son pozisyon kontrol zamanı
    last_order_sync: Option<Instant>,     // Son emir senkronizasyon zamanı
    order_fill_rate: f64,                 // Emir fill oranı (0.0-1.0)
    consecutive_no_fills: u32,            // Ardışık fill olmayan tick sayısı
    // --- AKILLI POZİSYON YÖNETİMİ: Zeka bazlı karar verme ---
    position_entry_time: Option<Instant>,  // Pozisyon açılış zamanı
    peak_pnl: Decimal,                     // En yüksek PnL (kar al için)
    position_hold_duration_ms: u64,        // Pozisyon tutma süresi (ms)
    last_order_price_update: HashMap<String, Px>, // Son emir fiyat güncellemesi (fiyat değişikliği kontrolü için)
    // --- GELİŞMİŞ RİSK VE KAZANÇ TAKİBİ: Detaylı analiz ---
    daily_pnl: Decimal,                    // Günlük PnL (reset edilebilir)
    total_funding_cost: Decimal,           // Toplam funding maliyeti (futures)
    position_size_notional_history: Vec<f64>, // Pozisyon boyutu geçmişi (risk analizi için)
    last_pnl_alert: Option<Instant>,       // Son PnL uyarısı zamanı (spam önleme)
    cumulative_pnl: Decimal,               // Kümülatif PnL (bot başlangıcından beri)
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
    // --- Spread ve Fiyatlama Eşikleri ---
    #[serde(default)]
    min_spread_bps: Option<f64>,
    #[serde(default)]
    max_spread_bps: Option<f64>,
    #[serde(default)]
    spread_arbitrage_min_bps: Option<f64>,
    #[serde(default)]
    spread_arbitrage_max_bps: Option<f64>,
    // --- Trend Takibi Eşikleri ---
    #[serde(default)]
    strong_trend_bps: Option<f64>,
    #[serde(default)]
    momentum_strong_bps: Option<f64>,
    #[serde(default)]
    trend_bias_multiplier: Option<f64>,
    // --- Adverse Selection Eşikleri ---
    #[serde(default)]
    adverse_selection_threshold_on: Option<f64>,
    #[serde(default)]
    adverse_selection_threshold_off: Option<f64>,
    // --- Fırsat Modu Eşikleri ---
    #[serde(default)]
    opportunity_threshold_on: Option<f64>,
    #[serde(default)]
    opportunity_threshold_off: Option<f64>,
    // --- Manipülasyon Tespit Eşikleri ---
    #[serde(default)]
    price_jump_threshold_bps: Option<f64>,
    #[serde(default)]
    fake_breakout_threshold_bps: Option<f64>,
    #[serde(default)]
    liquidity_drop_threshold: Option<f64>,
    // --- Envanter Yönetimi ---
    #[serde(default)]
    inventory_threshold_ratio: Option<f64>,
    // --- Adaptif Spread Katsayıları ---
    #[serde(default)]
    volatility_coefficient: Option<f64>,
    #[serde(default)]
    ofi_coefficient: Option<f64>,
    // --- Diğer ---
    #[serde(default)]
    min_liquidity_required: Option<f64>,
    #[serde(default)]
    opportunity_size_multiplier: Option<f64>,
    #[serde(default)]
    strong_trend_multiplier: Option<f64>,
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

fn default_min_quote_balance_usd() -> f64 {
    1.0 // Default: 1 USD minimum quote asset balance
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
    #[serde(default = "default_min_quote_balance_usd")]
    min_quote_balance_usd: f64, // Quote asset minimum bakiye eşiği (USD)
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

/// Per-symbol step_size kullanır, yoksa fallback olarak global qty_step kullanır
fn get_qty_step(symbol_rules: Option<&std::sync::Arc<exec::binance::SymbolRules>>, fallback: f64) -> f64 {
    symbol_rules
        .map(|r| r.step_size.to_f64().unwrap_or(fallback))
        .unwrap_or(fallback)
}

/// Per-symbol tick_size kullanır, yoksa fallback olarak global price_tick kullanır
fn get_price_tick(symbol_rules: Option<&std::sync::Arc<exec::binance::SymbolRules>>, fallback: f64) -> f64 {
    symbol_rules
        .map(|r| r.tick_size.to_f64().unwrap_or(fallback))
        .unwrap_or(fallback)
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
        // Config'den gelen değerler (yoksa default kullanılır)
        min_spread_bps: cfg.strategy.min_spread_bps.unwrap_or(3.0),
        max_spread_bps: cfg.strategy.max_spread_bps.unwrap_or(100.0),
        spread_arbitrage_min_bps: cfg.strategy.spread_arbitrage_min_bps.unwrap_or(30.0),
        spread_arbitrage_max_bps: cfg.strategy.spread_arbitrage_max_bps.unwrap_or(200.0),
        strong_trend_bps: cfg.strategy.strong_trend_bps.unwrap_or(100.0),
        momentum_strong_bps: cfg.strategy.momentum_strong_bps.unwrap_or(50.0),
        trend_bias_multiplier: cfg.strategy.trend_bias_multiplier.unwrap_or(1.0),
        adverse_selection_threshold_on: cfg.strategy.adverse_selection_threshold_on.unwrap_or(0.6),
        adverse_selection_threshold_off: cfg.strategy.adverse_selection_threshold_off.unwrap_or(0.4),
        opportunity_threshold_on: cfg.strategy.opportunity_threshold_on.unwrap_or(0.5),
        opportunity_threshold_off: cfg.strategy.opportunity_threshold_off.unwrap_or(0.2),
        price_jump_threshold_bps: cfg.strategy.price_jump_threshold_bps.unwrap_or(150.0),
        fake_breakout_threshold_bps: cfg.strategy.fake_breakout_threshold_bps.unwrap_or(100.0),
        liquidity_drop_threshold: cfg.strategy.liquidity_drop_threshold.unwrap_or(0.5),
        inventory_threshold_ratio: cfg.strategy.inventory_threshold_ratio.unwrap_or(0.05),
        volatility_coefficient: cfg.strategy.volatility_coefficient.unwrap_or(0.5),
        ofi_coefficient: cfg.strategy.ofi_coefficient.unwrap_or(0.5),
        min_liquidity_required: cfg.strategy.min_liquidity_required.unwrap_or(0.01),
        opportunity_size_multiplier: cfg.strategy.opportunity_size_multiplier.unwrap_or(2.0), // 5.0 → 2.0: daha güvenli
        strong_trend_multiplier: cfg.strategy.strong_trend_multiplier.unwrap_or(1.5), // 3.0 → 1.5: daha güvenli
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
                    rate_limit_guard().await;
                    let q_free = vtmp.asset_free(&meta.quote_asset).await?.to_f64().unwrap_or(0.0);
                    rate_limit_guard().await;
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
        
        // --- QUOTE ASSET BAKİYE FİLTRESİ: Yetersiz bakiye olan quote asset'li sembolleri filtrele ---
        // Tüm quote asset'lerin bakiyelerini kontrol et ve yetersiz olanları baştan filtrele
        // Böylece gereksiz işlem yapılmaz
        let mut quote_asset_balances: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
        
        if mode_lower == "futures" {
            if let V::Fut(vtmp) = &venue {
                // Tüm benzersiz quote asset'leri bul
                let unique_quotes: std::collections::HashSet<String> = auto.iter()
                    .map(|m| m.quote_asset.clone())
                    .collect();
                
                // Her quote asset için bakiye kontrolü yap
                for quote in unique_quotes {
                    let balance = vtmp.available_balance(&quote).await.ok()
                        .and_then(|b| b.to_f64())
                        .unwrap_or(0.0);
                    quote_asset_balances.insert(quote.clone(), balance);
                    
                    if balance < cfg.min_quote_balance_usd {
                        info!(
                            quote_asset = %quote,
                            balance,
                            min_required = cfg.min_quote_balance_usd,
                            "FILTERING: quote asset balance insufficient, removing all symbols with this quote asset"
                        );
                    }
                }
            }
        } else {
            if let V::Spot(vtmp) = &venue {
                // Tüm benzersiz quote asset'leri bul
                let unique_quotes: std::collections::HashSet<String> = auto.iter()
                    .map(|m| m.quote_asset.clone())
                    .collect();
                
                // Her quote asset için bakiye kontrolü yap
                for quote in unique_quotes {
                    rate_limit_guard().await;
                    let balance = vtmp.asset_free(&quote).await.ok()
                        .and_then(|b| b.to_f64())
                        .unwrap_or(0.0);
                    quote_asset_balances.insert(quote.clone(), balance);
                    
                    if balance < cfg.min_quote_balance_usd {
                        info!(
                            quote_asset = %quote,
                            balance,
                            min_required = cfg.min_quote_balance_usd,
                            "FILTERING: quote asset balance insufficient, removing all symbols with this quote asset"
                        );
                    }
                }
            }
        }
        
        // Yetersiz bakiye olan quote asset'li sembolleri filtrele
        auto.retain(|m| {
            if let Some(&balance) = quote_asset_balances.get(&m.quote_asset) {
                if balance >= cfg.min_quote_balance_usd {
                    true // Yeterli bakiye var, tut
                } else {
                    false // Yetersiz bakiye, filtrele
                }
            } else {
                // Bakiye bilgisi yok, güvenli tarafta kal ve filtrele
                false
            }
        });
        
        info!(
            symbols_after_filtering = auto.len(),
            "filtered symbols by quote asset balance"
        );
        
        // AKILLI SEÇİM: Hangi USD-stable asset'te daha fazla para varsa onu önceliklendir
        // Not: Yetersiz bakiye olan quote asset'ler zaten filtrelenmiş durumda
        if want_group && mode_lower == "futures" {
            if let V::Fut(_) = &venue {
                // USDT ve USDC bakiyelerini kontrol et (zaten quote_asset_balances'da var)
                let usdt_bal = quote_asset_balances.get("USDT").copied().unwrap_or(0.0);
                let usdc_bal = quote_asset_balances.get("USDC").copied().unwrap_or(0.0);
                
                // Hangi asset'te daha fazla para varsa, o asset'in sembollerini önceliklendir
                // (Her iki asset de zaten yeterli bakiyeye sahip, çünkü filtrelenmiş)
                if usdt_bal >= cfg.min_quote_balance_usd && usdt_bal > usdc_bal {
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
                } else if usdc_bal >= cfg.min_quote_balance_usd && usdc_bal > usdt_bal {
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
                // Spot için de aynı önceliklendirme mantığı
                if want_group {
                    // USDT ve USDC bakiyelerini kontrol et (zaten quote_asset_balances'da var)
                    let usdt_bal = quote_asset_balances.get("USDT").copied().unwrap_or(0.0);
                    let usdc_bal = quote_asset_balances.get("USDC").copied().unwrap_or(0.0);
                    
                    if usdt_bal >= cfg.min_quote_balance_usd && usdt_bal > usdc_bal {
                        auto.sort_by(|a, b| {
                            let a_usdt = a.quote_asset.eq_ignore_ascii_case("USDT");
                            let b_usdt = b.quote_asset.eq_ignore_ascii_case("USDT");
                            match (a_usdt, b_usdt) {
                                (true, false) => std::cmp::Ordering::Less,
                                (false, true) => std::cmp::Ordering::Greater,
                                _ => a.symbol.cmp(&b.symbol),
                            }
                        });
                        info!(
                            count = auto.len(),
                            quote_asset = %cfg.quote_asset,
                            usdt_balance = usdt_bal,
                            usdc_balance = usdc_bal,
                            "auto-discovered symbols (spot): USDT prioritized (more balance)"
                        );
                    } else if usdc_bal >= cfg.min_quote_balance_usd && usdc_bal > usdt_bal {
                        auto.sort_by(|a, b| {
                            let a_usdc = a.quote_asset.eq_ignore_ascii_case("USDC");
                            let b_usdc = b.quote_asset.eq_ignore_ascii_case("USDC");
                            match (a_usdc, b_usdc) {
                                (true, false) => std::cmp::Ordering::Less,
                                (false, true) => std::cmp::Ordering::Greater,
                                _ => a.symbol.cmp(&b.symbol),
                            }
                        });
                        info!(
                            count = auto.len(),
                            quote_asset = %cfg.quote_asset,
                            usdt_balance = usdt_bal,
                            usdc_balance = usdc_bal,
                            "auto-discovered symbols (spot): USDC prioritized (more balance)"
                        );
                    } else {
                        auto.sort_by(|a, b| a.symbol.cmp(&b.symbol));
                        info!(
                            count = auto.len(),
                            quote_asset = %cfg.quote_asset,
                            usdt_balance = usdt_bal,
                            usdc_balance = usdc_bal,
                            "auto-discovered symbols (spot): equal balance or insufficient, alphabetical order"
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

    // Bakiye yetersizse, bakiye gelene kadar bekle (kod durmamalı)
    if selected.is_empty() {
        warn!(
            quote_asset = %cfg.quote_asset,
            min_required = cfg.min_quote_balance_usd,
            "no eligible symbols found - waiting for balance to become available"
        );
        
        // Bakiye gelene kadar döngüde bekle
        loop {
            use tokio::time::{sleep, Duration};
            sleep(Duration::from_secs(30)).await; // 30 saniye bekle
            
            // Tekrar sembol keşfi yap
            let mut retry_selected: Vec<SymbolMeta> = Vec::new();
            if cfg.auto_discover_quote {
                let want_group = is_usd_stable(&cfg.quote_asset);
                let mut retry_auto: Vec<SymbolMeta> = metadata
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
                
                // Bakiye kontrolü
                let mut retry_quote_balances: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
                if mode_lower == "futures" {
                    if let V::Fut(vtmp) = &venue {
                        let unique_quotes: std::collections::HashSet<String> = retry_auto.iter()
                            .map(|m| m.quote_asset.clone())
                            .collect();
                        for quote in unique_quotes {
                            let balance = vtmp.available_balance(&quote).await.ok()
                                .and_then(|b| b.to_f64())
                                .unwrap_or(0.0);
                            retry_quote_balances.insert(quote.clone(), balance);
                        }
                    }
                } else {
                    if let V::Spot(vtmp) = &venue {
                        let unique_quotes: std::collections::HashSet<String> = retry_auto.iter()
                            .map(|m| m.quote_asset.clone())
                            .collect();
                        for quote in unique_quotes {
                            rate_limit_guard().await;
                            let balance = vtmp.asset_free(&quote).await.ok()
                                .and_then(|b| b.to_f64())
                                .unwrap_or(0.0);
                            retry_quote_balances.insert(quote.clone(), balance);
                        }
                    }
                }
                
                retry_auto.retain(|m| {
                    if let Some(&balance) = retry_quote_balances.get(&m.quote_asset) {
                        balance >= cfg.min_quote_balance_usd
                    } else {
                        false
                    }
                });
                
                retry_selected = retry_auto;
            } else {
                // Manuel sembol listesi için de aynı kontrol
                for sym in &normalized {
                    if let Some(meta) = metadata.iter().find(|m| &m.symbol == sym) {
                        let exact_quote = meta.quote_asset.eq_ignore_ascii_case(&cfg.quote_asset);
                        let group_quote = is_usd_stable(&cfg.quote_asset) && is_usd_stable(&meta.quote_asset);
                        if !(exact_quote || group_quote) {
                            continue;
                        }
                        if let Some(status) = meta.status.as_deref() {
                            if status != "TRADING" {
                                continue;
                            }
                        }
                        if mode_lower == "futures" {
                            match meta.contract_type.as_deref() {
                                Some("PERPETUAL") => {}
                                _ => continue,
                            }
                        }
                        
                        // Bakiye kontrolü
                        let has_balance = if mode_lower == "futures" {
                            if let V::Fut(vtmp) = &venue {
                                vtmp.available_balance(&meta.quote_asset).await.ok()
                                    .and_then(|b| b.to_f64())
                                    .unwrap_or(0.0) >= cfg.min_quote_balance_usd
                            } else {
                                false
                            }
                        } else {
                            if let V::Spot(vtmp) = &venue {
                                rate_limit_guard().await;
                                vtmp.asset_free(&meta.quote_asset).await.ok()
                                    .and_then(|b| b.to_f64())
                                    .unwrap_or(0.0) >= cfg.min_quote_balance_usd
                            } else {
                                false
                            }
                        };
                        
                        if has_balance {
                            retry_selected.push(meta.clone());
                        }
                    }
                }
            }
            
            if !retry_selected.is_empty() {
                info!(
                    count = retry_selected.len(),
                    quote_asset = %cfg.quote_asset,
                    "balance became available, proceeding with symbol initialization"
                );
                selected = retry_selected;
                break; // Bakiye geldi, döngüden çık
            } else {
                info!(
                    quote_asset = %cfg.quote_asset,
                    min_required = cfg.min_quote_balance_usd,
                    "still waiting for balance to become available..."
                );
            }
        }
    }

    // Per-symbol metadata'yı başlangıçta çek (quantize için)
    info!("fetching per-symbol metadata for quantization...");
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
        
        // Per-symbol metadata çek (fallback: global cfg değerleri)
        let symbol_rules = match &venue {
            V::Spot(v) => v.rules_for(&meta.symbol).await.ok(),
            V::Fut(v) => v.rules_for(&meta.symbol).await.ok(),
        };
        if let Some(ref rules) = symbol_rules {
            info!(
                symbol = %meta.symbol,
                tick_size = %rules.tick_size,
                step_size = %rules.step_size,
                price_precision = rules.price_precision,
                qty_precision = rules.qty_precision,
                "fetched per-symbol metadata"
            );
        } else {
            warn!(
                symbol = %meta.symbol,
                "failed to fetch per-symbol metadata, will use global fallback"
            );
        }
        
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
            symbol_rules, // Per-symbol metadata (fallback: None, global cfg kullanılır)
            last_position_check: None,
            last_order_sync: None,
            order_fill_rate: 0.5, // Başlangıçta %50 varsay
            consecutive_no_fills: 0,
            position_entry_time: None,
            peak_pnl: Decimal::ZERO,
            position_hold_duration_ms: 0,
            last_order_price_update: HashMap::new(),
            // Gelişmiş risk ve kazanç takibi
            daily_pnl: Decimal::ZERO,
            total_funding_cost: Decimal::ZERO,
            position_size_notional_history: Vec::with_capacity(100),
            last_pnl_alert: None,
            cumulative_pnl: Decimal::ZERO,
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
                        // Reconnect sonrası ilk event geldiğinde sync trigger gönder
                        let mut first_event_after_reconnect = true;
                        while let Ok(event) = stream.next_event().await {
                            // Reconnect sonrası ilk event geldiğinde sync flag'i set et
                            if first_event_after_reconnect {
                                first_event_after_reconnect = false;
                                // Reconnect sonrası sync event'i gönder (main loop'ta handle edilecek)
                                let _ = tx.send(UserEvent::Heartbeat); // Heartbeat olarak kullan (sync trigger)
                            }
                            if tx.send(event).is_err() {
                                break;
                            }
                        }
                        warn!("user data stream reader exited, will reconnect and sync missed events");
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
    
    // Tick counter: Loop başında ve sonunda kullanılacak
    use std::sync::atomic::{AtomicU64, Ordering};
    static TICK_COUNTER: AtomicU64 = AtomicU64::new(0);
    
    info!(
        symbol_count = states.len(),
        tick_interval_ms = tick_ms,
        min_usd_per_order,
        "main trading loop starting"
    );
    
    // WebSocket reconnect sonrası missed events sync flag'i (loop dışında)
    let mut force_sync_all = false;
    
    loop {
        interval.tick().await;
        
        // DEBUG: Her tick'te log (ilk birkaç tick için)
        let tick_num = TICK_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
        if tick_num <= 5 || tick_num % 10 == 0 {
            info!(tick_num, "=== MAIN LOOP TICK START ===");
        }

        // WebSocket event'lerini işle
        while let Ok(event) = event_rx.try_recv() {
            match event {
                UserEvent::Heartbeat => {
                    // Reconnect sonrası sync trigger (WebSocket reconnect'ten sonra gönderilir)
                    force_sync_all = true;
                    info!("WebSocket reconnect detected, will sync all symbols on next tick");
                }
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
                        
                        // AKILLI TAKİP: Fill oranını güncelle
                        state.consecutive_no_fills = 0; // Fill oldu, sıfırla
                        // Fill oranı: Son 20 tick'te fill olan emir sayısı / toplam emir sayısı
                        // Basit yaklaşım: Her fill'de artır, her tick'te azalt
                        state.order_fill_rate = (state.order_fill_rate * 0.95 + 0.05).min(1.0);
                        
                        info!(
                            %symbol,
                            order_id = %order_id,
                            side = ?side,
                            qty = %qty.0,
                            new_inventory = %state.inv.0,
                            fill_rate = state.order_fill_rate,
                            "order filled: updating inventory and fill rate"
                        );
                    }
                }
                UserEvent::OrderCanceled { symbol, order_id } => {
                    if let Some(idx) = symbol_index.get(&symbol) {
                        let state = &mut states[*idx];
                        state.active_orders.remove(&order_id);
                        
                        // AKILLI TAKİP: Cancel oldu, fill oranını düşür
                        state.order_fill_rate = (state.order_fill_rate * 0.98).max(0.0);
                        
                        info!(
                            %symbol,
                            order_id = %order_id,
                            fill_rate = state.order_fill_rate,
                            "order canceled: updating fill rate"
                        );
                    }
                }
            }
        }

        // Ana trading loop: Her sembol için işlem yap
        // PERFORMANS: 571 sembol sıralı işlenirse çok uzun sürer, bu yüzden progress log ekliyoruz
        // NOT: effective_leverage config'den geliyor ve değişmiyor, bu yüzden loop başında hesaplamak mantıklı
        // Ama pozisyon, fiyat, funding rate gibi saniyelik değişen veriler her tick'te alınıyor (cache yok)
        let effective_leverage = cfg.leverage.unwrap_or(cfg.risk.max_leverage).max(1) as f64;
        let effective_leverage_ask = effective_leverage; // Aynı değer, tekrar hesaplamaya gerek yok
        
        // --- KRİTİK DÜZELTME: Bakiye kontrolü race condition'ını önle ---
        // Loop başında tüm unique quote asset'leri topla ve bakiyelerini bir kere çek
        let mut unique_quote_assets: std::collections::HashSet<String> = std::collections::HashSet::new();
        for state in states.iter() {
            unique_quote_assets.insert(state.meta.quote_asset.clone());
        }
        
        // Her unique quote asset için bakiyeyi bir kere çek ve cache'e al
        let mut quote_balances: HashMap<String, f64> = HashMap::new();
        for quote_asset in &unique_quote_assets {
            rate_limit_guard().await;
            let balance = match &venue {
                V::Spot(v) => {
                    match tokio::time::timeout(Duration::from_secs(5), v.asset_free(quote_asset)).await {
                        Ok(Ok(b)) => b.to_f64().unwrap_or(0.0),
                        _ => 0.0,
                    }
                }
                V::Fut(v) => {
                    match tokio::time::timeout(Duration::from_secs(5), v.available_balance(quote_asset)).await {
                        Ok(Ok(b)) => b.to_f64().unwrap_or(0.0),
                        _ => 0.0,
                    }
                }
            };
            quote_balances.insert(quote_asset.clone(), balance);
        }
        
        let mut processed_count = 0;
        let mut skipped_count = 0;
        let mut disabled_count = 0;
        let mut no_balance_count = 0;
        let total_symbols = states.len();
        let mut symbol_index = 0;
        
        for state in states.iter_mut() {
            symbol_index += 1;
            
            // Progress log: Her 50 sembolde bir veya ilk 10 sembolde
            if symbol_index <= 10 || symbol_index % 50 == 0 {
                info!(
                    progress = format!("{}/{}", symbol_index, total_symbols),
                    processed_so_far = processed_count,
                    skipped_so_far = skipped_count,
                    "processing symbols..."
                );
            }
            // PERFORMANS: Disabled sembolleri en başta filtrele (clone'dan önce)
            if state.disabled {
                skipped_count += 1;
                disabled_count += 1;
                continue;
            }
            
            // PERFORMANS: Clone'ları sadece gerektiğinde yap
            let symbol = &state.meta.symbol;
            let base_asset = &state.meta.base_asset;
            let quote_asset = &state.meta.quote_asset;

            // --- ERKEN BAKİYE KONTROLÜ: Bakiye yoksa gereksiz işlem yapma ---
            // KRİTİK DÜZELTME: Cache'den oku (race condition önlendi)
            // DEBUG: İlk birkaç sembol için detaylı log
            let is_debug_symbol = symbol_index <= 5 || symbol == "BTCUSDT" || symbol == "ETHUSDT" || symbol == "BNBUSDT";
            
            // Cache'den bakiye oku (loop başında çekildi)
            let q_free = quote_balances.get(quote_asset).copied().unwrap_or(0.0);
            
            let has_balance = match &venue {
                V::Spot(v) => {
                    // Spot için: quote veya base bakiyesi yeterli mi?
                    if is_debug_symbol {
                        info!(
                            %symbol,
                            quote_asset = %quote_asset,
                            available_balance = q_free,
                            min_required = min_usd_per_order,
                            "balance check for debug symbol (spot, from cache)"
                        );
                    }
                    
                    // HIZLI KONTROL: Config'deki minimum eşikten azsa işlem yapma
                    if q_free < cfg.min_quote_balance_usd {
                        if is_debug_symbol {
                            info!(
                                %symbol,
                                quote_asset = %quote_asset,
                                available_balance = q_free,
                                "SKIPPING: quote asset balance < 1 USD, will try other quote assets if available"
                            );
                        }
                        // Base bakiyesi kontrolü (yaklaşık kontrol, gerçek fiyat bilinmiyor)
                        // NOT: Base bakiyesi cache'de yok, bu yüzden sadece gerektiğinde çek
                        rate_limit_guard().await;
                        match tokio::time::timeout(Duration::from_secs(2), v.asset_free(base_asset)).await {
                            Ok(Ok(b)) => {
                                let b_free = b.to_f64().unwrap_or(0.0);
                                // Base varsa devam et (daha sonra gerçek fiyatla kontrol edilecek)
                                b_free >= cfg.qty_step
                            }
                            _ => false,
                        }
                    } else {
                        // Quote bakiyesi yeterliyse devam et
                        q_free >= min_usd_per_order
                    }
                }
                V::Fut(_) => {
                    // Futures için: leverage ile toplam kullanılabilir miktar
                    if is_debug_symbol {
                        info!(
                            %symbol,
                            quote_asset = %quote_asset,
                            available_balance = q_free,
                            min_required = min_usd_per_order,
                            "balance check for debug symbol (futures, from cache)"
                        );
                    }
                    if q_free < cfg.min_quote_balance_usd {
                        false // Bakiye çok düşük, skip
                    } else {
                        // Leverage ile toplam kullanılabilir miktar
                        let total = q_free * effective_leverage;
                        let has_enough = total >= min_usd_per_order;
                        if is_debug_symbol {
                            info!(
                                %symbol,
                                available_balance = q_free,
                                effective_leverage,
                                total_with_leverage = total,
                                min_required = min_usd_per_order,
                                has_enough,
                                "balance check result (from cache)"
                            );
                        }
                        has_enough
                    }
                }
            };
            
            // KRİTİK DÜZELTME: Bakiye yoksa bile açık pozisyon/emir varsa devam et
            // Önce açık pozisyon/emir kontrolü yap (bakiye kontrolünden önce)
            let has_open_position_or_orders = !state.active_orders.is_empty();
            
            // Eğer bakiye yoksa VE açık pozisyon/emir de yoksa, atla
            if !has_balance && !has_open_position_or_orders {
                // Bakiye yok ve açık pozisyon/emir yok, bu tick'i atla
                skipped_count += 1;
                no_balance_count += 1;
                continue;
            }
            
            // Bakiye yoksa ama açık pozisyon/emir varsa devam et (yönetmeye devam)
            if !has_balance && has_open_position_or_orders {
                info!(
                    %symbol,
                    active_orders = state.active_orders.len(),
                    "no balance but has open position/orders, continuing to manage them"
                );
            }
            
            processed_count += 1;

            // --- AKILLI EMİR SENKRONİZASYONU: API'den gerçek durumu kontrol et ---
            // ÖNEMLİ: Saniyelik değişimler olduğu için her tick'te kontrol etmeliyiz
            // Ama rate limit koruması için minimum 2 saniye bekle (çok sık API çağrısı yapma)
            // WebSocket zaten real-time güncellemeleri sağlıyor, API sadece doğrulama için
            // KRİTİK DÜZELTME: Reconnect sonrası tüm semboller için HEMEN sync yap
            // force_sync_all true ise hemen sync yap (reconnect sonrası), normal durumda 2 saniyede bir
            let should_sync_orders = if force_sync_all {
                true // Reconnect sonrası hemen sync - tüm semboller için
            } else {
                state.last_order_sync
                    .map(|last| last.elapsed().as_secs() >= 2) // Her 2 saniyede bir senkronize et (rate limit koruması + hızlı güncelleme)
                    .unwrap_or(true) // İlk çalıştırmada mutlaka senkronize et
            };
            if should_sync_orders {
                // API Rate Limit koruması
                rate_limit_guard().await;
                let sync_result = match &venue {
                    V::Spot(v) => v.get_open_orders(&symbol).await,
                    V::Fut(v) => v.get_open_orders(&symbol).await,
                };
                
                match sync_result {
                    Ok(api_orders) => {
                        // API'den gelen emirlerle local state'i senkronize et
                        let api_order_ids: std::collections::HashSet<String> = api_orders
                            .iter()
                            .map(|o| o.order_id.clone())
                            .collect();
                        
                        // Local'de olup API'de olmayan emirleri temizle (muhtemelen fill oldu)
                        let mut removed_count = 0;
                        state.active_orders.retain(|order_id, _| {
                            if !api_order_ids.contains(order_id) {
                                removed_count += 1;
                                false // Remove
                            } else {
                                true // Keep
                            }
                        });
                        
                        if removed_count > 0 {
                            // KRİTİK DÜZELTME: Fill olan emirler varsa consecutive_no_fills sıfırla
                            state.consecutive_no_fills = 0;
                            // Fill oranını artır
                            state.order_fill_rate = (state.order_fill_rate * 0.95 + 0.05 * (removed_count as f64).min(1.0)).min(1.0);
                            info!(
                                %symbol,
                                removed_orders = removed_count,
                                remaining_orders = state.active_orders.len(),
                                "synced orders: removed filled/canceled orders from local state"
                            );
                        }
                        
                        // API'de olup local'de olmayan emirleri ekle (başka yerden açılmış olabilir)
                        for api_order in &api_orders {
                            if !state.active_orders.contains_key(&api_order.order_id) {
                                let info = OrderInfo {
                                    order_id: api_order.order_id.clone(),
                                    side: api_order.side,
                                    price: api_order.price,
                                    qty: api_order.qty,
                                    created_at: Instant::now(), // Tahmini zaman
                                };
                                state.active_orders.insert(api_order.order_id.clone(), info);
                                info!(
                                    %symbol,
                                    order_id = %api_order.order_id,
                                    side = ?api_order.side,
                                    "found new order from API (not in local state)"
                                );
                            }
                        }
                        
                        state.last_order_sync = Some(Instant::now());
                    }
                    Err(err) => {
                        warn!(%symbol, ?err, "failed to sync orders from API, continuing with local state");
                    }
                }
            }
            
            // Reconnect sonrası sync yapıldı, flag'i sıfırla (sadece son sembol işlendikten sonra)
            if force_sync_all && symbol_index == total_symbols {
                force_sync_all = false;
                info!("WebSocket reconnect sync completed for all symbols");
            }
            
            // --- AKILLI EMİR YÖNETİMİ: Stale emirleri iptal et ---
            // Not: WebSocket event'leri bu noktada hala gelebilir, bu normaldir
            if !state.active_orders.is_empty() {
                let existing_orders: Vec<OrderInfo> =
                    state.active_orders.values().cloned().collect();
                let mut stale_count = 0;
                let mut canceled_count = 0;
                
                for order in &existing_orders {
                    let age_ms = order.created_at.elapsed().as_millis() as u64;
                    let stale = age_ms > cfg.exec.max_order_age_ms;
                    
                    if stale {
                        stale_count += 1;
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
                        // API Rate Limit koruması
                        rate_limit_guard().await;
                        match &venue {
                            V::Spot(v) => {
                                if v.cancel(&order.order_id, &symbol).await.is_ok() {
                                    canceled_count += 1;
                                    state.active_orders.remove(&order.order_id);
                                } else {
                                    warn!(%symbol, order_id = %order.order_id, "failed to cancel stale spot order");
                                }
                            }
                            V::Fut(v) => {
                                if v.cancel(&order.order_id, &symbol).await.is_ok() {
                                    canceled_count += 1;
                                    state.active_orders.remove(&order.order_id);
                                } else {
                                    warn!(%symbol, order_id = %order.order_id, "failed to cancel stale futures order");
                                }
                            }
                        }
                    }
                }
                
                if stale_count > 0 {
                    info!(
                        %symbol,
                        stale_orders = stale_count,
                        canceled_orders = canceled_count,
                        remaining_orders = state.active_orders.len(),
                        "cleaned up stale orders"
                    );
                }
                
                // Eğer çok fazla stale emir varsa, hepsini temizle
                if stale_count > 0 && state.active_orders.len() > 10 {
                    warn!(
                        %symbol,
                        total_orders = state.active_orders.len(),
                        "too many active orders, canceling all to reset"
                    );
                    // API Rate Limit koruması
                    rate_limit_guard().await;
                    match &venue {
                        V::Spot(v) => {
                            if let Err(err) = v.cancel_all(&symbol).await {
                                warn!(%symbol, ?err, "failed to cancel all orders");
                            } else {
                                state.active_orders.clear();
                            }
                        }
                        V::Fut(v) => {
                            if let Err(err) = v.cancel_all(&symbol).await {
                                warn!(%symbol, ?err, "failed to cancel all orders");
                            } else {
                                state.active_orders.clear();
                            }
                        }
                    }
                }
            }

            // API Rate Limit koruması
            rate_limit_guard().await;
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
            
            // Pozisyon bilgisini al (bir kere, tüm analizler için kullanılacak)
            // KRİTİK DÜZELTME: Bakiye yoksa bile pozisyon kontrolü yap (açık pozisyon olabilir)
            // API Rate Limit koruması
            rate_limit_guard().await;
            let pos = match &venue {
                V::Spot(v) => match v.get_position(&symbol).await {
                    Ok(pos) => pos,
                    Err(err) => {
                        // Pozisyon fetch hatası: Eğer açık emir varsa devam et, yoksa atla
                        if state.active_orders.is_empty() && !has_balance {
                            warn!(%symbol, ?err, "failed to fetch position, no open orders, and no balance, skipping tick");
                            continue;
                        } else {
                            warn!(%symbol, ?err, "failed to fetch position but has open orders/balance, continuing with default position");
                            // Default pozisyon (qty=0) kullan
                            Position {
                                symbol: symbol.clone(),
                                qty: Qty(Decimal::ZERO),
                                entry: Px(Decimal::ZERO),
                                leverage: 1,
                                liq_px: None,
                            }
                        }
                    }
                },
                V::Fut(v) => match v.get_position(&symbol).await {
                    Ok(pos) => pos,
                    Err(err) => {
                        // Pozisyon fetch hatası: Eğer açık emir varsa devam et, yoksa atla
                        if state.active_orders.is_empty() && !has_balance {
                            warn!(%symbol, ?err, "failed to fetch position, no open orders, and no balance, skipping tick");
                            continue;
                        } else {
                            warn!(%symbol, ?err, "failed to fetch position but has open orders/balance, continuing with default position");
                            // Default pozisyon (qty=0) kullan
                            Position {
                                symbol: symbol.clone(),
                                qty: Qty(Decimal::ZERO),
                                entry: Px(Decimal::ZERO),
                                leverage: 1,
                                liq_px: None,
                            }
                        }
                    }
                },
            };
            
            // KRİTİK DÜZELTME: Pozisyon varsa (qty != 0) veya açık emir varsa, bakiye kontrolünü atla
            let has_position = !pos.qty.0.is_zero();
            if has_position && !has_balance {
                info!(
                    %symbol,
                    position_qty = %pos.qty.0,
                    "has open position but no balance, continuing to manage position"
                );
            }
            
            // Mark price ve funding rate'i al (bir kere, tüm analizler için kullanılacak)
            // API Rate Limit koruması
            rate_limit_guard().await;
            let (mark_px, funding_rate, next_funding_time) = match &venue {
                V::Spot(v) => match v.mark_price(&symbol).await {
                    Ok(px) => (px, None, None),
                    Err(_) => {
                        // Fallback: bid/ask mid price
                        let mid = (bid.0 + ask.0) / Decimal::from(2u32);
                        (Px(mid), None, None)
                    }
                },
                V::Fut(v) => match v.fetch_premium_index(&symbol).await {
                    Ok((mark, funding, next_time)) => (mark, funding, next_time),
                    Err(_) => {
                        // Fallback: bid/ask mid price
                        let mid = (bid.0 + ask.0) / Decimal::from(2u32);
                        (Px(mid), None, None)
                    }
                },
            };
            
            // Pozisyon boyutu hesapla (order analizi ve pozisyon analizi için kullanılacak)
            let position_size_notional = (mark_px.0 * pos.qty.0.abs()).to_f64().unwrap_or(0.0);
            
            // --- AKILLI EMİR ANALİZİ: Mevcut emirleri zeka ile değerlendir ---
            // 1. Emir fiyatlarını market ile karşılaştır
            // 2. Çok uzakta olan emirleri iptal et veya güncelle
            // 3. Stale emirleri temizle
            if !state.active_orders.is_empty() {
                let mut orders_to_cancel: Vec<String> = Vec::new();
                
                for (order_id, order) in &state.active_orders {
                    let order_price_f64 = order.price.0.to_f64().unwrap_or(0.0);
                    let order_age_ms = order.created_at.elapsed().as_millis() as u64;
                    
                    // Market fiyatı ile karşılaştır
                    let market_distance_pct = match order.side {
                        Side::Buy => {
                            // Bid emri: ask'ten ne kadar uzakta?
                            let ask_f64 = ask.0.to_f64().unwrap_or(0.0);
                            if ask_f64 > 0.0 {
                                (ask_f64 - order_price_f64) / ask_f64
                            } else {
                                0.0
                            }
                        }
                        Side::Sell => {
                            // Ask emri: bid'den ne kadar uzakta?
                            let bid_f64 = bid.0.to_f64().unwrap_or(0.0);
                            if bid_f64 > 0.0 {
                                (order_price_f64 - bid_f64) / bid_f64
                            } else {
                                0.0
                            }
                        }
                    };
                    
                    // Akıllı karar: Emir çok uzakta mı?
                    // Pozisyon varsa daha toleranslı ol (pozisyon kapatmak için emir gerekebilir)
                    let max_distance_pct = if position_size_notional > 0.0 {
                        0.01 // %1 (pozisyon varsa daha toleranslı)
                    } else {
                        0.005 // %0.5 (pozisyon yoksa daha sıkı)
                    };
                    let should_cancel_far = market_distance_pct.abs() > max_distance_pct;
                    
                    // Akıllı karar: Emir çok eski mi?
                    // Pozisyon varsa stale emirleri daha hızlı temizle (pozisyon yönetimi için)
                    let max_age_for_stale = if position_size_notional > 0.0 {
                        cfg.exec.max_order_age_ms / 2 // Pozisyon varsa yarı süre
                    } else {
                        cfg.exec.max_order_age_ms
                    };
                    let should_cancel_stale = order_age_ms > max_age_for_stale;
                    
                    // Akıllı karar: Fiyat değişti mi? (son güncellemeden beri)
                    let price_changed = state.last_order_price_update
                        .get(order_id)
                        .map(|last_px| {
                            let price_diff = (order.price.0 - last_px.0).abs();
                            let price_diff_pct = if order.price.0 > Decimal::ZERO {
                                price_diff / order.price.0
                            } else {
                                Decimal::ZERO
                            };
                            price_diff_pct > Decimal::new(1, 4) // %0.01'den fazla değişmiş
                        })
                        .unwrap_or(true); // İlk kez görülüyorsa güncelle
                    
                    if should_cancel_far || should_cancel_stale {
                        orders_to_cancel.push(order_id.clone());
                        info!(
                            %symbol,
                            order_id = %order_id,
                            side = ?order.side,
                            order_price = %order.price.0,
                            market_distance_pct = market_distance_pct * 100.0,
                            order_age_ms,
                            reason = if should_cancel_far { "too_far_from_market" } else { "stale" },
                            "intelligent order analysis: canceling order"
                        );
                    } else if price_changed && order_age_ms > 5_000 {
                        // Fiyat değişti ve emir 5 saniyeden eski, güncelleme öner
                        // (Strateji yeni fiyat üretecek, bu sadece bilgilendirme)
                        info!(
                            %symbol,
                            order_id = %order_id,
                            side = ?order.side,
                            order_price = %order.price.0,
                            market_distance_pct = market_distance_pct * 100.0,
                            "intelligent order analysis: order price may need update"
                        );
                    }
                }
                
                // İptal edilecek emirleri iptal et (STAGGER: Her iptal arasında kısa gecikme)
                let stagger_delay_ms = 50; // Her iptal arasında 50ms bekle
                for (idx, order_id) in orders_to_cancel.iter().enumerate() {
                    if idx > 0 {
                        // İlk iptal hariç, her iptal arasında bekle (stagger)
                        tokio::time::sleep(Duration::from_millis(stagger_delay_ms)).await;
                    }
                    // API Rate Limit koruması
                    rate_limit_guard().await;
                    match &venue {
                        V::Spot(v) => {
                            if let Err(err) = v.cancel(order_id, &symbol).await {
                                warn!(%symbol, order_id = %order_id, ?err, "failed to cancel order");
                            } else {
                                state.active_orders.remove(order_id);
                                state.last_order_price_update.remove(order_id);
                            }
                        }
                        V::Fut(v) => {
                            if let Err(err) = v.cancel(order_id, &symbol).await {
                                warn!(%symbol, order_id = %order_id, ?err, "failed to cancel order");
                            } else {
                                state.active_orders.remove(order_id);
                                state.last_order_price_update.remove(order_id);
                            }
                        }
                    }
                }
            }
            
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

            // Pozisyon ve mark price zaten yukarıda alındı, tekrar almayalım
            // Envanter senkronizasyonu yap
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

            record_pnl_snapshot(&mut state.pnl_history, &pos, mark_px);
            
            // --- AKILLI POZİSYON ANALİZİ: Durumu detaylı incele ---
            let current_pnl = (mark_px.0 - pos.entry.0) * pos.qty.0;
            let pnl_f64 = current_pnl.to_f64().unwrap_or(0.0);
            // position_size_notional zaten yukarıda hesaplandı, tekrar hesaplamaya gerek yok
            
            // --- GELİŞMİŞ RİSK VE KAZANÇ TAKİBİ: Detaylı analiz ---
            
            // 1. Günlük PnL takibi (basit: her tick'te güncelle, reset mekanizması eklenebilir)
            state.daily_pnl = current_pnl; // Şimdilik current PnL, ileride günlük reset eklenebilir
            
            // 2. Funding cost takibi (futures için)
            if let Some(funding) = funding_rate {
                let funding_cost = funding * position_size_notional;
                state.total_funding_cost += Decimal::from_f64_retain(funding_cost).unwrap_or(Decimal::ZERO);
            }
            
            // 3. Pozisyon boyutu geçmişi (risk analizi için)
            state.position_size_notional_history.push(position_size_notional);
            if state.position_size_notional_history.len() > 100 {
                state.position_size_notional_history.remove(0);
            }
            
            // 4. Kümülatif PnL takibi
            state.cumulative_pnl = current_pnl; // Şimdilik current, ileride gerçek kümülatif hesaplanabilir
            
            // 5. Pozisyon boyutu risk kontrolü: Çok büyük pozisyonlar riskli
            let max_position_size_usd = cfg.max_usd_per_order * effective_leverage * 5.0; // 5x buffer
            if position_size_notional > max_position_size_usd {
                warn!(
                    %symbol,
                    position_size_notional,
                    max_allowed = max_position_size_usd,
                    "POSITION SIZE RISK: position too large, force closing"
                );
                // KRİTİK DÜZELTME: Risk limiti aşılınca pozisyonu kapat
                rate_limit_guard().await;
                match &venue {
                    V::Spot(v) => {
                        if let Err(err) = v.close_position(&symbol).await {
                            error!(%symbol, ?err, "failed to close position due to size risk");
                        } else {
                            info!(%symbol, "closed position due to size risk");
                        }
                    }
                    V::Fut(v) => {
                        if let Err(err) = v.close_position(&symbol).await {
                            error!(%symbol, ?err, "failed to close position due to size risk");
                        } else {
                            info!(%symbol, "closed position due to size risk");
                        }
                    }
                }
                continue; // Bu tick'i atla
            }
            
            // 6. Real-time PnL alerts: Kritik seviyelerde uyarı
            // ÖNEMLİ: PnL her tick'te kontrol ediliyor, sadece alert spam'ini önle
            let pnl_alert_threshold_positive = 0.05; // %5 kar
            let pnl_alert_threshold_negative = -0.03; // %3 zarar
            let should_alert = state.last_pnl_alert
                .map(|last| last.elapsed().as_secs() >= 10) // Her 10 saniyede bir alert (spam önleme, ama hızlı tepki)
                .unwrap_or(true);
            
            if should_alert {
                if pnl_f64 > 0.0 && position_size_notional > 0.0 {
                    let pnl_pct = pnl_f64 / position_size_notional;
                    if pnl_pct >= pnl_alert_threshold_positive {
                        info!(
                            %symbol,
                            pnl = pnl_f64,
                            pnl_pct = pnl_pct * 100.0,
                            position_size = position_size_notional,
                            "PNL ALERT: Significant profit achieved"
                        );
                        state.last_pnl_alert = Some(Instant::now());
                    }
                }
                if pnl_f64 < 0.0 && position_size_notional > 0.0 {
                    let pnl_pct = pnl_f64 / position_size_notional;
                    if pnl_pct <= pnl_alert_threshold_negative {
                        warn!(
                            %symbol,
                            pnl = pnl_f64,
                            pnl_pct = pnl_pct * 100.0,
                            position_size = position_size_notional,
                            "PNL ALERT: Significant loss detected"
                        );
                        state.last_pnl_alert = Some(Instant::now());
                    }
                }
            }
            
            // Peak PnL takibi: En yüksek karı kaydet (kar al için)
            if current_pnl > state.peak_pnl {
                state.peak_pnl = current_pnl;
            }
            
            // Pozisyon tutma süresi takibi
            if pos.qty.0.abs() > Decimal::new(1, 8) {
                // Pozisyon var
                if state.position_entry_time.is_none() {
                    state.position_entry_time = Some(Instant::now());
                }
                if let Some(entry_time) = state.position_entry_time {
                    state.position_hold_duration_ms = entry_time.elapsed().as_millis() as u64;
                }
            } else {
                // Pozisyon yok, sıfırla
                state.position_entry_time = None;
                state.peak_pnl = Decimal::ZERO;
                state.position_hold_duration_ms = 0;
            }
            
            // Pozisyon trend analizi: Son 10 snapshot'a bak
            let pnl_trend = if state.pnl_history.len() >= 10 {
                let recent = &state.pnl_history[state.pnl_history.len().saturating_sub(10)..];
                let first = recent[0];
                let last = recent[recent.len() - 1];
                if first > Decimal::ZERO {
                    ((last - first) / first).to_f64().unwrap_or(0.0)
                } else {
                    0.0
                }
            } else {
                0.0
            };
            
            // --- AKILLI POZİSYON YÖNETİMİ: Kar al / Zarar durdur mantığı ---
            let entry_price_f64 = pos.entry.0.to_f64().unwrap_or(0.0);
            let mark_price_f64 = mark_px.0.to_f64().unwrap_or(0.0);
            let position_qty_f64 = pos.qty.0.to_f64().unwrap_or(0.0);
            
            // Pozisyon varsa akıllı karar ver
            if position_qty_f64.abs() > 0.0001 && entry_price_f64 > 0.0 && mark_price_f64 > 0.0 {
                let price_change_pct = if pos.qty.0.is_sign_positive() {
                    // Long pozisyon: fiyat artışı = kar
                    (mark_price_f64 - entry_price_f64) / entry_price_f64
                } else {
                    // Short pozisyon: fiyat düşüşü = kar
                    (entry_price_f64 - mark_price_f64) / entry_price_f64
                };
                
                // Kar al mantığı: Daha büyük kazançlar için optimize edildi
                // Küçük kazançlar için erken kar alma, büyük kazançlar için daha uzun tut
                let take_profit_threshold_small = 0.01; // %1 kar (küçük pozisyonlar için)
                let take_profit_threshold_large = 0.05; // %5 kar (büyük pozisyonlar/fırsat modu için)
                // Dinamik trailing stop: Kar büyüdükçe genişlet
                let base_trailing_stop_threshold = 0.02; // %2 base trailing stop
                
                // Pozisyon boyutuna göre eşik seç
                let is_large_position = position_size_notional > 200.0; // 200 USD'den büyük = büyük pozisyon
                let take_profit_threshold = if is_large_position {
                    take_profit_threshold_large // Büyük pozisyonlar için daha yüksek eşik
                } else {
                    take_profit_threshold_small // Küçük pozisyonlar için düşük eşik
                };
                
                let should_take_profit = if price_change_pct >= take_profit_threshold {
                    // Kar var, trend analizi yap
                    if pnl_trend < -0.15 {
                        // Trend tersine dönüyor (%15'ten fazla), kar al
                        true
                    } else if state.position_hold_duration_ms > 600_000 && price_change_pct < 0.10 {
                        // 10 dakikadan fazla tutuldu ve %10'dan az kar varsa, kısmi kar al
                        true
                    } else if price_change_pct >= 0.10 {
                        // %10+ kar varsa, trend hala iyiyse tut (daha büyük kazançlar için)
                        // Sadece trend çok kötüyse kar al
                        pnl_trend < -0.20
                    } else {
                        false
                    }
                } else {
                    false
                };
                
                // Dinamik trailing stop: Peak'ten düşerse kapat, kar büyüdükçe genişlet
                let peak_pnl_f64 = state.peak_pnl.to_f64().unwrap_or(0.0);
                let current_pnl_f64 = current_pnl.to_f64().unwrap_or(0.0);
                let should_trailing_stop = if peak_pnl_f64 > 0.0 && current_pnl_f64 < peak_pnl_f64 {
                    let drawdown_from_peak = (peak_pnl_f64 - current_pnl_f64) / peak_pnl_f64.abs().max(0.01);
                    // Dinamik trailing stop: Kar büyüdükçe genişlet
                    let trailing_stop = if peak_pnl_f64 > 50.0 {
                        0.05 // %5 (büyük karlar için)
                    } else if peak_pnl_f64 > 20.0 {
                        0.03 // %3 (orta karlar için)
                    } else {
                        base_trailing_stop_threshold // %2 (küçük karlar için)
                    };
                    drawdown_from_peak >= trailing_stop
                } else {
                    false
                };
                
                // Peak PnL güncelle
                if current_pnl_f64 > peak_pnl_f64 {
                    state.peak_pnl = current_pnl;
                }
                
                // Zarar durdur: %1'den fazla zarar varsa ve trend kötüleşiyorsa kapat
                let stop_loss_threshold = -0.01; // %1 zarar
                let should_stop_loss = if price_change_pct <= stop_loss_threshold {
                    // Zarar var, trend analizi yap
                    if pnl_trend < -0.2 || state.position_hold_duration_ms > 600_000 {
                        // Trend kötüleşiyor veya 10 dakikadan fazla zararda, kapat
                        true
                    } else {
                        false
                    }
                } else {
                    false
                };
                
                // Akıllı karar: Pozisyonu kapat
                if should_take_profit || should_trailing_stop || should_stop_loss {
                    let reason = if should_take_profit {
                        "take_profit"
                    } else if should_trailing_stop {
                        "trailing_stop"
                    } else {
                        "stop_loss"
                    };
                    
                    warn!(
                        %symbol,
                        reason,
                        current_pnl = pnl_f64,
                        price_change_pct = price_change_pct * 100.0,
                        peak_pnl = peak_pnl_f64,
                        position_hold_duration_ms = state.position_hold_duration_ms,
                        pnl_trend,
                        "intelligent position management: closing position"
                    );
                    
                    // Pozisyonu kapat: Tüm emirleri iptal et, pozisyonu kapat
                    // API Rate Limit koruması
                    rate_limit_guard().await;
                    let cancel_result = match &venue {
                        V::Spot(v) => v.cancel_all(&symbol).await,
                        V::Fut(v) => v.cancel_all(&symbol).await,
                    };
                    if let Err(err) = cancel_result {
                        warn!(%symbol, ?err, "failed to cancel orders before position close");
                    }
                    
                    // Pozisyonu kapat (reduceOnly market order) - KRİTİK DÜZELTME: Retry mekanizması
                    let mut position_closed = false;
                    for attempt in 0..3 {
                        // API Rate Limit koruması
                        rate_limit_guard().await;
                        let close_result = match &venue {
                            V::Spot(v) => v.close_position(&symbol).await,
                            V::Fut(v) => v.close_position(&symbol).await,
                        };
                        match close_result {
                            Ok(_) => {
                                position_closed = true;
                                info!(
                                    %symbol,
                                    reason,
                                    final_pnl = pnl_f64,
                                    attempt = attempt + 1,
                                    "position closed successfully"
                                );
                                break;
                            }
                            Err(err) if attempt < 2 => {
                                warn!(%symbol, ?err, attempt = attempt + 1, "failed to close position, retrying...");
                                tokio::time::sleep(Duration::from_millis(100)).await;
                            }
                            Err(err) => {
                                warn!(%symbol, ?err, "failed to close position after 3 attempts");
                            }
                        }
                    }
                    
                    if !position_closed {
                        warn!(%symbol, reason, "position close failed after retries, state will be reset anyway");
                    }
                    
                    // State'i sıfırla
                    state.position_entry_time = None;
                    state.peak_pnl = Decimal::ZERO;
                    state.position_hold_duration_ms = 0;
                    state.daily_pnl = Decimal::ZERO; // Pozisyon kapandı, günlük PnL sıfırla
                }
            }
            
            // Pozisyon durumu logla (sadece önemli değişikliklerde)
            // ÖNEMLİ: Pozisyon analizi her tick'te yapılıyor, sadece log sıklığını azalt
            // Log spam'ini önlemek için 30 saniyede bir log (ama analiz her tick'te)
            let should_log_position = state.last_position_check
                .map(|last| last.elapsed().as_secs() >= 30) // Her 30 saniyede bir log (log spam'ini önle)
                .unwrap_or(true);
            
            if should_log_position {
                info!(
                    %symbol,
                    position_qty = %pos.qty.0,
                    entry_price = %pos.entry.0,
                    mark_price = %mark_px.0,
                    current_pnl = pnl_f64,
                    position_size_notional = position_size_notional,
                    pnl_trend = pnl_trend,
                    active_orders = state.active_orders.len(),
                    order_fill_rate = state.order_fill_rate,
                    consecutive_no_fills = state.consecutive_no_fills,
                    "position status: monitoring for intelligent decisions"
                );
                state.last_position_check = Some(Instant::now());
            }

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
            
            // --- AKILLI FILL ORANI TAKİBİ: Ardışık fill olmayan tick sayısını artır ---
            // KRİTİK DÜZELTME: Sadece emir varsa ve fill olmadıysa artır
            // (Fill kontrolü yukarıda yapıldı, burada sadece artırma yapıyoruz)
            if state.active_orders.len() > 0 {
                state.consecutive_no_fills += 1;
                // Eğer çok uzun süre fill olmuyorsa, fill oranını düşür
                if state.consecutive_no_fills > 10 {
                    state.order_fill_rate = (state.order_fill_rate * 0.99).max(0.1);
                }
            } else {
                // Emir yoksa, consecutive_no_fills sıfırla ve fill oranını yavaşça normale döndür
                state.consecutive_no_fills = 0;
                state.order_fill_rate = (state.order_fill_rate * 0.995 + 0.005).min(1.0);
            }
            
            // --- AKILLI POZİSYON YÖNETİMİ: Fill oranına göre strateji ayarla ---
            // Eğer fill oranı çok düşükse (emirler doldurulmuyor), spread'i genişlet veya fiyatı ayarla
            let fill_rate_threshold = 0.2; // %20'nin altındaysa sorun var
            if state.order_fill_rate < fill_rate_threshold && state.active_orders.len() > 0 {
                warn!(
                    %symbol,
                    fill_rate = state.order_fill_rate,
                    active_orders = state.active_orders.len(),
                    consecutive_no_fills = state.consecutive_no_fills,
                    "low fill rate detected: orders may be too far from market"
                );
            }

            if matches!(risk_action, RiskAction::Halt) {
                warn!(%symbol, "risk halt triggered, cancelling and flattening");
                // API Rate Limit koruması
                rate_limit_guard().await;
                match &venue {
                    V::Spot(v) => {
                        if let Err(err) = v.cancel_all(&symbol).await {
                            warn!(%symbol, ?err, "failed to cancel all orders during halt");
                        }
                        rate_limit_guard().await;
                        if let Err(err) = v.close_position(&symbol).await {
                            warn!(%symbol, ?err, "failed to close position during halt");
                        }
                    }
                    V::Fut(v) => {
                        if let Err(err) = v.cancel_all(&symbol).await {
                            warn!(%symbol, ?err, "failed to cancel all orders during halt");
                        }
                        rate_limit_guard().await;
                        if let Err(err) = v.close_position(&symbol).await {
                            warn!(%symbol, ?err, "failed to close position during halt");
                        }
                    }
                }
                continue;
            }

            // Per-symbol tick_size'ı Context'e geç (crossing guard için)
            let tick_size_f64 = get_price_tick(state.symbol_rules.as_ref(), cfg.price_tick);
            let tick_size_decimal = Decimal::from_f64_retain(tick_size_f64);
            
            let ctx = Context {
                ob,
                sigma: 0.5,
                inv: state.inv,
                liq_gap_bps,
                funding_rate,
                next_funding_time,
                mark_price: mark_px, // Mark price stratejiye veriliyor
                tick_size: tick_size_decimal, // Per-symbol tick_size (crossing guard için)
            };
            let mut quotes = state.strategy.on_tick(&ctx);
            // Debug: Strateji neden quote üretmedi?
            if quotes.bid.is_none() && quotes.ask.is_none() {
                use tracing::debug;
                debug!(
                    %symbol,
                    ?risk_action,
                    inventory = %state.inv.0,
                    liq_gap_bps,
                    "strategy produced no quotes - investigating reason"
                );
            }
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
                    rate_limit_guard().await;
                    let quote_free = match v.asset_free(&quote_asset).await {
                        Ok(q) => {
                            let q_f64 = q.to_f64().unwrap_or(0.0);
                            // HIZLI KONTROL: Config'deki minimum eşikten azsa işlem yapma
                            // Her sembol kendi quote asset'ini kullanır (BTCUSDT → USDT, BTCUSDC → USDC)
                            // Eğer o quote asset'te yeterli bakiye yoksa, bu sembolü skip et
                            if q_f64 < cfg.min_quote_balance_usd {
                                info!(
                                    %symbol,
                                    quote_asset = %quote_asset,
                                    available_balance = q_f64,
                                    min_required = cfg.min_quote_balance_usd,
                                    "SKIPPING: quote asset balance below minimum threshold, will try other quote assets if available"
                                );
                                0.0
                            } else {
                                q_f64
                            }
                        },
                        Err(err) => {
                            warn!(%symbol, ?err, "failed to fetch quote asset balance, using zero");
                            0.0
                        }
                    };
                    rate_limit_guard().await;
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
                            // HIZLI KONTROL: Config'deki minimum eşikten azsa işlem yapma
                            // Her sembol kendi quote asset'ini kullanır (BTCUSDT → USDT, BTCUSDC → USDC)
                            // Eğer o quote asset'te yeterli bakiye yoksa, bu sembolü skip et
                            if avail_f64 < cfg.min_quote_balance_usd {
                                info!(
                                    %symbol,
                                    quote_asset = %quote_asset,
                                    available_balance = avail_f64,
                                    min_required = cfg.min_quote_balance_usd,
                                    "SKIPPING: quote asset balance below minimum threshold, will try other quote assets if available"
                                );
                                0.0 // Bakiye çok düşük, bu sembolü skip et
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
                    // NOT: effective_leverage config'den geliyor ve değişmiyor, loop başında hesaplanan değeri kullan
                    // (Futures için leverage sembol bazında değişmez, config'den gelir)
                    
                    // MEVCUT POZİSYONLARIN MARGİN'İNİ ÇIKAR: Mevcut pozisyon varsa, onun margin'ini hesaptan çıkar
                    // Mevcut pozisyonun margin'i = pozisyon notional / leverage
                    // position_size_notional zaten yukarıda hesaplandı (satır 1242)
                    let existing_position_margin = if position_size_notional > 0.0 {
                        position_size_notional / effective_leverage
                    } else {
                        0.0
                    };
                    let available_after_position = (avail - existing_position_margin).max(0.0);
                    
                    // ÖNEMLİ: Hesaptan giden para mantığı:
                    // - 20 USD varsa → 20 USD kullanılır (tamamı)
                    // - 100 USD varsa → 100 USD kullanılır (tamamı)
                    // - 200 USD varsa → 100 USD kullanılır (max limit), kalan 100 başka semboller için
                    // Leverage sadece pozisyon boyutunu belirler, hesaptan giden parayı etkilemez
                    // Örnek: 20 USD bakiye, 20x leverage → hesaptan 20 USD gider, pozisyon 400 USD olur
                    // Örnek: 200 USD bakiye, 20x leverage → hesaptan 100 USD gider (max limit), pozisyon 2000 USD olur
                    // MEVCUT POZİSYON DİKKATE ALINARAK: Mevcut pozisyonun margin'i çıkarıldıktan sonra kalan bakiye kullanılır
                    let max_usable_from_account = available_after_position.min(cfg.max_usd_per_order);
                    
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
                        existing_position_margin,
                        available_after_position,
                        effective_leverage,
                        max_usable_from_account,
                        position_size_with_leverage,
                        per_order_limit_margin_usd = per_order_cap_margin,
                        per_order_limit_notional_usd = per_order_notional,
                        "calculated futures caps: max_usable_from_account is max USD that will leave your account, leverage only affects position size (existing position margin deducted)"
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

            // --- QUOTE ASSET BAKİYE KONTROLÜ: Eğer quote asset'te bakiye yoksa skip et ---
            // Her sembol kendi quote asset'ini kullanır (BTCUSDT → USDT, BTCUSDC → USDC)
            // Eğer o quote asset'te bakiye yoksa (config'deki eşikten az), bu sembolü skip et
            // Diğer quote asset'li semboller (örn: BTCUSDC) devam edebilir
            if caps.buy_total < cfg.min_quote_balance_usd {
                info!(
                    %symbol,
                    quote_asset = %quote_asset,
                    buy_total = caps.buy_total,
                    min_required = cfg.min_quote_balance_usd,
                    "SKIPPING SYMBOL: quote asset balance below minimum threshold, will try other quote assets if available"
                );
                continue; // Bu sembolü skip et, diğer quote asset'li sembollere devam et
            }

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

            // Per-symbol metadata kullan (fallback: global cfg)
            let qty_step_f64 = get_qty_step(state.symbol_rules.as_ref(), cfg.qty_step);
            let qty_step_dec = Decimal::from_f64_retain(qty_step_f64).unwrap_or(Decimal::ZERO);
            
            // QTY CLAMP SIRASI GARANTİSİ: 1) USD clamp, 2) Base clamp (spot sell), 3) Quantize, 4) Min notional check
            // min_usd_per_order > 0 doğrulaması zaten yukarıda yapıldı, burada sadece notional kontrolü yapıyoruz
            
            // KRİTİK DÜZELTME: Bakiye yoksa ama pozisyon/emir varsa, yeni emir verme (sadece mevcut pozisyon/emirleri yönet)
            // Pozisyon/emir yönetimi yukarıda yapıldı, burada sadece yeni emir verme kontrolü
            let should_place_new_orders = has_balance || has_position || has_open_position_or_orders;
            if !should_place_new_orders {
                info!(
                    %symbol,
                    "no balance, no position, no open orders - skipping new order placement"
                );
                // Yeni emir verme, ama mevcut pozisyon/emir yönetimi yukarıda yapıldı
                quotes.bid = None;
                quotes.ask = None;
            }

            if let Some((px, q)) = quotes.bid {
                if px.0 <= Decimal::ZERO {
                    warn!(%symbol, ?px, "dropping bid quote with non-positive price");
                    quotes.bid = None;
                } else {
                    // 1. USD clamp
                    let nq = clamp_qty_by_usd(q, px, caps.buy_notional, qty_step_f64);
                    // 2. Quantize kontrolü
                    let quantized_to_zero = qty_step_dec > Decimal::ZERO
                        && nq.0 < qty_step_dec
                        && nq.0 != Decimal::ZERO;
                    // 3. Min notional kontrolü (min_usd_per_order > 0 garantisi yukarıda)
                    let notional = px.0.to_f64().unwrap_or(0.0) * nq.0.to_f64().unwrap_or(0.0);
                    if nq.0 == Decimal::ZERO
                        || quantized_to_zero
                        || (min_usd_per_order > 0.0 && notional < min_usd_per_order)
                    {
                        info!(
                            %symbol,
                            ?px,
                            original_qty = ?q,
                            qty_step = qty_step_f64,
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
                    // QTY CLAMP SIRASI GARANTİSİ: 1) USD clamp, 2) Base clamp (spot sell), 3) Quantize, 4) Min notional check
                    // 1. USD clamp
                    let mut nq = clamp_qty_by_usd(q, px, caps.sell_notional, qty_step_f64);
                    // 2. Base clamp (spot sell için)
                    if let Some(max_base) = caps.sell_base {
                        nq = clamp_qty_by_base(nq, max_base, qty_step_f64);
                    }
                    // 3. Quantize kontrolü
                    let quantized_to_zero = qty_step_dec > Decimal::ZERO
                        && nq.0 < qty_step_dec
                        && nq.0 != Decimal::ZERO;
                    // 4. Min notional kontrolü (min_usd_per_order > 0 garantisi yukarıda)
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
                            qty_step = qty_step_f64,
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
                        // API Rate Limit koruması
                        rate_limit_guard().await;
                        match v.place_limit(&symbol, Side::Buy, px, qty, tif).await {
                            Ok(order_id) => {
                                let info = OrderInfo { order_id: order_id.clone(), side: Side::Buy, price: px, qty, created_at: Instant::now() };
                                state.active_orders.insert(order_id.clone(), info);
                                // Fiyat güncellemesini kaydet (akıllı emir analizi için)
                                state.last_order_price_update.insert(order_id, px);
                            }
                            Err(err) => {
                                warn!(%symbol, ?px, ?qty, tif = ?tif, ?err, "failed to place spot bid order");
                            }
                        }

                        // Kalan USD ile ikinci emir
                        let spent = (px.0.to_f64().unwrap_or(0.0)) * (qty.0.to_f64().unwrap_or(0.0));
                        let remaining = (caps.buy_total - spent).max(0.0);
                        if remaining >= min_usd_per_order && px.0 > Decimal::ZERO {
                            let qty_step_local = get_qty_step(state.symbol_rules.as_ref(), cfg.qty_step);
                            let qty2 = clamp_qty_by_usd(qty, px, remaining, qty_step_local);
                            if qty2.0 > Decimal::ZERO {
                                info!(%symbol, ?px, qty = ?qty2, tif = ?tif, remaining, "placing extra spot bid with leftover USD");
                                // API Rate Limit koruması
                                rate_limit_guard().await;
                                match v.place_limit(&symbol, Side::Buy, px, qty2, tif).await {
                                    Ok(order_id2) => {
                                        let info2 = OrderInfo { order_id: order_id2.clone(), side: Side::Buy, price: px, qty: qty2, created_at: Instant::now() };
                                        state.active_orders.insert(order_id2.clone(), info2);
                                        state.last_order_price_update.insert(order_id2, px);
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
                        // API Rate Limit koruması
                        rate_limit_guard().await;
                        match v.place_limit(&symbol, Side::Sell, px, qty, tif).await {
                            Ok(order_id) => {
                                let info = OrderInfo { order_id: order_id.clone(), side: Side::Sell, price: px, qty, created_at: Instant::now() };
                                state.active_orders.insert(order_id.clone(), info);
                                // Fiyat güncellemesini kaydet (akıllı emir analizi için)
                                state.last_order_price_update.insert(order_id, px);
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
                                let qty_step_local = get_qty_step(state.symbol_rules.as_ref(), cfg.qty_step);
                                let qty2 = clamp_qty_by_base(qty, remaining_base, qty_step_local);
                                if qty2.0 > Decimal::ZERO {
                                    info!(%symbol, ?px, qty = ?qty2, tif = ?tif, remaining_base, "placing extra spot ask with leftover base");
                                    // API Rate Limit koruması
                                    rate_limit_guard().await;
                                    match v.place_limit(&symbol, Side::Sell, px, qty2, tif).await {
                                        Ok(order_id2) => {
                                            let info2 = OrderInfo { order_id: order_id2.clone(), side: Side::Sell, price: px, qty: qty2, created_at: Instant::now() };
                                        state.active_orders.insert(order_id2.clone(), info2);
                                        state.last_order_price_update.insert(order_id2, px);
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
                    // NOT: effective_leverage config'den geliyor ve değişmiyor, loop başında hesaplanan değeri kullan
                    let mut total_spent_on_bids = 0.0f64; // Hesaptan giden para (margin) toplamı
                    if let Some((px, qty)) = quotes.bid {
                        
                        // İlk bid emri (max 100 USD)
                        info!(%symbol, ?px, ?qty, tif = ?tif, "placing futures bid order");
                        // API Rate Limit koruması
                        rate_limit_guard().await;
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
                                            // ÖNEMLİ: Kullanıcının isteği: "20 USD varsa 20 USD kullanılabilir"
                                            // Yani minimum notional'ı karşılamaya çalışma, mevcut bakiyeyle işlem yap
                                            // Önce mevcut bakiyeyle maksimum ne kadar işlem yapılabilir hesapla
                                            let max_qty_by_margin = ((available_margin * effective_leverage) / price_quantized / step).floor() * step;
                                            
                                            // Eğer mevcut bakiyeyle minimum notional'ı karşılayabiliyorsak, onu hedefle
                                            // Ama karşılayamıyorsak, mevcut bakiyeyle ne kadar yapılabilirse o kadar yap
                                            let target_notional = if max_qty_by_margin * price_quantized >= min_notional {
                                                // Mevcut bakiyeyle minimum notional'ı karşılayabiliyoruz
                                                min_notional * 1.10 // Güvenli margin: %10 fazlasını hedefle
                                            } else {
                                                // Mevcut bakiyeyle minimum notional'ı karşılayamıyoruz
                                                // Mevcut bakiyeyle maksimum ne kadar yapılabilirse o kadar yap
                                                max_qty_by_margin * price_quantized
                                            };
                                            
                                            let raw_qty = target_notional / price_quantized;
                                            new_qty = (raw_qty / step).floor() * step;
                                            
                                            // Cap kontrolü: max qty by cap (notional)
                                            let max_qty_by_cap = (order_cap / price_quantized / step).floor() * step;
                                            let max_qty = max_qty_by_cap.min(max_qty_by_margin);
                                            if new_qty > max_qty { 
                                                new_qty = max_qty; 
                                            }
                                            
                                            // Min notional garantisi: Eğer mevcut bakiyeyle minimum notional'ı karşılayabiliyorsak, onu garanti et
                                            // KRİTİK DÜZELTME: Bakiye yetersizse min notional'ı karşılayamayan emir yapma, skip et
                                            if max_qty_by_margin * price_quantized < min_notional {
                                                // Bakiye yetersiz, min notional'ı karşılayamıyoruz → Skip et, retry yapma
                                                warn!(%symbol, ?px, required_min = ?min_notional, available_notional = max_qty_by_margin * price_quantized, "skip bid: insufficient balance for min_notional, skipping order");
                                                new_qty = 0.0; // Skip et
                                            } else {
                                                // Mevcut bakiyeyle minimum notional'ı karşılayabiliyoruz, garanti et
                                                let mut attempts = 0;
                                                let max_attempts = 20; // Maksimum 20 step artır
                                                while attempts < max_attempts {
                                                    let notional_after_quantize = new_qty * price_quantized;
                                                    // KRİTİK DÜZELTME: Önce min notional kontrolü (pozisyon boyutu)
                                                    if notional_after_quantize >= min_notional {
                                                        // Min notional karşılandı, şimdi margin kontrolü (ayrı kontrol)
                                                        let required_margin = notional_after_quantize / effective_leverage;
                                                        if required_margin <= available_margin {
                                                            break; // Yeterli notional VE margin
                                                        } else {
                                                            // Margin yetersiz, daha küçük miktar dene
                                                            if new_qty >= step {
                                                                new_qty -= step;
                                                                continue; // Tekrar kontrol et
                                                            } else {
                                                                break; // Daha fazla azaltılamaz
                                                            }
                                                        }
                                                    }
                                                    // Notional yeterli değil, artır ve tekrar kontrol et
                                                    if new_qty + step > max_qty {
                                                        break; // Cap'e ulaştık
                                                    }
                                                    new_qty += step;
                                                    attempts += 1;
                                                }
                                            }
                                        }
                                        if new_qty > 0.0 {
                                            let retry_qty = Qty(rust_decimal::Decimal::from_f64_retain(new_qty).unwrap_or(rust_decimal::Decimal::ZERO));
                                            info!(%symbol, ?px, qty = ?retry_qty, tif = ?tif, min_notional, "retrying futures bid with exchange min notional");
                                            // API Rate Limit koruması
                                            rate_limit_guard().await;
                                            match v.place_limit(&symbol, Side::Buy, px, retry_qty, tif).await {
                                                Ok(order_id) => {
                                                    let info = OrderInfo { order_id: order_id.clone(), side: Side::Buy, price: px, qty: retry_qty, created_at: Instant::now() };
                                                    state.active_orders.insert(order_id.clone(), info);
                                                    state.last_order_price_update.insert(order_id, px);
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
                            let qty_step_local = get_qty_step(state.symbol_rules.as_ref(), cfg.qty_step);
                            let qty2 = clamp_qty_by_usd(qty, px, order_size_notional, qty_step_local);
                            let qty2_notional = (px.0.to_f64().unwrap_or(0.0)) * (qty2.0.to_f64().unwrap_or(0.0));
                            
                            if qty2.0 > Decimal::ZERO && qty2_notional >= min_req_for_second {
                                info!(%symbol, ?px, qty = ?qty2, tif = ?tif, remaining, order_size_margin, order_size_notional, min_notional = min_req_for_second, "placing extra futures bid with leftover notional");
                                // API Rate Limit koruması
                                rate_limit_guard().await;
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
                    // NOT: effective_leverage_ask config'den geliyor ve değişmiyor, loop başında hesaplanan değeri kullan
                    if let Some((px, qty)) = quotes.ask {
                        let mut total_spent_on_asks = 0.0f64; // Hesaptan giden para (margin) toplamı
                        
                        // İlk ask emri (max 100 USD)
                        info!(%symbol, ?px, ?qty, tif = ?tif, "placing futures ask order");
                        // API Rate Limit koruması
                        rate_limit_guard().await;
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
                                        // Per-symbol metadata kullan (fallback: global cfg)
                                        let price_raw = px.0.to_f64().unwrap_or(0.0);
                                        let price_tick = get_price_tick(state.symbol_rules.as_ref(), cfg.price_tick);
                                        let step = get_qty_step(state.symbol_rules.as_ref(), cfg.qty_step);
                                        
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
                                            // ÖNEMLİ: Kullanıcının isteği: "20 USD varsa 20 USD kullanılabilir"
                                            // Yani minimum notional'ı karşılamaya çalışma, mevcut bakiyeyle işlem yap
                                            // Önce mevcut bakiyeyle maksimum ne kadar işlem yapılabilir hesapla
                                            let max_qty_by_margin = ((available_margin * effective_leverage_ask) / price_quantized / step).floor() * step;
                                            
                                            // Eğer mevcut bakiyeyle minimum notional'ı karşılayabiliyorsak, onu hedefle
                                            // Ama karşılayamıyorsak, mevcut bakiyeyle ne kadar yapılabilirse o kadar yap
                                            let target_notional = if max_qty_by_margin * price_quantized >= min_notional {
                                                // Mevcut bakiyeyle minimum notional'ı karşılayabiliyoruz
                                                min_notional * 1.10 // Güvenli margin: %10 fazlasını hedefle
                                            } else {
                                                // Mevcut bakiyeyle minimum notional'ı karşılayamıyoruz
                                                // Mevcut bakiyeyle maksimum ne kadar yapılabilirse o kadar yap
                                                max_qty_by_margin * price_quantized
                                            };
                                            
                                            let raw_qty = target_notional / price_quantized;
                                            new_qty = (raw_qty / step).floor() * step;
                                            
                                            // Cap kontrolü: max qty by cap (notional)
                                            let max_qty_by_cap = (order_cap / price_quantized / step).floor() * step;
                                            let max_qty = max_qty_by_cap.min(max_qty_by_margin);
                                            if new_qty > max_qty { 
                                                new_qty = max_qty; 
                                            }
                                            
                                            // Min notional garantisi: Eğer mevcut bakiyeyle minimum notional'ı karşılayabiliyorsak, onu garanti et
                                            // KRİTİK DÜZELTME: Bakiye yetersizse min notional'ı karşılayamayan emir yapma, skip et
                                            if max_qty_by_margin * price_quantized < min_notional {
                                                // Bakiye yetersiz, min notional'ı karşılayamıyoruz → Skip et, retry yapma
                                                warn!(%symbol, ?px, required_min = ?min_notional, available_notional = max_qty_by_margin * price_quantized, "skip ask: insufficient balance for min_notional, skipping order");
                                                new_qty = 0.0; // Skip et
                                            } else {
                                                // Mevcut bakiyeyle minimum notional'ı karşılayabiliyoruz, garanti et
                                                let mut attempts = 0;
                                                let max_attempts = 20; // Maksimum 20 step artır
                                                while attempts < max_attempts {
                                                    let notional_after_quantize = new_qty * price_quantized;
                                                    // KRİTİK DÜZELTME: Önce min notional kontrolü (pozisyon boyutu)
                                                    if notional_after_quantize >= min_notional {
                                                        // Min notional karşılandı, şimdi margin kontrolü (ayrı kontrol)
                                                        let required_margin = notional_after_quantize / effective_leverage_ask;
                                                        if required_margin <= available_margin {
                                                            break; // Yeterli notional VE margin
                                                        } else {
                                                            // Margin yetersiz, daha küçük miktar dene
                                                            if new_qty >= step {
                                                                new_qty -= step;
                                                                continue; // Tekrar kontrol et
                                                            } else {
                                                                break; // Daha fazla azaltılamaz
                                                            }
                                                        }
                                                    }
                                                    // Notional yeterli değil, artır ve tekrar kontrol et
                                                    if new_qty + step > max_qty {
                                                        break; // Cap'e ulaştık
                                                    }
                                                    new_qty += step;
                                                    attempts += 1;
                                                }
                                            }
                                        }
                                        if new_qty > 0.0 {
                                            let retry_qty = Qty(rust_decimal::Decimal::from_f64_retain(new_qty).unwrap_or(rust_decimal::Decimal::ZERO));
                                            info!(%symbol, ?px, qty = ?retry_qty, tif = ?tif, min_notional, "retrying futures ask with exchange min notional");
                                            // API Rate Limit koruması
                                            rate_limit_guard().await;
                                            match v.place_limit(&symbol, Side::Sell, px, retry_qty, tif).await {
                                                Ok(order_id) => {
                                                    let info = OrderInfo { order_id: order_id.clone(), side: Side::Sell, price: px, qty: retry_qty, created_at: Instant::now() };
                                                    state.active_orders.insert(order_id.clone(), info);
                                                    state.last_order_price_update.insert(order_id, px);
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
                            let qty_step_local = get_qty_step(state.symbol_rules.as_ref(), cfg.qty_step);
                            let qty2 = clamp_qty_by_usd(qty, px, order_size_notional, qty_step_local);
                            let qty2_notional = (px.0.to_f64().unwrap_or(0.0)) * (qty2.0.to_f64().unwrap_or(0.0));
                            
                            if qty2.0 > Decimal::ZERO && qty2_notional >= min_req_for_second {
                                info!(%symbol, ?px, qty = ?qty2, tif = ?tif, remaining, order_size_margin, order_size_notional, min_notional = min_req_for_second, "placing extra futures ask with leftover notional");
                                // API Rate Limit koruması
                                rate_limit_guard().await;
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
        
        // Loop sonu: İstatistikleri logla (her 10 tick'te bir veya ilk 5 tick)
        // tick_num zaten yukarıda hesaplandı, scope'ta hala erişilebilir
        let current_tick = TICK_COUNTER.load(Ordering::Relaxed);
        if current_tick <= 5 || current_tick % 10 == 0 {
                info!(
                    tick_count = current_tick,
                    processed_symbols = processed_count,
                    skipped_symbols = skipped_count,
                    disabled_symbols = disabled_count,
                    no_balance_symbols = no_balance_count,
                    total_symbols = states.len(),
                    "main loop tick completed: statistics"
                );
        }
    }
}

#[cfg(test)]
#[path = "position_order_tests.rs"]
mod position_order_tests;

