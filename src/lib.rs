pub mod balance;
pub mod config;
pub mod connection;
pub mod event_bus;
pub mod follow_orders;
pub mod logging;
pub mod ordering;
pub mod state;
pub mod trending;
pub mod types;

pub use config::BotConfig;
pub use trending::{
    calculate_advanced_metrics, export_backtest_to_csv, generate_signals, print_advanced_report,
    run_backtest, run_trending,
};
pub use types::{
    AdvancedBacktestResult, AlgoConfig, BacktestResult, BalanceChannels, Connection,
    ConnectionChannels, EventBus, FollowChannels, FuturesClient, LoggingChannels, NewOrderRequest,
    OrderingChannels, PositionSide, SharedState, Signal, SignalSide, TrendParams, TrendingChannels,
    Trade,
};
