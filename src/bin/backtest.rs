use anyhow::Result;
use chrono::Utc;
use dotenvy::dotenv;
use trading_bot::{run_backtest, AlgoConfig};

/// Standalone backtest runner - sadece gerçek Binance API verisi kullanır
///
/// Kullanım:
/// ```bash
/// cargo run --bin backtest
/// ```
///
/// Veya environment variable'lar ile:
/// ```bash
/// SYMBOL=BTCUSDT INTERVAL=5m PERIOD=5m LIMIT=288 cargo run --bin backtest
/// ```
#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok(); // Şu an public endpoint kullanıyoruz, API key zorunlu değil.

    // Environment variable'lardan veya default değerlerden al
    let symbol = std::env::var("SYMBOL").unwrap_or_else(|_| "BTCUSDT".to_string());
    let interval = std::env::var("INTERVAL").unwrap_or_else(|_| "5m".to_string());
    let period = std::env::var("PERIOD").unwrap_or_else(|_| "5m".to_string());
    let limit: u32 = std::env::var("LIMIT")
        .unwrap_or_else(|_| "288".to_string())
        .parse()
        .unwrap_or(288); // 288 * 5m = son 24 saat

    let cfg = AlgoConfig {
        rsi_trend_long_min: 55.0,
        rsi_trend_short_max: 45.0,
        funding_extreme_pos: 0.0005, // 0.05% (extreme long funding, ~219% APR annualized)
        funding_extreme_neg: -0.0005, // -0.05% (extreme short funding, ~-219% APR annualized)
        lsr_crowded_long: 1.3,       // longShortRatio > 1.3 => crowded long
        lsr_crowded_short: 0.8,      // longShortRatio < 0.8 => crowded short
        long_min_score: 4, // Minimum 4 score gerekli (kullanıcının "en iyi sistem" tanımı)
        short_min_score: 4, // Minimum 4 score gerekli
        fee_bps_round_trip: 8.0, // giriş+çıkış toplam 0.08% varsayalım
        max_holding_bars: 48, // max 48 bar (~4 saat @5m)
        slippage_bps: 0.0, // No slippage simulation (optimistic backtest)
        // Signal Quality Filtering (TrendPlan.md önerileri)
        min_volume_ratio: 1.5,   // Minimum volume ratio vs 20-bar average
        max_volatility_pct: 2.0, // Maximum ATR volatility % (2% = çok volatile)
        max_price_change_5bars_pct: 3.0, // 5 bar içinde max price change % (3% = parabolic move)
        enable_signal_quality_filter: true, // Signal quality filtering aktif
        // Enhanced Signal Scoring (TrendPlan.md)
        enable_enhanced_scoring: false, // Enhanced scoring kapalı (default)
        enhanced_score_excellent: 80.0, // 80-100: Excellent signal
        enhanced_score_good: 65.0,      // 65-79: Good signal
        enhanced_score_marginal: 50.0,  // 50-64: Marginal signal
        // Stop Loss & Risk Management (coin-agnostic)
        atr_stop_loss_multiplier: 3.0, // ATR multiplier for stop-loss (3.0 = 3x ATR)
        // Recommended: 2.5-3.5 for most coins, adjust based on volatility
        atr_take_profit_multiplier: 4.0, // ATR multiplier for take-profit (4.0 = 4x ATR)
        // R:R ratio = 4:3 = 1.33x (iyi risk/reward)
        // Recommended: 3.5-5.0 for most coins
        min_holding_bars: 3, // Minimum holding time (3 bars = 15 minutes @5m)
        // Çok kısa trade'leri filtrele
        // Recommended: 3-6 bars (15-30 minutes @5m)
        // ✅ ADIM 2: Config.yaml parametreleri (default değerler)
        hft_mode: false,
        base_min_score: 6.5,
        trend_threshold_hft: 0.5,
        trend_threshold_normal: 0.6,
        weak_trend_score_multiplier: 1.15,
        regime_multiplier_trending: 0.9,
        regime_multiplier_ranging: 1.15,
    };

    println!("===== BACKTEST BAŞLIYOR =====");
    println!("Symbol      : {}", symbol);
    println!("Interval    : {}", interval);
    println!("Period      : {}", period);
    println!(
        "Limit       : {} (son {} saat @{})",
        limit,
        limit as f64 * 5.0 / 60.0,
        interval
    );
    println!(
        "Başlangıç   : {}",
        Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
    );
    println!();

    let res = run_backtest(&symbol, &interval, &period, limit, &cfg).await?;

    println!(
        "===== BACKTEST SONUÇLARI: {} {}, son {} saat =====",
        symbol,
        interval,
        limit as f64 * 5.0 / 60.0
    );
    println!("Total trades   : {}", res.total_trades);
    println!("Win trades     : {}", res.win_trades);
    println!("Loss trades    : {}", res.loss_trades);
    println!("Win rate       : {:.2}%", res.win_rate * 100.0);
    println!("Total PnL      : {:.4}%", res.total_pnl_pct * 100.0);
    println!("Avg PnL/trade  : {:.4}%", res.avg_pnl_pct * 100.0);
    println!("Avg R (R/R)    : {:.2}", res.avg_r);
    println!();
    println!(
        "Bitiş         : {}",
        Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
    );

    Ok(())
}
