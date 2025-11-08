//location: /crates/app/src/main.rs
// Main application entry point and trading loop

mod config;
mod logger;
mod types;
mod utils;

use anyhow::{anyhow, Result};
use bot_core::types::*;
use config::load_config;
use data::binance_ws::{UserDataStream, UserEvent, UserStreamKind};
use exec::binance::{BinanceCommon, BinanceFutures, BinanceSpot, SymbolMeta};
use exec::{decimal_places, Venue};
use risk::{RiskAction, RiskLimits};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use strategy::{Context, DynMm, DynMmCfg, Strategy};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use logger::create_logger;
use types::{OrderInfo, SymbolState};
use utils::{clamp_qty_by_base, clamp_qty_by_usd, compute_drawdown_bps, get_price_tick, get_qty_step, init_rate_limiter, is_usd_stable, rate_limit_guard, record_pnl_snapshot};

use std::cmp::max;
use std::collections::HashMap;
// Removed unused Ordering import
use std::time::{Duration, Instant};

// ============================================================================
// Constants
// ============================================================================

/// Default tick interval in milliseconds
// Removed unused constant

// ============================================================================
// Helper Functions
// ============================================================================

/// Convert TIF string from config to Tif enum
fn tif_from_cfg(s: &str) -> Tif {
    match s.to_lowercase().as_str() {
        "gtc" => Tif::Gtc,
        "ioc" => Tif::Ioc,
        "post_only" => Tif::PostOnly,
        _ => Tif::PostOnly,
    }
}



#[tokio::main]
async fn main() -> Result<()> {
    // ---- LOG INIT ----
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .compact()
        .init();

    let cfg = load_config()?;
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

    // Initialize JSON logger
    let json_logger = create_logger("logs/trading_events.json")
        .map_err(|e| anyhow!("Failed to initialize JSON logger: {}", e))?;

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
        min_spread_bps: cfg.strategy.min_spread_bps.unwrap_or(60.0), // Config'den: 60.0 (fee + kar garantisi)
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
        min_24h_volume_usd: cfg.strategy.min_24h_volume_usd.unwrap_or(0.0),
        min_book_depth_usd: cfg.strategy.min_book_depth_usd.unwrap_or(0.0),
        opportunity_size_multiplier: cfg.strategy.opportunity_size_multiplier.unwrap_or(1.05), // Config'den: 1.05 (konservatif)
        strong_trend_multiplier: cfg.strategy.strong_trend_multiplier.unwrap_or(1.0), // Config'den: 1.0 (normal boyut)
        // Strategy internal config (config.yaml'den strategy_internal bölümünden)
        manipulation_volume_ratio_threshold: Some(cfg.strategy_internal.manipulation_volume_ratio_threshold),
        manipulation_time_threshold_ms: Some(cfg.strategy_internal.manipulation_time_threshold_ms),
        manipulation_price_history_min_len: Some(cfg.strategy_internal.manipulation_price_history_min_len),
        manipulation_price_history_max_len: Some(cfg.strategy_internal.manipulation_price_history_max_len),
        confidence_price_drop_max: Some(cfg.strategy_internal.confidence_price_drop_max),
        confidence_volume_ratio_min: Some(cfg.strategy_internal.confidence_volume_ratio_min),
        confidence_volume_ratio_max: Some(cfg.strategy_internal.confidence_volume_ratio_max),
        confidence_spread_min: Some(cfg.strategy_internal.confidence_spread_min),
        confidence_spread_max: Some(cfg.strategy_internal.confidence_spread_max),
        confidence_bonus_multiplier: Some(cfg.strategy_internal.confidence_bonus_multiplier),
        confidence_max_multiplier: Some(cfg.strategy_internal.confidence_max_multiplier),
            confidence_min_threshold: Some(cfg.strategy_internal.confidence_min_threshold),
            default_confidence: Some(cfg.strategy_internal.default_confidence),
            min_confidence_value: Some(cfg.strategy_internal.min_confidence_value),
            trend_analysis_min_history: Some(cfg.strategy_internal.trend_analysis_min_history),
        trend_analysis_threshold_negative: Some(cfg.strategy_internal.trend_analysis_threshold_negative),
        trend_analysis_threshold_strong_negative: Some(cfg.strategy_internal.trend_analysis_threshold_strong_negative),
    };
    let mode_lower = cfg.mode.to_lowercase();
    let strategy_name = cfg.strategy.r#type.clone();
    let build_strategy = |symbol: &str| -> Box<dyn Strategy> {
        // DynMmCfg Clone edilebilir (derive(Clone) ile)
        let dyn_cfg_clone = DynMmCfg {
            a: dyn_cfg.a,
            b: dyn_cfg.b,
            base_size: dyn_cfg.base_size,
            inv_cap: dyn_cfg.inv_cap,
            min_spread_bps: dyn_cfg.min_spread_bps,
            max_spread_bps: dyn_cfg.max_spread_bps,
            spread_arbitrage_min_bps: dyn_cfg.spread_arbitrage_min_bps,
            spread_arbitrage_max_bps: dyn_cfg.spread_arbitrage_max_bps,
            strong_trend_bps: dyn_cfg.strong_trend_bps,
            momentum_strong_bps: dyn_cfg.momentum_strong_bps,
            trend_bias_multiplier: dyn_cfg.trend_bias_multiplier,
            adverse_selection_threshold_on: dyn_cfg.adverse_selection_threshold_on,
            adverse_selection_threshold_off: dyn_cfg.adverse_selection_threshold_off,
            opportunity_threshold_on: dyn_cfg.opportunity_threshold_on,
            opportunity_threshold_off: dyn_cfg.opportunity_threshold_off,
            price_jump_threshold_bps: dyn_cfg.price_jump_threshold_bps,
            fake_breakout_threshold_bps: dyn_cfg.fake_breakout_threshold_bps,
            liquidity_drop_threshold: dyn_cfg.liquidity_drop_threshold,
            inventory_threshold_ratio: dyn_cfg.inventory_threshold_ratio,
            volatility_coefficient: dyn_cfg.volatility_coefficient,
            ofi_coefficient: dyn_cfg.ofi_coefficient,
            min_liquidity_required: dyn_cfg.min_liquidity_required,
            min_24h_volume_usd: dyn_cfg.min_24h_volume_usd,
            min_book_depth_usd: dyn_cfg.min_book_depth_usd,
            opportunity_size_multiplier: dyn_cfg.opportunity_size_multiplier,
            strong_trend_multiplier: dyn_cfg.strong_trend_multiplier,
            manipulation_volume_ratio_threshold: dyn_cfg.manipulation_volume_ratio_threshold,
            manipulation_time_threshold_ms: dyn_cfg.manipulation_time_threshold_ms,
            manipulation_price_history_min_len: dyn_cfg.manipulation_price_history_min_len,
            manipulation_price_history_max_len: dyn_cfg.manipulation_price_history_max_len,
            confidence_price_drop_max: dyn_cfg.confidence_price_drop_max,
            confidence_volume_ratio_min: dyn_cfg.confidence_volume_ratio_min,
            confidence_volume_ratio_max: dyn_cfg.confidence_volume_ratio_max,
            confidence_spread_min: dyn_cfg.confidence_spread_min,
            confidence_spread_max: dyn_cfg.confidence_spread_max,
            confidence_bonus_multiplier: dyn_cfg.confidence_bonus_multiplier,
            confidence_max_multiplier: dyn_cfg.confidence_max_multiplier,
            confidence_min_threshold: dyn_cfg.confidence_min_threshold,
            default_confidence: dyn_cfg.default_confidence,
            min_confidence_value: dyn_cfg.min_confidence_value,
            trend_analysis_min_history: dyn_cfg.trend_analysis_min_history,
            trend_analysis_threshold_negative: dyn_cfg.trend_analysis_threshold_negative,
            trend_analysis_threshold_strong_negative: dyn_cfg.trend_analysis_threshold_strong_negative,
        };
        match strategy_name.as_str() {
            "dyn_mm" => Box::new(DynMm::from(dyn_cfg_clone)),
            other => {
                warn!(symbol = %symbol, strategy = %other, "unknown strategy type, defaulting dyn_mm");
                Box::new(DynMm::from(dyn_cfg_clone))
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
    let is_futures = cfg.mode.to_lowercase().as_str() == "futures";
    
    // Initialize rate limiter based on mode (spot or futures)
    init_rate_limiter(is_futures);
    info!(
        mode = cfg.mode,
        is_futures,
        "rate limiter initialized"
    );
    
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
        // Özel karakterli sembolleri filtrele (API signature hatalarına neden olur)
        if !sym.is_ascii() {
            warn!(
                symbol = %sym,
                "skipping symbol with non-ASCII characters (causes API signature errors)"
            );
            continue;
        }
        
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
                    rate_limit_guard(10).await; // GET /api/v3/account: Weight 10
                    let q_free = vtmp.asset_free(&meta.quote_asset).await?.to_f64().unwrap_or(0.0);
                    rate_limit_guard(10).await; // GET /api/v3/account: Weight 10
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
        // --- auto-discover: USD-stable grup kuralı + USDT seçeneği ---
        let want_group = is_usd_stable(&cfg.quote_asset);
        let mut auto: Vec<SymbolMeta> = metadata
            .iter()
            .filter(|m| {
                // Ana quote asset kontrolü
                let match_primary_quote = if want_group {
                    is_usd_stable(&m.quote_asset)
                } else {
                    m.quote_asset.eq_ignore_ascii_case(&cfg.quote_asset)
                };
                
                // USDT/USDC seçeneği: allow_usdt_quote true ise karşılıklı olarak ekle
                let match_cross_quote = if cfg.allow_usdt_quote {
                    let cfg_quote_upper = cfg.quote_asset.to_uppercase();
                    if cfg_quote_upper == "USDC" {
                        m.quote_asset.eq_ignore_ascii_case("USDT")
                    } else if cfg_quote_upper == "USDT" {
                        m.quote_asset.eq_ignore_ascii_case("USDC")
                    } else {
                        false
                    }
                } else {
                    false
                };
                
                let match_quote = match_primary_quote || match_cross_quote;
                
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
                    rate_limit_guard(10).await; // GET /api/v3/account: Weight 10
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
        
        // Özel karakterli sembolleri filtrele (API signature hatalarına neden olur)
        auto.retain(|m| {
            // Sadece ASCII karakterler içeren sembolleri kabul et
            m.symbol.is_ascii()
        });
        
        // USDC ve USDT eşit muamele görmeli - alfabetik sırala
        auto.sort_by(|a, b| a.symbol.cmp(&b.symbol));
        info!(
            count = auto.len(),
            quote_asset = %cfg.quote_asset,
            "auto-discovered symbols"
        );
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
                        // Ana quote asset kontrolü
                        let match_primary_quote = if want_group {
                            is_usd_stable(&m.quote_asset)
                        } else {
                            m.quote_asset.eq_ignore_ascii_case(&cfg.quote_asset)
                        };
                        
                        // USDT/USDC seçeneği: allow_usdt_quote true ise karşılıklı olarak ekle
                        let match_cross_quote = if cfg.allow_usdt_quote {
                            let cfg_quote_upper = cfg.quote_asset.to_uppercase();
                            if cfg_quote_upper == "USDC" {
                                m.quote_asset.eq_ignore_ascii_case("USDT")
                            } else if cfg_quote_upper == "USDT" {
                                m.quote_asset.eq_ignore_ascii_case("USDC")
                            } else {
                                false
                            }
                        } else {
                            false
                        };
                        
                        let match_quote = match_primary_quote || match_cross_quote;
                        
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
                            rate_limit_guard(10).await; // GET /api/v3/account: Weight 10
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
                                rate_limit_guard(10).await; // GET /api/v3/account: Weight 10
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
            order_fill_rate: cfg.internal.initial_fill_rate,
            consecutive_no_fills: 0,
            last_fill_time: None, // Zaman bazlı fill rate için
            last_inventory_update: None, // Envanter güncelleme race condition önleme için
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
    use std::sync::atomic::AtomicU64;
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
                    force_sync_all = true;
                    info!("websocket reconnect detected, will sync all symbols");
                    
                    // PATCH: Reconnect sonrası tüm sembollerin açık emirlerini sync et
                    // Hayalet emirleri önlemek için full sync yap
                    for state in &mut states {
                        rate_limit_guard(3).await; // GET /api/v3/openOrders: Weight 3
                        let sync_result = match &venue {
                            V::Spot(v) => v.get_open_orders(&state.meta.symbol).await,
                            V::Fut(v) => v.get_open_orders(&state.meta.symbol).await,
                        };
                        
                        match sync_result {
                            Ok(api_orders) => {
                                let api_order_ids: std::collections::HashSet<String> = api_orders
                                    .iter()
                                    .map(|o| o.order_id.clone())
                                    .collect();
                                
                                // Local'de olup API'de olmayan emirleri temizle
                                let mut removed_count = 0;
                                state.active_orders.retain(|order_id, _| {
                                    if !api_order_ids.contains(order_id) {
                                        removed_count += 1;
                                        false
                                    } else {
                                        true
                                    }
                                });
                                
                                if removed_count > 0 {
                                    state.consecutive_no_fills = 0;
                                    state.order_fill_rate = (state.order_fill_rate * 0.95 + 0.05).min(1.0);
                                    info!(
                                        symbol = %state.meta.symbol,
                                        removed_orders = removed_count,
                                        "full sync after reconnect: removed filled/canceled orders"
                                    );
                                }
                            }
                            Err(err) => {
                                warn!(symbol = %state.meta.symbol, ?err, "failed to sync orders after reconnect");
                            }
                        }
                    }
                }
                UserEvent::OrderFill {
                    symbol,
                    order_id,
                    client_order_id,
                    side,
                    qty,
                    cumulative_filled_qty,
                    price,
                    is_maker,
                    order_status,
                } => {
                    if let Some(idx) = symbol_index.get(&symbol) {
                        let state = &mut states[*idx];
                        
                        // KRİTİK: Event-based state management - sadece event'lerle state güncelle
                        // Duplicate event handling: Aynı order_id için aynı cumulative_filled_qty'yi ignore et
                        let is_duplicate = if let Some(existing_order) = state.active_orders.get(&order_id) {
                            // Eğer cumulative filled qty aynıysa, bu duplicate event
                            existing_order.filled_qty.0 >= cumulative_filled_qty.0
                        } else {
                            // Order yoksa, bu yeni bir fill (reconnect sonrası olabilir)
                            false
                        };
                        
                        if is_duplicate {
                            warn!(
                                %symbol,
                                order_id = %order_id,
                                cumulative_filled_qty = %cumulative_filled_qty.0,
                                "duplicate fill event ignored"
                            );
                            continue; // Duplicate event'i ignore et
                        }
                        
                        // Post-Only doğrulaması: Post-only emirler maker olarak fill olmalı
                        let is_post_only = cfg.exec.tif.to_lowercase() == "post_only";
                        if is_post_only && !is_maker {
                            warn!(
                                %symbol,
                                order_id = %order_id,
                                side = ?side,
                                "POST-ONLY VIOLATION: order filled as taker (should be maker), this should not happen with post-only orders"
                            );
                        }
                        
                        // KRİTİK: Partial fill handling
                        // Event'ten gelen qty = last executed qty (incremental)
                        // cumulative_filled_qty = total filled so far
                        let fill_increment = qty.0; // Bu fill'de ne kadar fill oldu
                        
                        // Inventory güncelle (sadece incremental fill miktarı)
                        let mut inv = state.inv.0;
                        if side == Side::Buy {
                            inv += fill_increment;
                        } else {
                            inv -= fill_increment;
                        }
                        state.inv = Qty(inv);
                        state.last_inventory_update = Some(std::time::Instant::now());
                        
                        // Order state güncelle
                        let should_remove = if let Some(order_info) = state.active_orders.get_mut(&order_id) {
                            // Order var, güncelle
                            order_info.filled_qty = cumulative_filled_qty;
                            order_info.remaining_qty = Qty(order_info.qty.0 - cumulative_filled_qty.0);
                            order_info.last_fill_time = Some(std::time::Instant::now());
                            
                            let remaining_qty = order_info.remaining_qty.0;
                            
                            // KRİTİK: Partial fill sonrası risk limit kontrolü
                            // Pozisyon boyutu değişti, risk limitleri tekrar kontrol et
                            // Bu kontrol main loop'ta yapılıyor, burada sadece log
                            info!(
                                %symbol,
                                order_id = %order_id,
                                side = ?side,
                                fill_increment = %fill_increment,
                                cumulative_filled_qty = %cumulative_filled_qty.0,
                                remaining_qty = %remaining_qty,
                                order_status = %order_status,
                                is_maker,
                                new_inventory = %state.inv.0,
                                "order fill event: {}",
                                if order_status == "FILLED" { "fully filled" } else { "partial fill" }
                            );
                            
                            // Eğer order tamamen fill olduysa (FILLED status), active_orders'dan kaldır
                            if order_status == "FILLED" || remaining_qty.is_zero() {
                                update_fill_rate_on_fill(
                                    state,
                                    cfg.internal.fill_rate_increase_factor,
                                    cfg.internal.fill_rate_increase_bonus,
                                );
                                true // Remove order
                            } else {
                                // Partial fill - sadece fill rate'i hafifçe artır
                                state.order_fill_rate = (state.order_fill_rate * 0.98 + 0.02).min(1.0);
                                false // Keep order
                            }
                        } else {
                            // Order local state'de yok (reconnect sonrası olabilir)
                            // API'den order bilgisini al ve state'e ekle
                            warn!(
                                %symbol,
                                order_id = %order_id,
                                "fill event for unknown order (reconnect?), will sync from API"
                            );
                            // Reconnect sonrası sync yapılacak, burada sadece inventory güncelle
                            update_fill_rate_on_fill(
                                state,
                                cfg.internal.fill_rate_increase_factor,
                                cfg.internal.fill_rate_increase_bonus,
                            );
                            false // Don't remove (order not in state)
                        };
                        
                        // Order'ı kaldır (eğer tamamen fill olduysa)
                        if should_remove {
                            state.active_orders.remove(&order_id);
                            state.last_order_price_update.remove(&order_id);
                        }
                        
                        // JSON log: Order filled
                        if let Ok(logger) = json_logger.lock() {
                            logger.log_order_filled(
                                &symbol,
                                &order_id,
                                side,
                                price,
                                qty,
                                is_maker,
                                state.inv,
                                state.order_fill_rate,
                            );
                        }
                    }
                }
                UserEvent::OrderCanceled { symbol, order_id, client_order_id } => {
                    if let Some(idx) = symbol_index.get(&symbol) {
                        let state = &mut states[*idx];
                        
                        // KRİTİK: Event-based state management - sadece event'lerle state güncelle
                        // client_order_id ile idempotency kontrolü
                        let was_removed = if let Some(order_info) = state.active_orders.get(&order_id) {
                            // client_order_id eşleşiyorsa veya yoksa, order'ı kaldır
                            if let (Some(client_id), Some(order_client_id)) = (&client_order_id, &order_info.client_order_id) {
                                client_id == order_client_id
                            } else {
                                true // client_order_id yoksa, order_id ile eşleştir
                            }
                        } else {
                            false // Order zaten yok
                        };
                        
                        if was_removed {
                            state.active_orders.remove(&order_id);
                            state.last_order_price_update.remove(&order_id);
                            update_fill_rate_on_cancel(state, cfg.internal.fill_rate_decrease_factor);
                            
                            info!(
                                %symbol,
                                order_id = %order_id,
                                client_order_id = ?client_order_id,
                                "order canceled via event"
                            );
                        } else {
                            warn!(
                                %symbol,
                                order_id = %order_id,
                                client_order_id = ?client_order_id,
                                "cancel event for unknown order or client_order_id mismatch"
                            );
                        }
                        
                        // JSON log: Order canceled
                        if let Ok(logger) = json_logger.lock() {
                            logger.log_order_canceled(
                                &symbol,
                                &order_id,
                                "price_update_or_timeout",
                                state.order_fill_rate,
                            );
                        }
                        
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

        let effective_leverage = calculate_effective_leverage(cfg.leverage, cfg.risk.max_leverage);
        let effective_leverage_ask = effective_leverage;
        
        let unique_quote_assets: std::collections::HashSet<String> = states
            .iter()
            .map(|s| s.meta.quote_asset.clone())
            .collect();
        
        let mut quote_balances: HashMap<String, f64> = HashMap::new();
        for quote_asset in &unique_quote_assets {
            rate_limit_guard(10).await; // GET /api/v3/account or /fapi/v2/balance: Weight 10/5
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
        
        let max_symbols_per_tick = cfg.internal.max_symbols_per_tick;
        let mut symbols_processed_this_tick = 0;
        
        use std::sync::atomic::{AtomicUsize, Ordering};
        static ROUND_ROBIN_OFFSET: AtomicUsize = AtomicUsize::new(0);
        let round_robin_offset = ROUND_ROBIN_OFFSET.fetch_add(1, Ordering::Relaxed) % states.len().max(1);
        
        let mut states_with_priority: Vec<(usize, bool)> = states
            .iter()
            .enumerate()
            .map(|(idx, state)| {
                let has_priority = !state.active_orders.is_empty() || !state.inv.0.is_zero();
                (idx, has_priority)
            })
            .collect();
        
        states_with_priority.sort_by(|a, b| b.1.cmp(&a.1));
        
        let prioritized_indices: Vec<usize> = states_with_priority
            .iter()
            .map(|(idx, _)| *idx)
            .cycle()
            .skip(round_robin_offset)
            .take(states.len())
            .collect();
        
        for state_idx in prioritized_indices {
            let state = &mut states[state_idx];
            
            // Rate limit koruması: Her tick'te maksimum sembol sayısı
            if symbols_processed_this_tick >= max_symbols_per_tick {
                // Bu tick'te yeterli sembol işlendi, kalan semboller bir sonraki tick'te işlenecek
                skipped_count += 1;
                continue;
            }
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
            let is_debug_symbol = symbol_index <= 5;
            
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
                        rate_limit_guard(10).await; // GET /api/v3/account: Weight 10
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
            symbols_processed_this_tick += 1;

            let should_sync_orders = should_sync_orders(
                force_sync_all,
                state.last_order_sync,
                cfg.internal.order_sync_interval_sec,
            );
            if should_sync_orders {
                // API Rate Limit koruması
                rate_limit_guard(3).await; // GET /api/v3/openOrders: Weight 3
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
                            state.order_fill_rate = (state.order_fill_rate * cfg.internal.fill_rate_increase_factor + cfg.internal.fill_rate_increase_bonus * (removed_count as f64).min(1.0)).min(1.0);
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
                                    client_order_id: None, // API'den gelmiyor, sync sonrası event'lerle güncellenecek
                                    side: api_order.side,
                                    price: api_order.price,
                                    qty: api_order.qty,
                                    filled_qty: Qty(Decimal::ZERO), // API sync'te bilinmiyor, event'lerle güncellenecek
                                    remaining_qty: api_order.qty, // Başlangıçta qty = remaining
                                    created_at: Instant::now(), // Tahmini zaman
                                    last_fill_time: None,
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
                        rate_limit_guard(1).await; // DELETE /api/v3/order: Weight 1
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
                    // cancel_all multiple calls yapabilir, her biri Weight 1
                    rate_limit_guard(1).await; // DELETE /api/v3/order: Weight 1 (per order)
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
            // best_prices: Weight 2 (Spot), Weight 1 (Futures)
            let best_prices_weight = if is_futures { 1 } else { 2 };
            rate_limit_guard(best_prices_weight).await;
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
            // get_position: Weight 10 (Spot account), Weight 5 (Futures positionRisk)
            let position_weight = if is_futures { 5 } else { 10 };
            rate_limit_guard(position_weight).await;
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
            // mark_price: Weight 1 (Futures), Weight 2 (Spot best_prices)
            let mark_price_weight = if is_futures { 1 } else { 2 };
            rate_limit_guard(mark_price_weight).await;
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
                        cfg.internal.order_price_distance_with_position // Config'den: %1 (pozisyon varsa daha toleranslı)
                    } else {
                        cfg.internal.order_price_distance_no_position // Config'den: %0.5 (pozisyon yoksa daha sıkı)
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
                            price_diff_pct > Decimal::from_f64_retain(cfg.internal.order_price_change_threshold).unwrap_or(Decimal::new(1, 4)) // Config'den: %0.01'den fazla değişmiş
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
                let stagger_delay_ms = cfg.internal.cancel_stagger_delay_ms; // Config'den: Her iptal arasında bekleme süresi
                for (idx, order_id) in orders_to_cancel.iter().enumerate() {
                    if idx > 0 {
                        // İlk iptal hariç, her iptal arasında bekle (stagger)
                        tokio::time::sleep(Duration::from_millis(stagger_delay_ms)).await;
                    }
                    // API Rate Limit koruması
                    rate_limit_guard(1).await; // DELETE /api/v3/order: Weight 1
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
            // KRİTİK DÜZELTME: WebSocket reconnect sonrası pozisyon sync'i
            // Reconnect sonrası sadece emirleri değil, pozisyonları da sync et
            // force_sync_all true ise TAM sync yap (reconnect sonrası)
            // RACE CONDITION ÖNLEME: Son envanter güncellemesinden 100ms geçmeden sync yapma
            let inv_diff = (state.inv.0 - pos.qty.0).abs();
            let reconcile_threshold = Decimal::from_str_radix(&cfg.internal.inventory_reconcile_threshold, 10)
                .unwrap_or(Decimal::new(1, 8));
            let is_reconnect_sync = force_sync_all;
            
            // Son envanter güncellemesinden bu yana geçen süre (race condition önleme)
            let time_since_last_update = state.last_inventory_update
                .map(|t| t.elapsed().as_millis())
                .unwrap_or(1000); // Eğer hiç güncelleme yoksa, sync yap
            
            // Reconnect sonrası veya normal durumda uyumsuzluk varsa sync yap
            // Ama son güncellemeden 100ms geçmeden sync yapma (race condition önleme)
            if is_reconnect_sync || (inv_diff > reconcile_threshold && time_since_last_update > 100) {
                if is_reconnect_sync {
                    // Reconnect sonrası: Her zaman sync yap (uyumsuzluk olsun ya da olmasın)
                    info!(
                        %symbol,
                        ws_inv = %state.inv.0,
                        api_inv = %pos.qty.0,
                        diff = %inv_diff,
                        "force syncing position after reconnect (full sync)"
                    );
                    state.inv = pos.qty; // Force sync
                    state.last_inventory_update = Some(std::time::Instant::now());
                } else if inv_diff > reconcile_threshold && time_since_last_update > 100 {
                    // Normal durumda: Sadece uyumsuzluk varsa ve son güncellemeden 100ms geçtiyse sync yap
                    warn!(
                        %symbol,
                        ws_inv = %state.inv.0,
                        api_inv = %pos.qty.0,
                        diff = %inv_diff,
                        time_since_last_update_ms = time_since_last_update,
                        "inventory mismatch detected, syncing with API position (race condition safe)"
                    );
                    state.inv = pos.qty; // Force sync
                    state.last_inventory_update = Some(std::time::Instant::now());
                }
            }

            record_pnl_snapshot(&mut state.pnl_history, &pos, mark_px, cfg.internal.pnl_history_max_len);
            
            // --- AKILLI POZİSYON ANALİZİ: Durumu detaylı incele ---
            let current_pnl = (mark_px.0 - pos.entry.0) * pos.qty.0;
            let pnl_f64 = current_pnl.to_f64().unwrap_or(0.0);
            // position_size_notional zaten yukarıda hesaplandı, tekrar hesaplamaya gerek yok
            
            // --- GELİŞMİŞ RİSK VE KAZANÇ TAKİBİ: Detaylı analiz ---
            
            // 1. Günlük PnL takibi (basit: her tick'te güncelle, reset mekanizması eklenebilir)
            state.daily_pnl = current_pnl; // Şimdilik current PnL, ileride günlük reset eklenebilir
            
            // 2. Funding cost takibi (futures için)
            // KRİTİK DÜZELTME: Funding cost hesaplama düzeltildi
            // Funding rate zaten per 8h rate, direkt çarpılabilir
            if let Some(funding_rate) = funding_rate {
                // Funding cost = funding_rate * position_size_notional (her 8 saatte bir)
                // Not: Bu sadece bir tick'teki maliyet, gerçek maliyet 8 saatte bir uygulanır
                let funding_cost = funding_rate * position_size_notional;
                state.total_funding_cost += Decimal::from_f64_retain(funding_cost).unwrap_or(Decimal::ZERO);
            }
            
            // 3. Pozisyon boyutu geçmişi (risk analizi için)
            state.position_size_notional_history.push(position_size_notional);
            if state.position_size_notional_history.len() > cfg.internal.position_size_history_max_len {
                state.position_size_notional_history.remove(0);
            }
            
            // 4. Kümülatif PnL takibi
            state.cumulative_pnl = current_pnl; // Şimdilik current, ileride gerçek kümülatif hesaplanabilir
            
            // 5. Pozisyon boyutu risk kontrolü: Çok büyük pozisyonlar riskli
            // KRİTİK: Opportunity mode için soft-limit mekanizması
            let is_opportunity_mode = state.strategy.is_opportunity_mode();
            let max_position_multiplier = if is_opportunity_mode {
                cfg.internal.opportunity_mode_position_multiplier
            } else {
                1.0
            };
            
            // KRİTİK: max_position_size_usd hesabını exchange pozisyon riskine göre hesapla
            // Mark-price vs entry-price: Exchange risk hesaplaması için mark-price kullanılmalı
            // Ancak entry-price ile karşılaştırma yaparak risk değerlendirmesi yapılabilir
            let position_size_notional_mark = position_size_notional; // Mark-price ile (mevcut)
            let position_size_notional_entry = (pos.entry.0 * pos.qty.0.abs()).to_f64().unwrap_or(0.0); // Entry-price ile
            
            // Exchange risk hesaplaması: Mark-price bazlı (exchange'in gördüğü risk)
            // Ancak entry-price ile karşılaştırma yaparak gerçek risk değerlendirmesi
            let max_position_size_usd = cfg.max_usd_per_order * effective_leverage * cfg.internal.max_position_size_buffer * max_position_multiplier;
            
            // KRİTİK: Multiple open orders ve toplam notional birikimi reconcile et
            // Active orders'ın toplam notional'ını hesapla
            let total_active_orders_notional: f64 = state.active_orders.values()
                .map(|order| {
                    let order_notional = (order.price.0 * order.remaining_qty.0).to_f64().unwrap_or(0.0);
                    order_notional
                })
                .sum();
            
            // Toplam risk: Mevcut pozisyon + açık emirler
            let total_exposure_notional = position_size_notional_mark + total_active_orders_notional;
            
            // Opportunity mode için soft-limit mekanizması
            let soft_limit = max_position_size_usd * cfg.internal.opportunity_mode_soft_limit_ratio;
            let medium_limit = max_position_size_usd * cfg.internal.opportunity_mode_medium_limit_ratio;
            let hard_limit = max_position_size_usd * cfg.internal.opportunity_mode_hard_limit_ratio;
            
            // Risk seviyesi belirleme
            let position_size_risk_level = if total_exposure_notional >= hard_limit {
                "hard" // Force-close
            } else if total_exposure_notional >= medium_limit {
                "medium" // Mevcut emirleri azalt
            } else if total_exposure_notional >= soft_limit {
                "soft" // Yeni emirleri durdur
            } else {
                "ok" // Normal
            };
            
            // KRİTİK: Opportunity mode için soft-limit flag'i
            // quotes henüz tanımlanmadı, bu yüzden flag kullanıyoruz
            let mut should_block_new_orders = false;
            
            // Opportunity mode'da soft-limit uygula
            if is_opportunity_mode {
                match position_size_risk_level {
                    "hard" => {
                        // KRİTİK: Hard limit - Force-close
                        warn!(
                            %symbol,
                            position_size_notional = position_size_notional_mark,
                            total_exposure_notional,
                            max_allowed = max_position_size_usd,
                            hard_limit,
                            active_orders_count = state.active_orders.len(),
                            active_orders_notional = total_active_orders_notional,
                            "OPPORTUNITY MODE HARD LIMIT: position + orders exceed hard limit, force closing"
                        );
                        // Önce tüm emirleri iptal et
                        rate_limit_guard(1).await;
                        match &venue {
                            V::Spot(v) => {
                                if let Err(err) = v.cancel_all(&symbol).await {
                                    warn!(%symbol, ?err, "failed to cancel all orders before force-close");
                                }
                            }
                            V::Fut(v) => {
                                if let Err(err) = v.cancel_all(&symbol).await {
                                    warn!(%symbol, ?err, "failed to cancel all orders before force-close");
                                }
                            }
                        }
                        // Sonra pozisyonu kapat
                        rate_limit_guard(1).await;
                        match &venue {
                            V::Spot(v) => {
                                if let Err(err) = v.close_position(&symbol).await {
                                    error!(%symbol, ?err, "failed to close position due to hard limit");
                                } else {
                                    info!(%symbol, "closed position due to hard limit");
                                }
                            }
                            V::Fut(v) => {
                                if let Err(err) = v.close_position(&symbol).await {
                                    error!(%symbol, ?err, "failed to close position due to hard limit");
                                } else {
                                    info!(%symbol, "closed position due to hard limit");
                                }
                            }
                        }
                        continue; // Bu tick'i atla
                    }
                    "medium" => {
                        // Medium limit - Mevcut emirleri kademeli azalt
                        warn!(
                            %symbol,
                            position_size_notional = position_size_notional_mark,
                            total_exposure_notional,
                            max_allowed = max_position_size_usd,
                            medium_limit,
                            active_orders_count = state.active_orders.len(),
                            active_orders_notional = total_active_orders_notional,
                            "OPPORTUNITY MODE MEDIUM LIMIT: reducing active orders gradually"
                        );
                        // En eski emirlerin %50'sini iptal et (kademeli azaltma)
                        let mut orders_with_times: Vec<(String, Instant)> = state.active_orders.iter()
                            .map(|(order_id, order)| (order_id.clone(), order.created_at))
                            .collect();
                        // En eski önce sırala
                        orders_with_times.sort_by(|a, b| a.1.cmp(&b.1));
                        let orders_to_cancel: Vec<String> = orders_with_times
                            .into_iter()
                            .take((state.active_orders.len() / 2).max(1)) // En az 1 emir iptal et
                            .map(|(order_id, _)| order_id)
                            .collect();
                        
                        for order_id in &orders_to_cancel {
                            rate_limit_guard(1).await;
                            match &venue {
                                V::Spot(v) => {
                                    if let Err(err) = v.cancel(order_id, &symbol).await {
                                        warn!(%symbol, order_id = %order_id, ?err, "failed to cancel order in medium limit");
                                    } else {
                                        state.active_orders.remove(order_id);
                                        state.last_order_price_update.remove(order_id);
                                    }
                                }
                                V::Fut(v) => {
                                    if let Err(err) = v.cancel(order_id, &symbol).await {
                                        warn!(%symbol, order_id = %order_id, ?err, "failed to cancel order in medium limit");
                                    } else {
                                        state.active_orders.remove(order_id);
                                        state.last_order_price_update.remove(order_id);
                                    }
                                }
                            }
                        }
                        // Yeni emirleri de durdur (soft limit davranışı)
                        should_block_new_orders = true;
                    }
                    "soft" => {
                        // Soft limit - Yeni emirleri durdur
                        info!(
                            %symbol,
                            position_size_notional = position_size_notional_mark,
                            total_exposure_notional,
                            max_allowed = max_position_size_usd,
                            soft_limit,
                            active_orders_count = state.active_orders.len(),
                            active_orders_notional = total_active_orders_notional,
                            "OPPORTUNITY MODE SOFT LIMIT: stopping new orders, keeping existing orders"
                        );
                        // Yeni emirleri durdur
                        should_block_new_orders = true;
                    }
                    _ => {
                        // Normal - Yeni emirler verilebilir
                    }
                }
            } else {
                // Normal mode: Eski davranış (anında force-close)
                if position_size_notional_mark > max_position_size_usd {
                    warn!(
                        %symbol,
                        position_size_notional = position_size_notional_mark,
                        max_allowed = max_position_size_usd,
                        "POSITION SIZE RISK: position too large, force closing (normal mode)"
                    );
                    rate_limit_guard(1).await;
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
            }
            
            // 6. Real-time PnL alerts: Kritik seviyelerde uyarı
            // ÖNEMLİ: PnL her tick'te kontrol ediliyor, sadece alert spam'ini önle
            let pnl_alert_threshold_positive = cfg.internal.pnl_alert_threshold_positive; // Config'den: %5 kar
            let pnl_alert_threshold_negative = cfg.internal.pnl_alert_threshold_negative; // Config'den: %3 zarar
            let should_alert = state.last_pnl_alert
                .map(|last| last.elapsed().as_secs() >= cfg.internal.pnl_alert_interval_sec) // Config'den: Alert interval
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
            let position_qty_threshold = Decimal::from_str_radix(&cfg.internal.position_qty_threshold, 10)
                .unwrap_or(Decimal::new(1, 8));
            if pos.qty.0.abs() > position_qty_threshold {
                // Pozisyon var
                if state.position_entry_time.is_none() {
                    state.position_entry_time = Some(Instant::now());
                    
                    // JSON log: Position opened
                    if let Ok(logger) = json_logger.lock() {
                        let side = if pos.qty.0.is_sign_positive() { "long" } else { "short" };
                        logger.log_position_opened(
                            symbol,
                            side,
                            pos.entry,
                            pos.qty,
                            pos.leverage,
                            "order_filled",
                        );
                    }
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
                let peak_pnl_f64 = state.peak_pnl.to_f64().unwrap_or(0.0);
                let current_pnl_f64 = current_pnl.to_f64().unwrap_or(0.0);
                
                if current_pnl_f64 > peak_pnl_f64 {
                    state.peak_pnl = current_pnl;
                }
                
                let (should_close, reason) = should_close_position(
                    current_pnl,
                    state.peak_pnl,
                    price_change_pct,
                    position_size_notional,
                    state.position_hold_duration_ms,
                    pnl_trend,
                    &cfg.internal,
                    &cfg.strategy_internal,
                );
                
                // Akıllı karar: Pozisyonu kapat
                if should_close {
                    
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
                    rate_limit_guard(1).await; // DELETE /api/v3/order: Weight 1 (per order)
                    let cancel_result = match &venue {
                        V::Spot(v) => v.cancel_all(&symbol).await,
                        V::Fut(v) => v.cancel_all(&symbol).await,
                    };
                    if let Err(err) = cancel_result {
                        warn!(%symbol, ?err, "failed to cancel orders before position close");
                    }
                    
                    // KRİTİK: Pozisyonu kapat (reduceOnly market order garantisi ile)
                    // close_position fonksiyonu içinde:
                    // 1. reduceOnly=true garantisi (futures için)
                    // 2. Market order (post-only değil)
                    // 3. Pozisyon kapatma sonrası doğrulama
                    // 4. Kısmi kapatma durumunda otomatik retry (3 deneme)
                    rate_limit_guard(1).await; // POST /fapi/v1/order: Weight 1
                    let close_result = match &venue {
                        V::Spot(v) => v.close_position(&symbol).await,
                        V::Fut(v) => {
                            // Futures için özel kontrol: Pozisyon kapatma sonrası doğrulama
                            let result = v.close_position(&symbol).await;
                            
                            // KRİTİK: Pozisyon kapatma sonrası doğrulama
                            if result.is_ok() {
                                // Kısa bir bekleme (exchange'in işlemesi için)
                                tokio::time::sleep(Duration::from_millis(500)).await;
                                
                                // Pozisyon durumunu kontrol et
                                rate_limit_guard(5).await; // GET /fapi/v2/positionRisk: Weight 5
                                match v.get_position(&symbol).await {
                                    Ok(verify_pos) => {
                                        if !verify_pos.qty.0.is_zero() {
                                            warn!(
                                                %symbol,
                                                remaining_qty = %verify_pos.qty.0,
                                                "position not fully closed after close_position call, this should not happen (retry mechanism should handle this)"
                                            );
                                            // close_position içinde retry mekanizması var, burada sadece log
                                        } else {
                                            info!(
                                                %symbol,
                                                "position fully closed and verified"
                                            );
                                        }
                                    }
                                    Err(e) => {
                                        warn!(
                                            %symbol,
                                            error = %e,
                                            "failed to verify position closure"
                                        );
                                    }
                                }
                            }
                            
                            result
                        }
                    };
                    
                    match close_result {
                        Ok(_) => {
                            // JSON log: Position closed
                            if let Ok(logger) = json_logger.lock() {
                                let side = if pos.qty.0.is_sign_positive() { "long" } else { "short" };
                                let leverage = pos.leverage;
                                logger.log_position_closed(
                                    symbol,
                                    side,
                                    pos.entry,
                                    mark_px,
                                    pos.qty,
                                    leverage,
                                    &reason,
                                );
                                
                                // Also log as completed trade
                                let fees = 0.0; // Fees calculated separately if needed
                                logger.log_trade_completed(
                                    symbol,
                                    side,
                                    pos.entry,
                                    mark_px,
                                    pos.qty,
                                    fees,
                                    leverage,
                                );
                            }
                            
                            info!(
                                %symbol,
                                reason,
                                final_pnl = pnl_f64,
                                entry_price = %pos.entry.0,
                                exit_price = %mark_px.0,
                                quantity = %pos.qty.0,
                                leverage = pos.leverage,
                                "position closed successfully with reduceOnly guarantee"
                            );
                        }
                        Err(err) => {
                            error!(
                                %symbol,
                                error = %err,
                                reason,
                                "CRITICAL: failed to close position, manual intervention may be required"
                            );
                            // Hata durumunda state'i sıfırlamaya devam et (pozisyon hala açık olabilir)
                        }
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
            
            if should_log_position && has_position {
                // JSON log: Position updated
                if let Ok(logger) = json_logger.lock() {
                    let side = if pos.qty.0.is_sign_positive() { "long" } else { "short" };
                    logger.log_position_updated(
                        symbol,
                        side,
                        pos.entry,
                        pos.qty,
                        mark_px,
                        pos.leverage,
                    );
                }
                
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
            
            // --- AKILLI FILL ORANI TAKİBİ: Zaman bazlı fill rate kontrolü ---
            // KRİTİK DÜZELTME: Tick sayısı yerine zaman bazlı kontrol (1 saniye = 1000 tick yerine gerçek zaman)
            if state.active_orders.len() > 0 {
                // Emirlerin ne kadar süredir açık olduğunu kontrol et
                let oldest_order_age = state.active_orders.values()
                    .map(|o| o.created_at.elapsed().as_secs_f64())
                    .fold(0.0, f64::max);
                
                // Son fill'den bu yana geçen süre
                let time_since_last_fill = state.last_fill_time
                    .map(|t| t.elapsed().as_secs_f64())
                    .unwrap_or(f64::MAX);
                
                // Eğer emirler 5 saniyeden fazla açıksa ve son fill'den 5 saniye geçtiyse, fill rate'i düşür
                let no_fill_threshold_sec = cfg.internal.no_fill_threshold_sec;
                if oldest_order_age > no_fill_threshold_sec && time_since_last_fill > no_fill_threshold_sec {
                    state.order_fill_rate = (state.order_fill_rate * cfg.internal.fill_rate_decrease_on_no_fill).max(cfg.internal.min_fill_rate);
                    state.consecutive_no_fills += 1; // Geriye dönük uyumluluk için
                    
                    warn!(
                        symbol = %state.meta.symbol,
                        fill_rate = state.order_fill_rate,
                        consecutive_no_fills = state.consecutive_no_fills,
                        "FILL RATE WARNING: no fills for 5+ seconds, reducing fill rate aggressively"
                    );
                }
            } else {
                // Emir yoksa, consecutive_no_fills sıfırla ve fill oranını yavaşça normale döndür
                state.consecutive_no_fills = 0;
                state.order_fill_rate = (state.order_fill_rate * cfg.internal.fill_rate_slow_decrease_factor + cfg.internal.fill_rate_slow_decrease_bonus).min(1.0);
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
                rate_limit_guard(1).await; // DELETE /api/v3/order: Weight 1 (per order)
                match &venue {
                    V::Spot(v) => {
                        if let Err(err) = v.cancel_all(&symbol).await {
                            warn!(%symbol, ?err, "failed to cancel all orders during halt");
                        }
                        rate_limit_guard(1).await; // POST /api/v3/order: Weight 1
                        if let Err(err) = v.close_position(&symbol).await {
                            warn!(%symbol, ?err, "failed to close position during halt");
                        }
                    }
                    V::Fut(v) => {
                        if let Err(err) = v.cancel_all(&symbol).await {
                            warn!(%symbol, ?err, "failed to cancel all orders during halt");
                        }
                        rate_limit_guard(1).await; // POST /fapi/v1/order: Weight 1
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
            
            // KRİTİK: Opportunity mode soft-limit kontrolü - yeni emirleri durdur
            if should_block_new_orders {
                quotes.bid = None;
                quotes.ask = None;
                info!(
                    %symbol,
                    "OPPORTUNITY MODE: blocking new orders due to position size limits"
                );
            }
            
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
                    let widen = Decimal::from_f64_retain(cfg.internal.order_price_distance_no_position).unwrap_or(Decimal::ZERO);
                    quotes.bid = quotes
                        .bid
                        .map(|(px, qty)| (Px(px.0 * (Decimal::ONE - widen)), qty));
                    quotes.ask = quotes
                        .ask
                        .map(|(px, qty)| (Px(px.0 * (Decimal::ONE + widen)), qty));
                }
                RiskAction::Widen => {
                    let widen = Decimal::from_f64_retain(cfg.internal.spread_widen_factor).unwrap_or(Decimal::ZERO);
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
                    rate_limit_guard(10).await; // GET /api/v3/account: Weight 10
                    let quote_free = match v.asset_free(&quote_asset).await {
                        Ok(q) => {
                            let q_f64 = q.to_f64().unwrap_or(0.0);
                            // HIZLI KONTROL: Config'deki minimum eşikten azsa işlem yapma
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
                    rate_limit_guard(10).await; // GET /api/v3/account: Weight 10
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
                    
                    // MEVCUT POZİSYONLARIN GERÇEK MARGİN'İNİ ÇIKAR: Unrealized PnL hesaba katılmalı
                    // KRİTİK DÜZELTME: Zarar eden pozisyon margin'i tüketir ama kod bunu görmüyordu
                    // Mevcut pozisyonun GERÇEK margin'i = (pozisyon notional / leverage) - unrealized PnL
                    // position_size_notional ve current_pnl zaten yukarıda hesaplandı
                    let existing_position_margin = if position_size_notional > 0.0 {
                        // Base margin: Pozisyon açmak için gereken margin
                        let base_margin = position_size_notional / effective_leverage;
                        // Unrealized PnL: Zarar eden pozisyon margin'i tüketir, kar eden pozisyon margin'i serbest bırakır
                        let position_pnl = current_pnl.to_f64().unwrap_or(0.0);
                        // Gerçek margin kullanımı = base_margin - position_pnl
                        // Negatif PnL (zarar) margin'i tüketir, pozitif PnL (kar) margin'i serbest bırakır
                        (base_margin - position_pnl).max(0.0) // Negatif olamaz
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
                    
                    // KRİTİK DÜZELTME: Fırsat modunda leverage'i yarıya düşür
                    let is_opportunity_mode = state.strategy.is_opportunity_mode();
                    let effective_leverage_for_caps = if is_opportunity_mode {
                        effective_leverage * cfg.internal.opportunity_mode_leverage_reduction
                    } else {
                        effective_leverage
                    };
                    
                    // Leverage ile açılan pozisyon boyutu (sadece bilgi amaçlı)
                    let position_size_with_leverage = max_usable_from_account * effective_leverage_for_caps;
                    
                    // per_order_cap = margin (hesaptan giden para) = 100 USD
                    // per_order_notional = pozisyon boyutu = margin * leverage = 100 * 20 = 2000 USD
                    let per_order_cap_margin = cfg.max_usd_per_order;
                    let per_order_notional = per_order_cap_margin * effective_leverage_for_caps;
                    info!(
                        %symbol,
                        quote_asset = %quote_asset,
                        available_balance = avail,
                        existing_position_margin,
                        available_after_position,
                        effective_leverage,
                        effective_leverage_for_caps,
                        is_opportunity_mode,
                        max_usable_from_account,
                        position_size_with_leverage,
                        per_order_limit_margin_usd = per_order_cap_margin,
                        per_order_limit_notional_usd = per_order_notional,
                        "calculated futures caps: max_usable_from_account is max USD that will leave your account, leverage only affects position size (existing position margin deducted, opportunity mode reduces leverage by 50%)"
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

            // Quote asset bakiye kontrolü: Yetersiz bakiye varsa skip et
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

            // --- PROFIT GUARANTEE FILTER: Trade yapılmadan önce karlılık kontrolü ---
            if let (Some((bid_px, bid_qty)), Some((ask_px, ask_qty))) = (quotes.bid, quotes.ask) {
                let spread_bps = utils::calculate_spread_bps(bid_px.0, ask_px.0);
                let position_size_usd = {
                    let bid_notional = bid_px.0.to_f64().unwrap_or(0.0) * bid_qty.0.to_f64().unwrap_or(0.0);
                    let ask_notional = ask_px.0.to_f64().unwrap_or(0.0) * ask_qty.0.to_f64().unwrap_or(0.0);
                    bid_notional.max(ask_notional) // Use larger of the two
                };
                
                let min_spread_bps = cfg.strategy.min_spread_bps.unwrap_or(60.0);
                let stop_loss_threshold = cfg.internal.stop_loss_threshold;
                let min_risk_reward_ratio = cfg.internal.min_risk_reward_ratio;
                
                let (should_place, reason) = utils::should_place_trade(
                    spread_bps,
                    position_size_usd,
                    min_spread_bps,
                    stop_loss_threshold,
                    min_risk_reward_ratio,
                );
                
                if !should_place {
                    // JSON log: Trade rejected
                    if let Ok(logger) = json_logger.lock() {
                        logger.log_trade_rejected(
                            symbol,
                            reason,
                            spread_bps,
                            position_size_usd,
                            min_spread_bps,
                        );
                    }
                    
                    use tracing::debug;
                    debug!(
                        %symbol,
                        spread_bps,
                        position_size_usd,
                        reason,
                        "TRADE FILTERED: not profitable or risk/reward too low"
                    );
                    // Filter out quotes that don't meet profit guarantee
                    quotes.bid = None;
                    quotes.ask = None;
                }
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
                        rate_limit_guard(1).await; // POST /api/v3/order: Weight 1
                        match v.place_limit(&symbol, Side::Buy, px, qty, tif).await {
                            Ok(order_id) => {
                                let info = OrderInfo { 
                                    order_id: order_id.clone(), 
                                    client_order_id: None, // TODO: clientOrderId ekle
                                    side: Side::Buy, 
                                    price: px, 
                                    qty, 
                                    filled_qty: Qty(Decimal::ZERO),
                                    remaining_qty: qty,
                                    created_at: Instant::now(),
                                    last_fill_time: None,
                                };
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
                                rate_limit_guard(1).await; // POST /api/v3/order: Weight 1
                                match v.place_limit(&symbol, Side::Buy, px, qty2, tif).await {
                                    Ok(order_id2) => {
                                        let info2 = OrderInfo { 
                                            order_id: order_id2.clone(), 
                                            client_order_id: None,
                                            side: Side::Buy, 
                                            price: px, 
                                            qty: qty2, 
                                            filled_qty: Qty(Decimal::ZERO),
                                            remaining_qty: qty2,
                                            created_at: Instant::now(),
                                            last_fill_time: None,
                                        };
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
                        rate_limit_guard(1).await; // POST /api/v3/order: Weight 1
                        match v.place_limit(&symbol, Side::Sell, px, qty, tif).await {
                            Ok(order_id) => {
                                let info = OrderInfo { 
                                    order_id: order_id.clone(), 
                                    client_order_id: None, // TODO: clientOrderId ekle
                                    side: Side::Sell, 
                                    price: px, 
                                    qty, 
                                    filled_qty: Qty(Decimal::ZERO),
                                    remaining_qty: qty,
                                    created_at: Instant::now(),
                                    last_fill_time: None,
                                };
                                state.active_orders.insert(order_id.clone(), info);
                                // Fiyat güncellemesini kaydet (akıllı emir analizi için)
                                state.last_order_price_update.insert(order_id.clone(), px);
                                
                                // JSON log: Order created
                                if let Ok(logger) = json_logger.lock() {
                                    logger.log_order_created(
                                        symbol,
                                        &order_id,
                                        Side::Sell,
                                        px,
                                        qty,
                                        "spread_opportunity",
                                        &cfg.exec.tif,
                                    );
                                }
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
                                    rate_limit_guard(1).await; // POST /api/v3/order: Weight 1
                                    match v.place_limit(&symbol, Side::Sell, px, qty2, tif).await {
                                        Ok(order_id2) => {
                                            let info2 = OrderInfo { 
                                            order_id: order_id2.clone(), 
                                            client_order_id: None,
                                            side: Side::Sell, 
                                            price: px, 
                                            qty: qty2, 
                                            filled_qty: Qty(Decimal::ZERO),
                                            remaining_qty: qty2,
                                            created_at: Instant::now(),
                                            last_fill_time: None,
                                        };
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
                        rate_limit_guard(1).await; // POST /fapi/v1/order: Weight 1
                        match v.place_limit(&symbol, Side::Buy, px, qty, tif).await {
                            Ok(order_id) => {
                                let info = OrderInfo { 
                                    order_id: order_id.clone(), 
                                    client_order_id: None, // TODO: clientOrderId ekle
                                    side: Side::Buy, 
                                    price: px, 
                                    qty, 
                                    filled_qty: Qty(Decimal::ZERO),
                                    remaining_qty: qty,
                                    created_at: Instant::now(),
                                    last_fill_time: None,
                                };
                                state.active_orders.insert(order_id.clone(), info);
                                // Başarılı emir için spent hesapla (hesaptan giden para = margin)
                                // Notional (pozisyon boyutu) / leverage = hesaptan giden para
                                let notional = (px.0.to_f64().unwrap_or(0.0)) * (qty.0.to_f64().unwrap_or(0.0));
                                total_spent_on_bids = notional / effective_leverage;
                                
                                // JSON log: Order created
                                if let Ok(logger) = json_logger.lock() {
                                    logger.log_order_created(
                                        symbol,
                                        &order_id,
                                        Side::Buy,
                                        px,
                                        qty,
                                        "spread_opportunity",
                                        &cfg.exec.tif,
                                    );
                                }
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
                                        // >= kontrolü: min_notional (pozisyon boyutu) >= max_notional (margin * leverage) ise bu tick'i skip et
                                        // KRİTİK DÜZELTME: Disable etme, sadece bu tick'i skip et (bakiye artarsa veya fiyat değişirse tekrar deneyebilir)
                                        let max_notional = cfg.max_usd_per_order * effective_leverage;
                                        if min_notional >= max_notional {
                                            warn!(%symbol, min_notional, max_notional, max_usd_per_order = cfg.max_usd_per_order, effective_leverage,
                                                  "skipping tick: exchange min_notional >= max position size (margin * leverage), will retry on next tick if conditions change");
                                            // Disable etme, sadece bu emri skip et ve continue ile bir sonraki emre geç
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
                                            // KRİTİK DÜZELTME: Fırsat modunda leverage'i yarıya düşür
                                            let effective_leverage_for_qty_bid = if state.strategy.is_opportunity_mode() {
                                                effective_leverage * cfg.internal.opportunity_mode_leverage_reduction
                                            } else {
                                                effective_leverage
                                            };
                                            let max_qty_by_margin = ((available_margin * effective_leverage_for_qty_bid) / price_quantized / step).floor() * step;
                                            
                                            // KRİTİK DÜZELTME: Min notional retry mantığını düzelt
                                            // Önce bakiye yeterliliğini kontrol et
                                            if max_qty_by_margin * price_quantized < min_notional {
                                                // Bakiye yetersiz, min notional'ı karşılayamıyoruz → Skip et, retry yapma
                                                warn!(%symbol, ?px, required_min = ?min_notional, available_notional = max_qty_by_margin * price_quantized, "skip bid: insufficient balance for min_notional, skipping order");
                                                new_qty = 0.0; // Skip et
                                            } else {
                                                // Bakiye yeterli, min notional'a göre qty hesapla
                                                // Min notional için gerekli qty'yi hesapla (%10 güvenli margin ile)
                                                // PATCH: Güvenlik marjını 10% → 5%'e düşür
                                                let min_qty_for_notional = (min_notional * 1.05 / price_quantized / step).ceil() * step;
                                                
                                                // Cap kontrolü: max qty by cap (notional)
                                                let max_qty_by_cap = (order_cap / price_quantized / step).floor() * step;
                                                let max_qty = max_qty_by_cap.min(max_qty_by_margin);
                                                
                                                // Final qty: min_notional gereksinimi, cap ve margin constraint'i karşılamalı
                                                new_qty = min_qty_for_notional.min(max_qty);
                                                
                                                // Final kontrol: Min notional ve margin kontrolü
                                                let final_notional = new_qty * price_quantized;
                                                let required_margin = final_notional / effective_leverage_for_qty_bid;
                                                
                                                if final_notional < min_notional || required_margin > available_margin {
                                                    // Hala yetersiz, skip et
                                                    warn!(%symbol, ?px, required_min = ?min_notional, final_notional, required_margin, available_margin, "skip bid: cannot satisfy both min_notional and margin constraint");
                                                    new_qty = 0.0; // Skip et
                                                }
                                            }
                                        }
                                        if new_qty > 0.0 {
                                            let retry_qty = Qty(rust_decimal::Decimal::from_f64_retain(new_qty).unwrap_or(rust_decimal::Decimal::ZERO));
                                            info!(%symbol, ?px, qty = ?retry_qty, tif = ?tif, min_notional, "retrying futures bid with exchange min notional");
                                            // API Rate Limit koruması
                                            rate_limit_guard(1).await; // POST /fapi/v1/order: Weight 1
                                            match v.place_limit(&symbol, Side::Buy, px, retry_qty, tif).await {
                                                Ok(order_id) => {
                                                    let info = OrderInfo { 
                                                        order_id: order_id.clone(), 
                                                        client_order_id: None,
                                                        side: Side::Buy, 
                                                        price: px, 
                                                        qty: retry_qty, 
                                                        filled_qty: Qty(Decimal::ZERO),
                                                        remaining_qty: retry_qty,
                                                        created_at: Instant::now(),
                                                        last_fill_time: None,
                                                    };
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
                                rate_limit_guard(1).await; // POST /fapi/v1/order: Weight 1
                                match v.place_limit(&symbol, Side::Buy, px, qty2, tif).await {
                                    Ok(order_id2) => {
                                        let info2 = OrderInfo { 
                                            order_id: order_id2.clone(), 
                                            client_order_id: None,
                                            side: Side::Buy, 
                                            price: px, 
                                            qty: qty2, 
                                            filled_qty: Qty(Decimal::ZERO),
                                            remaining_qty: qty2,
                                            created_at: Instant::now(),
                                            last_fill_time: None,
                                        };
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
                        rate_limit_guard(1).await; // POST /fapi/v1/order: Weight 1
                        match v.place_limit(&symbol, Side::Sell, px, qty, tif).await {
                            Ok(order_id) => {
                                let info = OrderInfo { 
                                    order_id: order_id.clone(), 
                                    client_order_id: None, // TODO: clientOrderId ekle
                                    side: Side::Sell, 
                                    price: px, 
                                    qty, 
                                    filled_qty: Qty(Decimal::ZERO),
                                    remaining_qty: qty,
                                    created_at: Instant::now(),
                                    last_fill_time: None,
                                };
                                state.active_orders.insert(order_id.clone(), info);
                                
                                // JSON log: Order created
                                if let Ok(logger) = json_logger.lock() {
                                    logger.log_order_created(
                                        symbol,
                                        &order_id,
                                        Side::Sell,
                                        px,
                                        qty,
                                        "spread_opportunity",
                                        &cfg.exec.tif,
                                    );
                                }
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
                                        // >= kontrolü: min_notional (pozisyon boyutu) >= max_notional (margin * leverage) ise bu tick'i skip et
                                        // KRİTİK DÜZELTME: Disable etme, sadece bu tick'i skip et (bakiye artarsa veya fiyat değişirse tekrar deneyebilir)
                                        let max_notional = cfg.max_usd_per_order * effective_leverage_ask;
                                        if min_notional >= max_notional {
                                            warn!(%symbol, min_notional, max_notional, max_usd_per_order = cfg.max_usd_per_order, effective_leverage = effective_leverage_ask,
                                                  "skipping tick: exchange min_notional >= max position size (margin * leverage), will retry on next tick if conditions change");
                                            // Disable etme, sadece bu emri skip et ve continue ile bir sonraki emre geç
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
                                            // KRİTİK DÜZELTME: Fırsat modunda leverage'i yarıya düşür
                                            let effective_leverage_for_qty_ask = if state.strategy.is_opportunity_mode() {
                                                effective_leverage_ask * cfg.internal.opportunity_mode_leverage_reduction
                                            } else {
                                                effective_leverage_ask
                                            };
                                            let max_qty_by_margin = ((available_margin * effective_leverage_for_qty_ask) / price_quantized / step).floor() * step;
                                            
                                            // KRİTİK DÜZELTME: Min notional retry mantığını düzelt
                                            // Önce bakiye yeterliliğini kontrol et
                                            if max_qty_by_margin * price_quantized < min_notional {
                                                // Bakiye yetersiz, min notional'ı karşılayamıyoruz → Skip et, retry yapma
                                                warn!(%symbol, ?px, required_min = ?min_notional, available_notional = max_qty_by_margin * price_quantized, "skip ask: insufficient balance for min_notional, skipping order");
                                                new_qty = 0.0; // Skip et
                                            } else {
                                                // Bakiye yeterli, min notional'a göre qty hesapla
                                                // Min notional için gerekli qty'yi hesapla (%10 güvenli margin ile)
                                                // PATCH: Güvenlik marjını 10% → 5%'e düşür
                                                let min_qty_for_notional = (min_notional * 1.05 / price_quantized / step).ceil() * step;
                                                
                                                // Cap kontrolü: max qty by cap (notional)
                                                let max_qty_by_cap = (order_cap / price_quantized / step).floor() * step;
                                                let max_qty = max_qty_by_cap.min(max_qty_by_margin);
                                                
                                                // Final qty: min_notional gereksinimi, cap ve margin constraint'i karşılamalı
                                                new_qty = min_qty_for_notional.min(max_qty);
                                                
                                                // Final kontrol: Min notional ve margin kontrolü
                                                let final_notional = new_qty * price_quantized;
                                                let required_margin = final_notional / effective_leverage_for_qty_ask;
                                                
                                                if final_notional < min_notional || required_margin > available_margin {
                                                    // Hala yetersiz, skip et
                                                    warn!(%symbol, ?px, required_min = ?min_notional, final_notional, required_margin, available_margin, "skip ask: cannot satisfy both min_notional and margin constraint");
                                                    new_qty = 0.0; // Skip et
                                                }
                                            }
                                        }
                                        if new_qty > 0.0 {
                                            let retry_qty = Qty(rust_decimal::Decimal::from_f64_retain(new_qty).unwrap_or(rust_decimal::Decimal::ZERO));
                                            info!(%symbol, ?px, qty = ?retry_qty, tif = ?tif, min_notional, "retrying futures ask with exchange min notional");
                                            // API Rate Limit koruması
                                            rate_limit_guard(1).await; // POST /fapi/v1/order: Weight 1
                                            match v.place_limit(&symbol, Side::Sell, px, retry_qty, tif).await {
                                                Ok(order_id) => {
                                                    let info = OrderInfo { 
                                                        order_id: order_id.clone(), 
                                                        client_order_id: None,
                                                        side: Side::Sell, 
                                                        price: px, 
                                                        qty: retry_qty, 
                                                        filled_qty: Qty(Decimal::ZERO),
                                                        remaining_qty: retry_qty,
                                                        created_at: Instant::now(),
                                                        last_fill_time: None,
                                                    };
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
                                rate_limit_guard(1).await; // POST /fapi/v1/order: Weight 1
                                match v.place_limit(&symbol, Side::Sell, px, qty2, tif).await {
                                    Ok(order_id2) => {
                                        let info2 = OrderInfo { 
                                            order_id: order_id2.clone(), 
                                            client_order_id: None,
                                            side: Side::Sell, 
                                            price: px, 
                                            qty: qty2, 
                                            filled_qty: Qty(Decimal::ZERO),
                                            remaining_qty: qty2,
                                            created_at: Instant::now(),
                                            last_fill_time: None,
                                        };
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

// ============================================================================
// Trading Loop Helper Functions
// ============================================================================

pub enum VenueType {
    Spot(BinanceSpot),
    Futures(BinanceFutures),
}

/// Fetch balance for a quote asset from venue
#[allow(dead_code)]
async fn fetch_quote_balance(
    venue: &VenueType,
    quote_asset: &str,
) -> f64 {
    rate_limit_guard(10).await; // GET /api/v3/account or /fapi/v2/balance: Weight 10/5
    let result = match venue {
        VenueType::Spot(v) => {
            timeout(Duration::from_secs(5), v.asset_free(quote_asset)).await
        }
        VenueType::Futures(v) => {
            timeout(Duration::from_secs(5), v.available_balance(quote_asset)).await
        }
    };
    
    match result {
        Ok(Ok(balance)) => balance.to_f64().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// Collect unique quote assets from states
#[allow(dead_code)]
fn collect_unique_quote_assets(states: &[SymbolState]) -> Vec<String> {
    let mut unique: std::collections::HashSet<String> = std::collections::HashSet::new();
    for state in states {
        unique.insert(state.meta.quote_asset.clone());
    }
    unique.into_iter().collect()
}

/// Fetch balances for all unique quote assets
#[allow(dead_code)]
async fn fetch_all_quote_balances(
    venue: &VenueType,
    quote_assets: &[String],
) -> HashMap<String, f64> {
    let mut balances = HashMap::new();
    for quote_asset in quote_assets {
        let balance = fetch_quote_balance(venue, quote_asset).await;
        balances.insert(quote_asset.clone(), balance);
    }
    balances
}

/// Check if symbol should be processed based on balance
#[allow(dead_code)]
fn should_process_symbol(
    state: &SymbolState,
    quote_balance: f64,
    min_balance: f64,
    min_order_size: f64,
    effective_leverage: f64,
    mode: &str,
) -> bool {
    let has_open_orders = !state.active_orders.is_empty();
    let has_position = !state.inv.0.is_zero();
    
    if has_open_orders || has_position {
        return true;
    }
    
    if quote_balance < min_balance {
        return false;
    }
    
    match mode {
        "futures" => {
            let total_with_leverage = quote_balance * effective_leverage;
            total_with_leverage >= min_order_size
        }
        _ => quote_balance >= min_order_size,
    }
}

/// Update fill rate after order fill
fn update_fill_rate_on_fill(
    state: &mut SymbolState,
    increase_factor: f64,
    increase_bonus: f64,
) {
    state.consecutive_no_fills = 0;
    state.last_fill_time = Some(std::time::Instant::now()); // Zaman bazlı fill rate için
    state.order_fill_rate = (state.order_fill_rate * increase_factor + increase_bonus)
        .min(1.0);
}

/// Update fill rate after order cancel
fn update_fill_rate_on_cancel(state: &mut SymbolState, decrease_factor: f64) {
    state.order_fill_rate = (state.order_fill_rate * decrease_factor).max(0.0);
}

/// Check if order should be synced
fn should_sync_orders(
    force_sync: bool,
    last_sync: Option<Instant>,
    sync_interval_sec: u64,
) -> bool {
    if force_sync {
        return true;
    }
    last_sync
        .map(|last| last.elapsed().as_secs() >= sync_interval_sec)
        .unwrap_or(true)
}

/// Check if order is stale
#[allow(dead_code)]
fn is_order_stale(order: &OrderInfo, max_age_ms: u64, has_position: bool) -> bool {
    let age_ms = order.created_at.elapsed().as_millis() as u64;
    let threshold = if has_position {
        max_age_ms / 2
    } else {
        max_age_ms
    };
    age_ms > threshold
}

/// Check if position should be closed based on profit/loss
fn should_close_position(
    current_pnl: Decimal,
    peak_pnl: Decimal,
    price_change_pct: f64,
    position_size_notional: f64,
    position_hold_duration_ms: u64,
    pnl_trend: f64,
    cfg: &config::InternalCfg,
    strategy_cfg: &config::StrategyInternalCfg,
) -> (bool, &'static str) {
    let current_pnl_f64 = current_pnl.to_f64().unwrap_or(0.0);
    let peak_pnl_f64 = peak_pnl.to_f64().unwrap_or(0.0);
    
    let take_profit_threshold = if position_size_notional > cfg.take_profit_position_size_threshold {
        cfg.take_profit_threshold_large
    } else {
        cfg.take_profit_threshold_small
    };
    
    let should_take_profit = if price_change_pct >= take_profit_threshold {
        pnl_trend < strategy_cfg.trend_analysis_threshold_negative
            || (position_hold_duration_ms > cfg.take_profit_time_threshold_ms
                && price_change_pct < cfg.take_profit_min_profit_threshold)
            || (price_change_pct >= cfg.take_profit_min_profit_threshold
                && pnl_trend < strategy_cfg.trend_analysis_threshold_strong_negative)
    } else {
        false
    };
    
    let should_trailing_stop = if peak_pnl_f64 > 0.0 && current_pnl_f64 < peak_pnl_f64 {
        let drawdown = (peak_pnl_f64 - current_pnl_f64)
            / peak_pnl_f64.abs().max(cfg.trailing_stop_min_peak);
        let threshold = if peak_pnl_f64 > cfg.trailing_stop_peak_threshold_large {
            cfg.trailing_stop_drawdown_large
        } else if peak_pnl_f64 > cfg.trailing_stop_peak_threshold_medium {
            cfg.trailing_stop_drawdown_medium
        } else {
            cfg.trailing_stop_drawdown_small
        };
        drawdown >= threshold
    } else {
        false
    };
    
    // PATCH: Stop loss'u daha erken tetikle (zarar varsa trend kontrolü gereksiz)
    let should_stop_loss = if price_change_pct <= cfg.stop_loss_threshold {
        // Sadece stop_loss_threshold yeterli, trend kontrolü gereksiz (daha sıkı)
        true
    } else {
        false
    };
    
    if should_trailing_stop && peak_pnl_f64 > cfg.trailing_stop_peak_threshold_medium {
        (true, "trailing_stop")
    } else if should_take_profit {
        (true, "take_profit")
    } else if should_stop_loss {
        (true, "stop_loss")
    } else {
        (false, "")
    }
}

/// Calculate effective leverage
/// Risk katmanı her zaman kazanır: max_leverage hard cap olarak uygulanır
fn calculate_effective_leverage(config_leverage: Option<u32>, max_leverage: u32) -> f64 {
    let requested = config_leverage.unwrap_or(max_leverage).max(1);
    // Risk katmanı kazansın: max_leverage hard cap
    requested.min(max_leverage).max(1) as f64
}

#[cfg(test)]
#[path = "position_order_tests.rs"]
mod position_order_tests;

