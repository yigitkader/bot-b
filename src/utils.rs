
use crate::types::Px;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::{Decimal, RoundingStrategy};
use std::collections::VecDeque;
use std::sync::OnceLock;
use std::str::FromStr;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
pub fn quantize_decimal(value: Decimal, step: Decimal) -> Decimal {
    if step.is_zero() || step.is_sign_negative() {
        return value;
    }
    let ratio = value / step;
    let floored = ratio.floor();
    let result = floored * step;
    let step_scale = step.scale();
    let normalized = result.normalize();
    let rounded = normalized.round_dp_with_strategy(
        step_scale,
        RoundingStrategy::ToNegativeInfinity,
    );
    let re_quantized_ratio = rounded / step;
    let re_quantized_floor = re_quantized_ratio.floor();
    let final_result = re_quantized_floor * step;
    final_result.normalize().round_dp_with_strategy(
        step_scale,
        RoundingStrategy::ToNegativeInfinity,
    )
}
pub fn format_decimal_fixed(value: Decimal, precision: usize) -> String {
    let precision = precision.min(28);
    let scale = precision as u32;
    let normalized = value.normalize();
    let rounded = normalized.round_dp_with_strategy(scale, RoundingStrategy::ToNegativeInfinity);
    if scale == 0 {
        let s = rounded.to_string();
        if let Some(dot_pos) = s.find('.') {
            s[..dot_pos].to_string()
        } else {
            s
        }
    } else {
        let s = rounded.to_string();
        if let Some(dot_pos) = s.find('.') {
            let integer_part = &s[..dot_pos];
            let decimal_part = &s[dot_pos + 1..];
            let current_decimals = decimal_part.len();
            if current_decimals < scale as usize {
                format!(
                    "{}.{}{}",
                    integer_part,
                    decimal_part,
                    "0".repeat(scale as usize - current_decimals)
                )
            } else if current_decimals > scale as usize {
                let truncated_decimal = &decimal_part[..scale as usize];
                format!("{}.{}", integer_part, truncated_decimal)
            } else {
                if decimal_part.len() == scale as usize {
                    s
                } else {
                    format!(
                        "{}.{}{}",
                        integer_part,
                        decimal_part,
                        "0".repeat(scale as usize - decimal_part.len())
                    )
                }
            }
        } else {
            format!("{}.{}", s, "0".repeat(scale as usize))
        }
    }
}
pub fn calculate_spread_bps(bid: Px, ask: Px) -> f64 {
    if bid.0.is_zero() {
        return 0.0;
    }
    let spread_bps = ((ask.0 - bid.0) / bid.0) * Decimal::from(10000);
    spread_bps.to_f64().unwrap_or(0.0)
}
pub fn calculate_mid_price(bid: Px, ask: Px) -> Decimal {
    (bid.0 + ask.0) / Decimal::from(2)
}
#[allow(dead_code)]
pub fn f64_to_decimal_percent(value: f64, fallback: Decimal) -> Decimal {
    Decimal::from_str(&value.to_string())
        .unwrap_or_else(|_| fallback)
}
pub fn f64_to_decimal(value: f64, fallback: Decimal) -> Decimal {
    Decimal::from_str(&value.to_string())
        .unwrap_or_else(|_| fallback)
}
pub fn f64_to_decimal_pct(value: f64) -> Decimal {
    Decimal::from_str(&value.to_string())
        .unwrap_or(Decimal::ZERO) / Decimal::from(100)
}
pub fn get_commission_rate(is_maker: bool, maker_rate: f64, taker_rate: f64) -> Decimal {
    let rate = if is_maker { maker_rate } else { taker_rate };
    f64_to_decimal_pct(rate)
}
pub fn calculate_commission(notional: Decimal, is_maker: bool, maker_rate: f64, taker_rate: f64) -> Decimal {
    let commission_rate = get_commission_rate(is_maker, maker_rate, taker_rate);
    notional * commission_rate
}
pub fn calculate_total_commission(
    entry_notional: Decimal,
    exit_notional: Decimal,
    entry_is_maker: Option<bool>,
    maker_rate: f64,
    taker_rate: f64,
) -> Decimal {
    let entry_commission = if let Some(is_maker) = entry_is_maker {
        calculate_commission(entry_notional, is_maker, maker_rate, taker_rate)
    } else {
        calculate_commission(entry_notional, false, maker_rate, taker_rate)
    };
    let exit_commission = calculate_commission(exit_notional, false, maker_rate, taker_rate);
    entry_commission + exit_commission
}
pub fn exponential_backoff(attempt: u32, base_ms: u64, multiplier: u64) -> Duration {
    Duration::from_millis(base_ms * multiplier.pow(attempt))
}
pub fn calculate_price_change(
    entry_price: Decimal,
    current_price: Decimal,
    direction: crate::types::PositionDirection,
) -> Decimal {
    if entry_price.is_zero() {
        return Decimal::ZERO;
    }
    if direction == crate::types::PositionDirection::Long {
        (current_price - entry_price) / entry_price
    } else {
        (entry_price - current_price) / entry_price
    }
}
pub fn calculate_pnl_percentage(
    entry_price: Decimal,
    current_price: Decimal,
    direction: crate::types::PositionDirection,
    leverage: u32,
) -> f64 {
    let leverage_decimal = Decimal::from(leverage);
    let price_change = calculate_price_change(entry_price, current_price, direction);
    let pnl_pct = price_change * leverage_decimal * Decimal::from(100);
    pnl_pct.to_f64().unwrap_or(0.0)
}
pub fn calculate_net_pnl(
    entry_price: Decimal,
    exit_price: Decimal,
    qty: Decimal,
    direction: crate::types::PositionDirection,
    leverage: u32,
    entry_commission_pct: Decimal,
    exit_commission_pct: Decimal,
) -> Decimal {
    let notional = entry_price * qty;
    let leverage_decimal = Decimal::from(leverage);
    let price_change = calculate_price_change(entry_price, exit_price, direction);
    let gross_pnl = notional * leverage_decimal * price_change;
    let total_commission = (notional * entry_commission_pct) + (exit_price * qty * exit_commission_pct);
    gross_pnl - total_commission
}
fn error_to_lowercase(error: &anyhow::Error) -> String {
    error.to_string().to_lowercase()
}
pub fn is_position_not_found_error(error: &anyhow::Error) -> bool {
    let error_lower = error_to_lowercase(error);
    error_lower.contains("position not found")
        || error_lower.contains("no position")
        || error_lower.contains("-2011")
}
pub fn is_min_notional_error(error: &anyhow::Error) -> bool {
    let error_lower = error_to_lowercase(error);
    contains_min_notional(&error_lower)
}
pub fn contains_min_notional(text: &str) -> bool {
    let text_lower = text.to_lowercase();
    text_lower.contains("-1013")
        || text_lower.contains("min notional")
        || text_lower.contains("min_notional")
        || text_lower.contains("below min notional")
}
pub fn is_position_already_closed_error(error: &anyhow::Error) -> bool {
    let error_lower = error_to_lowercase(error);
    error_lower.contains("position not found")
        || error_lower.contains("no position")
        || error_lower.contains("position already closed")
        || error_lower.contains("reduceonly")
        || error_lower.contains("reduce only")
        || error_lower.contains("-2011")
        || error_lower.contains("-2019")
        || error_lower.contains("-2021")
}
pub fn is_permanent_error(error: &anyhow::Error) -> bool {
    let error_lower = error_to_lowercase(error);
    error_lower.contains("invalid")
        || error_lower.contains("margin")
        || error_lower.contains("insufficient balance")
        || error_lower.contains("min notional")
        || error_lower.contains("below min notional")
        || error_lower.contains("invalid symbol")
        || error_lower.contains("symbol not found")
        || is_position_not_found_error(error)
        || is_position_already_closed_error(error)
}
pub fn decimal_to_f64(value: Decimal) -> f64 {
    value.to_f64().unwrap_or(0.0)
}
pub fn calculate_percentage(numerator: Decimal, denominator: Decimal) -> Decimal {
    if denominator.is_zero() {
        Decimal::ZERO
    } else {
        (numerator / denominator) * Decimal::from(100)
    }
}
pub fn calculate_dust_threshold(min_notional: Decimal, current_price: Decimal) -> Decimal {
    if !current_price.is_zero() {
        min_notional / current_price
    } else {
        // Fallback: assume minimum price of $0.01 (1 cent) for dust calculation
        // This is only used when current_price is zero (shouldn't happen in production)
        // In production, current_price should always come from real market data
        let assumed_min_price = Decimal::new(1, 2); // 0.01
        min_notional / assumed_min_price
    }
}

// ============================================================================
// Rate Limiting - Weight-Based Binance API Rate Limiter
// ============================================================================

/// Weight-based rate limiter for Binance Futures API calls
/// 
/// Binance Futures API Limits:
/// - Futures: 2400 requests/minute (40 req/sec) - weight-based
/// 
/// Common endpoint weights:
/// - GET /fapi/v1/depth (best_prices): Weight 5
/// - POST /fapi/v1/order (place_limit): Weight 1
/// - DELETE /fapi/v1/order (cancel): Weight 1
/// - GET /fapi/v1/openOrders: Weight 1
/// - GET /fapi/v2/positionRisk: Weight 5
/// - GET /fapi/v2/balance: Weight 5
pub struct RateLimiter {
    /// Per-second request timestamps (for burst protection)
    requests: Mutex<VecDeque<Instant>>,
    /// Per-minute weight tracking (for weight-based limits)
    weights: Mutex<VecDeque<(Instant, u32)>>,
    /// Atomik counter: Sleep sırasında reserve edilen weight (race condition fix)
    /// Bu, sleep sırasında başka thread'lerin weight eklemesini önler
    reserved_weight: std::sync::atomic::AtomicU32,
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
    /// * `max_requests_per_sec` - Maximum requests per second (Futures: 40)
    /// * `max_weight_per_minute` - Maximum weight per minute (Futures: 2400)
    /// * `safety_factor` - Safety margin (0.0-1.0). 0.7 = use only 70% of limit
    pub fn new(max_requests_per_sec: u32, max_weight_per_minute: u32, safety_factor: f64) -> Self {
        let safety_factor = safety_factor.clamp(0.5, 0.95); // En az %50, en fazla %95 kullan
        let safe_req_limit = (max_requests_per_sec as f64 * safety_factor) as u32;
        let safe_weight_limit = (max_weight_per_minute as f64 * safety_factor) as u32;
        let min_interval_ms = (1000.0 / safe_req_limit as f64).ceil() as u64;
        
        Self {
            requests: Mutex::new(VecDeque::new()),
            weights: Mutex::new(VecDeque::new()),
            reserved_weight: std::sync::atomic::AtomicU32::new(0),
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
            let mut requests = self.requests.lock().await;
            let mut weights = self.weights.lock().await;
            
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
                        // Kısa bekleme (< 1 saniye): 100ms polling
                        let poll_interval_ms = 100;
                        self.sleep_with_polling(sleep_duration, poll_interval_ms).await;
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
                        let poll_interval_ms = 100;
                        self.sleep_with_polling(wait, poll_interval_ms).await;
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
            
            // Toplam weight'i hesapla (reserved weight dahil - race condition fix)
            let total_weight: u32 = weights.iter().map(|(_, w)| *w).sum();
            let reserved = self.reserved_weight.load(std::sync::atomic::Ordering::Acquire);
            let total_with_reserved = total_weight + reserved;
            
            // Eğer weight limit aşıldıysa bekle
            if total_with_reserved + weight > self.max_weight_per_minute {
                if let Some((oldest_time, _)) = weights.front().copied() {
                    let wait_time = oldest_time + Duration::from_secs(60);
                    if wait_time > now {
                        let sleep_duration = wait_time.duration_since(now);
                        
                        // KRİTİK RACE CONDITION FIX: Weight'i atomik olarak reserve et
                        self.reserved_weight.fetch_add(weight, std::sync::atomic::Ordering::AcqRel);
                        
                        drop(requests);
                        drop(weights);
                        
                        // Adaptive polling interval
                        let poll_interval_ms = if sleep_duration.as_secs() >= 1 {
                            5000 // 5 saniye - Binance 1 dakikalık window için yeterli
                        } else {
                            100 // 100ms - kısa bekleme için
                        };
                        self.sleep_with_polling(sleep_duration, poll_interval_ms).await;
                        
                        // Sleep bitince reserve'i serbest bırak
                        self.reserved_weight.fetch_sub(weight, std::sync::atomic::Ordering::AcqRel);
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
    
    /// Sleep with polling: Sleep'i küçük parçalara böl (overhead azaltma için)
    async fn sleep_with_polling(&self, total_duration: Duration, poll_interval_ms: u64) {
        let poll_interval = Duration::from_millis(poll_interval_ms);
        let mut remaining = total_duration;
        
        while remaining > Duration::ZERO {
            let sleep_duration = remaining.min(poll_interval);
            tokio::time::sleep(sleep_duration).await;
            remaining = remaining.saturating_sub(sleep_duration);
        }
    }
    
    /// Get current usage statistics (for monitoring)
    #[allow(dead_code)]
    pub async fn get_stats(&self) -> (usize, u32, f64) {
        let now = Instant::now();
        let requests = self.requests.lock().await;
        let weights = self.weights.lock().await;
        
        // Son 1 saniyede yapılan request sayısı
        let one_sec_ago = now.checked_sub(Duration::from_secs(1))
            .unwrap_or(Instant::now());
        let req_count = requests.iter().filter(|&&t| t >= one_sec_ago).count();
        
        // Son 1 dakikada kullanılan weight
        let one_min_ago = now.checked_sub(Duration::from_secs(60))
            .unwrap_or(Instant::now());
        let total_weight: u32 = weights.iter()
            .filter(|(t, _)| *t >= one_min_ago)
            .map(|(_, w)| *w)
            .sum();
        
        let weight_usage_pct = (total_weight as f64 / self.max_weight_per_minute as f64) * 100.0;
        
        (req_count, total_weight, weight_usage_pct)
    }
}

/// Global rate limiter instance
/// 
/// Futures limits:
/// - Futures: 40 req/sec, 2400 weight/min → 28 req/sec, 1680 weight/min (70% safety)
static RATE_LIMITER: OnceLock<RateLimiter> = OnceLock::new();

/// Initialize rate limiter for futures
pub fn init_rate_limiter() {
    // Futures: 40 req/sec, 2400 weight/min
    let max_req_per_sec = 40;
    let max_weight_per_min = 2400;
    
    // Safety factor 0.7 (daha agresif, ban riski hala minimal)
    let safety_factor = 0.7;
    
    RATE_LIMITER.set(RateLimiter::new(
        max_req_per_sec,
        max_weight_per_min,
        safety_factor,
    )).ok();
}

/// Get or initialize the global rate limiter (default: Futures limits)
pub fn get_rate_limiter() -> &'static RateLimiter {
    RATE_LIMITER.get_or_init(|| {
        // Default: Futures limits with 70% safety factor
        RateLimiter::new(40, 2400, 0.7)
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
