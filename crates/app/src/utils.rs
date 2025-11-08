//location: /crates/app/src/utils.rs
// Utility functions and helpers

use bot_core::types::*;
use exec::binance::SymbolRules;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

// ============================================================================
// Quantity and Price Helpers
// ============================================================================

/// Get quantity step size (per-symbol or fallback)
pub fn get_qty_step(
    symbol_rules: Option<&std::sync::Arc<SymbolRules>>,
    fallback: f64,
) -> f64 {
    symbol_rules
        .map(|r| r.step_size.to_f64().unwrap_or(fallback))
        .unwrap_or(fallback)
}

/// Get price tick size (per-symbol or fallback)
pub fn get_price_tick(
    symbol_rules: Option<&std::sync::Arc<SymbolRules>>,
    fallback: f64,
) -> f64 {
    symbol_rules
        .map(|r| r.tick_size.to_f64().unwrap_or(fallback))
        .unwrap_or(fallback)
}

/// Helper trait for floor division by step
pub trait FloorStep {
    fn floor_div_step(self, step: f64) -> f64;
}

impl FloorStep for f64 {
    fn floor_div_step(self, step: f64) -> f64 {
        if step <= 0.0 {
            return self;
        }
        (self / step).floor() * step
    }
}

/// Clamp quantity by USD value
pub fn clamp_qty_by_usd(qty: Qty, px: Px, max_usd: f64, qty_step: f64) -> Qty {
    let p = px.0.to_f64().unwrap_or(0.0);
    if p <= 0.0 || max_usd <= 0.0 {
        return Qty(Decimal::ZERO);
    }
    let max_qty = (max_usd / p).floor_div_step(qty_step);
    let wanted = qty.0.to_f64().unwrap_or(0.0);
    let q = wanted.min(max_qty);
    Qty(Decimal::from_f64_retain(q).unwrap_or(Decimal::ZERO))
}

/// Clamp quantity by base asset amount
pub fn clamp_qty_by_base(qty: Qty, max_base: f64, qty_step: f64) -> Qty {
    if max_base <= 0.0 {
        return Qty(Decimal::ZERO);
    }
    let max_qty = max_base.floor_div_step(qty_step);
    let wanted = qty.0.to_f64().unwrap_or(0.0);
    let q = wanted.min(max_qty);
    Qty(Decimal::from_f64_retain(q).unwrap_or(Decimal::ZERO))
}

/// Check if asset is a USD stablecoin
pub fn is_usd_stable(asset: &str) -> bool {
    matches!(
        asset.to_uppercase().as_str(),
        "USDT" | "USDC" | "BUSD"
    )
}

// ============================================================================
// PnL Calculation Helpers
// ============================================================================

/// Compute drawdown in basis points from equity history
pub fn compute_drawdown_bps(history: &[Decimal]) -> i64 {
    if history.is_empty() {
        return 0;
    }
    
    let mut peak = history[0];
    let mut max_drawdown = Decimal::ZERO;
    
    for value in history {
        if *value > peak {
            peak = *value;
        }
        if peak > Decimal::ZERO {
            let drawdown = ((*value - peak) / peak) * Decimal::from(10_000i32);
            if drawdown < max_drawdown {
                max_drawdown = drawdown;
            }
        }
    }
    
    max_drawdown.to_i64().unwrap_or(0)
}

/// Record PnL snapshot to history
pub fn record_pnl_snapshot(
    history: &mut Vec<Decimal>,
    pos: &Position,
    mark_px: Px,
    max_len: usize,
) {
    let pnl = (mark_px.0 - pos.entry.0) * pos.qty.0;
    let mut equity = Decimal::ONE + pnl;
    
    // Keep the history strictly positive so drawdown math remains stable
    if equity <= Decimal::ZERO {
        equity = Decimal::new(1, 4);
    }
    
    history.push(equity);
    if history.len() > max_len {
        let excess = history.len() - max_len;
        history.drain(0..excess);
    }
}

// ============================================================================
// Rate Limiting - Weight-Based Binance API Rate Limiter
// ============================================================================

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Weight-based rate limiter for Binance API calls
/// 
/// Binance API Limits:
/// - Spot: 1200 requests/minute (20 req/sec) - weight-based
/// - Futures: 2400 requests/minute (40 req/sec) - weight-based
/// 
/// Common endpoint weights:
/// - GET /api/v3/ticker/bookTicker (best_prices): Weight 2
/// - POST /api/v3/order (place_limit): Weight 1
/// - DELETE /api/v3/order (cancel): Weight 1
/// - GET /api/v3/openOrders: Weight 3
/// - GET /api/v3/account: Weight 10
/// - GET /fapi/v1/ticker/bookTicker: Weight 1
/// - POST /fapi/v1/order: Weight 1
/// - DELETE /fapi/v1/order: Weight 1
/// - GET /fapi/v2/positionRisk: Weight 5
pub struct RateLimiter {
    /// Per-second request timestamps (for burst protection)
    requests: Mutex<VecDeque<Instant>>,
    /// Per-minute weight tracking (for weight-based limits)
    weights: Mutex<VecDeque<(Instant, u32)>>,
    /// Maximum requests per second (with safety margin)
    max_requests_per_sec: u32,
    /// Maximum weight per minute (with safety margin)
    max_weight_per_minute: u32,
    /// Minimum interval between requests (ms)
    min_interval_ms: u64,
    /// Safety factor (0.0-1.0) - how much of the limit to use
    #[allow(dead_code)]
    safety_factor: f64,
}

impl RateLimiter {
    /// Create a new rate limiter with safety margin
    /// 
    /// # Arguments
    /// * `max_requests_per_sec` - Maximum requests per second (Spot: 20, Futures: 40)
    /// * `max_weight_per_minute` - Maximum weight per minute (Spot: 1200, Futures: 2400)
    /// * `safety_factor` - Safety margin (0.0-1.0). 0.6 = use only 60% of limit (very safe)
    pub fn new(max_requests_per_sec: u32, max_weight_per_minute: u32, safety_factor: f64) -> Self {
        let safety_factor = safety_factor.clamp(0.5, 0.95); // En az %50, en fazla %95 kullan
        let safe_req_limit = (max_requests_per_sec as f64 * safety_factor) as u32;
        let safe_weight_limit = (max_weight_per_minute as f64 * safety_factor) as u32;
        let min_interval_ms = (1000.0 / safe_req_limit as f64).ceil() as u64;
        
        Self {
            requests: Mutex::new(VecDeque::new()),
            weights: Mutex::new(VecDeque::new()),
            max_requests_per_sec: safe_req_limit,
            max_weight_per_minute: safe_weight_limit,
            min_interval_ms,
            safety_factor,
        }
    }
    
    /// Wait if needed to respect rate limits (weight-based)
    /// 
    /// # Arguments
    /// * `weight` - API endpoint weight (default: 1 for most endpoints)
    pub async fn wait_if_needed(&self, weight: u32) {
        loop {
            let now = Instant::now();
            let mut requests = self.requests.lock().unwrap();
            let mut weights = self.weights.lock().unwrap();
            
            // ===== PER-SECOND LIMIT (Burst Protection) =====
            // Son 1 saniyede yapılan request'leri temizle
            let one_sec_ago = now.checked_sub(Duration::from_secs(1))
                .unwrap_or(Instant::now());
            while requests.front().map_or(false, |&t| t < one_sec_ago) {
                requests.pop_front();
            }
            
            // Eğer per-second limit aşıldıysa bekle
            if requests.len() >= self.max_requests_per_sec as usize {
                if let Some(oldest) = requests.front().copied() {
                    let wait_time = oldest + Duration::from_secs(1);
                    if wait_time > now {
                        let sleep_duration = wait_time.duration_since(now);
                        drop(requests);
                        drop(weights);
                        tokio::time::sleep(sleep_duration).await;
                        continue;
                    }
                }
            }
            
            // Minimum interval kontrolü (her request arasında minimum bekleme)
            if let Some(last) = requests.back() {
                let elapsed = now.duration_since(*last);
                if elapsed.as_millis() < self.min_interval_ms as u128 {
                    let wait = Duration::from_millis(self.min_interval_ms)
                        .saturating_sub(elapsed);
                    if wait.as_millis() > 0 {
                        drop(requests);
                        drop(weights);
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                }
            }
            
            // ===== PER-MINUTE WEIGHT LIMIT =====
            // Son 1 dakikada kullanılan weight'leri temizle
            let one_min_ago = now.checked_sub(Duration::from_secs(60))
                .unwrap_or(Instant::now());
            while weights.front().map_or(false, |&(t, _)| t < one_min_ago) {
                weights.pop_front();
            }
            
            // Toplam weight'i hesapla
            let total_weight: u32 = weights.iter().map(|(_, w)| w).sum();
            
            // Eğer weight limit aşıldıysa bekle
            if total_weight + weight > self.max_weight_per_minute {
                if let Some((oldest_time, _)) = weights.front().copied() {
                    let wait_time = oldest_time + Duration::from_secs(60);
                    if wait_time > now {
                        let sleep_duration = wait_time.duration_since(now);
                        drop(requests);
                        drop(weights);
                        tokio::time::sleep(sleep_duration).await;
                        continue;
                    }
                }
            }
            
            // Request'i kaydet ve çık
            requests.push_back(now);
            weights.push_back((now, weight));
            break;
        }
    }
    
    /// Get current usage statistics (for monitoring)
    #[allow(dead_code)]
    pub fn get_stats(&self) -> (usize, u32, f64) {
        let now = Instant::now();
        let requests = self.requests.lock().unwrap();
        let weights = self.weights.lock().unwrap();
        
        // Son 1 saniyede yapılan request sayısı
        let one_sec_ago = now.checked_sub(Duration::from_secs(1))
            .unwrap_or(Instant::now());
        let req_count = requests.iter().filter(|&&t| t >= one_sec_ago).count();
        
        // Son 1 dakikada kullanılan weight
        let one_min_ago = now.checked_sub(Duration::from_secs(60))
            .unwrap_or(Instant::now());
        let total_weight: u32 = weights.iter()
            .filter(|(t, _)| *t >= one_min_ago)
            .map(|(_, w)| w)
            .sum();
        
        let weight_usage_pct = (total_weight as f64 / self.max_weight_per_minute as f64) * 100.0;
        
        (req_count, total_weight, weight_usage_pct)
    }
}

/// Global rate limiter instance
/// 
/// Mode'a göre limit seçilir:
/// - Spot: 20 req/sec, 1200 weight/min → 12 req/sec, 720 weight/min (60% safety)
/// - Futures: 40 req/sec, 2400 weight/min → 24 req/sec, 1440 weight/min (60% safety)
static RATE_LIMITER: OnceLock<RateLimiter> = OnceLock::new();

/// Initialize rate limiter based on mode (spot or futures)
pub fn init_rate_limiter(is_futures: bool) {
    let (max_req_per_sec, max_weight_per_min) = if is_futures {
        (40, 2400) // Futures: 40 req/sec, 2400 weight/min
    } else {
        (20, 1200) // Spot: 20 req/sec, 1200 weight/min
    };
    
    // PATCH: Safety factor 0.6 → 0.7 (daha agresif, ban riski hala minimal)
    let safety_factor = 0.7;
    
    RATE_LIMITER.set(RateLimiter::new(
        max_req_per_sec,
        max_weight_per_min,
        safety_factor,
    )).ok();
}

/// Get or initialize the global rate limiter (default: Spot limits)
pub fn get_rate_limiter() -> &'static RateLimiter {
    RATE_LIMITER.get_or_init(|| {
        // Default: Spot limits with 60% safety factor
        RateLimiter::new(20, 1200, 0.6)
    })
}

/// Guard function for rate limiting (async)
/// 
/// # Arguments
/// * `weight` - API endpoint weight (default: 1)
pub async fn rate_limit_guard(weight: u32) {
    get_rate_limiter().wait_if_needed(weight).await;
}

/// Convenience function for rate limiting with default weight (1)
#[allow(dead_code)]
pub async fn rate_limit_guard_default() {
    rate_limit_guard(1).await;
}

// ============================================================================
// Profit Guarantee and Tracking
// ============================================================================

/// Profit guarantee calculator - ensures each trade is profitable after fees
pub struct ProfitGuarantee {
    /// Minimum profit per trade in USD (e.g., 0.50 cents = 0.005 USD)
    min_profit_usd: f64,
    /// Maker fee rate (e.g., 0.0002 = 0.02% = 2 bps)
    maker_fee_rate: f64,
    /// Taker fee rate (e.g., 0.0004 = 0.04% = 4 bps)
    taker_fee_rate: f64,
}

impl ProfitGuarantee {
    /// Create a new profit guarantee calculator
    /// 
    /// # Arguments
    /// * `min_profit_usd` - Minimum profit per trade in USD (e.g., 0.005 for 0.50 cents)
    /// * `maker_fee_rate` - Maker fee rate (default: 0.0002 = 2 bps for Binance Futures)
    /// * `taker_fee_rate` - Taker fee rate (default: 0.0004 = 4 bps for Binance Futures)
    pub fn new(min_profit_usd: f64, maker_fee_rate: f64, taker_fee_rate: f64) -> Self {
        Self {
            min_profit_usd,
            maker_fee_rate,
            taker_fee_rate,
        }
    }

    /// Default constructor with Binance Futures fees
    pub fn default() -> Self {
        Self::new(0.005, 0.0002, 0.0004) // 0.50 cents, 2 bps maker, 4 bps taker
    }

    /// Calculate minimum spread required for profitable trade
    /// 
    /// # Arguments
    /// * `position_size_usd` - Position size in USD
    /// 
    /// # Returns
    /// Minimum spread in bps (basis points) required for profit
    pub fn calculate_min_spread_bps(&self, position_size_usd: f64) -> f64 {
        if position_size_usd <= 0.0 {
            return 0.0;
        }

        // Net profit = Gross profit - Fees
        // Gross profit = position_size * spread_rate
        // Fees = position_size * (maker_fee + taker_fee)
        // 
        // min_profit = position_size * spread_rate - position_size * (maker_fee + taker_fee)
        // min_profit = position_size * (spread_rate - total_fee_rate)
        // spread_rate = (min_profit / position_size) + total_fee_rate
        // spread_bps = spread_rate * 10000

        let total_fee_rate = self.maker_fee_rate + self.taker_fee_rate;
        let min_spread_rate = (self.min_profit_usd / position_size_usd) + total_fee_rate;
        min_spread_rate * 10000.0 // Convert to bps
    }

    /// Check if a trade is profitable given spread and position size
    /// 
    /// # Arguments
    /// * `spread_bps` - Current spread in basis points
    /// * `position_size_usd` - Position size in USD
    /// 
    /// # Returns
    /// true if trade is profitable, false otherwise
    pub fn is_trade_profitable(&self, spread_bps: f64, position_size_usd: f64) -> bool {
        if position_size_usd <= 0.0 {
            return false;
        }

        let min_spread_bps = self.calculate_min_spread_bps(position_size_usd);
        spread_bps >= min_spread_bps
    }

    /// Calculate expected profit for a trade
    /// 
    /// # Arguments
    /// * `spread_bps` - Current spread in basis points
    /// * `position_size_usd` - Position size in USD
    /// 
    /// # Returns
    /// Expected profit in USD (can be negative if unprofitable)
    pub fn calculate_expected_profit(&self, spread_bps: f64, position_size_usd: f64) -> f64 {
        if position_size_usd <= 0.0 {
            return 0.0;
        }

        let spread_rate = spread_bps / 10000.0;
        let gross_profit = position_size_usd * spread_rate;
        let total_fees = position_size_usd * (self.maker_fee_rate + self.taker_fee_rate);
        gross_profit - total_fees
    }

    /// Calculate optimal position size for a given spread
    /// 
    /// # Arguments
    /// * `spread_bps` - Current spread in basis points
    /// * `min_position_usd` - Minimum position size in USD
    /// * `max_position_usd` - Maximum position size in USD
    /// 
    /// # Returns
    /// Optimal position size in USD that guarantees minimum profit
    pub fn calculate_optimal_position_size(
        &self,
        spread_bps: f64,
        min_position_usd: f64,
        max_position_usd: f64,
    ) -> f64 {
        if spread_bps <= 0.0 {
            return min_position_usd;
        }

        let spread_rate = spread_bps / 10000.0;
        let total_fee_rate = self.maker_fee_rate + self.taker_fee_rate;
        let net_spread_rate = spread_rate - total_fee_rate;

        if net_spread_rate <= 0.0 {
            // Spread doesn't cover fees, use minimum
            return min_position_usd;
        }

        // Calculate position size needed for minimum profit
        let optimal_size = self.min_profit_usd / net_spread_rate;
        
        // Clamp to min/max bounds
        optimal_size.max(min_position_usd).min(max_position_usd)
    }

    /// Calculate risk/reward ratio for a trade
    /// 
    /// # Arguments
    /// * `spread_bps` - Current spread in basis points
    /// * `position_size_usd` - Position size in USD
    /// * `max_loss_pct` - Maximum loss percentage (e.g., 0.01 = 1%)
    /// 
    /// # Returns
    /// Risk/reward ratio (e.g., 2.0 = 2:1 reward:risk)
    pub fn calculate_risk_reward_ratio(
        &self,
        spread_bps: f64,
        position_size_usd: f64,
        max_loss_pct: f64,
    ) -> f64 {
        if position_size_usd <= 0.0 || max_loss_pct <= 0.0 {
            return 0.0;
        }

        let expected_profit = self.calculate_expected_profit(spread_bps, position_size_usd);
        let max_loss = position_size_usd * max_loss_pct.abs();

        if max_loss <= 0.0 {
            return 0.0;
        }

        expected_profit / max_loss
    }
}

/// Profit tracker for monitoring trading performance
pub struct ProfitTracker {
    total_trades: u32,
    winning_trades: u32,
    total_profit: f64,
    total_fees: f64,
    total_loss: f64,
}

impl ProfitTracker {
    pub fn new() -> Self {
        Self {
            total_trades: 0,
            winning_trades: 0,
            total_profit: 0.0,
            total_fees: 0.0,
            total_loss: 0.0,
        }
    }

    /// Record a completed trade
    /// 
    /// # Arguments
    /// * `profit` - Net profit in USD (can be negative)
    /// * `fees` - Total fees paid in USD
    pub fn record_trade(&mut self, profit: f64, fees: f64) {
        self.total_trades += 1;
        self.total_fees += fees;

        if profit > 0.0 {
            self.winning_trades += 1;
            self.total_profit += profit;
        } else {
            self.total_loss += profit.abs();
        }
    }

    /// Get win rate as percentage
    pub fn win_rate(&self) -> f64 {
        if self.total_trades == 0 {
            return 0.0;
        }
        (self.winning_trades as f64 / self.total_trades as f64) * 100.0
    }

    /// Get net profit (profit - fees - losses)
    pub fn net_profit(&self) -> f64 {
        self.total_profit - self.total_fees - self.total_loss
    }

    /// Get statistics as formatted string
    pub fn get_stats(&self) -> String {
        let win_rate = self.win_rate();
        let net_profit = self.net_profit();
        let avg_profit_per_trade = if self.total_trades > 0 {
            net_profit / self.total_trades as f64
        } else {
            0.0
        };

        format!(
            "Trades: {}, Win Rate: {:.1}%, Net Profit: ${:.2}, Fees: ${:.2}, Avg Profit/Trade: ${:.4}",
            self.total_trades, win_rate, net_profit, self.total_fees, avg_profit_per_trade
        )
    }

    /// Reset all statistics
    pub fn reset(&mut self) {
        self.total_trades = 0;
        self.winning_trades = 0;
        self.total_profit = 0.0;
        self.total_fees = 0.0;
        self.total_loss = 0.0;
    }
}

impl Default for ProfitTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Thread-safe profit tracker wrapper
pub type SharedProfitTracker = Mutex<ProfitTracker>;

/// Calculate spread in basis points from bid and ask prices
/// 
/// # Arguments
/// * `bid` - Bid price
/// * `ask` - Ask price
/// 
/// # Returns
/// Spread in basis points (bps), or 0.0 if invalid
pub fn calculate_spread_bps(bid: Decimal, ask: Decimal) -> f64 {
    if bid <= Decimal::ZERO || ask <= Decimal::ZERO || ask <= bid {
        return 0.0;
    }
    
    let bid_f = bid.to_f64().unwrap_or(0.0);
    let ask_f = ask.to_f64().unwrap_or(0.0);
    
    if bid_f <= 0.0 || ask_f <= 0.0 || ask_f <= bid_f {
        return 0.0;
    }
    
    let spread_rate = (ask_f - bid_f) / bid_f;
    spread_rate * 10000.0 // Convert to bps
}

/// Check if a trade should be placed based on profit guarantee and risk/reward
/// 
/// # Arguments
/// * `spread_bps` - Current spread in basis points
/// * `position_size_usd` - Position size in USD
/// * `min_spread_bps` - Minimum spread threshold from config
/// * `stop_loss_threshold` - Stop loss threshold (negative, e.g., -0.01 for 1%)
/// * `min_risk_reward_ratio` - Minimum risk/reward ratio (e.g., 2.0 for 2:1)
/// 
/// # Returns
/// (should_place, reason) - true if trade should be placed, false otherwise with reason
pub fn should_place_trade(
    spread_bps: f64,
    position_size_usd: f64,
    min_spread_bps: f64,
    stop_loss_threshold: f64,
    min_risk_reward_ratio: f64,
) -> (bool, &'static str) {
    // 1. Minimum spread kontrolü
    if spread_bps < min_spread_bps {
        return (false, "spread_below_minimum");
    }
    
    // 2. Profit guarantee kontrolü
    let profit_guarantee = ProfitGuarantee::default();
    if !profit_guarantee.is_trade_profitable(spread_bps, position_size_usd) {
        return (false, "not_profitable_after_fees");
    }
    
    // 3. Risk/Reward oranı kontrolü
    let max_loss_pct = stop_loss_threshold.abs();
    let risk_reward_ratio = profit_guarantee.calculate_risk_reward_ratio(
        spread_bps,
        position_size_usd,
        max_loss_pct,
    );
    
    if risk_reward_ratio < min_risk_reward_ratio {
        return (false, "risk_reward_too_low");
    }
    
    (true, "ok")
}

