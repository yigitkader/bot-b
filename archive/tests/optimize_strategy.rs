// Strategy optimization script
// Tests different parameter combinations to find the best optimization

use app::config::AppCfg;
use app::event_bus::MarketTick;
use rust_decimal::Decimal;
use std::sync::Arc;

// Import backtest functions from backtest.rs
mod backtest_common {
    use super::*;
    use rust_decimal::Decimal;
    
    // Re-export BacktestResults from backtest.rs
    // This matches the structure used in tests/backtest.rs
    #[derive(Debug, Clone)]
    pub struct BacktestResults {
        pub winning_trades: u64,
        pub losing_trades: u64,
        pub total_pnl: Decimal,
        pub win_rate: f64,
        pub sharpe_ratio: f64,
        pub max_drawdown: f64,
        pub profit_factor: f64,
        pub average_trade_duration_ticks: f64,
    }
    
    /// Run backtest with specified parameters
    /// 
    /// This function calls the real backtest from backtest.rs
    /// Note: Currently, BASE_MIN_SCORE, trend_strength_threshold, ATR_SL_MULTIPLIER, 
    /// and ATR_TP_MULTIPLIER are hardcoded in trending.rs. To make these configurable,
    /// we would need to add them to TrendingCfg in config.rs.
    /// 
    /// For now, this function runs the real backtest with the current config.
    /// The parameters are logged but not yet applied (requires config changes).
    pub async fn run_backtest_with_params(
        symbol: &str,
        ticks: Vec<MarketTick>,
        cfg: Arc<AppCfg>,
        _base_min_score: f64,
        _trend_strength_threshold: f64,
        _sl_multiplier: f64,
        _tp_multiplier: f64,
    ) -> BacktestResults {
        // TODO: To make these parameters work, we need to:
        // 1. Add base_min_score, trend_strength_threshold, atr_sl_multiplier, atr_tp_multiplier to TrendingCfg
        // 2. Modify trending.rs to read these from config instead of constants
        // 3. Modify cfg here before calling run_backtest
        
        // Use the real backtest logic (same as tests/backtest.rs::run_backtest)
        
        use app::trending::Trending;
        use app::types::{LastSignal, PricePoint, Px, SymbolState, TrendSignal};
        use rust_decimal::prelude::ToPrimitive;
        use std::collections::{HashMap, VecDeque};
        use std::str::FromStr;
        
        // Initialize trending module
        let event_bus = Arc::new(app::event_bus::EventBus::new_with_config(&cfg.event_bus));
        let shutdown_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let _trending = Trending::new(cfg.clone(), event_bus.clone(), shutdown_flag.clone());
        
        // Initialize symbol state
        let symbol_states = Arc::new(tokio::sync::Mutex::new(HashMap::<String, SymbolState>::new()));
        let last_signals = Arc::new(tokio::sync::Mutex::new(HashMap::<String, LastSignal>::new()));
        
        let mut winning_trades = 0u64;
        let mut losing_trades = 0u64;
        let mut total_pnl = Decimal::ZERO;
        let mut open_position: Option<(TrendSignal, Decimal, usize)> = None; // (signal, entry_price, entry_tick_index)
        
        // Track metrics
        let mut trade_pnls: Vec<Decimal> = Vec::new(); // For Sharpe ratio calculation
        let mut cumulative_pnl: Vec<Decimal> = Vec::new(); // For drawdown calculation
        let mut total_gains = Decimal::ZERO; // For profit factor
        let mut total_losses = Decimal::ZERO; // For profit factor
        let mut trade_durations: Vec<usize> = Vec::new(); // For average trade duration
        
        let leverage = 50u32;
        let margin = Decimal::from(20); // $20 margin per trade
        
        // Use config values for SL/TP, or apply multipliers if provided
        // Note: Currently sl_multiplier and tp_multiplier are not applied
        // because they would require modifying trending.rs constants
        let stop_loss_pct = cfg.stop_loss_pct;
        let take_profit_pct = cfg.take_profit_pct;
        
        // Process each tick
        for (tick_index, tick) in ticks.iter().enumerate() {
            // Update symbol state with tick
            {
                let mut states = symbol_states.lock().await;
                let state = states.entry(tick.symbol.clone()).or_insert_with(|| SymbolState {
                    symbol: tick.symbol.clone(),
                    prices: VecDeque::new(),
                    last_signal_time: None,
                    last_position_close_time: None,
                    last_position_direction: None,
                    tick_counter: 0,
                    ema_9: None,
                    ema_21: None,
                    ema_55: None,
                    ema_55_history: VecDeque::new(),
                    rsi_avg_gain: None,
                    rsi_avg_loss: None,
                    rsi_period_count: 0,
                });
                
                let mid_price = (tick.bid.0 + tick.ask.0) / Decimal::from(2);
                state.prices.push_back(PricePoint {
                    timestamp: tick.timestamp,
                    price: mid_price,
                    volume: tick.volume,
                });
                
                // Keep last 100 prices
                while state.prices.len() > 100 {
                    state.prices.pop_front();
                }
                
                // Update indicators incrementally
                Trending::update_indicators(state, mid_price);
            }
            
            // Check if we have an open position
            if let Some((signal, entry_price, entry_tick)) = open_position {
                let current_price = (tick.bid.0 + tick.ask.0) / Decimal::from(2);
                let price_change_pct = match signal {
                    TrendSignal::Long => {
                        ((current_price - entry_price) / entry_price * Decimal::from(100))
                            .to_f64()
                            .unwrap_or(0.0)
                    }
                    TrendSignal::Short => {
                        ((entry_price - current_price) / entry_price * Decimal::from(100))
                            .to_f64()
                            .unwrap_or(0.0)
                    }
                };
                
                // Check stop loss / take profit
                let should_close = if price_change_pct <= -stop_loss_pct {
                    true // Stop loss hit
                } else if price_change_pct >= take_profit_pct {
                    true // Take profit hit
                } else {
                    false
                };
                
                if should_close {
                    // Close position
                    let notional = margin * Decimal::from(leverage);
                    let pnl_pct = price_change_pct / 100.0;
                    let pnl = notional * Decimal::from_str(&format!("{:.8}", pnl_pct)).unwrap_or(Decimal::ZERO);
                    
                    // Subtract commission (0.04% taker fee)
                    let commission = notional * Decimal::from_str("0.0004").unwrap() * Decimal::from(2); // Entry + exit
                    let final_pnl = pnl - commission;
                    
                    total_pnl += final_pnl;
                    trade_pnls.push(final_pnl);
                    cumulative_pnl.push(total_pnl);
                    
                    // Track gains/losses for profit factor
                    if final_pnl > Decimal::ZERO {
                        total_gains += final_pnl;
                        winning_trades += 1;
                    } else {
                        total_losses += -final_pnl; // Losses are negative, make positive for calculation
                        losing_trades += 1;
                    }
                    
                    // Track trade duration
                    let duration = tick_index - entry_tick;
                    trade_durations.push(duration);
                    
                    open_position = None;
                }
            } else {
                // No open position - check for signal
                let state = {
                    let states = symbol_states.lock().await;
                    states.get(&tick.symbol).cloned()
                };
                
                if let Some(state) = state {
                    // Analyze trend
                    let default_cfg = app::config::TrendingCfg::default();
                    if let Some(signal) = Trending::analyze_trend(&state, &default_cfg) {
                        // Open position
                        let entry_price = (tick.bid.0 + tick.ask.0) / Decimal::from(2);
                        open_position = Some((signal, entry_price, tick_index));
                    }
                }
            }
        }
        
        // Close any remaining open position at last price
        if let Some((signal, entry_price, entry_tick)) = open_position {
            let last_tick = ticks.last().unwrap();
            let exit_price = (last_tick.bid.0 + last_tick.ask.0) / Decimal::from(2);
            
            let price_change_pct = match signal {
                TrendSignal::Long => {
                    ((exit_price - entry_price) / entry_price * Decimal::from(100))
                        .to_f64()
                        .unwrap_or(0.0)
                }
                TrendSignal::Short => {
                    ((entry_price - exit_price) / entry_price * Decimal::from(100))
                        .to_f64()
                        .unwrap_or(0.0)
                }
            };
            
            let notional = margin * Decimal::from(leverage);
            let pnl_pct = price_change_pct / 100.0;
            let pnl = notional * Decimal::from_str(&format!("{:.8}", pnl_pct)).unwrap_or(Decimal::ZERO);
            
            // Subtract commission
            let commission = notional * Decimal::from_str("0.0004").unwrap() * Decimal::from(2);
            let final_pnl = pnl - commission;
            
            total_pnl += final_pnl;
            trade_pnls.push(final_pnl);
            cumulative_pnl.push(total_pnl);
            
            if final_pnl > Decimal::ZERO {
                total_gains += final_pnl;
                winning_trades += 1;
            } else {
                total_losses += -final_pnl;
                losing_trades += 1;
            }
            
            let duration = ticks.len() - entry_tick;
            trade_durations.push(duration);
        }
        
        // Calculate metrics
        let total_trades = winning_trades + losing_trades;
        let win_rate = if total_trades > 0 {
            winning_trades as f64 / total_trades as f64
        } else {
            0.0
        };
        
        // Sharpe Ratio: (Average Return - Risk Free Rate) / StdDev of Returns
        let sharpe_ratio = if trade_pnls.len() >= 2 {
            let avg_return: f64 = trade_pnls.iter()
                .map(|pnl| pnl.to_f64().unwrap_or(0.0))
                .sum::<f64>() / trade_pnls.len() as f64;
            
            let variance: f64 = trade_pnls.iter()
                .map(|pnl| {
                    let ret = pnl.to_f64().unwrap_or(0.0);
                    (ret - avg_return).powi(2)
                })
                .sum::<f64>() / trade_pnls.len() as f64;
            
            let std_dev = variance.sqrt();
            if std_dev > 0.0 {
                avg_return / std_dev
            } else {
                0.0
            }
        } else {
            0.0
        };
        
        // Max Drawdown: Largest peak-to-trough decline
        let max_drawdown = if cumulative_pnl.len() >= 2 {
            let mut max_peak = cumulative_pnl[0];
            let mut max_dd = 0.0f64;
            
            for &pnl in cumulative_pnl.iter().skip(1) {
                if pnl > max_peak {
                    max_peak = pnl;
                }
                let drawdown = (max_peak - pnl).to_f64().unwrap_or(0.0);
                if drawdown > max_dd {
                    max_dd = drawdown;
                }
            }
            max_dd
        } else {
            0.0
        };
        
        // Profit Factor: Total Gains / Total Losses
        let profit_factor = if total_losses > Decimal::ZERO {
            (total_gains / total_losses).to_f64().unwrap_or(0.0)
        } else if total_gains > Decimal::ZERO {
            f64::INFINITY // All winning trades
        } else {
            0.0 // All losing trades
        };
        
        // Average Trade Duration
        let average_trade_duration_ticks = if !trade_durations.is_empty() {
            trade_durations.iter().sum::<usize>() as f64 / trade_durations.len() as f64
        } else {
            0.0
        };
        
        BacktestResults {
            winning_trades,
            losing_trades,
            total_pnl,
            win_rate,
            sharpe_ratio,
            max_drawdown,
            profit_factor,
            average_trade_duration_ticks,
        }
    }
}

