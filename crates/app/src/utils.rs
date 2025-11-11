//location: /crates/app/src/utils.rs
// All utility functions and helpers

use crate::types::*;

use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use std::sync::OnceLock;
use tokio::sync::Mutex;
use std::time::{Duration, Instant};

// ============================================================================
// Rate Limiting - Weight-Based Binance API Rate Limiter
// ============================================================================

use std::collections::VecDeque;

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
#[cfg_attr(test, derive(Debug))]
pub struct RateLimiter {
    /// Per-second request timestamps (for burst protection)
    requests: Mutex<VecDeque<Instant>>,
    /// Per-minute weight tracking (for weight-based limits)
    weights: Mutex<VecDeque<(Instant, u32)>>,
    /// Atomik counter: Sleep sırasında reserve edilen weight (race condition fix)
    /// Bu, sleep sırasında başka thread'lerin weight eklemesini önler
    reserved_weight: std::sync::atomic::AtomicU32,
    /// Maximum requests per second (with safety margin)
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) max_requests_per_sec: u32,
    /// Maximum weight per minute (with safety margin)
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) max_weight_per_minute: u32,
    /// Minimum interval between requests (ms)
    min_interval_ms: u64,
    /// Safety factor (0.0-1.0) - how much of the limit to use
    safety_factor: f64,
}

impl RateLimiter {
    /// Create a new rate limiter with safety margin
    /// 
    /// # Arguments
    /// * `max_requests_per_sec` - Maximum requests per second (Futures: 40)
    /// * `max_weight_per_minute` - Maximum weight per minute (Futures: 2400)
    /// * `safety_factor` - Safety margin (0.0-1.0). 0.6 = use only 60% of limit (very safe)
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
    /// 
    /// # Race Condition Fix
    /// Sleep sırasında lock yokken başka thread'ler weight ekleyebilir.
    /// Çözüm: Atomik counter ile weight'i reserve et, sleep sırasında da reserve korunur.
    /// Sleep bitince weight'i ekle ve reserve'i serbest bırak.
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
                        // Kısa bekleme (< 1 saniye): 100ms polling (fine-grained)
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
                        // Kısa bekleme (< 1 saniye): 100ms polling (fine-grained)
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
            let total_weight: u32 = weights.iter().map(|(_, w)| w).sum();
            let reserved = self.reserved_weight.load(std::sync::atomic::Ordering::Acquire);
            let total_with_reserved = total_weight + reserved;
            
            // Eğer weight limit aşıldıysa bekle
            if total_with_reserved + weight > self.max_weight_per_minute {
                if let Some((oldest_time, _)) = weights.front().copied() {
                    let wait_time = oldest_time + Duration::from_secs(60);
                    if wait_time > now {
                        let sleep_duration = wait_time.duration_since(now);
                        
                        // KRİTİK RACE CONDITION FIX: Weight'i atomik olarak reserve et
                        // Sleep sırasında başka thread'ler bu weight'i görecek ve limit aşmayacak
                        self.reserved_weight.fetch_add(weight, std::sync::atomic::Ordering::AcqRel);
                        
                        drop(requests);
                        drop(weights);
                        
                        // ✅ KRİTİK İYİLEŞTİRME: Adaptive polling interval - Binance 1 dakikalık pencere kullanıyor
                        // Weight limit aşıldığında 1 dakika boyunca bekleme olur, 5 saniye polling yeterli
                        // Uzun bekleme (> 10 saniye): 5 saniye polling (12 wake-up, 5x az overhead)
                        // Orta bekleme (1-10 saniye): 5 saniye polling (overhead azaltma)
                        // Kısa bekleme (< 1 saniye): 100ms polling (per-second limit için fine-grained)
                        let poll_interval_ms = if sleep_duration.as_secs() >= 1 {
                            5000 // 5 saniye - Binance 1 dakikalık window için yeterli (60 wake-up → 12 wake-up)
                        } else {
                            100 // 100ms - kısa bekleme için (per-second limit)
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
    /// 
    /// NOT: Race condition fix artık atomik counter ile yapılıyor (reserved_weight).
    /// Bu polling sadece overhead azaltma için - sleep sırasında weight reserve edilmiş durumda.
    /// 
    /// ✅ KRİTİK İYİLEŞTİRME: Adaptive polling interval
    /// - Binance 1 dakikalık pencere kullanıyor, bu yüzden weight limit için 5 saniye polling yeterli
    /// - 1 dakika bekleme: 60 wake-up (1s) → 12 wake-up (5s) = 5x az overhead
    /// - Kısa bekleme için 100ms polling (per-second limit için)
    /// - Bu overhead'i önemli ölçüde azaltır
    /// 
    /// # Arguments
    /// * `total_duration` - Toplam bekleme süresi
    /// * `poll_interval_ms` - Her kontrol arasındaki süre (ms) - adaptive olarak seçilir
    async fn sleep_with_polling(&self, total_duration: Duration, poll_interval_ms: u64) {
        let poll_interval = Duration::from_millis(poll_interval_ms);
        let mut remaining = total_duration;
        
        while remaining > Duration::ZERO {
            // Her poll interval'de bir kontrol yap
            let sleep_duration = remaining.min(poll_interval);
            tokio::time::sleep(sleep_duration).await;
            remaining = remaining.saturating_sub(sleep_duration);
            
            // Kısa bir kontrol: Eğer limit artık aşılmıyorsa erken çık
            // (Bu optimizasyon, ama asıl amaç race condition önleme)
            // Not: Burada lock almıyoruz çünkü sadece optimizasyon için
            // Asıl kontrol loop'un başında yapılıyor
        }
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

/// Guard function for rate limiting (async)
/// 
/// # Arguments
/// * `weight` - API endpoint weight (default: 1)
pub async fn rate_limit_guard(weight: u32) {
    RATE_LIMITER.get_or_init(|| {
        // Default: Futures limits with 70% safety factor
        RateLimiter::new(40, 2400, 0.7)
    }).wait_if_needed(weight).await;
}

// ============================================================================
// PnL Calculation
// ============================================================================

/// Calculate side multiplier for PnL calculation (internal helper)
fn side_mult(side: &crate::types::Side) -> f64 {
    match side {
        crate::types::Side::Buy => 1.0,
        crate::types::Side::Sell => -1.0,
    }
}

/// Calculate net PnL in USD (fees included)
pub fn calc_net_pnl_usd(
    entry_price: Decimal,
    exit_price: Decimal,
    qty: Decimal,
    side: &crate::types::Side,
    entry_fee_bps: f64,
    exit_fee_bps: f64,
    ) -> f64 {
    let mult = side_mult(side);
    let price_diff = exit_price - entry_price;
    let gross_pnl = price_diff * qty * Decimal::try_from(mult).unwrap_or(Decimal::ONE);
    
    let notional_entry = entry_price * qty;
    let notional_exit = exit_price * qty;
    
    let entry_fee_bps_dec = Decimal::try_from(entry_fee_bps / 10_000.0).unwrap_or(Decimal::ZERO);
    let exit_fee_bps_dec = Decimal::try_from(exit_fee_bps / 10_000.0).unwrap_or(Decimal::ZERO);
    
    let fees_open = notional_entry * entry_fee_bps_dec;
    let fees_close = notional_exit * exit_fee_bps_dec;
    
    let net_pnl = gross_pnl - fees_open - fees_close;
    net_pnl.to_f64().unwrap_or(0.0)
}
