//location: /crates/strategy/src/lib.rs
use bot_core::types::*;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

pub struct Context {
    pub ob: OrderBook,
    pub sigma: f64,
    pub inv: Qty,
    pub liq_gap_bps: f64,
    pub funding_rate: Option<f64>,
    pub next_funding_time: Option<u64>,
}

pub trait Strategy: Send + Sync {
    fn on_tick(&mut self, ctx: &Context) -> Quotes;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DynMmCfg {
    pub a: f64,
    pub b: f64,
    pub base_size: Decimal,
    pub inv_cap: Decimal,
}

pub struct DynMm {
    pub a: f64,
    pub b: f64,
    pub base_notional: Decimal,
    pub inv_cap: Qty,
}

impl From<DynMmCfg> for DynMm {
    fn from(c: DynMmCfg) -> Self {
        Self {
            a: c.a,
            b: c.b,
            base_notional: c.base_size,
            inv_cap: Qty(c.inv_cap),
        }
    }
}

impl Strategy for DynMm {
    fn on_tick(&mut self, c: &Context) -> Quotes {
        let (bid, ask) = match (c.ob.best_bid, c.ob.best_ask) {
            (Some(b), Some(a)) => (b.px.0, a.px.0),
            _ => return Quotes::default(),
        };
        let mid = (bid + ask) / Decimal::from(2u32);
        let mid_f = mid.to_f64().unwrap_or(0.0);
        if mid_f <= 0.0 {
            return Quotes::default();
        }
        // Envanter bias: pozitif envanter varsa ask'i yukarı, bid'i aşağı çek (satmaya zorla)
        // Negatif envanter varsa bid'i yukarı, ask'i aşağı çek (almaya zorla)
        let inv_bias = if self.inv_cap.0.is_zero() {
            0.0
        } else {
            (c.inv.0 / self.inv_cap.0).to_f64().unwrap_or(0.0).abs()
        };
        let inv_direction = if c.inv.0.is_sign_positive() {
            1.0 // Pozitif envanter: ask'i yukarı, bid'i aşağı
        } else if c.inv.0.is_sign_negative() {
            -1.0 // Negatif envanter: bid'i yukarı, ask'i aşağı
        } else {
            0.0
        };
        
        // Base spread hesaplama
        let mut spread_bps = (self.a * c.sigma + self.b * inv_bias).max(1e-4);
        
        // Likidasyon riski: yakınsa spread'i genişlet
        if c.liq_gap_bps < 300.0 {
            spread_bps *= 1.5;
        }
        
        // Funding rate skew: pozitif funding'de ask'i yukarı çek (long pozisyon için daha iyi)
        let funding_skew = c.funding_rate.unwrap_or(0.0) * 100.0;
        
        // Envanter yönüne göre asimetrik spread: envanter varsa o tarafı daha agresif yap
        let inv_skew_bps = inv_direction * inv_bias * 20.0; // Envanter bias'ına göre ekstra skew
        
        let half = Decimal::try_from(spread_bps / 2.0 / 1e4).unwrap_or(Decimal::ZERO);
        let skew = Decimal::try_from(funding_skew / 1e4).unwrap_or(Decimal::ZERO);
        let inv_skew = Decimal::try_from(inv_skew_bps / 1e4).unwrap_or(Decimal::ZERO);
        
        // Bid: mid'den aşağı (half + funding_skew + inv_skew)
        // Pozitif envanter varsa inv_skew pozitif, bid daha aşağı (satmaya zorla)
        // Negatif envanter varsa inv_skew negatif, bid daha yukarı (almaya zorla)
        let bid_px = Px(mid * (Decimal::ONE - half - skew - inv_skew));
        
        // Ask: mid'den yukarı (half + funding_skew + inv_skew)
        // Pozitif envanter varsa inv_skew pozitif, ask daha yukarı (satmaya zorla)
        // Negatif envanter varsa inv_skew negatif, ask daha aşağı (almaya zorla)
        let ask_px = Px(mid * (Decimal::ONE + half + skew + inv_skew));
        let usd_size = self.base_notional.to_f64().unwrap_or(0.0);
        let qty = if usd_size > 0.0 {
            usd_size / mid_f
        } else {
            0.0
        };
        let qty = Qty(Decimal::from_f64_retain(qty).unwrap_or(Decimal::ZERO));
        Quotes {
            bid: Some((bid_px, qty)),
            ask: Some((ask_px, qty)),
        }
    }
}
