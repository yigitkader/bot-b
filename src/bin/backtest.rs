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
        funding_extreme_pos: 0.0005,  // 0.05% (extreme long funding, ~219% APR annualized)
        funding_extreme_neg: -0.0005, // -0.05% (extreme short funding, ~-219% APR annualized)
        lsr_crowded_long: 1.3,        // longShortRatio > 1.3 => crowded long
        lsr_crowded_short: 0.8,       // longShortRatio < 0.8 => crowded short
        long_min_score: 4,             // Minimum 4 score gerekli (kullanıcının "en iyi sistem" tanımı)
        short_min_score: 4,            // Minimum 4 score gerekli
        fee_bps_round_trip: 8.0,      // giriş+çıkış toplam 0.08% varsayalım
        max_holding_bars: 48,          // max 48 bar (~4 saat @5m)
        slippage_bps: 0.0,            // No slippage simulation (optimistic backtest)
    };

    println!("===== BACKTEST BAŞLIYOR =====");
    println!("Symbol      : {}", symbol);
    println!("Interval    : {}", interval);
    println!("Period      : {}", period);
    println!("Limit       : {} (son {} saat @{})", limit, limit as f64 * 5.0 / 60.0, interval);
    println!("Başlangıç   : {}", Utc::now().format("%Y-%m-%d %H:%M:%S UTC"));
    println!();

    let res = run_backtest(&symbol, &interval, &period, limit, &cfg).await?;

    println!("===== BACKTEST SONUÇLARI: {} {}, son {} saat =====", symbol, interval, limit as f64 * 5.0 / 60.0);
    println!("Total trades   : {}", res.total_trades);
    println!("Win trades     : {}", res.win_trades);
    println!("Loss trades    : {}", res.loss_trades);
    println!("Win rate       : {:.2}%", res.win_rate * 100.0);
    println!(
        "Total PnL      : {:.4}%",
        res.total_pnl_pct * 100.0
    );
    println!(
        "Avg PnL/trade  : {:.4}%",
        res.avg_pnl_pct * 100.0
    );
    println!(
        "Avg R (R/R)    : {:.2}",
        res.avg_r
    );
    println!();
    println!("Bitiş         : {}", Utc::now().format("%Y-%m-%d %H:%M:%S UTC"));

    Ok(())
}

