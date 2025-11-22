use anyhow::Result;
use log::{info, warn};
use std::sync::Arc;
use crate::{
    cache,
    config::BotConfig,
    metrics_cache,
    risk_manager::{RiskLimits, RiskManager},
    symbol_scanner::{SymbolScanner, SymbolSelectionConfig},
    types::FileConfig,
    Connection, SharedState,
};

pub struct AppComponents {
    pub shared_state: SharedState,
    pub connection: Arc<Connection>,
    pub symbol_cache: Arc<cache::SymbolInfoCache>,
    pub depth_cache: Arc<cache::DepthCache>,
    pub risk_manager: Arc<RiskManager>,
    pub metrics_cache: Arc<metrics_cache::MetricsCache>,
    pub scanner: Arc<SymbolScanner>,
    pub file_config: FileConfig,
}

pub async fn initialize_app(config: &BotConfig) -> Result<AppComponents> {
    let shared_state = SharedState::new();
    let connection = Arc::new(Connection::new(config.clone()));

    connection.initialize_trading_settings().await?;

    let symbol_cache = Arc::new(cache::SymbolInfoCache::new());
    let depth_cache = Arc::new(cache::DepthCache::new());

    let file_cfg = FileConfig::load("config.yaml").unwrap_or_default();

    let risk_config = file_cfg.risk.as_ref();
    let risk_limits = RiskLimits::from_config(config.max_position_notional_usd, risk_config);
    let risk_manager = Arc::new(RiskManager::new(risk_limits, risk_config));

    let cache_update_interval = file_cfg
        .event_bus
        .as_ref()
        .and_then(|eb| eb.metrics_cache_update_interval_secs)
        .unwrap_or(300);
    let metrics_cache = Arc::new(metrics_cache::MetricsCache::new(cache_update_interval));

    let allowed_quotes = determine_allowed_quotes(&file_cfg);
    use crate::connection::rest;
    let (usdt_balance, usdc_balance) = match rest::fetch_balance(&connection).await {
        Ok(balances) => {
            let usdt = balances
                .iter()
                .find(|b| b.asset == "USDT")
                .map(|b| b.free)
                .unwrap_or(0.0);
            let usdc = balances
                .iter()
                .find(|b| b.asset == "USDC")
                .map(|b| b.free)
                .unwrap_or(0.0);
            (usdt, usdc)
        }
        Err(err) => {
            warn!("Failed to fetch balance: {}, using 0.0 for both", err);
            (0.0, 0.0)
        }
    };

    let temp_scanner_config = SymbolSelectionConfig::from_file_config(&file_cfg, allowed_quotes.clone());
    let min_balance_threshold = temp_scanner_config.min_balance_threshold(&file_cfg);

    let mut final_allowed_quotes = Vec::new();
    if allowed_quotes.contains(&"USDT".to_string()) {
        if usdt_balance >= min_balance_threshold {
            final_allowed_quotes.push("USDT".to_string());
            info!(
                "INIT: USDT balance {:.2} >= threshold {:.2}, enabling USDT quote",
                usdt_balance, min_balance_threshold
            );
        } else {
            warn!(
                "INIT: USDT balance {:.2} < threshold {:.2}, disabling USDT quote",
                usdt_balance, min_balance_threshold
            );
        }
    }
    if allowed_quotes.contains(&"USDC".to_string()) {
        if usdc_balance >= min_balance_threshold {
            final_allowed_quotes.push("USDC".to_string());
            info!(
                "INIT: USDC balance {:.2} >= threshold {:.2}, enabling USDC quote",
                usdc_balance, min_balance_threshold
            );
        } else {
            warn!(
                "INIT: USDC balance {:.2} < threshold {:.2}, disabling USDC quote",
                usdc_balance, min_balance_threshold
            );
        }
    }

    if final_allowed_quotes.is_empty() {
        warn!(
            "INIT: No quotes have sufficient balance, falling back to USDT"
        );
        final_allowed_quotes.push("USDT".to_string());
    }

    let scanner_config = SymbolSelectionConfig::from_file_config(&file_cfg, final_allowed_quotes);
    let scanner = Arc::new(SymbolScanner::new(scanner_config));

    Ok(AppComponents {
        shared_state,
        connection,
        symbol_cache,
        depth_cache,
        risk_manager,
        metrics_cache,
        scanner,
        file_config: file_cfg,
    })
}

fn determine_allowed_quotes(file_cfg: &FileConfig) -> Vec<String> {
    if file_cfg.allow_usdt_quote.unwrap_or(false) {
        vec!["USDT".to_string(), "USDC".to_string()]
    } else {
        vec![file_cfg
            .quote_asset
            .clone()
            .unwrap_or_else(|| "USDT".to_string())]
    }
}

pub struct EventBusConfig {
    pub market_tick_buffer: usize,
    pub trade_signal_buffer: usize,
    pub close_request_buffer: usize,
    pub order_update_buffer: usize,
    pub position_update_buffer: usize,
    pub balance_update_buffer: usize,
}

impl EventBusConfig {
    pub fn from_file_config(file_cfg: &FileConfig) -> Self {
        let event_bus_cfg = file_cfg.event_bus.as_ref().cloned().unwrap_or_default();
        Self {
            market_tick_buffer: event_bus_cfg.market_tick_buffer.unwrap_or(2000),
            trade_signal_buffer: event_bus_cfg.trade_signal_buffer.unwrap_or(500),
            close_request_buffer: event_bus_cfg.close_request_buffer.unwrap_or(500),
            order_update_buffer: event_bus_cfg.order_update_buffer.unwrap_or(1000),
            position_update_buffer: event_bus_cfg.position_update_buffer.unwrap_or(500),
            balance_update_buffer: event_bus_cfg.balance_update_buffer.unwrap_or(100),
        }
    }
}

