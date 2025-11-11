//location: /crates/app/src/trend.rs
// Trend Module: Analyze tokens, learn with AI, get best tokens to invest

use crate::types::*;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::time::Instant;
use tracing::{debug, info};

/// Token analysis result
#[derive(Debug, Clone)]
pub struct TokenAnalysis {
    pub symbol: String,
    pub trend_score: f64,        // -1.0 to 1.0 (negative = downtrend, positive = uptrend)
    pub momentum_bps: f64,       // Price momentum in basis points
    pub volatility: f64,         // Volatility measure
    pub signal_strength: f64,    // 0.0 to 1.0 (how strong the signal is)
    pub recommendation: String,  // "LONG", "SHORT", "HOLD"
}

/// Simple AI learning state (online learning)
pub struct TrendLearner {
    // Learning weights for different features
    weights: HashMap<String, f64>,
    // Recent trade results for learning
    recent_results: Vec<(String, f64, f64)>, // (symbol, predicted_score, actual_pnl)
    learning_rate: f64,
}

impl TrendLearner {
    pub fn new() -> Self {
        let mut weights = HashMap::new();
        // Initialize with default weights
        weights.insert("momentum".to_string(), 0.3);
        weights.insert("volatility".to_string(), -0.2); // Negative: high volatility = bad
        weights.insert("trend".to_string(), 0.4);
        weights.insert("volume".to_string(), 0.1);
        
        Self {
            weights,
            recent_results: Vec::new(),
            learning_rate: 0.01,
        }
    }
    
    /// Learn from trade result (AI learning)
    pub fn learn_with_ai(&mut self, symbol: &str, predicted_score: f64, actual_pnl: f64) {
        // Simple gradient descent: adjust weights based on error
        let error = actual_pnl - predicted_score;
        
        // Update weights (simplified - in real AI this would be more sophisticated)
        for (key, weight) in self.weights.iter_mut() {
            let adjustment = self.learning_rate * error * 0.1; // Small adjustment
            *weight = (*weight + adjustment).clamp(-1.0, 1.0);
        }
        
        // Store result (keep last 100)
        self.recent_results.push((symbol.to_string(), predicted_score, actual_pnl));
        if self.recent_results.len() > 100 {
            self.recent_results.remove(0);
        }
        
        debug!(
            symbol = %symbol,
            error = error,
            "learned from trade"
        );
    }
    
    /// Get current weights (for analysis)
    pub fn get_weights(&self) -> &HashMap<String, f64> {
        &self.weights
    }
}

/// Analyze a token and return analysis result
pub fn describe_and_analyse_token(
    symbol: &str,
    bid: Decimal,
    ask: Decimal,
    price_history: &[(Instant, Decimal)],
    learner: &TrendLearner,
) -> TokenAnalysis {
    let mid_price = (bid + ask) / Decimal::from(2);
    let spread_bps = ((ask - bid) / mid_price * Decimal::from(10000))
        .to_f64()
        .unwrap_or(0.0);
    
    // Calculate momentum (price change over last 5 prices)
    let momentum_bps = if price_history.len() >= 2 {
        let recent: Vec<Decimal> = price_history
            .iter()
            .rev()
            .take(5.min(price_history.len()))
            .map(|(_, px)| *px)
            .collect();
        
        if recent.len() >= 2 {
            let first = recent.last().unwrap();
            let last = recent.first().unwrap();
            if !first.is_zero() {
                ((*last - *first) / *first * Decimal::from(10000))
                    .to_f64()
                    .unwrap_or(0.0)
            } else {
                0.0
            }
        } else {
            0.0
        }
    } else {
        0.0
    };
    
    // Calculate volatility (standard deviation of recent prices)
    let volatility = if price_history.len() >= 2 {
        let prices: Vec<f64> = price_history
            .iter()
            .rev()
            .take(10.min(price_history.len()))
            .map(|(_, px)| px.to_f64().unwrap_or(0.0))
            .collect();
        
        if prices.len() >= 2 {
            let mean = prices.iter().sum::<f64>() / prices.len() as f64;
            let variance = prices.iter()
                .map(|p| (p - mean).powi(2))
                .sum::<f64>() / prices.len() as f64;
            variance.sqrt()
        } else {
            0.0
        }
    } else {
        0.0
    };
    
    // Calculate trend score using learner weights
    let momentum_score = (momentum_bps / 100.0).clamp(-1.0, 1.0);
    let vol_penalty = if volatility > 0.05 { -0.3 } else { 0.0 };
    let trend_score = learner.weights.get("momentum").unwrap_or(&0.3) * momentum_score
        + learner.weights.get("volatility").unwrap_or(&-0.2) * vol_penalty
        + learner.weights.get("trend").unwrap_or(&0.4) * momentum_score.signum();
    
    // Signal strength (0.0 to 1.0)
    let signal_strength = trend_score.abs();
    
    // Recommendation
    let recommendation = if trend_score > 0.3 {
        "LONG"
    } else if trend_score < -0.3 {
        "SHORT"
    } else {
        "HOLD"
    };
    
    TokenAnalysis {
        symbol: symbol.to_string(),
        trend_score,
        momentum_bps,
        volatility,
        signal_strength,
        recommendation: recommendation.to_string(),
    }
}

/// Get best tokens to invest now after analysis
pub fn get_best_tokens_to_invest_now_after_analyse(
    analyses: &[TokenAnalysis],
    max_count: usize,
) -> Vec<String> {
    let mut scored: Vec<(String, f64)> = analyses
        .iter()
        .map(|a| {
            // Score = trend_score * signal_strength (higher is better)
            let score = a.trend_score.abs() * a.signal_strength;
            (a.symbol.clone(), score)
        })
        .collect();
    
    // Sort by score (descending)
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    
    // Return top N
    scored
        .iter()
        .take(max_count)
        .filter(|(_, score)| *score > 0.2) // Minimum threshold
        .map(|(symbol, _)| symbol.clone())
        .collect()
}

