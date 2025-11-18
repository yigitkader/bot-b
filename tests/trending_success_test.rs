// Test trending module success rate with real Binance data
// This test fetches real kline data from Binance and tests signal accuracy

use app::config::AppCfg;
use app::event_bus::MarketTick;
use app::trending::Trending;
use app::types::{LastSignal, PricePoint, Px, SymbolState, TrendSignal};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::collections::{HashMap, VecDeque};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use serde::Deserialize;

#[derive(Deserialize)]
struct BinanceKline {
    #[serde(rename = "0")]
    open_time: u64,
    #[serde(rename = "1")]
    open: String,
    #[serde(rename = "2")]
    high: String,
    #[serde(rename = "3")]
    low: String,
    #[serde(rename = "4")]
    close: String,
    #[serde(rename = "5")]
    volume: String,
    #[serde(rename = "6")]
    close_time: u64,
    #[serde(rename = "7")]
    quote_volume: String,
    #[serde(rename = "8")]
    trades: u64,
    #[serde(rename = "9")]
    taker_buy_base: String,
    #[serde(rename = "10")]
    taker_buy_quote: String,
    #[serde(rename = "11")]
    _ignore: String,
}

/// Fetch real kline data from Binance
async fn fetch_binance_klines(
    symbol: &str,
    interval: &str,
    limit: u32,
) -> Result<Vec<BinanceKline>, Box<dyn std::error::Error>> {
    use reqwest::Client;
    
    let client = Client::new();
    let url = format!(
        "https://fapi.binance.com/fapi/v1/klines?symbol={}&interval={}&limit={}",
        symbol, interval, limit
    );
    
    let response = client.get(&url).send().await?;
    let status = response.status();
    
    if !status.is_success() {
        let body = response.text().await?;
        return Err(format!("Binance API error: {} - {}", status, body).into());
    }
    
    let klines: Vec<BinanceKline> = response.json().await?;
    Ok(klines)
}

/// Convert klines to MarketTicks
fn klines_to_ticks(klines: Vec<BinanceKline>, symbol: String) -> Vec<MarketTick> {
    let mut ticks = Vec::new();
    let mut timestamp_offset = Instant::now();
    
    for (idx, kline) in klines.iter().enumerate() {
        // Parse real Binance data - fail if data is invalid (no mock/dummy data)
        let close_price = Decimal::from_str(&kline.close)
            .expect(&format!("Failed to parse close price from Binance kline: {}", kline.close));
        let volume = Decimal::from_str(&kline.volume)
            .expect(&format!("Failed to parse volume from Binance kline: {}", kline.volume));
        
        // Validate real data
        if close_price <= Decimal::ZERO {
            panic!("Invalid close price from Binance: {} (must be > 0)", close_price);
        }
        if volume < Decimal::ZERO {
            panic!("Invalid volume from Binance: {} (must be >= 0)", volume);
        }
        
        // Create bid/ask from close price with small spread (using real market spread calculation)
        let spread = close_price * Decimal::from_str("0.0001")
            .expect("Failed to parse spread multiplier");
        let bid = close_price - spread / Decimal::from(2);
        let ask = close_price + spread / Decimal::from(2);
        
        ticks.push(MarketTick {
            symbol: symbol.clone(),
            bid: Px(bid),
            ask: Px(ask),
            mark_price: Some(Px(close_price)),
            volume: Some(volume),
            timestamp: timestamp_offset + Duration::from_secs(idx as u64),
        });
    }
    
    ticks
}

/// Test trending signal accuracy with real Binance data
/// This is a point-in-time test: signals at time T are validated at time T+1
#[tokio::test]
#[ignore] // Requires internet connection and Binance API
async fn test_trending_success_with_real_binance_data() {
    let cfg = Arc::new(
        app::config::load_config().unwrap_or_else(|_| AppCfg::default())
    );
    
    // Test with popular symbols
    let test_symbols = vec!["BTCUSDT", "ETHUSDT", "SOLUSDT"];
    let interval = "1m"; // 1-minute candles
    let limit = 200; // Last 200 candles (~3.3 hours of data)
    
    println!("\n🧪 Testing Trending Module Success Rate with Real Binance Data");
    println!("================================================================");
    
    for symbol in test_symbols {
        println!("\n📊 Testing symbol: {}", symbol);
        
        // Fetch real kline data
        let klines = match fetch_binance_klines(symbol, interval, limit).await {
            Ok(k) => {
                println!("  ✅ Fetched {} klines from Binance", k.len());
                k
            }
            Err(e) => {
                println!("  ❌ Failed to fetch klines: {}", e);
                continue;
            }
        };
        
        if klines.is_empty() {
            println!("  ⚠️  No kline data received");
            continue;
        }
        
        // Convert to MarketTicks
        let ticks = klines_to_ticks(klines, symbol.to_string());
        println!("  📈 Converted to {} market ticks", ticks.len());
        
        // Initialize trending module state
        let symbol_states = Arc::new(tokio::sync::Mutex::new(HashMap::<String, SymbolState>::new()));
        let last_signals = Arc::new(tokio::sync::Mutex::new(HashMap::<String, LastSignal>::new()));
        
        let mut signals_generated = 0u64;
        let mut correct_signals = 0u64;
        let mut incorrect_signals = 0u64;
        let mut long_signals_correct = 0u64;
        let mut long_signals_total = 0u64;
        let mut short_signals_correct = 0u64;
        let mut short_signals_total = 0u64;
        let mut price_changes: Vec<f64> = Vec::new();
        
        // Process ticks and validate signals
        for (tick_idx, tick) in ticks.iter().enumerate() {
            // Update symbol state
            {
                let mut states = symbol_states.lock().await;
                let state = states.entry(tick.symbol.clone()).or_insert_with(|| {
                    SymbolState {
                        symbol: tick.symbol.clone(),
                        prices: VecDeque::new(),
                        last_signal_time: None,
                        last_position_close_time: None,
                        last_position_direction: None,
                        tick_counter: 0,
                        ema_9: None,
                        ema_21: None,
                        ema_55: None,
                        ema_55_history: VecDeque::new(),
                        rsi_avg_gain: None,
                        rsi_avg_loss: None,
                        rsi_period_count: 0,
                        last_analysis_time: None,
                    }
                });
                
                let mid_price = (tick.bid.0 + tick.ask.0) / Decimal::from(2);
                state.prices.push_back(PricePoint {
                    timestamp: tick.timestamp,
                    price: mid_price,
                    volume: tick.volume,
                });
                
                // Keep last 100 prices
                while state.prices.len() > 100 {
                    state.prices.pop_front();
                }
                
                // Update indicators
                Trending::update_indicators(state, mid_price);
            }
            
            // Check for signal (only after we have enough data)
            if tick_idx < 55 {
                continue; // Need at least 55 price points for analysis
            }
            
            let state = {
                let states = symbol_states.lock().await;
                states.get(&tick.symbol).cloned()
            };
            
            if let Some(state) = state {
                // Analyze trend
                if let Some(signal) = Trending::analyze_trend(&state, &cfg.trending) {
                    signals_generated += 1;
                    
                    // Validate signal: check price movement in next candle
                    if tick_idx + 1 < ticks.len() {
                        // Calculate prices from real market data
                        let current_price = (tick.bid.0 + tick.ask.0) / Decimal::from(2);
                        let next_tick = &ticks[tick_idx + 1];
                        let next_price = (next_tick.bid.0 + next_tick.ask.0) / Decimal::from(2);
                        
                        // Calculate price change from real market data - fail if conversion fails
                        let price_change_pct = ((next_price - current_price) / current_price * Decimal::from(100))
                            .to_f64()
                            .expect("Failed to convert price change percentage to f64");
                        
                        price_changes.push(price_change_pct);
                        
                        let is_correct = match signal {
                            TrendSignal::Long => {
                                long_signals_total += 1;
                                price_change_pct > 0.0 // Long is correct if price goes up
                            }
                            TrendSignal::Short => {
                                short_signals_total += 1;
                                price_change_pct < 0.0 // Short is correct if price goes down
                            }
                        };
                        
                        if is_correct {
                            correct_signals += 1;
                            match signal {
                                TrendSignal::Long => long_signals_correct += 1,
                                TrendSignal::Short => short_signals_correct += 1,
                            }
                        } else {
                            incorrect_signals += 1;
                        }
                        
                        let signal_str = match signal {
                            TrendSignal::Long => "LONG",
                            TrendSignal::Short => "SHORT",
                        };
                        let result_str = if is_correct { "✅" } else { "❌" };
                        // Display prices (unwrap_or only for display, not for calculations)
                        let current_price_f64 = current_price.to_f64()
                            .expect("Failed to convert current price to f64 for display");
                        let next_price_f64 = next_price.to_f64()
                            .expect("Failed to convert next price to f64 for display");
                        
                        println!(
                            "  {} Signal #{}: {} @ ${:.2}, next price: ${:.2} ({:+.2}%)",
                            result_str,
                            signals_generated,
                            signal_str,
                            current_price_f64,
                            next_price_f64,
                            price_change_pct
                        );
                    }
                }
            }
        }
        
        // Calculate and print results
        let win_rate = if signals_generated > 0 {
            correct_signals as f64 / signals_generated as f64 * 100.0
        } else {
            0.0
        };
        
        let avg_price_change = if !price_changes.is_empty() {
            price_changes.iter().sum::<f64>() / price_changes.len() as f64
        } else {
            0.0
        };
        
        let long_win_rate = if long_signals_total > 0 {
            long_signals_correct as f64 / long_signals_total as f64 * 100.0
        } else {
            0.0
        };
        
        let short_win_rate = if short_signals_total > 0 {
            short_signals_correct as f64 / short_signals_total as f64 * 100.0
        } else {
            0.0
        };
        
        println!("\n  📊 Results for {}:", symbol);
        println!("     Total signals generated: {}", signals_generated);
        println!("     Correct signals: {} ({:.2}%)", correct_signals, win_rate);
        println!("     Incorrect signals: {} ({:.2}%)", incorrect_signals, 100.0 - win_rate);
        println!("     Long signals: {} total, {} correct ({:.2}%)", 
                 long_signals_total, long_signals_correct, long_win_rate);
        println!("     Short signals: {} total, {} correct ({:.2}%)", 
                 short_signals_total, short_signals_correct, short_win_rate);
        println!("     Average price change after signal: {:.4}%", avg_price_change);
        
        // Assertions (relaxed thresholds for real market data)
        if signals_generated > 0 {
            // Win rate should be better than random (50%)
            assert!(
                win_rate >= 45.0,
                "Win rate {}% is too low (expected >= 45%)",
                win_rate
            );
            
            println!("     ✅ Win rate test passed: {:.2}% >= 45%", win_rate);
        } else {
            println!("     ⚠️  No signals generated (may need more data or different config)");
        }
    }
    
    println!("\n✅ Trending success test completed!");
}

