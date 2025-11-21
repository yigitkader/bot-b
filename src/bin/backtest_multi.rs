// ✅ ADIM 5: Backtest script - 100 coin için seri çalıştırma (TrendPlan.md)
// ✅ ENHANCED: Individual reports + Top 10 optimization

use anyhow::Result;
use chrono::Utc;
use csv::Writer;
use dotenvy::dotenv;
use serde_json;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use trading_bot::{
    calculate_advanced_metrics, export_backtest_to_csv, run_backtest,
    symbol_scanner::{SymbolScanner, SymbolSelectionConfig},
    types::FileConfig,
    AlgoConfig, BacktestResult,
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
        enable_enhanced_scoring: true, // ✅ FIX: Enhanced scoring açık
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
            "max_drawdown_pct",
            "sharpe_ratio",
            "profit_factor",
            "timestamp",
        ])?;
    }

    let mut success_count = 0;
    let mut error_count = 0;
    let total_start = Utc::now();
    
    // ✅ NEW: Store all results for top 10 selection
    let mut all_results: Vec<(String, BacktestResult)> = Vec::new();

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
                
                // ✅ NEW: Calculate advanced metrics
                let advanced = calculate_advanced_metrics(&result);

                // ✅ NEW: Export individual trade CSV
                let trades_file = reports_dir.join(format!("{}_trades.csv", symbol));
                if let Err(e) = export_backtest_to_csv(&result, trades_file.to_str().unwrap()) {
                    eprintln!("  ⚠️  Failed to export trades CSV for {}: {}", symbol, e);
                }

                // ✅ NEW: Create individual report JSON
                let report = CoinReport {
                    symbol: symbol.clone(),
                    basic_metrics: BasicMetrics {
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

                wtr.serialize(&row)?;
                wtr.flush()?;
                
                println!(
                    "  ✅ {}: {} trades, {:.2}% win rate, {:.4}% total PnL, Sharpe: {:.2}",
                    symbol,
                    result.total_trades,
                    result.win_rate * 100.0,
                    result.total_pnl_pct * 100.0,
                    advanced.sharpe_ratio
                );
                
                // ✅ NEW: Store result for top 10 selection (clone after use)
                all_results.push((symbol.clone(), result));
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
    println!();

    // ✅ NEW: Identify Top 10 coins by total PnL
    if all_results.len() >= 10 {
        println!("===== TOP 10 COIN IDENTIFICATION =====");
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
        println!("Running optimized backtest with enhanced scoring enabled...");
        println!();
        
        // ✅ NEW: Optimized config for top 10 coins
        let optimized_cfg = AlgoConfig {
            rsi_trend_long_min: 55.0,
            rsi_trend_short_max: 45.0,
            funding_extreme_pos: 0.0003, // ✅ More aggressive
            funding_extreme_neg: -0.0003,
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
            enable_enhanced_scoring: true, // ✅ Enhanced scoring enabled
            enhanced_score_excellent: 75.0, // ✅ Slightly lower for more signals
            enhanced_score_good: 60.0,
            enhanced_score_marginal: 45.0,
            atr_stop_loss_multiplier: 3.0,
            atr_take_profit_multiplier: 4.0,
            min_holding_bars: 3,
            hft_mode: false,
            base_min_score: 6.5,
            trend_threshold_hft: 0.5,
            trend_threshold_normal: 0.6,
            weak_trend_score_multiplier: 1.15,
            regime_multiplier_trending: 0.9,
            regime_multiplier_ranging: 1.15,
        };
        
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
    }
    
    println!();
    println!("Individual reports saved to: {:?}", reports_dir);

    Ok(())
}
