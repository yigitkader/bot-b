pub mod strategies;
pub mod core;
pub mod multi_timeframe;

pub use strategies::{
    FundingArbitrage, FundingArbitrageSignal, FundingExhaustionSignal, PostFundingSignal,
    OrderFlowAnalyzer, AbsorptionSignal, SpoofingSignal, IcebergSignal,
    LiquidationMap, LiquidationWall, CascadeDirection, CascadeSignal,
    VolumeProfile, build_liquidation_map_from_force_orders,
};

pub use multi_timeframe::{
    Timeframe, TimeframeSignal, MultiTimeframeAnalysis, DivergenceType, AlignedSignal,
};

pub use core::{
    build_signal_contexts, create_mtf_analysis, classify_trend, generate_signal_enhanced,
    generate_signals, run_backtest_on_series, export_backtest_to_csv, calculate_advanced_metrics,
    print_advanced_report, LastSignalState, SymbolHandler, calculate_enhanced_signal_score,
    build_enhanced_signal_context, run_trending, run_backtest, run_combined_kline_stream,
};

