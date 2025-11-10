//location: /crates/app/src/main.rs
// Main application entry point and trading loop

mod binance_exec;
mod binance_rest;
mod binance_ws;
mod cap_manager;
mod config;
mod constants;
mod core;
mod exec;
mod logger;
mod monitor;
mod order_manager;
mod order_placement;
mod position_manager;
mod quote_generator;
mod qmel;
#[cfg(test)]
mod qmel_tests;
mod risk;
mod risk_manager;
mod strategy;
mod symbol_discovery;
mod types;
mod utils;

use anyhow::{anyhow, Result};
use crate::core::types::*;
use config::load_config;
use crate::binance_ws::{UserDataStream, UserEvent, UserStreamKind};
use crate::binance_exec::{BinanceCommon, BinanceFutures};
use crate::constants::*;
use crate::exec::{decimal_places, Venue};
use crate::risk::RiskAction;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use crate::strategy::{Context, DynMmCfg};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;
use logger::create_logger;
use types::{OrderInfo, SymbolState};
use utils::{clamp_qty_by_usd, compute_drawdown_bps, get_price_tick, get_qty_step, init_rate_limiter, rate_limit_guard};
use order_placement::place_orders_with_profit_guarantee;

use std::cmp::max;
use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};
use futures_util::stream::{self, StreamExt};
use std::sync::Arc;

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
        .with_ansi(true) // ✅ Renk desteği aktif
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
        crate::monitor::init_prom(port);
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
    // Strategy configuration is now handled in symbol_discovery module
    let strategy_name = cfg.strategy.r#type.clone();

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
    let price_tick_dec = Decimal::from_f64_retain(cfg.price_tick)
        .ok_or_else(|| anyhow!("Failed to convert price_tick {} to Decimal", cfg.price_tick))?;
    let qty_step_dec = Decimal::from_f64_retain(cfg.qty_step)
        .ok_or_else(|| anyhow!("Failed to convert qty_step {} to Decimal", cfg.qty_step))?;
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

    // Discover symbols using the new module
    let mut selected = symbol_discovery::discover_symbols(&venue, &cfg, &metadata).await?;

    // Wait for balance if no symbols found
    if selected.is_empty() {
        warn!(
            quote_asset = %cfg.quote_asset,
            min_required = cfg.min_quote_balance_usd,
            "no eligible symbols found - waiting for balance to become available"
        );
        selected = symbol_discovery::wait_and_retry_discovery(&venue, &cfg, &metadata).await?;
    }

    // KRİTİK: Startup'ta cache'i temizle (fresh rules fetch için)
    info!("clearing cached exchange rules for fresh startup");
    binance_exec::FUT_RULES.clear();

    // Initialize symbol states
    let mut states = symbol_discovery::initialize_symbol_states(selected, &dyn_cfg, &strategy_name, &cfg);
    
    // Fetch per-symbol metadata and update states - OPTIMIZED: Parallel processing
    info!(
        total_symbols = states.len(),
        "fetching per-symbol metadata for quantization (parallel processing)..."
    );
    let mut symbol_index: HashMap<String, usize> = HashMap::new();
    
    // Build symbol index first
    for (idx, state) in states.iter().enumerate() {
        symbol_index.insert(state.meta.symbol.clone(), idx);
    }
    
    // Parallel fetch rules for all symbols
    const CONCURRENT_LIMIT: usize = 10;
    let venue_arc = Arc::new(venue.clone());
    let symbols: Vec<(String, usize)> = states.iter()
        .enumerate()
        .map(|(idx, state)| (state.meta.symbol.clone(), idx))
        .collect();
    
    let rules_results: Vec<_> = stream::iter(symbols.iter())
        .map(|(symbol, idx)| {
            let venue = venue_arc.clone();
            let symbol = symbol.clone();
            let idx = *idx;
            async move {
                let rules = venue.rules_for(&symbol).await.ok();
                (idx, symbol, rules)
            }
        })
        .buffer_unordered(CONCURRENT_LIMIT)
        .collect()
        .await;
    
    // Update states with fetched rules
    for (idx, symbol, symbol_rules) in rules_results {
        let state = &mut states[idx];
        let rules_fetch_failed = symbol_rules.is_none();
        
        info!(
            symbol = %symbol,
            base_asset = %state.meta.base_asset,
            quote_asset = %state.meta.quote_asset,
            mode = %cfg.mode,
            "bot initialized assets"
        );
        
        if let Some(ref rules) = symbol_rules {
            info!(
                symbol = %symbol,
                tick_size = %rules.tick_size,
                step_size = %rules.step_size,
                price_precision = rules.price_precision,
                qty_precision = rules.qty_precision,
                min_notional = %rules.min_notional,
                "fetched per-symbol metadata (eager warmup)"
            );
        } else {
            warn!(
                symbol = %symbol,
                "CRITICAL: failed to fetch per-symbol metadata, symbol DISABLED (will not trade)"
            );
        }
        
        state.disabled = rules_fetch_failed;
        state.symbol_rules = symbol_rules;
        state.rules_fetch_failed = rules_fetch_failed;
    }

    // Setup margin and leverage for all symbols
    symbol_discovery::setup_margin_and_leverage(&venue, &mut states, &cfg).await?;

    let symbol_list: Vec<String> = states.iter().map(|s| s.meta.symbol.clone()).collect();
    info!(symbols = ?symbol_list, mode = %cfg.mode, "bot started with real Binance venue");

    let risk_limits = crate::risk::RiskLimits {
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

    // Caps struct moved to cap_manager module

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
    
    // ✅ KRİTİK FIX: Graceful shutdown için signal handler'ı bir kez spawn et
    // Signal handler bir kez kurulmalı, her tick'te yeni handler kurmak yanlış ve çalışmıyor
    // Cross-platform çözüm: tokio::signal::ctrl_c() kullan ama bir kez spawn et
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let shutdown_tx_clone = shutdown_tx.clone();
    
    // Signal handler task'ı spawn et (bir kez)
    tokio::spawn(async move {
        if let Err(e) = tokio::signal::ctrl_c().await {
            error!(error = %e, "failed to wait for ctrl_c signal");
            return;
        }
        info!("Ctrl+C signal received, sending shutdown signal");
        let _ = shutdown_tx_clone.send(());
    });
    
    // WebSocket reconnect sonrası missed events sync flag'i (loop dışında)
    let mut force_sync_all = false;
    let mut shutdown_requested = false;
    
    // BEST PRACTICE: Bot sürekli çalışır, ancak Ctrl+C ile graceful shutdown yapılabilir
    // Shutdown mekanizması sadece manuel durdurma için - bot normalde sürekli çalışır
    loop {
        tokio::select! {
            _ = interval.tick() => {
                // Normal trading tick - bot sürekli çalışır
            }
            _ = shutdown_rx.recv() => {
                // ✅ Signal handler bir kez kuruldu, shutdown channel'dan mesaj geldi
                info!("shutdown signal received (Ctrl+C), initiating graceful shutdown");
                shutdown_requested = true;
                break;
            }
        }
        
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
                                            
                                            // ✅ KRİTİK FIX: position_orders tracking kaldırıldı (kullanılmıyordu)
                                            
                                            // KRİTİK DÜZELTME: Position entry time SADECE WebSocket fill event'te set edilir
                                            // Reconnect sync fallback kaldırıldı - WebSocket event bekleniyor
                                            // Bu, PnL hesaplamalarının doğruluğunu garanti eder (yanlış zaman set etmek yerine bekler)
                                            
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
                    client_order_id: _client_order_id, // ✅ FIX: Kullanılmıyor, prefix eklendi
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
                        // KRİTİK İYİLEŞTİRME: Event ID bazlı duplicate detection
                        // Network retry'de aynı event 2 kez gelebilir, bu yüzden event ID bazlı dedupe yapıyoruz
                        // Event ID format: "{order_id}-{cumulative_filled_qty}-{timestamp}"
                        // Timestamp event'in geldiği zaman (SystemTime::now()) kullanılır
                        let event_timestamp = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis();
                        let event_id = format!("{}-{}-{}", order_id, cumulative_filled_qty.0, event_timestamp);
                        
                        // Event ID bazlı duplicate kontrolü
                        if state.processed_events.contains(&event_id) {
                            warn!(
                                %symbol,
                                order_id = %order_id,
                                cumulative_filled_qty = %cumulative_filled_qty.0,
                                event_id = %event_id,
                                "duplicate fill event ignored (event ID already processed)"
                            );
                            continue; // Duplicate event'i ignore et
                        }
                        
                        // Eski duplicate detection (geriye dönük uyumluluk için)
                        // Event ID kontrolü yeterli ama ekstra güvenlik için
                        let is_duplicate_legacy = if let Some(existing_order) = state.active_orders.get(&order_id) {
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
                        
                        if is_duplicate_legacy {
                            warn!(
                                %symbol,
                                order_id = %order_id,
                                cumulative_filled_qty = %cumulative_filled_qty.0,
                                order_status = %order_status,
                                "duplicate fill event ignored (legacy check: cumulative_qty already processed)"
                            );
                            continue; // Duplicate event'i ignore et
                        }
                        
                        // Event ID'yi kaydet (duplicate önleme için)
                        state.processed_events.insert(event_id.clone());
                        
                        // Memory leak önleme: Eski event ID'leri temizle (her 1000 event'te bir)
                        if state.processed_events.len() > 1000 {
                            if let Some(last_cleanup) = state.last_event_cleanup {
                                if last_cleanup.elapsed().as_secs() > 3600 {
                                    // 1 saatte bir temizle (eski event ID'leri kaldır)
                                    state.processed_events.clear();
                                    state.last_event_cleanup = Some(std::time::Instant::now());
                                    info!(%symbol, "cleaned up processed_events (memory leak prevention)");
                                }
                            } else {
                                state.last_event_cleanup = Some(std::time::Instant::now());
                            }
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
                        
                        // ✅ KRİTİK FIX: PnL hesaplama - SADECE pozisyon tam kapandığında
                        // Partial close durumunda PnL hesaplama (double counting önleme)
                        // PnL sadece pozisyon tam kapandığında (inventory 0'a düştüğünde) hesaplanacak
                        // Bu, aşağıdaki "Pozisyon tam kapanınca final PnL kontrolü" bloğunda yapılıyor
                        
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
                            
                            // KRİTİK DÜZELTME: Position entry time SADECE WebSocket fill event'te set edilir
                            // Bu, gerçek fill zamanını verir ve PnL hesaplamalarının doğruluğunu garanti eder
                            // Fallback'ler kaldırıldı - position check ve reconnect sync artık entry_time set etmiyor
                            // Pozisyon ilk kez açılıyorsa entry time set et (gerçek fill zamanı)
                            if old_inv.is_zero() && !inv.is_zero() {
                                if state.position_entry_time.is_none() {
                                    state.position_entry_time = Some(std::time::Instant::now());
                                    info!(
                                        %symbol,
                                        entry_time_set = "from_websocket_fill",
                                        fill_price = %price.0,
                                        fill_qty = %fill_increment,
                                        "position entry time set from WebSocket fill event (ONLY SOURCE)"
                                    );
                                } else {
                                    // Entry time zaten set edilmiş (normal durum değil, ama koruyalım)
                                    debug!(
                                        %symbol,
                                        existing_entry_time = ?state.position_entry_time,
                                        "position entry time already set, keeping existing value"
                                    );
                                }
                            }
                        }
                        
                        // ✅ KRİTİK FIX: Pozisyon tam kapanınca final PnL hesapla
                        // PnL SADECE pozisyon tam kapandığında (inventory 0'a düştüğünde) hesaplanır
                        // Bu, double counting'i önler ve doğru PnL hesaplaması sağlar
                        if inv.is_zero() && !old_inv.is_zero() {
                            // Pozisyon tam kapandı - final PnL hesapla
                            if let Some(avg_entry) = state.avg_entry_price {
                                // Tam kapanış için pozisyon miktarı
                                let closed_qty = old_inv.abs();
                                if closed_qty > Decimal::ZERO {
                                    // Final PnL = (fill_price - entry_price) * closed_qty - komisyon
                                    let price_diff = if old_inv.is_sign_positive() {
                                        // Long pozisyon kapatılıyor (sell fill)
                                        price.0 - avg_entry
                                    } else {
                                        // Short pozisyon kapatılıyor (buy fill)
                                        avg_entry - price.0
                                    };
                                    let gross_pnl = price_diff * closed_qty;
                                    
                                    // Komisyon: Bu fill için gelen commission'ı kullan
                                    // Eğer bu fill tam kapanış için yeterliyse, commission'ın tamamı bu pozisyon için
                                    let actual_commission = if fill_increment >= closed_qty {
                                        // Bu fill tam kapanış için yeterli
                                        commission
                                    } else {
                                        // Partial fill, proportional commission
                                        (commission / fill_increment) * closed_qty
                                    };
                                    
                                    let final_net_pnl = gross_pnl - actual_commission;
                                    
                                    // Daily ve cumulative PnL'e ekle (SADECE tam kapanışta)
                                    state.daily_pnl += final_net_pnl;
                                    state.cumulative_pnl += final_net_pnl;
                                    
                                    info!(
                                        %symbol,
                                        fill_price = %price.0,
                                        entry_price = %avg_entry,
                                        closed_qty = %closed_qty,
                                        gross_pnl = %gross_pnl,
                                        actual_commission = %actual_commission,
                                        final_net_pnl = %final_net_pnl,
                                        daily_pnl = %state.daily_pnl,
                                        cumulative_pnl = %state.cumulative_pnl,
                                        "realized PnL from complete position close (using actual commission from executionReport)"
                                    );
                                }
                            }
                            
                            // avg_entry_price'ı sıfırla (pozisyon kapandı)
                            state.avg_entry_price = None;
                        }
                        
                        // ✅ KRİTİK FIX: position_orders tracking kaldırıldı (kullanılmıyordu, sadece overhead yaratıyordu)
                        // Eğer gelecekte partial close veya order-to-position mapping gerekirse, o zaman eklenebilir
                        
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
                Ok(Ok(b)) => match b.to_f64() {
                    Some(bal) => bal,
                    None => {
                        warn!(
                            quote_asset = %quote_asset,
                            balance_decimal = %b,
                            "Failed to convert balance to f64, using 0.0"
                        );
                        0.0
                    }
                },
                Ok(Err(e)) => {
                    warn!(
                        quote_asset = %quote_asset,
                        error = %e,
                        "Failed to fetch balance, using 0.0"
                    );
                    0.0
                }
                Err(_) => {
                    warn!(
                        quote_asset = %quote_asset,
                        "Timeout while fetching balance, using 0.0"
                    );
                    0.0
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
            let _base_asset = &state.meta.base_asset; // ✅ FIX: Kullanılmıyor, prefix eklendi
            let quote_asset = state.meta.quote_asset.clone();

            // --- ERKEN BAKİYE KONTROLÜ: Bakiye yoksa gereksiz işlem yapma ---
            // KRİTİK DÜZELTME: Cache'den oku (race condition önlendi)
            // DEBUG: İlk birkaç sembol için detaylı log
            let is_debug_symbol = symbol_index <= cfg.internal.debug_symbol_count;
            
            // Cache'den bakiye oku (loop başında çekildi)
            let q_free = quote_balances.get(&quote_asset).copied().unwrap_or(0.0);
            
            // KRİTİK DÜZELTME: Futures için margin kontrolü (leverage uygulanmadan)
            // q_free zaten margin (hesaptan çıkan para), leverage ile çarpmak yanlış
            // Leverage sadece pozisyon büyüklüğünü belirler, hesaptan çıkan parayı etkilemez
            let has_balance = {
                    if is_debug_symbol {
                        info!(
                            %symbol,
                            quote_asset = %quote_asset,
                        available_margin = q_free,
                            min_required = min_usd_per_order,
                        "balance check for debug symbol (futures, margin-based, from cache)"
                        );
                    }
                    if q_free < cfg.min_quote_balance_usd {
                        false // Bakiye çok düşük, skip
                    } else {
                    // Margin >= min_usd_per_order kontrolü (leverage uygulanmadan)
                    let has_enough = q_free >= min_usd_per_order;
                        if is_debug_symbol {
                            info!(
                                %symbol,
                            available_margin = q_free,
                                min_required = min_usd_per_order,
                                has_enough,
                            "balance check result (futures, margin-based)"
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
                state,
                cfg.internal.order_sync_interval_sec * 1000, // Convert to ms
            ) || force_sync_all;
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
                                    
                                    // ✅ KRİTİK FIX: position_orders tracking kaldırıldı (kullanılmıyordu)
                                    
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
            
            // KRİTİK: Pozisyon varsa MUTLAKA kapatılmalı - otomatik kapatma mekanizması
            if has_position {
                // Pozisyon timeout kontrolü - çok uzun süre açık kalmışsa zorla kapat
                if let Some(entry_time) = state.position_entry_time {
                    let age_secs = entry_time.elapsed().as_secs() as f64;
                    if age_secs >= crate::constants::MAX_POSITION_DURATION_SEC {
                        warn!(
                            %symbol,
                            position_qty = %pos.qty.0,
                            age_secs,
                            "FORCE CLOSE: Position exceeded max duration, closing automatically"
                        );
                        // Zorla kapat
                        if !state.position_closing {
                            let _ = position_manager::close_position(&venue, &symbol, state).await;
                        }
                        continue;
                    }
                }
                
            if has_position && !has_balance {
                info!(
                    %symbol,
                    position_qty = %pos.qty.0,
                    "has open position but no balance, continuing to manage position"
                );
                }
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
            let position_size_notional = match (mark_px.0 * pos.qty.0.abs()).to_f64() {
                Some(notional) => notional,
                None => {
                    warn!(
                        symbol = %state.meta.symbol,
                        mark_price = %mark_px.0,
                        qty = %pos.qty.0,
                        "Failed to convert position notional to f64, using 0.0"
                    );
                    0.0
                }
            };
            
            // Analyze and cancel stale/far orders using order_manager
            if !state.active_orders.is_empty() {
                let orders_to_cancel = order_manager::analyze_orders(
                    state,
                    bid,
                    ask,
                    position_size_notional,
                    &cfg,
                );
                
                if !orders_to_cancel.is_empty() {
                    order_manager::cancel_orders(
                        &venue,
                        &symbol,
                        &orders_to_cancel,
                        state,
                        cfg.internal.cancel_stagger_delay_ms,
                    ).await?;
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

            // Sync inventory with API position using position_manager
            let reconcile_threshold = Decimal::from_str_radix(&cfg.internal.inventory_reconcile_threshold, 10)
                .unwrap_or(Decimal::new(1, 4));
            const MIN_SYNC_INTERVAL_MS: u128 = 500;
            position_manager::sync_inventory(
                state,
                &pos,
                force_sync_all,
                reconcile_threshold,
                MIN_SYNC_INTERVAL_MS,
            );

            // Update position tracking
            position_manager::update_position_tracking(state, &pos, mark_px, &cfg);
            
            // Calculate current PnL for analysis
            let current_pnl = (mark_px.0 - pos.entry.0) * pos.qty.0;
            let pnl_f64 = current_pnl.to_f64().unwrap_or(0.0);
            
            // Update daily PnL reset if needed
            position_manager::update_daily_pnl_reset(state);
            
            // Apply funding cost if needed
            position_manager::apply_funding_cost(
                state,
                funding_rate,
                next_funding_time,
                position_size_notional,
            );
            
            // Check position size risk using risk_manager
            let total_active_orders_notional = risk_manager::calculate_total_active_orders_notional(state);
            let (risk_level, max_position_size_usd, should_block_new_orders) = risk_manager::check_position_size_risk(
                state,
                        position_size_notional,
                total_active_orders_notional,
                cfg.max_usd_per_order,
                effective_leverage,
                &cfg,
            );
            
            // Handle risk levels
            match risk_level {
                risk_manager::PositionRiskLevel::Hard => {
                warn!(
                    %symbol,
                        position_size_notional,
                        total_exposure = position_size_notional + total_active_orders_notional,
                    max_allowed = max_position_size_usd,
                        "HARD LIMIT: force closing position"
                        );
                    // Risk level handled below
                        state.position_closing = true;
                        state.last_close_attempt = Some(Instant::now());
                        
                    rate_limit_guard(2).await;
                        if let Err(err) = venue.cancel_all(&symbol).await {
                            warn!(%symbol, ?err, "failed to cancel all orders before force-close");
                        }
                    if let Err(err) = venue.close_position(&symbol).await {
                            state.last_close_attempt = Some(Instant::now());
                        error!(%symbol, ?err, "failed to close position due to hard limit");
                        } else {
                            info!(%symbol, "closed position due to hard limit");
                        }
                    state.position_closing = false;
                    continue;
                    }
                risk_manager::PositionRiskLevel::Medium => {
                        warn!(
                            %symbol,
                        position_size_notional,
                        total_exposure = position_size_notional + total_active_orders_notional,
                        "MEDIUM LIMIT: reducing active orders"
                    );
                    // Cancel oldest 50% of orders
                        let mut orders_with_times: Vec<(String, Instant)> = state.active_orders.iter()
                            .map(|(order_id, order)| (order_id.clone(), order.created_at))
                            .collect();
                        orders_with_times.sort_by(|a, b| a.1.cmp(&b.1));
                        let orders_to_cancel: Vec<String> = orders_with_times
                            .into_iter()
                        .take((state.active_orders.len() / 2).max(1))
                            .map(|(order_id, _)| order_id)
                            .collect();
                        
                        for order_id in &orders_to_cancel {
                            rate_limit_guard(1).await;
                        if venue.cancel(order_id, &symbol).await.is_ok() {
                                state.active_orders.remove(order_id);
                                state.last_order_price_update.remove(order_id);
                            }
                        }
                    // Risk level handled below
                    }
                risk_manager::PositionRiskLevel::Soft => {
                        info!(
                            %symbol,
                        position_size_notional,
                        total_exposure = position_size_notional + total_active_orders_notional,
                        "SOFT LIMIT: blocking new orders"
                    );
                    // Risk level handled below
                }
                risk_manager::PositionRiskLevel::Ok => {
                    // Normal operation
                }
            }
            
            // Check PnL alerts
            risk_manager::check_pnl_alerts(state, pnl_f64, position_size_notional, &cfg);
            
            // Update peak PnL
            risk_manager::update_peak_pnl(state, current_pnl);
            
            // Check if position should be closed using position_manager
                let (should_close, reason) = position_manager::should_close_position_smart(
                state,
                &pos,
                mark_px,
                bid,
                ask,
                min_profit_usd,
                maker_fee_rate,
                taker_fee_rate,
            );
            
            // Cooldown check
                let close_cooldown_ms = cfg.strategy.position_close_cooldown_ms.unwrap_or(500) as u128;
                let can_attempt_close = state.last_close_attempt
                    .map(|last| Instant::now().duration_since(last).as_millis() >= close_cooldown_ms)
                    .unwrap_or(true);
                
            if should_close && !state.position_closing && can_attempt_close {
                    info!(
                        %symbol,
                    reason = %reason,
                    "closing position based on smart position management"
                );
                
                // KRİTİK: Pozisyon kapatmadan önce entry price ve quantity'yi kaydet (log için)
                let entry_price_before_close = pos.entry;
                let qty_before_close = pos.qty;
                let leverage_before_close = pos.leverage;
                let side_str = if pos.qty.0.is_sign_positive() { "long" } else { "short" };
                
                // Close position using position_manager
                if position_manager::close_position(&venue, &symbol, state).await.is_ok() {
                    // KRİTİK: Pozisyon kapandıktan sonra exit price'ı al ve logla
                    // Exit price için bid/ask kullan (long için bid, short için ask)
                    let exit_price = match side_str {
                        "long" => bid,
                        _ => ask,
                    };
                    
                    // PnL hesapla
                    let qty_abs = qty_before_close.0.abs();
                    let entry_f = entry_price_before_close.0.to_f64().unwrap_or(0.0);
                    let exit_f = exit_price.0.to_f64().unwrap_or(0.0);
                    let realized_pnl = if qty_before_close.0.is_sign_positive() {
                        (exit_f - entry_f) * qty_abs.to_f64().unwrap_or(0.0)
                    } else {
                        (entry_f - exit_f) * qty_abs.to_f64().unwrap_or(0.0)
                    };
                    
                    // Fees hesapla (entry + exit)
                    let notional = entry_f * qty_abs.to_f64().unwrap_or(0.0);
                    let entry_fee = notional * maker_fee_rate;
                    let exit_fee = notional * maker_fee_rate; // Maker olarak kapatmaya çalışıyoruz
                    let total_fees = entry_fee + exit_fee;
                    let net_profit = realized_pnl - total_fees;
                    
                    // KRİTİK: Detaylı trade loglama
                    if let Ok(logger) = json_logger.lock() {
                        logger.log_position_closed(
                            &symbol,
                            side_str,
                            entry_price_before_close,
                            exit_price,
                            qty_before_close,
                            leverage_before_close,
                            &reason,
                        );
                        
                        logger.log_trade_completed(
                            &symbol,
                            side_str,
                            entry_price_before_close,
                            exit_price,
                            qty_before_close,
                            total_fees,
                            leverage_before_close,
                        );
                    }
                    
                    // KRİTİK: PnL tracking güncelle
                    state.trade_count += 1;
                    let net_profit_decimal = Decimal::from_f64_retain(net_profit).unwrap_or(Decimal::ZERO);
                    let total_fees_decimal = Decimal::from_f64_retain(total_fees).unwrap_or(Decimal::ZERO);
                    
                    if net_profit > 0.0 {
                        state.profitable_trade_count += 1;
                        state.total_profit += net_profit_decimal;
                        if net_profit_decimal > state.largest_win {
                            state.largest_win = net_profit_decimal;
                        }
                        info!(
                            %symbol,
                            entry_price = %entry_price_before_close.0,
                            exit_price = %exit_price.0,
                            quantity = %qty_abs,
                            realized_pnl = realized_pnl,
                            fees = total_fees,
                            net_profit = net_profit,
                            total_trades = state.trade_count,
                            profitable_trades = state.profitable_trade_count,
                            "✅ TRADE PROFIT - Position closed with profit"
                        );
                    } else {
                        state.losing_trade_count += 1;
                        state.total_loss += net_profit_decimal.abs(); // Loss is negative, store as positive
                        if net_profit_decimal < state.largest_loss {
                            state.largest_loss = net_profit_decimal.abs(); // Store as positive
                        }
                        warn!(
                            %symbol,
                            entry_price = %entry_price_before_close.0,
                            exit_price = %exit_price.0,
                            quantity = %qty_abs,
                            realized_pnl = realized_pnl,
                            fees = total_fees,
                            net_profit = net_profit,
                            total_trades = state.trade_count,
                            losing_trades = state.losing_trade_count,
                            "❌ TRADE LOSS - Position closed with loss"
                        );
                    }
                    
                    state.total_fees_paid += total_fees_decimal;
                    
                    // KRİTİK: Stratejisine trade sonucunu öğret (online learning)
                    // Bu botu gerçekten "akıllı" yapan kısım - geçmiş trade'lerden öğreniyor
                    state.strategy.learn_from_trade(net_profit, None, None);
                    
                    // Feature importance tracking: Her 20 trade'de bir özet logla
                    // Hangi feature'ların daha önemli olduğunu öğren ve logla
                    if state.trade_count > 0 && state.trade_count % 20 == 0 {
                        if let Some(top_features) = state.strategy.get_feature_importance() {
                            info!(
                                %symbol,
                                total_trades = state.trade_count,
                                top_5_features = ?top_features.iter().take(5).map(|(name, score)| format!("{}: {:.4}", name, score)).collect::<Vec<_>>(),
                                "📊 Feature Importance Analysis - Learning which features matter most"
                            );
                        }
                    }
                    
                    debug!(
                        %symbol,
                        net_profit,
                        "Strategy learned from trade result"
                    );
                    
                    // PnL summary log (her 10 işlemde bir veya 1 saatte bir)
                    let should_log_summary = state.last_pnl_summary_time
                        .map(|last| {
                            last.elapsed().as_secs() >= 3600 || // 1 saat
                            state.trade_count % 10 == 0 // Her 10 işlemde
                        })
                        .unwrap_or(false);
                    
                    if should_log_summary && state.trade_count > 0 {
                        let total_profit_f = state.total_profit.to_f64().unwrap_or(0.0);
                        let total_loss_f = state.total_loss.to_f64().unwrap_or(0.0);
                        let net_pnl_f = total_profit_f - total_loss_f;
                        let largest_win_f = state.largest_win.to_f64().unwrap_or(0.0);
                        let largest_loss_f = state.largest_loss.to_f64().unwrap_or(0.0);
                        let total_fees_f = state.total_fees_paid.to_f64().unwrap_or(0.0);
                        
                        if let Ok(logger) = json_logger.lock() {
                            logger.log_pnl_summary(
                                "hourly",
                                state.trade_count,
                                state.profitable_trade_count,
                                state.losing_trade_count,
                                total_profit_f,
                                total_loss_f,
                                net_pnl_f,
                                largest_win_f,
                                largest_loss_f,
                                total_fees_f,
                            );
                        }
                        
                        info!(
                            %symbol,
                            total_trades = state.trade_count,
                            profitable = state.profitable_trade_count,
                            losing = state.losing_trade_count,
                            total_profit = total_profit_f,
                            total_loss = total_loss_f,
                            net_pnl = net_pnl_f,
                            win_rate = if state.trade_count > 0 { state.profitable_trade_count as f64 / state.trade_count as f64 } else { 0.0 },
                            "📊 PnL SUMMARY - Trade statistics"
                        );
                        
                        state.last_pnl_summary_time = Some(Instant::now());
                    }
                }
                
                // Reset position tracking after close
                        state.position_entry_time = None;
                        state.avg_entry_price = None;
                            state.peak_pnl = Decimal::ZERO;
                            state.position_hold_duration_ms = 0;
                        // Loglanan değerleri de sıfırla (pozisyon kapandı)
                        state.last_logged_position_qty = None;
                        state.last_logged_pnl = None;
            }
            
            // Pozisyon durumu logla (sadece önemli değişikliklerde)
            // KRİTİK DÜZELTME: Her tick'te loglama disk I/O bottleneck yaratır
            // Sadece değişiklik olduğunda logla: qty değişti, PnL önemli ölçüde değişti, veya ilk pozisyon
            let should_log_position = if has_position {
                let qty_changed = state.last_logged_position_qty
                    .map(|last_qty| (last_qty - pos.qty.0).abs() > Decimal::new(1, 8))
                    .unwrap_or(true); // İlk pozisyon → logla
                
                let pnl_changed_significantly = state.last_logged_pnl
                    .map(|last_pnl| {
                        let pnl_diff = (current_pnl - last_pnl).abs();
                        // PnL %5'ten fazla değiştiyse veya işaret değiştiyse logla
                        let pnl_diff_pct = if last_pnl.abs() > Decimal::ZERO {
                            (pnl_diff / last_pnl.abs()).to_f64().unwrap_or(0.0)
                        } else {
                            1.0 // last_pnl sıfırsa her zaman logla
                        };
                        pnl_diff_pct > 0.05 || (current_pnl.is_sign_positive() != last_pnl.is_sign_positive())
                    })
                    .unwrap_or(true); // İlk pozisyon → logla
                
                qty_changed || pnl_changed_significantly
            } else {
                false
            };
            
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
                    pnl_trend = if state.pnl_history.len() >= 10 {
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
                    },
                    active_orders = state.active_orders.len(),
                    order_fill_rate = state.order_fill_rate,
                    consecutive_no_fills = state.consecutive_no_fills,
                    "position status: monitoring for intelligent decisions"
                );
                // Son loglanan değerleri kaydet (değişiklik bazlı loglama için)
                state.last_logged_position_qty = Some(pos.qty.0);
                state.last_logged_pnl = Some(current_pnl);
                state.last_position_check = Some(Instant::now());
            }

            let liq_gap_bps = if let Some(liq_px) = pos.liq_px {
                let mark = mark_px.0.to_f64().unwrap_or(0.0);
                let liq = liq_px.0.to_f64().unwrap_or(0.0);
                if mark > 0.0 {
                    ((mark - liq).abs() / mark) * 10_000.0
                } else {
                    DEFAULT_LIQ_GAP_BPS
                }
            } else {
                DEFAULT_LIQ_GAP_BPS
            };

            let dd_bps = compute_drawdown_bps(&state.pnl_history);
            let risk_action = crate::risk::check_risk(&pos, state.inv, liq_gap_bps, dd_bps, &risk_limits);
            
            // --- AKILLI FILL ORANI TAKİBİ: Zaman bazlı fill rate kontrolü ---
            // Sadece belirli aralıklarla kontrol et (overhead önleme)
            let should_check_decay = state.last_decay_check
                .map(|last| last.elapsed().as_secs() >= FILL_RATE_DECAY_CHECK_INTERVAL_SEC)
                .unwrap_or(true);
            
            if should_check_decay {
                state.last_decay_check = Some(Instant::now());
                
                if let Some(last_fill) = state.last_fill_time {
                    let seconds_since_fill = last_fill.elapsed().as_secs();
                
                    if seconds_since_fill >= FILL_RATE_DECAY_THRESHOLD_SEC {
                        let current_period = seconds_since_fill / FILL_RATE_DECAY_INTERVAL_SEC;
                        
                    if Some(current_period) != state.last_decay_period {
                            state.order_fill_rate *= FILL_RATE_DECAY_MULTIPLIER;
                            state.consecutive_no_fills += 1;
                            state.last_decay_period = Some(current_period);
                        
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
            } else {
                state.last_decay_period = None;
                }
            }
            
            // ✅ KRİTİK FIX: Adaptive fill rate logic redundancy kaldırıldı
            // Order age check mekanizması kaldırıldı - time-based decay zaten optimize edilmiş ve genel
            // Time-based decay (30 saniye threshold, 5 saniyede bir kontrol) yeterli ve daha verimli
            // Emir yoksa, fill oranını yavaşça normale döndür (bu mantık korunuyor)
            if state.active_orders.is_empty() {
                // Emir yoksa, consecutive_no_fills sıfırla ve fill oranını yavaşça normale döndür
                state.consecutive_no_fills = 0;
                state.order_fill_rate = (state.order_fill_rate * cfg.internal.fill_rate_slow_decrease_factor + cfg.internal.fill_rate_slow_decrease_bonus).min(1.0);
            }
            
            // --- AKILLI POZİSYON YÖNETİMİ: Fill oranına göre strateji ayarla ---
            if state.order_fill_rate < LOW_FILL_RATE_THRESHOLD && !state.active_orders.is_empty() {
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
                // ✅ PERFORMANS: Batch request'lerde tek guard yeterli (cancel_all + close_position)
                rate_limit_guard(2).await; // DELETE /api/v3/order (1) + POST /fapi/v1/order (1) = 2
                if let Err(err) = venue.cancel_all(&symbol).await {
                            warn!(%symbol, ?err, "failed to cancel all orders during halt");
                        }
                // Aynı guard bloğu içinde pozisyonu kapat
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
            
            // OrderBook is used in Context and also passed to order_placement, so we need a clone
            let ob_for_orders = ob.clone();
            
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
            
            // KRİTİK: Long/Short seçimi - Orderbook imbalance ve histerezis kontrolü
            // Orderbook imbalance hesapla (top-K levels varsa kullan, yoksa best bid/ask)
            let (bid_vol, ask_vol) = if let (Some(ref top_bids), Some(ref top_asks)) = (&ctx.ob.top_bids, &ctx.ob.top_asks) {
                // Top-K levels mevcut: tüm level'ların volume'larını topla
                let bid_vol_sum: Decimal = top_bids.iter().map(|b| b.qty.0).sum();
                let ask_vol_sum: Decimal = top_asks.iter().map(|a| a.qty.0).sum();
                (bid_vol_sum.max(Decimal::ONE), ask_vol_sum.max(Decimal::ONE))
            } else {
                // Fallback: best bid/ask volumes
                let bid_vol = ctx.ob.best_bid.map(|b| b.qty.0).unwrap_or(Decimal::ONE);
                let ask_vol = ctx.ob.best_ask.map(|a| a.qty.0).unwrap_or(Decimal::ONE);
                (bid_vol, ask_vol)
            };
            
            // Orderbook imbalance ratio
            let imbalance_ratio = if ask_vol > Decimal::ZERO {
                bid_vol / ask_vol
            } else {
                Decimal::ONE
            };
            let imbalance_ratio_f64 = imbalance_ratio.to_f64().unwrap_or(1.0);
            
            // Long/Short sinyal gücü hesapla (config'den threshold'lar)
            let imbalance_long_threshold = cfg.strategy.orderbook_imbalance_long_threshold.unwrap_or(1.2);
            let imbalance_short_threshold = cfg.strategy.orderbook_imbalance_short_threshold.unwrap_or(0.83);
            
            let long_signal_strength = if imbalance_ratio_f64 > imbalance_long_threshold {
                // Bid vol > Ask vol → Long lehine
                let range = imbalance_long_threshold - 1.0;
                if range > 0.0 {
                    ((imbalance_ratio_f64 - 1.0) / range).min(1.0)
                } else {
                    0.0
                }
            } else {
                0.0
            };
            
            let short_signal_strength = if imbalance_ratio_f64 < imbalance_short_threshold {
                // Ask vol > Bid vol → Short lehine
                let range = 1.0 - imbalance_short_threshold;
                if range > 0.0 {
                    ((1.0 - imbalance_ratio_f64) / range).min(1.0)
                } else {
                    0.0
                }
            } else {
                0.0
            };
            
            // Histerezis ve cooldown kontrolü (config'den)
            let cooldown_secs = cfg.strategy.direction_cooldown_secs.unwrap_or(60);
            let signal_strength_threshold = cfg.strategy.direction_signal_strength_threshold.unwrap_or(0.2);
            
            let now = Instant::now();
            let can_change_direction = state.last_direction_change
                .map(|last| now.duration_since(last).as_secs() >= cooldown_secs)
                .unwrap_or(true); // İlk seferde değiştirebilir
            
            // Yön seçimi
            let new_direction = if long_signal_strength > short_signal_strength + signal_strength_threshold {
                Some(Side::Buy) // Long
            } else if short_signal_strength > long_signal_strength + signal_strength_threshold {
                Some(Side::Sell) // Short
            } else {
                state.current_direction // Mevcut yönü koru
            };
            
            // Yön değişikliği kontrolü
            let direction_changed = new_direction != state.current_direction;
            if direction_changed && can_change_direction {
                state.current_direction = new_direction;
                state.last_direction_change = Some(now);
                state.direction_signal_strength = long_signal_strength.max(short_signal_strength);
                debug!(
                    %symbol,
                    new_direction = ?new_direction,
                    signal_strength = state.direction_signal_strength,
                    imbalance_ratio = imbalance_ratio_f64,
                    "direction changed (long/short selection)"
                );
            } else if direction_changed && !can_change_direction {
                // Cooldown'da, yön değiştirme
                debug!(
                    %symbol,
                    requested_direction = ?new_direction,
                    current_direction = ?state.current_direction,
                    "direction change blocked by cooldown"
                );
            }
            
            // Long/Short filtreleme: Sadece seçilen yönde emir yerleştir (config'den threshold'lar)
            // KRİTİK DÜZELTME: Strateji kendi long/short kararını veriyorsa (QMelStrategy gibi)
            // main.rs'de direction filter uygulanmamalı - stratejinin mantığını bozmamak için
            // Sadece DynMm gibi market making stratejileri için direction filter uygulanır
            if !state.strategy.applies_own_direction_filter() {
                // DynMm gibi stratejiler için: main.rs'de direction filter uygula
                let min_signal_strength = cfg.strategy.direction_min_signal_strength.unwrap_or(0.3);
                let strong_imbalance_long = imbalance_long_threshold + 0.3; // Default: 1.5
                let strong_imbalance_short = imbalance_short_threshold - 0.16; // Default: 0.67
                
                if let Some(current_dir) = state.current_direction {
                    match current_dir {
                        Side::Buy => {
                            // Long seçildi → sadece bid (buy) emirleri
                            quotes.ask = None;
                            if quotes.bid.is_none() && long_signal_strength < min_signal_strength {
                                // Sinyal çok zayıf, emir yerleştirme
                                debug!(%symbol, "long signal too weak, skipping bid orders");
                            }
                        }
                        Side::Sell => {
                            // Short seçildi → sadece ask (sell) emirleri
                            quotes.bid = None;
                            if quotes.ask.is_none() && short_signal_strength < min_signal_strength {
                                // Sinyal çok zayıf, emir yerleştirme
                                debug!(%symbol, "short signal too weak, skipping ask orders");
                            }
                        }
                    }
                } else {
                    // İlk seferde, her iki yönde de emir yerleştir (başlangıç)
                    // Ancak imbalance çok güçlüyse tek yöne odaklan
                    if imbalance_ratio_f64 > strong_imbalance_long {
                        quotes.ask = None; // Sadece long
                        state.current_direction = Some(Side::Buy);
                    } else if imbalance_ratio_f64 < strong_imbalance_short {
                        quotes.bid = None; // Sadece short
                        state.current_direction = Some(Side::Sell);
                    }
                }
            } else {
                // QMelStrategy gibi stratejiler: Kendi kararını veriyor, filter uygulama
                // Strateji zaten should_trade_long ve should_trade_short kararlarını vermiş
                // ve quotes.bid/ask'i buna göre ayarlamış
                debug!(%symbol, "strategy applies own direction filter, skipping main.rs filter");
            }
            
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

            // Calculate caps using cap_manager
            let caps = cap_manager::calculate_caps(
                state,
                &quote_asset,
                &quote_balances,
                position_size_notional,
                current_pnl,
                        effective_leverage,
                &cfg,
            );


            // Check if caps are sufficient
            if caps.buy_total < cfg.min_quote_balance_usd {
                info!(
                    %symbol,
                    quote_asset = %quote_asset,
                    buy_total = caps.buy_total,
                    min_required = cfg.min_quote_balance_usd,
                    "SKIPPING SYMBOL: quote asset balance below minimum threshold"
                );
                continue;
            }

            let (buy_cap_ok, sell_cap_ok) = cap_manager::check_caps_sufficient(
                &caps,
                min_usd_per_order,
                state.min_notional_req,
            );

            if !buy_cap_ok && !sell_cap_ok {
                info!(
                    %symbol,
                    buy_total = caps.buy_total,
                    min_usd_per_order,
                    "skip tick: insufficient balance or below min_notional"
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
            #[allow(unused_variables)] // bid_qty and ask_qty are used in position_size_usd calculation
            if let (Some((bid_px, bid_qty)), Some((ask_px, ask_qty))) = (quotes.bid, quotes.ask) {
                let spread_bps = utils::calculate_spread_bps(bid_px.0, ask_px.0);
                let position_size_usd = {
                    let bid_notional = bid_px.0.to_f64().unwrap_or(0.0) * bid_qty.0.to_f64().unwrap_or(0.0);
                    let ask_notional = ask_px.0.to_f64().unwrap_or(0.0) * ask_qty.0.to_f64().unwrap_or(0.0);
                    bid_notional.max(ask_notional) // Use larger of the two
                };
                // Note: bid_qty and ask_qty are used above for position_size_usd calculation
                
                // KRİTİK DÜZELTME: Dinamik min_spread_bps hesapla (ProfitGuarantee ile)
                // Sabit 60 bps yerine, pozisyon boyutuna göre dinamik hesapla
                // ✅ calculate_min_spread_bps() artık safety margin içeriyor (slippage, partial fill, volatility)
                // Not: slippage_bps_reserve hala config'de var ama calculate_min_spread_bps içinde safety margin zaten var
                // İsteğe bağlı olarak ek bir reserve çıkarılabilir (daha konservatif için)
                let dyn_min_spread_bps = profit_guarantee.calculate_min_spread_bps(position_size_usd);
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
                    // ✅ KRİTİK FIX: Margin chunking kullanıldığı için bu kod bloğu gereksiz
                    // Margin chunking sistemi (satır 3397+) zaten calc_qty_from_margin kullanıyor
                    // Bu kod bloğu sadece quote hazırlama için kullanılıyor, margin chunking ile override ediliyor
                    // Bu yüzden burada sadece basit bir kontrol yapıyoruz (leverage uygulamadan)
                    // NOT: Bu kod artık kullanılmıyor çünkü margin chunking sistemi var
                    // Ama yine de quote hazırlama için basit bir kontrol yapıyoruz
                    // Leverage uygulaması margin chunking'de calc_qty_from_margin ile yapılıyor
                    // ✅ KRİTİK FIX: clamp_qty_by_usd max_usd parametresi notional (pozisyon boyutu) bekliyor
                    // caps.buy_notional zaten notional içeriyor (cap_manager'da leverage uygulanmış)
                    // caps.buy_notional opportunity mode leverage reduction'ı zaten içeriyor (cap_manager'da uygulandı)
                    // caps.buy_total margin'dir, leverage ile çarpmaya gerek yok - caps.buy_notional kullanılmalı
                    // ❌ YANLIŞ: caps.buy_total * effective_leverage_for_clamp → Çift sayma (leverage 2 kere uygulanır)
                    // ✅ DOĞRU: caps.buy_notional → Tek sayma (leverage zaten cap_manager'da uygulandı)
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
                            "skipping quote: qty too small after caps/quantization (NOTE: margin chunking will override this)"
                        );
                        quotes.bid = None;
                    } else {
                        quotes.bid = Some((px, nq));
                        info!(%symbol, ?px, original_qty = ?q, clamped_qty = ?nq, "prepared bid quote (NOTE: margin chunking will override with calc_qty_from_margin)");
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
                    // ✅ KRİTİK FIX: Margin chunking kullanıldığı için bu kod bloğu gereksiz
                    // Margin chunking sistemi (satır 3897+) zaten calc_qty_from_margin kullanıyor
                    // Bu kod bloğu sadece quote hazırlama için kullanılıyor, margin chunking ile override ediliyor
                    // Bu yüzden burada sadece basit bir kontrol yapıyoruz (leverage uygulamadan)
                    // NOT: Bu kod artık kullanılmıyor çünkü margin chunking sistemi var
                    // Ama yine de quote hazırlama için basit bir kontrol yapıyoruz
                    // Leverage uygulaması margin chunking'de calc_qty_from_margin ile yapılıyor
                    // ✅ KRİTİK FIX: clamp_qty_by_usd max_usd parametresi notional (pozisyon boyutu) bekliyor
                    // caps.sell_notional zaten notional içeriyor (cap_manager'da leverage uygulanmış)
                    // caps.sell_notional opportunity mode leverage reduction'ı zaten içeriyor (cap_manager'da uygulandı)
                    // caps.buy_total margin'dir, leverage ile çarpmaya gerek yok - caps.sell_notional kullanılmalı
                    // ❌ YANLIŞ: caps.buy_total * effective_leverage_for_clamp → Çift sayma (leverage 2 kere uygulanır)
                    // ✅ DOĞRU: caps.sell_notional → Tek sayma (leverage zaten cap_manager'da uygulandı)
                    let nq = clamp_qty_by_usd(q, px, caps.sell_notional, qty_step_f64);
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
                            "skipping quote: qty too small after caps/quantization (NOTE: margin chunking will override this)"
                        );
                        quotes.ask = None;
                    } else {
                        quotes.ask = Some((px, nq));
                        info!(%symbol, ?px, original_qty = ?q, clamped_qty = ?nq, "prepared ask quote (NOTE: margin chunking will override with calc_qty_from_margin)");
                    }
                }
            } else {
                info!(%symbol, "no ask quote generated for this tick");
            }

                    // ---- FUTURES BID ----
                    // Unified order placement using order_placement module (eliminates code duplication)
                    let mut total_spent_on_bids = 0.0f64;
                    let open_bid_orders = state.active_orders.values()
                        .filter(|o| o.side == Side::Buy)
                        .count();
                    let max_chunks = cfg.risk.max_open_chunks_per_symbol_per_side;
                        let available_margin_for_bids = (caps.buy_total - total_spent_on_bids).max(0.0);
                    let min_margin: f64 = (50.0f64 * 0.2f64).max(cfg.min_usd_per_order.unwrap_or(10.0f64)).min(100.0f64);
                    
                    if let Some(_) = quotes.bid {
                        place_orders_with_profit_guarantee(
                            &venue,
                            &symbol,
                            Side::Buy,
                            quotes.bid,
                            state,
                            bid,
                            ask,
                            position_size_notional,
                            available_margin_for_bids,
                            effective_leverage,
                            open_bid_orders,
                            max_chunks,
                            &quote_asset,
                            &mut quote_balances,
                            &mut total_spent_on_bids,
                            &cfg,
                            tif,
                            &json_logger,
                            &ob_for_orders,
                            maker_fee_rate,
                            taker_fee_rate,
                            min_margin,
                        ).await?;
                    }

                    // ---- FUTURES ASK ----
                    // Unified order placement using order_placement module (eliminates code duplication)
                    let mut total_spent_on_asks = 0.0f64;
                        let open_ask_orders = state.active_orders.values()
                            .filter(|o| o.side == Side::Sell)
                            .count();
                        let max_chunks = cfg.risk.max_open_chunks_per_symbol_per_side;
                        let available_margin_for_asks = (caps.buy_total - total_spent_on_bids - total_spent_on_asks).max(0.0);
                    let min_margin: f64 = (50.0f64 * 0.2f64).max(cfg.min_usd_per_order.unwrap_or(10.0f64)).min(100.0f64);
                    
                    if let Some(_) = quotes.ask {
                        place_orders_with_profit_guarantee(
                            &venue,
                            &symbol,
                            Side::Sell,
                            quotes.ask,
                            state,
                            bid,
                            ask,
                            position_size_notional,
                            available_margin_for_asks,
                            effective_leverage_ask,
                            open_ask_orders,
                            max_chunks,
                            &quote_asset,
                            &mut quote_balances,
                            &mut total_spent_on_asks,
                            &cfg,
                            tif,
                            &json_logger,
                            &ob_for_orders,
                            maker_fee_rate,
                            taker_fee_rate,
                            min_margin,
                        ).await?;
                    }

                    // Eski bid ve ask kodları kaldırıldı - chunking sistemi kullanılıyor
                    // Chunking sistemi yukarıda implement edildi (bid ve ask için)
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
    
    // ✅ BEST PRACTICE: Graceful shutdown - tüm kaynakları temizle
    info!("main loop ended, performing graceful shutdown");
    
    // 1. Açık pozisyonları kontrol et (log için)
    if shutdown_requested {
        info!("checking for open positions...");
        for state in states.iter() {
            let symbol = &state.meta.symbol;
            if !state.inv.0.is_zero() {
                warn!(%symbol, inventory = %state.inv.0, "position still open during shutdown");
            }
        }
    }
    
    // 2. Event channel'ı kapat (WebSocket task'ları bunu algılayacak ve duracak)
    info!("closing event channels...");
    drop(event_tx);
    
    // 3. Async logger'ın channel'ını kapat (writer task'ın son event'leri yazması için)
    // Logger Arc ile shared olduğu için drop edildiğinde sender drop olur ve receiver None alır
    info!("closing logger...");
    drop(json_logger);
    
    // 4. Shutdown channel'ı kapat
    drop(shutdown_tx);
    
    // 5. Kısa bir süre bekle ki pending işlemler (logger flush, WebSocket cleanup) tamamlansın
    info!("waiting for background tasks to complete...");
    tokio::time::sleep(Duration::from_millis(1000)).await; // 500ms → 1000ms (daha güvenli)
    
    info!("graceful shutdown completed");
    Ok(())
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
    match venue.available_balance(quote_asset).await {
        Ok(balance) => balance.to_f64().unwrap_or(0.0),
        Err(_) => 0.0,
    }
}

/// Collect unique quote assets from all symbol states
fn collect_unique_quote_assets(states: &[SymbolState]) -> Vec<String> {
    let mut quote_assets = std::collections::HashSet::new();
    for state in states {
        quote_assets.insert(state.meta.quote_asset.clone());
    }
    quote_assets.into_iter().collect()
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
                        state.last_fill_time = Some(std::time::Instant::now());
    state.last_decay_period = None; // Fill oldu, decay period'u sıfırla
    state.last_decay_check = None; // Fill oldu, decay check'i sıfırla (hemen kontrol et)
    state.order_fill_rate = (state.order_fill_rate * increase_factor + increase_bonus)
        .min(1.0);
}

/// Update fill rate on order cancel
fn update_fill_rate_on_cancel(state: &mut SymbolState, decrease_factor: f64) {
    state.consecutive_no_fills += 1;
    state.order_fill_rate = (state.order_fill_rate * decrease_factor).max(0.0);
}

/// Check if orders should be synced
fn should_sync_orders(
    state: &SymbolState,
    sync_interval_ms: u64,
) -> bool {
    if let Some(last_sync) = state.last_order_sync {
        let elapsed = last_sync.elapsed().as_millis() as u64;
        elapsed >= sync_interval_ms
    } else {
        true
    }
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

