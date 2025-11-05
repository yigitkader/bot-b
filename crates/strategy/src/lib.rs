use bot_core::types::*;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use rust_decimal::prelude::ToPrimitive;

pub struct Context {
    pub ob: OrderBook,
    pub sigma: f64,
    pub inv: Qty,
    pub liq_gap_bps: f64,
}

pub trait Strategy: Send + Sync {
    fn on_tick(&mut self, ctx: &Context) -> Quotes;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DynMmCfg { pub a: f64, pub b: f64, pub base_size: Decimal, pub inv_cap: Decimal }

pub struct DynMm { pub a: f64, pub b: f64, pub base_size: Qty, pub inv_cap: Qty }

impl From<DynMmCfg> for DynMm {
    fn from(c: DynMmCfg) -> Self { Self { a: c.a, b: c.b, base_size: Qty(c.base_size), inv_cap: Qty(c.inv_cap) } }
}

impl Strategy for DynMm {
    fn on_tick(&mut self, c: &Context) -> Quotes {
        let (bid, ask) = match (c.ob.best_bid, c.ob.best_ask) {
            (Some(b), Some(a)) => (b.px.0, a.px.0),
            _ => return Quotes::default(),
        };
        let mid = (bid + ask) / Decimal::from(2u32);
        let inv_bias = if self.inv_cap.0.is_zero() { 0.0 } else {
            (c.inv.0 / self.inv_cap.0).to_f64().unwrap_or(0.0).abs()
        };
        let mut spread_bps = (self.a * c.sigma + self.b * inv_bias).max(1e-4);
        if c.liq_gap_bps < 300.0 { spread_bps *= 1.5; }
        let half = Decimal::try_from(spread_bps / 2.0 / 1e4).unwrap_or(Decimal::ZERO);
        let bid_px = Px(mid * (Decimal::ONE - half));
        let ask_px = Px(mid * (Decimal::ONE + half));
        Quotes { bid: Some((bid_px, self.base_size)), ask: Some((ask_px, self.base_size)) }
    }
}
