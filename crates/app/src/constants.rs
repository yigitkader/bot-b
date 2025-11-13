//location: /crates/app/src/constants.rs
// Application-wide constants

// ============================================================================
// Risk & Position Constants
// ============================================================================

/// Default liquidation gap when position has no liquidation price
pub const DEFAULT_LIQ_GAP_BPS: f64 = 9_999.0;

/// Fill rate decay threshold (seconds without fills)
pub const FILL_RATE_DECAY_THRESHOLD_SEC: u64 = 30;

/// Fill rate decay check interval (seconds)
pub const FILL_RATE_DECAY_CHECK_INTERVAL_SEC: u64 = 5;

/// Fill rate decay interval (seconds)
pub const FILL_RATE_DECAY_INTERVAL_SEC: u64 = 30;

/// Fill rate decay multiplier (reduce by 10%)
pub const FILL_RATE_DECAY_MULTIPLIER: f64 = 0.9;

/// Low fill rate threshold (below this is considered problematic)
pub const LOW_FILL_RATE_THRESHOLD: f64 = 0.2;

/// Minimum profit guarantee (USD)
pub const DEFAULT_MIN_PROFIT_USD: f64 = 0.50;

/// Depth analysis volume multiplier (50% of notional)
pub const DEPTH_VOLUME_MULTIPLIER: f64 = 0.5;

/// Safety margin for min spread calculation (bps)
/// Covers: slippage (~1-5 bps), partial fill risks, market volatility
pub const MIN_SPREAD_SAFETY_MARGIN_BPS: f64 = 5.0;

/// Maximum position hold duration in loss (seconds)
/// If position is in loss for longer than this, force close
/// KRİTİK DÜZELTME: Market making için 2 saniye çok kısa - slippage ve fee'ler nedeniyle zarar eder
/// 30 saniye daha makul bir süre - pozisyonun kar etmesi için yeterli zaman verir
pub const MAX_LOSS_DURATION_SEC: f64 = 30.0; // 30 saniye

/// Maximum position hold duration overall (seconds)
/// Absolute timeout - pozisyon ne durumda olursa olsun bu süre sonra kapat
/// KRİTİK DÜZELTME: Market making için 5 saniye çok kısa - pozisyonların kar etmesi için yeterli zaman yok
/// 2 dakika (120 saniye) daha makul - market making stratejisi için yeterli süre
pub const MAX_POSITION_DURATION_SEC: f64 = 120.0; // 2 dakika (120 saniye)

// ============================================================================
// Decimal Constants
// ============================================================================

// Decimal constants removed - use Decimal::ZERO and Decimal::ONE directly

// ============================================================================
// Fee Conversion
// ============================================================================

/// Convert fee rate to basis points
#[inline]
pub fn fee_rate_to_bps(rate: f64) -> f64 {
    rate * 10_000.0
}
