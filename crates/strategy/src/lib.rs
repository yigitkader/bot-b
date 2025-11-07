//location: /crates/strategy/src/lib.rs
use bot_core::types::*;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Manipülasyon fırsatı: Tespit edilen manipülasyonu avantaja çevirme stratejisi
#[derive(Clone, Debug)]
enum ManipulationOpportunity {
    /// Flash crash tespit edildi: Fiyat anormal düştü → LONG fırsatı
    FlashCrashLong { price_drop_bps: f64 },
    /// Flash pump tespit edildi: Fiyat anormal yükseldi → SHORT fırsatı
    FlashPumpShort { price_rise_bps: f64 },
    /// Geniş spread: Market maker olarak spread'den kazanç
    WideSpreadArbitrage { spread_bps: f64 },
    /// Volume anomali: Büyük oyuncu hareketi → Trend takibi
    VolumeAnomalyTrend { direction: f64, volume_ratio: f64 },
}

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
    // --- MİKRO-YAPI SİNYALLERİ: Gelişmiş algoritmalar ---
    ewma_volatility: f64,        // EWMA volatilite (σ²)
    ewma_volatility_alpha: f64,  // EWMA decay factor (λ)
    ofi_signal: f64,             // Order Flow Imbalance (kümülatif)
    ofi_window_ms: u64,          // OFI penceresi (ms)
    last_mid_price: Option<Decimal>, // Son mid price (volatilite için)
    last_timestamp_ms: Option<u64>,  // Son timestamp (OFI için)
    // --- MANİPÜLASYON KORUMA VE FIRSAT: Anti-manipulation + opportunity detection ---
    flash_crash_detected: bool,      // Flash crash tespit edildi mi?
    flash_crash_direction: f64,       // Flash crash yönü: pozitif = pump, negatif = dump
    last_spread_bps: f64,            // Son spread (bps) - anomali tespiti için
    volume_history: Vec<f64>,         // Volume geçmişi (anomali tespiti için)
    price_jump_threshold_bps: f64,   // Flash crash eşiği (bps)
    max_spread_bps: f64,             // Maksimum kabul edilebilir spread (bps)
    min_liquidity_required: f64,     // Minimum likidite gereksinimi
    manipulation_opportunity: Option<ManipulationOpportunity>, // Manipülasyon fırsatı
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
            // Mikro-yapı sinyalleri başlangıç değerleri
            ewma_volatility: 0.0001,      // Başlangıç volatilite (1 bps)
            ewma_volatility_alpha: 0.95,   // EWMA decay: %95 eski, %5 yeni
            ofi_signal: 0.0,              // Başlangıçta nötr
            ofi_window_ms: 200,           // 200ms OFI penceresi
            last_mid_price: None,
            last_timestamp_ms: None,
            // Manipülasyon koruma başlangıç değerleri
            flash_crash_detected: false,
            flash_crash_direction: 0.0,
            last_spread_bps: 0.0,
            volume_history: Vec::with_capacity(50), // Son 50 volume
            price_jump_threshold_bps: 200.0, // 200 bps (2%) ani değişim = flash crash
            max_spread_bps: 100.0,         // 100 bps (1%) max spread
            min_liquidity_required: 0.01,   // Minimum likidite (USD)
            manipulation_opportunity: None,
        }
    }
}

impl DynMm {
    // --- MİKRO-YAPI SİNYALLERİ: Gelişmiş algoritmalar ---
    
    /// Microprice hesaplama: Volume-weighted mid price
    /// mp = (A·B_v + B·A_v) / (A_v + B_v)
    /// A = best ask, B = best bid, A_v/B_v = volumes
    fn calculate_microprice(&self, bid: Decimal, ask: Decimal, bid_vol: Decimal, ask_vol: Decimal) -> Decimal {
        if bid_vol.is_zero() && ask_vol.is_zero() {
            return (bid + ask) / Decimal::from(2u32); // Fallback to mid
        }
        let total_vol = bid_vol + ask_vol;
        if total_vol.is_zero() {
            return (bid + ask) / Decimal::from(2u32);
        }
        // Microprice: volume-weighted average
        (ask * bid_vol + bid * ask_vol) / total_vol
    }
    
    /// Order Book Imbalance (top-k): (BidVol - AskVol) / (BidVol + AskVol)
    /// k=1 için best bid/ask volumes kullanılır
    fn calculate_imbalance(&self, bid_vol: Decimal, ask_vol: Decimal) -> f64 {
        let total_vol = bid_vol + ask_vol;
        if total_vol.is_zero() {
            return 0.0;
        }
        let imbalance = (bid_vol - ask_vol) / total_vol;
        imbalance.to_f64().unwrap_or(0.0)
    }
    
    /// EWMA Volatilite güncelleme: σ²_t = λ·σ²_{t-1} + (1-λ)·r²_t
    /// r_t = log return veya basit return
    fn update_volatility(&mut self, current_mid: Decimal) {
        if let Some(last_mid) = self.last_mid_price {
            if !last_mid.is_zero() {
                // Basit return: (current - last) / last
                let return_val = (current_mid - last_mid) / last_mid;
                let return_squared = return_val * return_val;
                let return_sq_f64 = return_squared.to_f64().unwrap_or(0.0);
                
                // EWMA: σ²_t = λ·σ²_{t-1} + (1-λ)·r²_t
                self.ewma_volatility = self.ewma_volatility_alpha * self.ewma_volatility 
                    + (1.0 - self.ewma_volatility_alpha) * return_sq_f64;
            }
        }
        self.last_mid_price = Some(current_mid);
    }
    
    /// OFI (Order Flow Imbalance) güncelleme: Tick bazlı akış analizi
    /// Basit versiyon: mid price değişimine göre OFI tahmini
    /// Gerçek OFI için order book update'leri gerekir, şimdilik mid price momentum kullanıyoruz
    fn update_ofi(&mut self, current_mid: Decimal, timestamp_ms: u64) {
        if let (Some(last_mid), Some(last_ts)) = (self.last_mid_price, self.last_timestamp_ms) {
            let dt_ms = timestamp_ms.saturating_sub(last_ts);
            if dt_ms > 0 && dt_ms <= self.ofi_window_ms {
                // Mid price değişimi: pozitif = buy pressure, negatif = sell pressure
                let mid_change = current_mid - last_mid;
                let mid_change_f64 = mid_change.to_f64().unwrap_or(0.0);
                
                // OFI sinyali: momentum'a göre (basitleştirilmiş)
                // Gerçek OFI için order book update'leri gerekir
                let ofi_increment = mid_change_f64 * 1000.0; // Scale
                
                // Exponential decay: eski OFI'yı azalt
                let decay_factor = (dt_ms as f64 / self.ofi_window_ms as f64).min(1.0);
                self.ofi_signal = self.ofi_signal * (1.0 - decay_factor * 0.1) + ofi_increment * decay_factor;
            }
        }
        self.last_timestamp_ms = Some(timestamp_ms);
    }
    
    /// Adaptif spread: max(min_spread, c₁·σ + c₂·|OFI|)
    fn calculate_adaptive_spread(&self, base_spread_bps: f64, min_spread_bps: f64) -> f64 {
        // Volatilite bileşeni: c₁·σ (σ = sqrt(σ²))
        let vol_component = (self.ewma_volatility.sqrt() * 10000.0).max(0.0); // bps'e çevir
        let c1 = 2.0; // Volatilite katsayısı
        
        // OFI bileşeni: c₂·|OFI|
        let ofi_component = self.ofi_signal.abs() * 100.0; // Scale to bps
        let c2 = 0.5; // OFI katsayısı
        
        // Adaptif spread
        let adaptive = (c1 * vol_component + c2 * ofi_component).max(min_spread_bps);
        base_spread_bps.max(adaptive)
    }
    
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
        
        // Volume'ları al (best bid/ask için)
        let bid_vol = c.ob.best_bid.map(|b| b.qty.0).unwrap_or(Decimal::ONE);
        let ask_vol = c.ob.best_ask.map(|a| a.qty.0).unwrap_or(Decimal::ONE);
        
        // 3. Klasik mid price (fallback)
        let mid = (bid + ask) / Decimal::from(2u32);
        let mid_f = mid.to_f64().unwrap_or(0.0);
        if mid_f <= 0.0 {
            return Quotes::default();
        }
        
        // --- MANİPÜLASYON FIRSAT ANALİZİ: Manipülasyonu avantaja çevir ---
        self.manipulation_opportunity = None;
        
        // 1. FLASH CRASH/PUMP DETECTION: Ani fiyat değişimleri → LONG/SHORT fırsatı
        if !self.price_history.is_empty() {
            let last_price = self.price_history.last().map(|(_, p)| *p).unwrap_or(mid);
            if !last_price.is_zero() {
                let price_change = (mid - last_price) / last_price;
                let price_change_bps = price_change.to_f64().unwrap_or(0.0) * 10000.0;
                self.flash_crash_direction = price_change_bps;
                
                // Ani değişim tespiti: 200 bps (2%) veya daha fazla
                if price_change_bps.abs() > self.price_jump_threshold_bps {
                    self.flash_crash_detected = true;
                    
                    // FIRSAT: Flash crash → LONG, Flash pump → SHORT
                    if price_change_bps < -self.price_jump_threshold_bps {
                        // Fiyat anormal düştü → LONG fırsatı (dip alım)
                        self.manipulation_opportunity = Some(ManipulationOpportunity::FlashCrashLong {
                            price_drop_bps: price_change_bps.abs(),
                        });
                        use tracing::info;
                        info!(
                            price_drop_bps = price_change_bps.abs(),
                            "FLASH CRASH OPPORTUNITY: going LONG (buying the dip)"
                        );
                    } else if price_change_bps > self.price_jump_threshold_bps {
                        // Fiyat anormal yükseldi → SHORT fırsatı (tepe satış)
                        self.manipulation_opportunity = Some(ManipulationOpportunity::FlashPumpShort {
                            price_rise_bps: price_change_bps,
                        });
                        use tracing::info;
                        info!(
                            price_rise_bps = price_change_bps,
                            "FLASH PUMP OPPORTUNITY: going SHORT (selling the top)"
                        );
                    }
                } else {
                    // Normal piyasa, flash crash yok
                    self.flash_crash_detected = false;
                }
            }
        }
        
        // 2. SPREAD ARBITRAGE: Geniş spread → Market maker fırsatı
        let spread = ask - bid;
        let spread_bps = if mid_f > 0.0 {
            (spread / mid).to_f64().unwrap_or(0.0) * 10000.0
        } else {
            0.0
        };
        self.last_spread_bps = spread_bps;
        
        // FIRSAT: Geniş spread varsa market maker olarak spread'den kazanç
        if spread_bps > 50.0 && spread_bps <= 200.0 {
            // 50-200 bps arası spread → Arbitraj fırsatı (çok geniş değil, kabul edilebilir)
            self.manipulation_opportunity = Some(ManipulationOpportunity::WideSpreadArbitrage {
                spread_bps,
            });
            use tracing::info;
            info!(
                spread_bps,
                "WIDE SPREAD OPPORTUNITY: market making with wider spread"
            );
        } else if spread_bps > self.max_spread_bps {
            // Çok geniş spread (>200 bps) → Risk çok yüksek, işlem yapma
            use tracing::warn;
            warn!(
                spread_bps,
                max_allowed = self.max_spread_bps,
                "SPREAD TOO WIDE: refusing to trade (too risky)"
            );
            return Quotes::default();
        }
        
        // 3. LIQUIDITY CHECK: Yeterli likidite yoksa işlem yapma
        let bid_notional = (bid * bid_vol).to_f64().unwrap_or(0.0);
        let ask_notional = (ask * ask_vol).to_f64().unwrap_or(0.0);
        let min_liquidity = bid_notional.min(ask_notional);
        
        if min_liquidity < self.min_liquidity_required {
            use tracing::warn;
            warn!(
                min_liquidity,
                required = self.min_liquidity_required,
                "INSUFFICIENT LIQUIDITY: refusing to trade"
            );
            return Quotes::default(); // Likidite yetersiz
        }
        
        // 4. VOLUME ANOMALY TREND: Anormal volume → Trend takibi fırsatı
        let total_volume = bid_vol + ask_vol;
        let total_volume_f = total_volume.to_f64().unwrap_or(0.0);
        self.volume_history.push(total_volume_f);
        if self.volume_history.len() > 50 {
            self.volume_history.remove(0);
        }
        
        let _volume_anomaly_direction = if self.volume_history.len() >= 10 {
            // Son 10 volume'un ortalaması
            let recent_avg: f64 = self.volume_history.iter().rev().take(10).sum::<f64>() / 10.0;
            // Önceki 10 volume'un ortalaması
            let older_avg: f64 = if self.volume_history.len() >= 20 {
                self.volume_history.iter().rev().skip(10).take(10).sum::<f64>() / 10.0
            } else {
                recent_avg
            };
            
            // Volume 3x veya daha fazla arttıysa anomali
            if older_avg > 0.0 && recent_avg > older_avg * 3.0 {
                let volume_ratio = recent_avg / older_avg;
                // Trend yönü: Fiyat yükseliyorsa +1, düşüyorsa -1
                let trend_direction = if !self.price_history.is_empty() && self.price_history.len() >= 2 {
                    let recent_prices: Vec<Decimal> = self.price_history.iter().rev().take(5).map(|(_, p)| *p).collect();
                    if recent_prices.len() >= 2 {
                        let price_trend = (recent_prices[0] - recent_prices[recent_prices.len() - 1]).to_f64().unwrap_or(0.0);
                        if price_trend > 0.0 { 1.0 } else { -1.0 }
                    } else {
                        0.0
                    }
                } else {
                    0.0
                };
                
                // FIRSAT: Volume anomali + trend → Trend takibi
                if trend_direction != 0.0 {
                    self.manipulation_opportunity = Some(ManipulationOpportunity::VolumeAnomalyTrend {
                        direction: trend_direction,
                        volume_ratio,
                    });
                    use tracing::info;
                    info!(
                        volume_ratio,
                        trend_direction,
                        "VOLUME ANOMALY OPPORTUNITY: following trend"
                    );
                }
            }
            0.0 // No anomaly
        } else {
            0.0
        };
        
        // --- MİKRO-YAPI SİNYALLERİ: Gelişmiş fiyatlama ---
        // 1. Microprice hesapla (volume-weighted mid)
        let microprice = self.calculate_microprice(bid, ask, bid_vol, ask_vol);
        
        // 2. Order Book Imbalance
        let imbalance = self.calculate_imbalance(bid_vol, ask_vol);
        
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
        
        // --- VOLATİLİTE VE OFI GÜNCELLEME ---
        // EWMA volatilite güncelle (microprice kullan)
        self.update_volatility(microprice);
        
        // OFI güncelle (mid price momentum)
        self.update_ofi(microprice, now_ms);
        
        // Trend analizi
        let trend_bps = self.detect_trend();
        
        // Hedef envanter hesapla (funding rate ve trend'e göre)
        self.target_inventory = self.calculate_target_inventory(c.funding_rate, trend_bps);
        
        // Envanter kararı: hedef envantere göre al/sat
        let (mut should_bid, mut should_ask) = self.inventory_decision(c.inv, self.target_inventory);
        
        // --- MANİPÜLASYON FIRSAT KULLANIMI: Fırsatları avantaja çevir ---
        if let Some(ref opp) = self.manipulation_opportunity {
            match opp {
                ManipulationOpportunity::FlashCrashLong { price_drop_bps } => {
                    // Flash crash → LONG pozisyon (dip alım)
                    // Agresif bid, ask'i kaldır
                    should_bid = true;
                    should_ask = false;
                    use tracing::info;
                    info!(
                        price_drop_bps,
                        "EXECUTING FLASH CRASH STRATEGY: aggressive LONG (buying the dip)"
                    );
                }
                ManipulationOpportunity::FlashPumpShort { price_rise_bps } => {
                    // Flash pump → SHORT pozisyon (tepe satış)
                    // Agresif ask, bid'i kaldır
                    should_bid = false;
                    should_ask = true;
                    use tracing::info;
                    info!(
                        price_rise_bps,
                        "EXECUTING FLASH PUMP STRATEGY: aggressive SHORT (selling the top)"
                    );
                }
                ManipulationOpportunity::WideSpreadArbitrage { spread_bps } => {
                    // Geniş spread → Market maker olarak her iki tarafta da işlem yap
                    // Spread'den kazanç sağla
                    should_bid = true;
                    should_ask = true;
                    use tracing::info;
                    info!(
                        spread_bps,
                        "EXECUTING SPREAD ARBITRAGE: market making with wide spread"
                    );
                }
                ManipulationOpportunity::VolumeAnomalyTrend { direction, volume_ratio } => {
                    // Volume anomali + trend → Trend takibi
                    if *direction > 0.0 {
                        // Yukarı trend → LONG
                        should_bid = true;
                        should_ask = false;
                        use tracing::info;
                        info!(
                            volume_ratio,
                            direction,
                            "EXECUTING VOLUME TREND STRATEGY: going LONG (following uptrend)"
                        );
                    } else if *direction < 0.0 {
                        // Aşağı trend → SHORT
                        should_bid = false;
                        should_ask = true;
                        use tracing::info;
                        info!(
                            volume_ratio,
                            direction,
                            "EXECUTING VOLUME TREND STRATEGY: going SHORT (following downtrend)"
                        );
                    }
                }
            }
        }
        
        // --- ADVERSE SELECTION FİLTRESİ: OFI ve momentum yüksekse pasif tarafı geri çek ---
        let adverse_selection_threshold = 0.5; // OFI eşiği
        let ofi_abs = self.ofi_signal.abs();
        let momentum_strong = trend_bps.abs() > 50.0; // 50 bps trend
        
        // OFI pozitif (buy pressure) ve momentum yukarı → ask'i geri çek (bid riskli değil)
        // OFI negatif (sell pressure) ve momentum aşağı → bid'i geri çek (ask riskli değil)
        let mut adverse_bid = false;
        let mut adverse_ask = false;
        if ofi_abs > adverse_selection_threshold && momentum_strong {
            if self.ofi_signal > 0.0 && trend_bps > 0.0 {
                // Buy pressure + uptrend → ask riskli, bid güvenli
                adverse_ask = true;
            } else if self.ofi_signal < 0.0 && trend_bps < 0.0 {
                // Sell pressure + downtrend → bid riskli, ask güvenli
                adverse_bid = true;
            }
        }
        
        // Debug log (tracing kullanarak)
        use tracing::debug;
        debug!(
            current_inv = %c.inv.0,
            target_inv = %self.target_inventory.0,
            trend_bps,
            funding_rate = ?c.funding_rate,
            should_bid,
            should_ask,
            microprice = %microprice,
            imbalance,
            volatility = self.ewma_volatility,
            ofi_signal = self.ofi_signal,
            adverse_bid,
            adverse_ask,
            "strategy decision with microstructure signals"
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
        
        // Base spread hesaplama (eski yöntem)
        let base_spread_bps = (self.a * c.sigma + self.b * inv_bias).max(1e-4);
        let min_spread_bps = 1.0; // Minimum 1 bps spread
        
        // --- ADAPTİF SPREAD: Volatilite ve OFI'ye göre ---
        let mut spread_bps = self.calculate_adaptive_spread(base_spread_bps, min_spread_bps);
        
        // Imbalance'a göre spread ayarla: dengesizlik yüksekse spread'i genişlet
        let imbalance_adj = (imbalance.abs() * 10.0).min(5.0); // Max 5 bps artış
        spread_bps += imbalance_adj;
        
        // Likidasyon riski: yakınsa spread'i genişlet
        if c.liq_gap_bps < 300.0 {
            spread_bps *= 1.5;
        }
        
        // Funding rate skew: pozitif funding'de ask'i yukarı çek (long pozisyon için daha iyi)
        let funding_skew = c.funding_rate.unwrap_or(0.0) * 100.0;
        
        // Envanter yönüne göre asimetrik spread: envanter varsa o tarafı daha agresif yap
        let inv_skew_bps = inv_direction * inv_bias * 20.0; // Envanter bias'ına göre ekstra skew
        
        // Imbalance skew: pozitif imbalance (bid heavy) → bid'i yukarı, ask'i aşağı
        let imbalance_skew_bps = imbalance * 10.0; // Imbalance'a göre skew
        
        let half = Decimal::try_from(spread_bps / 2.0 / 1e4).unwrap_or(Decimal::ZERO);
        let skew = Decimal::try_from(funding_skew / 1e4).unwrap_or(Decimal::ZERO);
        let inv_skew = Decimal::try_from(inv_skew_bps / 1e4).unwrap_or(Decimal::ZERO);
        let imb_skew = Decimal::try_from(imbalance_skew_bps / 1e4).unwrap_or(Decimal::ZERO);
        
        // Fiyatlama: Microprice kullan (daha iyi tahmin)
        let pricing_base = microprice;
        
        // Bid: microprice'den aşağı (half + funding_skew + inv_skew + imbalance_skew)
        // Pozitif envanter varsa inv_skew pozitif, bid daha aşağı (satmaya zorla)
        // Negatif envanter varsa inv_skew negatif, bid daha yukarı (almaya zorla)
        // Pozitif imbalance (bid heavy) → imbalance_skew negatif, bid yukarı
        let bid_px = Px(pricing_base * (Decimal::ONE - half - skew - inv_skew - imb_skew));
        
        // Ask: microprice'den yukarı (half + funding_skew + inv_skew + imbalance_skew)
        // Pozitif envanter varsa inv_skew pozitif, ask daha yukarı (satmaya zorla)
        // Negatif envanter varsa inv_skew negatif, ask daha aşağı (almaya zorla)
        // Pozitif imbalance (bid heavy) → imbalance_skew negatif, ask aşağı
        let ask_px = Px(pricing_base * (Decimal::ONE + half + skew + inv_skew + imb_skew));
        
        let usd_size = self.base_notional.to_f64().unwrap_or(0.0);
        let qty = if usd_size > 0.0 {
            usd_size / mid_f
        } else {
            0.0
        };
        let qty = Qty(Decimal::from_f64_retain(qty).unwrap_or(Decimal::ZERO));
        
        // Akıllı karar: hedef envantere göre sadece gerekli tarafı koy
        // Adverse selection filtresi: riskli tarafı kaldır
        // MANİPÜLASYON FIRSAT KULLANIMI: Fırsat varsa agresif pozisyon al
        
        // Sadece çok geniş spread (>200 bps) varsa işlem yapma, diğer durumlarda fırsatları kullan
        let final_quotes = if spread_bps > self.max_spread_bps {
            Quotes::default() // Çok geniş spread, risk çok yüksek
        } else {
            // Manipülasyon fırsatı varsa agresif pozisyon, yoksa normal market making
            let (final_bid, final_ask) = if self.manipulation_opportunity.is_some() {
                // FIRSAT MODU: Agresif pozisyon al, adverse selection filtresini gevşet
                // Manipülasyon fırsatlarında daha agresif ol
                (should_bid, should_ask) // Adverse selection filtresini bypass et
            } else {
                // NORMAL MOD: Adverse selection filtresi aktif
                (should_bid && !adverse_bid, should_ask && !adverse_ask)
            };
            
            Quotes {
                bid: if final_bid { Some((bid_px, qty)) } else { None },
                ask: if final_ask { Some((ask_px, qty)) } else { None },
            }
        };
        
        // Debug log: Manipülasyon fırsat durumu
        debug!(
            flash_crash_detected = self.flash_crash_detected,
            spread_bps,
            max_spread_bps = self.max_spread_bps,
            min_liquidity,
            liquidity_ok = min_liquidity >= self.min_liquidity_required,
            opportunity = ?self.manipulation_opportunity,
            should_bid,
            should_ask,
            "anti-manipulation checks and opportunity analysis completed"
        );
        
        final_quotes
    }
}
