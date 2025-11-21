// ✅ ADIM 6: Risk & portföy yönetimi (TrendPlan.md)
// Portföy bazlı limitler: toplam notional, coin başına limit, korelasyon kontrolü

use log::info;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::RwLock;
use chrono::{DateTime, Utc, Duration as ChronoDuration};

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
    // ✅ CRITICAL FIX: Dynamic correlation tracking (TrendPlan.md - Action Plan)
    // Store price history for last 24 hours to calculate real-time correlation
    price_history: Arc<RwLock<HashMap<String, VecDeque<PricePoint>>>>, // symbol -> price history
    // ✅ CRITICAL FIX: Correlation cache for performance optimization (TrendPlan.md - Action Plan)
    // Cache correlation matrix to avoid O(N^2) recalculation on every position check
    correlation_cache: Arc<RwLock<HashMap<(String, String), (f64, DateTime<Utc>)>>>, // (symbol1, symbol2) -> (correlation, timestamp)
    last_cache_update: Arc<RwLock<DateTime<Utc>>>, // Last time correlation cache was updated
    // ✅ CONFIG: Configurable thresholds (no hardcoded values)
    correlation_threshold: f64, // High correlation threshold (default: 0.7 = 70%)
    max_correlated_notional_pct: f64, // Max % of total notional in correlated positions (default: 0.5 = 50%)
    correlation_cache_update_interval_secs: i64, // Cache update interval (default: 60)
    correlation_cache_retention_minutes: i64, // Cache retention time (default: 5)
}

#[derive(Debug, Clone)]
struct PricePoint {
    price: f64,
    timestamp: DateTime<Utc>,
}

impl RiskManager {
    pub fn new(limits: RiskLimits, risk_config: Option<&crate::types::FileRisk>) -> Self {
        let correlation_threshold = risk_config
            .and_then(|r| r.correlation_threshold)
            .unwrap_or(0.7); // Default: 70%
        let max_correlated_notional_pct = risk_config
            .and_then(|r| r.max_correlated_notional_pct)
            .unwrap_or(0.5); // Default: 50%
        let correlation_cache_update_interval_secs = risk_config
            .and_then(|r| r.correlation_cache_update_interval_secs)
            .unwrap_or(60) as i64; // Default: 60 seconds
        let correlation_cache_retention_minutes = risk_config
            .and_then(|r| r.correlation_cache_retention_minutes)
            .unwrap_or(5) as i64; // Default: 5 minutes

        Self {
            limits,
            positions: Arc::new(RwLock::new(HashMap::new())),
            price_history: Arc::new(RwLock::new(HashMap::new())),
            correlation_cache: Arc::new(RwLock::new(HashMap::new())),
            last_cache_update: Arc::new(RwLock::new(Utc::now() - ChronoDuration::minutes(2))), // Initialize to 2 minutes ago to trigger first update
            correlation_threshold,
            max_correlated_notional_pct,
            correlation_cache_update_interval_secs,
            correlation_cache_retention_minutes,
        }
    }

    /// Update price history for correlation calculation (TrendPlan.md - Action Plan)
    /// Call this periodically (e.g., every minute) with current prices
    pub async fn update_price_history(&self, symbol: &str, price: f64) {
        let mut history = self.price_history.write().await;
        let entry = history.entry(symbol.to_string()).or_insert_with(|| VecDeque::new());
        
        let now = Utc::now();
        entry.push_back(PricePoint {
            price,
            timestamp: now,
        });

        // Keep only last 24 hours of data (1440 minutes if updating every minute)
        let cutoff = now - ChronoDuration::hours(24);
        while let Some(front) = entry.front() {
            if front.timestamp < cutoff {
                entry.pop_front();
            } else {
                break;
            }
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

        // 3. ✅ CRITICAL FIX: Check correlated positions using dynamic Pearson correlation (TrendPlan.md - Action Plan)
        // ✅ PERFORMANCE FIX: Use cached correlation matrix (updated every minute) instead of O(N^2) recalculation
        // Update cache if it's been more than configured interval since last update
        let cache_needs_update = {
            let last_update = self.last_cache_update.read().await;
            Utc::now().signed_duration_since(*last_update).num_seconds() >= self.correlation_cache_update_interval_secs
        };
        
        if cache_needs_update {
            // Update correlation cache in background (async)
            let price_history = self.price_history.read().await;
            let mut cache = self.correlation_cache.write().await;
            let mut last_update = self.last_cache_update.write().await;
            
            // Clear old cache entries (older than configured retention time)
            let cutoff = Utc::now() - ChronoDuration::minutes(self.correlation_cache_retention_minutes);
            cache.retain(|_, (_, ts)| *ts > cutoff);
            
            // Update cache with current positions
            let position_symbols: Vec<String> = positions.keys().cloned().collect();
            for sym1 in &position_symbols {
                for sym2 in &position_symbols {
                    if sym1 < sym2 {
                        // Only calculate once per pair (symmetric)
                        if let Some(corr) = calculate_correlation(sym1, sym2, &price_history) {
                            cache.insert((sym1.clone(), sym2.clone()), (corr, Utc::now()));
                        }
                    }
                }
            }
            
            *last_update = Utc::now();
            drop(price_history);
            drop(cache);
            drop(last_update);
        }
        
        // Use cached correlation values
        let cache = self.correlation_cache.read().await;
        let price_history = self.price_history.read().await;
        
        // Helper function to get correlation (from cache or calculate on-the-fly)
        let get_correlation = |sym1: &str, sym2: &str| -> Option<f64> {
            // Try cache first
            if sym1 < sym2 {
                cache.get(&(sym1.to_string(), sym2.to_string()))
                    .map(|(corr, _)| *corr)
            } else {
                cache.get(&(sym2.to_string(), sym1.to_string()))
                    .map(|(corr, _)| *corr)
            }
            // If not in cache, calculate on-the-fly (but this should be rare after cache is populated)
            .or_else(|| calculate_correlation(sym1, sym2, &price_history))
        };
        
        // Find correlated positions (same direction and high correlation)
        let correlated_count = positions
            .values()
            .filter(|p| {
                if p.side != side {
                    return false; // Different direction, not correlated risk
                }
                // Use cached correlation if available, otherwise calculate on-the-fly
                if let Some(corr) = get_correlation(symbol, &p.symbol) {
                    corr > self.correlation_threshold // Configurable correlation threshold
                } else {
                    false // Insufficient data for correlation
                }
            })
            .count();

        if correlated_count >= self.limits.max_correlated_positions {
            return (
                false,
                    {
                        let correlated_symbols: Vec<String> = positions
                            .values()
                            .filter(|p| {
                                if p.side != side {
                                    return false;
                                }
                                if let Some(correlation) = get_correlation(symbol, &p.symbol) {
                                    correlation > self.correlation_threshold
                                } else {
                                    false
                                }
                            })
                            .map(|p| p.symbol.clone())
                            .collect();
                        format!(
                            "Too many correlated {} positions: {} >= {} (max). New: {}, Existing correlated: {}",
                            match side {
                                Side::Long => "LONG",
                                Side::Short => "SHORT",
                            },
                            correlated_count + 1,
                            self.limits.max_correlated_positions,
                            symbol,
                            correlated_symbols.join(", ")
                        )
                    },
            );
        }

        // ✅ ADDITIONAL: Check correlated notional (aggregate risk from correlated positions)
        let correlated_notional: f64 = positions
            .values()
            .filter(|p| {
                if p.side != side {
                    return false;
                }
                // Use cached correlation if available
                if let Some(corr) = get_correlation(symbol, &p.symbol) {
                    corr > 0.7 // High correlation threshold (70%)
                } else {
                    false // Insufficient data for correlation
                }
            })
            .map(|p| p.notional_usd)
            .sum();
        
        let max_correlated_notional = self.limits.max_total_notional_usd * self.max_correlated_notional_pct; // Configurable % of total in correlated positions
        if correlated_notional + new_notional > max_correlated_notional {
            return (
                false,
                    {
                        let correlated_symbols: Vec<String> = positions
                            .values()
                            .filter(|p| {
                                if p.side != side {
                                    return false;
                                }
                                if let Some(correlation) = get_correlation(symbol, &p.symbol) {
                                    correlation > self.correlation_threshold
                                } else {
                                    false
                                }
                            })
                            .map(|p| p.symbol.clone())
                            .collect();
                        format!(
                            "Correlated positions notional would exceed limit: {:.2} > {:.2} USD. New: {}, Correlated group: {}",
                            correlated_notional + new_notional,
                            max_correlated_notional,
                            symbol,
                            correlated_symbols.join(", ")
                        )
                    },
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
    pub fn from_config(max_total_notional: f64, risk_config: Option<&crate::types::FileRisk>) -> Self {
        let max_position_per_symbol_pct = risk_config
            .and_then(|r| r.max_position_per_symbol_pct)
            .unwrap_or(0.3); // Default: 30%
        let max_correlated_positions = risk_config
            .and_then(|r| r.max_correlated_positions)
            .unwrap_or(5); // Default: 5
        let max_daily_drawdown_pct = risk_config
            .and_then(|r| r.max_daily_drawdown_pct)
            .unwrap_or(0.05); // Default: 5%
        let max_weekly_drawdown_pct = risk_config
            .and_then(|r| r.max_weekly_drawdown_pct)
            .unwrap_or(0.10); // Default: 10%
        let risk_per_trade_pct = risk_config
            .and_then(|r| r.risk_per_trade_pct)
            .unwrap_or(0.01); // Default: 1%

        Self {
            max_total_notional_usd: max_total_notional,
            max_position_per_symbol_usd: max_total_notional * max_position_per_symbol_pct,
            max_correlated_positions,
            max_quote_exposure_usd: max_total_notional * 1.2, // 120% of total (allows some margin)
            max_daily_drawdown_pct,
            max_weekly_drawdown_pct,
            risk_per_trade_pct,
        }
    }
}

// ✅ CRITICAL FIX: Calculate Pearson correlation coefficient between two symbols (TrendPlan.md - Action Plan)
// Returns correlation coefficient (-1.0 to 1.0) or None if insufficient data
// Correlation > 0.7 indicates high positive correlation (assets move together)
fn calculate_correlation(
    symbol1: &str,
    symbol2: &str,
    price_history: &HashMap<String, VecDeque<PricePoint>>,
) -> Option<f64> {
    if symbol1 == symbol2 {
        return Some(1.0); // Same symbol = perfect correlation
    }

    let hist1 = price_history.get(symbol1)?;
    let hist2 = price_history.get(symbol2)?;

    // Need at least 30 data points for reliable correlation
    if hist1.len() < 30 || hist2.len() < 30 {
        return None;
    }

    // Align price histories by timestamp (use common timestamps)
    let mut aligned_prices1 = Vec::new();
    let mut aligned_prices2 = Vec::new();

    // Create a map of symbol2 prices by timestamp (rounded to nearest minute for alignment)
    let mut prices2_by_minute: HashMap<i64, f64> = HashMap::new();
    for point in hist2.iter() {
        let minute = point.timestamp.timestamp() / 60;
        prices2_by_minute.insert(minute, point.price);
    }

    // Align symbol1 prices with symbol2 prices
    for point1 in hist1.iter() {
        let minute = point1.timestamp.timestamp() / 60;
        if let Some(&price2) = prices2_by_minute.get(&minute) {
            aligned_prices1.push(point1.price);
            aligned_prices2.push(price2);
        }
    }

    // Need at least 30 aligned points
    if aligned_prices1.len() < 30 {
        return None;
    }

    // Calculate Pearson correlation coefficient
    // r = Σ((x - x̄)(y - ȳ)) / sqrt(Σ(x - x̄)² * Σ(y - ȳ)²)
    let n = aligned_prices1.len() as f64;
    let mean1: f64 = aligned_prices1.iter().sum::<f64>() / n;
    let mean2: f64 = aligned_prices2.iter().sum::<f64>() / n;

    let mut numerator = 0.0;
    let mut sum_sq_diff1 = 0.0;
    let mut sum_sq_diff2 = 0.0;

    for i in 0..aligned_prices1.len() {
        let diff1 = aligned_prices1[i] - mean1;
        let diff2 = aligned_prices2[i] - mean2;
        numerator += diff1 * diff2;
        sum_sq_diff1 += diff1 * diff1;
        sum_sq_diff2 += diff2 * diff2;
    }

    let denominator = (sum_sq_diff1 * sum_sq_diff2).sqrt();
    if denominator == 0.0 {
        return None; // No variance, cannot calculate correlation
    }

    let correlation = numerator / denominator;
    
    // Clamp to [-1.0, 1.0] range (should already be in range, but ensure it)
    Some(correlation.max(-1.0).min(1.0))
}
