use trading_bot::trending::{export_backtest_to_csv, run_backtest};
use trading_bot::{create_test_config, BacktestFormatter};

/// Runs backtest using real Binance data (klines, funding, OI, long/short ratio)
///
/// This test uses REAL Binance API data to validate the trading strategy.
/// Run with: cargo test --test backtest -- --nocapture
#[tokio::test]
async fn backtest_with_real_binance_data() {
    let symbol = "BTCUSDT";
    let interval = "5m";
    let period = "5m";
    let limit = 288;

    let cfg = create_test_config();

    println!("\n");
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
    println!(
        "{}",
        BacktestFormatter::format_complete_report(&res, symbol, interval, limit, false)
    );

    let csv_path = "tests/data/backtest_result.csv";
    if let Err(e) = export_backtest_to_csv(&res, csv_path) {
        eprintln!("⚠️  Warning: Failed to export CSV: {e:?}");
    } else {
        println!("💾 Backtest results exported to: {}", csv_path);
    }

    assert!(
        res.total_trades > 0,
        "❌ Expected at least one trade - Strategy may need adjustment or market conditions were not suitable"
    );
    assert!(
        res.win_trades + res.loss_trades == res.total_trades,
        "❌ Win + Loss trades should equal total trades"
    );

    println!();
    println!("📝 Note: This test uses REAL Binance API data.");
    println!("   All signals, prices, and PnL calculations are based on actual market data.");
    println!();
}

#[tokio::test]
async fn backtest_with_conservative_config() {
    let symbol = "BTCUSDT";
    let interval = "5m";
    let period = "5m";
    let limit = 144;

    let cfg = trading_bot::create_conservative_config();

    println!("\n");
    println!("📊 Running backtest with CONSERVATIVE config...");
    println!();

    let res = match run_backtest(symbol, interval, period, limit, &cfg).await {
        Ok(result) => result,
        Err(e) => {
            eprintln!("❌ Backtest failed: {e:?}");
            return;
        }
    };

    println!();
    println!(
        "{}",
        BacktestFormatter::format_complete_report(&res, symbol, interval, limit, false)
    );
}

#[tokio::test]
async fn backtest_with_aggressive_config() {
    let symbol = "BTCUSDT";
    let interval = "5m";
    let period = "5m";
    let limit = 144;

    let cfg = trading_bot::create_aggressive_config();

    println!("\n");
    println!("📊 Running backtest with AGGRESSIVE config...");
    println!();

    let res = match run_backtest(symbol, interval, period, limit, &cfg).await {
        Ok(result) => result,
        Err(e) => {
            eprintln!("❌ Backtest failed: {e:?}");
            return;
        }
    };

    println!();
    println!(
        "{}",
        BacktestFormatter::format_complete_report(&res, symbol, interval, limit, false)
    );
}
