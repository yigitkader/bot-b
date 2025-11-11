//location: /crates/app/src/order.rs
// Order Module: Create orders, check positions, close positions, save PnL

use crate::types::*;
use crate::connection::{BinanceFutures, VenueTrait};
use crate::utils::{rate_limit_guard, calc_net_pnl_usd};
use anyhow::Result;
use rust_decimal::Decimal;
use std::time::Instant;
use tracing::info;

/// Create open long order (buy)
pub async fn create_open_long_order(
    venue: &BinanceFutures,
    symbol: &str,
    price: Decimal,
    qty: Decimal,
    tif: Tif,
) -> Result<String> {
    rate_limit_guard(1).await;
    
    let order_id = venue.place_limit(symbol, Side::Buy, Px(price), Qty(qty), tif)
        .await?;
    
            info!(
        symbol = %symbol,
                order_id = %order_id,
        side = "LONG",
        price = %price,
        qty = %qty,
        "long order created"
    );
    
    Ok(order_id)
}

/// Handle open short order (sell)
pub async fn handle_open_short_order(
    venue: &BinanceFutures,
    symbol: &str,
    price: Decimal,
    qty: Decimal,
    tif: Tif,
) -> Result<String> {
    rate_limit_guard(1).await;
    
    let order_id = venue.place_limit(symbol, Side::Sell, Px(price), Qty(qty), tif)
        .await?;
    
    info!(
        symbol = %symbol,
        order_id = %order_id,
        side = "SHORT",
        price = %price,
        qty = %qty,
        "short order created"
    );
    
    Ok(order_id)
}

/// Check current position for a symbol
pub async fn check_position(
    venue: &BinanceFutures,
    symbol: &str,
) -> Result<Position> {
    rate_limit_guard(5).await;
    
    let position = venue.get_position(symbol).await?;
    
                        info!(
        symbol = %symbol,
        qty = %position.qty.0,
        entry = %position.entry.0,
        leverage = position.leverage,
        "position checked"
    );
    
    Ok(position)
}

/// Close position (market order to close)
pub async fn close_position(
    venue: &BinanceFutures,
    symbol: &str,
    position: &Position,
) -> Result<()> {
    if position.qty.0.is_zero() {
        return Ok(()); // No position to close
    }
    
    rate_limit_guard(1).await;
    
    // Determine side: if qty is positive, we need to sell (close long)
    // if qty is negative, we need to buy (close short)
    let side = if position.qty.0.is_sign_positive() {
        Side::Sell
    } else {
        Side::Buy
    };
    
    // Use market order to close quickly
    let qty = position.qty.0.abs();
    
    // Get current price for market order
    let (bid, ask) = venue.best_prices(symbol).await?;
    let market_price = if side == Side::Sell {
        bid.0 // Sell at bid
    } else {
        ask.0 // Buy at ask
    };
    
    // Place market order (IOC = Immediate or Cancel)
    let _order_id = venue.place_limit(symbol, side, Px(market_price), Qty(qty), Tif::Ioc)
        .await?;

    info!(
        symbol = %symbol,
        side = ?side,
        qty = %qty,
        price = %market_price,
        "position closed"
    );
    
    Ok(())
}

/// Save profits and losses (track PnL)
pub struct PnLTracker {
    trades: Vec<TradeRecord>,
}

#[derive(Debug, Clone)]
struct TradeRecord {
    symbol: String,
    entry_price: Decimal,
    exit_price: Decimal,
    qty: Decimal,
    side: Side,
    pnl_usd: f64,
    timestamp: Instant,
}

impl PnLTracker {
    pub fn new() -> Self {
        Self {
            trades: Vec::new(),
        }
    }
    
    /// Record a closed trade
    pub fn record_trade(
        &mut self,
        symbol: &str,
        entry_price: Decimal,
        exit_price: Decimal,
        qty: Decimal,
        side: Side,
        entry_fee_bps: f64,
        exit_fee_bps: f64,
    ) {
        let pnl = calc_net_pnl_usd(
            entry_price,
            exit_price,
            qty,
            &side,
            entry_fee_bps,
            exit_fee_bps,
        );
        
        let record = TradeRecord {
            symbol: symbol.to_string(),
            entry_price,
            exit_price,
            qty,
            side,
            pnl_usd: pnl,
            timestamp: Instant::now(),
        };
        
        self.trades.push(record);
        
        // Keep only last 1000 trades
        if self.trades.len() > 1000 {
            self.trades.remove(0);
        }
        
        info!(
            symbol = %symbol,
            pnl_usd = pnl,
            side = ?side,
            "trade PnL recorded"
        );
    }
    
    /// Get total PnL
    pub fn get_total_pnl(&self) -> f64 {
        self.trades.iter().map(|t| t.pnl_usd).sum()
    }
    
    /// Get win rate
    pub fn get_win_rate(&self) -> f64 {
        if self.trades.is_empty() {
            return 0.0;
        }
        let wins = self.trades.iter().filter(|t| t.pnl_usd > 0.0).count();
        wins as f64 / self.trades.len() as f64
    }
    
    /// Get summary statistics
    pub fn get_summary(&self) -> PnLSummary {
        let total_pnl = self.get_total_pnl();
        let win_rate = self.get_win_rate();
        let profitable_trades = self.trades.iter().filter(|t| t.pnl_usd > 0.0).count();
        let losing_trades = self.trades.len() - profitable_trades;
        
        let total_profit: f64 = self.trades.iter()
            .filter(|t| t.pnl_usd > 0.0)
            .map(|t| t.pnl_usd)
            .sum();
        
        let total_loss: f64 = self.trades.iter()
            .filter(|t| t.pnl_usd < 0.0)
            .map(|t| t.pnl_usd.abs())
            .sum();
        
        PnLSummary {
            total_trades: self.trades.len(),
            profitable_trades,
            losing_trades,
            total_pnl,
            total_profit,
            total_loss,
            win_rate,
        }
    }
}

#[derive(Debug)]
pub struct PnLSummary {
    pub total_trades: usize,
    pub profitable_trades: usize,
    pub losing_trades: usize,
    pub total_pnl: f64,
    pub total_profit: f64,
    pub total_loss: f64,
    pub win_rate: f64,
}

/// Save profits and losses (wrapper function)
pub fn save_profits_and_loses(
    tracker: &mut PnLTracker,
    symbol: &str,
    entry_price: Decimal,
    exit_price: Decimal,
    qty: Decimal,
    side: Side,
    entry_fee_bps: f64,
    exit_fee_bps: f64,
) {
    tracker.record_trade(
                        symbol,
        entry_price,
        exit_price,
        qty,
                        side,
        entry_fee_bps,
        exit_fee_bps,
    );
}

