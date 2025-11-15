// Configuration structures and loading logic
// Clean architecture - minimal config for event-driven modules

use anyhow::{anyhow, Result};
use serde::Deserialize;

// ============================================================================
// Configuration Structures
// ============================================================================

#[derive(Debug, Deserialize, Clone, Default)]
pub struct RiskCfg {
    /// Maximum allowed leverage (validation only, not used as default)
    /// Used to validate that leverage and exec.default_leverage don't exceed this limit
    #[serde(default = "default_max_leverage")]
    pub max_leverage: u32,
    #[serde(default = "default_use_isolated_margin")]
    pub use_isolated_margin: bool,
    #[serde(default = "default_max_position_notional_usd")]
    pub max_position_notional_usd: f64,
    /// Maker commission rate (percentage, e.g., 0.02 for 0.02%)
    /// Maker orders add liquidity to the order book (post-only orders)
    #[serde(default = "default_maker_commission_pct")]
    pub maker_commission_pct: f64,
    /// Taker commission rate (percentage, e.g., 0.04 for 0.04%)
    /// Taker orders remove liquidity from the order book (market orders, IOC orders)
    /// Default: 0.04% (Binance futures standard taker fee)
    #[serde(default = "default_taker_commission_pct")]
    pub taker_commission_pct: f64,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct TrendingCfg {
    #[serde(default = "default_min_spread_bps")]
    pub min_spread_bps: f64,
    #[serde(default = "default_max_spread_bps")]
    pub max_spread_bps: f64,
    #[serde(default = "default_signal_cooldown_seconds")]
    pub signal_cooldown_seconds: u64,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct ExecCfg {
    #[serde(default = "default_tif")]
    pub tif: String,
    /// Default leverage to use when leverage (top-level) is not set
    /// Used as fallback: cfg.leverage.unwrap_or(cfg.exec.default_leverage)
    #[serde(default = "default_default_leverage")]
    pub default_leverage: u32,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct WebsocketCfg {
    #[serde(default = "default_ws_enabled")]
    pub enabled: bool,
    #[serde(default = "default_ws_reconnect_delay")]
    pub reconnect_delay_ms: u64,
    #[serde(default = "default_ws_ping_interval")]
    pub ping_interval_ms: u64,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct EventBusCfg {
    /// Buffer size for MarketTick events (high frequency, needs larger buffer)
    /// With 100 symbols at 1 tick/sec = 100 ticks/sec, 1000 buffer = ~10 seconds (sufficient for most use cases)
    #[serde(default = "default_market_tick_buffer")]
    pub market_tick_buffer: usize,
    /// Buffer size for TradeSignal events
    #[serde(default = "default_trade_signal_buffer")]
    pub trade_signal_buffer: usize,
    /// Buffer size for CloseRequest events
    #[serde(default = "default_default_event_buffer")]
    pub close_request_buffer: usize,
    /// Buffer size for OrderUpdate events
    #[serde(default = "default_default_event_buffer")]
    pub order_update_buffer: usize,
    /// Buffer size for PositionUpdate events
    #[serde(default = "default_default_event_buffer")]
    pub position_update_buffer: usize,
    /// Buffer size for BalanceUpdate events
    #[serde(default = "default_default_event_buffer")]
    pub balance_update_buffer: usize,
}

#[derive(Debug, Deserialize, Clone)]
pub struct BinanceCfg {
    pub api_key: String,
    pub secret_key: String,
    #[serde(default = "default_recv_window")]
    pub recv_window_ms: u64,
    pub futures_base: String,
    #[serde(default = "default_hedge_mode")]
    pub hedge_mode: bool,
}

impl Default for BinanceCfg {
    fn default() -> Self {
        Self {
            api_key: "test_api_key".to_string(),
            secret_key: "test_secret_key".to_string(),
            recv_window_ms: default_recv_window(),
            futures_base: "https://fapi.binance.com".to_string(),
            hedge_mode: default_hedge_mode(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct AppCfg {
    #[serde(default)]
    pub symbol: Option<String>,
    #[serde(default)]
    pub symbols: Vec<String>,
    #[serde(default = "default_auto_discover_quote")]
    pub auto_discover_quote: bool,
    #[serde(default = "default_quote_asset")]
    pub quote_asset: String,
    #[serde(default = "default_allow_usdt_quote")]
    pub allow_usdt_quote: bool,
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default = "default_max_usd_per_order")]
    pub max_usd_per_order: f64,
    #[serde(default = "default_min_usd_per_order")]
    pub min_usd_per_order: f64,
    #[serde(default = "default_min_quote_balance_usd")]
    pub min_quote_balance_usd: f64,
    /// Explicit leverage setting (optional)
    /// If set, this value is used. Otherwise, exec.default_leverage is used.
    /// Must not exceed risk.max_leverage (validated in validate_config)
    #[serde(default)]
    pub leverage: Option<u32>,
    #[serde(default = "default_price_tick")]
    pub price_tick: f64,
    #[serde(default = "default_qty_step")]
    pub qty_step: f64,
    #[serde(default = "default_take_profit_pct")]
    pub take_profit_pct: f64,
    #[serde(default = "default_stop_loss_pct")]
    pub stop_loss_pct: f64,
    pub binance: BinanceCfg,
    #[serde(default)]
    pub risk: RiskCfg,
    #[serde(default)]
    pub trending: TrendingCfg,
    #[serde(default)]
    pub exec: ExecCfg,
    #[serde(default)]
    pub websocket: WebsocketCfg,
    #[serde(default)]
    pub event_bus: EventBusCfg,
}

impl Default for AppCfg {
    fn default() -> Self {
        Self {
            symbol: None,
            symbols: Vec::new(),
            auto_discover_quote: default_auto_discover_quote(),
            quote_asset: default_quote_asset(),
            allow_usdt_quote: default_allow_usdt_quote(),
            mode: default_mode(),
            max_usd_per_order: default_max_usd_per_order(),
            min_usd_per_order: default_min_usd_per_order(),
            min_quote_balance_usd: default_min_quote_balance_usd(),
            leverage: None,
            price_tick: default_price_tick(),
            qty_step: default_qty_step(),
            take_profit_pct: default_take_profit_pct(),
            stop_loss_pct: default_stop_loss_pct(),
            binance: BinanceCfg::default(),
            risk: RiskCfg::default(),
            trending: TrendingCfg::default(),
            exec: ExecCfg::default(),
            websocket: WebsocketCfg::default(),
            event_bus: EventBusCfg::default(),
        }
    }
}

// ============================================================================
// Default Value Functions
// ============================================================================

fn default_max_leverage() -> u32 {
    50
}

fn default_use_isolated_margin() -> bool {
    true
}

fn default_max_position_notional_usd() -> f64 {
    1000.0
}

fn default_maker_commission_pct() -> f64 {
    0.02 // 0.02% (Binance futures standard maker fee)
}

fn default_taker_commission_pct() -> f64 {
    0.04 // 0.04% (Binance futures standard taker fee)
}

fn default_min_spread_bps() -> f64 {
    5.0
}

fn default_max_spread_bps() -> f64 {
    200.0
}

fn default_signal_cooldown_seconds() -> u64 {
    30 // Default: 30 seconds cooldown between signals for same symbol
}

fn default_tif() -> String {
    "post_only".to_string()
}

fn default_default_leverage() -> u32 {
    20
}

fn default_ws_enabled() -> bool {
    true
}

fn default_ws_reconnect_delay() -> u64 {
    5_000
}

fn default_ws_ping_interval() -> u64 {
    30_000
}

fn default_recv_window() -> u64 {
    5_000
}

fn default_hedge_mode() -> bool {
    false
}

fn default_auto_discover_quote() -> bool {
    true
}

fn default_quote_asset() -> String {
    "USDC".to_string()
}

fn default_allow_usdt_quote() -> bool {
    true
}

fn default_mode() -> String {
    "futures".to_string()
}

fn default_max_usd_per_order() -> f64 {
    100.0
}

fn default_min_usd_per_order() -> f64 {
    10.0
}

fn default_min_quote_balance_usd() -> f64 {
    1.0
}

fn default_price_tick() -> f64 {
    0.001
}

fn default_qty_step() -> f64 {
    0.001
}

fn default_take_profit_pct() -> f64 {
    5.0
}

fn default_stop_loss_pct() -> f64 {
    2.0
}

fn default_market_tick_buffer() -> usize {
    1000 // Reduced from 10000 - 1000 is sufficient for most use cases
}

fn default_trade_signal_buffer() -> usize {
    1000
}

fn default_default_event_buffer() -> usize {
    1000
}

// ============================================================================
// Configuration Loading
// ============================================================================

/// Load application configuration from file or command line arguments.
///
/// This function loads the configuration from a YAML file. The file path can be specified
/// via command line argument `--config <path>`, or it defaults to `./config.yaml`.
///
/// # Returns
///
/// Returns `Ok(AppCfg)` if the configuration file is found and valid, or `Err` if:
/// - The file cannot be read
/// - The YAML is invalid or cannot be deserialized
/// - Configuration validation fails (see `validate_config`)
///
/// # Configuration File Format
///
/// The configuration file should be in YAML format and include:
/// - `binance`: API keys and exchange settings
/// - `symbols` or `symbol`: Trading symbols
/// - `risk`: Risk management parameters (leverage, margin type)
/// - `trending`: Trend analysis parameters
/// - `exec`: Execution parameters (TIF, leverage)
/// - `event_bus`: Event bus buffer sizes
///
/// # Example
///
/// ```no_run
/// use crate::config::load_config;
///
/// // Load from default path (./config.yaml)
/// let cfg = load_config()?;
///
/// // Or specify path via command line:
/// // cargo run -- --config /path/to/config.yaml
/// ```
///
/// # Errors
///
/// Common errors include:
/// - Missing or invalid API keys
/// - Invalid leverage settings
/// - Missing required fields
/// - File I/O errors
pub fn load_config() -> Result<AppCfg> {
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

    let content = std::fs::read_to_string(&path)?;
    let cfg: AppCfg = serde_yaml::from_str(&content)?;

    validate_config(&cfg)?;
    Ok(cfg)
}

/// Validate configuration values
fn validate_config(cfg: &AppCfg) -> Result<()> {
    if cfg.price_tick <= 0.0 {
        return Err(anyhow!("price_tick must be positive"));
    }
    if cfg.qty_step <= 0.0 {
        return Err(anyhow!("qty_step must be positive"));
    }
    if cfg.max_usd_per_order <= 0.0 {
        return Err(anyhow!("max_usd_per_order must be positive"));
    }

    // Quote asset validation: Only USDC or USDT accepted
    let quote_upper = cfg.quote_asset.to_uppercase();
    if quote_upper != "USDC" && quote_upper != "USDT" {
        return Err(anyhow!(
            "quote_asset must be either 'USDC' or 'USDT', got '{}'. Only USDC and USDT are supported.",
            cfg.quote_asset
        ));
    }

    // API key validation
    if cfg.binance.api_key.trim().is_empty() {
        return Err(anyhow!(
            "binance.api_key is required but is empty. Please set your API key in config.yaml"
        ));
    }
    if cfg.binance.secret_key.trim().is_empty() {
        return Err(anyhow!(
            "binance.secret_key is required but is empty. Please set your secret key in config.yaml"
        ));
    }

    // API key format check (Binance API keys are typically 64 characters)
    if cfg.binance.api_key.len() < 20 {
        return Err(anyhow!(
            "binance.api_key appears to be invalid (too short). Binance API keys are typically 64 characters long"
        ));
    }
    if cfg.binance.secret_key.len() < 20 {
        return Err(anyhow!(
            "binance.secret_key appears to be invalid (too short). Binance secret keys are typically 64 characters long"
        ));
    }

    // Leverage validation
    if let Some(leverage) = cfg.leverage {
        if leverage == 0 {
            return Err(anyhow!("leverage must be greater than 0"));
        }
        if leverage > cfg.risk.max_leverage {
            return Err(anyhow!(
                "leverage ({}) exceeds max_leverage ({})",
                leverage,
                cfg.risk.max_leverage
            ));
        }
    }
    
    // CRITICAL: Cross margin mode validation
    // TP/SL PnL calculation in follow_orders.rs assumes isolated margin
    // Cross margin uses shared account equity, which requires different PnL calculation
    // Formula for isolated: PnL% = PriceChange% × Leverage
    // Formula for cross: PnL% = (PriceChange% × PositionNotional) / TotalAccountEquity
    // Using isolated formula with cross margin causes incorrect TP/SL triggers:
    // - Premature or delayed TP/SL triggers
    // - Incorrect risk management
    // - Potential financial losses
    if !cfg.risk.use_isolated_margin {
        return Err(anyhow!(
            "CRITICAL: Cross margin mode is NOT supported for TP/SL. \
             PnL calculation in follow_orders.rs assumes isolated margin. \
             Cross margin requires different PnL calculation formula that accounts for shared account equity. \
             Please set risk.use_isolated_margin: true in config.yaml"
        ));
    }
    
    // Also validate exec.default_leverage
    if cfg.exec.default_leverage == 0 {
        return Err(anyhow!("exec.default_leverage must be greater than 0"));
    }
    if cfg.exec.default_leverage > cfg.risk.max_leverage {
        return Err(anyhow!(
            "exec.default_leverage ({}) exceeds risk.max_leverage ({})",
            cfg.exec.default_leverage,
            cfg.risk.max_leverage
        ));
    }

    // Take profit percentage validation
    if cfg.take_profit_pct <= 0.0 {
        return Err(anyhow!("take_profit_pct must be greater than 0"));
    }
    if cfg.take_profit_pct >= 100.0 {
        return Err(anyhow!("take_profit_pct must be less than 100"));
    }

    // Stop loss percentage validation
    if cfg.stop_loss_pct <= 0.0 {
        return Err(anyhow!("stop_loss_pct must be greater than 0"));
    }
    if cfg.stop_loss_pct >= cfg.take_profit_pct {
        return Err(anyhow!(
            "stop_loss_pct ({}) must be less than take_profit_pct ({})",
            cfg.stop_loss_pct,
            cfg.take_profit_pct
        ));
    }

    // Order size validation
    if cfg.max_usd_per_order <= cfg.min_usd_per_order {
        return Err(anyhow!(
            "max_usd_per_order ({}) must be greater than min_usd_per_order ({})",
            cfg.max_usd_per_order,
            cfg.min_usd_per_order
        ));
    }
    if cfg.min_usd_per_order <= 0.0 {
        return Err(anyhow!("min_usd_per_order must be greater than 0"));
    }

    // Minimum quote balance validation
    if cfg.min_quote_balance_usd <= 0.0 {
        return Err(anyhow!("min_quote_balance_usd must be greater than 0"));
    }

    // Validate max_usd_per_order vs min_quote_balance_usd relationship
    // The minimum balance must be at least as large as the maximum order size
    // Otherwise, we might try to place orders larger than the available balance
    if cfg.min_quote_balance_usd < cfg.max_usd_per_order {
        return Err(anyhow!(
            "min_quote_balance_usd ({}) must be at least as large as max_usd_per_order ({}) to ensure sufficient balance for trading",
            cfg.min_quote_balance_usd,
            cfg.max_usd_per_order
        ));
    }

    // Validate that profitable trades are possible
    // For a trade to be profitable, take_profit_pct must exceed stop_loss_pct + total commission
    // Total commission = entry commission + exit commission (worst case: both taker fees)
    // Entry commission: taker_commission_pct
    // Exit commission (TP): taker_commission_pct (on exit price)
    // Exit commission (SL): taker_commission_pct (on exit price)
    // Worst case total commission ≈ 2 * taker_commission_pct (entry + exit)
    // We need: take_profit_pct > stop_loss_pct + (2 * taker_commission_pct)
    let total_commission_pct = 2.0 * cfg.risk.taker_commission_pct;
    let min_required_tp = cfg.stop_loss_pct + total_commission_pct;
    if cfg.take_profit_pct <= min_required_tp {
        return Err(anyhow!(
            "take_profit_pct ({}) must be greater than stop_loss_pct ({}) + total commission ({}). Current: {} <= {}. Profitable trades would be impossible.",
            cfg.take_profit_pct,
            cfg.stop_loss_pct,
            total_commission_pct,
            cfg.take_profit_pct,
            min_required_tp
        ));
    }

    // Validate trending spread configuration
    if cfg.trending.min_spread_bps < 0.0 {
        return Err(anyhow!("trending.min_spread_bps must be non-negative"));
    }
    if cfg.trending.max_spread_bps <= 0.0 {
        return Err(anyhow!("trending.max_spread_bps must be greater than 0"));
    }
    if cfg.trending.min_spread_bps > cfg.trending.max_spread_bps {
        return Err(anyhow!(
            "trending.min_spread_bps ({}) must be less than or equal to trending.max_spread_bps ({})",
            cfg.trending.min_spread_bps,
            cfg.trending.max_spread_bps
        ));
    }

    // CRITICAL: Validate hedge mode configuration
    // Hedge mode support is incomplete and causes system failures
    // 
    // Problem: Current implementation cannot handle hedge mode correctly
    // - Position struct only supports single position per symbol (one qty, one entry)
    // - TP/SL tracking is symbol-based, not position-side-based
    // - If both LONG and SHORT positions exist, only one is tracked (the other is lost)
    // - flatten_position closes ALL positions for the symbol (both LONG and SHORT)
    // - Position tracking is incomplete - LONG and SHORT should be tracked separately
    // 
    // This causes:
    // - Incorrect TP/SL triggers (only one position tracked)
    // - Unintended position closures (both LONG and SHORT closed when closing one)
    // - Position data loss (one position ignored)
    // - System instability and potential financial losses
    // 
    // Full hedge mode support requires:
    // - Position struct to support multiple positions per symbol (LONG and SHORT separately)
    // - Separate TP/SL tracking for LONG and SHORT positions
    // - position_id-based closing (CloseRequest.position_id)
    // - Separate position tracking in ORDERING state
    // - HashMap<(String, PositionSide), PositionInfo> for TP/SL tracking
    if cfg.binance.hedge_mode {
        return Err(anyhow!(
            "CRITICAL: Hedge mode (hedge_mode=true) is NOT supported. \
             Current implementation cannot handle hedge mode correctly and will cause system failures. \
             \
             Limitations: \
             - Position struct only supports single position per symbol \
             - TP/SL tracking is symbol-based, not position-side-based \
             - If both LONG and SHORT positions exist, only one is tracked \
             - flatten_position closes ALL positions (both LONG and SHORT) \
             \
             Please set binance.hedge_mode: false in config.yaml \
             \
             Full hedge mode support requires significant architectural changes: \
             - Position struct to support multiple positions per symbol \
             - Separate TP/SL tracking for LONG and SHORT \
             - position_id-based closing \
             - Separate position tracking in ORDERING state"
        ));
    }

    // ⚠️ CRITICAL: Validate margin mode configuration
    // TP/SL PnL calculation assumes isolated margin - warn if cross margin is used
    if !cfg.risk.use_isolated_margin {
        eprintln!("⚠️  WARNING: Cross margin mode (use_isolated_margin=false) is enabled.");
        eprintln!("   - TP/SL PnL calculation assumes isolated margin");
        eprintln!("   - TP/SL trigger levels will be INCORRECT with cross margin");
        eprintln!("   - This can lead to premature or delayed TP/SL triggers");
        eprintln!("   - This can lead to financial losses");
        eprintln!("   RECOMMENDATION: Use use_isolated_margin=true for correct TP/SL behavior.");
    }

    // ✅ CRITICAL: Validate signal size consistency
    // TRENDING generates signals using: notional = max_usd_per_order * leverage
    // ORDERING validates: notional <= max_position_notional_usd
    // These must be consistent to avoid signals being systematically rejected
    let leverage = cfg.leverage.unwrap_or(cfg.exec.default_leverage);
    let expected_notional = cfg.max_usd_per_order * leverage as f64;
    if expected_notional > cfg.risk.max_position_notional_usd {
        return Err(anyhow!(
            "Config mismatch: TRENDING will generate signals with notional {} (max_usd_per_order {} * leverage {}) but ORDERING limit is {}. Signals will be systematically rejected. Please ensure max_usd_per_order * leverage <= risk.max_position_notional_usd",
            expected_notional,
            cfg.max_usd_per_order,
            leverage,
            cfg.risk.max_position_notional_usd
        ));
    }

    Ok(())
}
