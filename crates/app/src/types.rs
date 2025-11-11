//location: /crates/app/src/types.rs
// All types and structures for the trading bot

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use crate::exec::binance::SymbolMeta;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use crate::strategy::Strategy;

// ============================================================================
// Core Domain Types (moved from core.rs)
// ============================================================================

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Px(pub Decimal);

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Qty(pub Decimal);

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct BookLevel {
    pub px: Px,
    pub qty: Qty,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OrderBook {
    pub best_bid: Option<BookLevel>,
    pub best_ask: Option<BookLevel>,
    pub top_bids: Option<Vec<BookLevel>>,
    pub top_asks: Option<Vec<BookLevel>>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum Tif {
    Gtc,
    Ioc,
    PostOnly,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Quote {
    pub bid: Px,
    pub ask: Px,
    pub size: Qty,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Position {
    pub symbol: String,
    pub qty: Qty,
    pub entry: Px,
    pub leverage: u32,
    pub liq_px: Option<Px>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Fill {
    pub id: i64,
    pub symbol: String,
    pub side: Side,
    pub qty: Qty,
    pub price: Px,
    pub timestamp: u64,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct Quotes {
    pub bid: Option<(Px, Qty)>,
    pub ask: Option<(Px, Qty)>,
}

// ============================================================================
// Symbol State
// ============================================================================

/// Per-symbol state tracking
pub struct SymbolState {
    pub meta: SymbolMeta,
    pub inv: Qty,
    pub strategy: Box<dyn Strategy>,
    pub active_orders: HashMap<String, OrderInfo>,
    pub pnl_history: Vec<Decimal>,
    
    // Min notional tracking
    pub min_notional_req: Option<f64>,
    pub disabled: bool,
    
    // Per-symbol metadata
    pub symbol_rules: Option<std::sync::Arc<crate::exec::binance::SymbolRules>>,
    // KRİTİK: ExchangeInfo fetch durumu - başarısızsa trade etme
    pub rules_fetch_failed: bool, // ExchangeInfo çekilemediyse true (trade etme)
    pub last_rules_retry: Option<Instant>, // Son retry zamanı (periyodik retry için)
    pub test_order_passed: bool, // İlk emir öncesi test order başarılı mı?
    // ✅ KRİTİK FIX: test_order_passed_buy ve test_order_passed_sell kaldırıldı (kullanılmıyordu)
    
    // Position and order tracking
    pub last_position_check: Option<Instant>,
    pub last_logged_position_qty: Option<Decimal>, // Son loglanan pozisyon qty (değişiklik bazlı loglama için)
    pub last_logged_pnl: Option<Decimal>, // Son loglanan PnL (değişiklik bazlı loglama için)
    pub last_order_sync: Option<Instant>,
    pub order_fill_rate: f64,
    pub consecutive_no_fills: u32,
    pub last_fill_time: Option<Instant>, // Son fill zamanı (zaman bazlı fill rate için)
    pub last_inventory_update: Option<Instant>, // Son envanter güncelleme zamanı (race condition önleme için)
    pub last_decay_period: Option<u64>, // Son fill rate decay period (optimizasyon için)
    pub last_decay_check: Option<Instant>, // Son decay kontrol zamanı (overhead önleme için)
    
    // Position management
    pub position_entry_time: Option<Instant>,
    pub peak_pnl: Decimal,
    pub position_hold_duration_ms: u64,
    pub last_order_price_update: HashMap<String, Px>,
    // ✅ KRİTİK FIX: position_orders tracking kaldırıldı (kullanılmıyordu, sadece overhead yaratıyordu)
    // Eğer gelecekte partial close veya order-to-position mapping gerekirse, o zaman eklenebilir
    
    // Advanced tracking
    pub daily_pnl: Decimal,
    pub total_funding_cost: Decimal,
    pub position_size_notional_history: Vec<f64>,
    pub last_pnl_alert: Option<Instant>,
    pub cumulative_pnl: Decimal,
    
    // Funding cost tracking (futures)
    // KRİTİK: Son uygulanan funding time'ı takip et (8 saatte bir tek seferde uygula)
    pub last_applied_funding_time: Option<u64>, // Unix timestamp (ms) - son uygulanan funding time
    
    // PnL tracking
    pub last_daily_reset: Option<u64>, // Unix timestamp (ms) - son günlük reset zamanı
    pub avg_entry_price: Option<Decimal>, // Ortalama entry price (pozisyon açılırken güncellenir)
    
    // Long/Short seçimi için histerezis ve cooldown
    pub last_direction_change: Option<Instant>, // Son yön değişikliği zamanı
    pub current_direction: Option<Side>, // Mevcut yön (Long=Buy, Short=Sell)
    pub direction_signal_strength: f64, // Sinyal gücü (0.0-1.0)
    
    // ✅ KRİTİK İYİLEŞTİRME: Price momentum tracking (trend analizi için)
    pub price_history: Vec<(Instant, Decimal)>, // (timestamp, price) - son 10 fiyat noktası
    pub price_momentum_bps: f64, // Son 5 fiyat değişiminin ortalaması (bps)
    
    // Position closing control
    // ✅ KRİTİK: Thread-safe flag (race condition önleme)
    // WebSocket ve REST API aynı anda close edebilir, AtomicBool ile korunuyor
    pub position_closing: Arc<AtomicBool>, // Pozisyon kapatma süreci başlamış mı (spam önleme)
    pub last_close_attempt: Option<Instant>, // Son kapatma denemesi zamanı (cooldown için)
    
    // PnL tracking for summary
    pub trade_count: u64, // Toplam işlem sayısı
    pub profitable_trade_count: u64, // Kazançlı işlem sayısı
    pub losing_trade_count: u64, // Zararlı işlem sayısı
    pub total_profit: Decimal, // Toplam kazanç
    pub total_loss: Decimal, // Toplam zarar
    pub largest_win: Decimal, // En büyük kazanç
    pub largest_loss: Decimal, // En büyük zarar
    pub total_fees_paid: Decimal, // Toplam ödenen fees
    pub last_pnl_summary_time: Option<Instant>, // Son PnL özeti zamanı
    
    // WebSocket event deduplication
    pub processed_events: HashSet<String>, // İşlenmiş event ID'leri (duplicate önleme için)
    pub last_event_cleanup: Option<Instant>, // Son event cleanup zamanı (memory leak önleme için)
}

// ============================================================================
// Order Info
// ============================================================================

/// Order information for tracking
/// KRİTİK: Partial fill ve idempotency desteği
#[derive(Clone, Debug)]
pub struct OrderInfo {
    pub order_id: String,
    pub client_order_id: Option<String>, // Idempotency için client order ID
    pub side: Side,
    pub price: Px,
    pub qty: Qty, // Original order quantity
    pub filled_qty: Qty, // Cumulative filled quantity
    pub remaining_qty: Qty, // Remaining quantity (qty - filled_qty)
    pub created_at: Instant,
    pub last_fill_time: Option<Instant>, // Son fill zamanı
}

