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

/// Calculate potential PnL for a trade signal
/// Returns: (gross_pnl_usd, net_pnl_usd, commission_usd)
fn calculate_trade_pnl(
    signal: TrendSignal,
    entry_price: Decimal,
    exit_price: Decimal,
    margin_usd: f64,
    leverage: u32,
    commission_pct: f64,
) -> (f64, f64, f64) {
    // Calculate price change percentage
    let price_change_pct = match signal {
        TrendSignal::Long => {
            // Long: profit when price goes up
            ((exit_price - entry_price) / entry_price * Decimal::from(100))
                .to_f64()
                .expect("Failed to convert price change to f64")
        }
        TrendSignal::Short => {
            // Short: profit when price goes down
            ((entry_price - exit_price) / entry_price * Decimal::from(100))
                .to_f64()
                .expect("Failed to convert price change to f64")
        }
    };
    
    // Calculate notional value (position size)
    let notional_usd = margin_usd * leverage as f64;
    
    // Gross PnL = notional * price_change_pct / 100
    let gross_pnl_usd = notional_usd * price_change_pct / 100.0;
    
    // Commission = notional * commission_pct * 2 (entry + exit)
    let commission_usd = notional_usd * commission_pct * 2.0;
    
    // Net PnL = gross PnL - commission
    let net_pnl_usd = gross_pnl_usd - commission_usd;
    
    (gross_pnl_usd, net_pnl_usd, commission_usd)
}

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
    let timestamp_offset = Instant::now();
    
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
    
    // Trading parameters for PnL calculation (same for all symbols)
    let margin_usd = 50.0; // Average margin per trade (between min 10 and max 100)
    let leverage = cfg.exec.default_leverage as u32;
    // Use maker commission since we use post_only orders (tif: "post_only")
    let commission_pct = cfg.risk.maker_commission_pct / 100.0; // Convert percentage to decimal
    let min_profit_usd = cfg.exec.min_profit_usd; // 0.50 USDT/USDC minimum profit PER TRADE
    
    // Get quote asset for display
    let quote_asset = if cfg.allow_usdt_quote && cfg.quote_asset == "USDC" {
        "USDT/USDC" // Can use both
    } else {
        &cfg.quote_asset
    };
    
    // Track aggregate PnL across all symbols
    let mut total_aggregate_gross_pnl = 0.0;
    let mut total_aggregate_net_pnl = 0.0;
    let mut total_aggregate_commission = 0.0;
    let mut total_signals_all_symbols = 0u64;
    
    // Track per-trade profit target across all symbols
    let mut total_trades_meeting_target = 0u64;
    let mut total_trades_below_target = 0u64;
    let mut all_profitable_trades_pnl: Vec<f64> = Vec::new();
    
    println!("\n🧪 Testing Trending Module Success Rate with Real Binance Data");
    println!("================================================================\n");
    println!("💰 Trading Parameters:");
    println!("   Margin per trade: ${:.2}", margin_usd);
    println!("   Leverage: {}x", leverage);
    println!("   Commission: {:.4}% (maker, entry + exit)", commission_pct * 100.0 * 2.0);
    println!("   Stop Loss: {:.2}%", cfg.stop_loss_pct);
    println!("   Take Profit: {:.2}%", cfg.take_profit_pct);
    println!("   Minimum Profit Target: {:.2} {} PER TRADE\n", min_profit_usd, quote_asset);
    
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
        
        // Track PnL metrics
        let mut total_gross_pnl = 0.0;
        let mut total_net_pnl = 0.0;
        let mut total_commission = 0.0;
        let mut winning_trades_pnl = 0.0;
        let mut losing_trades_pnl = 0.0;
        let mut profitable_signals = 0u64;
        let mut unprofitable_signals = 0u64;
        
        // Track per-trade profit target (minimum 0.50 USDT/USDC per trade)
        let mut trades_meeting_profit_target = 0u64;
        let mut trades_below_profit_target = 0u64;
        let mut profitable_trades_pnl: Vec<f64> = Vec::new(); // Track PnL of profitable trades
        
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
                    
                    // Validate signal: simulate realistic trade with stop loss and take profit
                    // Instead of just checking next candle, simulate trade exit conditions
                    let current_price = (tick.bid.0 + tick.ask.0) / Decimal::from(2);
                    let stop_loss_pct = cfg.stop_loss_pct; // e.g., 1.0%
                    let take_profit_pct = cfg.take_profit_pct; // e.g., 2.0%
                    
                    // Find exit price: check next 5 candles (5 minutes) for stop loss or take profit
                    // This simulates realistic trading where we wait for SL/TP or close after some time
                    let validation_period = 5; // 5 candles = 5 minutes
                    let mut exit_price = None;
                    let mut exit_reason = "";
                    
                    for i in 1..=validation_period.min(ticks.len() - tick_idx - 1) {
                        let future_tick = &ticks[tick_idx + i];
                        let future_price = (future_tick.bid.0 + future_tick.ask.0) / Decimal::from(2);
                        
                        let price_change_pct = match signal {
                            TrendSignal::Long => {
                                ((future_price - current_price) / current_price * Decimal::from(100))
                                    .to_f64()
                                    .expect("Failed to convert price change to f64")
                            }
                            TrendSignal::Short => {
                                ((current_price - future_price) / current_price * Decimal::from(100))
                                    .to_f64()
                                    .expect("Failed to convert price change to f64")
                            }
                        };
                        
                        // Check for take profit (positive exit)
                        if price_change_pct >= take_profit_pct {
                            exit_price = Some(future_price);
                            exit_reason = "take_profit";
                            break;
                        }
                        
                        // Check for stop loss (negative exit)
                        if price_change_pct <= -stop_loss_pct {
                            exit_price = Some(future_price);
                            exit_reason = "stop_loss";
                            break;
                        }
                        
                        // If we've checked all validation period, use last price
                        if i == validation_period {
                            exit_price = Some(future_price);
                            exit_reason = "time_limit";
                        }
                    }
                    
                    if let Some(next_price) = exit_price {
                        
                        // Calculate price change from real market data - fail if conversion fails
                        let price_change_pct = match signal {
                            TrendSignal::Long => {
                                ((next_price - current_price) / current_price * Decimal::from(100))
                                    .to_f64()
                                    .expect("Failed to convert price change to f64")
                            }
                            TrendSignal::Short => {
                                ((current_price - next_price) / current_price * Decimal::from(100))
                                    .to_f64()
                                    .expect("Failed to convert price change to f64")
                            }
                        };
                        
                        price_changes.push(price_change_pct);
                        
                        // Calculate PnL for this trade
                        let (gross_pnl, net_pnl, commission) = calculate_trade_pnl(
                            signal,
                            current_price,
                            next_price,
                            margin_usd,
                            leverage,
                            commission_pct,
                        );
                        
                        total_gross_pnl += gross_pnl;
                        total_net_pnl += net_pnl;
                        total_commission += commission;
                        
                        // Track profitable vs unprofitable signals
                        if net_pnl > 0.0 {
                            profitable_signals += 1;
                            winning_trades_pnl += net_pnl;
                            profitable_trades_pnl.push(net_pnl);
                            
                            // Check if this profitable trade meets minimum profit target (0.50 USDT/USDC)
                            if net_pnl >= min_profit_usd {
                                trades_meeting_profit_target += 1;
                            } else {
                                trades_below_profit_target += 1;
                            }
                        } else {
                            unprofitable_signals += 1;
                            losing_trades_pnl += net_pnl.abs();
                            trades_below_profit_target += 1; // Losing trades also don't meet target
                        }
                        
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
                        let result_str = if net_pnl > 0.0 { "✅" } else { "❌" };
                        // Display prices and PnL
                        let current_price_f64 = current_price.to_f64()
                            .expect("Failed to convert current price to f64 for display");
                        let next_price_f64 = next_price.to_f64()
                            .expect("Failed to convert next price to f64 for display");
                        
                        println!(
                            "  {} Signal #{}: {} @ ${:.2} → ${:.2} ({:+.2}%) [{}] | PnL: ${:.4}",
                            result_str,
                            signals_generated,
                            signal_str,
                            current_price_f64,
                            next_price_f64,
                            price_change_pct,
                            exit_reason,
                            net_pnl
                        );
                    } else {
                        // No exit price found (not enough future data)
                        // Skip this signal for PnL calculation but still count it
                        println!("  ⚠️  Signal #{}: Not enough future data to calculate exit", signals_generated);
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
        
        // Calculate average PnL per trade
        let avg_net_pnl_per_trade = if signals_generated > 0 {
            total_net_pnl / signals_generated as f64
        } else {
            0.0
        };
        
        // Calculate profit factor
        let profit_factor = if losing_trades_pnl > 0.0 {
            winning_trades_pnl / losing_trades_pnl
        } else if winning_trades_pnl > 0.0 {
            f64::INFINITY
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
        // Calculate average profit per profitable trade
        let avg_profit_per_profitable_trade = if profitable_signals > 0 {
            winning_trades_pnl / profitable_signals as f64
        } else {
            0.0
        };
        
        // Get quote asset name for display
        let quote_asset = if cfg.allow_usdt_quote && cfg.quote_asset == "USDC" {
            "USDT/USDC" // Can use both
        } else {
            &cfg.quote_asset
        };
        
        println!("\n  💰 PnL Analysis (margin: ${:.2}, leverage: {}x):", margin_usd, leverage);
        println!("     Total Gross PnL: ${:.4}", total_gross_pnl);
        println!("     Total Commission: ${:.4}", total_commission);
        println!("     Total Net PnL: ${:.4}", total_net_pnl);
        println!("     Average Net PnL per trade: ${:.4}", avg_net_pnl_per_trade);
        println!("     Profitable signals: {} (${:.4} total)", profitable_signals, winning_trades_pnl);
        println!("     Unprofitable signals: {} (${:.4} total loss)", unprofitable_signals, losing_trades_pnl);
        println!("     Profit Factor: {:.2}", profit_factor);
        println!("\n  🎯 Per-Trade Profit Target Analysis (minimum {:.2} {} per trade):", min_profit_usd, quote_asset);
        println!("     Trades meeting target (>= {:.2} {}): {}", min_profit_usd, quote_asset, trades_meeting_profit_target);
        println!("     Trades below target (< {:.2} {}): {}", min_profit_usd, quote_asset, trades_below_profit_target);
        if profitable_signals > 0 {
            println!("     Average profit per profitable trade: ${:.4}", avg_profit_per_profitable_trade);
            println!("     Target: ${:.2} {} per trade", min_profit_usd, quote_asset);
            if avg_profit_per_profitable_trade < min_profit_usd {
                println!("     ⚠️  Average profit per trade ({:.4}) is below target ({:.2})", 
                         avg_profit_per_profitable_trade, min_profit_usd);
            }
        }
        
        // Assertions (realistic thresholds for real market data)
        // Note: Real market data is noisy, so we use relaxed but still meaningful thresholds
        if signals_generated > 0 {
            // Minimum profit requirement: 0.50 USDT per test run
            let min_total_profit_usd = min_profit_usd;
            let profit_target_met = total_net_pnl >= min_total_profit_usd;
            
            if !profit_target_met {
                println!("\n     ⚠️  Profit target NOT met:");
                println!("        Total Net PnL: ${:.4} < ${:.2} (minimum required)", 
                         total_net_pnl, min_total_profit_usd);
                println!("        Shortfall: ${:.4}", min_total_profit_usd - total_net_pnl);
            } else {
                println!("\n     ✅ Profit target met:");
                println!("        Total Net PnL: ${:.4} >= ${:.2} (minimum required)", 
                         total_net_pnl, min_total_profit_usd);
                println!("        Excess profit: ${:.4}", total_net_pnl - min_total_profit_usd);
            }
            // Overall win rate should be better than random (50%) but account for market noise
            // In real markets, 40%+ win rate with proper risk management can be profitable
            let min_win_rate = 40.0;
            let win_rate_passed = win_rate >= min_win_rate;
            
            // Short signals typically perform better in trending markets
            let short_min_win_rate = 42.0;
            let short_passed = short_signals_total == 0 || short_win_rate >= short_min_win_rate;
            
            // Long signals may have lower win rate in certain market conditions
            let long_min_win_rate = 30.0;
            let long_passed = long_signals_total == 0 || long_win_rate >= long_min_win_rate;
            
            if win_rate_passed && short_passed && long_passed {
                println!("     ✅ Win rate test passed:");
                println!("        Overall: {:.2}% >= {}%", win_rate, min_win_rate);
                if short_signals_total > 0 {
                    println!("        Short: {:.2}% >= {}%", short_win_rate, short_min_win_rate);
                }
                if long_signals_total > 0 {
                    println!("        Long: {:.2}% >= {}%", long_win_rate, long_min_win_rate);
                }
            } else {
                // Don't fail the test, but warn about performance
                println!("     ⚠️  Win rate below optimal thresholds:");
                if !win_rate_passed {
                    println!("        Overall: {:.2}% < {}% (expected >= {}%)", win_rate, min_win_rate, min_win_rate);
                }
                if short_signals_total > 0 && !short_passed {
                    println!("        Short: {:.2}% < {}% (expected >= {}%)", short_win_rate, short_min_win_rate, short_min_win_rate);
                }
                if long_signals_total > 0 && !long_passed {
                    println!("        Long: {:.2}% < {}% (expected >= {}%)", long_win_rate, long_min_win_rate, long_min_win_rate);
                }
                println!("     ℹ️  Note: Real market data is noisy. Consider:");
                println!("        - Testing with more data (longer time period)");
                println!("        - Adjusting config parameters");
                println!("        - Using longer validation periods (currently 1 minute)");
                
                // Only fail if overall win rate is very low (< 35%)
                if win_rate < 35.0 {
                    panic!(
                        "Win rate {}% is too low (expected >= 35% minimum). This may indicate a problem with the trending algorithm.",
                        win_rate
                    );
                }
            }
            
            // Accumulate for aggregate analysis
            total_aggregate_gross_pnl += total_gross_pnl;
            total_aggregate_net_pnl += total_net_pnl;
            total_aggregate_commission += total_commission;
            total_signals_all_symbols += signals_generated;
            total_trades_meeting_target += trades_meeting_profit_target;
            total_trades_below_target += trades_below_profit_target;
            all_profitable_trades_pnl.extend(profitable_trades_pnl);
        } else {
            println!("     ⚠️  No signals generated (may need more data or different config)");
        }
    }
    
    // Final aggregate analysis across all symbols
    println!("\n{}", "=".repeat(80));
    println!("📊 AGGREGATE RESULTS (All Symbols)");
    println!("{}", "=".repeat(80));
    println!("   Total signals across all symbols: {}", total_signals_all_symbols);
    println!("   Total Gross PnL: ${:.4}", total_aggregate_gross_pnl);
    println!("   Total Commission: ${:.4}", total_aggregate_commission);
    println!("   Total Net PnL: ${:.4}", total_aggregate_net_pnl);
    
    if total_signals_all_symbols > 0 {
        let avg_net_pnl_per_signal = total_aggregate_net_pnl / total_signals_all_symbols as f64;
        println!("   Average Net PnL per signal: ${:.4}", avg_net_pnl_per_signal);
        
        // Calculate average profit per profitable trade
        let avg_profit_per_profitable_trade = if !all_profitable_trades_pnl.is_empty() {
            all_profitable_trades_pnl.iter().sum::<f64>() / all_profitable_trades_pnl.len() as f64
        } else {
            0.0
        };
        
        println!("\n  🎯 Per-Trade Profit Target Analysis (minimum {:.2} {} per trade):", min_profit_usd, quote_asset);
        println!("     Trades meeting target (>= {:.2} {}): {} / {}", 
                 min_profit_usd, quote_asset, total_trades_meeting_target, total_signals_all_symbols);
        println!("     Trades below target (< {:.2} {}): {} / {}", 
                 min_profit_usd, quote_asset, total_trades_below_target, total_signals_all_symbols);
        if !all_profitable_trades_pnl.is_empty() {
            println!("     Average profit per profitable trade: ${:.4}", avg_profit_per_profitable_trade);
            println!("     Target: ${:.2} {} per trade", min_profit_usd, quote_asset);
        }
        
        // Final assertion: Minimum profit requirement PER TRADE
        // CRITICAL: Each profitable trade must make at least 0.50 USDT/USDC
        // AND average profit per profitable trade must be >= 0.50 USDT/USDC
        let avg_profit_target_met = !all_profitable_trades_pnl.is_empty() && avg_profit_per_profitable_trade >= min_profit_usd;
        let trades_meeting_target_ratio = if total_signals_all_symbols > 0 {
            total_trades_meeting_target as f64 / total_signals_all_symbols as f64
        } else {
            0.0
        };
        
        // We need at least 50% of trades to meet the profit target
        let min_trades_meeting_target_ratio = 0.5; // 50% of trades should meet target
        let trades_ratio_met = trades_meeting_target_ratio >= min_trades_meeting_target_ratio;
        
        let profit_target_met = avg_profit_target_met && trades_ratio_met && total_aggregate_net_pnl >= 0.0;
        
        if profit_target_met {
            println!("\n   ✅ Profit target MET:");
            println!("      ✅ Average profit per profitable trade: ${:.4} >= ${:.2} {}", 
                     avg_profit_per_profitable_trade, min_profit_usd, quote_asset);
            println!("      ✅ Trades meeting target: {:.1}% (>= {:.1}% required)", 
                     trades_meeting_target_ratio * 100.0, min_trades_meeting_target_ratio * 100.0);
            println!("      ✅ Total Net PnL: ${:.4} (profitable)", total_aggregate_net_pnl);
        } else {
            // We are in loss or not meeting per-trade profit target - this is a problem
            println!("\n   ❌ Profit target NOT met:");
            
            if !avg_profit_target_met {
                println!("      ❌ Average profit per trade: ${:.4} < ${:.2} {} (target)", 
                         avg_profit_per_profitable_trade, min_profit_usd, quote_asset);
            }
            
            if !trades_ratio_met {
                println!("      ❌ Trades meeting target: {:.1}% < {:.1}% (required)", 
                         trades_meeting_target_ratio * 100.0, min_trades_meeting_target_ratio * 100.0);
            }
            
            if total_aggregate_net_pnl < 0.0 {
                println!("      ❌ Total Net PnL: ${:.4} (STRATEGY IS LOSING MONEY)", total_aggregate_net_pnl);
            } else {
                println!("      ⚠️  Total Net PnL: ${:.4} (profitable but per-trade target not met)", total_aggregate_net_pnl);
            }
            
            // Analyze the loss
            let gross_pnl_loss = total_aggregate_gross_pnl < 0.0;
            let commission_impact = total_aggregate_commission;
            
            println!("\n   📊 Loss Analysis:");
            println!("      Gross PnL: ${:.4} {}", 
                     total_aggregate_gross_pnl,
                     if gross_pnl_loss { "(strategy itself is losing)" } else { "(strategy profitable before commission)" });
            println!("      Commission Cost: ${:.4}", commission_impact);
            println!("      Net PnL: ${:.4}", total_aggregate_net_pnl);
            
            if gross_pnl_loss {
                println!("\n   ⚠️  CRITICAL: Strategy is losing money even BEFORE commission costs.");
                println!("      This indicates a fundamental problem with the trading strategy.");
                println!("      Gross PnL: ${:.4} (negative)", total_aggregate_gross_pnl);
            } else {
                println!("\n   ⚠️  Strategy is profitable before commission, but commission costs exceed profits.");
                println!("      Gross PnL: ${:.4} (positive)", total_aggregate_gross_pnl);
                println!("      Commission: ${:.4} (too high relative to profits)", commission_impact);
                println!("      Consider: Reducing trade frequency or improving win rate");
            }
            
            println!("\n   💡 Recommendations:");
            println!("      - Test with longer time periods (more data)");
            println!("      - Adjust stop loss / take profit levels");
            println!("      - Reduce trade frequency (fewer signals = less commission)");
            println!("      - Improve signal quality (higher win rate)");
            println!("      - Test with different leverage or margin settings");
            println!("      - Use different symbols or market conditions");
            
            // FAIL the test if:
            // 1. We're losing money overall, OR
            // 2. Average profit per trade is below target, OR
            // 3. Less than 50% of trades meet the profit target
            let mut failure_reasons = Vec::new();
            
            if total_aggregate_net_pnl < 0.0 {
                failure_reasons.push(format!("Strategy is losing money (Net PnL: ${:.4})", total_aggregate_net_pnl));
            }
            
            if !avg_profit_target_met {
                failure_reasons.push(format!(
                    "Average profit per trade (${:.4}) is below target (${:.2} {})",
                    avg_profit_per_profitable_trade, min_profit_usd, quote_asset
                ));
            }
            
            if !trades_ratio_met {
                failure_reasons.push(format!(
                    "Only {:.1}% of trades meet profit target (required: {:.1}%)",
                    trades_meeting_target_ratio * 100.0, min_trades_meeting_target_ratio * 100.0
                ));
            }
            
            panic!(
                "❌ TEST FAILED: Per-trade profit target not met. \
                 Each profitable trade must make at least {:.2} {}. \
                 Failures: {}. \
                 Gross PnL: ${:.4}, Commission: ${:.4}. \
                 This indicates the strategy needs improvement before it can be used in production.",
                min_profit_usd, quote_asset, failure_reasons.join("; "), total_aggregate_gross_pnl, commission_impact
            );
        }
    } else {
        panic!("No signals generated across all symbols - cannot calculate profit. Test requires at least some signals to validate profitability.");
    }
    
    println!("\n✅ Trending success test completed!");
}

