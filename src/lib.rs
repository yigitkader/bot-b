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
pub use trending::{generate_signals, run_backtest, run_trending};
pub use types::{
    AlgoConfig, BacktestResult, BalanceChannels, Connection, ConnectionChannels, EventBus,
    FollowChannels, FuturesClient, LoggingChannels, NewOrderRequest, OrderingChannels,
    PositionSide, SharedState, Signal, SignalSide, TrendParams, TrendingChannels,
};
