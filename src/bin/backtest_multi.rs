// ✅ ADIM 5: Backtest script - 100 coin için seri çalıştırma (TrendPlan.md)
// ✅ ENHANCED: Individual reports + Top 10 optimization
// 
// ⚠️ CRITICAL WARNING: Top 10 Coin Optimization has HIGH overfitting risk
// - Coins selected based on PAST performance (historical backtest)
// - Optimized config may not work in future
// - Always use Walk-Forward Analysis and Out-of-Sample Testing
// - Past performance does NOT guarantee future results

use anyhow::Result;
use chrono::Utc;
use csv::Writer;
use dotenvy::dotenv;
use futures::stream::{self, StreamExt};
use serde_json;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;
use trading_bot::{
    calculate_advanced_metrics, export_backtest_to_csv, run_backtest,
    symbol_scanner::{SymbolScanner, SymbolSelectionConfig},
    test_utils::AlgoConfigBuilder,
    types::FileConfig,
    BacktestResult,
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
    // ✅ NEW: Advanced metrics
    max_drawdown_pct: f64,
    sharpe_ratio: f64,
    profit_factor: f64,
    timestamp: String,
}

#[derive(Debug, serde::Serialize)]
struct CoinReport {
    symbol: String,
    basic_metrics: BasicMetrics,
    advanced_metrics: AdvancedMetrics,
    trades_file: String,
    timestamp: String,
}

#[derive(Debug, serde::Serialize)]
struct BasicMetrics {
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
}

#[derive(Debug, serde::Serialize)]
struct AdvancedMetrics {
    max_drawdown_pct: f64,
    max_consecutive_losses: usize,
    sharpe_ratio: f64,
    sortino_ratio: f64,
    profit_factor: f64,
    recovery_factor: f64,
    avg_trade_duration_hours: f64,
    kelly_criterion: f64,
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
    
    // ✅ NEW: Create reports directory
    let reports_dir = PathBuf::from("backtest_reports");
    std::fs::create_dir_all(&reports_dir)?;
    
    println!("Reports directory: {:?}", reports_dir);

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

    // ✅ PLAN.MD ADIM 3: KONFİGÜRASYON (Sahtelikten Arındırılmış)
    let cfg = AlgoConfigBuilder::new()
        .with_enhanced_scoring(true, 75.0, 60.0, 40.0) // Sıkı filtreleme
        .with_risk_management(2.5, 5.0) // Geniş stop, yüksek kar
        .with_fees(10.0) // 10 bps komisyon (VIP 0)
        .with_slippage(5.0) // 5 bps sabit kayma (Simülasyon yok)
        .build();

    // ✅ PLAN.MD ADIM 3: Paralel çalıştırma (Zaman Kazanımı)
    // CSV writer oluştur (Arc<Mutex> ile thread-safe)
    let file_exists = Path::new(&output_file).exists();
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&output_file)?;
    let wtr = Arc::new(Mutex::new(Writer::from_writer(file)));

    // Header yaz (sadece yeni dosya ise)
    {
        let mut wtr_guard = wtr.lock().await;
        wtr_guard.write_record(&[
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
            "max_drawdown_pct",
            "sharpe_ratio",
            "profit_factor",
            "timestamp",
        ])?;
    }

    let mut success_count = 0;
    let mut error_count = 0;
    let total_start = Utc::now();
    
    // ✅ NEW: Store all results for top 10 selection (Arc<Mutex> ile thread-safe)
    let all_results: Arc<Mutex<Vec<(String, BacktestResult)>>> = Arc::new(Mutex::new(Vec::new()));

    // ✅ PLAN.MD ADIM 3: Aynı anda 10 coin işle (API limitlerini zorlamadan maksimum hız)
    let concurrency = 10;
    
    println!("🚀 SEÇİLEN {} COIN İÇİN %100 GERÇEK VERİ TESTİ BAŞLIYOR...", selected_symbols.len());
    println!("⚡ Paralel işleme: Aynı anda {} coin işlenecek", concurrency);
    println!();

    let results = stream::iter(selected_symbols)
        .map(|symbol| {
            let cfg = cfg.clone();
            let wtr = wtr.clone();
            let all_results = all_results.clone();
            let reports_dir = reports_dir.clone();
            let interval = interval.clone();
            let period = period.clone();
            async move {
                println!("⏳ Veri İndiriliyor ve İşleniyor: {} (ForceOrders dahil)", symbol);
                
                // 1000 Mum = ~3.5 Günlük Veri (Trend analizi için yeterli)
                // Historical Force Orders da bu süreçte indirilecek
                let res = run_backtest(&symbol, &interval, &period, limit, &cfg).await;
                (symbol, res, wtr, all_results, reports_dir, interval)
            }
        })
        .buffer_unordered(concurrency)
        .collect::<Vec<_>>()
        .await;

    for (symbol, result, wtr_arc, all_results_arc, reports_dir, interval) in results {
        match result {
            Ok(res) => {
                success_count += 1;
                
                // ✅ NEW: Calculate advanced metrics
                let advanced = calculate_advanced_metrics(&res);

                // ✅ NEW: Export individual trade CSV
                let trades_file = reports_dir.join(format!("{}_trades.csv", symbol));
                if let Err(e) = export_backtest_to_csv(&res, trades_file.to_str().unwrap()) {
                    eprintln!("  ⚠️  Failed to export trades CSV for {}: {}", symbol, e);
                }

                // ✅ NEW: Create individual report JSON
                let report = CoinReport {
                    symbol: symbol.clone(),
                    basic_metrics: BasicMetrics {
                        total_trades: res.total_trades,
                        win_trades: res.win_trades,
                        loss_trades: res.loss_trades,
                        win_rate: res.win_rate,
                        total_pnl_pct: res.total_pnl_pct,
                        avg_pnl_pct: res.avg_pnl_pct,
                        avg_r: res.avg_r,
                        total_signals: res.total_signals,
                        long_signals: res.long_signals,
                        short_signals: res.short_signals,
                    },
                    advanced_metrics: AdvancedMetrics {
                        max_drawdown_pct: advanced.max_drawdown_pct,
                        max_consecutive_losses: advanced.max_consecutive_losses,
                        sharpe_ratio: advanced.sharpe_ratio,
                        sortino_ratio: advanced.sortino_ratio,
                        profit_factor: advanced.profit_factor,
                        recovery_factor: advanced.recovery_factor,
                        avg_trade_duration_hours: advanced.avg_trade_duration_hours,
                        kelly_criterion: advanced.kelly_criterion,
                    },
                    trades_file: format!("{}_trades.csv", symbol),
                    timestamp: Utc::now().to_rfc3339(),
                };
                
                let report_file = reports_dir.join(format!("{}_report.json", symbol));
                if let Ok(mut file) = File::create(&report_file) {
                    let json = serde_json::to_string_pretty(&report)?;
                    file.write_all(json.as_bytes())?;
                }

                let row = BacktestRow {
                    symbol: symbol.clone(),
                    interval: interval.clone(),
                    total_trades: res.total_trades,
                    win_trades: res.win_trades,
                    loss_trades: res.loss_trades,
                    win_rate: res.win_rate,
                    total_pnl_pct: res.total_pnl_pct,
                    avg_pnl_pct: res.avg_pnl_pct,
                    avg_r: res.avg_r,
                    total_signals: res.total_signals,
                    long_signals: res.long_signals,
                    short_signals: res.short_signals,
                    max_drawdown_pct: advanced.max_drawdown_pct,
                    sharpe_ratio: advanced.sharpe_ratio,
                    profit_factor: advanced.profit_factor,
                    timestamp: Utc::now().to_rfc3339(),
                };

                let mut wtr = wtr_arc.lock().await;
                wtr.serialize(&row)?;
                wtr.flush()?;
                drop(wtr);
                
                println!(
                    "✅ {}: PnL: %{:.2} | Sharpe: {:.2} | Trades: {}", 
                    symbol, res.total_pnl_pct * 100.0, advanced.sharpe_ratio, res.total_trades
                );
                
                // ✅ NEW: Store result for top 10 selection (thread-safe)
                let mut all_results = all_results_arc.lock().await;
                all_results.push((symbol.clone(), res));
            }
            Err(e) => {
                error_count += 1;
                eprintln!("❌ {} HATA: {}", symbol, e);
            }
        }
    }
    
    // Get all_results from Arc<Mutex>
    let all_results = all_results.lock().await.clone();

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
    println!();

    // ✅ NEW: Identify Top 10 coins by total PnL
    // ⚠️⚠️⚠️ CRITICAL OVERFITTING WARNING ⚠️⚠️⚠️
    // This "Top 10 Coin Optimization" has HIGH overfitting risk:
    // 1. Coins are selected based on PAST performance (historical backtest results)
    // 2. Optimized config is applied to coins that already performed well
    // 3. Settings that worked in the past may NOT work in the future
    // 
    // ⚠️ RECOMMENDATIONS to reduce overfitting (Plan.md - Action Plan):
    // ✅ CRITICAL: Use Walk-Forward Analysis - Split data into train/test periods
    //   - Step 1: Run backtest on FIRST HALF of data (e.g., first 12 hours of 24h period)
    //   - Step 2: Select Top 10 coins from FIRST HALF results
    //   - Step 3: Run backtest on SECOND HALF (unseen data) with Top 10 coins
    //   - Step 4: If second half also profitable, strategy is validated
    //   - If second half fails, strategy is overfitted - DO NOT use in production
    //
    // - Use Out-of-Sample Testing: Test on data NOT used for optimization
    // - Use Cross-Validation: Test on multiple time periods
    // - Be conservative: Don't assume past winners will continue winning
    // - Monitor live performance closely and be ready to adjust
    //
    // ⚠️ The "Top 10" coins are selected from HISTORICAL data only.
    // Market conditions change - what worked yesterday may fail tomorrow.
    //
    // ⚠️ IMPLEMENTATION NOTE: Walk-Forward Analysis requires:
    //   - Split kline_limit in half (e.g., 288 -> 144 for train, 144 for test)
    //   - Run first backtest with limit=144, select Top 10
    //   - Run second backtest with limit=144 but different time period (out-of-sample)
    //   - Compare results - if test period fails, strategy is overfitted
    if all_results.len() >= 10 {
        println!("===== TOP 10 COIN IDENTIFICATION =====");
        println!("⚠️  OVERFITTING WARNING: Top 10 selection based on PAST performance only.");
        println!("⚠️  These coins performed well in historical backtest - future may differ.");
        println!();
        all_results.sort_by(|a, b| {
            b.1.total_pnl_pct
                .partial_cmp(&a.1.total_pnl_pct)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        
        let top_10: Vec<String> = all_results
            .iter()
            .take(10)
            .map(|(symbol, result)| {
                println!(
                    "  {}: {:.4}% PnL, {:.2}% win rate, {} trades",
                    symbol,
                    result.total_pnl_pct * 100.0,
                    result.win_rate * 100.0,
                    result.total_trades
                );
                symbol.clone()
            })
            .collect();
        
        println!();
        println!("===== TOP 10 COIN OPTIMIZED BACKTEST =====");
        println!("⚠️⚠️⚠️  CRITICAL OVERFITTING WARNING  ⚠️⚠️⚠️");
        println!("This optimized backtest uses:");
        println!("  1. Coins selected from PAST performance (may not work in future)");
        println!("  2. Optimized config tuned for these specific coins (overfitting risk)");
        println!("  3. Same historical period (no out-of-sample testing)");
        println!();
        println!("⚠️  RECOMMENDATION: Use these results with EXTREME CAUTION.");
        println!("⚠️  Past performance does NOT guarantee future results.");
        println!("⚠️  Always test on out-of-sample data before live trading.");
        println!();
        println!("Running optimized backtest with enhanced scoring enabled...");
        println!();
        
        // ✅ NEW: Optimized config for top 10 coins - Builder pattern kullanarak
        let optimized_cfg = AlgoConfigBuilder::new()
            .with_rsi_thresholds(55.0, 45.0)
            .with_funding_thresholds(0.0003, -0.0003)
            .with_lsr_thresholds(1.3, 0.8)
            .with_min_scores(4, 4)
            .with_fees(8.0)
            .with_holding_bars(3, 48)
            .with_slippage(0.0)
            .with_signal_quality(1.5, 2.0, 3.0)
            .with_enhanced_scoring(true, 70.0, 55.0, 40.0)
            .with_risk_management(3.0, 4.0)
            .with_regime_settings(false, 6.5, 0.5, 0.6, 1.15, 0.9, 1.15)
            .build();
        
        // ✅ NEW: Run optimized backtest for top 10
        let optimized_output_file = "backtest_results_top10_optimized.csv";
        let file_exists = Path::new(&optimized_output_file).exists();
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&optimized_output_file)?;
        let mut optimized_wtr = Writer::from_writer(file);
        
        if !file_exists {
            optimized_wtr.write_record(&[
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
                "max_drawdown_pct",
                "sharpe_ratio",
                "profit_factor",
                "timestamp",
            ])?;
        }
        
        let mut optimized_results: Vec<(String, BacktestResult)> = Vec::new();
        
        for (idx, symbol) in top_10.iter().enumerate() {
            println!(
                "[{}/10] Running optimized backtest for {}...",
                idx + 1,
                symbol
            );
            
            match run_backtest(symbol, &interval, &period, limit, &optimized_cfg).await {
                Ok(result) => {
                    let advanced = calculate_advanced_metrics(&result);
                    
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
                        max_drawdown_pct: advanced.max_drawdown_pct,
                        sharpe_ratio: advanced.sharpe_ratio,
                        profit_factor: advanced.profit_factor,
                        timestamp: Utc::now().to_rfc3339(),
                    };
                    
                    optimized_wtr.serialize(&row)?;
                    optimized_wtr.flush()?;
                    
                    println!(
                        "  ✅ {}: {} trades, {:.2}% win rate, {:.4}% total PnL, Sharpe: {:.2}",
                        symbol,
                        result.total_trades,
                        result.win_rate * 100.0,
                        result.total_pnl_pct * 100.0,
                        advanced.sharpe_ratio
                    );
                    
                    optimized_results.push((symbol.clone(), result));
                }
                Err(err) => {
                    eprintln!("  ❌ {}: Optimized backtest failed: {}", symbol, err);
                }
            }
            
            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
        }
        
        // ✅ NEW: Summary report
        println!();
        println!("===== SUMMARY REPORT =====");
        println!("⚠️  OVERFITTING WARNING: These results are from optimized backtest.");
        println!("⚠️  Coins and config were selected/tuned based on PAST data.");
        println!("⚠️  Future performance may differ significantly.");
        println!();
        println!("Top 10 Coins (Optimized):");
        optimized_results.sort_by(|a, b| {
            b.1.total_pnl_pct
                .partial_cmp(&a.1.total_pnl_pct)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        
        for (idx, (symbol, result)) in optimized_results.iter().enumerate() {
            let advanced = calculate_advanced_metrics(result);
            println!(
                "  {}. {}: {:.4}% PnL, {:.2}% win rate, Sharpe: {:.2}, PF: {:.2}",
                idx + 1,
                symbol,
                result.total_pnl_pct * 100.0,
                result.win_rate * 100.0,
                advanced.sharpe_ratio,
                advanced.profit_factor
            );
        }
        
        println!();
        println!("Optimized results saved to: {}", optimized_output_file);
        println!();
        println!("===== FINAL OVERFITTING WARNING =====");
        println!("⚠️  These optimized results are from HISTORICAL backtest only.");
        println!("⚠️  Market conditions change - past winners may become future losers.");
        println!("⚠️  Always validate on out-of-sample data before live trading.");
        println!("⚠️  Monitor live performance closely and be ready to adjust strategy.");
        println!();
    }
    
    println!();
    println!("Individual reports saved to: {:?}", reports_dir);

    Ok(())
}
