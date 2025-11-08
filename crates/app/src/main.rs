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
use exec::binance::{BinanceCommon, BinanceFutures, SymbolMeta};
use exec::{decimal_places, Venue};
use risk::{RiskAction, RiskLimits};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use strategy::{Context, DynMm, DynMmCfg, Strategy};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;
use logger::create_logger;
use types::{OrderInfo, SymbolState};
use utils::{clamp_qty_by_usd, compute_drawdown_bps, get_price_tick, get_qty_step, init_rate_limiter, is_usd_stable, rate_limit_guard, record_pnl_snapshot};

use std::cmp::max;
use std::collections::HashMap;
// Removed unused Ordering import
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
        flash_crash_recovery_window_ms: Some(cfg.strategy_internal.flash_crash_recovery_window_ms),
        flash_crash_recovery_min_points: Some(cfg.strategy_internal.flash_crash_recovery_min_points),
        flash_crash_recovery_min_ratio: Some(cfg.strategy_internal.flash_crash_recovery_min_ratio),
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
    // Always use futures mode
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
            flash_crash_recovery_window_ms: dyn_cfg.flash_crash_recovery_window_ms,
            flash_crash_recovery_min_points: dyn_cfg.flash_crash_recovery_min_points,
            flash_crash_recovery_min_ratio: dyn_cfg.flash_crash_recovery_min_ratio,
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

    // Always use futures, no enum needed
    let price_tick_dec = Decimal::from_f64_retain(cfg.price_tick).unwrap_or(Decimal::ZERO);
    let qty_step_dec = Decimal::from_f64_retain(cfg.qty_step).unwrap_or(Decimal::ZERO);
    let price_precision = decimal_places(price_tick_dec);
    let qty_precision = decimal_places(qty_step_dec);
    // Initialize rate limiter for futures
    init_rate_limiter();
    info!("rate limiter initialized for futures");
    
    let hedge_mode = cfg.binance.hedge_mode;
    let venue = BinanceFutures {
            base: cfg.binance.futures_base.clone(),
            common: common.clone(),
            price_tick: price_tick_dec,
            qty_step: qty_step_dec,
            price_precision,
            qty_precision,
            hedge_mode,
    };
    
    // KRİTİK: Başlangıçta position side mode'u açıkça ayarla
    // Hedge mode açıksa dual-side, kapalıysa one-way mode
    if let Err(err) = venue.set_position_side_dual(hedge_mode).await {
        warn!(hedge_mode, error = %err, "failed to set position side mode, continuing anyway");
        // Hata olsa bile devam et (hesap zaten doğru modda olabilir)
    } else {
        info!(hedge_mode, "position side mode set successfully");
    }

    let metadata = venue.symbol_metadata().await?;

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
            // Only accept PERPETUAL futures contracts
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

            // --- opsiyonel: başlangıçta bakiye tabanlı ön eleme ---
            let have_min = cfg.min_usd_per_order.unwrap_or(0.0);
            let avail = venue.available_balance(&meta.quote_asset).await?.to_f64().unwrap_or(0.0);
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
                    && m.contract_type.as_deref().map(|ct| ct == "PERPETUAL").unwrap_or(false)
            })
            .cloned()
            .collect();
        
        // --- QUOTE ASSET BAKİYE FİLTRESİ: Yetersiz bakiye olan quote asset'li sembolleri filtrele ---
        // Tüm quote asset'lerin bakiyelerini kontrol et ve yetersiz olanları baştan filtrele
        // Böylece gereksiz işlem yapılmaz
        let mut quote_asset_balances: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
        
                // Tüm benzersiz quote asset'leri bul
                let unique_quotes: std::collections::HashSet<String> = auto.iter()
                    .map(|m| m.quote_asset.clone())
                    .collect();
                
        // Her quote asset için bakiye kontrolü yap (futures)
                for quote in unique_quotes {
            let balance = venue.available_balance(&quote).await.ok()
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
            sleep(Duration::from_secs(cfg.internal.symbol_discovery_retry_interval_sec)).await;
            
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
                            && m.contract_type.as_deref().map(|ct| ct == "PERPETUAL").unwrap_or(false)
                    })
                    .cloned()
                    .collect();
                
                // Bakiye kontrolü
                let mut retry_quote_balances: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
                // Futures only - spot removed
                        let unique_quotes: std::collections::HashSet<String> = retry_auto.iter()
                            .map(|m| m.quote_asset.clone())
                            .collect();
                        for quote in unique_quotes {
                    let balance = venue.available_balance(&quote).await.ok()
                                .and_then(|b| b.to_f64())
                                .unwrap_or(0.0);
                            retry_quote_balances.insert(quote.clone(), balance);
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
                        // Only accept PERPETUAL futures contracts
                            match meta.contract_type.as_deref() {
                                Some("PERPETUAL") => {}
                                _ => continue,
                        }
                        
                        // Bakiye kontrolü (futures only)
                        let has_balance = venue.available_balance(&meta.quote_asset).await.ok()
                                    .and_then(|b| b.to_f64())
                            .unwrap_or(0.0) >= cfg.min_quote_balance_usd;
                        
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
        
        // KRİTİK: Eager warmup - Per-symbol metadata çek (exchangeInfo)
        // Başarısız olursa sembolü disable et (kuralsız sembolde trade etme)
        let symbol_rules = venue.rules_for(&meta.symbol).await.ok();
        let rules_fetch_failed = symbol_rules.is_none();
        if let Some(ref rules) = symbol_rules {
            info!(
                symbol = %meta.symbol,
                tick_size = %rules.tick_size,
                step_size = %rules.step_size,
                price_precision = rules.price_precision,
                qty_precision = rules.qty_precision,
                min_notional = %rules.min_notional,
                "fetched per-symbol metadata (eager warmup)"
            );
        } else {
            warn!(
                symbol = %meta.symbol,
                "CRITICAL: failed to fetch per-symbol metadata, symbol DISABLED (will not trade)"
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
            disabled: rules_fetch_failed, // KRİTİK: ExchangeInfo başarısızsa disable et
            symbol_rules, // Per-symbol metadata (fallback: None, global cfg kullanılır)
            rules_fetch_failed, // ExchangeInfo fetch durumu
            last_rules_retry: None, // Periyodik retry için
            test_order_passed: false, // İlk emir öncesi test order henüz yapılmadı
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
            position_orders: Vec::new(), // KRİTİK İYİLEŞTİRME: Order-to-position mapping
            // Gelişmiş risk ve kazanç takibi
            daily_pnl: Decimal::ZERO,
            total_funding_cost: Decimal::ZERO,
            position_size_notional_history: Vec::with_capacity(cfg.internal.position_size_history_max_len),
            last_pnl_alert: None,
            cumulative_pnl: Decimal::ZERO,
            // Funding cost tracking
            last_applied_funding_time: None,
            // PnL tracking
            last_daily_reset: None,
            avg_entry_price: None,
        });
    }

    // KRİTİK: Başlangıçta her sembol için leverage'i açıkça ayarla
    // Config'den leverage al: exec.default_leverage veya cfg.leverage (fallback)
    let leverage_to_set = cfg.exec.default_leverage
        .or(cfg.leverage)
        .unwrap_or(1); // Default: 1x (en güvenli)
    
    if cfg.mode == "futures" {
        info!(leverage = leverage_to_set, "setting leverage for all symbols");
        for state in &states {
            let symbol = &state.meta.symbol;
            // API Rate Limit koruması
            rate_limit_guard(1).await; // POST /fapi/v1/leverage: Weight 1
            if let Err(err) = venue.set_leverage(symbol, leverage_to_set).await {
                warn!(%symbol, leverage = leverage_to_set, error = %err, "failed to set leverage, continuing anyway");
                // Hata olsa bile devam et (leverage zaten doğru olabilir)
            }
        }
        info!("leverage set for all symbols");
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
        let futures_base = cfg.binance.futures_base.clone();
        let reconnect_delay = Duration::from_millis(cfg.websocket.reconnect_delay_ms);
        let tx = event_tx.clone();
        let kind = UserStreamKind::Futures;
        info!(
            reconnect_delay_ms = cfg.websocket.reconnect_delay_ms,
            ping_interval_ms = cfg.websocket.ping_interval_ms,
            ?kind,
            "launching user data stream task"
        );
        tokio::spawn(async move {
            let base = futures_base;
            loop {
                match UserDataStream::connect(client.clone(), &base, &api_key, kind).await {
                    Ok(mut stream) => {
                        info!(?kind, "connected to Binance user data stream");
                        // KRİTİK DÜZELTME: Reconnect callback set et - missed events sync için
                        let tx_sync = tx.clone();
                        stream.set_on_reconnect(move || {
                            // Reconnect sonrası sync event'i gönder (main loop'ta handle edilecek)
                            let _ = tx_sync.send(UserEvent::Heartbeat); // Heartbeat olarak kullan (sync trigger)
                            info!("reconnect callback triggered, sync event sent");
                        });
                        
                        // Reconnect sonrası ilk event geldiğinde sync trigger gönder (fallback)
                        let mut first_event_after_reconnect = true;
                        while let Ok(event) = stream.next_event().await {
                            // Reconnect sonrası ilk event geldiğinde sync flag'i set et (fallback)
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
        buy_total: f64,         // toplam kullanılabilir quote USD (örn 140)
    }

    let tick_ms = max(cfg.internal.min_tick_interval_ms, cfg.exec.cancel_replace_interval_ms);
    let min_usd_per_order = cfg.min_usd_per_order.unwrap_or(0.0);
    
    // KRİTİK DÜZELTME: ProfitGuarantee'yi config'den oluştur (ana akışa entegre)
    let min_profit_usd = cfg.strategy.min_profit_usd.unwrap_or(0.50); // Default: $0.50
    let maker_fee_rate = cfg.strategy.maker_fee_rate.unwrap_or(0.0002); // Default: 2 bps
    let taker_fee_rate = cfg.strategy.taker_fee_rate.unwrap_or(0.0004); // Default: 4 bps
    let profit_guarantee = utils::ProfitGuarantee::new(min_profit_usd, maker_fee_rate, taker_fee_rate);
    
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
        min_profit_usd,
        maker_fee_rate,
        taker_fee_rate,
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
                    // KRİTİK İYİLEŞTİRME: Missed events recovery - pozisyon kontrolü ile fill/cancel ayrımı
                    for state in &mut states {
                        // Önce pozisyonu al (inventory sync için)
                        let current_pos = match venue.get_position(&state.meta.symbol).await {
                            Ok(pos) => Some(pos),
                            Err(err) => {
                                warn!(symbol = %state.meta.symbol, ?err, "failed to get position for reconnect sync");
                                None
                            }
                        };
                        
                        rate_limit_guard(3).await; // GET /api/v3/openOrders: Weight 3
                        let sync_result = venue.get_open_orders(&state.meta.symbol).await;
                        
                        match sync_result {
                            Ok(api_orders) => {
                                let api_order_ids: std::collections::HashSet<String> = api_orders
                                    .iter()
                                    .map(|o| o.order_id.clone())
                                    .collect();
                                
                                // KRİTİK İYİLEŞTİRME: Removed orders'ı track et (fill mi cancel mi anlamak için)
                                let mut removed_orders = Vec::new();
                                state.active_orders.retain(|order_id, order_info| {
                                    if !api_order_ids.contains(order_id) {
                                        removed_orders.push(order_info.clone());
                                        false
                                    } else {
                                        true
                                    }
                                });
                                
                                if !removed_orders.is_empty() {
                                    // Inventory sync yap (fill olmuş olabilir)
                                    if let Some(pos) = current_pos {
                                        let old_inv = state.inv.0;
                                        state.inv = Qty(pos.qty.0);
                                        state.last_inventory_update = Some(std::time::Instant::now());
                                        
                                        // Eğer inventory değiştiyse fill olmuş
                                        if old_inv != pos.qty.0 {
                                            state.consecutive_no_fills = 0;
                                            state.order_fill_rate = (state.order_fill_rate * 0.95 + 0.05).min(1.0);
                                            
                                            // KRİTİK İYİLEŞTİRME: Buy order'lar fill olduysa position_orders'a ekle
                                            let inv_increased = pos.qty.0 > old_inv;
                                            if inv_increased {
                                                for removed_order in &removed_orders {
                                                    if removed_order.side == Side::Buy {
                                                        if !state.position_orders.contains(&removed_order.order_id) {
                                                            state.position_orders.push(removed_order.order_id.clone());
                                                            debug!(
                                                                symbol = %state.meta.symbol,
                                                                order_id = %removed_order.order_id,
                                                                "reconnect sync: buy order filled, added to position_orders"
                                                            );
                                                        }
                                                    }
                                                }
                                            }
                                            // Pozisyon sıfıra düştüyse position_orders'ı temizle
                                            if pos.qty.0.is_zero() && !old_inv.is_zero() {
                                                state.position_orders.clear();
                                                debug!(symbol = %state.meta.symbol, "reconnect sync: position closed, cleared position_orders");
                                            }
                                            
                                            info!(
                                                symbol = %state.meta.symbol,
                                                removed_orders = removed_orders.len(),
                                                inv_change = %(pos.qty.0 - old_inv),
                                                "reconnect sync: orders removed and inventory changed - likely filled"
                                            );
                                        } else {
                                            // Inventory değişmediyse cancel olmuş
                                            update_fill_rate_on_cancel(state, cfg.internal.fill_rate_decrease_factor);
                                            info!(
                                                symbol = %state.meta.symbol,
                                                removed_orders = removed_orders.len(),
                                                "reconnect sync: orders removed but inventory unchanged - likely canceled"
                                            );
                                        }
                                    } else {
                                        // Position alınamadı, eski mantıkla devam et (fill olarak varsay)
                                        state.consecutive_no_fills = 0;
                                        state.order_fill_rate = (state.order_fill_rate * cfg.internal.fill_rate_reconnect_factor + cfg.internal.fill_rate_reconnect_bonus).min(1.0);
                                        warn!(
                                            symbol = %state.meta.symbol,
                                            removed_orders = removed_orders.len(),
                                            "reconnect sync: orders removed but position unavailable, assuming filled"
                                        );
                                    }
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
                    commission,
                } => {
                    if let Some(idx) = symbol_index.get(&symbol) {
                        let state = &mut states[*idx];
                        
                        // KRİTİK: Event-based state management - sadece event'lerle state güncelle
                        // KRİTİK İYİLEŞTİRME: Duplicate detection - cumulative_filled_qty + order_status kontrolü
                        // Aynı cumulative_filled_qty birden fazla gelebilir (network retry), bu yüzden
                        // sadece cumulative qty değil, order state'i de kontrol et
                        let is_duplicate = if let Some(existing_order) = state.active_orders.get(&order_id) {
                            // Eğer cumulative qty aynı VE status değişmemişse duplicate
                            // (Status değişmişse yeni bir event olabilir, örneğin PARTIALLY_FILLED -> FILLED)
                            existing_order.filled_qty.0 >= cumulative_filled_qty.0
                        } else {
                            // Order yoksa, bu reconnect sonrası olabilir
                            // Inventory güncellemesi için event'i kabul et ama log yap
                            warn!(
                                %symbol,
                                order_id = %order_id,
                                cumulative_filled_qty = %cumulative_filled_qty.0,
                                "fill event for unknown order (reconnect?)"
                            );
                            false // Inventory'yi güncelle ama order state'i yok
                        };
                        
                        if is_duplicate {
                            warn!(
                                %symbol,
                                order_id = %order_id,
                                cumulative_filled_qty = %cumulative_filled_qty.0,
                                order_status = %order_status,
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
                        
                        // KRİTİK DÜZELTME: Fill event'lerinden gerçekleşen PnL hesapla
                        // Pozisyon kapatıldığında (inv sıfıra düştüğünde veya tersine döndüğünde) PnL hesapla
                        let old_inv = state.inv.0;
                        let mut inv = old_inv;
                        if side == Side::Buy {
                            inv += fill_increment;
                        } else {
                            inv -= fill_increment;
                        }
                        
                        // PnL hesaplama: Pozisyon kapatıldığında (long → sell fill veya short → buy fill)
                        let realized_pnl = if let Some(avg_entry) = state.avg_entry_price {
                            // Pozisyon var, entry price biliniyor
                            let closed_qty = if (old_inv.is_sign_positive() && side == Side::Sell) || 
                                               (old_inv.is_sign_negative() && side == Side::Buy) {
                                // Pozisyon kapatılıyor
                                fill_increment.min(old_inv.abs())
                            } else {
                                Decimal::ZERO // Pozisyon açılıyor/artıyor, PnL yok
                            };
                            
                            if closed_qty > Decimal::ZERO {
                                // Gerçekleşen PnL = (fill_price - entry_price) * closed_qty - komisyon
                                let price_diff = if old_inv.is_sign_positive() {
                                    // Long pozisyon kapatılıyor (sell fill)
                                    price.0 - avg_entry
                                } else {
                                    // Short pozisyon kapatılıyor (buy fill)
                                    avg_entry - price.0
                                };
                                let gross_pnl = price_diff * closed_qty;
                                
                                // KRİTİK DÜZELTME: Gerçek komisyon kullan (executionReport'tan gelen)
                                // commission zaten UserEvent::OrderFill'den geliyor (executionReport'tan "n" field'ı)
                                // commission = last executed qty için komisyon (incremental)
                                // closed_qty ile orantılı olarak hesapla (eğer partial fill ise)
                                let actual_commission = if fill_increment > Decimal::ZERO {
                                    // Proportional commission: (commission / fill_increment) * closed_qty
                                    (commission / fill_increment) * closed_qty
                                } else {
                                    commission // Full fill, direkt kullan
                                };
                                
                                let net_pnl = gross_pnl - actual_commission;
                                
                                // Daily ve cumulative PnL'e ekle
                                state.daily_pnl += net_pnl;
                                state.cumulative_pnl += net_pnl;
                                
                                info!(
                                    %symbol,
                                    fill_price = %price.0,
                                    entry_price = %avg_entry,
                                    closed_qty = %closed_qty,
                                    gross_pnl = %gross_pnl,
                                    actual_commission = %actual_commission,
                                    net_pnl = %net_pnl,
                                    daily_pnl = %state.daily_pnl,
                                    cumulative_pnl = %state.cumulative_pnl,
                                    "realized PnL from fill event (using actual commission from executionReport)"
                                );
                                
                                net_pnl
                            } else {
                                Decimal::ZERO
                            }
                        } else {
                            Decimal::ZERO // Entry price bilinmiyor, PnL hesaplanamaz
                        };
                        
                        // Inventory güncelle (sadece incremental fill miktarı)
                        // NOT: Order state'de yoksa bile inventory güncelle (reconnect sonrası olabilir)
                        state.inv = Qty(inv);
                        state.last_inventory_update = Some(std::time::Instant::now());
                        
                        // Avg entry price güncelle (pozisyon açılıyor/artıyor)
                        if (old_inv.is_zero() && !inv.is_zero()) || 
                           (old_inv.is_sign_positive() && side == Side::Buy && inv > old_inv) ||
                           (old_inv.is_sign_negative() && side == Side::Sell && inv < old_inv) {
                            // Pozisyon açılıyor veya artıyor → avg entry price güncelle
                            if let Some(ref mut avg_entry) = state.avg_entry_price {
                                // Weighted average: (old_qty * old_avg + new_qty * new_price) / total_qty
                                let old_qty = old_inv.abs();
                                let new_qty = fill_increment;
                                let total_qty = inv.abs();
                                if total_qty > Decimal::ZERO {
                                    *avg_entry = (*avg_entry * old_qty + price.0 * new_qty) / total_qty;
                                }
                            } else {
                                // İlk pozisyon → entry price = fill price
                                state.avg_entry_price = Some(price.0);
                            }
                        }
                        
                        // Pozisyon sıfıra düştüyse avg_entry_price'ı sıfırla
                        if inv.is_zero() && !old_inv.is_zero() {
                            state.avg_entry_price = None;
                        }
                        
                        // KRİTİK İYİLEŞTİRME: Order-to-position mapping - pozisyon oluşturan order'ları track et
                        // Buy fill → pozisyon artar → order'ı ekle
                        // Sell fill → pozisyon azalır → order ekleme (pozisyon kapatıyor, oluşturmuyor)
                        if side == Side::Buy && fill_increment > Decimal::ZERO {
                            // Buy order fill oldu ve pozisyon arttı → bu order pozisyonu oluşturdu/arttırdı
                            if !state.position_orders.contains(&order_id) {
                                state.position_orders.push(order_id.clone());
                                debug!(
                                    %symbol,
                                    order_id = %order_id,
                                    fill_increment = %fill_increment,
                                    old_inv = %old_inv,
                                    new_inv = %inv,
                                    total_position_orders = state.position_orders.len(),
                                    "position entry: buy order filled, added to position_orders"
                                );
                            }
                        }
                        // Pozisyon sıfıra düştüyse position_orders'ı temizle
                        if inv.is_zero() && !old_inv.is_zero() {
                            state.position_orders.clear();
                            debug!(
                                %symbol,
                                "position closed: cleared position_orders"
                            );
                        }
                        
                        // Order state güncelle (varsa)
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
                            
                            // Status kontrolü: FILLED ise kaldır
                            order_status == "FILLED" || remaining_qty.is_zero()
                        } else {
                            // Order state'de yok, ama inventory güncelledik
                            // API'den sync gerekecek
                            false
                        };
                        
                        // Fill rate güncelle (order state'e göre)
                        if should_remove {
                            // Full fill: Normal fill rate güncellemesi
                            update_fill_rate_on_fill(
                                state,
                                cfg.internal.fill_rate_increase_factor,
                                cfg.internal.fill_rate_increase_bonus,
                            );
                        } else if fill_increment > Decimal::ZERO {
                            // Partial fill: Daha hafif fill rate güncellemesi
                            // Order state'de yoksa bile (reconnect sonrası) hafif güncelleme yap
                            state.order_fill_rate = (state.order_fill_rate * 0.98 + 0.02).min(1.0);
                        }
                        
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
                        // KRİTİK İYİLEŞTİRME: Idempotency kontrolü - client_order_id validation
                        // Aynı order_id farklı semboller için kullanılabilir (Binance'ta sembol bazlı unique)
                        // Bu yüzden client_order_id kontrolü kritik
                        let should_remove = if let Some(order_info) = state.active_orders.get(&order_id) {
                            // client_order_id varsa kontrol et
                            if let Some(ref client_id) = client_order_id {
                                if let Some(ref order_client_id) = order_info.client_order_id {
                                    // Her ikisi de varsa eşleşmeli
                                    client_id == order_client_id
                                } else {
                                    // Event'te var ama order'da yok - muhtemelen yanlış event
                                    warn!(
                                        %symbol,
                                        order_id = %order_id,
                                        client_order_id = %client_id,
                                        "cancel event has client_order_id but order doesn't, ignoring"
                                    );
                                    false
                                }
                            } else {
                                // Event'te client_order_id yok - legacy event, order_id ile eşleştir
                                // NOT: Order'da client_order_id varsa bile, legacy event'i kabul et
                                // (geriye dönük uyumluluk için)
                                true
                            }
                        } else {
                            // Order zaten yok
                            false
                        };
                        
                        if should_remove {
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
            let balance = match tokio::time::timeout(Duration::from_secs(5), venue.available_balance(quote_asset)).await {
                        Ok(Ok(b)) => b.to_f64().unwrap_or(0.0),
                        _ => 0.0,
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
            
            // Progress log: Her N sembolde bir veya ilk N sembolde
            if symbol_index <= cfg.internal.progress_log_first_n_symbols || symbol_index % cfg.internal.progress_log_interval == 0 {
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
            // KRİTİK: symbol'i clone et çünkü state'i mutable borrow edeceğiz
            let symbol = state.meta.symbol.clone();
            let base_asset = &state.meta.base_asset;
            let quote_asset = state.meta.quote_asset.clone();

            // --- ERKEN BAKİYE KONTROLÜ: Bakiye yoksa gereksiz işlem yapma ---
            // KRİTİK DÜZELTME: Cache'den oku (race condition önlendi)
            // DEBUG: İlk birkaç sembol için detaylı log
            let is_debug_symbol = symbol_index <= cfg.internal.debug_symbol_count;
            
            // Cache'den bakiye oku (loop başında çekildi)
            let q_free = quote_balances.get(&quote_asset).copied().unwrap_or(0.0);
            
                    // Futures için: leverage ile toplam kullanılabilir miktar
            let has_balance = {
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
                // KRİTİK İYİLEŞTİRME: Missed events recovery - önce pozisyonu al (inventory sync için)
                // Fill olmuş emirler için inventory güncellemesi yapılabilmesi için pozisyon gerekli
                let current_pos = match venue.get_position(&symbol).await {
                    Ok(pos) => Some(pos),
                    Err(err) => {
                        warn!(%symbol, ?err, "failed to get position for order sync, skipping inventory check");
                        // Position alınamazsa devam et ama inventory check yapma
                        None
                    }
                };
                
                // API Rate Limit koruması
                rate_limit_guard(3).await; // GET /api/v3/openOrders: Weight 3
                let sync_result = venue.get_open_orders(&symbol).await;
                
                match sync_result {
                    Ok(api_orders) => {
                        // API'den gelen emirlerle local state'i senkronize et
                        let api_order_ids: std::collections::HashSet<String> = api_orders
                            .iter()
                            .map(|o| o.order_id.clone())
                            .collect();
                        
                        // KRİTİK İYİLEŞTİRME: Removed orders'ı track et (fill mi cancel mi anlamak için)
                        let mut removed_orders = Vec::new();
                        state.active_orders.retain(|order_id, order_info| {
                            if !api_order_ids.contains(order_id) {
                                removed_orders.push(order_info.clone());
                                false // Remove
                            } else {
                                true // Keep
                            }
                        });
                        
                        if !removed_orders.is_empty() {
                            // Inventory sync yap (fill olmuş olabilir)
                            if let Some(pos) = current_pos {
                                let old_inv = state.inv.0;
                                state.inv = Qty(pos.qty.0);
                                state.last_inventory_update = Some(std::time::Instant::now());
                                
                                // Eğer inventory değiştiyse fill olmuş
                                if old_inv != pos.qty.0 {
                                    state.consecutive_no_fills = 0;
                                    state.order_fill_rate = (state.order_fill_rate * 0.95 + 0.05).min(1.0);
                                    
                                    // KRİTİK İYİLEŞTİRME: Buy order'lar fill olduysa position_orders'a ekle
                                    // Inventory arttıysa buy order'lar fill olmuş demektir
                                    let inv_increased = pos.qty.0 > old_inv;
                                    if inv_increased {
                                        for removed_order in &removed_orders {
                                            if removed_order.side == Side::Buy {
                                                if !state.position_orders.contains(&removed_order.order_id) {
                                                    state.position_orders.push(removed_order.order_id.clone());
                                                    debug!(
                                                        %symbol,
                                                        order_id = %removed_order.order_id,
                                                        "order sync: buy order filled, added to position_orders"
                                                    );
                                                }
                                            }
                                        }
                                    }
                                    // Pozisyon sıfıra düştüyse position_orders'ı temizle
                                    if pos.qty.0.is_zero() && !old_inv.is_zero() {
                                        state.position_orders.clear();
                                        debug!(%symbol, "order sync: position closed, cleared position_orders");
                                    }
                                    
                                    info!(
                                        %symbol,
                                        removed_orders = removed_orders.len(),
                                        inv_change = %(pos.qty.0 - old_inv),
                                        old_inv = %old_inv,
                                        new_inv = %pos.qty.0,
                                        "orders removed and inventory changed - likely filled"
                                    );
                                } else {
                                    // Inventory değişmediyse cancel olmuş
                                    update_fill_rate_on_cancel(state, cfg.internal.fill_rate_decrease_factor);
                                    info!(
                                        %symbol,
                                        removed_orders = removed_orders.len(),
                                        "orders removed but inventory unchanged - likely canceled"
                                    );
                                }
                            } else {
                                // Position alınamadı, eski mantıkla devam et (fill olarak varsay)
                                state.consecutive_no_fills = 0;
                                state.order_fill_rate = (state.order_fill_rate * cfg.internal.fill_rate_increase_factor + cfg.internal.fill_rate_increase_bonus * (removed_orders.len() as f64).min(1.0)).min(1.0);
                                warn!(
                                    %symbol,
                                    removed_orders = removed_orders.len(),
                                    "orders removed but position unavailable, assuming filled"
                                );
                            }
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
                        if venue.cancel(&order.order_id, &symbol).await.is_ok() {
                                    canceled_count += 1;
                                    state.active_orders.remove(&order.order_id);
                                } else {
                                    warn!(%symbol, order_id = %order.order_id, "failed to cancel stale futures order");
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
                if stale_count > 0 && state.active_orders.len() > cfg.internal.max_stale_orders_threshold {
                    warn!(
                        %symbol,
                        total_orders = state.active_orders.len(),
                        "too many active orders, canceling all to reset"
                    );
                    // API Rate Limit koruması
                    // cancel_all multiple calls yapabilir, her biri Weight 1
                    rate_limit_guard(1).await; // DELETE /api/v3/order: Weight 1 (per order)
                    if let Err(err) = venue.cancel_all(&symbol).await {
                                warn!(%symbol, ?err, "failed to cancel all orders");
                            } else {
                                state.active_orders.clear();
                    }
                }
            }

            // API Rate Limit koruması
            // best_prices: Weight 1 (Futures)
            rate_limit_guard(1).await;
            let (bid, ask) = match venue.best_prices(&symbol).await {
                    Ok(prices) => prices,
                    Err(err) => {
                        warn!(%symbol, ?err, "failed to fetch best prices, skipping tick");
                        continue;
                    }
            };
            info!(%symbol, ?bid, ?ask, "fetched best prices");
            
            // Pozisyon bilgisini al (bir kere, tüm analizler için kullanılacak)
            // KRİTİK DÜZELTME: Bakiye yoksa bile pozisyon kontrolü yap (açık pozisyon olabilir)
            // API Rate Limit koruması
            // get_position: Weight 5 (Futures positionRisk)
            rate_limit_guard(5).await;
            let pos = match venue.get_position(&symbol).await {
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
            // mark_price: Weight 1 (Futures)
            rate_limit_guard(1).await;
            let (mark_px, funding_rate, next_funding_time) = match venue.fetch_premium_index(&symbol).await {
                    Ok((mark, funding, next_time)) => (mark, funding, next_time),
                    Err(_) => {
                        // Fallback: bid/ask mid price
                        let mid = (bid.0 + ask.0) / Decimal::from(2u32);
                        (Px(mid), None, None)
                    }
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
                    // KRİTİK İYİLEŞTİRME: 1/2 → 2/3 (daha az agresif, yarıya düşürmek çok agresif)
                    let max_age_for_stale = if position_size_notional > 0.0 {
                        (cfg.exec.max_order_age_ms * 2) / 3 // Pozisyon varsa 2/3 süre (daha az agresif)
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
                    if let Err(err) = venue.cancel(order_id, &symbol).await {
                                warn!(%symbol, order_id = %order_id, ?err, "failed to cancel order");
                            } else {
                                state.active_orders.remove(order_id);
                                state.last_order_price_update.remove(order_id);
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
                top_bids: None, // Top-K levels not available from best_prices()
                top_asks: None, // Top-K levels not available from best_prices()
            };

            // Pozisyon ve mark price zaten yukarıda alındı, tekrar almayalım
            // Envanter senkronizasyonu yap
            // KRİTİK DÜZELTME: WebSocket reconnect sonrası pozisyon sync'i
            // Reconnect sonrası sadece emirleri değil, pozisyonları da sync et
            // force_sync_all true ise TAM sync yap (reconnect sonrası)
            // RACE CONDITION ÖNLEME: Son envanter güncellemesinden 500ms geçmeden sync yapma
            // WebSocket event'i ile REST API sync arasında 200-500ms gecikme normal
            let inv_diff = (state.inv.0 - pos.qty.0).abs();
            
            // KRİTİK İYİLEŞTİRME: Threshold çok küçük (0.00000001) rounding error'ları false positive yaratabilir
            // Daha makul bir threshold kullan: minimum step_size veya 0.0001 (0.01% of a unit)
            // Config'den gelen threshold'u kullan, yoksa daha makul bir default (0.0001)
            let reconcile_threshold = Decimal::from_str_radix(&cfg.internal.inventory_reconcile_threshold, 10)
                .unwrap_or(Decimal::new(1, 4)); // 0.0001 instead of 0.00000001 (100x larger, prevents rounding errors)
            let is_reconnect_sync = force_sync_all;
            
            // Son envanter güncellemesinden bu yana geçen süre (race condition önleme)
            // KRİTİK İYİLEŞTİRME: 100ms → 500ms (WebSocket/REST API gecikmesi için)
            const MIN_SYNC_INTERVAL_MS: u128 = 500; // 200-500ms gecikme normal, 500ms güvenli
            let time_since_last_update = state.last_inventory_update
                .map(|t| t.elapsed().as_millis())
                .unwrap_or(1000); // Eğer hiç güncelleme yoksa, sync yap
            
            // Reconnect sonrası veya normal durumda uyumsuzluk varsa sync yap
            // KRİTİK İYİLEŞTİRME: Force sync mantığı - reconnect sonrası her zaman sync yap
            // Normal durumda daha toleranslı ol (threshold + timing kontrolü)
            if is_reconnect_sync {
                // Reconnect sonrası: Her zaman sync yap (uyumsuzluk olsun ya da olmasın)
                // Timing kontrolü yok - reconnect sonrası hemen sync yap
                info!(
                    %symbol,
                    ws_inv = %state.inv.0,
                    api_inv = %pos.qty.0,
                    diff = %inv_diff,
                    "force syncing position after reconnect (full sync)"
                );
                let old_inv = state.inv.0;
                state.inv = pos.qty; // Force sync
                state.last_inventory_update = Some(std::time::Instant::now());
                // KRİTİK İYİLEŞTİRME: Pozisyon sıfıra düştüyse position_orders'ı temizle
                if pos.qty.0.is_zero() && !old_inv.is_zero() {
                    state.position_orders.clear();
                    debug!(%symbol, "position closed via reconnect sync: cleared position_orders");
                }
            } else if inv_diff > reconcile_threshold && time_since_last_update > MIN_SYNC_INTERVAL_MS {
                // Normal durumda: Sadece uyumsuzluk varsa ve son güncellemeden 500ms geçtiyse sync yap
                // Daha toleranslı: threshold daha büyük (rounding error'ları ignore et)
                warn!(
                    %symbol,
                    ws_inv = %state.inv.0,
                    api_inv = %pos.qty.0,
                    diff = %inv_diff,
                    threshold = %reconcile_threshold,
                    time_since_last_update_ms = time_since_last_update,
                    "inventory mismatch detected, syncing with API position (race condition safe)"
                );
                let old_inv = state.inv.0;
                state.inv = pos.qty; // Force sync
                state.last_inventory_update = Some(std::time::Instant::now());
                // KRİTİK İYİLEŞTİRME: Pozisyon sıfıra düştüyse position_orders'ı temizle
                if pos.qty.0.is_zero() && !old_inv.is_zero() {
                    state.position_orders.clear();
                    debug!(%symbol, "position closed via normal sync: cleared position_orders");
                }
            }

            record_pnl_snapshot(&mut state.pnl_history, &pos, mark_px, cfg.internal.pnl_history_max_len);
            
            // --- AKILLI POZİSYON ANALİZİ: Durumu detaylı incele ---
            let current_pnl = (mark_px.0 - pos.entry.0) * pos.qty.0;
            let pnl_f64 = current_pnl.to_f64().unwrap_or(0.0);
            // position_size_notional zaten yukarıda hesaplandı, tekrar hesaplamaya gerek yok
            
            // KRİTİK DÜZELTME: Günlük PnL reset mekanizması (gün başında reset)
            // Her gün başında (00:00 UTC) daily_pnl'i sıfırla
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            
            let should_reset_daily = if let Some(last_reset) = state.last_daily_reset {
                // Son reset'ten bu yana 24 saat geçti mi?
                const DAY_MS: u64 = 24 * 3600 * 1000;
                now_ms.saturating_sub(last_reset) >= DAY_MS
            } else {
                // İlk kez, reset yap
                true
            };
            
            if should_reset_daily {
                let old_daily_pnl = state.daily_pnl;
                state.daily_pnl = Decimal::ZERO;
                state.last_daily_reset = Some(now_ms);
                info!(
                    %symbol,
                    old_daily_pnl = %old_daily_pnl,
                    "daily PnL reset (new day started)"
                );
            }
            
            // KRİTİK DÜZELTME: Avg entry price'ı pozisyon bilgisinden güncelle
            // Binance'tan gelen pozisyon bilgisinde entry price var, bunu kullan
            if !pos.qty.0.is_zero() {
                state.avg_entry_price = Some(pos.entry.0);
            } else {
                state.avg_entry_price = None;
            }
            
            // --- GELİŞMİŞ RİSK VE KAZANÇ TAKİBİ: Detaylı analiz ---
            
            // NOT: daily_pnl ve cumulative_pnl artık fill event'lerinden akümüle ediliyor
            // Burada sadece anlık PnL hesaplanıyor (risk analizi için)
            
            // 2. Funding cost takibi (futures için)
            // KRİTİK DÜZELTME: Funding 8 saatte bir işler; sadece funding anında tek seferde uygula
            // next_funding_time: Gelecekteki funding time (Unix timestamp ms)
            // Bu funding time geçmişse, bir önceki funding (next_funding_ts - 8 hours) uygulanmış demektir
            // Kontrol: Son uygulanan funding time'dan sonra yeni bir funding geldi mi?
            if let (Some(funding_rate), Some(next_funding_ts)) = (funding_rate, next_funding_time) {
                const FUNDING_INTERVAL_MS: u64 = 8 * 3600 * 1000; // 8 saat = 28800000 ms
                let this_funding_ts = next_funding_ts.saturating_sub(FUNDING_INTERVAL_MS); // Bu funding time
                
                // Bu funding time daha önce uygulanmamış mı?
                let should_apply = if let Some(last_applied) = state.last_applied_funding_time {
                    this_funding_ts > last_applied // Yeni bir funding time
                } else {
                    true // İlk kez
                };
                
                if should_apply && position_size_notional > 0.0 {
                    // Funding cost = funding_rate * position_size_notional (8 saatte bir, tek seferde)
                    let funding_cost = funding_rate * position_size_notional;
                    state.total_funding_cost += Decimal::from_f64_retain(funding_cost).unwrap_or(Decimal::ZERO);
                    state.last_applied_funding_time = Some(this_funding_ts);
                    
                    info!(
                        %symbol,
                        funding_rate,
                        this_funding_ts,
                        next_funding_ts,
                        position_size_notional,
                        funding_cost,
                        total_funding_cost = %state.total_funding_cost,
                        "funding cost applied (8-hour interval, single application)"
                    );
                }
            }
            
            // 3. Pozisyon boyutu geçmişi (risk analizi için)
            state.position_size_notional_history.push(position_size_notional);
            if state.position_size_notional_history.len() > cfg.internal.position_size_history_max_len {
                state.position_size_notional_history.remove(0);
            }
            
            // NOT: cumulative_pnl artık fill event'lerinden akümüle ediliyor
            // Burada sadece anlık PnL hesaplanıyor (risk analizi için)
            
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
                        if let Err(err) = venue.cancel_all(&symbol).await {
                            warn!(%symbol, ?err, "failed to cancel all orders before force-close");
                        }
                        // Sonra pozisyonu kapat
                        rate_limit_guard(1).await;
                        if let Err(err) = venue.close_position(&symbol).await {
                            error!(%symbol, ?err, "failed to close position due to hard limit");
                        } else {
                            info!(%symbol, "closed position due to hard limit");
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
                            if let Err(err) = venue.cancel(order_id, &symbol).await {
                                warn!(%symbol, order_id = %order_id, ?err, "failed to cancel order in medium limit");
                        } else {
                                state.active_orders.remove(order_id);
                                state.last_order_price_update.remove(order_id);
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
                if let Err(err) = venue.close_position(&symbol).await {
                    error!(%symbol, ?err, "failed to close position due to size risk");
                } else {
                    info!(%symbol, "closed position due to size risk");
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
                            &symbol,
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
                state.position_orders.clear(); // KRİTİK İYİLEŞTİRME: Pozisyon kapandı, order tracking'i temizle
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
                    let cancel_result = venue.cancel_all(&symbol).await;
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
                    // Futures için özel kontrol: Pozisyon kapatma sonrası doğrulama
                    let result = venue.close_position(&symbol).await;
                    
                    // KRİTİK: Pozisyon kapatma sonrası doğrulama
                    if result.is_ok() {
                        // Kısa bir bekleme (exchange'in işlemesi için)
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        
                        // Pozisyon durumunu kontrol et
                        rate_limit_guard(5).await; // GET /fapi/v2/positionRisk: Weight 5
                        match venue.get_position(&symbol).await {
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
                    
                    let close_result = result;
                    
                        match close_result {
                            Ok(_) => {
                            // JSON log: Position closed
                            if let Ok(logger) = json_logger.lock() {
                                let side = if pos.qty.0.is_sign_positive() { "long" } else { "short" };
                                let leverage = pos.leverage;
                                logger.log_position_closed(
                                    &symbol,
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
                                    &symbol,
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
                    // NOT: daily_pnl sıfırlanmıyor - fill event'lerinden akümüle ediliyor, gün başında reset ediliyor
                    state.position_orders.clear(); // KRİTİK İYİLEŞTİRME: Pozisyon kapandı, order tracking'i temizle
                    state.avg_entry_price = None; // Pozisyon kapandı, entry price sıfırla
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
                        &symbol,
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
            // KRİTİK İYİLEŞTİRME: Time-based decay - 30 saniye fill yoksa fill rate'i düşür
            // Bu, tick-based tracking'den daha doğru (tick frequency değişebilir)
            // Decay sadece 30 saniyelik aralıklarda bir kez uygulanır (her tick'te değil)
            if let Some(last_fill) = state.last_fill_time {
                let seconds_since_fill = last_fill.elapsed().as_secs();
                const TIME_BASED_DECAY_THRESHOLD_SEC: u64 = 30; // 30 saniye fill yoksa decay
                const DECAY_INTERVAL_SEC: u64 = 30; // Her 30 saniyede bir decay uygula
                
                if seconds_since_fill >= TIME_BASED_DECAY_THRESHOLD_SEC {
                    // Her 30 saniyelik aralıkta bir kez decay uygula (her tick'te değil)
                    // Örnek: 30-59 saniye arası → 1 kez, 60-89 saniye arası → 1 kez, vb.
                    // Period hesapla: 30-59 → period 1, 60-89 → period 2, vb.
                    let current_period = seconds_since_fill / DECAY_INTERVAL_SEC;
                    // Bir önceki tick'teki period'u tahmin et (1 saniye önce)
                    let previous_period = seconds_since_fill.saturating_sub(1) / DECAY_INTERVAL_SEC;
                    
                    // Eğer yeni bir 30 saniyelik period'a geçtiysek decay uygula
                    // Bu, her 30 saniyede bir kez decay yapılmasını garanti eder
                    if current_period > previous_period {
                        // 30 saniyedir fill yok - fill rate'i düşür
                        state.order_fill_rate *= 0.9; // %10 azalt
                        state.consecutive_no_fills += 1; // Geriye dönük uyumluluk için
                        
                        debug!(
                            symbol = %state.meta.symbol,
                            fill_rate = state.order_fill_rate,
                            seconds_since_fill,
                            decay_period = current_period,
                            consecutive_no_fills = state.consecutive_no_fills,
                            "time-based fill rate decay: no fills for {} seconds (period {})",
                            seconds_since_fill,
                            current_period
                        );
                    }
                }
            }
            
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
                if let Err(err) = venue.cancel_all(&symbol).await {
                            warn!(%symbol, ?err, "failed to cancel all orders during halt");
                        }
                        rate_limit_guard(1).await; // POST /fapi/v1/order: Weight 1
                if let Err(err) = venue.close_position(&symbol).await {
                            warn!(%symbol, ?err, "failed to close position during halt");
                }
                continue;
            }

            // KRİTİK: Kuralsız sembolde trade etme - disabled veya rules_fetch_failed ise skip
            if state.disabled || state.rules_fetch_failed {
                // Periyodik retry: 30-60 saniyede bir rules'ı yeniden çek
                let should_retry = state.last_rules_retry
                    .map(|last| last.elapsed().as_secs() >= 45) // 45 saniye
                    .unwrap_or(true); // İlk kez
                
                if should_retry {
                    state.last_rules_retry = Some(std::time::Instant::now());
                    info!(%symbol, "retrying exchangeInfo fetch for disabled symbol");
                    match venue.rules_for(&symbol).await {
                        Ok(new_rules) => {
                            state.symbol_rules = Some(new_rules);
                            state.disabled = false;
                            state.rules_fetch_failed = false;
                            info!(%symbol, "exchangeInfo fetch succeeded, symbol re-enabled");
                        }
                        Err(e) => {
                            debug!(%symbol, error = %e, "exchangeInfo fetch still failed, will retry later");
                        }
                    }
                }
                continue; // Bu tick'te trade etme
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
            // Futures only - spot removed
            let caps = {
                    let avail = match venue.available_balance(&quote_asset).await {
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
                        // buy_total: Hesaptan giden para (margin)
                        // - 20 USD varsa → 20 USD kullanılır (tamamı)
                        // - 100 USD varsa → 100 USD kullanılır (tamamı)
                        // - 200 USD varsa → 100 USD kullanılır (max limit), kalan 100 başka semboller için
                        buy_total: max_usable_from_account,
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
                buy_total = caps.buy_total,
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
            // Futures only - no base balance check needed
            if !buy_cap_ok && !sell_cap_ok {
                info!(
                    %symbol,
                    buy_total = caps.buy_total,
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
                
                // KRİTİK DÜZELTME: Dinamik min_spread_bps hesapla (ProfitGuarantee ile)
                // Sabit 60 bps yerine, pozisyon boyutuna göre dinamik hesapla
                // Formül: min_spread = calculate_min_spread_bps(position_size_usd) - slippage_reserve
                let dyn_min_spread_bps = profit_guarantee.calculate_min_spread_bps(position_size_usd) - cfg.risk.slippage_bps_reserve;
                // Config'deki min_spread_bps minimum eşik olarak kullan (fallback, dinamik'ten küçükse)
                let min_spread_bps_config = cfg.strategy.min_spread_bps.unwrap_or(60.0);
                let min_spread_bps = dyn_min_spread_bps.max(min_spread_bps_config);
                
                let stop_loss_threshold = cfg.internal.stop_loss_threshold;
                let min_risk_reward_ratio = cfg.internal.min_risk_reward_ratio;
                
                let (should_place, reason) = utils::should_place_trade(
                    spread_bps,
                    position_size_usd,
                    min_spread_bps,
                    stop_loss_threshold,
                    min_risk_reward_ratio,
                    &profit_guarantee, // KRİTİK: ProfitGuarantee'yi parametre olarak geç
                );
                
                if !should_place {
                    // JSON log: Trade rejected
                    if let Ok(logger) = json_logger.lock() {
                        logger.log_trade_rejected(
                            &symbol,
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
            
            // QTY CLAMP SIRASI GARANTİSİ: 1) USD clamp, 2) Quantize, 3) Min notional check
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
                    // KRİTİK DÜZELTME: Futures için gerçek kullanılabilir notional (margin * leverage) kullan
                    // Futures için: caps.buy_notional = per_order_notional (300), ama gerçek kullanılabilir = buy_total * leverage (26.63 * 3 = 79.89)
                    // Futures için gerçek kullanılabilir notional (margin * leverage)
                    let effective_buy_notional = caps.buy_total * effective_leverage;
                    let nq = clamp_qty_by_usd(q, px, effective_buy_notional, qty_step_f64);
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
                    // QTY CLAMP SIRASI GARANTİSİ: 1) USD clamp, 2) Quantize, 3) Min notional check
                    // 1. USD clamp
                    // KRİTİK DÜZELTME: Futures için gerçek kullanılabilir notional (margin * leverage) kullan
                    // Futures için: caps.sell_notional = per_order_notional (300), ama gerçek kullanılabilir = buy_total * leverage (26.63 * 3 = 79.89)
                    // Futures için gerçek kullanılabilir notional (margin * leverage)
                    let effective_sell_notional = caps.buy_total * effective_leverage;
                    let nq = clamp_qty_by_usd(q, px, effective_sell_notional, qty_step_f64);
                    // 2. Quantize kontrolü
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

                    // ---- FUTURES BID ----
                    // Her bid emri bağımsız, max 100 USD, toplam varlık paylaşılır
                    // ÖNEMLİ: total_spent_on_bids = hesaptan giden para (margin), pozisyon boyutu değil
                    // Örnek: 100 USD notional pozisyon, 20x leverage → hesaptan giden: 100/20 = 5 USD
                    // NOT: effective_leverage config'den geliyor ve değişmiyor, loop başında hesaplanan değeri kullan
                    let mut total_spent_on_bids = 0.0f64; // Hesaptan giden para (margin) toplamı
                    if let Some((px, qty)) = quotes.bid {
                        
                        // İlk bid emri (max 100 USD)
                        info!(%symbol, ?px, ?qty, tif = ?tif, "placing futures bid order");
                        
                        // KRİTİK: Test order - İlk emir öncesi doğrulama
                        // Eğer test_order_passed false ise, test order yap
                        if !state.test_order_passed {
                            rate_limit_guard(1).await; // POST /fapi/v1/order/test: Weight 1
                            match venue.test_order(&symbol, Side::Buy, px, qty, tif).await {
                                Ok(_) => {
                                    state.test_order_passed = true;
                                    info!(%symbol, "test order passed, proceeding with real order");
                                }
                                Err(e) => {
                                    let error_str = e.to_string();
                                    let error_lower = error_str.to_lowercase();
                                    
                                    // -1111 hatası gelirse sembolü disable et ve rules'ı yeniden çek
                                    if error_lower.contains("precision is over") || error_lower.contains("-1111") {
                                        error!(%symbol, error = %e, "test order failed with -1111, disabling symbol and refreshing rules");
                                        
                                        // Rules'ı yeniden çek
                                        match venue.rules_for(&symbol).await {
                                            Ok(new_rules) => {
                                                state.symbol_rules = Some(new_rules);
                                                state.rules_fetch_failed = false;
                                                state.disabled = false;
                                                info!(%symbol, "rules refreshed after test order -1111, symbol re-enabled");
                                                // Test order'ı tekrar dene (bir kez daha)
                                                rate_limit_guard(1).await;
                                                match venue.test_order(&symbol, Side::Buy, px, qty, tif).await {
                                                    Ok(_) => {
                                                        state.test_order_passed = true;
                                                        info!(%symbol, "test order passed after rules refresh");
                                                    }
                                                    Err(e2) => {
                                                        error!(%symbol, error = %e2, "test order still failed after rules refresh, disabling symbol");
                                                        state.disabled = true;
                                                        state.rules_fetch_failed = true;
                                                        continue; // Bu sembolü skip et
                                                    }
                                                }
                                            }
                                            Err(e2) => {
                                                error!(%symbol, error = %e2, "failed to refresh rules after test order -1111, disabling symbol");
                                                state.disabled = true;
                                                state.rules_fetch_failed = true;
                                                continue; // Bu sembolü skip et
                                            }
                                        }
                                    } else {
                                        // Diğer hatalar için sembolü disable et ama rules'ı yeniden çekme
                                        warn!(%symbol, error = %e, "test order failed (non-precision error), disabling symbol");
                                        state.disabled = true;
                                        continue; // Bu sembolü skip et
                                    }
                                }
                            }
                        }
                        
                        // API Rate Limit koruması
                        rate_limit_guard(1).await; // POST /fapi/v1/order: Weight 1
                        
                        // ClientOrderId oluştur (idempotency için)
                        // Format: {symbol}_{side}_{timestamp_ms}_{random}
                        // Binance: max 36 karakter, alphanumeric, '-' ve '_' kullanılabilir
                        let timestamp_ms = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_millis();
                        let random_suffix = (timestamp_ms % 10000) as u64; // Son 4 haneli random
                        let client_order_id = format!("{}_{}_{}_{}", 
                            symbol.replace("-", "_").replace("/", "_"), // Symbol'deki özel karakterleri değiştir
                            "B", // Buy
                            timestamp_ms,
                            random_suffix
                        );
                        // 36 karakter limit kontrolü
                        let client_order_id = if client_order_id.len() > 36 {
                            // Kısalt: symbol'ü kısalt
                            let symbol_short = symbol.chars().take(8).collect::<String>();
                            format!("{}_{}_{}_{}", symbol_short, "B", timestamp_ms, random_suffix)
                                .chars().take(36).collect::<String>()
                        } else {
                            client_order_id
                        };
                        
                match venue.place_limit_with_client_id(&symbol, Side::Buy, px, qty, tif, &client_order_id).await {
                            Ok((order_id, returned_client_id)) => {
                let info = OrderInfo { 
                    order_id: order_id.clone(), 
                    client_order_id: returned_client_id.or(Some(client_order_id)),
                    side: Side::Buy, 
                    price: px, 
                    qty, 
                    filled_qty: Qty(Decimal::ZERO),
                    remaining_qty: qty,
                    created_at: Instant::now(),
                    last_fill_time: None,
                };
                state.active_orders.insert(order_id.clone(), info.clone());
                                // KRİTİK İYİLEŞTİRME: Gerçek fiyatı kaydet (crossing guard'dan sonra)
                                // place_limit içinde fiyat quantize/round edilebilir, ama crossing guard zaten strategy'de yapıldı
                                // info.price = crossing guard adjusted price (px), bu fiyatı kaydet
                                state.last_order_price_update.insert(order_id.clone(), info.price);
                                // Başarılı emir için spent hesapla (hesaptan giden para = margin)
                                // Notional (pozisyon boyutu) / leverage = hesaptan giden para
                                let notional = (px.0.to_f64().unwrap_or(0.0)) * (qty.0.to_f64().unwrap_or(0.0));
                                total_spent_on_bids = notional / effective_leverage;
                
                // JSON log: Order created
                if let Ok(logger) = json_logger.lock() {
                    logger.log_order_created(
                &symbol,
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
                                            // KRİTİK DÜZELTME: Retry için deterministik clientOrderId (idempotency)
                                            // Retry'de qty değiştiği için yeni ID oluştur, ama deterministik olmalı
                                            let retry_price_str = format!("{:.8}", px.0.to_f64().unwrap_or(0.0));
                                            let retry_qty_str = format!("{:.8}", retry_qty.0.to_f64().unwrap_or(0.0));
                                            let retry_price_hash = retry_price_str.chars().rev().take(6).collect::<String>().chars().rev().collect::<String>();
                                            let retry_qty_hash = retry_qty_str.chars().rev().take(6).collect::<String>().chars().rev().collect::<String>();
                                            let retry_timestamp_ms = SystemTime::now()
                                                .duration_since(UNIX_EPOCH)
                                                .unwrap()
                                                .as_millis();
                                            let symbol_clean = symbol.replace("-", "_").replace("/", "_");
                                            let retry_client_order_id = format!("{}_{}_{}_{}_{}", 
                                                symbol_clean,
                                                "B",
                                                retry_price_hash,
                                                retry_qty_hash,
                                                retry_timestamp_ms
                                            );
                                            // 36 karakter limit kontrolü
                                            let retry_client_order_id = if retry_client_order_id.len() > 36 {
                                                let symbol_short = symbol_clean.chars().take(8).collect::<String>();
                                                format!("{}_{}_{}_{}_{}", symbol_short, "B", retry_price_hash, retry_qty_hash, retry_timestamp_ms)
                                                    .chars().take(36).collect::<String>()
                                            } else {
                                                retry_client_order_id
                                            };
                    match venue.place_limit_with_client_id(&symbol, Side::Buy, px, retry_qty, tif, &retry_client_order_id).await {
                                                Ok((order_id, returned_client_id)) => {
                            let info = OrderInfo { 
                                order_id: order_id.clone(), 
                                client_order_id: returned_client_id.or(Some(retry_client_order_id)),
                                side: Side::Buy, 
                                price: px, 
                                qty: retry_qty, 
                                filled_qty: Qty(Decimal::ZERO),
                                remaining_qty: retry_qty,
                                created_at: Instant::now(),
                                last_fill_time: None,
                            };
                                                    state.active_orders.insert(order_id.clone(), info.clone());
                                                    // KRİTİK İYİLEŞTİRME: Gerçek fiyatı kaydet (crossing guard'dan sonra)
                                                    state.last_order_price_update.insert(order_id.clone(), info.price);
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
                        // Extra bid için clientOrderId
                        let extra_timestamp_ms = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_millis();
                        let extra_client_order_id = format!("{}_{}_E_{}", 
                            symbol.replace("-", "_").replace("/", "_"),
                            "B",
                            extra_timestamp_ms
                        ).chars().take(36).collect::<String>();
                        match venue.place_limit_with_client_id(&symbol, Side::Buy, px, qty2, tif, &extra_client_order_id).await {
                            Ok((order_id2, returned_client_id2)) => {
                                let info2 = OrderInfo { 
                                    order_id: order_id2.clone(), 
                                    client_order_id: returned_client_id2.or(Some(extra_client_order_id)),
                                    side: Side::Buy, 
                                    price: px, 
                                    qty: qty2, 
                                    filled_qty: Qty(Decimal::ZERO),
                                    remaining_qty: qty2,
                                    created_at: Instant::now(),
                                    last_fill_time: None,
                                };
                                        state.active_orders.insert(order_id2.clone(), info2.clone());
                                        // KRİTİK İYİLEŞTİRME: Gerçek fiyatı kaydet (crossing guard'dan sonra)
                                        state.last_order_price_update.insert(order_id2, info2.price);
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
                        
                        // KRİTİK: Test order - İlk emir öncesi doğrulama (ask için de)
                        // Eğer test_order_passed false ise, test order yap
                        if !state.test_order_passed {
                            rate_limit_guard(1).await; // POST /fapi/v1/order/test: Weight 1
                            match venue.test_order(&symbol, Side::Sell, px, qty, tif).await {
                                Ok(_) => {
                                    state.test_order_passed = true;
                                    info!(%symbol, "test order passed (ask), proceeding with real order");
                                }
                                Err(e) => {
                                    let error_str = e.to_string();
                                    let error_lower = error_str.to_lowercase();
                                    
                                    // -1111 hatası gelirse sembolü disable et ve rules'ı yeniden çek
                                    if error_lower.contains("precision is over") || error_lower.contains("-1111") {
                                        error!(%symbol, error = %e, "test order failed with -1111 (ask), disabling symbol and refreshing rules");
                                        
                                        // Rules'ı yeniden çek
                                        match venue.rules_for(&symbol).await {
                                            Ok(new_rules) => {
                                                state.symbol_rules = Some(new_rules);
                                                state.rules_fetch_failed = false;
                                                state.disabled = false;
                                                info!(%symbol, "rules refreshed after test order -1111 (ask), symbol re-enabled");
                                                // Test order'ı tekrar dene (bir kez daha)
                                                rate_limit_guard(1).await;
                                                match venue.test_order(&symbol, Side::Sell, px, qty, tif).await {
                                                    Ok(_) => {
                                                        state.test_order_passed = true;
                                                        info!(%symbol, "test order passed after rules refresh (ask)");
                                                    }
                                                    Err(e2) => {
                                                        error!(%symbol, error = %e2, "test order still failed after rules refresh (ask), disabling symbol");
                                                        state.disabled = true;
                                                        state.rules_fetch_failed = true;
                                                        continue; // Bu sembolü skip et
                                                    }
                                                }
                                            }
                                            Err(e2) => {
                                                error!(%symbol, error = %e2, "failed to refresh rules after test order -1111 (ask), disabling symbol");
                                                state.disabled = true;
                                                state.rules_fetch_failed = true;
                                                continue; // Bu sembolü skip et
                                            }
                                        }
                                    } else {
                                        // Diğer hatalar için sembolü disable et ama rules'ı yeniden çekme
                                        warn!(%symbol, error = %e, "test order failed (ask, non-precision error), disabling symbol");
                                        state.disabled = true;
                                        continue; // Bu sembolü skip et
                                    }
                                }
                            }
                        }
                        
                        // API Rate Limit koruması
                        rate_limit_guard(1).await; // POST /fapi/v1/order: Weight 1
                        
                        // ClientOrderId oluştur (idempotency için)
                        let timestamp_ms = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_millis();
                        let random_suffix = (timestamp_ms % 10000) as u64;
                        let client_order_id = format!("{}_{}_{}_{}", 
                            symbol.replace("-", "_").replace("/", "_"),
                            "S", // Sell
                            timestamp_ms,
                            random_suffix
                        );
                        // 36 karakter limit kontrolü
                        let client_order_id = if client_order_id.len() > 36 {
                            let symbol_short = symbol.chars().take(8).collect::<String>();
                            format!("{}_{}_{}_{}", symbol_short, "S", timestamp_ms, random_suffix)
                                .chars().take(36).collect::<String>()
                        } else {
                            client_order_id
                        };
                        
                match venue.place_limit_with_client_id(&symbol, Side::Sell, px, qty, tif, &client_order_id).await {
                            Ok((order_id, returned_client_id)) => {
                        let info = OrderInfo { 
                            order_id: order_id.clone(), 
                            client_order_id: returned_client_id.or(Some(client_order_id)),
                            side: Side::Sell, 
                            price: px, 
                            qty, 
                            filled_qty: Qty(Decimal::ZERO),
                            remaining_qty: qty,
                            created_at: Instant::now(),
                            last_fill_time: None,
                            };
                            state.active_orders.insert(order_id.clone(), info.clone());
                            // KRİTİK İYİLEŞTİRME: Gerçek fiyatı kaydet (crossing guard'dan sonra)
                            // place_limit içinde fiyat quantize/round edilebilir, ama crossing guard zaten strategy'de yapıldı
                            // info.price = crossing guard adjusted price (px), bu fiyatı kaydet
                            state.last_order_price_update.insert(order_id.clone(), info.price);
                            
                            // JSON log: Order created
                            if let Ok(logger) = json_logger.lock() {
                            logger.log_order_created(
                            &symbol,
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
                            // KRİTİK DÜZELTME: Retry için deterministik clientOrderId (idempotency)
                            // Retry'de qty değiştiği için yeni ID oluştur, ama deterministik olmalı
                            let retry_price_str = format!("{:.8}", px.0.to_f64().unwrap_or(0.0));
                            let retry_qty_str = format!("{:.8}", retry_qty.0.to_f64().unwrap_or(0.0));
                            let retry_price_hash = retry_price_str.chars().rev().take(6).collect::<String>().chars().rev().collect::<String>();
                            let retry_qty_hash = retry_qty_str.chars().rev().take(6).collect::<String>().chars().rev().collect::<String>();
                            let retry_timestamp_ms = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap()
                                .as_millis();
                            let symbol_clean = symbol.replace("-", "_").replace("/", "_");
                            let retry_client_order_id = format!("{}_{}_{}_{}_{}", 
                                symbol_clean,
                                "S",
                                retry_price_hash,
                                retry_qty_hash,
                                retry_timestamp_ms
                            );
                            // 36 karakter limit kontrolü
                            let retry_client_order_id = if retry_client_order_id.len() > 36 {
                                let symbol_short = symbol_clean.chars().take(8).collect::<String>();
                                format!("{}_{}_{}_{}_{}", symbol_short, "S", retry_price_hash, retry_qty_hash, retry_timestamp_ms)
                                    .chars().take(36).collect::<String>()
                            } else {
                                retry_client_order_id
                            };
                            match venue.place_limit_with_client_id(&symbol, Side::Sell, px, retry_qty, tif, &retry_client_order_id).await {
                                                Ok((order_id, returned_client_id)) => {
                                let info = OrderInfo { 
                                    order_id: order_id.clone(), 
                                    client_order_id: returned_client_id.or(Some(retry_client_order_id)),
                                    side: Side::Sell, 
                                    price: px, 
                                    qty: retry_qty, 
                                    filled_qty: Qty(Decimal::ZERO),
                                    remaining_qty: retry_qty,
                                    created_at: Instant::now(),
                                    last_fill_time: None,
                                };
                                                    state.active_orders.insert(order_id.clone(), info.clone());
                                                    // KRİTİK İYİLEŞTİRME: Gerçek fiyatı kaydet (crossing guard'dan sonra)
                                                    state.last_order_price_update.insert(order_id.clone(), info.price);
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
                    // Extra ask için clientOrderId
                    let extra_timestamp_ms = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_millis();
                    let extra_client_order_id = format!("{}_{}_E_{}", 
                        symbol.replace("-", "_").replace("/", "_"),
                        "S",
                        extra_timestamp_ms
                    ).chars().take(36).collect::<String>();
                    match venue.place_limit_with_client_id(&symbol, Side::Sell, px, qty2, tif, &extra_client_order_id).await {
                        Ok((order_id2, returned_client_id2)) => {
                            let info2 = OrderInfo { 
                                order_id: order_id2.clone(), 
                                client_order_id: returned_client_id2.or(Some(extra_client_order_id)),
                                side: Side::Sell, 
                                price: px, 
                                qty: qty2, 
                                filled_qty: Qty(Decimal::ZERO),
                                remaining_qty: qty2,
                                created_at: Instant::now(),
                                last_fill_time: None,
                            };
                                        state.active_orders.insert(order_id2.clone(), info2.clone());
                                        // KRİTİK İYİLEŞTİRME: Gerçek fiyatı kaydet (crossing guard'dan sonra)
                                        state.last_order_price_update.insert(order_id2, info2.price);
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
        } // Close for loop: for state_idx in prioritized_indices
        
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

// VenueType enum removed - futures only
// Fetch balance for a quote asset from venue (futures only)
#[allow(dead_code)]
async fn fetch_quote_balance(
    venue: &BinanceFutures,
    quote_asset: &str,
) -> f64 {
    rate_limit_guard(5).await; // GET /fapi/v2/balance: Weight 5
    let result = timeout(Duration::from_secs(5), venue.available_balance(quote_asset)).await;
    
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
    venue: &BinanceFutures,
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
        (max_age_ms * 2) / 3 // KRİTİK İYİLEŞTİRME: 1/2 → 2/3 (daha az agresif)
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

