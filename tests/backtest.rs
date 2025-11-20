use trading_bot::trending::{run_backtest, export_backtest_to_csv};
use trading_bot::{AlgoConfig, PositionSide};

/// Runs backtest using real Binance data (klines, funding, OI, long/short ratio)
#[tokio::test]
async fn backtest_with_real_binance_data() {
    let symbol = "BTCUSDT";
    let interval = "5m";       // kline interval
    let period = "5m";         // OI & L/S ratio period
    let limit = 288;           // 288 * 5m = son 24 saat

    let cfg = AlgoConfig {
        rsi_trend_long_min: 55.0,
        rsi_trend_short_max: 45.0,
        funding_extreme_pos: 0.0005,  // 0.05% (extreme long funding, ~219% APR annualized)
        funding_extreme_neg: -0.0005, // -0.05% (extreme short funding, ~-219% APR annualized)
        lsr_crowded_long: 1.3,        // longShortRatio > 1.3 => crowded long
        lsr_crowded_short: 0.8,       // longShortRatio < 0.8 => crowded short
        long_min_score: 4,             // Minimum 4 score gerekli
        short_min_score: 4,            // Minimum 4 score gerekli
        fee_bps_round_trip: 8.0,      // giriş+çıkış toplam 0.08% varsayalım
        max_holding_bars: 48,          // max 48 bar (~4 saat @5m)
        slippage_bps: 0.0,            // No slippage simulation in test
    };

    println!("\n===== BACKTEST: {symbol} {interval}, last 24h =====");
    println!("Fetching real data from Binance...");

    let res = match run_backtest(symbol, interval, period, limit, &cfg).await {
        Ok(result) => result,
        Err(e) => {
            eprintln!("Backtest failed: {e:?}");
            panic!("Backtest failed: {e:?}");
        }
    };

    println!("\n===== BACKTEST RESULTS =====");
    println!("Total trades   : {}", res.total_trades);
    println!("Win trades     : {}", res.win_trades);
    println!("Loss trades    : {}", res.loss_trades);
    println!("Win rate       : {:.2}%", res.win_rate * 100.0);
    println!("Total PnL      : {:.4}%", res.total_pnl_pct * 100.0);
    println!("Avg PnL/trade  : {:.4}%", res.avg_pnl_pct * 100.0);
    if res.avg_r.is_infinite() {
        println!("Avg R (R/R)    : ∞ (only wins)");
    } else {
        println!("Avg R (R/R)    : {:.4}", res.avg_r);
    }

    if !res.trades.is_empty() {
        println!("\n===== TRADE DETAILS =====");
        for (idx, trade) in res.trades.iter().enumerate() {
            let side_str = match trade.side {
                PositionSide::Long => "LONG",
                PositionSide::Short => "SHORT",
                PositionSide::Flat => "FLAT",
            };
            let win_str = if trade.win { "✅ WIN" } else { "❌ LOSS" };
            println!(
                "  {}. {} | Entry: ${:.2} @ {} → Exit: ${:.2} @ {} | PnL: {:.4}% | {}",
                idx + 1,
                side_str,
                trade.entry_price,
                trade.entry_time.format("%Y-%m-%d %H:%M"),
                trade.exit_price,
                trade.exit_time.format("%Y-%m-%d %H:%M"),
                trade.pnl_pct * 100.0,
                win_str
            );
        }

        // Summary by side
        let long_trades: Vec<_> = res.trades.iter().filter(|t| matches!(t.side, PositionSide::Long)).collect();
        let short_trades: Vec<_> = res.trades.iter().filter(|t| matches!(t.side, PositionSide::Short)).collect();

        if !long_trades.is_empty() {
            let long_pnl: f64 = long_trades.iter().map(|t| t.pnl_pct).sum();
            let long_wins = long_trades.iter().filter(|t| t.win).count();
            println!("\n===== LONG TRADES SUMMARY =====");
            println!("  Total: {} | PnL: {:.4}% | Win Rate: {:.1}% ({}/{})",
                long_trades.len(), long_pnl * 100.0,
                (long_wins as f64 / long_trades.len() as f64) * 100.0,
                long_wins, long_trades.len());
        }

        if !short_trades.is_empty() {
            let short_pnl: f64 = short_trades.iter().map(|t| t.pnl_pct).sum();
            let short_wins = short_trades.iter().filter(|t| t.win).count();
            println!("\n===== SHORT TRADES SUMMARY =====");
            println!("  Total: {} | PnL: {:.4}% | Win Rate: {:.1}% ({}/{})",
                short_trades.len(), short_pnl * 100.0,
                (short_wins as f64 / short_trades.len() as f64) * 100.0,
                short_wins, short_trades.len());
        }
    }

    // CSV export test
    let csv_path = "tests/data/backtest_result.csv";
    if let Err(e) = export_backtest_to_csv(&res, csv_path) {
        eprintln!("Warning: Failed to export CSV: {e:?}");
    } else {
        println!("\n✅ Backtest results exported to: {}", csv_path);
    }

    // Assertions
    assert!(
        res.total_trades > 0,
        "Expected at least one trade"
    );
    assert!(
        res.win_trades + res.loss_trades == res.total_trades,
        "Win + Loss trades should equal total trades"
    );
    
    println!("\n✅ Backtest completed successfully!");
}
