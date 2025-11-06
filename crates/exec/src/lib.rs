//location: /crates/exec/src/lib.rs
use anyhow::Result;
use async_trait::async_trait;
use bot_core::types::*;

#[derive(Clone, Debug)]
pub struct VenueOrder {
    pub order_id: String,
    pub side: Side,
    pub price: Px,
    pub qty: Qty,
}

pub mod binance;

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

// ======================= BEGIN: quant helpers =======================
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;

/// step'e göre floor: val -> en yakın aşağı step katı
pub fn quant_utils_floor_to_step(val: Decimal, step: Decimal) -> Decimal {
    if step.is_zero() {
        return val;
    }
    (val / step).floor() * step
}

/// step'e göre ceil: val -> en yakın yukarı step katı
pub fn quant_utils_ceil_to_step(val: Decimal, step: Decimal) -> Decimal {
    if step.is_zero() {
        return val;
    }
    (val / step).ceil() * step
}

/// tick'e snap (is_buy=true -> floor, is_buy=false -> ceil)
pub fn quant_utils_snap_price(raw: Decimal, tick: Decimal, is_buy: bool) -> Decimal {
    if is_buy {
        quant_utils_floor_to_step(raw, tick)
    } else {
        quant_utils_ceil_to_step(raw, tick)
    }
}

/// quote (USDT/USDC) bütçeden qty hesapla, lot step'e floor et
pub fn quant_utils_qty_from_quote(quote: Decimal, price: Decimal, lot_step: Decimal) -> Decimal {
    if price.is_zero() {
        return Decimal::ZERO;
    }
    quant_utils_floor_to_step(quote / price, lot_step)
}

/// bps farkı (|new-old|/old)*1e4
pub fn quant_utils_bps_diff(old_px: Decimal, new_px: Decimal) -> f64 {
    if old_px.is_zero() {
        return f64::INFINITY;
    }
    let num = (new_px - old_px).abs();
    (num / old_px).to_f64().unwrap_or(0.0) * 10_000.0
}

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
// ======================== END: quant helpers ========================
