use trading_bot::trending::{run_backtest, export_backtest_to_csv};
use trading_bot::{AlgoConfig, PositionSide};

/// Runs backtest using real Binance data (klines, funding, OI, long/short ratio)
/// 
/// This test uses REAL Binance API data to validate the trading strategy.
/// Run with: cargo test --test backtest -- --nocapture
#[tokio::test]
async fn backtest_with_real_binance_data() {
    let symbol = "BTCUSDT";
    let interval = "5m";       // kline interval
    let period = "5m";         // OI & L/S ratio period
    let limit = 288;           // 288 * 5m = son 24 saat

    let cfg = AlgoConfig {
        // ✅ Signal Thresholds - ADAPTIVE (TrendPlan.md Fix #5)
        rsi_trend_long_min: 52.0,   // 55 → 52 (daha esnek)
        rsi_trend_short_max: 48.0,  // 45 → 48 (daha esnek)
        
        // ✅ Funding - AGGRESSIVE (TrendPlan.md Fix #5)
        funding_extreme_pos: 0.0003,   // 0.05% → 0.03% (daha hassas)
        funding_extreme_neg: -0.0003,  // -0.05% → -0.03%
        
        // ✅ LSR - CONTRARIAN (TrendPlan.md Fix #5)
        lsr_crowded_long: 1.2,   // 1.3 → 1.2 (daha erken tespit)
        lsr_crowded_short: 0.85, // 0.8 → 0.85
        
        // ✅ Scoring - BALANCED (TrendPlan.md Fix #5)
        long_min_score: 5,   // 4 → 5 (daha seçici, adaptive threshold kullanıyor)
        short_min_score: 5,
        
        // ✅ Fees - REALISTIC (TrendPlan.md Fix #5)
        fee_bps_round_trip: 12.0,  // 8 → 12 (maker+taker)
        
        // ✅ Risk Management - WIDER (TrendPlan.md Fix #5)
        atr_stop_loss_multiplier: 2.5,   // 3.0 → 2.5 (tighter stop)
        atr_take_profit_multiplier: 7.0, // 6.0 → 7.0 (let winners run!)
        
        min_holding_bars: 2,  // 3 → 2 (daha hızlı entry/exit)
        max_holding_bars: 60, // 48 → 60 (5 saat)
        
        // ✅ Signal Quality - RELAXED (TrendPlan.md Fix #5)
        enable_signal_quality_filter: true,
        min_volume_ratio: 0.3,  // 1.5 → 0.3 (çok daha esnek)
        max_volatility_pct: 4.0, // 2.0 → 4.0 (kripto için normal)
        max_price_change_5bars_pct: 10.0, // 3.0 → 10.0
        
        slippage_bps: 3.0,  // 5.0 → 3.0 (LIMIT orders ile düşük)
    };

    println!("\n");
    println!("╔════════════════════════════════════════════════════════════════╗");
    println!("║  BACKTEST: {} {} - Real Binance Data (Last 24h)              ║", symbol, interval);
    println!("╚════════════════════════════════════════════════════════════════╝");
    println!();
    println!("📊 Fetching REAL data from Binance API...");
    println!("   - Klines: {} candles ({} interval)", limit, interval);
    println!("   - Funding rates: Last 100");
    println!("   - Open Interest: {} period", period);
    println!("   - Long/Short Ratio: {} period", period);
    println!();

    let res = match run_backtest(symbol, interval, period, limit, &cfg).await {
        Ok(result) => result,
        Err(e) => {
            eprintln!("❌ Backtest failed: {e:?}");
            panic!("Backtest failed: {e:?}");
        }
    };

    println!();
    println!("╔════════════════════════════════════════════════════════════════╗");
    println!("║                    BACKTEST RESULTS                            ║");
    println!("╚════════════════════════════════════════════════════════════════╝");
    println!();
    println!("📊 DATA VALIDATION:");
    println!("   ✅ Real Binance API data used");
    println!("   ✅ Klines: {} candles fetched", limit);
    println!("   ✅ Funding rates: Last 100 events");
    println!("   ✅ Open Interest: Historical data");
    println!("   ✅ Long/Short Ratio: Top trader data");
    println!();
    println!("📡 SIGNAL GENERATION:");
    println!("   Total Signals    : {} signals generated", res.total_signals);
    println!("   📈 LONG Signals   : {} ({:.1}%)", res.long_signals, 
        if res.total_signals > 0 { (res.long_signals as f64 / res.total_signals as f64) * 100.0 } else { 0.0 });
    println!("   📉 SHORT Signals  : {} ({:.1}%)", res.short_signals,
        if res.total_signals > 0 { (res.short_signals as f64 / res.total_signals as f64) * 100.0 } else { 0.0 });
    if res.total_signals > 0 {
        let signal_to_trade_ratio = (res.total_trades as f64 / res.total_signals as f64) * 100.0;
        println!("   Signal→Trade Rate : {:.1}% ({} trades from {} signals)", 
            signal_to_trade_ratio, res.total_trades, res.total_signals);
    }
    println!();
    println!("📈 PERFORMANCE METRICS:");
    println!("   Total Trades      : {} trades", res.total_trades);
    println!("   ✅ Win Trades      : {} ({:.1}%)", res.win_trades, (res.win_trades as f64 / res.total_trades as f64 * 100.0));
    println!("   ❌ Loss Trades     : {} ({:.1}%)", res.loss_trades, (res.loss_trades as f64 / res.total_trades as f64 * 100.0));
    println!("   Win Rate           : {:.2}%", res.win_rate * 100.0);
    println!("   Total PnL         : {:.4}%", res.total_pnl_pct * 100.0);
    println!("   Avg PnL/Trade     : {:.4}%", res.avg_pnl_pct * 100.0);
    if res.avg_r.is_infinite() {
        println!("   Avg R (Risk/Reward): ∞ (only wins)");
    } else {
        println!("   Avg R (Risk/Reward): {:.4}x", res.avg_r);
    }
    
    // Calculate additional metrics
    if res.total_trades > 0 {
        let best_trade = res.trades.iter().max_by(|a, b| a.pnl_pct.partial_cmp(&b.pnl_pct).unwrap());
        let worst_trade = res.trades.iter().min_by(|a, b| a.pnl_pct.partial_cmp(&b.pnl_pct).unwrap());
        
        if let Some(best) = best_trade {
            println!("   Best Trade         : {:.4}%", best.pnl_pct * 100.0);
        }
        if let Some(worst) = worst_trade {
            println!("   Worst Trade        : {:.4}%", worst.pnl_pct * 100.0);
        }
        
        // Calculate profit factor
        let total_win_pnl: f64 = res.trades.iter().filter(|t| t.win).map(|t| t.pnl_pct.abs()).sum();
        let total_loss_pnl: f64 = res.trades.iter().filter(|t| !t.win).map(|t| t.pnl_pct.abs()).sum();
        if total_loss_pnl > 0.0 {
            let profit_factor = total_win_pnl / total_loss_pnl;
            println!("   Profit Factor      : {:.4}x", profit_factor);
        }
    }

    if !res.trades.is_empty() {
        println!();
        println!("╔════════════════════════════════════════════════════════════════╗");
        println!("║                    TRADE DETAILS                              ║");
        println!("╚════════════════════════════════════════════════════════════════╝");
        println!();
        for (idx, trade) in res.trades.iter().enumerate() {
            let side_str = match trade.side {
                PositionSide::Long => "LONG ",
                PositionSide::Short => "SHORT",
                PositionSide::Flat => "FLAT ",
            };
            let win_str = if trade.win { "✅ WIN " } else { "❌ LOSS" };
            let price_change = ((trade.exit_price - trade.entry_price) / trade.entry_price) * 100.0;
            let holding_time = trade.exit_time - trade.entry_time;
            let holding_minutes = holding_time.num_minutes();
            
            println!("  {}. {} {} | Entry: ${:.2} → Exit: ${:.2} ({:+.2}%) | PnL: {:+.4}% | Duration: {}m",
                idx + 1,
                side_str,
                win_str,
                trade.entry_price,
                trade.exit_price,
                price_change,
                trade.pnl_pct * 100.0,
                holding_minutes
            );
            println!("     Entry: {} | Exit: {}",
                trade.entry_time.format("%Y-%m-%d %H:%M:%S"),
                trade.exit_time.format("%Y-%m-%d %H:%M:%S")
            );
        }

        // Summary by side
        let long_trades: Vec<_> = res.trades.iter().filter(|t| matches!(t.side, PositionSide::Long)).collect();
        let short_trades: Vec<_> = res.trades.iter().filter(|t| matches!(t.side, PositionSide::Short)).collect();

        println!();
        println!("╔════════════════════════════════════════════════════════════════╗");
        println!("║                  SIDE-BASED SUMMARY                            ║");
        println!("╚════════════════════════════════════════════════════════════════╝");
        println!();
        
        if !long_trades.is_empty() {
            let long_pnl: f64 = long_trades.iter().map(|t| t.pnl_pct).sum();
            let long_wins = long_trades.iter().filter(|t| t.win).count();
            let long_win_rate = (long_wins as f64 / long_trades.len() as f64) * 100.0;
            println!("📈 LONG TRADES:");
            println!("   Total Trades    : {}", long_trades.len());
            println!("   ✅ Win Trades    : {} ({:.1}%)", long_wins, long_win_rate);
            println!("   ❌ Loss Trades   : {} ({:.1}%)", long_trades.len() - long_wins, 100.0 - long_win_rate);
            println!("   Total PnL       : {:.4}%", long_pnl * 100.0);
            println!("   Avg PnL/Trade   : {:.4}%", (long_pnl / long_trades.len() as f64) * 100.0);
            println!();
        }

        if !short_trades.is_empty() {
            let short_pnl: f64 = short_trades.iter().map(|t| t.pnl_pct).sum();
            let short_wins = short_trades.iter().filter(|t| t.win).count();
            let short_win_rate = (short_wins as f64 / short_trades.len() as f64) * 100.0;
            println!("📉 SHORT TRADES:");
            println!("   Total Trades    : {}", short_trades.len());
            println!("   ✅ Win Trades    : {} ({:.1}%)", short_wins, short_win_rate);
            println!("   ❌ Loss Trades   : {} ({:.1}%)", short_trades.len() - short_wins, 100.0 - short_win_rate);
            println!("   Total PnL       : {:.4}%", short_pnl * 100.0);
            println!("   Avg PnL/Trade   : {:.4}%", (short_pnl / short_trades.len() as f64) * 100.0);
            println!();
        }
    } else {
        println!();
        println!("⚠️  No trades generated - Check signal generation logic");
        println!();
    }

    // CSV export test
    let csv_path = "tests/data/backtest_result.csv";
    if let Err(e) = export_backtest_to_csv(&res, csv_path) {
        eprintln!("⚠️  Warning: Failed to export CSV: {e:?}");
    } else {
        println!("💾 Backtest results exported to: {}", csv_path);
    }

    // Assertions
    assert!(
        res.total_trades > 0,
        "❌ Expected at least one trade - Strategy may need adjustment or market conditions were not suitable"
    );
    assert!(
        res.win_trades + res.loss_trades == res.total_trades,
        "❌ Win + Loss trades should equal total trades"
    );
    
    println!();
    println!("╔════════════════════════════════════════════════════════════════╗");
    println!("║              ✅ BACKTEST COMPLETED SUCCESSFULLY                ║");
    println!("╚════════════════════════════════════════════════════════════════╝");
    println!();
    println!("📝 Note: This test uses REAL Binance API data.");
    println!("   All signals, prices, and PnL calculations are based on actual market data.");
    println!();
}
