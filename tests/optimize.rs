// Strategy optimization - systematic parameter testing
// Tests different combinations to find optimal parameters

use std::process::Command;
use std::fs;
use serde_json::json;

#[tokio::main]
async fn main() {
    println!("🔍 Starting systematic strategy optimization...\n");
    
    // Parameter ranges to test
    let base_scores = vec![4.5, 4.7, 4.9, 5.0, 5.1];
    let trend_strengths = vec![0.5, 0.6, 0.65, 0.7];
    let sl_multipliers = vec![1.5, 2.0];
    let tp_multipliers = vec![4.0, 4.5, 5.0];
    
    let mut best_result: Option<(f64, f64, f64, f64, f64, f64)> = None; // (base_score, trend_strength, sl_mult, tp_mult, win_rate, pnl)
    let mut results = Vec::new();
    
    println!("Testing {} combinations...\n", 
        base_scores.len() * trend_strengths.len() * sl_multipliers.len() * tp_multipliers.len());
    
    for &base_score in &base_scores {
        for &trend_strength in &trend_strengths {
            for &sl_mult in &sl_multipliers {
                for &tp_mult in &tp_multipliers {
                    println!("Testing: BASE_MIN_SCORE={}, trend_strength={}, SL={}x, TP={}x", 
                        base_score, trend_strength, sl_mult, tp_mult);
                    
                    // Modify trending.rs with these parameters
                    modify_trending_params(base_score, trend_strength, sl_mult, tp_mult).await;
                    
                    // Run backtest
                    let result = run_backtest().await;
                    
                    let win_rate = result.0;
                    let pnl = result.1;
                    
                    results.push((base_score, trend_strength, sl_mult, tp_mult, win_rate, pnl));
                    
                    // Track best result (prioritize win_rate > 50% and positive PnL)
                    if win_rate >= 0.50 && pnl > 0.0 {
                        if best_result.is_none() || win_rate > best_result.unwrap().4 || 
                           (win_rate == best_result.unwrap().4 && pnl > best_result.unwrap().5) {
                            best_result = Some((base_score, trend_strength, sl_mult, tp_mult, win_rate, pnl));
                        }
                    }
                    
                    println!("  Result: Win Rate={:.2}%, PnL=${:.2}\n", win_rate * 100.0, pnl);
                }
            }
        }
    }
    
    // Print best result
    if let Some(best) = best_result {
        println!("\n🏆 BEST OPTIMIZATION FOUND:");
        println!("BASE_MIN_SCORE: {}", best.0);
        println!("Trend Strength: {}", best.1);
        println!("SL Multiplier: {}x", best.2);
        println!("TP Multiplier: {}x", best.3);
        println!("Win Rate: {:.2}%", best.4 * 100.0);
        println!("PnL: ${:.2}", best.5);
    } else {
        println!("\n⚠️  No combination achieved 50%+ win rate with positive PnL");
        // Find best by PnL
        if let Some(best) = results.iter().max_by(|a, b| a.5.partial_cmp(&b.5).unwrap()) {
            println!("\n🏆 BEST BY PnL:");
            println!("BASE_MIN_SCORE: {}", best.0);
            println!("Trend Strength: {}", best.1);
            println!("SL Multiplier: {}x", best.2);
            println!("TP Multiplier: {}x", best.3);
            println!("Win Rate: {:.2}%", best.4 * 100.0);
            println!("PnL: ${:.2}", best.5);
        }
    }
}

async fn modify_trending_params(base_score: f64, trend_strength: f64, sl_mult: f64, tp_mult: f64) {
    // This would modify trending.rs - for now, we'll need to do it manually or via sed
    // In production, we'd make these configurable
}

async fn run_backtest() -> (f64, f64) {
    // Run cargo test and parse results
    let output = Command::new("cargo")
        .args(&["test", "--test", "backtest", "test_strategy_with_multiple_binance_symbols", "--", "--ignored", "--nocapture"])
        .output()
        .expect("Failed to run backtest");
    
    let stdout = String::from_utf8_lossy(&output.stdout);
    
    // Parse win rate and PnL from output
    let win_rate = parse_win_rate(&stdout);
    let pnl = parse_pnl(&stdout);
    
    (win_rate, pnl)
}

fn parse_win_rate(output: &str) -> f64 {
    // Parse "Aggregate Win Rate: XX.XX%"
    if let Some(line) = output.lines().find(|l| l.contains("Aggregate Win Rate")) {
        if let Some(rate_str) = line.split(":").nth(1) {
            if let Ok(rate) = rate_str.trim().trim_end_matches('%').parse::<f64>() {
                return rate / 100.0;
            }
        }
    }
    0.0
}

fn parse_pnl(output: &str) -> f64 {
    // Parse "Total PnL (All Symbols): $XX.XX"
    if let Some(line) = output.lines().find(|l| l.contains("Total PnL (All Symbols)")) {
        if let Some(pnl_str) = line.split("$").nth(1) {
            if let Ok(pnl) = pnl_str.trim().parse::<f64>() {
                return pnl;
            }
        }
    }
    0.0
}

