// ✅ ADIM 6: Risk & portföy yönetimi (TrendPlan.md)
// Portföy bazlı limitler: toplam notional, coin başına limit, korelasyon kontrolü

use log::info;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

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
    pub max_total_notional_usd: f64, // Toplam açık pozisyon notional limiti
    pub max_position_per_symbol_usd: f64, // Coin başına maksimum notional
    pub max_correlated_positions: usize, // Aynı direction'da maksimum korele coin sayısı
    pub max_quote_exposure_usd: f64, // USDT/USDC bazlı toplam risk
    pub max_daily_drawdown_pct: f64, // Maximum daily drawdown (e.g., 0.05 = 5%)
    pub max_weekly_drawdown_pct: f64, // Maximum weekly drawdown (e.g., 0.10 = 10%)
    pub risk_per_trade_pct: f64, // Risk per trade as % of equity (e.g., 0.01 = 1%)
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

    pub fn limits(&self) -> &RiskLimits {
        &self.limits
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
            info!(
                "RISK_MANAGER: Position updated for {}: notional={:.2} USD",
                update.symbol, notional
            );
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

        // 3. ✅ CRITICAL FIX: Check correlated positions (real correlation, not just same direction)
        // Extract base asset from symbol (e.g., "BTCUSDT" -> "BTC")
        let new_base_asset = extract_base_asset(symbol);
        
        // Find correlated positions (same correlation group and same direction)
        let correlated_count = positions
            .values()
            .filter(|p| {
                if p.side != side {
                    return false; // Different direction, not correlated risk
                }
                let existing_base = extract_base_asset(&p.symbol);
                are_correlated(&new_base_asset, &existing_base)
            })
            .count();

        if correlated_count >= self.limits.max_correlated_positions {
            return (
                false,
                format!(
                    "Too many correlated {} positions: {} >= {} (max). New: {}, Existing correlated: {}",
                    match side {
                        Side::Long => "LONG",
                        Side::Short => "SHORT",
                    },
                    correlated_count + 1,
                    self.limits.max_correlated_positions,
                    symbol,
                    positions
                        .values()
                        .filter(|p| {
                            if p.side != side {
                                return false;
                            }
                            let existing_base = extract_base_asset(&p.symbol);
                            are_correlated(&new_base_asset, &existing_base)
                        })
                        .map(|p| p.symbol.clone())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
        }

        // ✅ ADDITIONAL: Check correlated notional (aggregate risk from correlated positions)
        let correlated_notional: f64 = positions
            .values()
            .filter(|p| {
                if p.side != side {
                    return false;
                }
                let existing_base = extract_base_asset(&p.symbol);
                are_correlated(&new_base_asset, &existing_base)
            })
            .map(|p| p.notional_usd)
            .sum();
        
        let max_correlated_notional = self.limits.max_total_notional_usd * 0.5; // Max 50% in correlated positions
        if correlated_notional + new_notional > max_correlated_notional {
            return (
                false,
                format!(
                    "Correlated positions notional would exceed limit: {:.2} > {:.2} USD. New: {}, Correlated group: {}",
                    correlated_notional + new_notional,
                    max_correlated_notional,
                    symbol,
                    positions
                        .values()
                        .filter(|p| {
                            if p.side != side {
                                return false;
                            }
                            let existing_base = extract_base_asset(&p.symbol);
                            are_correlated(&new_base_asset, &existing_base)
                        })
                        .map(|p| p.symbol.clone())
                        .collect::<Vec<_>>()
                        .join(", ")
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

    /// Calculate position size based on ATR and risk per trade
    /// Returns: (size_usdt, stop_distance_quote)
    pub fn calculate_position_size(
        &self,
        equity: f64,
        atr_value: f64,
        entry_price: f64,
        atr_sl_multiplier: f64,
    ) -> (f64, f64) {
        // Risk per trade in USD
        let risk_per_trade_usd = equity * self.limits.risk_per_trade_pct;

        // ✅ CRITICAL FIX: Use ATR/Price ratio instead of absolute ATR
        // This ensures consistent position sizing across different price levels
        // Example: BTC $40k (ATR $500 = 1.25%) vs Altcoin $0.001 (ATR $0.00005 = 5%)
        if atr_value <= 0.0 || entry_price <= 0.0 {
            return (0.0, 0.0);
        }

        let atr_ratio = atr_value / entry_price;
        // Stop distance as percentage of entry price
        let stop_distance_pct = atr_ratio * atr_sl_multiplier;
        // Stop distance in quote currency (for logging/display)
        let stop_distance_quote = entry_price * stop_distance_pct;

        // Position size: risk / stop_distance_pct
        // This ensures that if price moves stop_distance_pct, we lose exactly risk_per_trade_usd
        let size_usdt = risk_per_trade_usd / stop_distance_pct;

        (size_usdt, stop_distance_quote)
    }

    /// Check if drawdown limits are breached
    pub fn check_drawdown_limits(
        &self,
        daily_dd_pct: f64,
        weekly_dd_pct: f64,
    ) -> (bool, String) {
        if daily_dd_pct > self.limits.max_daily_drawdown_pct {
            return (
                false,
                format!(
                    "Daily drawdown limit breached: {:.2}% > {:.2}%",
                    daily_dd_pct * 100.0,
                    self.limits.max_daily_drawdown_pct * 100.0
                ),
            );
        }

        if weekly_dd_pct > self.limits.max_weekly_drawdown_pct {
            return (
                false,
                format!(
                    "Weekly drawdown limit breached: {:.2}% > {:.2}%",
                    weekly_dd_pct * 100.0,
                    self.limits.max_weekly_drawdown_pct * 100.0
                ),
            );
        }

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
            max_correlated_positions: 5, // Max 5 correlated positions in same direction
            max_quote_exposure_usd: max_total_notional * 1.2, // 120% of total (allows some margin)
            max_daily_drawdown_pct: 0.05, // 5% max daily drawdown
            max_weekly_drawdown_pct: 0.10, // 10% max weekly drawdown
            risk_per_trade_pct: 0.01, // 1% risk per trade
        }
    }
}

// ✅ CRITICAL FIX: Extract base asset from symbol (e.g., "BTCUSDT" -> "BTC", "ETHUSDC" -> "ETH")
fn extract_base_asset(symbol: &str) -> String {
    // Common quote assets
    let quote_assets = ["USDT", "USDC", "BUSD", "BTC", "ETH", "BNB"];
    
    for quote in &quote_assets {
        if symbol.ends_with(quote) {
            return symbol[..symbol.len() - quote.len()].to_string();
        }
    }
    
    // If no known quote asset found, assume last 4 chars are quote (e.g., "BTCUSDT")
    if symbol.len() > 4 {
        symbol[..symbol.len() - 4].to_string()
    } else {
        symbol.to_string()
    }
}

// ✅ CRITICAL FIX: Check if two assets are correlated
// Major coins (BTC, ETH) are highly correlated
// Altcoins are generally correlated with BTC
fn are_correlated(asset1: &str, asset2: &str) -> bool {
    if asset1 == asset2 {
        return true; // Same asset
    }
    
    // Major coins correlation group (highly correlated)
    let major_coins = ["BTC", "ETH", "BNB", "SOL", "XRP", "ADA", "DOT", "AVAX", "MATIC", "LINK"];
    let is_major1 = major_coins.contains(&asset1);
    let is_major2 = major_coins.contains(&asset2);
    
    if is_major1 && is_major2 {
        return true; // Both are major coins, highly correlated
    }
    
    // BTC is the market leader - all altcoins are correlated with BTC
    if asset1 == "BTC" || asset2 == "BTC" {
        return true; // Any coin is correlated with BTC
    }
    
    // ETH is also a major market leader
    if asset1 == "ETH" || asset2 == "ETH" {
        return true; // Any coin is correlated with ETH
    }
    
    // Layer 1 blockchains are correlated with each other
    let layer1_coins = ["SOL", "AVAX", "ADA", "DOT", "ATOM", "NEAR", "FTM", "ALGO"];
    let is_layer1_1 = layer1_coins.contains(&asset1);
    let is_layer1_2 = layer1_coins.contains(&asset2);
    
    if is_layer1_1 && is_layer1_2 {
        return true; // Both are Layer 1 blockchains, correlated
    }
    
    // DeFi tokens are correlated with each other
    let defi_coins = ["UNI", "AAVE", "SUSHI", "CRV", "COMP", "MKR", "SNX", "YFI"];
    let is_defi_1 = defi_coins.contains(&asset1);
    let is_defi_2 = defi_coins.contains(&asset2);
    
    if is_defi_1 && is_defi_2 {
        return true; // Both are DeFi tokens, correlated
    }
    
    // Gaming/Metaverse tokens are correlated
    let gaming_coins = ["AXS", "SAND", "MANA", "ENJ", "GALA", "IMX"];
    let is_gaming_1 = gaming_coins.contains(&asset1);
    let is_gaming_2 = gaming_coins.contains(&asset2);
    
    if is_gaming_1 && is_gaming_2 {
        return true; // Both are gaming tokens, correlated
    }
    
    // Meme coins are correlated
    let meme_coins = ["DOGE", "SHIB", "PEPE", "FLOKI", "BONK"];
    let is_meme_1 = meme_coins.contains(&asset1);
    let is_meme_2 = meme_coins.contains(&asset2);
    
    if is_meme_1 && is_meme_2 {
        return true; // Both are meme coins, correlated
    }
    
    // Default: assume low correlation if not in same category
    false
}
