//location: /crates/app/src/symbol_discovery.rs
// Symbol discovery and initialization logic

use anyhow::Result;
use crate::exec::binance::BinanceFutures;
use crate::exec::binance::SymbolMeta;
use tracing::{debug, info, warn};
use crate::utils::{is_usd_stable, rate_limit_guard};
use crate::config::AppCfg;
use crate::types::SymbolState;
use crate::strategy::{DynMm, DynMmCfg, Strategy};
use crate::qmel::QMelStrategy;
use crate::core::types::Qty;
use std::collections::{HashMap, HashSet};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

/// Discover and filter symbols based on configuration
pub async fn discover_symbols(
    venue: &BinanceFutures,
    cfg: &AppCfg,
    metadata: &[SymbolMeta],
) -> Result<Vec<SymbolMeta>> {
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
        if !sym.is_ascii() {
            warn!(
                symbol = %sym,
                "skipping symbol with non-ASCII characters"
            );
            continue;
        }
        
        if let Some(meta) = metadata.iter().find(|m| &m.symbol == sym) {
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

            let have_min = cfg.min_usd_per_order.unwrap_or(0.0);
            let avail = match venue.available_balance(&meta.quote_asset).await {
                Ok(b) => b.to_f64().unwrap_or(0.0),
                Err(_) => {
                    warn!(
                        symbol = %meta.symbol,
                        quote_asset = %meta.quote_asset,
                        "Failed to get balance, using 0.0"
                    );
                    0.0
                }
            };
            if avail < have_min {
                warn!(
                    symbol = %sym,
                    quote = %meta.quote_asset,
                    avail,
                    min_needed = have_min,
                    "skipping symbol at discovery: zero/low quote balance"
                );
                continue;
            }

            selected.push(meta.clone());
        } else {
            warn!(symbol = %sym, "configured symbol not found on venue");
        }
    }

    if selected.is_empty() && cfg.auto_discover_quote {
        selected = auto_discover_symbols(venue, cfg, metadata).await?;
    }

    Ok(selected)
}

/// Wait for balance to become available and retry symbol discovery
pub async fn wait_and_retry_discovery(
    venue: &BinanceFutures,
    cfg: &AppCfg,
    metadata: &[SymbolMeta],
) -> Result<Vec<SymbolMeta>> {
    loop {
        use tokio::time::{sleep, Duration};
        sleep(Duration::from_secs(cfg.internal.symbol_discovery_retry_interval_sec)).await;
        
        let retry_selected = discover_symbols(venue, cfg, metadata).await?;
        
        if !retry_selected.is_empty() {
            info!(
                count = retry_selected.len(),
                quote_asset = %cfg.quote_asset,
                "balance became available, proceeding with symbol initialization"
            );
            return Ok(retry_selected);
        } else {
            info!(
                quote_asset = %cfg.quote_asset,
                min_required = cfg.min_quote_balance_usd,
                "still waiting for balance to become available..."
            );
        }
    }
}

/// Auto-discover symbols based on quote asset
async fn auto_discover_symbols(
    venue: &BinanceFutures,
    cfg: &AppCfg,
    metadata: &[SymbolMeta],
) -> Result<Vec<SymbolMeta>> {
    let want_group = is_usd_stable(&cfg.quote_asset);
    let mut auto: Vec<SymbolMeta> = metadata
        .iter()
        .filter(|m| {
            let match_primary_quote = if want_group {
                is_usd_stable(&m.quote_asset)
            } else {
                m.quote_asset.eq_ignore_ascii_case(&cfg.quote_asset)
            };
            
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
    
    // Filter by balance
    let mut quote_asset_balances: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    let unique_quotes: std::collections::HashSet<String> = auto.iter()
        .map(|m| m.quote_asset.clone())
        .collect();
        
    for quote in unique_quotes {
        let balance = venue.available_balance(&quote).await
            .map(|b| b.to_f64().unwrap_or(0.0))
            .unwrap_or(0.0);
        quote_asset_balances.insert(quote.clone(), balance);
        
        if balance < cfg.min_quote_balance_usd {
            info!(
                quote_asset = %quote,
                balance,
                min_required = cfg.min_quote_balance_usd,
                "FILTERING: quote asset balance insufficient"
            );
        }
    }
    
    auto.retain(|m| {
        if let Some(&balance) = quote_asset_balances.get(&m.quote_asset) {
            balance >= cfg.min_quote_balance_usd
        } else {
            false
        }
    });
    
    auto.retain(|m| m.symbol.is_ascii());
    auto.sort_by(|a, b| a.symbol.cmp(&b.symbol));
    
    info!(
        count = auto.len(),
        quote_asset = %cfg.quote_asset,
        "auto-discovered symbols"
    );
    
    Ok(auto)
}

/// Initialize symbol states with strategies
pub fn initialize_symbol_states(
    selected: Vec<SymbolMeta>,
    dyn_cfg: &DynMmCfg,
    strategy_name: &str,
    cfg: &AppCfg,
) -> Vec<SymbolState> {
    let build_strategy = |symbol: &str| -> Box<dyn Strategy> {
        let dyn_cfg_clone = dyn_cfg.clone();
        match strategy_name {
            "dyn_mm" => Box::new(DynMm::from(dyn_cfg_clone)),
            "qmel" => {
                let maker_fee = cfg.strategy.maker_fee_rate.unwrap_or(0.0001);
                let taker_fee = cfg.strategy.taker_fee_rate.unwrap_or(0.0004);
                let ev_threshold = cfg.strategy.qmel_ev_threshold.unwrap_or(0.10);
                let min_margin = cfg.strategy.qmel_min_margin_usdc.unwrap_or(10.0);
                let max_margin = cfg.strategy.qmel_max_margin_usdc.unwrap_or(100.0);
                let max_leverage = cfg.risk.max_leverage as f64;
                Box::new(QMelStrategy::new(
                    maker_fee,
                    taker_fee,
                    ev_threshold,
                    min_margin,
                    max_margin,
                    max_leverage,
                ))
            },
            other => {
                warn!(symbol = %symbol, strategy = %other, "unknown strategy type, defaulting dyn_mm");
                Box::new(DynMm::from(dyn_cfg_clone))
            }
        }
    };

    let mut states = Vec::new();
    for meta in selected {
        let strategy = build_strategy(&meta.symbol);
        states.push(SymbolState {
            meta,
            inv: Qty(Decimal::ZERO),
            strategy,
            active_orders: HashMap::new(),
            pnl_history: Vec::new(),
            min_notional_req: None,
            disabled: false,
            disabled_until: None,
            symbol_rules: None,
            rules_fetch_failed: false,
            last_rules_retry: None,
            test_order_passed: false,
            last_position_check: None,
            last_order_sync: None,
            order_fill_rate: cfg.internal.initial_fill_rate,
            consecutive_no_fills: 0,
            last_fill_time: None,
            last_inventory_update: None,
            last_decay_period: None,
            last_decay_check: None,
            position_entry_time: None,
            peak_pnl: Decimal::ZERO,
            last_peak_update: None,
            position_hold_duration_ms: 0,
            last_order_price_update: HashMap::new(),
            last_cancel_all_time: None,
            cancel_all_attempt_count: 0,
            daily_pnl: Decimal::ZERO,
            total_funding_cost: Decimal::ZERO,
            position_size_notional_history: Vec::with_capacity(cfg.internal.position_size_history_max_len),
            last_pnl_alert: None,
            cumulative_pnl: Decimal::ZERO,
            last_applied_funding_time: None,
            last_daily_reset: None,
            // PnL tracking for summary
            trade_count: 0,
            profitable_trade_count: 0,
            losing_trade_count: 0,
            total_profit: Decimal::ZERO,
            total_loss: Decimal::ZERO,
            largest_win: Decimal::ZERO,
            largest_loss: Decimal::ZERO,
            total_fees_paid: Decimal::ZERO,
            last_pnl_summary_time: None,
            last_daily_reset_date: None,
            avg_entry_price: None,
            last_direction_change: None,
            current_direction: None,
            direction_signal_strength: 0.0,
            regime: None,
            position_closing: false,
            last_close_attempt: None,
            processed_events: HashSet::new(),
            last_event_cleanup: None,
        });
    }
    
    states
}

/// Setup margin type and leverage for all symbols
pub async fn setup_margin_and_leverage(
    venue: &BinanceFutures,
    states: &mut [SymbolState],
    cfg: &AppCfg,
) -> Result<()> {
    if cfg.mode != "futures" {
        return Ok(());
    }

    let use_isolated = cfg.risk.use_isolated_margin;
    let leverage_to_set = cfg.exec.default_leverage
        .or(cfg.leverage)
        .unwrap_or(1);

    if use_isolated {
        let mut isolated_set_count = 0;
        let mut isolated_skip_count = 0;
        let mut isolated_fail_count = 0;
        
        info!("checking and setting isolated margin for all symbols");
        for state in states.iter() {
            let symbol = &state.meta.symbol;
            
            rate_limit_guard(1).await;
            match venue.get_margin_type(symbol).await {
                Ok(current_is_isolated) => {
                    if current_is_isolated == use_isolated {
                        isolated_skip_count += 1;
                        debug!(%symbol, "margin type already set to isolated, skipping");
                        continue;
                    }
                }
                Err(e) => {
                    warn!(%symbol, error = %e, "failed to get margin type, will attempt to set anyway");
                }
            }
            
            rate_limit_guard(1).await;
            match venue.set_margin_type(symbol, true).await {
                Ok(_) => {
                    isolated_set_count += 1;
                    info!(%symbol, "isolated margin set successfully");
                }
                Err(err) => {
                    let error_str = err.to_string();
                    let error_lower = error_str.to_lowercase();
                    
                    if error_lower.contains("-4046") || error_lower.contains("no need to change") {
                        isolated_skip_count += 1;
                        debug!(%symbol, "margin type already isolated, no change needed");
                    } else {
                        isolated_fail_count += 1;
                        warn!(%symbol, error = %err, "failed to set isolated margin");
                    }
                }
            }
        }
        
        info!(
            isolated_set = isolated_set_count,
            isolated_skip = isolated_skip_count,
            isolated_fail = isolated_fail_count,
            "isolated margin setup completed"
        );
    }
    
    // Set leverage
    let mut leverage_set_count = 0;
    let mut leverage_skip_count = 0;
    let mut leverage_fail_count = 0;
    
    info!(leverage = leverage_to_set, "checking and setting leverage for all symbols");
    for state in states.iter() {
        let symbol = &state.meta.symbol;
        
        rate_limit_guard(5).await;
        match venue.get_leverage(symbol).await {
            Ok(current_leverage) => {
                if current_leverage == leverage_to_set {
                    leverage_skip_count += 1;
                    debug!(%symbol, leverage = current_leverage, "leverage already set, skipping");
                    continue;
                }
            }
            Err(e) => {
                warn!(%symbol, error = %e, "failed to get leverage, will attempt to set anyway");
            }
        }
        
        rate_limit_guard(1).await;
        match venue.set_leverage(symbol, leverage_to_set).await {
            Ok(_) => {
                leverage_set_count += 1;
                info!(%symbol, leverage = leverage_to_set, "leverage set successfully");
            }
            Err(err) => {
                let error_str = err.to_string();
                let error_lower = error_str.to_lowercase();
                
                if error_lower.contains("-4059") || error_lower.contains("no need to change") {
                    leverage_skip_count += 1;
                    debug!(%symbol, leverage = leverage_to_set, "leverage already set, no change needed");
                } else {
                    leverage_fail_count += 1;
                    warn!(%symbol, leverage = leverage_to_set, error = %err, "failed to set leverage");
                }
            }
        }
    }
    
    info!(
        leverage_set = leverage_set_count,
        leverage_skip = leverage_skip_count,
        leverage_fail = leverage_fail_count,
        "leverage setup completed"
    );
    
    Ok(())
}

