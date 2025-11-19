mod helpers;

use std::{env, fs};

use helpers::{record_ticks, run_backtest};
use trading_bot::{
    config::{BotConfig, TrendParams},
    types::{MarketTick, Side, TradeSignal},
};

#[derive(Debug, Clone)]
struct TradeResult {
    signal: TradeSignal,
    exit_price: f64,
    exit_reason: String,
    pnl_usdt: f64,
    pnl_percent: f64,
    duration_ticks: usize,
}

fn calculate_pnl(
    signals: &[TradeSignal],
    ticks: &[MarketTick],
    tp_percent: f64,
    sl_percent: f64,
) -> Vec<TradeResult> {
    let mut results = Vec::new();
    
    for signal in signals {
        // Find entry tick index
        let entry_idx = ticks
            .iter()
            .position(|t| t.ts >= signal.ts)
            .unwrap_or(ticks.len() - 1);
        
        let entry_price = signal.entry_price;
        let leverage = signal.leverage;
        let position_size = signal.size_usdt;
        
        // Calculate TP/SL prices
        let (tp_price, sl_price) = match signal.side {
            Side::Long => (
                entry_price * (1.0 + tp_percent / 100.0),
                entry_price * (1.0 - sl_percent / 100.0),
            ),
            Side::Short => (
                entry_price * (1.0 - tp_percent / 100.0),
                entry_price * (1.0 + sl_percent / 100.0),
            ),
        };
        
        // Find exit tick
        let mut exit_price = entry_price;
        let mut exit_reason = "END_OF_DATA".to_string();
        let mut duration_ticks = 0;
        
        for tick in ticks.iter().skip(entry_idx + 1) {
            duration_ticks += 1;
            
            let hit_tp = match signal.side {
                Side::Long => tick.price >= tp_price,
                Side::Short => tick.price <= tp_price,
            };
            
            let hit_sl = match signal.side {
                Side::Long => tick.price <= sl_price,
                Side::Short => tick.price >= sl_price,
            };
            
            if hit_tp {
                exit_price = tp_price;
                exit_reason = "TAKE_PROFIT".to_string();
                break;
            } else if hit_sl {
                exit_price = sl_price;
                exit_reason = "STOP_LOSS".to_string();
                break;
            }
        }
        
        // If no TP/SL hit, use last tick price
        if exit_reason == "END_OF_DATA" {
            exit_price = ticks.last().map(|t| t.price).unwrap_or(entry_price);
        }
        
        // Calculate PnL
        let pnl_percent = match signal.side {
            Side::Long => (exit_price - entry_price) / entry_price * leverage * 100.0,
            Side::Short => (entry_price - exit_price) / entry_price * leverage * 100.0,
        };
        
        let pnl_usdt = position_size * (pnl_percent / 100.0);
        
        results.push(TradeResult {
            signal: signal.clone(),
            exit_price,
            exit_reason,
            pnl_usdt,
            pnl_percent,
            duration_ticks,
        });
    }
    
    results
}

/// Runs TRENDING against a Binance tick capture.
/// If FORCE_RECORD env var is set, collects fresh data from Binance first.
#[tokio::test]
async fn backtest_replays_real_binance_ticks() {
    let data_file = "tests/data/binance_btcusdt_ticks.json";
    
    // Collect fresh data if FORCE_RECORD is set
    if env::var("FORCE_RECORD").is_ok() {
        let config = BotConfig::from_env();
        let samples: usize = env::var("RECORD_SAMPLES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60);
        
        println!("RECORDER: Collecting {} fresh samples from Binance...", samples);
        println!("RECORDER: Symbol: {}, Base URL: {}", config.symbol, config.base_url);
        
        match record_ticks(config, samples, data_file).await {
            Ok(_) => println!("RECORDER: Successfully collected {} samples", samples),
            Err(e) => {
                eprintln!("RECORDER: Failed to collect data: {e:?}");
                eprintln!("RECORDER: Using existing data file if available...");
            }
        }
    }
    
    let raw = fs::read_to_string(data_file)
        .expect("failed to read historical ticks");
    let ticks: Vec<MarketTick> =
        serde_json::from_str(&raw).expect("failed to parse historical tick data");

    let config = BotConfig::from_env();
    let params = TrendParams {
        ema_fast_period: 8,
        ema_slow_period: 21,
        rsi_period: 14,
        atr_period: 14,
        leverage: 5.0,
        position_size_quote: 75.0,
        rsi_long_min: 48.0,
        rsi_short_max: 52.0,
        atr_rising_factor: 1.001,
        obi_long_min: 1.05,
        obi_short_max: 0.9,
        funding_max_for_long: 0.0005,
        funding_min_for_short: -0.0005,
        long_min_score: 4,
        short_min_score: 4,
        signal_cooldown_secs: 2,
        warmup_min_ticks: 8,
    };

    println!("\n=== BACKTEST RESULTS ===");
    println!("Total ticks processed: {}", ticks.len());
    println!("Time range: {} to {}", ticks.first().map(|t| t.ts).unwrap_or_default(), ticks.last().map(|t| t.ts).unwrap_or_default());
    println!("TP: {:.2}% | SL: {:.2}% | Leverage: {}x | Position Size: {:.2} USDT", 
        config.tp_percent, config.sl_percent, params.leverage, params.position_size_quote);
    
    let signals = run_backtest(&ticks, params.clone());
    
    // Calculate PnL for each signal
    let trade_results = calculate_pnl(&signals, &ticks, config.tp_percent, config.sl_percent);
    
    let long_signals: Vec<_> = signals.iter().filter(|s| s.side == Side::Long).collect();
    let short_signals: Vec<_> = signals.iter().filter(|s| s.side == Side::Short).collect();
    
    println!("\n=== SIGNAL STATISTICS ===");
    println!("Total signals: {}", signals.len());
    println!("  Long signals: {} ({:.1}%)", long_signals.len(), (long_signals.len() as f64 / signals.len().max(1) as f64) * 100.0);
    println!("  Short signals: {} ({:.1}%)", short_signals.len(), (short_signals.len() as f64 / signals.len().max(1) as f64) * 100.0);
    println!("Signal rate: {:.2}% ({}/{} ticks)", (signals.len() as f64 / ticks.len() as f64) * 100.0, signals.len(), ticks.len());
    
    if !signals.is_empty() {
        println!("\n=== SIGNAL DETAILS ===");
        for (idx, signal) in signals.iter().enumerate() {
            println!(
                "  {}. {:?} @ ${:.2} | Size: {:.2} {} | Leverage: {}x | Time: {}",
                idx + 1,
                signal.side,
                signal.entry_price,
                signal.size_usdt,
                "USDT", // TODO: get from config
                signal.leverage,
                signal.ts.format("%H:%M:%S")
            );
        }
        
        let avg_entry_long = long_signals.iter().map(|s| s.entry_price).sum::<f64>() / long_signals.len().max(1) as f64;
        let avg_entry_short = short_signals.iter().map(|s| s.entry_price).sum::<f64>() / short_signals.len().max(1) as f64;
        
        println!("\n=== AVERAGE ENTRY PRICES ===");
        if !long_signals.is_empty() {
            println!("  Long avg entry: ${:.2}", avg_entry_long);
        }
        if !short_signals.is_empty() {
            println!("  Short avg entry: ${:.2}", avg_entry_short);
        }
    }
    
    // PnL Analysis
    if !trade_results.is_empty() {
        let total_pnl = trade_results.iter().map(|r| r.pnl_usdt).sum::<f64>();
        let winning_trades: Vec<_> = trade_results.iter().filter(|r| r.pnl_usdt > 0.0).collect();
        let losing_trades: Vec<_> = trade_results.iter().filter(|r| r.pnl_usdt < 0.0).collect();
        let breakeven_trades = trade_results.len() - winning_trades.len() - losing_trades.len();
        
        let win_rate = (winning_trades.len() as f64 / trade_results.len() as f64) * 100.0;
        let avg_win = if !winning_trades.is_empty() {
            winning_trades.iter().map(|r| r.pnl_usdt).sum::<f64>() / winning_trades.len() as f64
        } else {
            0.0
        };
        let avg_loss = if !losing_trades.is_empty() {
            losing_trades.iter().map(|r| r.pnl_usdt).sum::<f64>() / losing_trades.len() as f64
        } else {
            0.0
        };
        
        let profit_factor = if avg_loss.abs() > 0.0 {
            (avg_win * winning_trades.len() as f64) / (avg_loss.abs() * losing_trades.len() as f64)
        } else if !winning_trades.is_empty() {
            f64::INFINITY
        } else {
            0.0
        };
        
        let max_drawdown = trade_results
            .iter()
            .scan(0.0f64, |running_pnl, r| {
                *running_pnl += r.pnl_usdt;
                Some(*running_pnl)
            })
            .fold((0.0f64, 0.0f64), |(max, min), pnl| {
                (max.max(pnl), min.min(pnl))
            });
        
        println!("\n=== PnL ANALYSIS ===");
        println!("Total PnL: {:.2} USDT ({:.2}%)", total_pnl, (total_pnl / (params.position_size_quote * trade_results.len() as f64)) * 100.0);
        println!("Win Rate: {:.1}% ({} wins / {} losses / {} breakeven)", 
            win_rate, winning_trades.len(), losing_trades.len(), breakeven_trades);
        println!("Average Win: {:.2} USDT", avg_win);
        println!("Average Loss: {:.2} USDT", avg_loss);
        println!("Profit Factor: {:.2}", profit_factor);
        println!("Max Drawdown: {:.2} USDT (from peak {:.2})", max_drawdown.1, max_drawdown.0);
        
        println!("\n=== TRADE RESULTS ===");
        for (idx, result) in trade_results.iter().enumerate() {
            let pnl_sign = if result.pnl_usdt >= 0.0 { "+" } else { "" };
            println!(
                "  {}. {:?} Entry: ${:.2} → Exit: ${:.2} ({}) | PnL: {}{:.2} USDT ({}{:.2}%) | Duration: {} ticks",
                idx + 1,
                result.signal.side,
                result.signal.entry_price,
                result.exit_price,
                result.exit_reason,
                pnl_sign,
                result.pnl_usdt,
                pnl_sign,
                result.pnl_percent,
                result.duration_ticks
            );
        }
        
        // Summary by side
        let long_results: Vec<_> = trade_results.iter().filter(|r| r.signal.side == Side::Long).collect();
        let short_results: Vec<_> = trade_results.iter().filter(|r| r.signal.side == Side::Short).collect();
        
        if !long_results.is_empty() {
            let long_pnl: f64 = long_results.iter().map(|r| r.pnl_usdt).sum();
            let long_wins = long_results.iter().filter(|r| r.pnl_usdt > 0.0).count();
            println!("\n=== LONG TRADES SUMMARY ===");
            println!("  Total: {} trades | PnL: {:.2} USDT | Win Rate: {:.1}% ({}/{})",
                long_results.len(), long_pnl, 
                (long_wins as f64 / long_results.len() as f64) * 100.0,
                long_wins, long_results.len());
        }
        
        if !short_results.is_empty() {
            let short_pnl: f64 = short_results.iter().map(|r| r.pnl_usdt).sum();
            let short_wins = short_results.iter().filter(|r| r.pnl_usdt > 0.0).count();
            println!("\n=== SHORT TRADES SUMMARY ===");
            println!("  Total: {} trades | PnL: {:.2} USDT | Win Rate: {:.1}% ({}/{})",
                short_results.len(), short_pnl,
                (short_wins as f64 / short_results.len() as f64) * 100.0,
                short_wins, short_results.len());
        }
        
        // Final verdict
        println!("\n=== FINAL VERDICT ===");
        if total_pnl > 0.0 {
            println!("✅ PROFITABLE: +{:.2} USDT total profit", total_pnl);
        } else if total_pnl < 0.0 {
            println!("❌ LOSS: {:.2} USDT total loss", total_pnl);
        } else {
            println!("⚪ BREAKEVEN: 0.00 USDT");
        }
    }
    
    println!("\n=== TEST ASSERTIONS ===");
    assert!(
        signals.iter().any(|s| s.side == Side::Long),
        "expected at least one long signal"
    );
    assert!(
        signals.iter().any(|s| s.side == Side::Short),
        "expected at least one short signal"
    );
    assert!(
        signals.len() <= ticks.len(),
        "produced more signals than ticks"
    );
    
    println!("✅ All assertions passed!");
}
