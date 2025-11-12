//location: /crates/app/src/app_init.rs
// Application initialization logic extracted from main.rs

use anyhow::{anyhow, Result};
use crate::exchange::{BinanceCommon, BinanceFutures, UserDataStream, UserEvent, UserStreamKind};
use crate::config::AppCfg;
use crate::types::*;
use crate::exec::decimal_places;
use crate::logger::create_logger;
use crate::risk::RiskLimits;
use crate::strategy::DynMmCfg;
// processor module re-exports discovery functions
use crate::utils::tif_from_cfg;
use crate::types::SymbolState;
use crate::utils::init_rate_limiter;
use rust_decimal::Decimal;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{info, warn};
use futures_util::stream::{self, StreamExt};

pub struct AppInitResult {
    pub cfg: AppCfg,
    pub venue: BinanceFutures,
    pub states: Vec<SymbolState>,
    pub risk_limits: RiskLimits,
    pub event_tx: mpsc::UnboundedSender<UserEvent>,
    pub event_rx: mpsc::UnboundedReceiver<UserEvent>,
    pub json_logger: crate::logger::SharedLogger,
    pub profit_guarantee: crate::utils::ProfitGuarantee,
    pub tick_ms: u64,
    pub min_usd_per_order: f64,
    pub tif: crate::types::Tif,
}

/// Initialize the application: config, venue, symbols, states
pub async fn initialize_app() -> Result<AppInitResult> {
    // Initialize logging
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .compact()
        .with_ansi(true)
        .init();

    let cfg = crate::config::load_config()?;
    
    // Validate config
    if cfg.price_tick <= 0.0 {
        return Err(anyhow!("price_tick must be positive"));
    }
    if cfg.qty_step <= 0.0 {
        return Err(anyhow!("qty_step must be positive"));
    }
    if cfg.max_usd_per_order <= 0.0 {
        return Err(anyhow!("max_usd_per_order must be positive"));
    }
    
    // Initialize metrics
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

    // Build strategy config
    let dyn_cfg = build_strategy_config(&cfg)?;
    let strategy_name = cfg.strategy.r#type.clone();

    // Initialize venue
    let venue = initialize_venue(&cfg).await?;
    
    // Discover and initialize symbols
    let mut states = initialize_symbols(&venue, &cfg, &dyn_cfg, &strategy_name).await?;

    // Build risk limits
    let risk_limits = RiskLimits {
        inv_cap: Qty(Decimal::from_str_radix(&cfg.risk.inv_cap, 10)
            .map_err(|e| anyhow!("invalid risk.inv_cap: {}", e))?),
        min_liq_gap_bps: cfg.risk.min_liq_gap_bps,
        dd_limit_bps: cfg.risk.dd_limit_bps,
        max_leverage: cfg.risk.max_leverage,
    };

    // Setup WebSocket event channel
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    if cfg.websocket.enabled {
        setup_websocket(&cfg, event_tx.clone()).await;
    }

    // Calculate trading parameters
    let tick_ms = std::cmp::max(cfg.internal.min_tick_interval_ms, cfg.exec.cancel_replace_interval_ms);
    let min_usd_per_order = cfg.min_usd_per_order.unwrap_or(0.0);
    
    let min_profit_usd = cfg.strategy.min_profit_usd.unwrap_or(0.50);
    let maker_fee_rate = cfg.strategy.maker_fee_rate.unwrap_or(0.0002);
    let taker_fee_rate = cfg.strategy.taker_fee_rate.unwrap_or(0.0004);
    let profit_guarantee = crate::utils::ProfitGuarantee::new(min_profit_usd, maker_fee_rate, taker_fee_rate);
    let tif = tif_from_cfg(&cfg.exec.tif);

    Ok(AppInitResult {
        cfg,
        venue,
        states,
        risk_limits,
        event_tx,
        event_rx,
        json_logger,
        profit_guarantee,
        tick_ms,
        min_usd_per_order,
        tif,
    })
}

fn build_strategy_config(cfg: &AppCfg) -> Result<DynMmCfg> {
    Ok(DynMmCfg {
        a: cfg.strategy.a,
        b: cfg.strategy.b,
        base_size: Decimal::from_str_radix(&cfg.strategy.base_size, 10)
            .map_err(|e| anyhow!("invalid strategy.base_size: {}", e))?,
        inv_cap: Decimal::from_str_radix(
            cfg.strategy.inv_cap.as_deref().unwrap_or(&cfg.risk.inv_cap),
            10,
        )
        .map_err(|e| anyhow!("invalid strategy.inv_cap or risk.inv_cap: {}", e))?,
        min_spread_bps: cfg.strategy.min_spread_bps.unwrap_or(30.0),
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
        opportunity_size_multiplier: cfg.strategy.opportunity_size_multiplier.unwrap_or(1.05),
        strong_trend_multiplier: cfg.strategy.strong_trend_multiplier.unwrap_or(1.0),
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
    })
}

async fn initialize_venue(cfg: &AppCfg) -> Result<BinanceFutures> {
    let client = reqwest::Client::builder().build()?;
    let common = BinanceCommon {
        client,
        api_key: cfg.binance.api_key.clone(),
        secret_key: cfg.binance.secret_key.clone(),
        recv_window_ms: cfg.binance.recv_window_ms,
    };

    let price_tick_dec = Decimal::from_f64_retain(cfg.price_tick)
        .ok_or_else(|| anyhow!("Failed to convert price_tick {} to Decimal", cfg.price_tick))?;
    let qty_step_dec = Decimal::from_f64_retain(cfg.qty_step)
        .ok_or_else(|| anyhow!("Failed to convert qty_step {} to Decimal", cfg.qty_step))?;
    let price_precision = decimal_places(price_tick_dec);
    let qty_precision = decimal_places(qty_step_dec);
    
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
    
    // Set position side mode
    if let Err(err) = venue.set_position_side_dual(hedge_mode).await {
        warn!(hedge_mode, error = %err, "failed to set position side mode, continuing anyway");
    } else {
        info!(hedge_mode, "position side mode set successfully");
    }

    Ok(venue)
}

async fn initialize_symbols(
    venue: &BinanceFutures,
    cfg: &AppCfg,
    dyn_cfg: &DynMmCfg,
    strategy_name: &str,
) -> Result<Vec<SymbolState>> {
    let metadata = venue.symbol_metadata().await?;

    // Discover symbols
    let mut selected = crate::processor::discover_symbols(venue, cfg, &metadata).await?;

    // Wait for balance if no symbols found
    if selected.is_empty() {
        warn!(
            quote_asset = %cfg.quote_asset,
            min_required = cfg.min_quote_balance_usd,
            "no eligible symbols found - waiting for balance to become available"
        );
        selected = crate::processor::wait_and_retry_discovery(venue, cfg, &metadata).await?;
    }

    // Clear cache for fresh startup
    info!("clearing cached exchange rules for fresh startup");
    crate::exchange::FUT_RULES.clear();

    // Initialize symbol states
    let mut states = crate::processor::initialize_symbol_states(selected, dyn_cfg, strategy_name, cfg);
    
    // Fetch per-symbol metadata in parallel
    info!(
        total_symbols = states.len(),
        "fetching per-symbol metadata for quantization (parallel processing)..."
    );
    
    let venue_arc = Arc::new(venue.clone());
    let symbols: Vec<(String, usize)> = states.iter()
        .enumerate()
        .map(|(idx, state)| (state.meta.symbol.clone(), idx))
        .collect();
    
    const CONCURRENT_LIMIT: usize = 10;
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

    // Setup margin and leverage
    crate::processor::setup_margin_and_leverage(venue, &mut states, cfg).await?;

    let symbol_list: Vec<String> = states.iter().map(|s| s.meta.symbol.clone()).collect();
    info!(symbols = ?symbol_list, mode = %cfg.mode, "bot started with real Binance venue");

    Ok(states)
}

async fn setup_websocket(cfg: &AppCfg, event_tx: mpsc::UnboundedSender<UserEvent>) {
    let client = reqwest::Client::builder().build().unwrap();
    let api_key = cfg.binance.api_key.clone();
    let futures_base = cfg.binance.futures_base.clone();
    let reconnect_delay = Duration::from_millis(cfg.websocket.reconnect_delay_ms);
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
                    let tx_sync = event_tx.clone();
                    stream.set_on_reconnect(move || {
                        let _ = tx_sync.send(UserEvent::Heartbeat);
                        info!("reconnect callback triggered, sync event sent");
                    });
                    
                    let mut first_event_after_reconnect = true;
                    loop {
                        match stream.next_event().await {
                            Ok(event) => {
                                if first_event_after_reconnect {
                                    first_event_after_reconnect = false;
                                    let _ = event_tx.send(UserEvent::Heartbeat);
                                }
                                if event_tx.send(event).is_err() {
                                    break;
                                }
                            }
                            Err(_) => {
                                break;
                            }
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

