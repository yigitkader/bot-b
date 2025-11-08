//location: /crates/app/src/types.rs
// Core types and structures for the trading bot

use bot_core::types::*;
use exec::binance::SymbolMeta;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::time::Instant;
use strategy::Strategy;

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
    pub symbol_rules: Option<std::sync::Arc<exec::binance::SymbolRules>>,
    
    // Position and order tracking
    pub last_position_check: Option<Instant>,
    pub last_order_sync: Option<Instant>,
    pub order_fill_rate: f64,
    pub consecutive_no_fills: u32,
    pub last_fill_time: Option<Instant>, // Son fill zamanı (zaman bazlı fill rate için)
    pub last_inventory_update: Option<Instant>, // Son envanter güncelleme zamanı (race condition önleme için)
    
    // Position management
    pub position_entry_time: Option<Instant>,
    pub peak_pnl: Decimal,
    pub position_hold_duration_ms: u64,
    pub last_order_price_update: HashMap<String, Px>,
    
    // Advanced tracking
    pub daily_pnl: Decimal,
    pub total_funding_cost: Decimal,
    pub position_size_notional_history: Vec<f64>,
    pub last_pnl_alert: Option<Instant>,
    pub cumulative_pnl: Decimal,
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

