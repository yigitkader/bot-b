use std::collections::HashMap;
use crate::types::{SignalSide, TrendDirection};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Timeframe {
    M1,  // 1 minute
    M5,  // 5 minutes (main timeframe)
    M15, // 15 minutes
    H1,  // 1 hour
    H4,  // 4 hours
}

#[derive(Debug, Clone)]
pub struct TimeframeSignal {
    pub trend: TrendDirection,
    pub rsi: f64,
    pub ema_fast: f64,
    pub ema_slow: f64,
    pub strength: f64, // 0.0-1.0
}

#[derive(Debug, Clone)]
pub struct MultiTimeframeAnalysis {
    timeframes: HashMap<Timeframe, TimeframeSignal>,
}

#[derive(Debug, Clone, Copy)]
pub enum DivergenceType {
    BullishDivergence, // Short-term bullish, long-term bearish (risky long)
    BearishDivergence, // Short-term bearish, long-term bullish (risky short)
}

#[derive(Debug, Clone)]
pub struct AlignedSignal {
    pub side: SignalSide,
    pub alignment_pct: f64, // 0.0-1.0 (how many TFs agree)
    pub strength: f64,      // Average strength across TFs
    pub participating_timeframes: usize,
}

impl MultiTimeframeAnalysis {
    pub fn new() -> Self {
        Self {
            timeframes: HashMap::new(),
        }
    }

    pub fn add_timeframe(&mut self, tf: Timeframe, signal: TimeframeSignal) {
        self.timeframes.insert(tf, signal);
    }

    pub fn get_timeframe(&self, tf: Timeframe) -> Option<&TimeframeSignal> {
        self.timeframes.get(&tf)
    }

    pub fn calculate_confluence(&self, direction: SignalSide, atr_pct: Option<f64>) -> f64 {
        if self.timeframes.is_empty() {
            return 0.0;
        }

        let mut confluence_score = 0.0;
        let mut total_weight = 0.0;

        let base_weights = vec![
            (Timeframe::M1, 0.0),
            (Timeframe::M5, 0.25),
            (Timeframe::M15, 0.3),
            (Timeframe::H1, 0.3),
            (Timeframe::H4, 0.15),
        ];

        let volatility_multiplier = if let Some(atr) = atr_pct {
            if atr > 0.02 {
                vec![
                    (Timeframe::M1, 0.0),
                    (Timeframe::M5, 0.15),
                    (Timeframe::M15, 0.25),
                    (Timeframe::H1, 0.4),
                    (Timeframe::H4, 0.2),
                ]
            } else if atr < 0.01 {
                vec![
                    (Timeframe::M1, 0.0),
                    (Timeframe::M5, 0.3),
                    (Timeframe::M15, 0.3),
                    (Timeframe::H1, 0.25),
                    (Timeframe::H4, 0.15),
                ]
            } else {
                base_weights.clone()
            }
        } else {
            base_weights.clone()
        };

        for (tf, weight) in volatility_multiplier {
            if let Some(signal) = self.timeframes.get(&tf) {
                total_weight += weight;

                let agrees = match direction {
                    SignalSide::Long => matches!(signal.trend, TrendDirection::Up),
                    SignalSide::Short => matches!(signal.trend, TrendDirection::Down),
                    SignalSide::Flat => false,
                };

                if agrees {
                    confluence_score += weight * signal.strength;
                }
            }
        }

        if total_weight > 0.0 {
            confluence_score / total_weight
        } else {
            0.0
        }
    }

    pub fn detect_timeframe_divergence(&self) -> Option<DivergenceType> {
        let short_term = self.timeframes.get(&Timeframe::M5);
        let mid_term = self.timeframes.get(&Timeframe::M15);
        let long_term = self.timeframes.get(&Timeframe::H1);

        match (short_term, mid_term, long_term) {
            (Some(st), Some(mt), Some(lt)) => {
                if matches!(st.trend, TrendDirection::Up)
                    && matches!(lt.trend, TrendDirection::Down)
                {
                    if matches!(mt.trend, TrendDirection::Down) {
                        Some(DivergenceType::BullishDivergence)
                    } else {
                        None
                    }
                }
                else if matches!(st.trend, TrendDirection::Down)
                    && matches!(lt.trend, TrendDirection::Up)
                {
                    if matches!(mt.trend, TrendDirection::Up) {
                        Some(DivergenceType::BearishDivergence)
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            (Some(st), None, Some(lt)) => {
                if matches!(st.trend, TrendDirection::Up)
                    && matches!(lt.trend, TrendDirection::Down)
                {
                    Some(DivergenceType::BullishDivergence)
                }
                else if matches!(st.trend, TrendDirection::Down)
                    && matches!(lt.trend, TrendDirection::Up)
                {
                    Some(DivergenceType::BearishDivergence)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    pub fn generate_aligned_signal(&self) -> Option<AlignedSignal> {
        if self.timeframes.len() < 3 {
            return None;
        }

        let mut bullish_count = 0;
        let mut bearish_count = 0;
        let mut total_strength = 0.0;

        for signal in self.timeframes.values() {
            match signal.trend {
                TrendDirection::Up => {
                    bullish_count += 1;
                    total_strength += signal.strength;
                }
                TrendDirection::Down => {
                    bearish_count += 1;
                    total_strength += signal.strength;
                }
                TrendDirection::Flat => {}
            }
        }

        let total_count = bullish_count + bearish_count;
        if total_count == 0 {
            return None;
        }

        let alignment_pct = if bullish_count > bearish_count {
            bullish_count as f64 / total_count as f64
        } else {
            bearish_count as f64 / total_count as f64
        };

        if alignment_pct >= 0.8 {
            let avg_strength = total_strength / total_count as f64;

            if bullish_count > bearish_count {
                Some(AlignedSignal {
                    side: SignalSide::Long,
                    alignment_pct,
                    strength: avg_strength,
                    participating_timeframes: bullish_count,
                })
            } else {
                Some(AlignedSignal {
                    side: SignalSide::Short,
                    alignment_pct,
                    strength: avg_strength,
                    participating_timeframes: bearish_count,
                })
            }
        } else {
            None
        }
    }
}

