
use crate::types::OrderBook;
use rust_decimal::prelude::ToPrimitive;
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use tracing::warn;


/// Market state vector (9 dimensions for Q-MEL)
#[derive(Clone, Debug)]
pub struct MarketState {
    pub ofi: f64,
    pub microprice: f64,
    pub spread_velocity: f64,
    pub liquidity_pressure: f64,
    pub volatility_1s: f64,
    pub volatility_5s: f64,
    pub cancel_trade_ratio: f64,
    pub oi_delta_30s: f64,
    pub funding_rate: Option<f64>,
    pub timestamp: Instant,
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
            timestamp: Instant::now(),
        }
    }
}

/// Feature extractor for Q-MEL algorithm
pub struct FeatureExtractor {
    vol_1s_ewma: f64,
    vol_5s_ewma: f64,
    last_price: Option<f64>,
    last_spread: Option<f64>,
    last_spread_time: Option<Instant>,
    
    price_history_1s: VecDeque<(f64, Instant)>,
    price_history_5s: VecDeque<(f64, Instant)>,
    
    cancel_count: u64,
    trade_count: u64,
    last_reset_time: Instant,
    
    oi_history: VecDeque<(f64, Instant)>,
    
    prev_orderbook: Option<OrderBook>,
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
            prev_orderbook: None,
        }
    }

    /// Calculate Order Flow Imbalance (OFI)
    /// OFI = (ΔV_bid - ΔV_ask) / (ΔV_bid + ΔV_ask + ε)
    pub fn calculate_ofi(
        &self,
        ob: &OrderBook,
        prev_ob: Option<&OrderBook>,
    ) -> f64 {
        let epsilon = 1e-8;
        
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
            let total = v_bid + v_ask + epsilon;
            if total.abs() < epsilon {
                return 0.0;
            }
            (v_bid - v_ask) / total
        }
    }

    /// Get weighted volumes from order book (top-K levels)
    fn get_weighted_volumes(&self, ob: &OrderBook) -> (f64, f64) {
        if let (Some(ref top_bids), Some(ref top_asks)) = (&ob.top_bids, &ob.top_asks) {
            let v_bid: f64 = top_bids.iter()
                .map(|level| level.qty.0.to_f64().unwrap_or(0.0))
                .sum();
            let v_ask: f64 = top_asks.iter()
                .map(|level| level.qty.0.to_f64().unwrap_or(0.0))
                .sum();
            (v_bid, v_ask)
        } else {
            let v_bid = ob.best_bid
                .as_ref()
                .map(|level| level.qty.0.to_f64().unwrap_or(0.0))
                .unwrap_or(0.0);
            let v_ask = ob.best_ask
                .as_ref()
                .map(|level| level.qty.0.to_f64().unwrap_or(0.0))
                .unwrap_or(0.0);
            (v_bid, v_ask)
        }
    }

    /// Calculate microprice
    /// microprice = (P_ask * D_bid + P_bid * D_ask) / (D_bid + D_ask)
    pub fn calculate_microprice(&self, ob: &OrderBook) -> Option<f64> {
        let (v_bid, v_ask) = self.get_weighted_volumes(ob);
        
        let bid_px = ob.best_bid.as_ref()?.px.0.to_f64()?;
        let ask_px = ob.best_ask.as_ref()?.px.0.to_f64()?;
        
        let total_depth = v_bid + v_ask;
        if total_depth < 1e-8 {
            return None;
        }
        
        Some((ask_px * v_bid + bid_px * v_ask) / total_depth)
    }

    /// Calculate spread velocity (d(spread)/dt)
    pub fn calculate_spread_velocity(
        &mut self,
        ob: &OrderBook,
    ) -> f64 {
        let current_spread = self.get_spread(ob);
        let now = Instant::now();
        
        if let (Some(prev_spread), Some(prev_time)) = (self.last_spread, self.last_spread_time) {
            let dt = now.duration_since(prev_time).as_secs_f64();
            if dt > 0.0 && dt < 10.0 {
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
        if let (Some(bid), Some(ask)) = (ob.best_bid.as_ref(), ob.best_ask.as_ref()) {
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
            return 10.0;
        }
        v_ask / v_bid
    }

    /// Update volatility (EWMA)
    pub fn update_volatility(&mut self, price: f64) {
        let now = Instant::now();
        
        self.price_history_1s.push_back((price, now));
        self.price_history_5s.push_back((price, now));
        
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
        
        if let Some(prev_price) = self.last_price {
            if prev_price > 0.0 {
                let ret = (price / prev_price).ln();
                
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

    /// Update cancel/trade ratio
    pub fn record_cancel(&mut self) {
        self.cancel_count += 1;
    }

    pub fn record_trade(&mut self) {
        self.trade_count += 1;
    }

    /// Get cancel/trade ratio (reset every 10 seconds)
    pub fn get_cancel_trade_ratio(&mut self) -> f64 {
        let now = Instant::now();
        if now.duration_since(self.last_reset_time) > Duration::from_secs(10) {
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

    /// Update OI delta tracking
    pub fn update_oi(&mut self, oi: f64) {
        let now = Instant::now();
        self.oi_history.push_back((oi, now));
        
        while let Some(&(_, t)) = self.oi_history.front() {
            if now.duration_since(t) > Duration::from_secs(30) {
                self.oi_history.pop_front();
            } else {
                break;
            }
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
        funding_rate: Option<f64>,
    ) -> MarketState {
        let prev_ob = self.prev_orderbook.as_ref();
        let ofi = self.calculate_ofi(ob, prev_ob);
        let microprice = self.calculate_microprice(ob).unwrap_or(0.0);
        let spread_velocity = self.calculate_spread_velocity(ob);
        let liquidity_pressure = self.calculate_liquidity_pressure(ob);
        let cancel_trade_ratio = self.get_cancel_trade_ratio();
        let oi_delta = self.get_oi_delta_30s();
        
        if let Some(mp) = self.calculate_microprice(ob) {
            self.update_volatility(mp);
        }
        
        self.prev_orderbook = Some(ob.clone());
        
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
            timestamp: Instant::now(),
        }
    }
}


/// Expected Value (EV) calculator
pub struct ExpectedValueCalculator {
    maker_fee_rate: f64,
    taker_fee_rate: f64,
    ev_threshold: f64,
    base_ev_threshold: f64,
    recent_ev_performance: VecDeque<f64>,
}

impl ExpectedValueCalculator {
    pub fn new(maker_fee_rate: f64, taker_fee_rate: f64, ev_threshold: f64) -> Self {
        Self {
            maker_fee_rate,
            taker_fee_rate,
            ev_threshold,
            base_ev_threshold: ev_threshold,
            recent_ev_performance: VecDeque::new(),
        }
    }
    
    /// Adaptif EV threshold: Performansa göre dinamik ayarla
    pub fn get_adaptive_threshold(&self, win_rate: f64) -> f64 {
        if win_rate < 0.45 {
            self.base_ev_threshold * 1.5
        } else if win_rate > 0.60 {
            self.base_ev_threshold * 0.7
        } else {
            self.base_ev_threshold
        }
    }
    
    /// EV tahmininin performansını kaydet
    pub fn record_ev_performance(&mut self, predicted_ev: f64, actual_pnl: f64) {
        let ev_accuracy = if predicted_ev > 0.0 && actual_pnl > 0.0 {
            1.0
        } else if predicted_ev <= 0.0 && actual_pnl <= 0.0 {
            1.0
        } else {
            0.0
        };
        
        self.recent_ev_performance.push_back(ev_accuracy);
        while self.recent_ev_performance.len() > 50 {
            self.recent_ev_performance.pop_front();
        }
    }
    
    /// Edge validation: Gerçekten edge var mı kontrol et
    pub fn has_edge(&self) -> bool {
        if self.recent_ev_performance.len() < 10 {
            return true;
        }
        
        let avg_accuracy: f64 = self.recent_ev_performance.iter().sum::<f64>() / self.recent_ev_performance.len() as f64;
        avg_accuracy > 0.55
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
        let fee_rate = if is_maker { self.maker_fee_rate } else { self.taker_fee_rate };
        let fees = position_size_usd * fee_rate * 2.0;
        
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
        let fee_rate = if is_maker { self.maker_fee_rate } else { self.taker_fee_rate };
        let fees = position_size_usd * fee_rate * 2.0;
        
        p_down * target_profit_usd - p_up * stop_loss_usd - fees - estimated_slippage_usd
    }

    /// Check if EV passes the gate
    pub fn passes_alpha_gate(&self, ev: f64) -> bool {
        ev > self.ev_threshold
    }
}


/// Adam Optimizer: Adaptive Moment Estimation
/// Her parameter için adaptive learning rate sağlar
#[derive(Clone, Debug)]
struct AdamOptimizer {
    m: Vec<f64>,
    v: Vec<f64>,
    t: u64,
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
    fn update(&mut self, gradients: &[f64], weights: &mut [f64], bias: &mut f64, bias_gradient: f64) {
        let alpha: f64 = 0.001;
        let beta1: f64 = 0.9;
        let beta2: f64 = 0.999;
        let epsilon: f64 = 1e-8;
        
        self.t += 1;
        
        let beta1_t: f64 = beta1.powi(self.t as i32);
        let beta2_t: f64 = beta2.powi(self.t as i32);
        let m_bias_correction = 1.0 - beta1_t;
        let v_bias_correction = 1.0 - beta2_t;
        
        for i in 0..gradients.len() {
            self.m[i] = beta1 * self.m[i] + (1.0 - beta1) * gradients[i];
            self.v[i] = beta2 * self.v[i] + (1.0 - beta2) * gradients[i].powi(2);
            
            let m_hat = self.m[i] / m_bias_correction;
            let v_hat = self.v[i] / v_bias_correction;
            
            weights[i] -= alpha * m_hat / (v_hat.sqrt() + epsilon);
        }
        
        let bias_alpha = alpha * 0.1;
        *bias -= bias_alpha * bias_gradient;
    }
}


/// Direction probability model with Adam optimizer
pub struct DirectionModel {
    weights: Vec<f64>,
    bias: f64,
    feature_names: Vec<String>,
    feature_importance: Vec<f64>,
    feature_update_count: Vec<u64>,
    adam: AdamOptimizer,
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
        
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        use std::time::{SystemTime, UNIX_EPOCH};
        
        let mut hasher = DefaultHasher::new();
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos().hash(&mut hasher);
        let seed = hasher.finish();
        
        let std_dev = 1.0 / (feature_dim as f64).sqrt();
        let mut weights = Vec::with_capacity(feature_dim);
        for i in 0..feature_dim {
            let r = ((seed + i as u64) % 10000) as f64 / 10000.0;
            let weight = (r - 0.5) * 2.0 * std_dev;
            weights.push(weight);
        }
        
        Self {
            weights,
            bias: 0.0,
            feature_names,
            feature_importance: vec![0.0; feature_dim],
            feature_update_count: vec![0; feature_dim],
            adam: AdamOptimizer::new(feature_dim),
        }
    }
    
    /// Feature importance hesapla
    pub fn calculate_feature_importance(&self) -> Vec<(String, f64)> {
        let mut importance: Vec<(String, f64)> = self.feature_names.iter()
            .zip(self.weights.iter())
            .zip(self.feature_importance.iter())
            .zip(self.feature_update_count.iter())
            .map(|(((name, weight), importance), count)| {
                let weight_magnitude = weight.abs();
                let importance_score = importance.abs();
                let update_freq = if *count > 0 {
                    (*count as f64).ln() / 10.0
                } else {
                    0.0
                };
                
                let total_importance = weight_magnitude * (1.0 + importance_score) * (1.0 + update_freq);
                (name.clone(), total_importance)
            })
            .collect();
        
        importance.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        importance
    }
    
    /// Get top N features
    pub fn get_top_features(&self, n: usize) -> Vec<(String, f64)> {
        let importance = self.calculate_feature_importance();
        importance.into_iter().take(n).collect()
    }
    
    fn update_feature_importance(&mut self, feature_idx: usize, gradient_magnitude: f64) {
        if feature_idx < self.feature_importance.len() {
            let alpha = 0.1;
            self.feature_importance[feature_idx] = 
                alpha * gradient_magnitude + (1.0 - alpha) * self.feature_importance[feature_idx];
            self.feature_update_count[feature_idx] += 1;
        }
    }

    /// Predict probability of upward movement (p↑)
    pub fn predict_up_probability(&self, state: &MarketState, target_tick: f64) -> f64 {
        let feature_vec = self.state_to_features(state);
        let score: f64 = self.weights.iter()
            .zip(feature_vec.iter())
            .map(|(w, x)| w * x)
            .sum::<f64>() + self.bias;
        
        let p = 1.0 / (1.0 + (-score).exp());
        
        let tick_adjustment = 1.0 / (1.0 + target_tick * 10.0);
        let final_p = (p * tick_adjustment).max(0.0).min(1.0);
        
        if final_p.is_finite() && final_p >= 0.0 && final_p <= 1.0 {
            final_p
        } else {
            0.5
        }
    }

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

    /// Online update with Adam optimizer
    pub fn update(&mut self, state: &MarketState, actual_direction: f64, learning_rate: f64) {
        if !actual_direction.is_finite() || actual_direction < 0.0 || actual_direction > 1.0 {
            warn!("Invalid actual_direction: {}, skipping update", actual_direction);
            return;
        }
        
        let feature_vec = self.state_to_features(state);
        
        let feature_norm: f64 = feature_vec.iter().map(|x| x * x).sum::<f64>().sqrt();
        let normalized_features: Vec<f64> = if feature_norm > 1e-8 {
            feature_vec.iter().map(|x| x / feature_norm).collect()
        } else {
            feature_vec
        };
        
        let prediction = self.predict_up_probability(state, 1.0);
        
        if !prediction.is_finite() || prediction < 0.0 || prediction > 1.0 {
            warn!("Invalid prediction: {}, skipping update", prediction);
            return;
        }
        
        let error = actual_direction - prediction;
        let clipped_error = error.max(-1.0).min(1.0);
        
        let l2_reg = 0.0001;
        
        let mut gradients: Vec<f64> = Vec::with_capacity(normalized_features.len());
        let mut gradient_magnitudes: Vec<(usize, f64)> = Vec::new();
        
        for (idx, (w, x)) in self.weights.iter().zip(normalized_features.iter()).enumerate() {
            let gradient = clipped_error * x + l2_reg * w;
            let clipped_gradient = gradient.max(-10.0).min(10.0);
            let gradient_magnitude = clipped_gradient.abs();
            
            gradients.push(clipped_gradient);
            gradient_magnitudes.push((idx, gradient_magnitude));
        }
        
        let bias_gradient = clipped_error;
        
        self.adam.update(&gradients, &mut self.weights, &mut self.bias, bias_gradient);
        
        for (idx, w) in self.weights.iter_mut().enumerate() {
            if !w.is_finite() {
                warn!("Invalid weight after Adam update for feature {}, resetting", idx);
                *w = 0.0;
            }
        }
        
        if !self.bias.is_finite() {
            warn!("Invalid bias after Adam update, resetting");
            self.bias = 0.0;
        }
        
        for (idx, gradient_magnitude) in gradient_magnitudes {
            self.update_feature_importance(idx, gradient_magnitude);
        }
    }
}


/// Dynamic margin allocation with min 10, max 100 USDC rule
pub struct DynamicMarginAllocator {
    min_margin_usdc: f64,
    max_margin_usdc: f64,
    f_min: f64,
    f_max: f64,
}

impl DynamicMarginAllocator {
    pub fn new(min_margin_usdc: f64, max_margin_usdc: f64, f_min: f64, f_max: f64) -> Self {
        Self {
            min_margin_usdc,
            max_margin_usdc,
            f_min,
            f_max,
        }
    }

    /// Calculate optimal risk fraction using clipped Kelly
    /// f* = clip(EV / V, f_min, f_max)
    fn calculate_optimal_fraction(&self, ev: f64, variance: f64) -> f64 {
        if variance < 1e-8 {
            return self.f_min;
        }
        let kelly_f = ev / variance;
        kelly_f.max(self.f_min).min(self.f_max)
    }

    /// Split equity into margin chunks
    /// Rule: min 10, max 100 USDC per trade
    pub fn allocate_margin(
        &self,
        available_equity_usdc: f64,
        ev: f64,
        variance: f64,
    ) -> Vec<f64> {
        if available_equity_usdc < self.min_margin_usdc {
            return vec![];
        }

        let mut chunks = Vec::new();
        let mut remaining = available_equity_usdc;
        
        let f_star = self.calculate_optimal_fraction(ev, variance);
        
        while remaining >= self.min_margin_usdc {
            let margin = (f_star * remaining).max(self.min_margin_usdc).min(self.max_margin_usdc);
            
            if remaining < self.max_margin_usdc {
                if remaining >= self.min_margin_usdc {
                    chunks.push(remaining);
                }
                break;
            }
            
            chunks.push(margin);
            remaining -= margin;
            
            if chunks.len() > 100 {
                break;
            }
        }
        
        chunks
    }

    /// Calculate variance from recent returns
    pub fn estimate_variance(returns: &[f64]) -> f64 {
        if returns.is_empty() {
            return 0.01;
        }
        
        let mean = returns.iter().sum::<f64>() / returns.len() as f64;
        let variance = returns.iter()
            .map(|r| (r - mean).powi(2))
            .sum::<f64>() / returns.len() as f64;
        
        variance.max(1e-6)
    }
}


/// Auto-Risk Governor for leverage selection
pub struct AutoRiskGovernor {
    alpha: f64,
    beta: f64,
    max_leverage: f64,
}

impl AutoRiskGovernor {
    pub fn new(alpha: f64, beta: f64, max_leverage: f64) -> Self {
        Self {
            alpha: alpha.max(0.3).min(0.6),
            beta: beta.max(0.5).min(1.5),
            max_leverage,
        }
    }

    /// Calculate safe leverage based on stop distance, MMR, and volatility
    /// L ≤ (α · d_stop) / MMR
    /// L ← min(L, β · T / σ_1s)
    pub fn calculate_leverage(
        &self,
        stop_distance_pct: f64,
        mmr: f64,
        target_tick_usd: f64,
        volatility_1s: f64,
        daily_drawdown_pct: f64,
    ) -> f64 {
        let leverage_by_stop = if mmr > 0.0 {
            (self.alpha * stop_distance_pct) / mmr
        } else {
            self.max_leverage.min(20.0)
        };
        
        let leverage_by_vol = if volatility_1s > 1e-8 {
            (self.beta * target_tick_usd) / volatility_1s
        } else {
            self.max_leverage.min(20.0)
        };
        
        let drawdown_factor = if daily_drawdown_pct > 0.0 {
            (1.0 - daily_drawdown_pct * 0.3).max(0.3)
        } else {
            1.0
        };
        
        let base_leverage = leverage_by_stop.min(leverage_by_vol).min(self.max_leverage).min(20.0);
        let final_leverage = base_leverage * drawdown_factor;
        
        final_leverage.max(1.0).min(20.0).min(self.max_leverage)
    }
}


/// Execution optimizer for maker/taker decision and slippage estimation
pub struct ExecutionOptimizer {
    maker_fee_rate: f64,
    taker_fee_rate: f64,
    edge_decay_half_life_ms: f64,
}

impl ExecutionOptimizer {
    pub fn new(maker_fee_rate: f64, taker_fee_rate: f64, edge_decay_half_life_ms: f64) -> Self {
        Self {
            maker_fee_rate,
            taker_fee_rate,
            edge_decay_half_life_ms,
        }
    }

    /// Estimate fill probability for maker order
    pub fn estimate_fill_probability(
        &self,
        queue_position: usize,
        cancel_rate: f64,
    ) -> f64 {
        let base_prob = 1.0 / (1.0 + queue_position as f64 * 0.1);
        let cancel_penalty = cancel_rate.min(1.0);
        base_prob * (1.0 - cancel_penalty * 0.5)
    }

    /// Estimate expected queue wait time
    pub fn estimate_queue_wait_time(
        &self,
        queue_position: usize,
        avg_fill_rate: f64,
    ) -> f64 {
        if avg_fill_rate < 1e-8 {
            return 1000.0;
        }
        (queue_position as f64 / avg_fill_rate) * 1000.0
    }

    /// Decide maker vs taker
    pub fn decide_maker_taker(
        &self,
        edge_decay_half_life_ms: f64,
        estimated_queue_wait_ms: f64,
    ) -> bool {
        estimated_queue_wait_ms < edge_decay_half_life_ms
    }

    /// Estimate slippage cost
    pub fn estimate_slippage(
        &self,
        order_size_usd: f64,
        depth_at_price_usd: f64,
        latency_ms: f64,
    ) -> f64 {
        let size_ratio = order_size_usd / depth_at_price_usd.max(1.0);
        let base_slippage = size_ratio * 0.001;
        let latency_penalty = (latency_ms / 100.0).min(1.0) * 0.0005;
        (base_slippage + latency_penalty) * order_size_usd
    }

    /// Test if order passes slippage constraint
    pub fn passes_slippage_test(
        &self,
        ev: f64,
        estimated_slippage: f64,
        fees: f64,
        threshold: f64,
    ) -> bool {
        (ev - estimated_slippage - fees) > threshold
    }
}


/// Parameter arm for bandit algorithm
#[derive(Clone, Debug)]
pub struct ParameterArm {
    pub target_tick: f64,
    pub stop_tick: f64,
    pub timeout_secs: f64,
    pub maker_threshold: f64,
    pub reward_sum: f64,
    pub pull_count: u64,
}

impl ParameterArm {
    pub fn new(target_tick: f64, stop_tick: f64, timeout_secs: f64, maker_threshold: f64) -> Self {
        Self {
            target_tick,
            stop_tick,
            timeout_secs,
            maker_threshold,
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
    exploration_rate: f64,
}

impl ThompsonSamplingBandit {
    pub fn new(exploration_rate: f64) -> Self {
        let mut arms = Vec::new();
        
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
        
        Self {
            arms,
            exploration_rate,
        }
    }

    /// Select arm using Thompson Sampling
    pub fn select_arm(&self) -> usize {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        use std::time::{SystemTime, UNIX_EPOCH};
        
        let mut hasher = DefaultHasher::new();
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos().hash(&mut hasher);
        let seed = hasher.finish();
        
        let mut best_arm = 0;
        let mut best_sample = f64::NEG_INFINITY;
        
        for (i, arm) in self.arms.iter().enumerate() {
            let avg_reward = arm.average_reward();
            let pulls = arm.pull_count.max(1);
            
            let normalized_reward = (avg_reward + 1.0) / 2.0;
            let alpha = normalized_reward * pulls as f64 + 1.0;
            let beta = (1.0 - normalized_reward) * pulls as f64 + 1.0;
            
            let sample = if pulls == 0 {
                f64::INFINITY
            } else {
                let mean = alpha / (alpha + beta);
                let variance = (alpha * beta) / ((alpha + beta).powi(2) * (alpha + beta + 1.0));
                let noise = ((seed + i as u64) % 1000) as f64 / 1000.0 - 0.5;
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

    /// Get best arm parameters
    pub fn get_best_arm(&self) -> Option<&ParameterArm> {
        self.arms.iter()
            .max_by(|a, b| a.average_reward().partial_cmp(&b.average_reward()).unwrap_or(std::cmp::Ordering::Equal))
    }
}


#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MarketRegime {
    Normal,
    Frenzy,
    Drift,
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
            spread_threshold_wide: 0.001,
        }
    }

    pub fn classify(&self, state: &MarketState, spread_bps: f64) -> MarketRegime {
        if state.volatility_1s > self.volatility_threshold_high 
            && state.cancel_trade_ratio > self.cancel_trade_threshold {
            return MarketRegime::Frenzy;
        }
        
        if spread_bps > self.spread_threshold_wide * 10000.0 
            && state.liquidity_pressure > 5.0 {
            return MarketRegime::Frenzy;
        }
        
        if state.volatility_1s < self.volatility_threshold_low 
            && state.volatility_5s < self.volatility_threshold_low {
            return MarketRegime::Drift;
        }
        
        MarketRegime::Normal
    }

    /// Check if trading should be paused
    pub fn should_pause(&self, state: &MarketState) -> bool {
        state.cancel_trade_ratio > self.cancel_trade_threshold * 2.0
            || (state.volatility_1s > state.volatility_5s * 3.0 && state.volatility_1s > 0.1)
    }
}

