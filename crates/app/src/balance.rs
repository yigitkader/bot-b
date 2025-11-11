//location: /crates/app/src/balance.rs
// Balance Module: Check balances, get symbols with balance, get current prices

use crate::types::*;
use crate::connection::{BinanceFutures, Venue};
use std::collections::HashMap;
use rust_decimal::Decimal;
use tracing::{info, warn};

/// Check balances for all quote assets
pub async fn check_balances(
    venue: &BinanceFutures,
    quote_assets: &[String],
) -> HashMap<String, f64> {
    let mut balances = HashMap::new();
    for quote_asset in quote_assets {
        match venue.available_balance(quote_asset).await {
            Ok(balance) => {
                let balance_f64 = balance.to_f64().unwrap_or(0.0);
                balances.insert(quote_asset.clone(), balance_f64);
                info!(quote_asset = %quote_asset, balance = balance_f64, "balance checked");
            }
            Err(e) => {
                warn!(quote_asset = %quote_asset, error = %e, "failed to fetch balance");
                balances.insert(quote_asset.clone(), 0.0);
            }
        }
    }
    balances
}

/// Get symbols that match our balance (have enough balance to trade)
pub fn get_symbols_matched_with_our_balance(
    symbols: &[String],
    balances: &HashMap<String, f64>,
    min_balance: f64,
) -> Vec<String> {
    symbols
        .iter()
        .filter(|symbol| {
            // Extract quote asset from symbol (e.g., BTCUSDC -> USDC)
            if let Some(quote) = extract_quote_asset(symbol) {
                balances.get(&quote).map_or(false, |&balance| balance >= min_balance)
            } else {
                false
            }
        })
        .cloned()
        .collect()
}

/// Get current prices for symbols (loop: 1-2 sec)
pub async fn get_current_symbol_prices(
    venue: &BinanceFutures,
    symbols: &[String],
) -> HashMap<String, (Decimal, Decimal)> {
    let mut prices = HashMap::new();
    
    for symbol in symbols {
        match venue.best_prices(symbol).await {
            Ok((bid, ask)) => {
                prices.insert(symbol.clone(), (bid.0, ask.0));
            }
            Err(e) => {
                warn!(symbol = %symbol, error = %e, "failed to fetch prices");
            }
        }
        // Small delay to avoid rate limiting
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
    
    prices
}

/// Extract quote asset from symbol (e.g., BTCUSDC -> USDC)
fn extract_quote_asset(symbol: &str) -> Option<String> {
    let quote_assets = ["USDC", "USDT", "BUSD", "BTC", "ETH"];
    quote_assets.iter()
        .find(|quote| symbol.ends_with(*quote))
        .map(|q| q.to_string())
}

