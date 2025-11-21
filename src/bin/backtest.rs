use anyhow::Result;
use chrono::Utc;
use dotenvy::dotenv;
use trading_bot::{create_test_config, run_backtest, BacktestFormatter};

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

    let cfg = create_test_config();

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

    println!();
    println!(
        "{}",
        BacktestFormatter::format_complete_report(&res, &symbol, &interval, limit, true)
    );
    println!();
    println!(
        "Bitiş         : {}",
        Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
    );

    Ok(())
}
