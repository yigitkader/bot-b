// ✅ ADIM 6: Risk & portföy yönetimi (TrendPlan.md)
// Portföy bazlı limitler: toplam notional, coin başına limit, korelasyon kontrolü

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::types::{PositionUpdate, Side};

#[derive(Debug, Clone)]
pub struct PositionInfo {
    pub symbol: String,
    pub side: Side,
    pub entry_price: f64,
    pub size: f64,
    pub leverage: f64,
    pub notional_usd: f64, // size * entry_price * leverage
}

#[derive(Debug, Clone)]
pub struct RiskLimits {
    pub max_total_notional_usd: f64,      // Toplam açık pozisyon notional limiti
    pub max_position_per_symbol_usd: f64, // Coin başına maksimum notional
    pub max_correlated_positions: usize,  // Aynı direction'da maksimum korele coin sayısı
    pub max_quote_exposure_usd: f64,      // USDT/USDC bazlı toplam risk
}

pub struct RiskManager {
    limits: RiskLimits,
    positions: Arc<RwLock<HashMap<String, PositionInfo>>>, // symbol -> position
}

impl RiskManager {
    pub fn new(limits: RiskLimits) -> Self {
        Self {
            limits,
            positions: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Update position (called when position opens/closes)
    pub async fn update_position(&self, update: &PositionUpdate) {
        let mut positions = self.positions.write().await;
        
        if update.is_closed {
            positions.remove(&update.symbol);
            info!("RISK_MANAGER: Position closed for {}", update.symbol);
        } else {
            // Calculate notional
            let notional = update.size * update.entry_price * update.leverage;
            
            positions.insert(
                update.symbol.clone(),
                PositionInfo {
                    symbol: update.symbol.clone(),
                    side: update.side,
                    entry_price: update.entry_price,
                    size: update.size,
                    leverage: update.leverage,
                    notional_usd: notional,
                },
            );
            info!("RISK_MANAGER: Position updated for {}: notional={:.2} USD", update.symbol, notional);
        }
    }

    /// Check if new position is allowed (risk checks)
    pub async fn can_open_position(
        &self,
        symbol: &str,
        side: Side,
        entry_price: f64,
        size: f64,
        leverage: f64,
    ) -> (bool, String) {
        let positions = self.positions.read().await;
        
        // Calculate new position notional
        let new_notional = size * entry_price * leverage;
        
        // 1. Check total notional limit
        let total_notional: f64 = positions.values().map(|p| p.notional_usd).sum();
        let new_total = total_notional + new_notional;
        
        if new_total > self.limits.max_total_notional_usd {
            return (
                false,
                format!(
                    "Total notional would exceed limit: {:.2} > {:.2} USD",
                    new_total, self.limits.max_total_notional_usd
                ),
            );
        }
        
        // 2. Check per-symbol limit
        if let Some(existing) = positions.get(symbol) {
            let new_symbol_notional = existing.notional_usd + new_notional;
            if new_symbol_notional > self.limits.max_position_per_symbol_usd {
                return (
                    false,
                    format!(
                        "Symbol {} notional would exceed limit: {:.2} > {:.2} USD",
                        symbol, new_symbol_notional, self.limits.max_position_per_symbol_usd
                    ),
                );
            }
        } else if new_notional > self.limits.max_position_per_symbol_usd {
            return (
                false,
                format!(
                    "New position notional exceeds per-symbol limit: {:.2} > {:.2} USD",
                    new_notional, self.limits.max_position_per_symbol_usd
                ),
            );
        }
        
        // 3. Check correlated positions (same direction)
        let same_direction_count = positions
            .values()
            .filter(|p| p.side == side)
            .count();
        
        if same_direction_count >= self.limits.max_correlated_positions {
            return (
                false,
                format!(
                    "Too many {} positions: {} >= {} (max)",
                    match side {
                        Side::Long => "LONG",
                        Side::Short => "SHORT",
                    },
                    same_direction_count + 1,
                    self.limits.max_correlated_positions
                ),
            );
        }
        
        // 4. Check quote exposure (USDT/USDC)
        // For simplicity, assume all positions are in USDT/USDC quote
        // In reality, you'd need to track quote asset per symbol
        let quote_exposure = new_total; // Simplified: total notional = quote exposure
        if quote_exposure > self.limits.max_quote_exposure_usd {
            return (
                false,
                format!(
                    "Quote exposure would exceed limit: {:.2} > {:.2} USD",
                    quote_exposure, self.limits.max_quote_exposure_usd
                ),
            );
        }
        
        // All checks passed
        (true, String::new())
    }

    /// Get current portfolio statistics
    pub async fn get_portfolio_stats(&self) -> PortfolioStats {
        let positions = self.positions.read().await;
        
        let total_notional: f64 = positions.values().map(|p| p.notional_usd).sum();
        let long_count = positions.values().filter(|p| p.side == Side::Long).count();
        let short_count = positions.values().filter(|p| p.side == Side::Short).count();
        let long_notional: f64 = positions
            .values()
            .filter(|p| p.side == Side::Long)
            .map(|p| p.notional_usd)
            .sum();
        let short_notional: f64 = positions
            .values()
            .filter(|p| p.side == Side::Short)
            .map(|p| p.notional_usd)
            .sum();
        
        PortfolioStats {
            total_positions: positions.len(),
            long_positions: long_count,
            short_positions: short_count,
            total_notional_usd: total_notional,
            long_notional_usd: long_notional,
            short_notional_usd: short_notional,
            symbols: positions.keys().cloned().collect(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PortfolioStats {
    pub total_positions: usize,
    pub long_positions: usize,
    pub short_positions: usize,
    pub total_notional_usd: f64,
    pub long_notional_usd: f64,
    pub short_notional_usd: f64,
    pub symbols: Vec<String>,
}

impl RiskLimits {
    pub fn from_config(max_total_notional: f64) -> Self {
        Self {
            max_total_notional_usd: max_total_notional,
            max_position_per_symbol_usd: max_total_notional * 0.3, // 30% of total per symbol
            max_correlated_positions: 5, // Max 5 positions in same direction
            max_quote_exposure_usd: max_total_notional * 1.2, // 120% of total (allows some margin)
        }
    }
}

