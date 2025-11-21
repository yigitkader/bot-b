pub mod balance;
pub mod cache;
pub mod config;
pub mod connection;
pub mod event_bus;
pub mod follow_orders;
pub mod logging;
pub mod metrics_cache;
pub mod ordering;
pub mod risk_manager;
pub mod slippage;
pub mod state;
pub mod symbol_scanner;
pub mod test_utils;
pub mod trending;
pub mod types;

pub use cache::{DepthCache, SymbolInfoCache};

pub use config::BotConfig;
pub use metrics_cache::MetricsCache;
pub use risk_manager::{RiskLimits, RiskManager};
pub use slippage::{run_slippage_tracker, SlippageStats, SlippageTracker};
pub use symbol_scanner::{SymbolScanner, SymbolSelectionConfig};
pub use test_utils::{
    create_aggressive_config, create_conservative_config, create_test_config, AlgoConfigBuilder,
    BacktestFormatter, OptimizationResult, ParameterOptimizer,
};
pub use trending::{
    calculate_advanced_metrics, export_backtest_to_csv, generate_signals, print_advanced_report,
    run_backtest, run_trending,
};
pub use types::{
    AdvancedBacktestResult, AlgoConfig, BacktestResult, BalanceChannels, Connection,
    ConnectionChannels, EventBus, FollowChannels, FuturesClient, LoggingChannels, NewOrderRequest,
    OrderingChannels, PositionSide, SharedState, Signal, SignalSide, Trade, TrendParams,
    TrendingChannels,
};
