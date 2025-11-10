//location: /crates/exec/src/lib.rs
use anyhow::Result;
use async_trait::async_trait;
use crate::core::types::*;
use rust_decimal::Decimal;

#[derive(Clone, Debug)]
pub struct VenueOrder {
    pub order_id: String,
    pub side: Side,
    pub price: Px,
    pub qty: Qty,
}

// binance_exec is a separate module file

#[async_trait]
pub trait Venue: Send + Sync {
    async fn place_limit(
        &self,
        sym: &str,
        side: Side,
        px: Px,
        qty: Qty,
        tif: Tif,
    ) -> Result<String>;
    
    /// Place limit order with client order ID (for futures idempotency)
    async fn place_limit_with_client_id(
        &self,
        sym: &str,
        side: Side,
        px: Px,
        qty: Qty,
        tif: Tif,
        client_order_id: &str,
    ) -> Result<(String, Option<String>)>; // Returns (order_id, client_order_id)
    async fn cancel(&self, order_id: &str, sym: &str) -> Result<()>;
    async fn best_prices(&self, sym: &str) -> Result<(Px, Px)>;
    async fn get_open_orders(&self, sym: &str) -> Result<Vec<VenueOrder>>;
    async fn get_position(&self, sym: &str) -> Result<Position>;
    async fn mark_price(&self, sym: &str) -> Result<Px>;
    async fn close_position(&self, sym: &str) -> Result<()>;

    async fn cancel_all(&self, sym: &str) -> Result<()> {
        let orders = self.get_open_orders(sym).await?;
        for order in orders {
            self.cancel(&order.order_id, sym).await?;
        }
        Ok(())
    }
}

// Quantization helpers moved to utils.rs to avoid duplication
// Re-export for backward compatibility
// Re-export binance_exec types for convenience (binance_exec is a separate module)
pub mod binance {
    pub use crate::binance_exec::*;
}

// Re-export quantization helpers from utils
pub use crate::utils::{
    quant_utils_floor_to_step,
    quant_utils_ceil_to_step,
    quant_utils_snap_price,
    quant_utils_qty_from_quote,
    quant_utils_bps_diff,
    quantize_decimal,
};

/// Decimal adımından hassasiyet (ondalık hane sayısı) çıkarır
pub fn decimal_places(step: Decimal) -> usize {
    if step.is_zero() {
        return 0;
    }
    let s = step.normalize().to_string();
    if let Some(pos) = s.find('.') {
        s[pos + 1..].trim_end_matches('0').len()
    } else {
        0
    }
}
