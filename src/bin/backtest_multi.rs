// ✅ ADIM 5: Backtest script - 100 coin için seri çalıştırma (TrendPlan.md)

use anyhow::Result;
use chrono::Utc;
use csv::Writer;
use dotenvy::dotenv;
use std::fs::OpenOptions;
use std::path::Path;
use trading_bot::{
    run_backtest,
    symbol_scanner::{SymbolScanner, SymbolSelectionConfig},
    types::FileConfig,
    AlgoConfig,
};

#[derive(Debug, serde::Serialize)]
struct BacktestRow {
    symbol: String,
    interval: String,
    total_trades: usize,
    win_trades: usize,
    loss_trades: usize,
    win_rate: f64,
    total_pnl_pct: f64,
    avg_pnl_pct: f64,
    avg_r: f64,
    total_signals: usize,
    long_signals: usize,
    short_signals: usize,
    timestamp: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();

    // Config dosyasından oku
    let file_cfg = FileConfig::load("config.yaml").unwrap_or_default();

    // Symbol scanner config
    let allowed_quotes = vec!["USDT".to_string(), "USDC".to_string()];
    let scanner_config = SymbolSelectionConfig::from_file_config(&file_cfg, allowed_quotes);

    // Environment variable'lardan veya default değerlerden al
    let interval = std::env::var("INTERVAL").unwrap_or_else(|_| "5m".to_string());
    let period = std::env::var("PERIOD").unwrap_or_else(|_| "5m".to_string());
    let limit: u32 = std::env::var("LIMIT")
        .unwrap_or_else(|_| "288".to_string())
        .parse()
        .unwrap_or(288); // 288 * 5m = son 24 saat

    let max_symbols: usize = std::env::var("MAX_SYMBOLS")
        .unwrap_or_else(|_| "100".to_string())
        .parse()
        .unwrap_or(100);

    let output_file =
        std::env::var("OUTPUT_FILE").unwrap_or_else(|_| "backtest_results_multi.csv".to_string());

    println!("===== MULTI-SYMBOL BACKTEST BAŞLIYOR =====");
    println!("Interval    : {}", interval);
    println!("Period      : {}", period);
    println!(
        "Limit       : {} (son {} saat @{})",
        limit,
        limit as f64 * 5.0 / 60.0,
        interval
    );
    println!("Max symbols : {}", max_symbols);
    println!("Output file : {}", output_file);
    println!(
        "Başlangıç   : {}",
        Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
    );
    println!();

    // Symbol scanner oluştur ve symbol'leri keşfet
    let scanner = SymbolScanner::new(scanner_config);
    let all_symbols = scanner.discover_symbols().await?;

    println!(
        "Discovered {} symbols, selecting top {}...",
        all_symbols.len(),
        max_symbols
    );

    // Top N symbol'ü seç (scoring yaparak)
    let metrics = scanner.fetch_ticker_24hr(&all_symbols).await?;
    let scores = scanner.score_symbols(&metrics);
    let selected_symbols: Vec<String> = scores
        .into_iter()
        .take(max_symbols)
        .map(|s| s.symbol)
        .collect();

    println!("Selected {} symbols for backtest", selected_symbols.len());
    println!();

    // AlgoConfig (backtest için)
    let cfg = AlgoConfig {
        rsi_trend_long_min: 55.0,
        rsi_trend_short_max: 45.0,
        funding_extreme_pos: 0.0005,
        funding_extreme_neg: -0.0005,
        lsr_crowded_long: 1.3,
        lsr_crowded_short: 0.8,
        long_min_score: 4,
        short_min_score: 4,
        fee_bps_round_trip: 8.0,
        max_holding_bars: 48,
        slippage_bps: 0.0,
        min_volume_ratio: 1.5,
        max_volatility_pct: 2.0,
        max_price_change_5bars_pct: 3.0,
        enable_signal_quality_filter: true,
        // Enhanced Signal Scoring (TrendPlan.md)
        enable_enhanced_scoring: false, // Enhanced scoring kapalı (default)
        enhanced_score_excellent: 80.0, // 80-100: Excellent signal
        enhanced_score_good: 65.0,      // 65-79: Good signal
        enhanced_score_marginal: 50.0,  // 50-64: Marginal signal
        atr_stop_loss_multiplier: 3.0,
        atr_take_profit_multiplier: 4.0,
        min_holding_bars: 3,
        // ✅ ADIM 2: Config parametreleri (default değerler)
        hft_mode: false,
        base_min_score: 6.5,
        trend_threshold_hft: 0.5,
        trend_threshold_normal: 0.6,
        weak_trend_score_multiplier: 1.15,
        regime_multiplier_trending: 0.9,
        regime_multiplier_ranging: 1.15,
    };

    // CSV writer oluştur
    let file_exists = Path::new(&output_file).exists();
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&output_file)?;

    let mut wtr = Writer::from_writer(file);

    // Header yaz (sadece yeni dosya ise)
    if !file_exists {
        wtr.write_record(&[
            "symbol",
            "interval",
            "total_trades",
            "win_trades",
            "loss_trades",
            "win_rate",
            "total_pnl_pct",
            "avg_pnl_pct",
            "avg_r",
            "total_signals",
            "long_signals",
            "short_signals",
            "timestamp",
        ])?;
    }

    let mut success_count = 0;
    let mut error_count = 0;
    let total_start = Utc::now();

    // Her symbol için backtest çalıştır
    for (idx, symbol) in selected_symbols.iter().enumerate() {
        println!(
            "[{}/{}] Running backtest for {}...",
            idx + 1,
            selected_symbols.len(),
            symbol
        );

        match run_backtest(symbol, &interval, &period, limit, &cfg).await {
            Ok(result) => {
                success_count += 1;

                let row = BacktestRow {
                    symbol: symbol.clone(),
                    interval: interval.clone(),
                    total_trades: result.total_trades,
                    win_trades: result.win_trades,
                    loss_trades: result.loss_trades,
                    win_rate: result.win_rate,
                    total_pnl_pct: result.total_pnl_pct,
                    avg_pnl_pct: result.avg_pnl_pct,
                    avg_r: result.avg_r,
                    total_signals: result.total_signals,
                    long_signals: result.long_signals,
                    short_signals: result.short_signals,
                    timestamp: Utc::now().to_rfc3339(),
                };

                wtr.serialize(&row)?;
                wtr.flush()?;

                println!(
                    "  ✅ {}: {} trades, {:.2}% win rate, {:.4}% total PnL",
                    symbol,
                    result.total_trades,
                    result.win_rate * 100.0,
                    result.total_pnl_pct * 100.0
                );
            }
            Err(err) => {
                error_count += 1;
                eprintln!("  ❌ {}: Backtest failed: {}", symbol, err);
            }
        }

        // Rate limiting: küçük delay
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    }

    let total_duration = Utc::now() - total_start;

    println!();
    println!("===== MULTI-SYMBOL BACKTEST TAMAMLANDI =====");
    println!("Success      : {}", success_count);
    println!("Errors       : {}", error_count);
    println!("Total time   : {:?}", total_duration);
    println!("Output file  : {}", output_file);
    println!(
        "Bitiş        : {}",
        Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
    );

    Ok(())
}
