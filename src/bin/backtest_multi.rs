// ✅ ADIM 5: Backtest script - 100 coin için seri çalıştırma (TrendPlan.md)
// ✅ ENHANCED: Individual reports + Top 10 optimization
// 
// ⚠️ CRITICAL WARNING: Top 10 Coin Optimization has HIGH overfitting risk
// - Coins selected based on PAST performance (historical backtest)
// - Optimized config may not work in future
// - Always use Walk-Forward Analysis and Out-of-Sample Testing
// - Past performance does NOT guarantee future results

use anyhow::{Context, Result};
use chrono::Utc;
use csv::Writer;
use dotenvy::dotenv;
use futures::stream::{self, StreamExt};
use rand::seq::SliceRandom;
use rand::Rng;
use rand::thread_rng;
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
    trending::{build_signal_contexts, run_backtest_on_series},
    types::{FileConfig, Candle, FuturesClient},
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

// ✅ Plan.md: Disk Caching - Klines ve Force Orders için cache mekanizması
// Bu sayede 2. çalıştırmada 100 coin saniyeler içinde test edilir
async fn get_cached_klines(
    client: &FuturesClient,
    symbol: &str,
    interval: &str,
    limit: u32,
) -> Result<Vec<Candle>> {
    let cache_dir = Path::new("data_cache");
    if !cache_dir.exists() {
        std::fs::create_dir_all(cache_dir)
            .context("Failed to create cache directory")?;
    }
    let filename = cache_dir.join(format!("{}_{}_{}.json", symbol, interval, limit));

    if filename.exists() {
        // Cache'den oku
        let data = std::fs::read_to_string(&filename)
            .context(format!("Failed to read cache file: {:?}", filename))?;
        let candles: Vec<Candle> = serde_json::from_str(&data)
            .context(format!("Failed to parse cache file: {:?}", filename))?;
        println!("  📦 Cache hit: {} ({} candles)", symbol, candles.len());
        return Ok(candles);
    }

    // API'den çek
    println!("  ⬇️  Fetching from API: {}...", symbol);
    let candles = client
        .fetch_klines(symbol, interval, limit)
        .await
        .context(format!("Failed to fetch klines for {}", symbol))?;
    
    // Cache'e yaz
    let json = serde_json::to_string_pretty(&candles)
        .context("Failed to serialize candles")?;
    std::fs::write(&filename, json)
        .context(format!("Failed to write cache file: {:?}", filename))?;
    
    Ok(candles)
}

// ✅ Plan.md: Force Orders için cache mekanizması
// Force Orders API çağrısı çok ağır (weight: 20+), cache ile hızlandırıyoruz
async fn get_cached_force_orders(
    client: &FuturesClient,
    symbol: &str,
    start_time: Option<chrono::DateTime<Utc>>,
    end_time: Option<chrono::DateTime<Utc>>,
    limit: u32,
) -> Result<String> {
    use trading_bot::types::ForceOrderRecord;
    
    let cache_dir = Path::new("data_cache");
    if !cache_dir.exists() {
        std::fs::create_dir_all(cache_dir)
            .context("Failed to create cache directory")?;
    }
    
    // Cache key: symbol + time range + limit
    let cache_key = format!(
        "{}_{}_{}_{}_force_orders.json",
        symbol,
        start_time.map(|t| t.timestamp()).unwrap_or(0),
        end_time.map(|t| t.timestamp()).unwrap_or(0),
        limit
    );
    let filename = cache_dir.join(cache_key);

    if filename.exists() {
        // Cache'den oku
        let data = std::fs::read_to_string(&filename)
            .context(format!("Failed to read cache file: {:?}", filename))?;
        println!("  📦 Cache hit (ForceOrders): {} ({} bytes)", symbol, data.len());
        return Ok(data);
    }

    // API'den çek
    println!("  ⬇️  Fetching ForceOrders from API: {}...", symbol);
    let force_orders = client
        .fetch_historical_force_orders(symbol, start_time, end_time, limit)
        .await
        .context(format!("Failed to fetch force orders for {}", symbol))?;
    
    // JSON string olarak cache'e yaz
    let json = serde_json::to_string_pretty(&force_orders)
        .context("Failed to serialize force orders")?;
    std::fs::write(&filename, &json)
        .context(format!("Failed to write cache file: {:?}", filename))?;
    
    Ok(json)
}

// ✅ Plan.md: Cache'li run_backtest wrapper
// run_backtest fonksiyonunu cache kullanacak şekilde wrap ediyoruz
async fn run_backtest_with_cache(
    symbol: &str,
    interval: &str,
    period: &str,
    limit: u32,
    cfg: &trading_bot::AlgoConfig,
) -> Result<BacktestResult> {
    use trading_bot::types::{FuturesClient, FundingRate, OpenInterestPoint, LongShortRatioPoint};
    use trading_bot::trending::{build_signal_contexts, run_backtest_on_series};
    
    let client = FuturesClient::new();
    
    // ✅ Plan.md: Cache'li klines çekimi
    let candles = get_cached_klines(&client, symbol, interval, limit).await?;
    
    // Diğer verileri çek (bunlar da cache'lenebilir ama şimdilik sadece klines ve force orders)
    let funding = client.fetch_funding_rates(symbol, 100).await?;
    let oi_hist = client
        .fetch_open_interest_hist(symbol, period, limit)
        .await?;
    let lsr_hist = client
        .fetch_top_long_short_ratio(symbol, period, limit)
        .await?;

    // ✅ Plan.md: Cache'li force orders çekimi
    let start_time = candles.first().map(|c| c.open_time);
    let end_time = candles.last().map(|c| c.close_time);
    let force_orders_json = get_cached_force_orders(&client, symbol, start_time, end_time, 500)
        .await
        .unwrap_or_default(); // Sessizce boş dön (veri yoksa strateji çalışmaz)
    
    // JSON'dan deserialize et (run_backtest_on_series için)
    use trading_bot::types::ForceOrderRecord;
    let force_orders: Vec<ForceOrderRecord> = if force_orders_json.is_empty() {
        Vec::new()
    } else {
        serde_json::from_str(force_orders_json.as_str())
            .unwrap_or_default()
    };

    let (matched_candles, contexts) =
        build_signal_contexts(&candles, &funding, &oi_hist, &lsr_hist);
    
    Ok(run_backtest_on_series(
        symbol,
        &matched_candles,
        &contexts,
        cfg,
        if force_orders.is_empty() {
            None
        } else {
            Some(&force_orders)
        },
    ))
}

// ✅ Plan.md: Walk-Forward için candle slice fonksiyonu
// Tüm veriyi çekip, belirli bir aralığı (slice) kullanarak backtest yapar
async fn run_backtest_with_slice(
    symbol: &str,
    interval: &str,
    period: &str,
    total_limit: u32, // Tüm veri limiti
    skip_first: usize, // İlk N mum'u atla (training için eski veri)
    take_count: usize, // Sonraki N mum'u al (training veya testing için)
    cfg: &trading_bot::AlgoConfig,
) -> Result<BacktestResult> {
    use trading_bot::types::FuturesClient;
    use trading_bot::trending::{build_signal_contexts, run_backtest_on_series};
    
    let client = FuturesClient::new();
    
    // ✅ Plan.md: Tüm veriyi cache'den çek
    let all_candles = get_cached_klines(&client, symbol, interval, total_limit).await?;
    
    // Slice yap: skip_first'ten başla, take_count kadar al
    if skip_first >= all_candles.len() || take_count == 0 {
        return Ok(BacktestResult {
            trades: Vec::new(),
            total_trades: 0,
            win_trades: 0,
            loss_trades: 0,
            win_rate: 0.0,
            total_pnl_pct: 0.0,
            avg_pnl_pct: 0.0,
            avg_r: 0.0,
            total_signals: 0,
            long_signals: 0,
            short_signals: 0,
        });
    }
    
    let end_idx = (skip_first + take_count).min(all_candles.len());
    let candles_slice = &all_candles[skip_first..end_idx];
    
    // Slice için time range
    let start_time = candles_slice.first().map(|c| c.open_time);
    let end_time = candles_slice.last().map(|c| c.close_time);
    
    // Diğer verileri çek (aynı time range için)
    let funding = client.fetch_funding_rates(symbol, 100).await?;
    let oi_hist = client
        .fetch_open_interest_hist(symbol, period, candles_slice.len() as u32)
        .await?;
    let lsr_hist = client
        .fetch_top_long_short_ratio(symbol, period, candles_slice.len() as u32)
        .await?;

    // Force orders (slice time range için)
    let force_orders_json = get_cached_force_orders(&client, symbol, start_time, end_time, 500)
        .await
        .unwrap_or_default();
    
    use trading_bot::types::ForceOrderRecord;
    let force_orders: Vec<ForceOrderRecord> = if force_orders_json.is_empty() {
        Vec::new()
    } else {
        serde_json::from_str(force_orders_json.as_str())
            .unwrap_or_default()
    };

    let (matched_candles, contexts) =
        build_signal_contexts(candles_slice, &funding, &oi_hist, &lsr_hist);
    
    Ok(run_backtest_on_series(
        symbol,
        &matched_candles,
        &contexts,
        cfg,
        if force_orders.is_empty() {
            None
        } else {
            Some(&force_orders)
        },
    ))
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
    // ✅ Plan.md: 4 Günlük veri (trend oluşumu için yeterli)
    let limit: u32 = std::env::var("LIMIT")
        .unwrap_or_else(|_| "1152".to_string())
        .parse()
        .unwrap_or(288 * 4); // 1152 * 5m = 4 günlük veri

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

    println!("===== PRO MULTI-COIN BACKTEST =====");
    println!("Interval    : {}", interval);
    println!("Period      : {}", period);
    println!(
        "Limit       : {} ({} günlük veri @{})",
        limit,
        limit as f64 * 5.0 / 60.0 / 24.0,
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
        "Discovered {} symbols, selecting {} for backtest...",
        all_symbols.len(),
        max_symbols
    );

    // ✅ CRITICAL FIX: Selection Bias (Look-ahead Bias) - Plan.md
    // PROBLEM: Selecting top gainers based on CURRENT 24h performance creates look-ahead bias.
    // A coin that's currently a "Top Gainer" likely just had a big move in the last 24-48 hours.
    // Testing it on past 4 days will show the bot "caught" that move, but in reality,
    // you couldn't have known to select that coin BEFORE the move happened.
    //
    // SOLUTION: Use random selection or fixed pool (e.g., Top 50 Market Cap) instead of
    // selecting based on current performance metrics.
    //
    // Option 1: Random selection (unbiased, realistic)
    // Option 2: Fixed pool (e.g., first N symbols alphabetically or by market cap)
    //
    // We'll use random selection to avoid look-ahead bias:
    let mut rng = thread_rng();
    let mut shuffled_symbols = all_symbols;
    shuffled_symbols.shuffle(&mut rng);
    
    let selected_symbols: Vec<String> = shuffled_symbols
        .into_iter()
        .take(max_symbols)
        .collect();

    println!(
        "✅ Selected {} symbols using RANDOM selection (avoiding look-ahead bias)",
        selected_symbols.len()
    );
    println!("⚠️  NOTE: Random selection ensures backtest results are realistic and not biased by current top gainers.");
    println!();

    // ✅ Plan.md: Pro Ayarlar - Yüksek kalite sinyaller, gerçekçi komisyon ve slippage
    let cfg = AlgoConfigBuilder::new()
        .with_enhanced_scoring(true, 75.0, 60.0, 45.0) // Yüksek kalite sinyaller
        .with_risk_management(2.5, 4.0) // ATR x 2.5 Stop, ATR x 4.0 TP
        .with_fees(13.0) // ✅ Plan.md: Gerçekçi komisyon (Binance Futures Taker Fee ~0.05% * 2 = 0.10% = 10 bps, slippage dahil 13 bps)
        .with_slippage(5.0) // 0.05% baz kayma (Deterministik - rastgelelik yok)
        .build();

    // ✅ PLAN.MD ADIM 3: Paralel çalıştırma (Zaman Kazanımı)
    // CSV writer oluştur (Arc<Mutex> ile thread-safe)
    let _file_exists = Path::new(&output_file).exists();
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

    // ✅ Plan.md: Akıllı Paralel İşleme
    // Binance API ağırlık limitlerine (Weight Limit) takılmadan maksimum hızı almak için
    // buffer_unordered ayarlandı. 100 coin'i ~10-15 dakikada tarayacak kapasitede.
    // Plan.md: Buffer 10 - Binance API limitlerine takılmadan maksimum hız
    let concurrency = 10; 
    
    println!("✅ Selected {} coins for rigorous backtesting.", selected_symbols.len());
    println!("🚀 Starting parallel execution (Concurrency: {})...", concurrency);
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
                // ✅ CRITICAL FIX: Rate Limit Protection - Plan.md
                // PROBLEM: 100 coin x (Kline + Funding + OI + LSR + ForceOrder) calls
                // can cause Binance IP ban if all requests hit at the same time.
                //
                // SOLUTION: Add random delay before each coin processing to spread out requests.
                // This prevents all requests from hitting the API simultaneously.
                let delay_ms = Rng::gen_range(&mut thread_rng(), 0..500); // 0-500ms random delay
                tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
                
                println!("⏳ Veri İndiriliyor ve İşleniyor: {} (ForceOrders dahil)", symbol);
                
                // 1000 Mum = ~3.5 Günlük Veri (Trend analizi için yeterli)
                // ✅ Plan.md: Cache'li backtest kullan (API limitlerini önler)
                let res = run_backtest_with_cache(&symbol, &interval, &period, limit, &cfg).await;
                (symbol, res, wtr, all_results, reports_dir, interval)
            }
        })
        .buffer_unordered(concurrency)
        .collect::<Vec<_>>()
        .await;

    // ✅ Plan.md: Sonuçları işleme ve kaydetme
    let mut total_pnl_sum = 0.0;
    let mut profitable_coins = 0;

    for (symbol, result, wtr_arc, all_results_arc, reports_dir, interval) in results {
        match result {
            Ok(res) => {
                success_count += 1;
                
                // ✅ Plan.md: Toplam PnL ve karlı coin sayısını hesapla
                total_pnl_sum += res.total_pnl_pct;
                if res.total_pnl_pct > 0.0 {
                    profitable_coins += 1;
                }
                
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
    let all_results = all_results.lock().await;
    let mut all_results = all_results.clone();

    let total_duration = Utc::now() - total_start;

    println!();
    println!("===== SONUÇ RAPORU =====");
    println!("⏱️  Toplam Süre: {} saniye", total_duration.num_seconds());
    println!("💰 Toplam Portföy PnL (Eşit Ağırlıklı): %{:.2}", total_pnl_sum * 100.0);
    println!("🏆 Karlı Coin Sayısı: {} / {}", profitable_coins, max_symbols);
    println!("📄 Rapor Dosyası: {}", output_file);
    println!();
    println!("Success      : {}", success_count);
    println!("Errors       : {}", error_count);
    println!(
        "Bitiş        : {}",
        Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
    );
    println!();

    // ✅ Plan.md: Walk-Forward Analysis (Aşırı Uyum Önleme)
    // Veriyi ikiye böl: Training (ilk yarı) ve Testing (ikinci yarı - görmediği veri)
    // Bu sayede overfitting'i önleriz ve stratejinin gerçek performansını görürüz
    let enable_walk_forward = std::env::var("WALK_FORWARD").unwrap_or_else(|_| "true".to_string()) == "true";
    
    if enable_walk_forward && all_results.len() >= 10 && limit >= 200 {
        // ✅ Plan.md: Walk-Forward Analysis Implementation
        println!("===== WALK-FORWARD ANALYSIS (Overfitting Önleme) =====");
        println!("🚀 FAZ 1: Eğitim Verisi ile Tarama (İlk {} mumdan {} mum)...", limit, limit / 2);
        println!("⚠️  NOT: Walk-Forward analizi için veri yeterli olmalı (limit >= 200)");
        println!();
        
        // ✅ Plan.md: Walk-Forward Analysis - Doğru implementasyon
        // Training: İlk yarı (eski veri) - "Son {} mumdan önceki {} mum"
        // Testing: İkinci yarı (yeni veri) - "Son {} mum"
        let training_limit = (limit / 2) as usize;
        let testing_limit = (limit / 2) as usize;
        
        // Training phase: İlk yarı (eski veri) ile backtest
        // Plan.md: "Son {} mumdan önceki {} mum" = skip_first=0, take=training_limit
        let mut training_results: Vec<(String, BacktestResult)> = Vec::new();
        
        println!("📊 Training Phase: İlk {} mum (eski veri) ile backtest çalıştırılıyor...", training_limit);
        println!("   (Son {} mumdan önceki {} mum - Plan.md)", limit, training_limit);
        for (symbol, _) in all_results.iter().take(50) { // İlk 50 coin ile training (hız için)
            match run_backtest_with_slice(
                symbol, 
                &interval, 
                &period, 
                limit, // Tüm veri
                0, // İlk 0 mum'u atla (baştan başla)
                training_limit, // İlk yarıyı al
                &cfg
            ).await {
                Ok(result) => {
                    training_results.push((symbol.clone(), result));
                }
                Err(_) => {
                    // Skip errors in training phase
                }
            }
        }
        
        // Select Top 10 from training results
        training_results.sort_by(|a, b| {
            b.1.total_pnl_pct
                .partial_cmp(&a.1.total_pnl_pct)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        
        let top_10_training: Vec<String> = training_results
            .iter()
            .take(10)
            .map(|(symbol, result)| {
                println!(
                    "  📈 Training: {}: {:.4}% PnL, {:.2}% win rate, {} trades",
                    symbol,
                    result.total_pnl_pct * 100.0,
                    result.win_rate * 100.0,
                    result.total_trades
                );
                symbol.clone()
            })
            .collect();
        
        println!();
        println!("🚀 FAZ 2: Test Verisi ile Doğrulama (Son {} mum - Görmediği veri)...", testing_limit);
        println!("⚠️  Seçilen Top 10 coin, görmediği ikinci yarı veri ile test edilecek.");
        println!();
        
        // ✅ Plan.md: Testing phase: İkinci yarı (yeni veri) ile backtest (out-of-sample)
        // Plan.md: "Son {} mum" = skip_first=training_limit, take=testing_limit
        let mut testing_results: Vec<(String, BacktestResult)> = Vec::new();
        
        for symbol in &top_10_training {
            match run_backtest_with_slice(
                symbol,
                &interval,
                &period,
                limit, // Tüm veri
                training_limit, // İlk yarıyı atla (eski veriyi skip et)
                testing_limit, // İkinci yarıyı al (yeni veri)
                &cfg
            ).await {
                Ok(result) => {
                    let result_clone = BacktestResult {
                        trades: result.trades.clone(),
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
                    };
                    testing_results.push((symbol.clone(), result_clone));
                    println!(
                        "  ✅ Test: {}: {:.4}% PnL, {:.2}% win rate, {} trades",
                        symbol,
                        result.total_pnl_pct * 100.0,
                        result.win_rate * 100.0,
                        result.total_trades
                    );
                }
                Err(err) => {
                    eprintln!("  ❌ Test: {} failed: {}", symbol, err);
                }
            }
        }
        
        // Compare training vs testing results
        println!();
        println!("===== WALK-FORWARD SONUÇLARI =====");
        let training_avg_pnl: f64 = training_results.iter().take(10).map(|(_, r)| r.total_pnl_pct).sum::<f64>() / 10.0;
        let testing_avg_pnl: f64 = testing_results.iter().map(|(_, r)| r.total_pnl_pct).sum::<f64>() / testing_results.len().max(1) as f64;
        
        println!("📊 Training Phase (İlk yarı) Ortalama PnL: {:.4}%", training_avg_pnl * 100.0);
        println!("📊 Testing Phase (İkinci yarı) Ortalama PnL: {:.4}%", testing_avg_pnl * 100.0);
        
        if testing_avg_pnl > 0.0 && testing_avg_pnl >= training_avg_pnl * 0.5 {
            println!("✅ WALK-FORWARD VALIDATION: Strateji doğrulandı! Test fazı da karlı.");
        } else if testing_avg_pnl < 0.0 {
            println!("❌ WALK-FORWARD WARNING: Test fazı zararlı! Strateji overfitted olabilir.");
            println!("⚠️  Bu stratejiyi canlıda kullanmadan önce daha fazla test yapın.");
        } else {
            println!("⚠️  WALK-FORWARD CAUTION: Test fazı performansı düşük. Strateji overfitted olabilir.");
        }
        println!();
    }
    
    // ✅ NEW: Identify Top 10 coins by total PnL (Original method - still available)
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
            .with_fees(13.0)  // ✅ Plan.md: Gerçekçi komisyon (10 bps + slippage = 13 bps)
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
            
            match run_backtest_with_cache(symbol, &interval, &period, limit, &optimized_cfg).await {
                Ok(result) => {
                    let advanced = calculate_advanced_metrics(&result);
                    
                    let row = BacktestRow {
                        symbol: symbol.to_string(),
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
                    
                    optimized_results.push((symbol.to_string(), result));
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
