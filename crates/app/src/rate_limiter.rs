//location: /crates/app/src/rate_limiter.rs
// Rate limiting for API calls

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

// ============================================================================
// Rate Limiter
// ============================================================================

/// Rate limiter for API calls to prevent exceeding exchange limits
pub struct RateLimiter {
    requests: Mutex<VecDeque<Instant>>,
    max_requests_per_sec: u32,
    min_interval_ms: u64,
}

impl RateLimiter {
    /// Create a new rate limiter with safety margin
    pub fn new(max_requests_per_sec: u32) -> Self {
        // Güvenlik için %20 margin ekle (20 req/sec → 16 req/sec kullan)
        let safety_factor = 0.8;
        let safe_limit = (max_requests_per_sec as f64 * safety_factor) as u32;
        let min_interval_ms = 1000 / safe_limit as u64;
        
        Self {
            requests: Mutex::new(VecDeque::new()),
            max_requests_per_sec: safe_limit,
            min_interval_ms,
        }
    }
    
    /// Wait if needed to respect rate limits
    pub async fn wait_if_needed(&self) {
        loop {
            let now = Instant::now();
            let mut requests = self.requests.lock().unwrap();
            
            // Son 1 saniyede yapılan request'leri temizle
            let one_sec_ago = now.checked_sub(Duration::from_secs(1))
                .unwrap_or(Instant::now());
            while requests.front().map_or(false, |&t| t < one_sec_ago) {
                requests.pop_front();
            }
            
            // Eğer limit aşıldıysa bekle
            if requests.len() >= self.max_requests_per_sec as usize {
                if let Some(oldest) = requests.front().copied() {
                    let wait_time = oldest + Duration::from_secs(1);
                    if wait_time > now {
                        let sleep_duration = wait_time.duration_since(now);
                        drop(requests); // Lock'u bırak
                        tokio::time::sleep(sleep_duration).await;
                        continue; // Tekrar kontrol et
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
                        tokio::time::sleep(wait).await;
                        continue; // Tekrar kontrol et
                    }
                }
            }
            
            // Request'i kaydet ve çık
            requests.push_back(now);
            break;
        }
    }
}

// ============================================================================
// Global Rate Limiter
// ============================================================================

/// Global rate limiter instance (Spot limit kullan - en güvenli)
static RATE_LIMITER: OnceLock<RateLimiter> = OnceLock::new();

/// Get or initialize the global rate limiter
pub fn get_rate_limiter() -> &'static RateLimiter {
    RATE_LIMITER.get_or_init(|| {
        RateLimiter::new(20) // Spot: 20 req/sec, güvenlik için 16 req/sec kullan
    })
}

/// Guard function for rate limiting (async)
pub async fn rate_limit_guard() {
    get_rate_limiter().wait_if_needed().await;
}

