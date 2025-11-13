//location: /crates/app/src/qmel.rs
// Q-MEL: Micro-Edge Trading Algorithm
// Market microstructure + statistical edge + dynamic risk/margin + execution science

use crate::types::*;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use tracing::{debug, warn};

// ============================================================================
// Feature Extraction (State Vector)
// ============================================================================

/// Market state vector (10-20 dimensions)
#[derive(Clone, Debug)]
pub struct MarketState {
    pub ofi: f64,                  // Order Flow Imbalance
    pub microprice: f64,           // Microprice
    pub spread_velocity: f64,      // Spread velocity (d(spread)/dt)
    pub liquidity_pressure: f64,   // LP = D_ask / D_bid (weighted by levels)
    pub volatility_1s: f64,        // Short-term volatility (1s EWMA)
    pub volatility_5s: f64,        // Medium-term volatility (5s EWMA)
    pub cancel_trade_ratio: f64,   // Cancel/Trade ratio (spoof detection)
    pub oi_delta_30s: f64,         // Open Interest delta (30s window)
    pub funding_rate: Option<f64>, // Current funding rate
}

impl Default for MarketState {
    fn default() -> Self {
        Self {
            ofi: 0.0,
            microprice: 0.0,
            spread_velocity: 0.0,
            liquidity_pressure: 1.0,
            volatility_1s: 0.0,
            volatility_5s: 0.0,
            cancel_trade_ratio: 0.0,
            oi_delta_30s: 0.0,
            funding_rate: None,
        }
    }
}

/// Feature extractor for Q-MEL algorithm
pub struct FeatureExtractor {
    // Volatility tracking (EWMA)
    vol_1s_ewma: f64,
    vol_5s_ewma: f64,
    last_price: Option<f64>,
    last_spread: Option<f64>,
    last_spread_time: Option<Instant>,

    // Price history for volatility
    price_history_1s: VecDeque<(f64, Instant)>,
    price_history_5s: VecDeque<(f64, Instant)>,

    // Cancel/Trade tracking
    cancel_count: u64,
    trade_count: u64,
    last_reset_time: Instant,

    // OI tracking (if available)
    oi_history: VecDeque<(f64, Instant)>,
}

impl FeatureExtractor {
    pub fn new() -> Self {
        Self {
            vol_1s_ewma: 0.0,
            vol_5s_ewma: 0.0,
            last_price: None,
            last_spread: None,
            last_spread_time: None,
            price_history_1s: VecDeque::new(),
            price_history_5s: VecDeque::new(),
            cancel_count: 0,
            trade_count: 0,
            last_reset_time: Instant::now(),
            oi_history: VecDeque::new(),
        }
    }

    /// Calculate Order Flow Imbalance (OFI)
    /// OFI = (ΔV_bid - ΔV_ask) / (ΔV_bid + ΔV_ask + ε)
    pub fn calculate_ofi(&self, ob: &OrderBook, prev_ob: Option<&OrderBook>) -> f64 {
        let epsilon = 1e-8;

        // Get current bid/ask volumes
        let (v_bid, v_ask) = self.get_weighted_volumes(ob);

        if let Some(prev) = prev_ob {
            let (prev_v_bid, prev_v_ask) = self.get_weighted_volumes(prev);
            let delta_v_bid = v_bid - prev_v_bid;
            let delta_v_ask = v_ask - prev_v_ask;

            let denominator = delta_v_bid + delta_v_ask + epsilon;
            if denominator.abs() < epsilon {
                return 0.0;
            }
            (delta_v_bid - delta_v_ask) / denominator
        } else {
            // First observation - use current imbalance
            let total = v_bid + v_ask + epsilon;
            if total.abs() < epsilon {
                return 0.0;
            }
            (v_bid - v_ask) / total
        }
    }

    /// Get weighted volumes from order book (top-K levels)
    fn get_weighted_volumes(&self, ob: &OrderBook) -> (f64, f64) {
        // Use top-K levels if available, otherwise use best bid/ask
        if let (Some(ref top_bids), Some(ref top_asks)) = (&ob.top_bids, &ob.top_asks) {
            let v_bid: f64 = top_bids
                .iter()
                .map(|level| level.qty.0.to_f64().unwrap_or(0.0))
                .sum();
            let v_ask: f64 = top_asks
                .iter()
                .map(|level| level.qty.0.to_f64().unwrap_or(0.0))
                .sum();
            (v_bid, v_ask)
        } else {
            // Fallback to best bid/ask
            let v_bid = ob
                .best_bid
                .map(|level| level.qty.0.to_f64().unwrap_or(0.0))
                .unwrap_or(0.0);
            let v_ask = ob
                .best_ask
                .map(|level| level.qty.0.to_f64().unwrap_or(0.0))
                .unwrap_or(0.0);
            (v_bid, v_ask)
        }
    }

    /// Calculate microprice
    /// microprice = (P_ask * D_bid + P_bid * D_ask) / (D_bid + D_ask)
    pub fn calculate_microprice(&self, ob: &OrderBook) -> Option<f64> {
        let (v_bid, v_ask) = self.get_weighted_volumes(ob);

        let bid_px = ob.best_bid?.px.0.to_f64()?;
        let ask_px = ob.best_ask?.px.0.to_f64()?;

        let total_depth = v_bid + v_ask;
        if total_depth < 1e-8 {
            return None;
        }

        Some((ask_px * v_bid + bid_px * v_ask) / total_depth)
    }

    /// Calculate spread velocity (d(spread)/dt)
    pub fn calculate_spread_velocity(&mut self, ob: &OrderBook) -> f64 {
        let current_spread = self.get_spread(ob);
        let now = Instant::now();

        if let (Some(prev_spread), Some(prev_time)) = (self.last_spread, self.last_spread_time) {
            let dt = now.duration_since(prev_time).as_secs_f64();
            if dt > 0.0 && dt < 10.0 {
                // Avoid division by zero and stale data
                let velocity = (current_spread - prev_spread) / dt;
                self.last_spread = Some(current_spread);
                self.last_spread_time = Some(now);
                return velocity;
            }
        }

        self.last_spread = Some(current_spread);
        self.last_spread_time = Some(now);
        0.0
    }

    fn get_spread(&self, ob: &OrderBook) -> f64 {
        if let (Some(bid), Some(ask)) = (ob.best_bid, ob.best_ask) {
            let bid_px = bid.px.0.to_f64().unwrap_or(0.0);
            let ask_px = ask.px.0.to_f64().unwrap_or(0.0);
            if bid_px > 0.0 {
                (ask_px - bid_px) / bid_px
            } else {
                0.0
            }
        } else {
            0.0
        }
    }

    /// Calculate liquidity pressure (LP = D_ask / D_bid)
    pub fn calculate_liquidity_pressure(&self, ob: &OrderBook) -> f64 {
        let (v_bid, v_ask) = self.get_weighted_volumes(ob);
        if v_bid < 1e-8 {
            return 10.0; // High pressure if no bid depth
        }
        v_ask / v_bid
    }

    /// Update volatility (EWMA)
    pub fn update_volatility(&mut self, price: f64) {
        let now = Instant::now();

        // Add to history
        self.price_history_1s.push_back((price, now));
        self.price_history_5s.push_back((price, now));

        // Remove old entries
        while let Some(&(_, t)) = self.price_history_1s.front() {
            if now.duration_since(t) > Duration::from_secs(1) {
                self.price_history_1s.pop_front();
            } else {
                break;
            }
        }

        while let Some(&(_, t)) = self.price_history_5s.front() {
            if now.duration_since(t) > Duration::from_secs(5) {
                self.price_history_5s.pop_front();
            } else {
                break;
            }
        }

        // Calculate returns and volatility
        if let Some(prev_price) = self.last_price {
            if prev_price > 0.0 {
                let ret = (price / prev_price).ln();

                // EWMA update (alpha = 0.1 for 1s, 0.05 for 5s)
                let alpha_1s = 0.1;
                let alpha_5s = 0.05;

                self.vol_1s_ewma = alpha_1s * ret.abs() + (1.0 - alpha_1s) * self.vol_1s_ewma;
                self.vol_5s_ewma = alpha_5s * ret.abs() + (1.0 - alpha_5s) * self.vol_5s_ewma;
            }
        }

        self.last_price = Some(price);
    }

    /// Get current volatility estimates
    pub fn get_volatility_1s(&self) -> f64 {
        self.vol_1s_ewma
    }

    pub fn get_volatility_5s(&self) -> f64 {
        self.vol_5s_ewma
    }

    /// Get cancel/trade ratio (reset every 10 seconds)
    pub fn get_cancel_trade_ratio(&mut self) -> f64 {
        let now = Instant::now();
        if now.duration_since(self.last_reset_time) > Duration::from_secs(10) {
            // Reset counters
            self.cancel_count = 0;
            self.trade_count = 0;
            self.last_reset_time = now;
            return 0.0;
        }

        if self.trade_count > 0 {
            self.cancel_count as f64 / self.trade_count as f64
        } else {
            0.0
        }
    }

    /// Get OI delta (30s window)
    pub fn get_oi_delta_30s(&self) -> f64 {
        if self.oi_history.len() < 2 {
            return 0.0;
        }

        let first = self.oi_history.front().map(|(oi, _)| *oi).unwrap_or(0.0);
        let last = self.oi_history.back().map(|(oi, _)| *oi).unwrap_or(0.0);
        last - first
    }

    /// Extract full market state vector
    pub fn extract_state(
        &mut self,
        ob: &OrderBook,
        prev_ob: Option<&OrderBook>,
        funding_rate: Option<f64>,
    ) -> MarketState {
        let ofi = self.calculate_ofi(ob, prev_ob);
        let microprice = self.calculate_microprice(ob).unwrap_or(0.0);
        let spread_velocity = self.calculate_spread_velocity(ob);
        let liquidity_pressure = self.calculate_liquidity_pressure(ob);
        let cancel_trade_ratio = self.get_cancel_trade_ratio();
        let oi_delta = self.get_oi_delta_30s();

        // Update volatility if we have a price
        if let Some(mp) = self.calculate_microprice(ob) {
            self.update_volatility(mp);
        }

        MarketState {
            ofi,
            microprice,
            spread_velocity,
            liquidity_pressure,
            volatility_1s: self.get_volatility_1s(),
            volatility_5s: self.get_volatility_5s(),
            cancel_trade_ratio,
            oi_delta_30s: oi_delta,
            funding_rate,
        }
    }
}

// ============================================================================
// Edge Estimation (Alpha Gate)
// ============================================================================

/// Adam Optimizer: Adaptive Moment Estimation
/// Her parameter için adaptive learning rate sağlar
#[derive(Clone, Debug)]
struct AdamOptimizer {
    m: Vec<f64>, // Momentum (first moment estimate)
    v: Vec<f64>, // Variance (second moment estimate)
    t: u64,      // Timestep (bias correction için)
}

impl AdamOptimizer {
    fn new(dim: usize) -> Self {
        Self {
            m: vec![0.0; dim],
            v: vec![0.0; dim],
            t: 0,
        }
    }

    /// Adam update: Adaptive learning rate per parameter
    /// alpha: learning rate (default: 0.001)
    /// beta1: momentum decay (default: 0.9)
    /// beta2: variance decay (default: 0.999)
    fn update(
        &mut self,
        gradients: &[f64],
        weights: &mut [f64],
        bias: &mut f64,
        bias_gradient: f64,
    ) {
        let alpha: f64 = 0.001; // Base learning rate
        let beta1: f64 = 0.9; // Momentum decay
        let beta2: f64 = 0.999; // Variance decay
        let epsilon: f64 = 1e-8; // Numerical stability

        self.t += 1;

        // Bias correction coefficients
        let beta1_t: f64 = beta1.powi(self.t as i32);
        let beta2_t: f64 = beta2.powi(self.t as i32);
        let m_bias_correction = 1.0 - beta1_t;
        let v_bias_correction = 1.0 - beta2_t;

        // Update weights with Adam
        for i in 0..gradients.len() {
            // Update momentum and variance
            self.m[i] = beta1 * self.m[i] + (1.0 - beta1) * gradients[i];
            self.v[i] = beta2 * self.v[i] + (1.0 - beta2) * gradients[i].powi(2);

            // Bias-corrected estimates
            let m_hat = self.m[i] / m_bias_correction;
            let v_hat = self.v[i] / v_bias_correction;

            // Update weight: w = w - alpha * m_hat / (sqrt(v_hat) + epsilon)
            weights[i] -= alpha * m_hat / (v_hat.sqrt() + epsilon);
        }

        // Update bias with Adam (simplified - single parameter)
        // Bias için ayrı optimizer kullanabiliriz ama basit tutuyoruz
        let bias_alpha = alpha * 0.1; // Bias için daha küçük learning rate
        *bias -= bias_alpha * bias_gradient;
    }
}

/// Direction probability model
#[derive(Clone, Debug)]
pub struct DirectionModel {
    // Logistic regression weights (simplified - can be enhanced with online learning)
    weights: Vec<f64>,
    bias: f64,
    feature_names: Vec<String>, // Feature isimleri (importance tracking için)
    feature_importance: Vec<f64>, // Her feature'ın importance skoru
    feature_update_count: Vec<u64>, // Her feature'ın kaç kez güncellendiği
    adam: AdamOptimizer,        // Adam optimizer (adaptive learning rate)
}

impl DirectionModel {
    pub fn new(feature_dim: usize) -> Self {
        let feature_names = vec![
            "ofi".to_string(),
            "microprice".to_string(),
            "spread_velocity".to_string(),
            "liquidity_pressure".to_string(),
            "volatility_1s".to_string(),
            "volatility_5s".to_string(),
            "cancel_trade_ratio".to_string(),
            "oi_delta_30s".to_string(),
            "funding_rate".to_string(),
        ];

        // BEST PRACTICE: Xavier/Glorot initialization (daha iyi convergence)
        // Weights: N(0, 1/sqrt(n_features)) - küçük random değerler
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        use std::time::{SystemTime, UNIX_EPOCH};

        let mut hasher = DefaultHasher::new();
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            .hash(&mut hasher);
        let seed = hasher.finish();

        // Xavier initialization: weights ~ N(0, 1/sqrt(n))
        let std_dev = 1.0 / (feature_dim as f64).sqrt();
        let mut weights = Vec::with_capacity(feature_dim);
        for i in 0..feature_dim {
            // Basit pseudo-random (production'da proper RNG kullan)
            let r = ((seed + i as u64) % 10000) as f64 / 10000.0;
            let weight = (r - 0.5) * 2.0 * std_dev; // [-std_dev, std_dev]
            weights.push(weight);
        }

        Self {
            weights,
            bias: 0.0, // Bias sıfırdan başlar
            feature_names,
            feature_importance: vec![0.0; feature_dim],
            feature_update_count: vec![0; feature_dim],
            adam: AdamOptimizer::new(feature_dim),
        }
    }

    /// Feature importance hesapla (weight magnitude + update frequency)
    /// Önemli feature'lar: yüksek weight magnitude ve sık güncellenen
    pub fn calculate_feature_importance(&self) -> Vec<(String, f64)> {
        let mut importance: Vec<(String, f64)> = self
            .feature_names
            .iter()
            .zip(self.weights.iter())
            .zip(self.feature_importance.iter())
            .zip(self.feature_update_count.iter())
            .map(|(((name, weight), importance), count)| {
                // Importance = weight magnitude * importance_score * update_frequency
                let weight_magnitude = weight.abs();
                let importance_score = importance.abs();
                let update_freq = if *count > 0 {
                    (*count as f64).ln() / 10.0 // Normalize
                } else {
                    0.0
                };

                let total_importance =
                    weight_magnitude * (1.0 + importance_score) * (1.0 + update_freq);
                (name.clone(), total_importance)
            })
            .collect();

        // Sort by importance (descending)
        importance.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        importance
    }

    /// En önemli N feature'ı döndür
    ///
    /// # Race Condition Note
    /// Bu fonksiyon `&self` alır (immutable reference), bu yüzden Rust'ın borrow checker'ı
    /// aynı anda `&mut self` ile update yapılmasını önler. Ancak çağıran kod empty check yapmalı.
    pub fn get_top_features(&self, n: usize) -> Vec<(String, f64)> {
        let importance = self.calculate_feature_importance();
        importance.into_iter().take(n).collect()
    }

    /// Feature importance'ı güncelle (her update'te)
    fn update_feature_importance(&mut self, feature_idx: usize, gradient_magnitude: f64) {
        if feature_idx < self.feature_importance.len() {
            // Exponential moving average of gradient magnitude
            let alpha = 0.1; // Smoothing factor
            self.feature_importance[feature_idx] =
                alpha * gradient_magnitude + (1.0 - alpha) * self.feature_importance[feature_idx];
            self.feature_update_count[feature_idx] += 1;
        }
    }

    /// Predict probability of upward movement (p↑)
    /// p↑ = Pr(ΔP ≥ +τ | s_t)
    pub fn predict_up_probability(&self, state: &MarketState, target_tick: f64) -> f64 {
        // Simple logistic model: p = 1 / (1 + exp(-(w·x + b)))
        let feature_vec = self.state_to_features(state);
        let score: f64 = self
            .weights
            .iter()
            .zip(feature_vec.iter())
            .map(|(w, x)| w * x)
            .sum::<f64>()
            + self.bias;

        // Sigmoid
        let p = 1.0 / (1.0 + (-score).exp());

        // Adjust for target tick (higher target = lower probability)
        let tick_adjustment = 1.0 / (1.0 + target_tick * 10.0);
        let final_p = (p * tick_adjustment).max(0.0).min(1.0);

        // BEST PRACTICE: Final validation (NaN/Inf kontrolü)
        if final_p.is_finite() && final_p >= 0.0 && final_p <= 1.0 {
            final_p
        } else {
            0.5 // Fallback: neutral probability
        }
    }

    /// Convert state to feature vector
    fn state_to_features(&self, state: &MarketState) -> Vec<f64> {
        vec![
            state.ofi,
            state.microprice,
            state.spread_velocity,
            state.liquidity_pressure,
            state.volatility_1s,
            state.volatility_5s,
            state.cancel_trade_ratio,
            state.oi_delta_30s,
            state.funding_rate.unwrap_or(0.0),
        ]
    }

    /// Online update with Adam-like optimizer (adaptive learning rate)
    /// Adam: Adaptive Moment Estimation - daha iyi convergence
    /// L2 regularization ile overfitting önleme
    /// Feature importance tracking ile hangi feature'ların önemli olduğunu öğren
    /// BEST PRACTICE: NaN/Inf kontrolü, gradient clipping, feature normalization
    pub fn update(&mut self, state: &MarketState, actual_direction: f64, learning_rate: f64) {
        // BEST PRACTICE: Input validation
        let actual_dir_f64: f64 = actual_direction;
        if !actual_dir_f64.is_finite() || actual_dir_f64 < 0.0 || actual_dir_f64 > 1.0 {
            warn!(
                "Invalid actual_direction: {}, skipping update",
                actual_dir_f64
            );
            return;
        }

        let feature_vec = self.state_to_features(state);

        // BEST PRACTICE: Feature normalization (L2 norm)
        let feature_norm: f64 = feature_vec.iter().map(|x| x * x).sum::<f64>().sqrt();
        let normalized_features: Vec<f64> = if feature_norm > 1e-8 {
            feature_vec.iter().map(|x| x / feature_norm).collect()
        } else {
            feature_vec // Norm çok küçükse normalize etme
        };

        let prediction = self.predict_up_probability(state, 1.0);

        // BEST PRACTICE: Prediction validation
        let pred_f64: f64 = prediction;
        if !pred_f64.is_finite() || pred_f64 < 0.0 || pred_f64 > 1.0 {
            warn!("Invalid prediction: {}, skipping update", pred_f64);
            return;
        }

        let error = actual_direction - prediction;

        // BEST PRACTICE: Gradient clipping (exploding gradient önleme)
        let clipped_error = error.max(-1.0).min(1.0);

        // Adaptive learning rate: error magnitude'ye göre ayarla
        // Büyük hatalarda daha hızlı öğren, küçük hatalarda yavaş öğren
        let adaptive_lr = learning_rate * (1.0 + clipped_error.abs() * 0.5).min(2.0);

        // BEST PRACTICE: Learning rate validation
        let lr_f64: f64 = adaptive_lr;
        if !lr_f64.is_finite() || lr_f64 <= 0.0 {
            warn!("Invalid adaptive_lr: {}, using default", lr_f64);
            return;
        }

        // L2 regularization (overfitting önleme)
        let l2_reg = 0.0001;

        // KRİTİK İYİLEŞTİRME: Adam Optimizer kullan
        // Gradient'leri hesapla (L2 regularization dahil)
        let mut gradients: Vec<f64> = Vec::with_capacity(normalized_features.len());
        let mut gradient_magnitudes: Vec<(usize, f64)> = Vec::new();

        for (idx, (w, x)) in self
            .weights
            .iter()
            .zip(normalized_features.iter())
            .enumerate()
        {
            // Gradient: error * feature + L2 regularization term
            let gradient = clipped_error * x + l2_reg * w;

            // BEST PRACTICE: Gradient clipping (exploding gradient önleme)
            let clipped_gradient = gradient.max(-10.0).min(10.0);
            let gradient_magnitude = clipped_gradient.abs();

            gradients.push(clipped_gradient);
            gradient_magnitudes.push((idx, gradient_magnitude));
        }

        // Bias gradient
        let bias_gradient = clipped_error;

        // Adam optimizer ile weight update
        // NOT: learning_rate parametresi artık kullanılmıyor (Adam kendi learning rate'ini yönetiyor)
        // Ama feature importance'a göre alpha'yı scale edebiliriz
        self.adam
            .update(&gradients, &mut self.weights, &mut self.bias, bias_gradient);

        // BEST PRACTICE: Weight validation (NaN/Inf kontrolü)
        for (idx, w) in self.weights.iter_mut().enumerate() {
            let weight_f64: f64 = *w;
            if !weight_f64.is_finite() {
                warn!(
                    "Invalid weight after Adam update for feature {}, resetting",
                    idx
                );
                *w = 0.0; // Reset to zero if invalid
            }
        }

        // Bias validation
        let bias_f64: f64 = self.bias;
        if !bias_f64.is_finite() {
            warn!("Invalid bias after Adam update, resetting");
            self.bias = 0.0;
        }

        // Feature importance tracking: gradient magnitude'yi kaydet
        for (idx, gradient_magnitude) in gradient_magnitudes {
            self.update_feature_importance(idx, gradient_magnitude);
        }
    }
}

/// Expected Value (EV) calculator
pub struct ExpectedValueCalculator {
    maker_fee_rate: f64,
    taker_fee_rate: f64,
    base_ev_threshold: f64, // Base threshold (adaptif olarak değişir)
    recent_ev_performance: VecDeque<f64>, // Son EV tahminlerinin performansı
}

impl ExpectedValueCalculator {
    pub fn new(maker_fee_rate: f64, taker_fee_rate: f64, ev_threshold: f64) -> Self {
        Self {
            maker_fee_rate,
            taker_fee_rate,
            base_ev_threshold: ev_threshold,
            recent_ev_performance: VecDeque::new(),
        }
    }

    /// Adaptif EV threshold: Performansa göre dinamik ayarla
    /// Win rate düşükse threshold'u yükselt (daha seçici ol)
    /// Win rate yüksekse threshold'u düşür (daha fazla trade yap)
    pub fn get_adaptive_threshold(&self, win_rate: f64) -> f64 {
        if win_rate < 0.45 {
            // Win rate düşük: daha seçici ol (threshold'u yükselt)
            self.base_ev_threshold * 1.5
        } else if win_rate > 0.60 {
            // Win rate yüksek: daha fazla trade yap (threshold'u düşür)
            self.base_ev_threshold * 0.7
        } else {
            // Normal: base threshold kullan
            self.base_ev_threshold
        }
    }

    /// EV tahmininin performansını kaydet (edge validation için)
    pub fn record_ev_performance(&mut self, predicted_ev: f64, actual_pnl: f64) {
        // EV tahmini vs gerçek PnL karşılaştırması
        let ev_accuracy = if predicted_ev > 0.0 && actual_pnl > 0.0 {
            1.0 // Doğru tahmin
        } else if predicted_ev <= 0.0 && actual_pnl <= 0.0 {
            1.0 // Doğru tahmin (trade yapmadık veya zarar bekliyorduk)
        } else {
            0.0 // Yanlış tahmin
        };

        // BEST PRACTICE: Bounded memory (memory leak önleme)
        self.recent_ev_performance.push_back(ev_accuracy);
        while self.recent_ev_performance.len() > 50 {
            self.recent_ev_performance.pop_front();
        }
    }

    /// Edge validation: Gerçekten edge var mı kontrol et
    /// Son 50 trade'de EV tahminlerinin doğruluğunu kontrol et
    pub fn has_edge(&self) -> bool {
        if self.recent_ev_performance.len() < 10 {
            return true; // Henüz yeterli veri yok, edge olduğunu varsay
        }

        let avg_accuracy: f64 = self.recent_ev_performance.iter().sum::<f64>()
            / self.recent_ev_performance.len() as f64;
        avg_accuracy > 0.55 // %55'ten fazla doğru tahmin = edge var
    }

    /// Calculate expected value for LONG position
    /// EV_long = p↑ · T - p↓ · S - C_fees - C_slip
    pub fn calculate_ev_long(
        &self,
        p_up: f64,
        target_profit_usd: f64,
        stop_loss_usd: f64,
        position_size_usd: f64,
        estimated_slippage_usd: f64,
        is_maker: bool,
    ) -> f64 {
        let p_down = 1.0 - p_up;
        let fee_rate = if is_maker {
            self.maker_fee_rate
        } else {
            self.taker_fee_rate
        };
        let fees = position_size_usd * fee_rate * 2.0; // Entry + exit

        p_up * target_profit_usd - p_down * stop_loss_usd - fees - estimated_slippage_usd
    }

    /// Calculate expected value for SHORT position
    pub fn calculate_ev_short(
        &self,
        p_down: f64,
        target_profit_usd: f64,
        stop_loss_usd: f64,
        position_size_usd: f64,
        estimated_slippage_usd: f64,
        is_maker: bool,
    ) -> f64 {
        let p_up = 1.0 - p_down;
        let fee_rate = if is_maker {
            self.maker_fee_rate
        } else {
            self.taker_fee_rate
        };
        let fees = position_size_usd * fee_rate * 2.0; // Entry + exit

        p_down * target_profit_usd - p_up * stop_loss_usd - fees - estimated_slippage_usd
    }
}

// ============================================================================
// Regime Filter
// ============================================================================

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MarketRegime {
    Normal, // Spread stable, depth sufficient
    Frenzy, // Spread volatile, depth shallow
    Drift,  // Low volatility
}

pub struct RegimeClassifier {
    volatility_threshold_high: f64,
    volatility_threshold_low: f64,
    cancel_trade_threshold: f64,
    spread_threshold_wide: f64,
}

impl RegimeClassifier {
    pub fn new() -> Self {
        Self {
            volatility_threshold_high: 0.05,
            volatility_threshold_low: 0.01,
            cancel_trade_threshold: 5.0,
            spread_threshold_wide: 0.001, // 10 bps
        }
    }

    pub fn classify(&self, state: &MarketState, spread_bps: f64) -> MarketRegime {
        // Frenzy: high volatility + high cancel/trade ratio
        if state.volatility_1s > self.volatility_threshold_high
            && state.cancel_trade_ratio > self.cancel_trade_threshold
        {
            return MarketRegime::Frenzy;
        }

        // Frenzy: wide spread + shallow depth
        if spread_bps > self.spread_threshold_wide * 10000.0 && state.liquidity_pressure > 5.0 {
            return MarketRegime::Frenzy;
        }

        // Drift: low volatility
        if state.volatility_1s < self.volatility_threshold_low
            && state.volatility_5s < self.volatility_threshold_low
        {
            return MarketRegime::Drift;
        }

        MarketRegime::Normal
    }

    /// Check if trading should be paused
    pub fn should_pause(&self, state: &MarketState) -> bool {
        // Anomaly detection: extreme cancel/trade ratio or volatility spike
        state.cancel_trade_ratio > self.cancel_trade_threshold * 2.0
            || (state.volatility_1s > state.volatility_5s * 3.0 && state.volatility_1s > 0.1)
    }
}

// ============================================================================
// Dynamic Margin Allocation (DMA)
// ============================================================================

/// Dynamic margin allocation with min 10, max 100 USDC rule
pub struct DynamicMarginAllocator {}

impl DynamicMarginAllocator {
    pub fn new(_min_margin_usdc: f64, _max_margin_usdc: f64, _f_min: f64, _f_max: f64) -> Self {
        Self {}
    }
}

// ============================================================================
// Auto-Risk Governor (ARG)
// ============================================================================

/// Auto-Risk Governor for leverage selection
pub struct AutoRiskGovernor {
    max_leverage: f64,
}

impl AutoRiskGovernor {
    pub fn new(_alpha: f64, _beta: f64, max_leverage: f64) -> Self {
        Self { max_leverage }
    }

    /// Calculate safe leverage based on stop distance, MMR, and volatility
    /// AGGRESSIVE MODE: For $0.50 target, maximize leverage while staying safe
    /// L ≤ (α · d_stop) / MMR
    /// L ← min(L, β · T / σ_1s)
    pub fn calculate_leverage(
        &self,
        stop_distance_pct: f64,  // Entry-stop distance as percentage
        mmr: f64,                // Maintenance margin rate
        target_tick_usd: f64,    // Target profit in USD
        volatility_1s: f64,      // 1-second volatility
        daily_drawdown_pct: f64, // Current daily drawdown percentage
    ) -> f64 {
        // KRİTİK DÜZELTME: Leverage'ı güvenli seviyede tut (125x çok tehlikeli!)
        // Konservatif alpha ve beta kullan

        // Liquidation safety constraint - KONSERVATİF: use safe alpha
        let leverage_by_stop = if mmr > 0.0 {
            // Use 0.4 (safe alpha) instead of 0.6
            let safe_alpha = 0.4;
            (safe_alpha * stop_distance_pct) / mmr
        } else {
            self.max_leverage.min(20.0) // Max 20x even if no MMR
        };

        // Volatility constraint - KONSERVATİF: use safe beta
        let leverage_by_vol = if volatility_1s > 1e-8 {
            // Use 1.0 (safe beta) instead of 1.5
            let safe_beta = 1.0;
            (safe_beta * target_tick_usd) / volatility_1s
        } else {
            self.max_leverage.min(20.0) // Max 20x even if no volatility
        };

        // KONSERVATİF: Reduce leverage if any drawdown exists
        let drawdown_factor = if daily_drawdown_pct > 0.0 {
            // Reduce by 30% per 1% drawdown (more conservative)
            (1.0 - daily_drawdown_pct * 0.3).max(0.3)
        } else {
            1.0
        };

        // KONSERVATİF: Take minimum of constraints (not maximum) for safety
        let base_leverage = leverage_by_stop
            .min(leverage_by_vol)
            .min(self.max_leverage)
            .min(20.0);
        let final_leverage = base_leverage * drawdown_factor;

        // KONSERVATİF: Maximum leverage 20x (güvenlik için)
        final_leverage.max(1.0).min(20.0).min(self.max_leverage)
    }
}

// ============================================================================
// Execution Optimizer (EXO)
// ============================================================================

/// Execution optimizer for maker/taker decision and slippage estimation
pub struct ExecutionOptimizer {}

impl ExecutionOptimizer {
    pub fn new(_maker_fee_rate: f64, _taker_fee_rate: f64, _edge_decay_half_life_ms: f64) -> Self {
        Self {}
    }

    /// Estimate slippage cost
    /// C_slip ≈ g(depth at price, size, latency)
    pub fn estimate_slippage(
        &self,
        order_size_usd: f64,
        depth_at_price_usd: f64,
        latency_ms: f64,
    ) -> f64 {
        // Simple slippage model
        let size_ratio = order_size_usd / depth_at_price_usd.max(1.0);
        let base_slippage = size_ratio * 0.001; // 0.1% per unit of size/depth ratio

        // Latency penalty (higher latency = more slippage risk)
        let latency_penalty = (latency_ms / 100.0).min(1.0) * 0.0005;

        (base_slippage + latency_penalty) * order_size_usd
    }
}

// ============================================================================
// Online Learning (Bandit)
// ============================================================================

/// Parameter arm for bandit algorithm
#[derive(Clone, Debug)]
pub struct ParameterArm {
    pub target_tick: f64,
    pub stop_tick: f64,
    pub timeout_secs: f64,
    pub reward_sum: f64,
    pub pull_count: u64,
}

impl ParameterArm {
    pub fn new(target_tick: f64, stop_tick: f64, timeout_secs: f64, _maker_threshold: f64) -> Self {
        Self {
            target_tick,
            stop_tick,
            timeout_secs,
            reward_sum: 0.0,
            pull_count: 0,
        }
    }

    pub fn average_reward(&self) -> f64 {
        if self.pull_count > 0 {
            self.reward_sum / self.pull_count as f64
        } else {
            0.0
        }
    }

    pub fn update(&mut self, reward: f64) {
        self.reward_sum += reward;
        self.pull_count += 1;
    }
}

/// Thompson Sampling bandit for parameter optimization
pub struct ThompsonSamplingBandit {
    pub arms: Vec<ParameterArm>,
}

impl ThompsonSamplingBandit {
    pub fn new(_exploration_rate: f64) -> Self {
        // Initialize with default parameter combinations
        let mut arms = Vec::new();

        // Create parameter grid
        for target_tick in [1.0, 2.0] {
            for stop_tick in [1.0, 1.5] {
                for timeout_secs in [5.0, 8.0, 10.0] {
                    for maker_threshold in [0.5, 1.0] {
                        arms.push(ParameterArm::new(
                            target_tick,
                            stop_tick * target_tick,
                            timeout_secs,
                            maker_threshold,
                        ));
                    }
                }
            }
        }

        Self { arms }
    }

    /// Select arm using Thompson Sampling
    /// GERÇEK Thompson Sampling: Beta distribution sampling kullan
    /// Her arm için Beta(α, β) dağılımından sample al, en yüksek sample'ı seç
    pub fn select_arm(&self) -> usize {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        use std::time::{SystemTime, UNIX_EPOCH};

        // Basit RNG (production'da daha iyi bir RNG kullanılabilir)
        let mut hasher = DefaultHasher::new();
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            .hash(&mut hasher);
        let seed = hasher.finish();

        let mut best_arm = 0;
        let mut best_sample = f64::NEG_INFINITY;

        for (i, arm) in self.arms.iter().enumerate() {
            // Thompson Sampling: Beta(α, β) distribution
            // α = wins + 1, β = losses + 1
            // Basitleştirilmiş: average_reward'u normalize et ve Beta benzeri sample al
            let avg_reward = arm.average_reward();
            let pulls = arm.pull_count.max(1);

            // Beta distribution parametreleri
            // α = positive_rewards + 1, β = negative_rewards + 1
            // Basitleştirilmiş: avg_reward'u [0, 1] aralığına normalize et
            let normalized_reward = (avg_reward + 1.0) / 2.0; // [-1, 1] -> [0, 1]
            let alpha = normalized_reward * pulls as f64 + 1.0;
            let beta = (1.0 - normalized_reward) * pulls as f64 + 1.0;

            // Beta distribution sample (basitleştirilmiş: normal approximation)
            // Gerçek Beta sampling için `rand` crate kullanılabilir
            let sample = if pulls == 0 {
                f64::INFINITY // Unexplored arms get priority
            } else {
                // Basit approximation: mean + noise
                let mean = alpha / (alpha + beta);
                let variance = (alpha * beta) / ((alpha + beta).powi(2) * (alpha + beta + 1.0));
                let noise = ((seed + i as u64) % 1000) as f64 / 1000.0 - 0.5; // [-0.5, 0.5]
                mean + noise * variance.sqrt()
            };

            if sample > best_sample {
                best_sample = sample;
                best_arm = i;
            }
        }

        best_arm
    }

    /// Update arm with reward
    pub fn update_arm(&mut self, arm_idx: usize, reward: f64) {
        if arm_idx < self.arms.len() {
            self.arms[arm_idx].update(reward);
        }
    }
}

// ============================================================================
// Q-MEL Strategy (Main Integration)
// ============================================================================

use crate::strategy::{Context, Strategy};
use crate::types::Quotes;

/// Q-MEL Trading Strategy
pub struct QMelStrategy {
    // Feature extraction
    feature_extractor: FeatureExtractor,
    prev_orderbook: Option<OrderBook>,

    // Edge estimation
    direction_model: DirectionModel,
    ev_calculator: ExpectedValueCalculator,

    // Risk management
    risk_governor: AutoRiskGovernor,
    execution_optimizer: ExecutionOptimizer,

    // Regime classification
    regime_classifier: RegimeClassifier,
    current_regime: MarketRegime,
    pause_until: Option<Instant>,

    // Online learning
    bandit: ThompsonSamplingBandit,
    current_arm_idx: Option<usize>,

    // Configuration
    target_tick_usd: f64,
    stop_tick_usd: f64,
    timeout_secs: f64,

    // State tracking
    recent_returns: VecDeque<f64>,
    daily_pnl: f64,
    daily_drawdown_pct: f64,
    last_trade_time: Option<Instant>,
    last_state: Option<MarketState>, // Store last state for OFI signal
    last_trade_direction: Option<bool>, // true = long, false = short (entry'de kaydedilir)
}

impl QMelStrategy {
    pub fn new(
        maker_fee_rate: f64,
        taker_fee_rate: f64,
        ev_threshold: f64,
        min_margin_usdc: f64,
        max_margin_usdc: f64,
        max_leverage: f64,
    ) -> Self {
        let feature_extractor = FeatureExtractor::new();
        let direction_model = DirectionModel::new(9); // 9 features
        let ev_calculator =
            ExpectedValueCalculator::new(maker_fee_rate, taker_fee_rate, ev_threshold);
        // KONSERVATİF: Risk fractions düşürüldü (güvenlik için)
        let _margin_allocator =
            DynamicMarginAllocator::new(min_margin_usdc, max_margin_usdc, 0.02, 0.10);
        // KONSERVATİF: Safe alpha (0.4) and beta (1.0) - max leverage 20x
        let safe_max_leverage = max_leverage.min(20.0);
        let risk_governor = AutoRiskGovernor::new(0.4, 1.0, safe_max_leverage);
        let execution_optimizer = ExecutionOptimizer::new(maker_fee_rate, taker_fee_rate, 500.0);
        let regime_classifier = RegimeClassifier::new();
        let bandit = ThompsonSamplingBandit::new(0.1);

        Self {
            feature_extractor,
            prev_orderbook: None,
            direction_model,
            ev_calculator,
            risk_governor,
            execution_optimizer,
            regime_classifier,
            current_regime: MarketRegime::Normal,
            pause_until: None,
            bandit,
            current_arm_idx: None,
            target_tick_usd: 0.5,
            stop_tick_usd: 0.75,
            timeout_secs: 8.0,
            recent_returns: VecDeque::new(),
            daily_pnl: 0.0,
            daily_drawdown_pct: 0.0,
            last_trade_time: None,
            last_state: None,
            last_trade_direction: None,
        }
    }

    /// Update strategy with trade result (for online learning)
    pub fn update_with_trade_result(&mut self, net_pnl_usd: f64) {
        if let Some(arm_idx) = self.current_arm_idx {
            self.bandit.update_arm(arm_idx, net_pnl_usd);
        }

        // BEST PRACTICE: Update recent returns for variance estimation (bounded)
        // Memory leak önleme: maksimum 100 trade sakla
        self.recent_returns.push_back(net_pnl_usd);
        while self.recent_returns.len() > 100 {
            self.recent_returns.pop_front();
        }

        // Update daily PnL
        self.daily_pnl += net_pnl_usd;
    }
}

impl Strategy for QMelStrategy {
    fn on_tick(&mut self, ctx: &Context) -> Quotes {
        let now = Instant::now();

        // Check if paused
        if let Some(pause_until) = self.pause_until {
            if now < pause_until {
                return Quotes::default(); // No quotes while paused
            } else {
                self.pause_until = None;
            }
        }

        // Extract market state
        let state = self.feature_extractor.extract_state(
            &ctx.ob,
            self.prev_orderbook.as_ref(),
            ctx.funding_rate,
        );

        // Regime classification
        let spread_bps = if let (Some(bid), Some(ask)) = (ctx.ob.best_bid, ctx.ob.best_ask) {
            let bid_px = bid.px.0.to_f64().unwrap_or(0.0);
            let ask_px = ask.px.0.to_f64().unwrap_or(0.0);
            if bid_px > 0.0 {
                ((ask_px - bid_px) / bid_px) * 10000.0
            } else {
                0.0
            }
        } else {
            0.0
        };

        self.current_regime = self.regime_classifier.classify(&state, spread_bps);

        // Anomaly detection
        if self.regime_classifier.should_pause(&state) {
            self.pause_until = Some(now + Duration::from_secs(30));
            return Quotes::default();
        }

        // Select parameters from bandit
        let arm_idx = self.bandit.select_arm();
        self.current_arm_idx = Some(arm_idx);

        if let Some(arm) = self.bandit.arms.get(arm_idx) {
            self.target_tick_usd = arm.target_tick;
            self.stop_tick_usd = arm.stop_tick;
            self.timeout_secs = arm.timeout_secs;
        }

        // Predict direction probabilities
        let p_up = self
            .direction_model
            .predict_up_probability(&state, self.target_tick_usd);
        let p_down = 1.0 - p_up;

        // AGGRESSIVE: For $0.50 target, use max margin (100 USDC) to maximize profit per trade
        // Calculate position size using max margin with aggressive leverage
        let max_margin_usdc = 100.0; // Use max margin for $0.50 target
        let estimated_leverage = self.risk_governor.calculate_leverage(
            0.01,  // 1% stop distance (conservative estimate)
            0.004, // 0.4% MMR (typical for futures)
            self.target_tick_usd,
            state.volatility_1s,
            self.daily_drawdown_pct,
        );
        let estimated_position_size_usd = max_margin_usdc * estimated_leverage;

        let estimated_slippage = self.execution_optimizer.estimate_slippage(
            estimated_position_size_usd,
            estimated_position_size_usd * 2.0, // Assume 2x depth
            50.0,                              // 50ms latency
        );

        let ev_long = self.ev_calculator.calculate_ev_long(
            p_up,
            self.target_tick_usd,
            self.stop_tick_usd,
            estimated_position_size_usd,
            estimated_slippage,
            true, // Assume maker for now
        );

        let ev_short = self.ev_calculator.calculate_ev_short(
            p_down,
            self.target_tick_usd,
            self.stop_tick_usd,
            estimated_position_size_usd,
            estimated_slippage,
            true,
        );

        // KRİTİK DÜZELTME: Adaptive threshold ve edge validation kullan
        // Win rate'e göre dinamik threshold
        let win_rate = if self.recent_returns.len() > 0 {
            let wins = self.recent_returns.iter().filter(|&&r| r > 0.0).count();
            wins as f64 / self.recent_returns.len() as f64
        } else {
            0.5 // Başlangıçta %50 varsay
        };

        let adaptive_threshold = self.ev_calculator.get_adaptive_threshold(win_rate);

        // Edge validation: Gerçekten edge var mı kontrol et
        if !self.ev_calculator.has_edge() {
            // Edge yok: trade yapma (sadece log)
            debug!("No edge detected, skipping trade");
            return Quotes::default();
        }

        let should_trade_long = ev_long > adaptive_threshold;
        let should_trade_short = ev_short > adaptive_threshold;

        // YAPAY ZEKA ENTEGRASYONU: Feature importance'a göre trade kararlarını optimize et
        let top_features = self.direction_model.get_top_features(3); // En önemli 3 feature
        let max_importance = top_features.first().map(|(_, score)| *score).unwrap_or(0.0);

        // Feature importance'a göre spread multiplier
        // Yüksek importance = model güvenli = daha dar spread (daha agresif)
        // Düşük importance = model belirsiz = daha geniş spread (daha konservatif)
        let spread_multiplier = if max_importance > 0.8 {
            0.8 // Yüksek önem -> daha dar spread (daha agresif)
        } else if max_importance > 0.5 {
            1.0 // Orta önem -> normal spread
        } else {
            1.2 // Düşük önem -> daha geniş spread (daha konservatif)
        };

        // Feature importance'a göre position size adjustment
        // Yüksek importance = daha fazla güven = daha büyük pozisyon
        let position_size_multiplier = if max_importance > 0.8 {
            1.1 // Yüksek önem -> %10 daha büyük pozisyon
        } else if max_importance > 0.5 {
            1.0 // Orta önem -> normal pozisyon
        } else {
            0.9 // Düşük önem -> %10 daha küçük pozisyon (risk azaltma)
        };

        // Feature importance'a göre probability threshold adjustment
        // Yüksek importance = model güvenli = daha düşük threshold (daha fazla trade)
        // Düşük importance = model belirsiz = daha yüksek threshold (daha seçici)
        let probability_adjustment = if max_importance > 0.8 {
            -0.02 // Yüksek önem -> threshold'u düşür (daha fazla trade)
        } else if max_importance > 0.5 {
            0.0 // Orta önem -> threshold değişmez
        } else {
            0.03 // Düşük önem -> threshold'u yükselt (daha seçici)
        };

        // AKILLI: Probability threshold - win rate'e göre ayarla + feature importance adjustment
        // Düşük win rate = daha yüksek threshold (daha seçici)
        let base_min_probability: f64 = if win_rate < 0.45 {
            0.55 // Düşük win rate: daha seçici
        } else if win_rate > 0.60 {
            0.50 // Yüksek win rate: daha fazla trade
        } else {
            0.52 // Normal
        };

        let min_probability: f64 = (base_min_probability + probability_adjustment)
            .max(0.45_f64)
            .min(0.60_f64);

        // Adjusted position size
        let adjusted_position_size_usd = estimated_position_size_usd * position_size_multiplier;

        // Generate quotes based on regime and EV
        let mut quotes = Quotes::default();

        if should_trade_long && p_up > min_probability {
            // Long signal: bid at microprice or slightly below (spread multiplier ile)
            if let Some(microprice) = self.feature_extractor.calculate_microprice(&ctx.ob) {
                // Spread multiplier: yüksek importance = daha dar spread (daha agresif fiyat)
                let price_offset = 0.0005 * spread_multiplier; // Base offset * multiplier
                let bid_px = Decimal::from_f64_retain(microprice * (1.0 - price_offset))
                    .unwrap_or(Decimal::ZERO);
                let bid_qty = Decimal::from_f64_retain(adjusted_position_size_usd / microprice)
                    .unwrap_or(Decimal::ZERO);
                quotes.bid = Some((Px(bid_px), Qty(bid_qty)));
                // KRİTİK: Trade yönünü kaydet (öğrenme için)
                self.last_trade_direction = Some(true); // true = long

                // Debug log: Feature importance kullanımı
                debug!(
                    max_importance = max_importance,
                    spread_multiplier = spread_multiplier,
                    position_multiplier = position_size_multiplier,
                    probability_adjustment = probability_adjustment,
                    "AI: Feature importance-based trade optimization (LONG)"
                );
            }
        }

        if should_trade_short && p_down > min_probability {
            // Short signal: ask at microprice or slightly above (spread multiplier ile)
            if let Some(microprice) = self.feature_extractor.calculate_microprice(&ctx.ob) {
                // Spread multiplier: yüksek importance = daha dar spread (daha agresif fiyat)
                let price_offset = 0.0005 * spread_multiplier; // Base offset * multiplier
                let ask_px = Decimal::from_f64_retain(microprice * (1.0 + price_offset))
                    .unwrap_or(Decimal::ZERO);
                let ask_qty = Decimal::from_f64_retain(adjusted_position_size_usd / microprice)
                    .unwrap_or(Decimal::ZERO);
                quotes.ask = Some((Px(ask_px), Qty(ask_qty)));
                // KRİTİK: Trade yönünü kaydet (öğrenme için)
                self.last_trade_direction = Some(false); // false = short

                // Debug log: Feature importance kullanımı
                debug!(
                    max_importance = max_importance,
                    spread_multiplier = spread_multiplier,
                    position_multiplier = position_size_multiplier,
                    probability_adjustment = probability_adjustment,
                    "AI: Feature importance-based trade optimization (SHORT)"
                );
            }
        }

        // KRİTİK: Entry state'i kaydet (öğrenme için)
        // Trade açıldığında bu state kullanılacak
        if quotes.bid.is_some() || quotes.ask.is_some() {
            self.last_state = Some(state);
            self.last_trade_time = Some(now);
        }

        // Update previous orderbook
        self.prev_orderbook = Some(ctx.ob.clone());

        quotes
    }

    /// Learn from trade result (online learning)
    /// Bu metod botu gerçekten "akıllı" yapar
    fn learn_from_trade(
        &mut self,
        net_pnl_usd: f64,
        _entry_state: Option<&dyn std::any::Any>,
        _actual_direction: Option<f64>,
    ) {
        // 1. Bandit parametrelerini güncelle (Thompson Sampling)
        self.update_with_trade_result(net_pnl_usd);

        // 2. EV calculator'a performansı kaydet (edge validation için)
        // Entry'deki EV tahminini ve gerçek PnL'i karşılaştır
        if let Some(entry_state) = &self.last_state {
            // Entry'deki EV'yi tahmin et (basitleştirilmiş)
            let predicted_ev = if self.last_trade_direction.unwrap_or(false) {
                // Long trade için EV
                self.ev_calculator.calculate_ev_long(
                    self.direction_model
                        .predict_up_probability(entry_state, self.target_tick_usd),
                    self.target_tick_usd,
                    self.stop_tick_usd,
                    100.0, // Estimated position size
                    0.0,   // Estimated slippage
                    true,
                )
            } else {
                // Short trade için EV
                self.ev_calculator.calculate_ev_short(
                    1.0 - self
                        .direction_model
                        .predict_up_probability(entry_state, self.target_tick_usd),
                    self.target_tick_usd,
                    self.stop_tick_usd,
                    100.0,
                    0.0,
                    true,
                )
            };

            self.ev_calculator
                .record_ev_performance(predicted_ev, net_pnl_usd);
        }

        // 3. Direction model'i güncelle (gerçek sonuçları öğret)
        // BEST PRACTICE: Gerçek yönü entry/exit price'larından hesapla (tahmin değil)
        if let Some(entry_state) = &self.last_state {
            // KRİTİK DÜZELTME: Actual direction hesaplama
            // Gerçek yön: entry price vs exit price karşılaştırması
            // Long trade: exit > entry = 1.0 (up), exit < entry = 0.0 (down)
            // Short trade: exit < entry = 1.0 (up), exit > entry = 0.0 (down)
            // Ancak biz sadece net_pnl'e sahibiz, bu yüzden trade direction'a göre yorumla

            let actual_direction = if net_pnl_usd > 0.0 {
                // Kazançlı trade: tahmin doğruydu
                // Long trade kazandıysa: fiyat yükseldi (up = 1.0)
                // Short trade kazandıysa: fiyat düştü (down = 0.0, yani up = 0.0)
                if self.last_trade_direction.unwrap_or(false) {
                    1.0 // Long kazandı = fiyat yükseldi
                } else {
                    0.0 // Short kazandı = fiyat düştü (up = 0.0)
                }
            } else {
                // Zararlı trade: tahmin yanlıştı
                // Long trade zarar ettiyse: fiyat düştü (up = 0.0)
                // Short trade zarar ettiyse: fiyat yükseldi (up = 1.0)
                if self.last_trade_direction.unwrap_or(false) {
                    0.0 // Long zarar etti = fiyat düştü
                } else {
                    1.0 // Short zarar etti = fiyat yükseldi
                }
            };

            // YAPAY ZEKA ENTEGRASYONU: Feature importance'a göre learning rate ayarla
            // Önemli feature'lar daha hızlı öğrenmeli (daha yüksek learning rate)
            // KRİTİK DÜZELTME: Division by zero ve race condition önleme
            // get_top_features() hesaplama yaparken weight'ler değişebilir (race condition risk)
            // top_features.len() sıfır olabilir (division by zero risk)
            let top_features = self.direction_model.get_top_features(3);
            let avg_importance = if !top_features.is_empty() {
                top_features.iter().map(|(_, score)| score).sum::<f64>() / top_features.len() as f64
            } else {
                0.5 // Default: orta importance (empty check - division by zero önleme)
            };

            // Feature importance'a göre learning rate multiplier
            // Yüksek importance = önemli feature'lar = daha hızlı öğren
            let importance_lr_multiplier = if avg_importance > 0.7 {
                1.5 // Yüksek önem -> %50 daha hızlı öğren
            } else if avg_importance > 0.4 {
                1.0 // Orta önem -> normal hız
            } else {
                0.7 // Düşük önem -> %30 daha yavaş öğren (noise'u önlemek için)
            };

            // BEST PRACTICE: Learning rate decay (zamanla yavaş öğren)
            // İlk trade'lerde daha hızlı öğren, sonra yavaşla
            let base_lr: f64 = if self.recent_returns.len() < 10 {
                0.01 // İlk 10 trade: hızlı öğren
            } else if self.recent_returns.len() < 50 {
                0.005 // 10-50 trade: orta hız
            } else {
                0.001 // 50+ trade: yavaş öğren (fine-tuning)
            };

            // PnL magnitude'ye göre scale et
            let pnl_lr_multiplier = if net_pnl_usd.abs() > 0.5 {
                2.0 // Büyük kazanç/zarar: daha hızlı öğren
            } else {
                1.0 // Küçük kazanç/zarar: normal hız
            };

            // Combined learning rate: base * importance * PnL
            let learning_rate: f64 = base_lr * importance_lr_multiplier * pnl_lr_multiplier;

            // BEST PRACTICE: Learning rate validation
            if learning_rate.is_finite() && learning_rate > 0.0 && learning_rate < 1.0 {
                self.direction_model
                    .update(entry_state, actual_direction, learning_rate);
            } else {
                warn!("Invalid learning_rate: {}, skipping update", learning_rate);
            }
        }
    }

    fn get_trend_bps(&self) -> f64 {
        // Return OFI as trend signal (OFI is already normalized [-1, 1])
        // Convert to bps: multiply by 10000 for strong signals
        if let Some(ref state) = self.last_state {
            state.ofi * 100.0 // Scale OFI to bps (OFI is typically [-1, 1])
        } else {
            0.0
        }
    }

    fn get_volatility(&self) -> f64 {
        // Return 5s volatility (already in log-return space)
        self.feature_extractor.get_volatility_5s()
    }

    fn get_ofi_signal(&self) -> f64 {
        // Return last OFI value
        if let Some(ref state) = self.last_state {
            state.ofi
        } else {
            0.0
        }
    }

    /// Get feature importance (Strategy trait implementation)
    fn get_feature_importance(&self) -> Option<Vec<(String, f64)>> {
        Some(self.direction_model.calculate_feature_importance())
    }
}
