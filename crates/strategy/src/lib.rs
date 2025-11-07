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
    pub mark_price: Px, // Mark price (futures için)
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
    // Akıllı karar verme için state
    price_history: Vec<(u64, Decimal)>, // (timestamp_ms, price)
    target_inventory: Qty, // Hedef envanter seviyesi
}

impl From<DynMmCfg> for DynMm {
    fn from(c: DynMmCfg) -> Self {
        Self {
            a: c.a,
            b: c.b,
            base_notional: c.base_size,
            inv_cap: Qty(c.inv_cap),
            price_history: Vec::with_capacity(100), // Son 100 fiyat
            target_inventory: Qty(Decimal::ZERO), // Başlangıçta nötr
        }
    }
}

impl DynMm {
    // Fiyat trend analizi: son N fiyatın ortalamasına göre trend
    fn detect_trend(&self) -> f64 {
        if self.price_history.len() < 10 {
            return 0.0; // Yeterli veri yok
        }
        let recent: Vec<Decimal> = self.price_history
            .iter()
            .rev()
            .take(10)
            .map(|(_, p)| *p)
            .collect();
        let old_avg: Decimal = recent.iter().take(5).sum::<Decimal>() / Decimal::from(5);
        let new_avg: Decimal = recent.iter().skip(5).sum::<Decimal>() / Decimal::from(5);
        if old_avg.is_zero() {
            return 0.0;
        }
        ((new_avg - old_avg) / old_avg).to_f64().unwrap_or(0.0) * 10000.0 // bps
    }
    
    // Funding rate analizi: pozitif funding = long bias, negatif = short bias
    fn funding_bias(&self, funding_rate: Option<f64>) -> f64 {
        funding_rate.unwrap_or(0.0) * 10000.0 // bps cinsinden
    }
    
    // Hedef envanter hesaplama: funding rate ve trend'e göre
    fn calculate_target_inventory(&mut self, funding_rate: Option<f64>, trend_bps: f64) -> Qty {
        // Funding rate pozitifse long (pozitif envanter), negatifse short (negatif envanter)
        let funding_bias = self.funding_bias(funding_rate);
        // Trend yukarıysa long, aşağıysa short
        let trend_bias = trend_bps * 0.5; // Trend'in %50'si kadar etkili
        
        // Kombine bias: funding + trend
        let combined_bias = funding_bias + trend_bias;
        
        // Hedef envanter: bias'a göre inv_cap'in bir yüzdesi
        // Tanh benzeri fonksiyon: -1 ile 1 arası sınırla
        let bias_f64 = combined_bias / 100.0;
        let target_ratio = if bias_f64 > 10.0 {
            1.0
        } else if bias_f64 < -10.0 {
            -1.0
        } else {
            // Basit sigmoid: x / (1 + |x|)
            bias_f64 / (1.0 + bias_f64.abs())
        };
        let target = self.inv_cap.0 * Decimal::from_f64_retain(target_ratio).unwrap_or(Decimal::ZERO);
        Qty(target)
    }
    
    // Envanter yönetimi: hedef envantere göre al/sat kararı
    fn inventory_decision(&self, current_inv: Qty, target_inv: Qty) -> (bool, bool) {
        let diff = (current_inv.0 - target_inv.0).abs();
        let threshold = self.inv_cap.0 * Decimal::from_f64_retain(0.1).unwrap_or(Decimal::ZERO); // %10 threshold
        
        if diff < threshold {
            // Hedef envantere yakınsa: market making (her iki taraf)
            (true, true)
        } else if current_inv.0 < target_inv.0 {
            // Mevcut envanter hedeften düşük: sadece al (bid)
            (true, false)
        } else {
            // Mevcut envanter hedeften yüksek: sadece sat (ask)
            (false, true)
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
        
        // Fiyat geçmişini güncelle (basit timestamp simülasyonu)
        use std::time::{SystemTime, UNIX_EPOCH};
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        self.price_history.push((now_ms, c.mark_price.0));
        if self.price_history.len() > 100 {
            self.price_history.remove(0);
        }
        
        // Trend analizi
        let trend_bps = self.detect_trend();
        
        // Hedef envanter hesapla (funding rate ve trend'e göre)
        self.target_inventory = self.calculate_target_inventory(c.funding_rate, trend_bps);
        
        // Envanter kararı: hedef envantere göre al/sat
        let (should_bid, should_ask) = self.inventory_decision(c.inv, self.target_inventory);
        
        // Debug log (tracing kullanarak)
        use tracing::debug;
        debug!(
            current_inv = %c.inv.0,
            target_inv = %self.target_inventory.0,
            trend_bps,
            funding_rate = ?c.funding_rate,
            should_bid,
            should_ask,
            "strategy decision"
        );
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
        
        // Akıllı karar: hedef envantere göre sadece gerekli tarafı koy
        Quotes {
            bid: if should_bid { Some((bid_px, qty)) } else { None },
            ask: if should_ask { Some((ask_px, qty)) } else { None },
        }
    }
}
