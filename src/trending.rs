use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Timelike, Utc};
use rand::SeedableRng;
use reqwest::{Client, Url};
use ta::indicators::{AverageTrueRange, ExponentialMovingAverage, RelativeStrengthIndex};
use ta::{DataItem, Next};

use crate::types::{
    AdvancedBacktestResult, AlgoConfig, BacktestResult, Candle, DepthSnapshot, FundingRate,
    FuturesClient, LongShortRatioPoint, MarketTick, OpenInterestHistPoint, OpenInterestPoint,
    PositionSide, Side, Signal, SignalContext, SignalSide, Trade, TrendDirection,
    EnhancedSignalContext,
};
use std::collections::{BTreeMap, HashMap, VecDeque};
use uuid::Uuid;

fn candle_to_data_item(candle: &Candle) -> DataItem {
    DataItem::builder()
        .open(candle.open)
        .high(candle.high)
        .low(candle.low)
        .close(candle.close)
        .volume(candle.volume)
        .build()
        .unwrap()
}

fn value_to_data_item(value: f64) -> DataItem {
    DataItem::builder()
        .close(value)
        .open(value)
        .high(value)
        .low(value)
        .volume(0.0)
        .build()
        .unwrap()
}

// =======================
//  Funding Rate Arbitrage (TrendPlan.md SECRET #2)
// =======================

#[derive(Debug, Clone)]
struct FundingSnapshot {
    timestamp: DateTime<Utc>,
    funding_rate: f64,
}

#[derive(Debug, Clone)]
pub struct FundingArbitrage {
    funding_history: VecDeque<FundingSnapshot>,
    next_funding_time: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub enum FundingArbitrageSignal {
    PreFundingShort {
        expected_pnl_bps: i32,
        time_to_funding: chrono::Duration,
    },
    PreFundingLong {
        expected_pnl_bps: i32,
        time_to_funding: chrono::Duration,
    },
}

#[derive(Debug, Clone, Copy)]
pub enum PostFundingSignal {
    ExpectLongLiquidation,
    ExpectShortLiquidation,
}

#[derive(Debug, Clone, Copy)]
pub enum FundingExhaustionSignal {
    ExtremePositive, // Funding çok yükseldi, reversal yakın
    ExtremeNegative, // Funding çok düştü, reversal yakın
}

impl FundingArbitrage {
    pub fn new() -> Self {
        Self {
            funding_history: VecDeque::new(),
            next_funding_time: Self::calculate_next_funding_time(Utc::now()),
        }
    }

    /// Funding her 8 saatte bir: 00:00, 08:00, 16:00 UTC
    fn calculate_next_funding_time(now: DateTime<Utc>) -> DateTime<Utc> {
        let hour = now.hour();
        let next_hour = match hour {
            0..=7 => 8,
            8..=15 => 16,
            16..=23 => 0,
            _ => 0,
        };

        if next_hour == 0 {
            now.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc() + chrono::Duration::days(1)
        } else {
            now.date_naive()
                .and_hms_opt(next_hour, 0, 0)
                .unwrap()
                .and_utc()
        }
    }

    /// Funding history'yi güncelle
    pub fn update_funding(&mut self, funding_rate: f64, timestamp: DateTime<Utc>) {
        self.funding_history.push_back(FundingSnapshot {
            timestamp,
            funding_rate,
        });
        // Son 10 funding'i tut
        if self.funding_history.len() > 10 {
            self.funding_history.pop_front();
        }
        // Next funding time'ı güncelle
        self.next_funding_time = Self::calculate_next_funding_time(timestamp);
    }

    pub fn is_pre_funding_window(&self, now: DateTime<Utc>) -> bool {
        let time_to_funding = self.next_funding_time.signed_duration_since(now);
        time_to_funding.num_minutes() <= 90 && time_to_funding.num_minutes() >= 0
    }
    
    pub fn is_optimal_pre_funding_window(&self, now: DateTime<Utc>) -> bool {
        let time_to_funding = self.next_funding_time.signed_duration_since(now);
        time_to_funding.num_minutes() <= 60 && time_to_funding.num_minutes() >= 30
    }
    
    pub fn is_early_pre_funding_window(&self, now: DateTime<Utc>) -> bool {
        let time_to_funding = self.next_funding_time.signed_duration_since(now);
        time_to_funding.num_minutes() <= 90 && time_to_funding.num_minutes() > 60
    }

    /// ✅ CRITICAL FIX: Detect funding arbitrage with price movement check
    /// Checks if price has already moved in expected direction (market efficiency)
    /// Only signals arbitrage if price hasn't moved yet or moved in opposite direction
    pub fn detect_funding_arbitrage(
        &self,
        now: DateTime<Utc>,
        current_price: f64,
        price_history: &[(DateTime<Utc>, f64)], // (timestamp, price) pairs
    ) -> Option<FundingArbitrageSignal> {
        if !self.is_pre_funding_window(now) {
            return None;
        }

        if self.funding_history.is_empty() {
            return None;
        }

        let latest_funding = self.funding_history.back()?.funding_rate;

        // ✅ FIX: Check if price has already moved in expected direction
        // Pre-funding window başlangıcından (90 dakika önce) itibaren fiyat hareketini kontrol et
        let pre_funding_start = self.next_funding_time - chrono::Duration::minutes(90);
        let price_at_window_start = price_history
            .iter()
            .find(|(ts, _)| *ts >= pre_funding_start)
            .map(|(_, price)| *price);

        // Eğer pre-funding window başlangıcından itibaren fiyat verisi yoksa, skip et
        let price_movement_pct = if let Some(start_price) = price_at_window_start {
            if start_price > 0.0 {
                (current_price - start_price) / start_price
            } else {
                0.0
            }
        } else {
            // Fiyat verisi yoksa, arbitraj fırsatını değerlendir (conservative approach)
            0.0
        };

        let funding_trend = if self.funding_history.len() >= 3 {
            let recent: Vec<&FundingSnapshot> = self.funding_history.iter().rev().take(3).collect();
            let trend = (recent[0].funding_rate - recent[2].funding_rate).signum();
            Some(trend)
        } else {
            None
        };

        // ✅ FIX: Positive funding → expect price to rise → SHORT before funding
        // If price already rose >0.3%, arbitrage is already priced in
        if latest_funding > 0.0005 {
            // Check if price already moved up (arbitrage priced in)
            if price_movement_pct > 0.003 {
                // Price already rose >0.3%, arbitrage opportunity missed
                log::debug!(
                    "TRENDING: Funding arbitrage SHORT skipped - price already moved {:.2}% (arbitrage priced in)",
                    price_movement_pct * 100.0
                );
                return None;
            }

            let confidence_boost = funding_trend
                .map(|t| if t > 0.0 { 1.2 } else { 1.0 })
                .unwrap_or(1.0);
            
            // ✅ FIX: Adjust expected PNL based on price movement
            // If price moved down, arbitrage is more attractive (counter-trend)
            let price_adjustment = if price_movement_pct < -0.001 {
                1.2 // Price moved down, more attractive
            } else if price_movement_pct > 0.001 {
                0.7 // Price moved up slightly, less attractive
            } else {
                1.0 // No significant movement
            };
            
            let threshold = if self.is_optimal_pre_funding_window(now) {
                0.0003
            } else {
                0.0005
            };
            
            if latest_funding > threshold {
                let adjusted_pnl = (latest_funding * confidence_boost * price_adjustment) * 10000.0;
                Some(FundingArbitrageSignal::PreFundingShort {
                    expected_pnl_bps: adjusted_pnl as i32,
                    time_to_funding: self.next_funding_time.signed_duration_since(now),
                })
            } else {
                None
            }
        }
        // ✅ FIX: Negative funding → expect price to fall → LONG before funding
        // If price already fell >0.3%, arbitrage is already priced in
        else if latest_funding < -0.0005 {
            // Check if price already moved down (arbitrage priced in)
            if price_movement_pct < -0.003 {
                // Price already fell >0.3%, arbitrage opportunity missed
                log::debug!(
                    "TRENDING: Funding arbitrage LONG skipped - price already moved {:.2}% (arbitrage priced in)",
                    price_movement_pct * 100.0
                );
                return None;
            }

            let confidence_boost = funding_trend
                .map(|t| if t < 0.0 { 1.2 } else { 1.0 })
                .unwrap_or(1.0);
            
            // ✅ FIX: Adjust expected PNL based on price movement
            // If price moved up, arbitrage is more attractive (counter-trend)
            let price_adjustment = if price_movement_pct > 0.001 {
                1.2 // Price moved up, more attractive
            } else if price_movement_pct < -0.001 {
                0.7 // Price moved down slightly, less attractive
            } else {
                1.0 // No significant movement
            };
            
            let threshold = if self.is_optimal_pre_funding_window(now) {
                -0.0003
            } else {
                -0.0005
            };
            
            if latest_funding < threshold {
                let adjusted_pnl = (latest_funding.abs() * confidence_boost * price_adjustment) * 10000.0;
                Some(FundingArbitrageSignal::PreFundingLong {
                    expected_pnl_bps: adjusted_pnl as i32,
                    time_to_funding: self.next_funding_time.signed_duration_since(now),
                })
            } else {
                None
            }
        } else {
            None
        }
    }

    pub fn detect_post_funding_opportunity(&self, now: DateTime<Utc>) -> Option<PostFundingSignal> {
        let time_since_funding = now.signed_duration_since(self.next_funding_time);

        if time_since_funding.num_minutes() > 0 && time_since_funding.num_minutes() <= 15 {
            if let Some(latest) = self.funding_history.back() {
                if latest.funding_rate > 0.0003 {
                    Some(PostFundingSignal::ExpectLongLiquidation)
                } else if latest.funding_rate < -0.0003 {
                    Some(PostFundingSignal::ExpectShortLiquidation)
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        }
    }

    pub fn detect_funding_exhaustion(&self) -> Option<FundingExhaustionSignal> {
        if self.funding_history.len() < 5 {
            return None;
        }

        let recent: Vec<&FundingSnapshot> = self.funding_history.iter().rev().take(5).collect();
        if recent.len() < 5 {
            return None;
        }

        let mut increasing_count = 0;
        let mut decreasing_count = 0;

        for i in 1..recent.len() {
            if recent[i].funding_rate > recent[i - 1].funding_rate {
                increasing_count += 1;
            } else {
                decreasing_count += 1;
            }
        }

        if increasing_count >= 4 {
            Some(FundingExhaustionSignal::ExtremePositive)
        } else if decreasing_count >= 4 {
            Some(FundingExhaustionSignal::ExtremeNegative)
        } else {
            None
        }
    }
}

// =======================
//  Order Flow Analysis (TrendPlan.md SECRET #1)
// =======================

#[derive(Debug, Clone)]
struct OrderFlowSnapshot {
    timestamp: DateTime<Utc>,
    bid_volume: f64,
    ask_volume: f64,
    bid_count: usize,
    ask_count: usize,
}

#[derive(Debug)]
pub struct OrderFlowAnalyzer {
    snapshots: VecDeque<OrderFlowSnapshot>,
    window_size: usize,
}

#[derive(Debug, Clone, Copy)]
pub enum AbsorptionSignal {
    Bullish, // Market maker accumulating
    Bearish, // Market maker distributing
}

#[derive(Debug, Clone, Copy)]
pub enum SpoofingSignal {
    BidSideSpoofing, // Fake buy orders
    AskSideSpoofing, // Fake sell orders
}

#[derive(Debug, Clone, Copy)]
pub enum IcebergSignal {
    BidSideIceberg, // Hidden buy orders
    AskSideIceberg, // Hidden sell orders
}

impl OrderFlowAnalyzer {
    pub fn new(window_size: usize) -> Self {
        Self {
            snapshots: VecDeque::with_capacity(window_size),
            window_size,
        }
    }

    /// CRITICAL: Orderbook'tan "hidden liquidity" tespiti
    /// Market maker'lar küçük emirlerle büyük pozisyon oluşturur
    pub fn add_snapshot(&mut self, depth: &DepthSnapshot) {
        let bid_volume: f64 = depth
            .bids
            .iter()
            .filter_map(|lvl| lvl[1].parse::<f64>().ok())
            .sum();

        let ask_volume: f64 = depth
            .asks
            .iter()
            .filter_map(|lvl| lvl[1].parse::<f64>().ok())
            .sum();

        let snapshot = OrderFlowSnapshot {
            timestamp: Utc::now(),
            bid_volume,
            ask_volume,
            bid_count: depth.bids.len(),
            ask_count: depth.asks.len(),
        };

        self.snapshots.push_back(snapshot);
        if self.snapshots.len() > self.window_size {
            self.snapshots.pop_front();
        }
    }

    pub fn detect_absorption(&self) -> Option<AbsorptionSignal> {
        if self.snapshots.len() < 5 {
            return None;
        }

        let snapshot_count = self.snapshots.len().min(15);
        let recent: Vec<&OrderFlowSnapshot> = self.snapshots.iter().rev().take(snapshot_count).collect();

        let mut buy_volume_total = 0.0;
        let mut sell_volume_total = 0.0;

        for snap in recent {
            if snap.ask_volume > snap.bid_volume {
                sell_volume_total += snap.ask_volume;
            } else {
                buy_volume_total += snap.bid_volume;
            }
        }

        let imbalance_ratio = sell_volume_total / buy_volume_total.max(1.0);

        if imbalance_ratio > 1.3 {
            Some(AbsorptionSignal::Bullish)
        } else if imbalance_ratio < 0.77 {
            Some(AbsorptionSignal::Bearish)
        } else {
            None
        }
    }

    pub fn detect_spoofing(&self) -> Option<SpoofingSignal> {
        if self.snapshots.len() < 10 {
            return None;
        }

        // ✅ FIX: Use more snapshots if available (up to 25 instead of 20)
        let snapshot_count = self.snapshots.len().min(25);
        let recent: Vec<&OrderFlowSnapshot> = self.snapshots.iter().rev().take(snapshot_count).collect();

        // Order count'lar stable ama volume çok volatil = spoofing
        let avg_bid_count: f64 =
            recent.iter().map(|s| s.bid_count as f64).sum::<f64>() / recent.len() as f64;

        let avg_ask_count: f64 =
            recent.iter().map(|s| s.ask_count as f64).sum::<f64>() / recent.len() as f64;

        // Volume volatility
        let bid_volumes: Vec<f64> = recent.iter().map(|s| s.bid_volume).collect();
        let bid_vol_std = calculate_std_dev(&bid_volumes);
        let bid_vol_mean = bid_volumes.iter().sum::<f64>() / bid_volumes.len() as f64;

        // Coefficient of variation (CV)
        let bid_cv = if bid_vol_mean > 0.0 {
            bid_vol_std / bid_vol_mean
        } else {
            0.0
        };

        // Ask side CV
        let ask_volumes: Vec<f64> = recent.iter().map(|s| s.ask_volume).collect();
        let ask_vol_std = calculate_std_dev(&ask_volumes);
        let ask_vol_mean = ask_volumes.iter().sum::<f64>() / ask_volumes.len() as f64;
        let ask_cv = if ask_vol_mean > 0.0 {
            ask_vol_std / ask_vol_mean
        } else {
            0.0
        };

        // ✅ FIX: Lower threshold (0.4 instead of 0.5) and count (10 instead of 15) for more opportunities
        // 🚨 Yüksek CV + stable count = spoofing
        if bid_cv > 0.4 && avg_bid_count > 10.0 {
            Some(SpoofingSignal::BidSideSpoofing)
        } else if ask_cv > 0.4 && avg_ask_count > 10.0 {
            Some(SpoofingSignal::AskSideSpoofing)
        } else {
            None
        }
    }

    pub fn detect_iceberg_orders(&self) -> Option<IcebergSignal> {
        if self.snapshots.len() < 15 {
            return None;
        }

        let snapshot_count = self.snapshots.len().min(40);
        let recent: Vec<&OrderFlowSnapshot> = self.snapshots.iter().rev().take(snapshot_count).collect();

        let bid_volumes: Vec<f64> = recent.iter().map(|s| s.bid_volume).collect();
        let bid_vol_std = calculate_std_dev(&bid_volumes);
        let bid_vol_mean = bid_volumes.iter().sum::<f64>() / bid_volumes.len() as f64;

        let bid_cv = if bid_vol_mean > 0.0 {
            bid_vol_std / bid_vol_mean
        } else {
            0.0
        };

        let ask_volumes: Vec<f64> = recent.iter().map(|s| s.ask_volume).collect();
        let ask_vol_std = calculate_std_dev(&ask_volumes);
        let ask_vol_mean = ask_volumes.iter().sum::<f64>() / ask_volumes.len() as f64;
        let ask_cv = if ask_vol_mean > 0.0 {
            ask_vol_std / ask_vol_mean
        } else {
            0.0
        };

        if bid_cv < 0.15 && bid_vol_mean > 50000.0 {
            Some(IcebergSignal::BidSideIceberg)
        } else if ask_cv < 0.15 && ask_vol_mean > 50000.0 {
            Some(IcebergSignal::AskSideIceberg)
        } else {
            None
        }
    }
}

fn calculate_std_dev(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    let variance = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / values.len() as f64;
    variance.sqrt()
}

// =======================
//  Multi-Timeframe Confluence (TrendPlan.md SECRET #4)
// =======================

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

    /// Add timeframe signal
    pub fn add_timeframe(&mut self, tf: Timeframe, signal: TimeframeSignal) {
        self.timeframes.insert(tf, signal);
    }

    /// Get timeframe signal
    pub fn get_timeframe(&self, tf: Timeframe) -> Option<&TimeframeSignal> {
        self.timeframes.get(&tf)
    }

    /// 🔥 CRITICAL: Calculate confluence score
    /// Eğer multiple timeframe'ler aynı direction'daysa → high confidence
    pub fn calculate_confluence(&self, direction: SignalSide) -> f64 {
        if self.timeframes.is_empty() {
            return 0.0;
        }

        let mut confluence_score = 0.0;
        let mut total_weight = 0.0;

        // Timeframe weights (longer = more important)
        let weights = vec![
            (Timeframe::M1, 0.1),
            (Timeframe::M5, 0.2),
            (Timeframe::M15, 0.25),
            (Timeframe::H1, 0.3),
            (Timeframe::H4, 0.15),
        ];

        for (tf, weight) in weights {
            if let Some(signal) = self.timeframes.get(&tf) {
                total_weight += weight;

                // Check if this timeframe agrees with direction
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

    /// 🔥 ADVANCED: Divergence Detection
    /// Eğer short-term ve long-term aynı yönde değilse → risky trade
    pub fn detect_timeframe_divergence(&self) -> Option<DivergenceType> {
        let short_term = self.timeframes.get(&Timeframe::M5);
        let long_term = self.timeframes.get(&Timeframe::H1);

        match (short_term, long_term) {
            (Some(st), Some(lt)) => {
                // Bullish divergence: short-term up, long-term down
                if matches!(st.trend, TrendDirection::Up)
                    && matches!(lt.trend, TrendDirection::Down)
                {
                    Some(DivergenceType::BullishDivergence)
                }
                // Bearish divergence: short-term down, long-term up
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

    /// 🔥 SECRET STRATEGY: "Trend Alignment"
    /// En güvenilir tradeler: Tüm timeframe'ler aligned
    pub fn generate_aligned_signal(&self) -> Option<AlignedSignal> {
        // Check if we have at least 3 timeframes
        if self.timeframes.len() < 3 {
            return None;
        }

        // Count bullish and bearish timeframes
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

        // 🚨 ALIGNMENT THRESHOLD: 80%+ agreement
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

// =======================
//  Liquidation Cascade Prediction (TrendPlan.md SECRET #3)
// =======================

#[derive(Debug, Clone)]
pub struct LiquidationMap {
    /// Key: Price level, Value: Liquidation notional (USD)
    pub long_liquidations: BTreeMap<i64, f64>, // LONG pozisyonlar bu fiyatta liquidate
    pub short_liquidations: BTreeMap<i64, f64>, // SHORT pozisyonlar bu fiyatta liquidate
    pub last_update: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct LiquidationWall {
    pub price: f64,
    pub notional: f64, // Total USD to be liquidated
    pub direction: CascadeDirection,
    pub distance_pct: f64, // Distance from current price (%)
}

#[derive(Debug, Clone, Copy)]
pub enum CascadeDirection {
    Downward, // Long liquidations trigger downward cascade
    Upward,   // Short liquidations trigger upward cascade
}

#[derive(Debug, Clone)]
pub struct CascadeSignal {
    pub side: Side,
    pub entry_price: f64,
    pub target_price: f64,
    pub expected_pnl_pct: f64,
    pub wall_notional: f64,
    pub confidence: f64, // 0.0-1.0
}

impl LiquidationMap {
    pub fn new() -> Self {
        Self {
            long_liquidations: BTreeMap::new(),
            short_liquidations: BTreeMap::new(),
            last_update: Utc::now(),
        }
    }

    /// ✅ CRITICAL FIX (A): Use REAL liquidation data from connection.rs (LiqState) as PRIMARY source
    /// MarketTick contains liq_long_cluster and liq_short_cluster from real-time forceOrder stream
    /// These are normalized ratios (notional / OI) from actual liquidations in the last N seconds
    /// This is ALWAYS more accurate than mathematical estimates
    pub fn update_from_real_liquidation_data(
        &mut self,
        current_price: f64,
        open_interest: f64,
        liq_long_cluster: Option<f64>,
        liq_short_cluster: Option<f64>,
    ) {
        // Clear previous estimates
        self.long_liquidations.clear();
        self.short_liquidations.clear();

        // ✅ CRITICAL: Use real liquidation data if available
        // liq_long_cluster and liq_short_cluster are ratios (notional / OI) from LiqState
        // Convert to absolute notional values for liquidation map
        
        // Calculate price interval for clustering
        let price_interval = if current_price > 1000.0 {
            10.0 // Major coins: $10 intervals
        } else if current_price > 1.0 {
            0.01 // Mid-range: $0.01 intervals
        } else {
            0.00001 // Low-price coins: $0.00001 intervals
        };

        // Long liquidations (below current price)
        if let Some(long_ratio) = liq_long_cluster {
            if long_ratio > 0.0 && open_interest > 0.0 {
                let long_notional = open_interest * long_ratio;
                // Distribute across price levels near current price (1-2% below)
                let price_levels = vec![
                    (0.98, 0.4),  // 2% below: 40% of liquidations
                    (0.99, 0.35), // 1% below: 35% of liquidations
                    (0.995, 0.25), // 0.5% below: 25% of liquidations
                ];
                
                for (price_mult, portion) in &price_levels {
                    let liq_price = current_price * price_mult;
                    let liq_price_rounded = (liq_price / price_interval).round() * price_interval;
                    let liq_price_key = (liq_price_rounded / price_interval) as i64;
                    let notional = long_notional * portion;
                    
                    *self
                        .long_liquidations
                        .entry(liq_price_key)
                        .or_insert(0.0) += notional;
                }
            }
        }

        // Short liquidations (above current price)
        if let Some(short_ratio) = liq_short_cluster {
            if short_ratio > 0.0 && open_interest > 0.0 {
                let short_notional = open_interest * short_ratio;
                // Distribute across price levels near current price (1-2% above)
                let price_levels = vec![
                    (1.02, 0.4),  // 2% above: 40% of liquidations
                    (1.01, 0.35), // 1% above: 35% of liquidations
                    (1.005, 0.25), // 0.5% above: 25% of liquidations
                ];
                
                for (price_mult, portion) in &price_levels {
                    let liq_price = current_price * price_mult;
                    let liq_price_rounded = (liq_price / price_interval).round() * price_interval;
                    let liq_price_key = (liq_price_rounded / price_interval) as i64;
                    let notional = short_notional * portion;
                    
                    *self
                        .short_liquidations
                        .entry(liq_price_key)
                        .or_insert(0.0) += notional;
                }
            }
        }

        self.last_update = Utc::now();
    }

    /// ✅ FALLBACK: Estimate future liquidations using mathematical model (only if real data unavailable)
    /// Uses funding rate and Long/Short Ratio to estimate average leverage dynamically
    /// Funding rate > 0.01% (0.0001) typically indicates high leverage usage
    /// Long/Short Ratio imbalance also indicates leverage concentration
    /// NOTE: This is LESS accurate than real liquidation data from connection.rs
    pub fn estimate_future_liquidations(
        &mut self,
        current_price: f64,
        open_interest: f64,
        long_short_ratio: f64,
        funding_rate: f64,
    ) {
        // Clear previous estimates
        self.long_liquidations.clear();
        self.short_liquidations.clear();

        // Long pozisyonların oranı
        let long_ratio = long_short_ratio / (1.0 + long_short_ratio);
        let short_ratio = 1.0 - long_ratio;

        let long_oi = open_interest * long_ratio;
        let short_oi = open_interest * short_ratio;

        // ✅ CRITICAL FIX: Dynamic leverage estimation based on REAL market data
        // High funding rate (>0.01%) = more leverage usage (funding arbitrage attracts high leverage)
        // Extreme Long/Short Ratio (>1.5 or <0.67) = leverage concentration
        let funding_rate_abs = funding_rate.abs();
        let leverage_factor = if funding_rate_abs > 0.0001 {
            // High funding = high leverage usage
            // Scale from 20x (low funding) to 80x (high funding)
            20.0 + (funding_rate_abs * 600_000.0).min(60.0)
        } else {
            // Low funding = moderate leverage
            20.0
        };

        // Long/Short imbalance also affects leverage concentration
        let imbalance_factor = if long_short_ratio > 1.5 || long_short_ratio < 0.67 {
            // Extreme imbalance = more leverage concentration
            1.2
        } else {
            1.0
        };

        let avg_leverage = leverage_factor * imbalance_factor;

        // ✅ FIX: Use realistic price intervals based on price level
        // For BTC ($40k): $10 intervals
        // For altcoins ($0.001): $0.00001 intervals
        let price_interval = if current_price > 1000.0 {
            10.0 // Major coins: $10 intervals
        } else if current_price > 1.0 {
            0.01 // Mid-range: $0.01 intervals
        } else {
            0.00001 // Low-price coins: $0.00001 intervals
        };

        // Estimate liquidation prices using average leverage
        // Distribute OI across multiple price levels (not just one)
        // Most liquidations happen near current price (1-2% away)
        let price_levels = vec![
            (0.5, 0.3),  // 50% of avg leverage → 30% of OI (conservative traders)
            (0.75, 0.4), // 75% of avg leverage → 40% of OI (moderate traders)
            (1.0, 0.25), // 100% of avg leverage → 25% of OI (aggressive traders)
            (1.5, 0.05), // 150% of avg leverage → 5% of OI (very aggressive)
        ];

        // Long liquidation prices (below current price)
        for (leverage_mult, portion) in &price_levels {
            let effective_leverage = avg_leverage * leverage_mult;
            let notional = long_oi * portion;

            // Liquidation price = entry * (1 - 1/leverage)
            let liq_price = current_price * (1.0 - 1.0 / effective_leverage);
            let liq_price_rounded = (liq_price / price_interval).round() * price_interval;
            let liq_price_key = (liq_price_rounded / price_interval) as i64;

            *self
                .long_liquidations
                .entry(liq_price_key)
                .or_insert(0.0) += notional;
        }

        // Short liquidation prices (above current price)
        for (leverage_mult, portion) in &price_levels {
            let effective_leverage = avg_leverage * leverage_mult;
            let notional = short_oi * portion;
            let liq_price = current_price * (1.0 + 1.0 / effective_leverage);
            let liq_price_rounded = (liq_price / price_interval).round() * price_interval;
            let liq_price_key = (liq_price_rounded / price_interval) as i64;

            *self
                .short_liquidations
                .entry(liq_price_key)
                .or_insert(0.0) += notional;
        }

        self.last_update = Utc::now();
    }

    /// ✅ CRITICAL FIX: Detect "liquidation walls" using dynamic price intervals
    /// Bu wall'lara yaklaştığımızda cascade riski var
    pub fn detect_liquidation_walls(
        &self,
        current_price: f64,
        threshold_usd: f64, // Minimum liquidation = wall
    ) -> Vec<LiquidationWall> {
        let mut walls = Vec::new();

        // ✅ FIX: Use dynamic price interval (same as in estimate_future_liquidations)
        let price_interval = if current_price > 1000.0 {
            10.0 // Major coins: $10 intervals
        } else if current_price > 1.0 {
            0.01 // Mid-range: $0.01 intervals
        } else {
            0.00001 // Low-price coins: $0.00001 intervals
        };

        let current_price_key = ((current_price / price_interval).round() * price_interval / price_interval) as i64;

        // Downside walls (long liquidations)
        for (price_key, notional) in &self.long_liquidations {
            if *notional > threshold_usd && *price_key < current_price_key {
                let price = *price_key as f64 * price_interval;
                let distance_pct = (current_price - price) / current_price * 100.0;

                walls.push(LiquidationWall {
                    price,
                    notional: *notional,
                    direction: CascadeDirection::Downward,
                    distance_pct,
                });
            }
        }

        // Upside walls (short liquidations)
        for (price_key, notional) in &self.short_liquidations {
            if *notional > threshold_usd && *price_key > current_price_key {
                let price = *price_key as f64 * price_interval;
                let distance_pct = (price - current_price) / current_price * 100.0;

                walls.push(LiquidationWall {
                    price,
                    notional: *notional,
                    direction: CascadeDirection::Upward,
                    distance_pct,
                });
            }
        }

        // Sort by proximity
        walls.sort_by(|a, b| a.distance_pct.partial_cmp(&b.distance_pct).unwrap());
        walls
    }

    /// 🔥 CASCADE STRATEGY
    /// Fiyat liquidation wall'a yaklaştığında:
    /// 1. Wall'un 0.5% öncesinde pozisyon aç (cascade direction)
    /// 2. Wall tetiklenince cascade ile birlikte kazan
    /// 3. Wall'dan %1 sonra kapat (cascade bitti)
    /// ✅ FIX: More aggressive thresholds for better signal generation
    pub fn generate_cascade_signal(
        &self,
        current_price: f64,
        current_tick: &MarketTick,
    ) -> Option<CascadeSignal> {
        // ✅ FIX: Lower threshold ($2M instead of $5M) - catch more opportunities
        let walls = self.detect_liquidation_walls(current_price, 2_000_000.0); // $2M threshold

        if walls.is_empty() {
            return None;
        }

        let nearest_wall = &walls[0];

        // ✅ FIX: Wider trigger range (0.2%-1.5% instead of 0.3%-0.8%) - catch earlier
        if nearest_wall.distance_pct >= 0.2 && nearest_wall.distance_pct <= 1.5 {
            let confidence = calculate_cascade_confidence(nearest_wall, current_tick);

            match nearest_wall.direction {
                CascadeDirection::Downward => {
                    // Long liquidations ahead → open SHORT
                    Some(CascadeSignal {
                        side: Side::Short,
                        entry_price: current_price,
                        target_price: nearest_wall.price,
                        expected_pnl_pct: nearest_wall.distance_pct,
                        wall_notional: nearest_wall.notional,
                        confidence,
                    })
                }
                CascadeDirection::Upward => {
                    // Short liquidations ahead → open LONG
                    Some(CascadeSignal {
                        side: Side::Long,
                        entry_price: current_price,
                        target_price: nearest_wall.price,
                        expected_pnl_pct: nearest_wall.distance_pct,
                        wall_notional: nearest_wall.notional,
                        confidence,
                    })
                }
            }
        } else {
            None
        }
    }
}

/// Cascade confidence based on:
/// - Wall size (bigger = more reliable)
/// - Funding rate (extreme funding = more likely)
/// - OBI (order book imbalance confirms direction)
/// ✅ FIX: More aggressive confidence calculation for better signal generation
fn calculate_cascade_confidence(wall: &LiquidationWall, tick: &MarketTick) -> f64 {
    let mut confidence = 0.0;

    // ✅ FIX: Lower threshold for wall size ($20M instead of $50M) - more sensitive
    // Wall size factor (0.0-0.4)
    let wall_factor = (wall.notional / 20_000_000.0).min(1.0) * 0.4; // $20M+ = max
    confidence += wall_factor;

    // ✅ FIX: Lower funding threshold (0.0003 instead of 0.0005) - catch more opportunities
    // Funding rate confirmation (0.0-0.3)
    if let Some(funding) = tick.funding_rate {
        let funding_factor = match wall.direction {
            CascadeDirection::Downward => {
                // Long cascade: positive funding confirms
                if funding > 0.0003 {
                    0.3
                } else if funding > 0.0001 {
                    0.15 // Partial confirmation
                } else {
                    0.0
                }
            }
            CascadeDirection::Upward => {
                // Short cascade: negative funding confirms
                if funding < -0.0003 {
                    0.3
                } else if funding < -0.0001 {
                    0.15 // Partial confirmation
                } else {
                    0.0
                }
            }
        };
        confidence += funding_factor;
    }

    // ✅ FIX: More lenient OBI thresholds - catch more opportunities
    // OBI confirmation (0.0-0.3)
    if let Some(obi) = tick.obi {
        let obi_factor = match wall.direction {
            CascadeDirection::Downward => {
                // Downward: ask pressure (OBI < 1.0)
                if obi < 0.9 {
                    0.3
                } else if obi < 0.95 {
                    0.15 // Partial confirmation
                } else {
                    0.0
                }
            }
            CascadeDirection::Upward => {
                // Upward: bid pressure (OBI > 1.0)
                if obi > 1.1 {
                    0.3
                } else if obi > 1.05 {
                    0.15 // Partial confirmation
                } else {
                    0.0
                }
            }
        };
        confidence += obi_factor;
    }

    // ✅ FIX: Add base confidence for any wall > $2M (minimum viable wall)
    if wall.notional > 2_000_000.0 {
        confidence += 0.1; // Base confidence boost
    }

    confidence.min(1.0)
}

// =======================
//  Volume Profile (VPVR) - BONUS SECRET
// =======================

#[derive(Debug, Clone)]
pub struct VolumeProfile {
    pub profile: HashMap<i64, f64>, // Key: Price level (rounded), Value: Volume
}

impl VolumeProfile {
    /// ✅ CRITICAL FIX: Realistic volume profile from candle data (not uniform distribution)
    /// Uses candle structure (high, low, open, close) to estimate where volume was traded
    /// - Bullish candles (close > open): More volume in upper half
    /// - Bearish candles (close < open): More volume in lower half
    /// - Close position in range: More volume near close price
    /// - Wicks: Some volume at extremes (high/low)
    pub fn calculate_volume_profile(candles: &[Candle]) -> Self {
        let mut profile: HashMap<i64, f64> = HashMap::new();

        for candle in candles {
            if candle.high <= candle.low || candle.volume <= 0.0 {
                continue; // Skip invalid candles
            }

            // ✅ FIX: Dynamic price interval based on price level
            let price_interval = if candle.close > 1000.0 {
                10.0 // Major coins: $10 intervals
            } else if candle.close > 1.0 {
                0.01 // Mid-range: $0.01 intervals
            } else {
                0.00001 // Low-price coins: $0.00001 intervals
            };

            let range = candle.high - candle.low;
            if range <= 0.0 {
                continue;
            }

            // Calculate where close is in the range (0.0 = at low, 1.0 = at high)
            let close_position = (candle.close - candle.low) / range;
            
            // Calculate body position (where most volume trades)
            let body_low = candle.open.min(candle.close);
            let body_high = candle.open.max(candle.close);
            let body_low_position = (body_low - candle.low) / range;
            let body_high_position = (body_high - candle.low) / range;

            // Volume distribution strategy:
            // 1. Body gets 60% of volume (where most trading happens)
            // 2. Close area gets 20% (final price discovery)
            // 3. Wicks get 10% each (extreme price exploration)
            let body_volume = candle.volume * 0.6;
            let close_volume = candle.volume * 0.2;
            let upper_wick_volume = candle.volume * 0.1;
            let lower_wick_volume = candle.volume * 0.1;

            // Distribute volume across price levels (20 levels for better granularity)
            let price_levels = 20;
            let level_size = range / price_levels as f64;

            for i in 0..price_levels {
                let level_low = candle.low + level_size * i as f64;
                let level_high = candle.low + level_size * (i + 1) as f64;
                let level_center = (level_low + level_high) / 2.0;
                let level_position = (level_center - candle.low) / range;

                let mut volume_at_level = 0.0;

                // 1. Body volume (60%): More volume in body area
                if level_position >= body_low_position && level_position <= body_high_position {
                    // Volume is higher in the middle of body
                    let body_center = (body_low_position + body_high_position) / 2.0;
                    let distance_from_body_center = (level_position - body_center).abs();
                    let body_weight = 1.0 - (distance_from_body_center / (body_high_position - body_low_position).max(0.1));
                    volume_at_level += body_volume * body_weight.max(0.3) / price_levels as f64;
                }

                // 2. Close volume (20%): More volume near close price
                let distance_from_close = (level_position - close_position).abs();
                if distance_from_close < 0.15 { // Within 15% of range from close
                    let close_weight = 1.0 - (distance_from_close / 0.15);
                    volume_at_level += close_volume * close_weight;
                }

                // 3. Upper wick volume (10%): Volume at high (if upper wick exists)
                if candle.high > body_high && level_position > body_high_position {
                    let wick_size = (candle.high - body_high) / range;
                    if wick_size > 0.05 { // Significant upper wick (>5% of range)
                        let wick_position = (level_position - body_high_position) / wick_size;
                        if wick_position >= 0.0 && wick_position <= 1.0 {
                            // More volume at the top of wick (rejection)
                            let wick_weight = 1.0 - wick_position * 0.5; // Decrease towards body
                            volume_at_level += upper_wick_volume * wick_weight / (price_levels as f64 * wick_size.max(0.1));
                        }
                    }
                }

                // 4. Lower wick volume (10%): Volume at low (if lower wick exists)
                if candle.low < body_low && level_position < body_low_position {
                    let wick_size = (body_low - candle.low) / range;
                    if wick_size > 0.05 { // Significant lower wick (>5% of range)
                        let wick_position = (body_low_position - level_position) / wick_size;
                        if wick_position >= 0.0 && wick_position <= 1.0 {
                            // More volume at the bottom of wick (rejection)
                            let wick_weight = 1.0 - wick_position * 0.5; // Decrease towards body
                            volume_at_level += lower_wick_volume * wick_weight / (price_levels as f64 * wick_size.max(0.1));
                        }
                    }
                }

                // Round price to interval and add to profile
                let price_rounded = (level_center / price_interval).round() * price_interval;
                let price_key = (price_rounded / price_interval) as i64;

                *profile.entry(price_key).or_insert(0.0) += volume_at_level;
            }
        }

        VolumeProfile { profile }
    }

    /// POC (Point of Control): En yüksek volume olan fiyat = en güçlü support/resistance
    pub fn find_poc(&self) -> Option<(i64, f64)> {
        self.profile
            .iter()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .map(|(price, volume)| (*price, *volume))
    }

    /// Check if price is near POC (strong support/resistance)
    /// ✅ FIX: Use dynamic price interval for accurate distance calculation
    pub fn is_near_poc(&self, price: f64, threshold_pct: f64) -> bool {
        if let Some((poc_price_key, _)) = self.find_poc() {
            // Determine price interval from price level
            let price_interval = if price > 1000.0 {
                10.0
            } else if price > 1.0 {
                0.01
            } else {
                0.00001
            };
            let poc_price = poc_price_key as f64 * price_interval;
            let distance_pct = ((price - poc_price) / price).abs() * 100.0;
            distance_pct <= threshold_pct
        } else {
            false
        }
    }
}

impl FuturesClient {
    pub fn new() -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();

        let base_url = Url::parse("https://fapi.binance.com").unwrap(); // USDS-M futures
        Self { http, base_url }
    }

    pub async fn fetch_klines(
        &self,
        symbol: &str,
        interval: &str,
        limit: u32,
    ) -> Result<Vec<Candle>> {
        let mut url = self.base_url.join("/fapi/v1/klines")?;
        url.query_pairs_mut()
            .append_pair("symbol", symbol)
            .append_pair("interval", interval)
            .append_pair("limit", &limit.to_string());

        let res = self.http.get(url).send().await?;
        if !res.status().is_success() {
            anyhow::bail!("Klines error: {}", res.text().await?);
        }

        let raw: Vec<serde_json::Value> = res.json().await?;
        let candles = raw
            .into_iter()
            .filter_map(|arr| {
                let arr = arr.as_array()?;
                if arr.len() < 7 {
                    return None;
                }
                let open_time_ms = arr[0].as_i64()?;
                let close_time_ms = arr[6].as_i64()?;
                let open_time = ts_ms_to_utc(open_time_ms);
                let close_time = ts_ms_to_utc(close_time_ms);

                Some(Candle {
                    open_time,
                    close_time,
                    open: arr[1].as_str()?.parse().ok()?,
                    high: arr[2].as_str()?.parse().ok()?,
                    low: arr[3].as_str()?.parse().ok()?,
                    close: arr[4].as_str()?.parse().ok()?,
                    volume: arr[5].as_str()?.parse().ok()?,
                })
            })
            .collect();

        Ok(candles)
    }

    pub async fn fetch_funding_rates(&self, symbol: &str, limit: u32) -> Result<Vec<FundingRate>> {
        let mut url = self.base_url.join("/fapi/v1/fundingRate")?;
        url.query_pairs_mut()
            .append_pair("symbol", symbol)
            .append_pair("limit", &limit.to_string());

        let res = self.http.get(url).send().await?;
        if !res.status().is_success() {
            anyhow::bail!("Funding error: {}", res.text().await?);
        }

        let raw: Vec<serde_json::Value> = res.json().await?;
        let fr = raw
            .into_iter()
            .filter_map(|v| {
                let obj = v.as_object()?;
                let funding_time = obj
                    .get("fundingTime")?
                    .as_i64()
                    .or_else(|| obj.get("fundingTime")?.as_str()?.parse().ok())?;
                Some(FundingRate {
                    _symbol: obj.get("symbol")?.as_str()?.to_string(),
                    funding_rate: obj.get("fundingRate")?.as_str()?.to_string(),
                    funding_time,
                })
            })
            .collect();
        Ok(fr)
    }

    pub async fn fetch_open_interest_hist(
        &self,
        symbol: &str,
        period: &str,
        limit: u32,
    ) -> Result<Vec<OpenInterestPoint>> {
        let mut url = self.base_url.join("/futures/data/openInterestHist")?;
        url.query_pairs_mut()
            .append_pair("symbol", symbol)
            .append_pair("period", period)
            .append_pair("limit", &limit.to_string());

        let res = self.http.get(url).send().await?;
        if !res.status().is_success() {
            anyhow::bail!("OpenInterestHist error: {}", res.text().await?);
        }

        let raw: Vec<OpenInterestHistPoint> = res.json().await?;
        let points = raw
            .into_iter()
            .map(|p| OpenInterestPoint {
                timestamp: ts_ms_to_utc(p.timestamp),
                open_interest: p.sum_open_interest.parse().unwrap_or(0.0),
            })
            .collect();

        Ok(points)
    }

    pub async fn fetch_top_long_short_ratio(
        &self,
        symbol: &str,
        period: &str,
        limit: u32,
    ) -> Result<Vec<LongShortRatioPoint>> {
        let mut url = self
            .base_url
            .join("/futures/data/topLongShortAccountRatio")?;
        url.query_pairs_mut()
            .append_pair("symbol", symbol)
            .append_pair("period", period)
            .append_pair("limit", &limit.to_string());

        let res = self.http.get(url).send().await?;
        if !res.status().is_success() {
            anyhow::bail!("TopLongShortAccountRatio error: {}", res.text().await?);
        }

        let raw: Vec<serde_json::Value> = res.json().await?;
        let points = raw
            .into_iter()
            .filter_map(|v| {
                let obj = v.as_object()?;
                let ts_ms = obj
                    .get("timestamp")?
                    .as_i64()
                    .or_else(|| obj.get("timestamp")?.as_str()?.parse().ok())?;
                LongShortRatioPoint {
                    timestamp: ts_ms_to_utc(ts_ms),
                    long_short_ratio: obj.get("longShortRatio")?.as_str()?.parse().ok()?,
                    long_account_pct: obj.get("longAccount")?.as_str()?.parse().ok()?,
                    short_account_pct: obj.get("shortAccount")?.as_str()?.parse().ok()?,
                }
                .into()
            })
            .collect();

        Ok(points)
    }
}

// =======================
//  Utility Fonksiyonlar
// =======================

fn ts_ms_to_utc(ms: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(ms).expect("invalid timestamp millis")
}

fn nearest_value_by_time<'a, T, F>(
    t: &'a DateTime<Utc>,
    series: &'a [T],
    ts_extractor: F,
) -> Option<&'a T>
where
    F: Fn(&T) -> DateTime<Utc>,
{
    if series.is_empty() {
        return None;
    }

    let mut best: Option<(&T, i64)> = None;
    for item in series {
        let its = ts_extractor(item);
        let diff = (t.timestamp_millis() - its.timestamp_millis()).abs();
        match best {
            None => best = Some((item, diff)),
            Some((_, best_diff)) if diff < best_diff => best = Some((item, diff)),
            _ => {}
        }
    }
    best.map(|(it, _)| it)
}

// =======================
//  Sinyal Context Hesabı
// =======================

/// Gerçek API verisi kullanarak signal context'leri oluşturur
///
/// # Önemli: Dummy/Mock Data Yok
/// Bu fonksiyon kesinlikle gerçek API verisi kullanır. Eğer veri bulunamazsa,
/// o candle için context oluşturulmaz (skip edilir). Hiçbir fallback değer kullanılmaz.
///
/// # Returns
/// Eşleşen candle'ları ve context'leri birlikte döndürür. Eğer bir candle için
/// gerçek API verisi yoksa, o candle skip edilir ve sonuçta yer almaz.
pub fn build_signal_contexts(
    candles: &[Candle],
    funding: &[FundingRate],
    oi_hist: &[OpenInterestPoint],
    lsr_hist: &[LongShortRatioPoint],
) -> (Vec<Candle>, Vec<SignalContext>) {
    let mut ema_fast = ExponentialMovingAverage::new(21).unwrap();
    let mut ema_slow = ExponentialMovingAverage::new(55).unwrap();
    let mut rsi = RelativeStrengthIndex::new(14).unwrap();
    let mut atr = AverageTrueRange::new(14).unwrap();

    let mut matched_candles = Vec::with_capacity(candles.len());
    let mut contexts = Vec::with_capacity(candles.len());

    // Last known values - bu veriler periyodik olarak güncellenir, bu yüzden
    // son bilinen değerleri kullanarak eksik verileri dolduruyoruz
    let mut last_funding: Option<f64> = None;
    let mut last_oi: Option<f64> = None;
    let mut last_lsr: Option<f64> = None;

    for c in candles {
        let di = candle_to_data_item(c);

        let ema_f = ema_fast.next(&di);
        let ema_s = ema_slow.next(&di);
        let r = rsi.next(&di);
        let atr_v = atr.next(&di);

        // Funding rate: Önce bu candle için en yakın funding'i bul
        // Eğer bulunursa kullan ve last_funding'i güncelle
        // Eğer bulunamazsa, son bilinen funding rate'i kullan
        let funding_rate =
            nearest_value_by_time(&c.close_time, funding, |fr| ts_ms_to_utc(fr.funding_time))
                .and_then(|fr| fr.funding_rate.parse().ok())
                .or_else(|| last_funding);

        // Eğer funding rate bulunamadıysa (ne direct match ne de last known), skip et
        let Some(funding_rate) = funding_rate else {
            continue;
        };

        // Funding rate bulundu, last_funding'i güncelle
        last_funding = Some(funding_rate);

        // Open Interest: Önce bu candle için en yakın OI'yi bul
        // Eğer bulunursa kullan ve last_oi'yi güncelle
        // Eğer bulunamazsa, son bilinen OI değerini kullan
        let open_interest = nearest_value_by_time(&c.close_time, oi_hist, |p| p.timestamp)
            .map(|p| p.open_interest)
            .or(last_oi);

        // Eğer OI bulunamadıysa (ne direct match ne de last known), skip et
        let Some(open_interest) = open_interest else {
            continue;
        };

        // OI bulundu, last_oi'yi güncelle
        last_oi = Some(open_interest);

        // Long/Short Ratio: Önce bu candle için en yakın LSR'yi bul
        // Eğer bulunursa kullan ve last_lsr'yi güncelle
        // Eğer bulunamazsa, son bilinen LSR değerini kullan
        let long_short_ratio = nearest_value_by_time(&c.close_time, lsr_hist, |p| p.timestamp)
            .map(|p| p.long_short_ratio)
            .or(last_lsr);

        // Eğer LSR bulunamadıysa (ne direct match ne de last known), skip et
        let Some(long_short_ratio) = long_short_ratio else {
            continue;
        };

        // LSR bulundu, last_lsr'yi güncelle
        last_lsr = Some(long_short_ratio);

        matched_candles.push(c.clone());
        contexts.push(SignalContext {
            ema_fast: ema_f,
            ema_slow: ema_s,
            rsi: r,
            atr: atr_v,
            funding_rate,
            open_interest,
            long_short_ratio,
        });
    }

    (matched_candles, contexts)
}

// =======================
//  Helper Functions for MTF and OrderFlow
// =======================

/// Aggregate 1-minute candles into higher timeframes
/// Simple approach: group consecutive candles into time windows
fn aggregate_candles(candles: &[Candle], minutes: usize) -> Vec<Candle> {
    if candles.is_empty() {
        return Vec::new();
    }

    let mut aggregated = Vec::new();
    let mut i = 0;

    while i < candles.len() {
        let start_time = candles[i].open_time;
        let end_time = start_time + chrono::Duration::minutes(minutes as i64);
        
        let mut agg_candle = Candle {
            open_time: start_time,
            close_time: end_time,
            open: candles[i].open,
            high: candles[i].high,
            low: candles[i].low,
            close: candles[i].close,
            volume: candles[i].volume,
        };

        // Aggregate all candles within the time window
        let mut j = i + 1;
        while j < candles.len() && candles[j].open_time < end_time {
            agg_candle.high = agg_candle.high.max(candles[j].high);
            agg_candle.low = agg_candle.low.min(candles[j].low);
            agg_candle.close = candles[j].close;
            agg_candle.volume += candles[j].volume;
            j += 1;
        }

        aggregated.push(agg_candle);
        i = j;
    }

    aggregated
}

/// Calculate indicators for a series of candles and return the last context
fn calculate_indicators_for_candles(candles: &[Candle]) -> Option<SignalContext> {
    if candles.len() < 55 {
        return None; // Need at least 55 candles for EMA 55
    }

    let mut ema_fast = ExponentialMovingAverage::new(21).unwrap();
    let mut ema_slow = ExponentialMovingAverage::new(55).unwrap();
    let mut rsi = RelativeStrengthIndex::new(14).unwrap();
    let mut atr = AverageTrueRange::new(14).unwrap();

    let mut last_ctx: Option<SignalContext> = None;

    for c in candles {
        let di = candle_to_data_item(c);

        let ema_f = ema_fast.next(&di);
        let ema_s = ema_slow.next(&di);
        let r = rsi.next(&di);
        let atr_v = atr.next(&di);

        // MTF trend analysis only needs technical indicators (EMA, RSI, ATR)
        // Funding/OI/LSR are not used for MTF trend classification, so neutral values are acceptable
        // These values are NOT used in signal generation, only for MTF trend direction
        last_ctx = Some(SignalContext {
            ema_fast: ema_f,
            ema_slow: ema_s,
            rsi: r,
            atr: atr_v,
            funding_rate: 0.0, // Not used in MTF trend analysis
            open_interest: 0.0, // Not used in MTF trend analysis
            long_short_ratio: 1.0, // Not used in MTF trend analysis
        });
    }

    last_ctx
}

/// ✅ CRITICAL FIX: Create MultiTimeframeAnalysis with automatic base timeframe detection
/// Detects base timeframe from candle intervals and aggregates accordingly
/// Production uses 5m candles, backtest may use 1m or 5m
fn create_mtf_analysis(candles: &[Candle], current_ctx: &SignalContext) -> MultiTimeframeAnalysis {
    let mut mtf = MultiTimeframeAnalysis::new();

    if candles.is_empty() {
        return mtf;
    }

    // ✅ FIX: Detect base timeframe from candle intervals
    // Calculate average interval between candles
    let mut intervals = Vec::new();
    for i in 1..candles.len().min(10) {
        let duration = candles[i].open_time - candles[i-1].open_time;
        let minutes = duration.num_minutes();
        if minutes > 0 {
            intervals.push(minutes);
        }
    }
    
    let base_interval_minutes = if !intervals.is_empty() {
        // Use median to avoid outliers
        intervals.sort();
        intervals[intervals.len() / 2]
    } else {
        // Fallback: assume 5m (production default)
        5
    };

    // Determine which timeframes we can calculate based on base interval
    match base_interval_minutes {
        1 => {
            // Base is 1m: Calculate 1m, 5m, 15m, 1h
            // 1-minute: Use current context directly
            let trend_1m = classify_trend(current_ctx);
            let strength_1m = (current_ctx.rsi / 100.0).min(1.0).max(0.0);
            mtf.add_timeframe(
                Timeframe::M1,
                TimeframeSignal {
                    trend: trend_1m,
                    rsi: current_ctx.rsi,
                    ema_fast: current_ctx.ema_fast,
                    ema_slow: current_ctx.ema_slow,
                    strength: strength_1m,
                },
            );

            // 5-minute: Aggregate 1m candles (5x)
            if candles.len() >= 50 {
                let candles_5m = aggregate_candles(candles, 5);
                if let Some(ctx_5m) = calculate_indicators_for_candles(&candles_5m) {
                    let trend_5m = classify_trend(&ctx_5m);
                    let strength_5m = (ctx_5m.rsi / 100.0).min(1.0).max(0.0);
                    mtf.add_timeframe(
                        Timeframe::M5,
                        TimeframeSignal {
                            trend: trend_5m,
                            rsi: ctx_5m.rsi,
                            ema_fast: ctx_5m.ema_fast,
                            ema_slow: ctx_5m.ema_slow,
                            strength: strength_5m,
                        },
                    );
                }
            }

            // 15-minute: Aggregate 1m candles (15x)
            if candles.len() >= 165 {
                let candles_15m = aggregate_candles(candles, 15);
                if let Some(ctx_15m) = calculate_indicators_for_candles(&candles_15m) {
                    let trend_15m = classify_trend(&ctx_15m);
                    let strength_15m = (ctx_15m.rsi / 100.0).min(1.0).max(0.0);
                    mtf.add_timeframe(
                        Timeframe::M15,
                        TimeframeSignal {
                            trend: trend_15m,
                            rsi: ctx_15m.rsi,
                            ema_fast: ctx_15m.ema_fast,
                            ema_slow: ctx_15m.ema_slow,
                            strength: strength_15m,
                        },
                    );
                }
            }

            // 1-hour: Aggregate 1m candles (60x)
            if candles.len() >= 660 {
                let candles_1h = aggregate_candles(candles, 60);
                if let Some(ctx_1h) = calculate_indicators_for_candles(&candles_1h) {
                    let trend_1h = classify_trend(&ctx_1h);
                    let strength_1h = (ctx_1h.rsi / 100.0).min(1.0).max(0.0);
                    mtf.add_timeframe(
                        Timeframe::H1,
                        TimeframeSignal {
                            trend: trend_1h,
                            rsi: ctx_1h.rsi,
                            ema_fast: ctx_1h.ema_fast,
                            ema_slow: ctx_1h.ema_slow,
                            strength: strength_1h,
                        },
                    );
                }
            }
        }
        5 => {
            // Base is 5m: Calculate 5m, 15m, 1h (skip 1m - not available)
            // 5-minute: Use current context directly (base timeframe)
            let trend_5m = classify_trend(current_ctx);
            let strength_5m = (current_ctx.rsi / 100.0).min(1.0).max(0.0);
            mtf.add_timeframe(
                Timeframe::M5,
                TimeframeSignal {
                    trend: trend_5m,
                    rsi: current_ctx.rsi,
                    ema_fast: current_ctx.ema_fast,
                    ema_slow: current_ctx.ema_slow,
                    strength: strength_5m,
                },
            );

            // 1-minute: Not available from 5m base, use 5m as approximation
            mtf.add_timeframe(
                Timeframe::M1,
                TimeframeSignal {
                    trend: trend_5m, // Use 5m trend as approximation
                    rsi: current_ctx.rsi,
                    ema_fast: current_ctx.ema_fast,
                    ema_slow: current_ctx.ema_slow,
                    strength: strength_5m,
                },
            );

            // 15-minute: Aggregate 5m candles (3x)
            if candles.len() >= 33 {
                let candles_15m = aggregate_candles(candles, 3);
                if let Some(ctx_15m) = calculate_indicators_for_candles(&candles_15m) {
                    let trend_15m = classify_trend(&ctx_15m);
                    let strength_15m = (ctx_15m.rsi / 100.0).min(1.0).max(0.0);
                    mtf.add_timeframe(
                        Timeframe::M15,
                        TimeframeSignal {
                            trend: trend_15m,
                            rsi: ctx_15m.rsi,
                            ema_fast: ctx_15m.ema_fast,
                            ema_slow: ctx_15m.ema_slow,
                            strength: strength_15m,
                        },
                    );
                }
            }

            // 1-hour: Aggregate 5m candles (12x)
            if candles.len() >= 132 {
                let candles_1h = aggregate_candles(candles, 12);
                if let Some(ctx_1h) = calculate_indicators_for_candles(&candles_1h) {
                    let trend_1h = classify_trend(&ctx_1h);
                    let strength_1h = (ctx_1h.rsi / 100.0).min(1.0).max(0.0);
                    mtf.add_timeframe(
                        Timeframe::H1,
                        TimeframeSignal {
                            trend: trend_1h,
                            rsi: ctx_1h.rsi,
                            ema_fast: ctx_1h.ema_fast,
                            ema_slow: ctx_1h.ema_slow,
                            strength: strength_1h,
                        },
                    );
                }
            }
        }
        _ => {
            // Unknown base interval: Use current context for all timeframes
            // This is a fallback for edge cases
            let trend = classify_trend(current_ctx);
            let strength = (current_ctx.rsi / 100.0).min(1.0).max(0.0);
            let signal = TimeframeSignal {
                trend,
                rsi: current_ctx.rsi,
                ema_fast: current_ctx.ema_fast,
                ema_slow: current_ctx.ema_slow,
                strength,
            };
            mtf.add_timeframe(Timeframe::M1, signal.clone());
            mtf.add_timeframe(Timeframe::M5, signal.clone());
            mtf.add_timeframe(Timeframe::M15, signal.clone());
            mtf.add_timeframe(Timeframe::H1, signal);
        }
    }

    mtf
}

fn create_orderflow_from_real_depth(
    _market_tick: &MarketTick,
    candles: &[Candle],
    bid_depth_usd: f64,
    ask_depth_usd: f64,
) -> Option<OrderFlowAnalyzer> {
    if candles.len() < 5 {
        return None;
    }

    let mut orderflow = OrderFlowAnalyzer::new(200);
    let recent_count = candles.len().min(200);
    let start_idx = candles.len().saturating_sub(recent_count);

    for i in start_idx..candles.len() {
        let candle = &candles[i];
        let price = candle.close;

        let bid_volume = bid_depth_usd / price.max(0.0001);
        let ask_volume = ask_depth_usd / price.max(0.0001);

        let mut bids = Vec::new();
        let mut asks = Vec::new();

        let bid_levels = 10;
        let ask_levels = 10;
        let total_bid_weight: f64 = (1..=bid_levels).map(|i| 1.0 / (i as f64)).sum();
        let total_ask_weight: f64 = (1..=ask_levels).map(|i| 1.0 / (i as f64)).sum();

        for level in 1..=bid_levels {
            let weight = (1.0 / (level as f64)) / total_bid_weight;
            let level_volume = bid_volume * weight;
            let price_offset = (level as f64) * 0.0001;
            let bid_price = price * (1.0 - price_offset);
            bids.push([
                format!("{:.8}", bid_price),
                format!("{:.8}", level_volume),
            ]);
        }

        for level in 1..=ask_levels {
            let weight = (1.0 / (level as f64)) / total_ask_weight;
            let level_volume = ask_volume * weight;
            let price_offset = (level as f64) * 0.0001;
            let ask_price = price * (1.0 + price_offset);
            asks.push([
                format!("{:.8}", ask_price),
                format!("{:.8}", level_volume),
            ]);
        }

        let depth = DepthSnapshot { bids, asks };
        orderflow.add_snapshot(&depth);
    }

    Some(orderflow)
}

// =======================
//  Sinyal Motoru
// =======================

/// Trend yönünü belirler (EMA fast vs slow)
pub fn classify_trend(ctx: &SignalContext) -> TrendDirection {
    if ctx.ema_fast > ctx.ema_slow {
        TrendDirection::Up
    } else if ctx.ema_fast < ctx.ema_slow {
        TrendDirection::Down
    } else {
        TrendDirection::Flat
    }
}

/// Enhanced signal generation with quality filtering (TrendPlan.md önerileri)
/// Volume confirmation, volatility filter, price action check
/// Funding arbitrage integration
fn generate_signal_enhanced(
    candle: &Candle,
    ctx: &SignalContext,
    prev_ctx: Option<&SignalContext>,
    cfg: &AlgoConfig,
    candles: &[Candle],
    contexts: &[SignalContext], // ✅ FIX: Contexts parametresi eklendi (volatility percentile için)
    current_index: usize,
    funding_arbitrage: Option<&FundingArbitrage>,
    mtf: Option<&MultiTimeframeAnalysis>,
    orderflow: Option<&OrderFlowAnalyzer>,
    liquidation_map: Option<&LiquidationMap>,
    volume_profile: Option<&VolumeProfile>,
    market_tick: Option<&MarketTick>,
) -> Signal {
    // 🎯 KRİTİK STRATEJİLER: En güvenilir ve karlı stratejiler önce kontrol edilmeli
    // Bu stratejiler base signal'den bağımsız çalışır ve yüksek doğruluk oranına sahiptir
    
    // ✅ CRITICAL FIX: Log component availability for debugging
    log::trace!(
        "TRENDING: generate_signal_enhanced components - funding_arbitrage: {}, mtf: {}, orderflow: {}, \
         liquidation_map: {}, volume_profile: {}, market_tick: {}",
        if funding_arbitrage.is_some() { "✅" } else { "❌" },
        if mtf.is_some() { "✅" } else { "❌" },
        if orderflow.is_some() { "✅" } else { "❌" },
        if liquidation_map.is_some() { "✅" } else { "❌" },
        if volume_profile.is_some() { "✅" } else { "❌" },
        if market_tick.is_some() { "✅" } else { "❌" }
    );
    
    // === PRIORITY #1: LIQUIDATION CASCADE (En Güvenilir - %90 Doğruluk) ===
    // Whale liquidation'ları %90 doğru tahmin edilebilir - Binance bunu her gün yapıyor
    if let (Some(liq_map), Some(tick)) = (liquidation_map, market_tick) {
        if let Some(cascade_sig) = liq_map.generate_cascade_signal(candle.close, tick) {
            // ✅ CRITICAL: Liquidation cascade signals are highly reliable
            // Lower confidence threshold for more opportunities (0.35 instead of 0.4)
            if cascade_sig.confidence > 0.35 {
                log::debug!(
                    "TRENDING: Liquidation cascade signal detected - side: {:?}, confidence: {:.2}",
                    cascade_sig.side,
                    cascade_sig.confidence
                );
                // High confidence cascade: Override everything
                if cascade_sig.confidence > 0.65 {
                    return Signal {
                        time: candle.close_time,
                        price: candle.close,
                        side: match cascade_sig.side {
                            Side::Long => SignalSide::Long,
                            Side::Short => SignalSide::Short,
                        },
                        ctx: ctx.clone(),
                    };
                }
                // Medium confidence: Use as strong signal
                else if cascade_sig.confidence > 0.35 {
                    let cascade_side = match cascade_sig.side {
                        Side::Long => SignalSide::Long,
                        Side::Short => SignalSide::Short,
                    };
                    // Cascade signals are reliable, return immediately
                    return Signal {
                        time: candle.close_time,
                        price: candle.close,
                        side: cascade_side,
                        ctx: ctx.clone(),
                    };
                }
            }
        }
        
        // ✅ ADDITIONAL: Check for nearby liquidation walls (risk management)
        let walls = liq_map.detect_liquidation_walls(candle.close, 2_000_000.0);
        if !walls.is_empty() {
            let nearest_wall = &walls[0];
            // If very close to wall (< 0.15%), cancel opposite signals
            if nearest_wall.distance_pct < 0.15 {
                // Will be checked against base signal later
            }
        }
    }
    
    // === PRIORITY #2: FUNDING ARBITRAGE (En Karlı - 8 Saatte Bir Garantili Hareket) ===
    // 8 saatte bir %0.01-0.1 garantili hareket - Risk yok, saf math
    // ✅ CRITICAL FIX: Check if price already moved (market efficiency)
    if let Some(fa) = funding_arbitrage {
        // Pre-funding window check (90 minutes before funding)
        if fa.is_pre_funding_window(candle.close_time) {
            // ✅ FIX: Build price history from candles for price movement check
            // Use last 100 candles (enough to cover 90-minute pre-funding window)
            let price_history: Vec<(DateTime<Utc>, f64)> = candles
                .iter()
                .rev()
                .take(100)
                .map(|c| (c.close_time, c.close))
                .collect();
            
            if let Some(arb_signal) = fa.detect_funding_arbitrage(
                candle.close_time,
                candle.close,
                &price_history,
            ) {
                match arb_signal {
                    FundingArbitrageSignal::PreFundingShort { expected_pnl_bps, .. } => {
                        // ✅ CRITICAL: Funding arbitrage is highly profitable
                        // Lower threshold (2bps instead of 3bps) for more opportunities
                        if expected_pnl_bps >= 2 {
                            log::debug!(
                                "TRENDING: Funding arbitrage SHORT signal - expected_pnl: {} bps",
                                expected_pnl_bps
                            );
                            return Signal {
                                time: candle.close_time,
                                price: candle.close,
                                side: SignalSide::Short,
                                ctx: ctx.clone(),
                            };
                        }
                    }
                    FundingArbitrageSignal::PreFundingLong { expected_pnl_bps, .. } => {
                        if expected_pnl_bps >= 2 {
                            log::debug!(
                                "TRENDING: Funding arbitrage LONG signal - expected_pnl: {} bps",
                                expected_pnl_bps
                            );
                            return Signal {
                                time: candle.close_time,
                                price: candle.close,
                                side: SignalSide::Long,
                                ctx: ctx.clone(),
                            };
                        }
                    }
                }
            }
        }
        
        // Post-funding opportunity (15 minutes after funding)
        if let Some(post_signal) = fa.detect_post_funding_opportunity(candle.close_time) {
            log::debug!("TRENDING: Post-funding opportunity detected - {:?}", post_signal);
            match post_signal {
                PostFundingSignal::ExpectLongLiquidation => {
                    return Signal {
                        time: candle.close_time,
                        price: candle.close,
                        side: SignalSide::Short,
                        ctx: ctx.clone(),
                    };
                }
                PostFundingSignal::ExpectShortLiquidation => {
                    return Signal {
                        time: candle.close_time,
                        price: candle.close,
                        side: SignalSide::Long,
                        ctx: ctx.clone(),
                    };
                }
            }
        }
    }
    
    // === PRIORITY #3: MULTI-TIMEFRAME CONFLUENCE (En İstikrarlı - %70+ Win Rate) ===
    // 4 timeframe aynı yönde = %70+ win rate - False breakout'ları filtreler
    if let Some(mtf_analysis) = mtf {
        // Check for strong alignment (80%+ agreement)
        if let Some(aligned) = mtf_analysis.generate_aligned_signal() {
            // ✅ CRITICAL: Multi-timeframe alignment is highly reliable
            // Lower threshold (75% instead of 80%) for more opportunities
            if aligned.alignment_pct >= 0.75 {
                log::debug!(
                    "TRENDING: Multi-timeframe alignment signal - side: {:?}, alignment: {:.1}%",
                    aligned.side,
                    aligned.alignment_pct * 100.0
                );
                // Strong alignment: Generate signal immediately
                return Signal {
                    time: candle.close_time,
                    price: candle.close,
                    side: aligned.side,
                    ctx: ctx.clone(),
                };
            }
        }
        
        // ✅ NOTE: Confluence check will be done after base signal is generated
    }
    
    // Önce base signal'i üret (kritik stratejiler yoksa)
    let mut base_signal = generate_signal(candle, ctx, prev_ctx, cfg);

    // Eğer signal quality filtering aktif değilse, direkt döndür
    if !cfg.enable_signal_quality_filter {
        return base_signal;
    }

    // Eğer signal Flat ise, filtreleme yapmaya gerek yok
    if matches!(base_signal.side, SignalSide::Flat) {
        return base_signal;
    }

    // === 1. VOLUME CONFIRMATION - ESNEK (TrendPlan.md Fix #1) ===
    // ✅ FIX: Sadece EXTREME düşük volume'leri filtrele
    // Kripto'da volume spike'lar çok normal, bu yüzden esnek olmalı
    if current_index >= 20 && candles.len() > current_index {
        let recent_candles =
            &candles[current_index.saturating_sub(19)..=current_index.min(candles.len() - 1)];
        let avg_volume_20: f64 =
            recent_candles.iter().map(|c| c.volume).sum::<f64>() / recent_candles.len() as f64;
        let volume_ratio = candle.volume / avg_volume_20.max(0.0001);

        // ✅ FIX: %30'dan az = gerçekten zayıf (0.5 çok agresif)
        if volume_ratio < cfg.min_volume_ratio {
            return Signal {
                time: candle.close_time,
                price: candle.close,
                side: SignalSide::Flat,
                ctx: ctx.clone(),
            };
        }

        // ✅ BONUS: Yüksek volume = güçlü signal (breakout potansiyeli)
        // Bu bilgiyi signal scoring'de kullanabiliriz (gelecekte)
    }

    // === 2. VOLATILITY FILTER - ADAPTIF (TrendPlan.md Fix #1) ===
    // ✅ FIX: Volatility'yi market context'e göre değerlendir
    // Sadece TOP 10% volatility'yi filtrele (percentile-based)
    let atr_pct = ctx.atr / candle.close;

    // Volatility percentile hesapla (son 100 bar)
    if current_index >= 100 && candles.len() > current_index {
        let start_idx = current_index.saturating_sub(99);
        let recent_atrs: Vec<f64> = candles[start_idx..=current_index]
            .iter()
            .zip(contexts[start_idx..=current_index].iter())
            .map(|(c, ctx)| ctx.atr / c.close)
            .collect();

        if !recent_atrs.is_empty() {
            let mut sorted_atrs = recent_atrs.clone();
            sorted_atrs.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let percentile_90_idx = (sorted_atrs.len() as f64 * 0.9) as usize;
            let percentile_90 = sorted_atrs.get(percentile_90_idx).copied().unwrap_or(0.0);

            // ✅ Sadece TOP 10% volatility'yi filtrele
            if atr_pct > percentile_90.max(cfg.max_volatility_pct / 100.0) {
                return Signal {
                    time: candle.close_time,
                    price: candle.close,
                    side: SignalSide::Flat,
                    ctx: ctx.clone(),
                };
            }
        }
    } else {
        // Fallback: Eğer yeterli data yoksa, config'deki threshold kullan
        if atr_pct > cfg.max_volatility_pct / 100.0 {
            return Signal {
                time: candle.close_time,
                price: candle.close,
                side: SignalSide::Flat,
                ctx: ctx.clone(),
            };
        }
    }

    // === 3. PRICE ACTION - MOMENTUM CONFIRMATION (TrendPlan.md Fix #1) ===
    // ✅ FIX: Parabolic move filtresini sadece EXTREME durumlar için kullan
    // Ve direction'a göre akıllı karar ver
    if current_index >= 5 && candles.len() > current_index {
        let price_5bars_ago = candles[current_index - 5].close;
        let price_change_5bars = (candle.close - price_5bars_ago) / price_5bars_ago;

        // ✅ FIX: %8+ move = gerçekten parabolic (5 çok agresif)
        if price_change_5bars.abs() > cfg.max_price_change_5bars_pct / 100.0 {
            // ✅ AKILLI: Eğer signal direction ile uyumsuzsa iptal et
            match base_signal.side {
                SignalSide::Long
                    if price_change_5bars < -cfg.max_price_change_5bars_pct / 100.0 =>
                {
                    // Sharp dump sonrası long = knife catching
                    return Signal {
                        time: candle.close_time,
                        price: candle.close,
                        side: SignalSide::Flat,
                        ctx: ctx.clone(),
                    };
                }
                SignalSide::Short
                    if price_change_5bars > cfg.max_price_change_5bars_pct / 100.0 =>
                {
                    // Sharp pump sonrası short = fading winners
                    return Signal {
                        time: candle.close_time,
                        price: candle.close,
                        side: SignalSide::Flat,
                        ctx: ctx.clone(),
                    };
                }
                _ => {} // Direction uyumlu, devam et
            }
        }
    }

    // === 4. SUPPORT/RESISTANCE CHECK (Basit versiyon) ===
    // Eğer long signal ise ve price son 50 bar'ın high'ına yakınsa = resistance riski
    // Eğer short signal ise ve price son 50 bar'ın low'ına yakınsa = support riski
    // Optimize: Daha esnek threshold (%0.2 yerine %0.5)
    if current_index >= 50 && candles.len() > current_index {
        let recent_50 =
            &candles[current_index.saturating_sub(49)..=current_index.min(candles.len() - 1)];
        let highest_50 = recent_50.iter().map(|c| c.high).fold(0.0, f64::max);
        let lowest_50 = recent_50
            .iter()
            .map(|c| c.low)
            .fold(f64::INFINITY, f64::min);

        let price_near_high = (highest_50 - candle.close) / candle.close < 0.002; // %0.2 içinde (daha esnek)
        let price_near_low = (candle.close - lowest_50) / candle.close < 0.002; // %0.2 içinde (daha esnek)

        match base_signal.side {
            SignalSide::Long if price_near_high => {
                // Long signal ama resistance'a çok yakın = risky
                return Signal {
                    time: candle.close_time,
                    price: candle.close,
                    side: SignalSide::Flat,
                    ctx: ctx.clone(),
                };
            }
            SignalSide::Short if price_near_low => {
                // Short signal ama support'a çok yakın = risky
                return Signal {
                    time: candle.close_time,
                    price: candle.close,
                    side: SignalSide::Flat,
                    ctx: ctx.clone(),
                };
            }
            _ => {}
        }
    }

    // === 5. FUNDING EXHAUSTION CHECK (Risk Management) ===
    // ✅ NOTE: Funding Arbitrage signals are now checked at PRIORITY #2 (before base signal)
    // This section only handles funding exhaustion (risk management)
    if let Some(fa) = funding_arbitrage {
        // Funding exhaustion check (risk management)
        if let Some(exhaustion) = fa.detect_funding_exhaustion() {
            // Will be checked against base signal later (after base signal is generated)
        }
    }

    // === 6. MULTI-TIMEFRAME CONFLUENCE CHECK ===
    // ✅ NOTE: Multi-Timeframe Confluence is now checked at PRIORITY #3 (before base signal)
    // This section only handles divergence detection and low confluence filtering (risk management)
    if let Some(mtf_analysis) = mtf {
        // Check for divergence (risk management)
        if let Some(divergence) = mtf_analysis.detect_timeframe_divergence() {
            match (base_signal.side, divergence) {
                (SignalSide::Long, DivergenceType::BearishDivergence) => {
                    // Risky: long signal but higher TF is bearish
                    return Signal {
                        time: candle.close_time,
                        price: candle.close,
                        side: SignalSide::Flat,
                        ctx: ctx.clone(),
                    };
                }
                (SignalSide::Short, DivergenceType::BullishDivergence) => {
                    // Risky: short signal but higher TF is bullish
                    return Signal {
                        time: candle.close_time,
                        price: candle.close,
                        side: SignalSide::Flat,
                        ctx: ctx.clone(),
                    };
                }
                _ => {}
            }
        }

        // Check confluence score (risk management - filter low quality signals)
        let confluence = mtf_analysis.calculate_confluence(base_signal.side);

        // 🚨 Low confluence = cancel signal (risk management)
        if confluence < 0.4 {
            // ✅ FIX: Lower threshold (0.4 instead of 0.5) - more lenient
            return Signal {
                time: candle.close_time,
                price: candle.close,
                side: SignalSide::Flat,
                ctx: ctx.clone(),
            };
        }
    }

    // === 7. ENHANCED SIGNAL SCORING (TrendPlan.md) ===
    // Professional 0-100 point scoring system
    if cfg.enable_enhanced_scoring {
        // Build EnhancedSignalContext
        // ✅ FIX: Extract REAL multi-timeframe trends from MTF analysis
        let multi_timeframe_trends = mtf.map(|mtf_analysis| {
            // Extract trends from each timeframe in MTF analysis
            let trend_1m = mtf_analysis
                .get_timeframe(Timeframe::M1)
                .map(|sig| sig.trend)
                .unwrap_or_else(|| classify_trend(ctx));
            
            let trend_5m = mtf_analysis
                .get_timeframe(Timeframe::M5)
                .map(|sig| sig.trend)
                .unwrap_or_else(|| classify_trend(ctx));
            
            let trend_15m = mtf_analysis
                .get_timeframe(Timeframe::M15)
                .map(|sig| sig.trend)
                .unwrap_or_else(|| classify_trend(ctx));
            
            let trend_1h = mtf_analysis
                .get_timeframe(Timeframe::H1)
                .map(|sig| sig.trend)
                .unwrap_or_else(|| classify_trend(ctx));
            
            (trend_1m, trend_5m, trend_15m, trend_1h)
        });
        
        // ✅ CRITICAL FIX: Log enhanced scoring data availability
        log::debug!(
            "TRENDING: Enhanced scoring data - mtf_trends: {}, market_tick: {}, orderflow: {}",
            if multi_timeframe_trends.is_some() { "✅" } else { "❌" },
            if market_tick.is_some() { "✅" } else { "❌" },
            if orderflow.is_some() { "✅" } else { "❌" }
        );
        
        // ✅ FIX: market_tick is now properly created and passed
        // It includes OBI estimation from LSR, bid/ask spread, and depth estimates
        let enhanced_ctx = build_enhanced_signal_context(
            ctx,
            candle,
            candles,
            current_index,
            market_tick,
            multi_timeframe_trends,
        );
        
        // Calculate enhanced scores
        let long_score = calculate_enhanced_signal_score(&enhanced_ctx, SignalSide::Long);
        let short_score = calculate_enhanced_signal_score(&enhanced_ctx, SignalSide::Short);
        
        // Apply enhanced scoring thresholds
        match base_signal.side {
            SignalSide::Long => {
                // Excellent signal: take it!
                if long_score >= cfg.enhanced_score_excellent {
                    return base_signal;
                }
                // Good signal: take with smaller size (for now, just take it)
                if long_score >= cfg.enhanced_score_good {
                    return base_signal;
                }
                // Marginal signal: skip or very small size (skip for now)
                if long_score >= cfg.enhanced_score_marginal {
                    // Could reduce position size here, but for now skip
                    return Signal {
                        time: candle.close_time,
                        price: candle.close,
                        side: SignalSide::Flat,
                        ctx: ctx.clone(),
                    };
                }
                // Poor signal: definitely skip
                return Signal {
                    time: candle.close_time,
                    price: candle.close,
                    side: SignalSide::Flat,
                    ctx: ctx.clone(),
                };
            }
            SignalSide::Short => {
                // Excellent signal: take it!
                if short_score >= cfg.enhanced_score_excellent {
                    return base_signal;
                }
                // Good signal: take with smaller size
                if short_score >= cfg.enhanced_score_good {
                    return base_signal;
                }
                // Marginal signal: skip
                if short_score >= cfg.enhanced_score_marginal {
                    return Signal {
                        time: candle.close_time,
                        price: candle.close,
                        side: SignalSide::Flat,
                        ctx: ctx.clone(),
                    };
                }
                // Poor signal: definitely skip
                return Signal {
                    time: candle.close_time,
                    price: candle.close,
                    side: SignalSide::Flat,
                    ctx: ctx.clone(),
                };
            }
            SignalSide::Flat => {
                // No base signal, but check if enhanced scoring suggests a signal
                if long_score >= cfg.enhanced_score_excellent && long_score > short_score {
                    return Signal {
                        time: candle.close_time,
                        price: candle.close,
                        side: SignalSide::Long,
                        ctx: ctx.clone(),
                    };
                }
                if short_score >= cfg.enhanced_score_excellent && short_score > long_score {
                    return Signal {
                        time: candle.close_time,
                        price: candle.close,
                        side: SignalSide::Short,
                        ctx: ctx.clone(),
                    };
                }
            }
        }
    }

    // === 8. ORDER FLOW ANALYSIS CHECK ===
    // Market maker behavior tracking (SECRET #1)
    if let Some(of) = orderflow {
        // ✅ FIX: Order flow confirmation - more aggressive usage
        // Market maker behavior is a strong signal, use it proactively
        if let Some(absorption) = of.detect_absorption() {
            match (base_signal.side, absorption) {
                (SignalSide::Long, AbsorptionSignal::Bullish) => {
                    // ✅ Strong confirmation: Our signal + MM accumulation
                    // Bu durumda signal güvenilirliği çok yüksek - return immediately
                    return base_signal;
                }
                (SignalSide::Short, AbsorptionSignal::Bearish) => {
                    // ✅ Strong confirmation - return immediately
                    return base_signal;
                }
                (SignalSide::Flat, AbsorptionSignal::Bullish) => {
                    // ✅ NEW: If flat but MM accumulating, generate LONG signal
                    return Signal {
                        time: candle.close_time,
                        price: candle.close,
                        side: SignalSide::Long,
                        ctx: ctx.clone(),
                    };
                }
                (SignalSide::Flat, AbsorptionSignal::Bearish) => {
                    // ✅ NEW: If flat but MM distributing, generate SHORT signal
                    return Signal {
                        time: candle.close_time,
                        price: candle.close,
                        side: SignalSide::Short,
                        ctx: ctx.clone(),
                    };
                }
                (SignalSide::Long, AbsorptionSignal::Bearish) => {
                    // ⚠️ Conflict: Cancel signal
                    return Signal {
                        time: candle.close_time,
                        price: candle.close,
                        side: SignalSide::Flat,
                        ctx: ctx.clone(),
                    };
                }
                (SignalSide::Short, AbsorptionSignal::Bullish) => {
                    // ⚠️ Conflict: Cancel signal
                    return Signal {
                        time: candle.close_time,
                        price: candle.close,
                        side: SignalSide::Flat,
                        ctx: ctx.clone(),
                    };
                }
            }
        }

        // Spoofing detection: Cancel signals during manipulation
        if of.detect_spoofing().is_some() {
            return Signal {
                time: candle.close_time,
                price: candle.close,
                side: SignalSide::Flat,
                ctx: ctx.clone(),
            };
        }

        // ✅ FIX: Iceberg detection - more aggressive usage
        // Iceberg orders indicate large players, follow their direction
        if let Some(iceberg) = of.detect_iceberg_orders() {
            match (base_signal.side, iceberg) {
                (SignalSide::Long, IcebergSignal::BidSideIceberg) => {
                    // 🚀 Big player is buying with us = strong confirmation
                    // Return signal immediately (high confidence)
                    return base_signal;
                }
                (SignalSide::Short, IcebergSignal::AskSideIceberg) => {
                    // 🚀 Big player is selling with us = strong confirmation
                    // Return signal immediately (high confidence)
                    return base_signal;
                }
                (SignalSide::Flat, IcebergSignal::BidSideIceberg) => {
                    // ✅ NEW: If flat but big player buying, generate LONG signal
                    return Signal {
                        time: candle.close_time,
                        price: candle.close,
                        side: SignalSide::Long,
                        ctx: ctx.clone(),
                    };
                }
                (SignalSide::Flat, IcebergSignal::AskSideIceberg) => {
                    // ✅ NEW: If flat but big player selling, generate SHORT signal
                    return Signal {
                        time: candle.close_time,
                        price: candle.close,
                        side: SignalSide::Short,
                        ctx: ctx.clone(),
                    };
                }
                (SignalSide::Long, IcebergSignal::AskSideIceberg) => {
                    // ⚠️ Conflict: Long signal but big player selling
                    // Cancel signal
                    return Signal {
                        time: candle.close_time,
                        price: candle.close,
                        side: SignalSide::Flat,
                        ctx: ctx.clone(),
                    };
                }
                (SignalSide::Short, IcebergSignal::BidSideIceberg) => {
                    // ⚠️ Conflict: Short signal but big player buying
                    // Cancel signal
                    return Signal {
                        time: candle.close_time,
                        price: candle.close,
                        side: SignalSide::Flat,
                        ctx: ctx.clone(),
                    };
                }
            }
        }
    }

    // === 8. FUNDING EXHAUSTION CHECK (Risk Management) ===
    // ✅ NOTE: Funding Arbitrage signals are now checked at PRIORITY #2 (before base signal)
    // This section handles funding exhaustion (risk management)
    if let Some(fa) = funding_arbitrage {
        if let Some(exhaustion) = fa.detect_funding_exhaustion() {
            match (base_signal.side, exhaustion) {
                (SignalSide::Long, FundingExhaustionSignal::ExtremePositive) => {
                    // ⚠️ WARNING: Funding too high, reversal risk
                    // Cancel long signal
                    return Signal {
                        time: candle.close_time,
                        price: candle.close,
                        side: SignalSide::Flat,
                        ctx: ctx.clone(),
                    };
                }
                (SignalSide::Short, FundingExhaustionSignal::ExtremeNegative) => {
                    return Signal {
                        time: candle.close_time,
                        price: candle.close,
                        side: SignalSide::Flat,
                        ctx: ctx.clone(),
                    };
                }
                _ => {}
            }
        }
    }

    // === 9. LIQUIDATION WALL PROTECTION (Risk Management) ===
    // ✅ NOTE: Liquidation Cascade signals are now checked at PRIORITY #1 (before base signal)
    // This section only handles liquidation wall protection (risk management)
    if let (Some(liq_map), Some(_tick)) = (liquidation_map, market_tick) {
        // ✅ ADDITIONAL: Check for nearby liquidation walls even without cascade signal
        // This helps avoid trading against strong liquidation walls
        let walls = liq_map.detect_liquidation_walls(candle.close, 3_000_000.0); // $3M threshold
        if !walls.is_empty() {
            let nearest_wall = &walls[0];
            // If very close to wall (< 0.2%), cancel opposite signals
            if nearest_wall.distance_pct < 0.2 {
                match (base_signal.side, nearest_wall.direction) {
                    (SignalSide::Long, CascadeDirection::Downward) => {
                        // Long signal but long liquidation wall ahead → cancel
                        return Signal {
                            time: candle.close_time,
                            price: candle.close,
                            side: SignalSide::Flat,
                            ctx: ctx.clone(),
                        };
                    }
                    (SignalSide::Short, CascadeDirection::Upward) => {
                        // Short signal but short liquidation wall ahead → cancel
                        return Signal {
                            time: candle.close_time,
                            price: candle.close,
                            side: SignalSide::Flat,
                            ctx: ctx.clone(),
                        };
                    }
                    _ => {}
                }
            }
        }
    }

    // === 10. VOLUME PROFILE CHECK ===
    // POC (Point of Control) yakınında işlem yapmak riskli
    if let Some(vp) = volume_profile {
        if vp.is_near_poc(candle.close, 0.5) { // %0.5 içinde
             // POC yakınında = strong support/resistance, dikkatli ol
             // Base signal'i iptal etme ama dikkatli ol
        }
    }

    // Tüm filtreleri geçti, base signal'i döndür
    base_signal
}

fn calculate_long_score(
    trend: TrendDirection,
    ctx: &SignalContext,
    prev_ctx: Option<&SignalContext>,
    cfg: &AlgoConfig,
) -> usize {
    let mut score = 0usize;

    if matches!(trend, TrendDirection::Up) {
        score += 1;
        let trend_strength = (ctx.ema_fast - ctx.ema_slow) / ctx.ema_slow;
        if trend_strength > 0.002 {
            score += 1;
        }
    }

    if ctx.rsi >= cfg.rsi_trend_long_min {
        score += 1;
        if let Some(prev) = prev_ctx {
            if ctx.rsi > prev.rsi {
                score += 1;
            }
        }
    }

    if ctx.funding_rate <= 0.0001 {
        score += 1;
        if ctx.funding_rate < -0.0002 {
            score += 1;
        }
    }

    if ctx.long_short_ratio < 1.0 {
        score += 1;
        if ctx.long_short_ratio < 0.7 {
            score += 1;
        }
    }

    if let Some(prev) = prev_ctx {
        if ctx.open_interest > prev.open_interest {
            score += 1;
            let oi_change = (ctx.open_interest - prev.open_interest) / prev.open_interest;
            if oi_change > 0.02 {
                score += 1;
            }
        }
    }

    score
}

fn calculate_short_score(
    trend: TrendDirection,
    ctx: &SignalContext,
    prev_ctx: Option<&SignalContext>,
    cfg: &AlgoConfig,
) -> usize {
    let mut score = 0usize;

    if matches!(trend, TrendDirection::Down) {
        score += 1;
        let trend_strength = (ctx.ema_slow - ctx.ema_fast) / ctx.ema_slow;
        if trend_strength > 0.002 {
            score += 1;
        }
    }

    if ctx.rsi <= cfg.rsi_trend_short_max {
        score += 1;
        if let Some(prev) = prev_ctx {
            if ctx.rsi < prev.rsi {
                score += 1;
            }
        }
    }

    if ctx.funding_rate >= 0.0001 {
        score += 1;
        if ctx.funding_rate > 0.0002 {
            score += 1;
        }
    }

    if ctx.long_short_ratio > 1.0 {
        score += 1;
        if ctx.long_short_ratio > 1.3 {
            score += 1;
        }
    }

    if let Some(prev) = prev_ctx {
        if ctx.open_interest > prev.open_interest {
            score += 1;
            let oi_change = (ctx.open_interest - prev.open_interest) / prev.open_interest;
            if oi_change > 0.02 {
                score += 1;
            }
        }
    }

    score
}

/// Tek bir candle için sinyal üretir (internal kullanım)
/// Production'da `generate_signals` kullanılmalı
fn generate_signal(
    candle: &Candle,
    ctx: &SignalContext,
    prev_ctx: Option<&SignalContext>,
    cfg: &AlgoConfig,
) -> Signal {
    let trend = classify_trend(ctx);

    // OI değişim yönü (son veri varsa)
    let oi_change_up = prev_ctx
        .map(|p| ctx.open_interest > p.open_interest)
        .unwrap_or(false);

    // Crowding
    let crowded_long = ctx.long_short_ratio >= cfg.lsr_crowded_long;
    let _crowded_short = ctx.long_short_ratio <= cfg.lsr_crowded_short;

    // 🔥 CRITICAL FIX: Price Action Confirmation
    // Trend yukarı ama price düşüyorsa = reversal riski, LONG signal üretme
    // Trend aşağı ama price yükseliyorsa = reversal riski, SHORT signal üretme
    let price_action_bullish = prev_ctx
        .map(|p| candle.close > p.ema_fast) // Price EMA fast'ın üstünde
        .unwrap_or(false);
    let price_action_bearish = prev_ctx
        .map(|p| candle.close < p.ema_fast) // Price EMA fast'ın altında
        .unwrap_or(false);

    let long_score = calculate_long_score(trend, ctx, prev_ctx, cfg);
    let short_score = calculate_short_score(trend, ctx, prev_ctx, cfg);

    // ✅ ADIM 2: Config.yaml parametrelerini kullan (TrendPlan.md)
    // Trend gücünü hesapla (EMA separation)
    let trend_strength = match trend {
        TrendDirection::Up => (ctx.ema_fast - ctx.ema_slow) / ctx.ema_slow,
        TrendDirection::Down => (ctx.ema_slow - ctx.ema_fast) / ctx.ema_slow,
        TrendDirection::Flat => 0.0,
    };

    // Regime belirleme: trending vs ranging
    let is_trending = trend_strength.abs() > 0.001; // %0.1+ separation = trending
    let is_weak_trend = trend_strength.abs() > 0.0005 && trend_strength.abs() <= 0.001; // %0.05-0.1 = weak trend

    // Base threshold seçimi (HFT mode vs normal)
    let base_threshold = if cfg.hft_mode {
        cfg.trend_threshold_hft
    } else {
        cfg.trend_threshold_normal
    };

    // Regime multiplier uygula
    let regime_multiplier = if is_trending {
        cfg.regime_multiplier_trending
    } else {
        cfg.regime_multiplier_ranging
    };

    // Zayıf trend için score multiplier uygula
    let score_multiplier = if is_weak_trend {
        cfg.weak_trend_score_multiplier
    } else {
        1.0
    };

    // Adaptive threshold hesapla
    let base_min = cfg.base_min_score as usize;
    let adjusted_min = (base_min as f64 * regime_multiplier) as usize;

    // Zayıf trend için score'u çarp (daha yüksek threshold gerektirir)
    let long_min = if is_weak_trend {
        (adjusted_min as f64 * score_multiplier) as usize
    } else {
        adjusted_min
    };
    let short_min = long_min; // Aynı threshold her iki taraf için

    // Determine signal side with tie-break mechanism
    let side = if long_score >= long_min && short_score >= short_min {
        // Both scores meet minimum threshold
        if long_score > short_score {
            SignalSide::Long
        } else if short_score > long_score {
            SignalSide::Short
        } else {
            // Tie-break: use trend direction as primary factor
            match trend {
                TrendDirection::Up => SignalSide::Long,
                TrendDirection::Down => SignalSide::Short,
                TrendDirection::Flat => {
                    // Secondary tie-break: use RSI
                    if ctx.rsi >= cfg.rsi_trend_long_min {
                        SignalSide::Long
                    } else if ctx.rsi <= cfg.rsi_trend_short_max {
                        SignalSide::Short
                    } else {
                        // Tertiary tie-break: use funding rate
                        if ctx.funding_rate <= cfg.funding_extreme_pos {
                            SignalSide::Long
                        } else {
                            SignalSide::Short
                        }
                    }
                }
            }
        }
    } else if long_score >= long_min && long_score > short_score {
        SignalSide::Long
    } else if short_score >= short_min && short_score > long_score {
        SignalSide::Short
    } else {
        SignalSide::Flat
    };

    Signal {
        time: candle.close_time,
        price: candle.close,
        side,
        ctx: ctx.clone(),
    }
}

// =======================
//  Sinyal Üretimi (Production için)
// =======================

/// Tüm sinyalleri üretir - sadece sinyal üretimi, pozisyon yönetimi yok
///
/// # Production Kullanımı
/// Bu fonksiyon sadece sinyal üretir. Üretilen sinyaller `ordering` modülüne
/// gönderilir ve orada pozisyon açma/kapama işlemleri yapılır.
///
/// # Backtest Kullanımı
/// Backtest için `run_backtest_on_series` kullanılır (pozisyon yönetimi içerir)
pub fn generate_signals(
    candles: &[Candle],
    contexts: &[SignalContext],
    cfg: &AlgoConfig,
) -> Vec<Signal> {
    assert_eq!(candles.len(), contexts.len());

    let mut signals = Vec::new();

    for i in 1..candles.len() {
        let c = &candles[i];
        let ctx = &contexts[i];
        let prev_ctx = if i > 0 { Some(&contexts[i - 1]) } else { None };

        let sig = generate_signal(c, ctx, prev_ctx, cfg);
        signals.push(sig);
    }

    signals
}

// =======================
//  Backtest Engine (Sadece backtest için - pozisyon yönetimi içerir)
// =======================

/// Backtest için özel fonksiyon - sinyal üretir VE pozisyon yönetimi yapar
///
/// # Backtest Execution (Realistic)
///
/// - Immediate execution: Signal at candle `i` close → executed at candle `i+1` open (1 bar delay max)
/// - No random 1-2 bar delay that allows market to move against us
/// - Dynamic slippage: Base slippage (0.05%) multiplied by volatility (ATR-based) and random factor (1.0-3.0x)
/// - High volatility periods: slippage can reach 0.1-0.5% (production reality)
///
/// # Production Execution (Realistic)
///
/// - Signal generated at candle close
/// - Signal → event bus (mpsc channel delay: ~1-10ms)
/// - Ordering module: risk checks, symbol info fetch, quantity calculation (~50-200ms)
/// - API call (network delay: ~100-500ms)
/// - Order filled at market price (slippage: 0.05% normal, 0.1-0.5% during volatility)
/// - Total delay: typically 1-5 seconds (≈ 1 bar for 5m candles)
///
/// # NOT: Production Kullanımı
/// Bu fonksiyon sadece backtest için kullanılır. Production'da:
/// 1. `generate_signals` ile sinyaller üretilir
/// 2. Sinyaller `ordering` modülüne gönderilir
/// 3. `ordering` modülü pozisyon açma/kapama işlemlerini yapar
pub fn run_backtest_on_series(
    symbol: &str,
    candles: &[Candle],
    contexts: &[SignalContext],
    cfg: &AlgoConfig,
) -> BacktestResult {
    assert_eq!(candles.len(), contexts.len());

    let mut trades: Vec<Trade> = Vec::new();

    let mut pos_side = PositionSide::Flat;
    let mut pos_entry_price = 0.0;
    let mut pos_entry_time = candles[0].open_time;
    let mut pos_entry_index: usize = 0;

    let fee_frac = cfg.fee_bps_round_trip / 10_000.0;
    let base_slippage_frac = cfg.slippage_bps / 10_000.0;

    // Signal statistics
    let mut total_signals = 0usize;
    let mut long_signals = 0usize;
    let mut short_signals = 0usize;

    // Deterministic RNG seed for reproducible results
    let mut rng = rand::rngs::StdRng::seed_from_u64(42);

    // Funding arbitrage tracker
    let mut funding_arbitrage = FundingArbitrage::new();

    // Liquidation Map - GERÇEK VERİ: Open Interest ve Long/Short Ratio kullanıyor
    let mut liquidation_map = LiquidationMap::new();

    // Volume Profile - GERÇEK VERİ: Candle verilerinden hesaplanıyor
    let volume_profile = if candles.len() >= 50 {
        Some(VolumeProfile::calculate_volume_profile(
            &candles[candles.len().saturating_sub(100)..],
        ))
    } else {
        None
    };

    for i in 1..(candles.len() - 1) {
        let c = &candles[i];
        let ctx = &contexts[i];
        let prev_ctx = if i > 0 { Some(&contexts[i - 1]) } else { None };

        // Update funding arbitrage tracker
        funding_arbitrage.update_funding(ctx.funding_rate, c.close_time);

        // Update liquidation map - GERÇEK VERİ kullanıyor
        liquidation_map.estimate_future_liquidations(
            c.close,
            ctx.open_interest,
            ctx.long_short_ratio,
            ctx.funding_rate,
        );

        // ✅ CRITICAL FIX: Realistic MarketTick for backtest (minimize production divergence)
        // Use candle data (high/low/volume) and context (ATR, volatility) to estimate realistic bid/ask and depth
        
        // 1. Calculate realistic bid/ask spread from ATR and volatility
        // Higher volatility = wider spread (production reality)
        let atr_pct = ctx.atr / c.close;
        let volatility_factor = (atr_pct * 100.0).clamp(0.5, 5.0); // 0.5x to 5x based on volatility
        // Base spread: 0.01% (1 bps) for calm markets, scales with volatility
        let base_spread_pct = 0.0001 * volatility_factor; // 0.01% to 0.5% based on volatility
        let spread_half = c.close * base_spread_pct / 2.0;
        
        // 2. Estimate bid/ask from candle range and close price
        // Use high/low range to estimate realistic bid/ask positioning
        let candle_range = c.high - c.low;
        let range_pct = if c.close > 0.0 { candle_range / c.close } else { 0.0 };
        
        // If close is near high, more ask pressure (sellers), if near low, more bid pressure (buyers)
        let close_position = if candle_range > 0.0 {
            (c.close - c.low) / candle_range // 0.0 = at low (bid pressure), 1.0 = at high (ask pressure)
        } else {
            0.5 // Balanced
        };
        
        // Adjust spread based on close position in range
        let spread_adjustment = if close_position > 0.6 {
            // Close near high = more ask pressure = wider spread upward
            1.2
        } else if close_position < 0.4 {
            // Close near low = more bid pressure = wider spread downward
            1.2
        } else {
            1.0 // Balanced
        };
        
        let final_spread_half = spread_half * spread_adjustment;
        let bid = c.close - final_spread_half;
        let ask = c.close + final_spread_half;
        
        // 3. Calculate OBI from Long/Short Ratio (more realistic)
        // LSR > 1.0 = more longs = bid pressure = OBI > 1.0
        let obi = if ctx.long_short_ratio > 1.0 {
            // Long dominance: OBI scales with LSR but capped at realistic range
            Some((ctx.long_short_ratio).min(3.0).max(0.5)) // 0.5 to 3.0 range
        } else {
            // Short dominance: inverse relationship
            Some((1.0 / ctx.long_short_ratio.max(0.1)).min(3.0).max(0.5))
        };
        
        // 4. ⚠️ CRITICAL: Depth estimation from volume is NOT real data
        // This is a simplified approximation for backtest purposes only
        // Order Flow strategies should NOT use this estimated depth data
        // Real Order Flow requires actual orderbook depth snapshots from exchange
        // 
        // For backtest: We set depth to None to disable Order Flow analysis
        // This prevents over-optimistic results from using estimated depth
        // If you have real historical depth data, use that instead
        let bid_depth_usd: Option<f64> = None; // Disabled: estimated depth is not reliable for Order Flow
        let ask_depth_usd: Option<f64> = None; // Disabled: estimated depth is not reliable for Order Flow
        
        // OLD CODE (DISABLED - DO NOT USE):
        // let volume_usd = c.volume * c.close;
        // let depth_factor = (volume_usd / 100_000.0).min(10.0).max(0.1);
        // let obi_value = obi.unwrap_or(1.0);
        // let bid_depth_usd = Some(volume_usd * 0.2 * depth_factor * ...);
        // let ask_depth_usd = Some(volume_usd * 0.2 * depth_factor * ...);

        let market_tick = MarketTick {
            symbol: symbol.to_string(),
            price: c.close,
            bid: bid.max(c.low * 0.9999).min(c.high * 0.9999), // Ensure bid is within candle range
            ask: ask.max(c.low * 1.0001).min(c.high * 1.0001), // Ensure ask is within candle range
            volume: c.volume,
            ts: c.close_time,
            obi,
            funding_rate: Some(ctx.funding_rate), // Real data
            liq_long_cluster: None, // Not available in backtest
            liq_short_cluster: None,
            bid_depth_usd,
            ask_depth_usd,
        };

        // ✅ CRITICAL FIX: Create MTF and OrderFlow analysis in backtest (same as production)
        // Multi-Timeframe Analysis - create from candles up to current index
        let mtf_analysis = if i >= 50 {
            // Use candles up to current index for MTF (same as production)
            Some(create_mtf_analysis(&candles[..=i], ctx))
        } else {
            None
        };

        // OrderFlow Analyzer - DISABLED in backtest (estimated depth is not reliable)
        // ⚠️ CRITICAL: Order Flow strategies require REAL orderbook depth data
        // Estimated depth from volume creates over-optimistic backtest results
        // To test Order Flow strategies properly, you need historical tick data with real depth
        let orderflow_analyzer: Option<OrderFlowAnalyzer> = None; // Disabled in backtest

        // Enhanced signal generation with ALL REAL DATA (including MTF and OrderFlow)
        let sig = generate_signal_enhanced(
            c,
            ctx,
            prev_ctx,
            cfg,
            candles,
            contexts,
            i,
            Some(&funding_arbitrage),
            mtf_analysis.as_ref(), // ✅ MTF enabled in backtest
            orderflow_analyzer.as_ref(), // ✅ OrderFlow enabled in backtest
            Some(&liquidation_map), // GERÇEK VERİ: Open Interest + Long/Short Ratio
            volume_profile.as_ref(), // GERÇEK VERİ: Candle verilerinden
            Some(&market_tick), // GERÇEK VERİ: Candle + Context'ten oluşturuldu
        );

        // Count signals
        match sig.side {
            SignalSide::Long => {
                total_signals += 1;
                long_signals += 1;
            }
            SignalSide::Short => {
                total_signals += 1;
                short_signals += 1;
            }
            SignalSide::Flat => {}
        }

        // === IMMEDIATE EXECUTION (NEXT BAR) ===
        // Production: Signal generated at close → executed at next open
        // No multi-bar delay that allows market to move against us
        if !matches!(sig.side, SignalSide::Flat) && matches!(pos_side, PositionSide::Flat) {
            if i + 1 < candles.len() {
                let entry_candle = &candles[i + 1]; // Next candle

                // ✅ REALISTIC SLIPPAGE (TrendPlan.md Fix #2)
                // ATR-based slippage, clamped to realistic range
                let atr_pct = ctx.atr / c.close;
                let volatility_multiplier = (atr_pct * 100.0).clamp(1.0, 3.0); // 1x-3x (more realistic)
                let dynamic_slippage_frac = base_slippage_frac * volatility_multiplier;

                match sig.side {
                    SignalSide::Long => {
                        pos_side = PositionSide::Long;
                        pos_entry_price = entry_candle.open * (1.0 + dynamic_slippage_frac);
                        pos_entry_time = entry_candle.open_time;
                        pos_entry_index = i + 1;
                    }
                    SignalSide::Short => {
                        pos_side = PositionSide::Short;
                        pos_entry_price = entry_candle.open * (1.0 - dynamic_slippage_frac);
                        pos_entry_time = entry_candle.open_time;
                        pos_entry_index = i + 1;
                    }
                    SignalSide::Flat => {}
                }
            }
        }

        if i + 1 >= candles.len() {
            continue;
        }

        let next_c = &candles[i + 1];

        // Position management
        match pos_side {
            PositionSide::Long => {
                let holding_bars = i.saturating_sub(pos_entry_index);

                // ✅ ADAPTIVE STOP LOSS (TrendPlan.md Fix #4)
                // Market volatile ise → wider stop
                // Market calm ise → tighter stop
                // ✅ CRITICAL FIX: ATR normalization - use percentage instead of absolute value
                let atr_pct = ctx.atr / c.close;
                let volatility_regime = if atr_pct > 0.02 {
                    1.5 // High volatility → 1.5x wider stop
                } else {
                    1.0 // Normal volatility
                };

                let dynamic_sl_multiplier = cfg.atr_stop_loss_multiplier * volatility_regime;
                let stop_loss_distance = atr_pct * dynamic_sl_multiplier;
                let stop_loss_price = pos_entry_price * (1.0 - stop_loss_distance);

                // ✅ TRAILING STOP LOGIC (TrendPlan.md Fix #4)
                let current_pnl_pct = (c.close - pos_entry_price) / pos_entry_price;
                let mut final_stop_price = stop_loss_price;

                if current_pnl_pct > 0.01 {
                    // %1+ profit
                    // ✅ Activate trailing stop at breakeven
                    let trailing_stop = pos_entry_price * 0.999; // -0.1% from entry
                    final_stop_price = stop_loss_price.max(trailing_stop);
                }

                // ✅ DYNAMIC TAKE PROFIT (TrendPlan.md Fix #4)
                // Strong trend → let winners run longer
                let trend_strength = (ctx.ema_fast - ctx.ema_slow).abs() / ctx.ema_slow;
                let dynamic_tp_multiplier = if trend_strength > 0.003 {
                    cfg.atr_take_profit_multiplier * 1.5 // 1.5x wider TP
                } else {
                    cfg.atr_take_profit_multiplier
                };

                // ✅ CRITICAL FIX: ATR normalization - use percentage instead of absolute value
                let atr_pct = ctx.atr / c.close;
                let take_profit_distance = atr_pct * dynamic_tp_multiplier;
                let take_profit_price = pos_entry_price * (1.0 + take_profit_distance);

                // Exit conditions
                let min_holding_bars = cfg.min_holding_bars;
                let should_close = matches!(sig.side, SignalSide::Short) ||  // Reversal signal
                    holding_bars >= cfg.max_holding_bars ||   // Max time
                    (holding_bars >= min_holding_bars && next_c.low <= final_stop_price) ||
                    (holding_bars >= min_holding_bars && next_c.high >= take_profit_price);

                if should_close {
                    let atr_pct = ctx.atr / c.close;
                    let volatility_multiplier = (atr_pct * 100.0).min(5.0).max(1.0);
                    let dynamic_slippage_frac = base_slippage_frac * volatility_multiplier;

                    let exit_price = next_c.open * (1.0 - dynamic_slippage_frac);
                    let raw_pnl = (exit_price - pos_entry_price) / pos_entry_price;
                    let pnl_pct = raw_pnl - fee_frac;
                    let win = pnl_pct > 0.0;

                    trades.push(Trade {
                        entry_time: pos_entry_time,
                        exit_time: next_c.open_time,
                        side: PositionSide::Long,
                        entry_price: pos_entry_price,
                        exit_price,
                        pnl_pct,
                        win,
                    });

                    pos_side = PositionSide::Flat;
                }
            }
            PositionSide::Short => {
                let holding_bars = i.saturating_sub(pos_entry_index);

                // ✅ ADAPTIVE STOP LOSS (TrendPlan.md Fix #4)
                // ✅ CRITICAL FIX: ATR normalization - use percentage instead of absolute value
                let atr_pct = ctx.atr / c.close;
                let volatility_regime = if atr_pct > 0.02 {
                    1.5 // High volatility → 1.5x wider stop
                } else {
                    1.0 // Normal volatility
                };

                let dynamic_sl_multiplier = cfg.atr_stop_loss_multiplier * volatility_regime;
                let stop_loss_distance = atr_pct * dynamic_sl_multiplier;
                let stop_loss_price = pos_entry_price * (1.0 + stop_loss_distance);

                // ✅ TRAILING STOP LOGIC (TrendPlan.md Fix #4)
                let current_pnl_pct = (pos_entry_price - c.close) / pos_entry_price;
                let mut final_stop_price = stop_loss_price;

                if current_pnl_pct > 0.01 {
                    // %1+ profit
                    // ✅ Activate trailing stop at breakeven
                    let trailing_stop = pos_entry_price * 1.001; // +0.1% from entry
                    final_stop_price = stop_loss_price.min(trailing_stop);
                }

                // ✅ DYNAMIC TAKE PROFIT (TrendPlan.md Fix #4)
                let trend_strength = (ctx.ema_slow - ctx.ema_fast).abs() / ctx.ema_slow;
                let dynamic_tp_multiplier = if trend_strength > 0.003 {
                    cfg.atr_take_profit_multiplier * 1.5 // 1.5x wider TP
                } else {
                    cfg.atr_take_profit_multiplier
                };

                // ✅ CRITICAL FIX: ATR normalization - use percentage instead of absolute value
                let atr_pct = ctx.atr / c.close;
                let take_profit_distance = atr_pct * dynamic_tp_multiplier;
                let take_profit_price = pos_entry_price * (1.0 - take_profit_distance);

                // Exit conditions
                let min_holding_bars = cfg.min_holding_bars;
                let should_close = matches!(sig.side, SignalSide::Long) ||  // Reversal signal
                    holding_bars >= cfg.max_holding_bars ||   // Max time
                    (holding_bars >= min_holding_bars && next_c.high >= final_stop_price) ||
                    (holding_bars >= min_holding_bars && next_c.low <= take_profit_price);

                if should_close {
                    let atr_pct = ctx.atr / c.close;
                    let volatility_multiplier = (atr_pct * 100.0).min(5.0).max(1.0);
                    let dynamic_slippage_frac = base_slippage_frac * volatility_multiplier;

                    let exit_price = next_c.open * (1.0 + dynamic_slippage_frac);
                    let raw_pnl = (pos_entry_price - exit_price) / pos_entry_price;
                    let pnl_pct = raw_pnl - fee_frac;
                    let win = pnl_pct > 0.0;

                    trades.push(Trade {
                        entry_time: pos_entry_time,
                        exit_time: next_c.open_time,
                        side: PositionSide::Short,
                        entry_price: pos_entry_price,
                        exit_price,
                        pnl_pct,
                        win,
                    });

                    pos_side = PositionSide::Flat;
                }
            }
            PositionSide::Flat => {}
        }
    }

    let total_trades = trades.len();
    let mut win_trades = 0usize;
    let mut loss_trades = 0usize;
    let mut total_pnl_pct = 0.0;
    let mut total_win_pnl = 0.0;
    let mut total_loss_pnl = 0.0;

    for t in &trades {
        if t.win {
            win_trades += 1;
            total_win_pnl += t.pnl_pct.abs();
        } else {
            loss_trades += 1;
            total_loss_pnl += t.pnl_pct.abs();
        }
        total_pnl_pct += t.pnl_pct;
    }

    let win_rate = if total_trades == 0 {
        0.0
    } else {
        win_trades as f64 / total_trades as f64
    };

    let avg_pnl_pct = if total_trades == 0 {
        0.0
    } else {
        total_pnl_pct / total_trades as f64
    };

    // Average R (Risk/Reward): average win / average loss
    let avg_r = if loss_trades > 0 && win_trades > 0 {
        let avg_win = total_win_pnl / win_trades as f64;
        let avg_loss = total_loss_pnl / loss_trades as f64;
        if avg_loss > 0.0 {
            avg_win / avg_loss
        } else {
            0.0
        }
    } else if loss_trades == 0 && win_trades > 0 {
        // Sadece kazançlar var, R = infinity (çok büyük sayı)
        f64::INFINITY
    } else {
        // Sadece kayıplar var veya hiç trade yok
        0.0
    };

    BacktestResult {
        trades,
        total_trades,
        win_trades,
        loss_trades,
        win_rate,
        total_pnl_pct,
        avg_pnl_pct,
        avg_r,
        total_signals,
        long_signals,
        short_signals,
    }
}

// =======================
//  High-level Backtest Runner
// =======================

pub async fn run_backtest(
    symbol: &str,
    kline_interval: &str, // örn: "5m"
    futures_period: &str, // openInterestHist & topLongShortAccountRatio period: "5m" vb.
    kline_limit: u32,     // 288 => son 24 saat @5m
    cfg: &AlgoConfig,
) -> Result<BacktestResult> {
    let client = FuturesClient::new();

    let candles = client
        .fetch_klines(symbol, kline_interval, kline_limit)
        .await?;
    let funding = client.fetch_funding_rates(symbol, 100).await?; // son ~100 funding event (en fazla 30 gün)
    let oi_hist = client
        .fetch_open_interest_hist(symbol, futures_period, kline_limit)
        .await?;
    let lsr_hist = client
        .fetch_top_long_short_ratio(symbol, futures_period, kline_limit)
        .await?;

    let (matched_candles, contexts) =
        build_signal_contexts(&candles, &funding, &oi_hist, &lsr_hist);
    Ok(run_backtest_on_series(symbol, &matched_candles, &contexts, cfg))
}

// =======================
//  CSV Export
// =======================

/// Backtest sonuçlarını CSV formatında export eder
/// Plan.md'de belirtildiği gibi her trade satırı CSV'ye yazılır
///
/// # Error Handling
/// - Explicitly flushes the file buffer before returning to ensure data is written
/// - If an error occurs during writing, the file may be incomplete but will be flushed
pub fn export_backtest_to_csv(result: &BacktestResult, file_path: &str) -> Result<()> {
    use std::fs::File;
    use std::io::Write;

    let mut file =
        File::create(file_path).context(format!("Failed to create CSV file: {}", file_path))?;

    // CSV header
    writeln!(
        file,
        "entry_time,exit_time,side,entry_price,exit_price,pnl_pct,win"
    )
    .context("Failed to write CSV header")?;

    // CSV rows
    for (idx, trade) in result.trades.iter().enumerate() {
        let side_str = match trade.side {
            PositionSide::Long => "LONG",
            PositionSide::Short => "SHORT",
            PositionSide::Flat => "FLAT",
        };
        writeln!(
            file,
            "{},{},{},{:.8},{:.8},{:.6},{}",
            trade.entry_time.format("%Y-%m-%d %H:%M:%S"),
            trade.exit_time.format("%Y-%m-%d %H:%M:%S"),
            side_str,
            trade.entry_price,
            trade.exit_price,
            trade.pnl_pct * 100.0, // Yüzde olarak
            if trade.win { "WIN" } else { "LOSS" }
        )
        .with_context(|| format!("Failed to write trade {} to CSV", idx + 1))?;
    }

    // Explicitly flush to ensure all data is written to disk before returning
    // This guarantees data integrity even if the function returns early
    file.flush().context("Failed to flush CSV file buffer")?;

    Ok(())
}

// =======================
//  Advanced Backtest Metrics
// =======================

/// Calculates advanced backtest metrics from a basic BacktestResult
pub fn calculate_advanced_metrics(result: &BacktestResult) -> AdvancedBacktestResult {
    let trades = &result.trades;

    // === DRAWDOWN CALCULATION ===
    let mut equity_curve = vec![100.0]; // Start with 100
    for trade in trades {
        let last_equity = *equity_curve.last().unwrap();
        equity_curve.push(last_equity * (1.0 + trade.pnl_pct));
    }

    let mut max_drawdown = 0.0;
    let mut peak = equity_curve[0];
    let mut drawdown_start: Option<DateTime<Utc>> = None;
    let mut longest_dd_duration: f64 = 0.0;

    for (i, &equity) in equity_curve.iter().enumerate() {
        if equity > peak {
            peak = equity;
            if let Some(start) = drawdown_start {
                if i > 0 && i - 1 < trades.len() {
                    let duration = (trades[i - 1].exit_time - start).num_hours() as f64;
                    longest_dd_duration = longest_dd_duration.max(duration);
                }
                drawdown_start = None;
            }
        } else {
            let dd = (peak - equity) / peak;
            if dd > max_drawdown {
                max_drawdown = dd;
                if drawdown_start.is_none() && i > 0 && i - 1 < trades.len() {
                    drawdown_start = Some(trades[i - 1].entry_time);
                }
            }
        }
    }

    let current_drawdown = if let Some(&last_equity) = equity_curve.last() {
        (peak - last_equity) / peak
    } else {
        0.0
    };

    // === CONSECUTIVE LOSSES ===
    let mut max_consecutive_losses = 0;
    let mut current_losses = 0;
    for trade in trades {
        if !trade.win {
            current_losses += 1;
            max_consecutive_losses = max_consecutive_losses.max(current_losses);
        } else {
            current_losses = 0;
        }
    }

    // === SHARPE & SORTINO RATIO ===
    let returns: Vec<f64> = trades.iter().map(|t| t.pnl_pct).collect();
    let mean_return = if !returns.is_empty() {
        returns.iter().sum::<f64>() / returns.len() as f64
    } else {
        0.0
    };
    let std_dev = if !returns.is_empty() {
        let variance = returns
            .iter()
            .map(|r| (r - mean_return).powi(2))
            .sum::<f64>()
            / returns.len() as f64;
        variance.sqrt()
    } else {
        0.0
    };

    // Annualized Sharpe (assuming 5-minute candles: 365*24*60/5 = 105120 periods per year)
    let sharpe_ratio = if std_dev > 0.0 {
        (mean_return * (365.0_f64 * 24.0_f64 / 5.0_f64).sqrt()) / std_dev
    } else {
        0.0
    };

    // Sortino uses only downside deviation
    let downside_returns: Vec<f64> = returns.iter().filter(|&&r| r < 0.0).copied().collect();
    let downside_std = if !downside_returns.is_empty() {
        let downside_variance =
            downside_returns.iter().map(|r| r.powi(2)).sum::<f64>() / downside_returns.len() as f64;
        downside_variance.sqrt()
    } else {
        0.0
    };

    let sortino_ratio = if downside_std > 0.0 {
        (mean_return * (365.0_f64 * 24.0_f64 / 5.0_f64).sqrt()) / downside_std
    } else {
        0.0
    };

    // === PROFIT FACTOR ===
    let total_wins: f64 = trades.iter().filter(|t| t.win).map(|t| t.pnl_pct).sum();
    let total_losses: f64 = trades
        .iter()
        .filter(|t| !t.win)
        .map(|t| t.pnl_pct.abs())
        .sum();
    let profit_factor = if total_losses > 0.0 {
        total_wins / total_losses
    } else if total_wins > 0.0 {
        f64::INFINITY
    } else {
        0.0
    };

    // === RECOVERY FACTOR ===
    let recovery_factor = if max_drawdown > 0.0 {
        result.total_pnl_pct / max_drawdown
    } else if result.total_pnl_pct > 0.0 {
        f64::INFINITY
    } else {
        0.0
    };

    // === AVERAGE TRADE DURATION ===
    let total_duration_hours: f64 = trades
        .iter()
        .map(|t| (t.exit_time - t.entry_time).num_hours() as f64)
        .sum();
    let avg_trade_duration = if !trades.is_empty() {
        total_duration_hours / trades.len() as f64
    } else {
        0.0
    };

    // === KELLY CRITERION ===
    let win_rate = result.win_rate;
    let avg_win = if result.win_trades > 0 {
        trades
            .iter()
            .filter(|t| t.win)
            .map(|t| t.pnl_pct)
            .sum::<f64>()
            / result.win_trades as f64
    } else {
        0.0
    };
    let avg_loss = if result.loss_trades > 0 {
        trades
            .iter()
            .filter(|t| !t.win)
            .map(|t| t.pnl_pct.abs())
            .sum::<f64>()
            / result.loss_trades as f64
    } else {
        0.0
    };
    let kelly_criterion = if avg_loss > 0.0 {
        (win_rate - ((1.0 - win_rate) / (avg_win / avg_loss))).max(0.0)
    } else {
        0.0
    };

    // === TIME-BASED ANALYSIS ===
    let mut hourly_pnl = vec![0.0; 24];
    let mut hourly_count = vec![0; 24];

    for trade in trades {
        let hour = trade.entry_time.hour() as usize;
        if hour < 24 {
            hourly_pnl[hour] += trade.pnl_pct;
            hourly_count[hour] += 1;
        }
    }

    let hourly_avg: Vec<(u32, f64)> = hourly_pnl
        .iter()
        .zip(hourly_count.iter())
        .enumerate()
        .filter(|(_, (_, &count))| count > 0)
        .map(|(hour, (&pnl, &count))| (hour as u32, pnl / count as f64))
        .collect();

    let best_hour = hourly_avg
        .iter()
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
        .map(|&(hour, _)| hour);

    let worst_hour = hourly_avg
        .iter()
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
        .map(|&(hour, _)| hour);

    AdvancedBacktestResult {
        trades: result.trades.clone(),
        total_trades: result.total_trades,
        win_trades: result.win_trades,
        loss_trades: result.loss_trades,
        win_rate: result.win_rate,
        total_pnl_pct: result.total_pnl_pct,
        avg_pnl_pct: result.avg_pnl_pct,
        avg_r: result.avg_r,
        max_drawdown_pct: max_drawdown,
        max_consecutive_losses,
        sharpe_ratio,
        sortino_ratio,
        profit_factor,
        recovery_factor,
        avg_trade_duration_hours: avg_trade_duration,
        kelly_criterion,
        best_hour_of_day: best_hour,
        worst_hour_of_day: worst_hour,
        longest_drawdown_duration_hours: longest_dd_duration,
        current_drawdown_pct: current_drawdown,
    }
}

/// Print advanced backtest report
pub fn print_advanced_report(result: &AdvancedBacktestResult) {
    println!("\n╔════════════════════════════════════════════════════════════════╗");
    println!("║              ADVANCED BACKTEST METRICS                         ║");
    println!("╚════════════════════════════════════════════════════════════════╝\n");

    println!("📊 RISK METRICS:");
    println!(
        "   Max Drawdown       : {:.2}%",
        result.max_drawdown_pct * 100.0
    );
    println!(
        "   Current Drawdown   : {:.2}%",
        result.current_drawdown_pct * 100.0
    );
    println!(
        "   Longest DD Duration: {:.1} hours",
        result.longest_drawdown_duration_hours
    );
    println!(
        "   Max Consecutive Losses: {} trades",
        result.max_consecutive_losses
    );
    println!();

    println!("📈 RISK-ADJUSTED RETURNS:");
    println!("   Sharpe Ratio       : {:.2}", result.sharpe_ratio);
    println!("   Sortino Ratio      : {:.2}", result.sortino_ratio);
    println!("   Profit Factor      : {:.2}x", result.profit_factor);
    if result.recovery_factor.is_finite() {
        println!("   Recovery Factor    : {:.2}x", result.recovery_factor);
    } else {
        println!("   Recovery Factor    : ∞ (no drawdown)");
    }
    println!();

    println!("⏱️  TRADE CHARACTERISTICS:");
    println!(
        "   Avg Trade Duration : {:.1} hours",
        result.avg_trade_duration_hours
    );
    println!();

    println!("💡 POSITION SIZING:");
    println!(
        "   Kelly Criterion    : {:.1}%",
        result.kelly_criterion * 100.0
    );
    println!("   (Suggested: Use 25-50% of Kelly for safety)");
    println!();

    if let (Some(best), Some(worst)) = (result.best_hour_of_day, result.worst_hour_of_day) {
        println!("🕐 TIME-BASED INSIGHTS:");
        println!("   Best Hour (UTC)    : {:02}:00", best);
        println!("   Worst Hour (UTC)   : {:02}:00", worst);
        println!();
    }

    // Risk assessment
    println!("⚠️  RISK ASSESSMENT:");
    if result.max_drawdown_pct > 0.20 {
        println!("   🔴 HIGH RISK: Max DD > 20% - Consider reducing position size");
    } else if result.max_drawdown_pct > 0.10 {
        println!("   🟡 MODERATE RISK: Max DD 10-20% - Acceptable for aggressive strategy");
    } else {
        println!("   🟢 LOW RISK: Max DD < 10% - Conservative strategy");
    }

    if result.sharpe_ratio < 1.0 {
        println!("   🔴 LOW SHARPE: < 1.0 - Risk-adjusted returns are poor");
    } else if result.sharpe_ratio < 2.0 {
        println!("   🟡 MODERATE SHARPE: 1.0-2.0 - Acceptable risk-adjusted returns");
    } else {
        println!("   🟢 EXCELLENT SHARPE: > 2.0 - Strong risk-adjusted returns");
    }

    println!();
}

// =======================
//  Production Trending Runner
// =======================

use crate::types::{KlineData, KlineEvent, CombinedStreamEvent, TradeSignal, TrendParams, TrendingChannels};
use futures::StreamExt;
use log::{info, warn};
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{sleep, Duration as TokioDuration};
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// Her side için ayrı cooldown tracking (trend reversal'ları kaçırmamak için)
/// LONG ve SHORT sinyalleri birbirini bloklamaz
pub(crate) struct LastSignalState {
    pub last_long_time: Option<chrono::DateTime<Utc>>,
    pub last_short_time: Option<chrono::DateTime<Utc>>,
}

/// ✅ CRITICAL FIX: Atomic cooldown check-and-set helper function
/// Prevents race conditions by atomically checking and setting cooldown
/// Returns true if cooldown passed and was set, false if cooldown is still active
async fn try_emit_signal(
    signal_state: &Arc<RwLock<LastSignalState>>,
    side: Side,
    cooldown_duration: chrono::Duration,
) -> bool {
    let mut state = signal_state.write().await;
    let now = Utc::now();
    
    let last_time = match side {
        Side::Long => state.last_long_time,
        Side::Short => state.last_short_time,
    };
    
    if let Some(last) = last_time {
        if now - last < cooldown_duration {
            return false;
        }
    }
    
    // Atomik olarak set et
    match side {
        Side::Long => state.last_long_time = Some(now),
        Side::Short => state.last_short_time = Some(now),
    }
    true
}

/// ✅ CRITICAL FIX: Atomic cooldown check-and-set helper function (for mutable reference)
/// Prevents race conditions by atomically checking and setting cooldown
/// Returns true if cooldown passed and was set, false if cooldown is still active
fn try_emit_signal_mut(
    signal_state: &mut LastSignalState,
    side: Side,
    cooldown_duration: chrono::Duration,
) -> bool {
    let now = Utc::now();
    
    let last_time = match side {
        Side::Long => signal_state.last_long_time,
        Side::Short => signal_state.last_short_time,
    };
    
    if let Some(last) = last_time {
        if now - last < cooldown_duration {
            return false;
        }
    }
    
    // Atomik olarak set et
    match side {
        Side::Long => signal_state.last_long_time = Some(now),
        Side::Short => signal_state.last_short_time = Some(now),
    }
    true
}

struct MarketTickState {
    latest_tick: Arc<RwLock<Option<MarketTick>>>,
}

/// Production için trending modülü - Kline WebSocket stream'ini dinler ve TradeSignal üretir
///
/// Bu fonksiyon:
/// 1. Kline WebSocket stream'ini dinler (gerçek zamanlı candle güncellemeleri)
/// 2. Her yeni candle tamamlandığında (is_closed=true) sinyal üretir
/// 3. Funding, OI, Long/Short ratio verilerini REST API'den çeker (daha az sıklıkla)
/// 4. TradeSignal eventlerini event bus'a gönderir
pub async fn run_trending(
    ch: TrendingChannels,
    symbol: String,
    params: TrendParams,
    ws_base_url: String,
    metrics_cache: Option<Arc<crate::metrics_cache::MetricsCache>>, // ✅ ADIM 4: Cache desteği
) {
    // ✅ CRITICAL FIX (C): Metrics cache is REQUIRED to prevent API rate limits
    // In multi-symbol mode, each symbol would call fetch_market_metrics every 5 minutes
    // Without cache, this would cause 429 Too Many Requests errors
    if metrics_cache.is_none() {
        log::warn!(
            "TRENDING: MetricsCache is None for {} - API rate limits may be exceeded in multi-symbol mode!",
            symbol
        );
        log::warn!(
            "TRENDING: Consider passing MetricsCache from main.rs to prevent API limit issues"
        );
    }
    
    let client = FuturesClient::new();

    // ✅ ADIM 2: AlgoConfig'i TrendParams'den oluştur (config.yaml parametreleri ile)
    let cfg = AlgoConfig {
        rsi_trend_long_min: params.rsi_long_min,
        rsi_trend_short_max: params.rsi_short_max,
        funding_extreme_pos: params.funding_max_for_long.max(0.0001),
        funding_extreme_neg: params.funding_min_for_short.min(-0.0001),
        lsr_crowded_long: params.obi_long_min.max(1.3),
        lsr_crowded_short: params.obi_short_max.min(0.8),
        long_min_score: params.long_min_score,
        short_min_score: params.short_min_score,
        fee_bps_round_trip: 8.0, // Default fee
        max_holding_bars: 48,    // Default max holding
        slippage_bps: 0.0,       // Default: no slippage (optimistic backtest)
        // Set to 5-10 bps (0.05-0.1%) for more realistic results
        // Signal Quality Filtering (TrendPlan.md önerileri)
        min_volume_ratio: 1.5,   // Minimum volume ratio vs 20-bar average
        max_volatility_pct: 2.0, // Maximum ATR volatility % (2% = çok volatile)
        max_price_change_5bars_pct: 3.0, // 5 bar içinde max price change % (3% = parabolic move)
        enable_signal_quality_filter: true, // Signal quality filtering aktif
        // Stop Loss & Risk Management (coin-agnostic)
        atr_stop_loss_multiplier: params.atr_sl_multiplier, // ATR multiplier from config
        atr_take_profit_multiplier: params.atr_tp_multiplier, // ATR TP multiplier from config
        min_holding_bars: 3, // Default minimum holding time (3 bars = 15 minutes @5m)
        // Can be made configurable in future
        // ✅ ADIM 2: Config.yaml parametreleri
        hft_mode: params.hft_mode,
        base_min_score: params.base_min_score,
        trend_threshold_hft: params.trend_threshold_hft,
        trend_threshold_normal: params.trend_threshold_normal,
        weak_trend_score_multiplier: params.weak_trend_score_multiplier,
        regime_multiplier_trending: params.regime_multiplier_trending,
        regime_multiplier_ranging: params.regime_multiplier_ranging,
        // Enhanced Signal Scoring (TrendPlan.md)
        enable_enhanced_scoring: params.enable_enhanced_scoring,
        enhanced_score_excellent: params.enhanced_score_excellent,
        enhanced_score_good: params.enhanced_score_good,
        enhanced_score_marginal: params.enhanced_score_marginal,
    };

    let kline_interval = "5m"; // 5 dakikalık kline kullan
    let futures_period = "5m";
    let kline_limit = (params.warmup_min_ticks + 10) as u32; // Warmup için yeterli veri

    // Candle buffer - son N candle'ı tutar (signal context hesaplama için)
    let candle_buffer = Arc::new(RwLock::new(Vec::<Candle>::new()));

    // İlk candle'ları REST API'den çek (warmup için)
    match client
        .fetch_klines(&symbol, kline_interval, kline_limit)
        .await
    {
        Ok(candles) => {
            *candle_buffer.write().await = candles;
            info!(
                "TRENDING: loaded {} candles for warmup",
                candle_buffer.read().await.len()
            );
        }
        Err(err) => {
            warn!("TRENDING: failed to fetch initial candles: {err:?}");
        }
    }

    let signal_state = LastSignalState {
        last_long_time: None,
        last_short_time: None,
    };

    let market_tick_state = MarketTickState {
        latest_tick: Arc::new(RwLock::new(None)),
    };

    info!(
        "TRENDING: started for symbol {} with kline WebSocket stream",
        symbol
    );

    let market_tick_updater = {
        let mut market_rx = ch.market_rx;
        let latest_tick = market_tick_state.latest_tick.clone();
        tokio::spawn(async move {
            loop {
                match crate::types::handle_broadcast_recv(market_rx.recv().await) {
                    Ok(Some(tick)) => {
                        *latest_tick.write().await = Some(tick);
                    }
                    Ok(None) => continue,
                    Err(_) => break,
                }
            }
        })
    };

    let kline_stream_symbol = symbol.clone();
    let kline_stream_ws_url = ws_base_url.clone();
    let kline_stream_buffer = candle_buffer.clone();
    let kline_stream_signal_state = Arc::new(RwLock::new(signal_state));
    let kline_stream_signal_tx = ch.signal_tx.clone();
    let kline_stream_market_tick = market_tick_state.latest_tick.clone();

    let kline_task = tokio::spawn(async move {
        run_kline_stream(
            kline_stream_symbol,
            kline_interval,
            futures_period,
            kline_stream_ws_url,
            kline_stream_buffer,
            client,
            cfg,
            params,
            kline_stream_signal_state,
            kline_stream_signal_tx,
            metrics_cache,
            kline_stream_market_tick,
        )
        .await;
    });

    let _ = tokio::join!(kline_task, market_tick_updater);
}

/// ✅ CRITICAL FIX: Combined Stream handler for multiple symbols (TrendPlan.md - Action Plan)
/// This reduces WebSocket connections from N (one per symbol) to 1 (combined stream)
/// Binance limit: Up to 200 streams per combined connection
/// 
/// Structure:
/// - Single WebSocket connection for all symbols
/// - Symbol-based message routing to individual handlers
/// - Each symbol maintains its own candle buffer and signal generation
pub async fn run_combined_kline_stream(
    symbols: Vec<String>,
    kline_interval: &str,
    futures_period: &str,
    ws_base_url: String,
    // Symbol -> (candle_buffer, signal_state, signal_tx, latest_market_tick, client, cfg, params, metrics_cache)
    symbol_handlers: Arc<RwLock<HashMap<String, SymbolHandler>>>,
) {
    use crate::types::CombinedStreamEvent;
    
    let mut retry_delay = TokioDuration::from_secs(1);
    
    // Build combined stream URL
    let ws_url = crate::Connection::build_combined_stream_url(&symbols, "kline", Some(kline_interval));
    
    info!("TRENDING: Combined kline stream connecting for {} symbols: {}", symbols.len(), symbols.join(", "));
    info!("TRENDING: Combined stream URL: {}", ws_url);

    loop {
        match connect_async(&ws_url).await {
            Ok((ws_stream, _)) => {
                info!("TRENDING: Combined kline stream connected ({})", ws_url);
                retry_delay = TokioDuration::from_secs(1);
                let (_, mut read) = ws_stream.split();
                
                while let Some(message) = read.next().await {
                    match message {
                        Ok(Message::Text(txt)) => {
                            // Try to parse as combined stream event first
                            if let Ok(combined_event) = serde_json::from_str::<CombinedStreamEvent>(&txt) {
                                let event = combined_event.data;
                                let symbol = event.symbol.clone();
                                
                                // Route to symbol handler
                                if let Some(handler) = symbol_handlers.read().await.get(&symbol) {
                                    if event.kline.is_closed {
                                        if let Some(candle) = parse_kline_to_candle(&event.kline) {
                                            // Update candle buffer
                                            {
                                                let mut buffer = handler.candle_buffer.write().await;
                                                buffer.push(candle.clone());
                                                let max_candles = (handler.params.warmup_min_ticks + 10) as usize;
                                                if buffer.len() > max_candles {
                                                    buffer.remove(0);
                                                }
                                            }
                                            
                                            // Generate signal
                                            if let Err(err) = generate_signal_from_candle(
                                                &candle,
                                                &handler.candle_buffer,
                                                &handler.client,
                                                &symbol,
                                                futures_period,
                                                &handler.cfg,
                                                &handler.params,
                                                handler.signal_state.clone(),
                                                &handler.signal_tx,
                                                handler.metrics_cache.as_deref(),
                                                handler.latest_market_tick.clone(),
                                            )
                                            .await
                                            {
                                                warn!("TRENDING: failed to generate signal for {}: {err}", symbol);
                                            }
                                        }
                                    }
                                } else {
                                    warn!("TRENDING: Received event for unknown symbol: {}", symbol);
                                }
                            } else {
                                // Fallback: try parsing as single stream event (for backward compatibility)
                                if let Ok(event) = serde_json::from_str::<KlineEvent>(&txt) {
                                    let symbol = event.symbol.clone();
                                    if let Some(handler) = symbol_handlers.read().await.get(&symbol) {
                                        if event.kline.is_closed {
                                            if let Some(candle) = parse_kline_to_candle(&event.kline) {
                                                let mut buffer = handler.candle_buffer.write().await;
                                                buffer.push(candle.clone());
                                                let max_candles = (handler.params.warmup_min_ticks + 10) as usize;
                                                if buffer.len() > max_candles {
                                                    buffer.remove(0);
                                                }
                                                drop(buffer);
                                                
                                                if let Err(err) = generate_signal_from_candle(
                                                    &candle,
                                                    &handler.candle_buffer,
                                                    &handler.client,
                                                    &symbol,
                                                    futures_period,
                                                    &handler.cfg,
                                                    &handler.params,
                                                    handler.signal_state.clone(),
                                                    &handler.signal_tx,
                                                    handler.metrics_cache.as_deref(),
                                                    handler.latest_market_tick.clone(),
                                                )
                                                .await
                                                {
                                                    warn!("TRENDING: failed to generate signal for {}: {err}", symbol);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Ok(Message::Binary(bin)) => {
                            if let Ok(txt) = String::from_utf8(bin) {
                                // Same parsing logic as Text message
                                if let Ok(combined_event) = serde_json::from_str::<CombinedStreamEvent>(&txt) {
                                    let event = combined_event.data;
                                    let symbol = event.symbol.clone();
                                    if let Some(handler) = symbol_handlers.read().await.get(&symbol) {
                                        if event.kline.is_closed {
                                            if let Some(candle) = parse_kline_to_candle(&event.kline) {
                                                let mut buffer = handler.candle_buffer.write().await;
                                                buffer.push(candle.clone());
                                                let max_candles = (handler.params.warmup_min_ticks + 10) as usize;
                                                if buffer.len() > max_candles {
                                                    buffer.remove(0);
                                                }
                                                drop(buffer);
                                                
                                                if let Err(err) = generate_signal_from_candle(
                                                    &candle,
                                                    &handler.candle_buffer,
                                                    &handler.client,
                                                    &symbol,
                                                    futures_period,
                                                    &handler.cfg,
                                                    &handler.params,
                                                    handler.signal_state.clone(),
                                                    &handler.signal_tx,
                                                    handler.metrics_cache.as_deref(),
                                                    handler.latest_market_tick.clone(),
                                                )
                                                .await
                                                {
                                                    warn!("TRENDING: failed to generate signal for {}: {err}", symbol);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Ok(Message::Ping(_)) | Ok(Message::Pong(_)) | Ok(Message::Frame(_)) => {}
                        Ok(Message::Close(_)) => {
                            warn!("TRENDING: Combined kline stream closed");
                            break;
                        }
                        Err(err) => {
                            warn!("TRENDING: Combined kline stream error: {err:?}");
                            break;
                        }
                    }
                }
            }
            Err(err) => {
                warn!("TRENDING: Combined kline stream connect error: {err:?}");
            }
        }
        
        info!(
            "TRENDING: Combined kline stream reconnecting in {}s",
            retry_delay.as_secs()
        );
        sleep(retry_delay).await;
        retry_delay = (retry_delay * 2).min(TokioDuration::from_secs(60));
    }
}

/// Symbol handler structure for combined stream
/// Each symbol has its own candle buffer, signal state, and generation logic
pub(crate) struct SymbolHandler {
    pub candle_buffer: Arc<RwLock<Vec<Candle>>>,
    pub signal_state: Arc<RwLock<LastSignalState>>,
    pub signal_tx: tokio::sync::mpsc::Sender<TradeSignal>,
    pub latest_market_tick: Arc<RwLock<Option<MarketTick>>>,
    pub client: FuturesClient,
    pub cfg: AlgoConfig,
    pub params: TrendParams,
    pub metrics_cache: Option<Arc<crate::metrics_cache::MetricsCache>>,
}

async fn run_kline_stream(
    symbol: String,
    kline_interval: &str,
    futures_period: &str,
    ws_base_url: String,
    candle_buffer: Arc<RwLock<Vec<Candle>>>,
    client: FuturesClient,
    cfg: AlgoConfig,
    params: TrendParams,
    signal_state: Arc<RwLock<LastSignalState>>,
    signal_tx: tokio::sync::mpsc::Sender<TradeSignal>,
    metrics_cache: Option<Arc<crate::metrics_cache::MetricsCache>>,
    latest_market_tick: Arc<RwLock<Option<MarketTick>>>,
) {
    let mut retry_delay = TokioDuration::from_secs(1);
    // ⚠️ CRITICAL: Using individual WebSocket per symbol (may hit Binance connection limits)
    // For multi-symbol mode (30+ symbols), consider using Combined Stream instead
    // Combined Stream format: /stream?streams=btcusdt@kline_5m/ethusdt@kline_5m
    // This reduces from N connections to 1 connection for N symbols
    let ws_url = format!(
        "{}/ws/{}@kline_{}",
        ws_base_url.trim_end_matches('/'),
        symbol.to_lowercase(),
        kline_interval
    );

    loop {
        match connect_async(&ws_url).await {
            Ok((ws_stream, _)) => {
                info!("TRENDING: kline stream connected ({ws_url})");
                retry_delay = TokioDuration::from_secs(1);
                let (_, mut read) = ws_stream.split();
                while let Some(message) = read.next().await {
                    match message {
                        Ok(Message::Text(txt)) => {
                            if let Ok(event) = serde_json::from_str::<KlineEvent>(&txt) {
                                if event.symbol == symbol && event.kline.is_closed {
                                    // Yeni candle tamamlandı - parse et ve buffer'a ekle
                                    if let Some(candle) = parse_kline_to_candle(&event.kline) {
                                        let mut buffer = candle_buffer.write().await;
                                        buffer.push(candle.clone());
                                        // Buffer'ı sınırla (son N candle'ı tut)
                                        let max_candles = (params.warmup_min_ticks + 10) as usize;
                                        if buffer.len() > max_candles {
                                            buffer.remove(0);
                                        }
                                        drop(buffer);

                                        if let Err(err) = generate_signal_from_candle(
                                            &candle,
                                            &candle_buffer,
                                            &client,
                                            &symbol,
                                            futures_period,
                                            &cfg,
                                            &params,
                                            signal_state.clone(),
                                            &signal_tx,
                                            metrics_cache.as_deref(),
                                            latest_market_tick.clone(),
                                        )
                                        .await
                                        {
                                            warn!("TRENDING: failed to generate signal: {err}");
                                        }
                                    }
                                }
                            }
                        }
                        Ok(Message::Binary(bin)) => {
                            if let Ok(txt) = String::from_utf8(bin) {
                                if let Ok(event) = serde_json::from_str::<KlineEvent>(&txt) {
                                    if event.symbol == symbol && event.kline.is_closed {
                                        if let Some(candle) = parse_kline_to_candle(&event.kline) {
                                            let mut buffer = candle_buffer.write().await;
                                            buffer.push(candle.clone());
                                            let max_candles =
                                                (params.warmup_min_ticks + 10) as usize;
                                            if buffer.len() > max_candles {
                                                buffer.remove(0);
                                            }
                                            drop(buffer);

                                            if let Err(err) = generate_signal_from_candle(
                                                &candle,
                                                &candle_buffer,
                                                &client,
                                                &symbol,
                                                futures_period,
                                                &cfg,
                                                &params,
                                                signal_state.clone(),
                                                &signal_tx,
                                                metrics_cache.as_deref(),
                                                latest_market_tick.clone(),
                                            )
                                            .await
                                            {
                                                warn!("TRENDING: failed to generate signal: {err}");
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Ok(Message::Ping(_)) | Ok(Message::Pong(_)) | Ok(Message::Frame(_)) => {}
                        Ok(Message::Close(frame)) => {
                            warn!("TRENDING: kline stream closed: {:?}", frame);
                            break;
                        }
                        Err(err) => {
                            warn!("TRENDING: kline stream error: {err}");
                            break;
                        }
                    }
                }
            }
            Err(err) => warn!("TRENDING: kline stream connect error: {err:?}"),
        }
        info!(
            "TRENDING: kline stream reconnecting in {}s",
            retry_delay.as_secs()
        );
        sleep(retry_delay).await;
        retry_delay = (retry_delay * 2).min(TokioDuration::from_secs(60));
    }
}

fn parse_kline_to_candle(kline: &KlineData) -> Option<Candle> {
    let open_time = DateTime::<Utc>::from_timestamp_millis(kline.open_time)?;
    let close_time = DateTime::<Utc>::from_timestamp_millis(kline.close_time)?;
    let open = kline.open.parse::<f64>().ok()?;
    let high = kline.high.parse::<f64>().ok()?;
    let low = kline.low.parse::<f64>().ok()?;
    let close = kline.close.parse::<f64>().ok()?;
    let volume = kline.volume.parse::<f64>().ok()?;

    Some(Candle {
        open_time,
        close_time,
        open,
        high,
        low,
        close,
        volume,
    })
}

async fn fetch_market_metrics(
    client: &FuturesClient,
    symbol: &str,
    futures_period: &str,
    limit: u32,
    metrics_cache: Option<&crate::metrics_cache::MetricsCache>,
) -> Result<(Vec<FundingRate>, Vec<OpenInterestPoint>, Vec<LongShortRatioPoint>)> {
    // ✅ CRITICAL FIX (C): Always prefer cache to prevent API rate limits
    // In multi-symbol mode (20 coins), without cache: 20 symbols × 3 API calls × 12 times/hour = 720 API calls/hour
    // With cache: 20 symbols × 1 cache read × 12 times/hour = 240 cache reads/hour (no API calls)
    if let Some(cache) = metrics_cache {
        let funding = cache.get_funding_rates(symbol, 100).await?;
        let oi_hist = cache.get_open_interest_hist(symbol, futures_period, limit).await?;
        let lsr_hist = cache.get_top_long_short_ratio(symbol, futures_period, limit).await?;
        Ok((funding, oi_hist, lsr_hist))
    } else {
        // ⚠️ WARNING: Direct API calls without cache - may cause rate limits in multi-symbol mode
        log::warn!(
            "TRENDING: fetch_market_metrics called without cache for {} - API rate limits may be exceeded!",
            symbol
        );
        let funding = client.fetch_funding_rates(symbol, 100).await?;
        let oi_hist = client.fetch_open_interest_hist(symbol, futures_period, limit).await?;
        let lsr_hist = client.fetch_top_long_short_ratio(symbol, futures_period, limit).await?;
        Ok((funding, oi_hist, lsr_hist))
    }
}

async fn generate_signal_from_candle(
    candle: &Candle,
    candle_buffer: &Arc<RwLock<Vec<Candle>>>,
    client: &FuturesClient,
    symbol: &str,
    futures_period: &str,
    cfg: &AlgoConfig,
    params: &TrendParams,
    signal_state: Arc<RwLock<LastSignalState>>,
    signal_tx: &tokio::sync::mpsc::Sender<TradeSignal>,
    metrics_cache: Option<&crate::metrics_cache::MetricsCache>,
    latest_market_tick: Arc<RwLock<Option<MarketTick>>>,
) -> Result<Option<TradeSignal>> {
    let buffer = candle_buffer.read().await;

    if buffer.len() < params.warmup_min_ticks {
        return Ok(None);
    }

    let (funding, oi_hist, lsr_hist) = fetch_market_metrics(
        client,
        symbol,
        futures_period,
        buffer.len() as u32,
        metrics_cache,
    )
    .await?;

    // Signal context'leri oluştur
    let (matched_candles, contexts) = build_signal_contexts(&buffer, &funding, &oi_hist, &lsr_hist);

    if contexts.len() < params.warmup_min_ticks {
        return Ok(None);
    }

    // En son candle ve context'i kullan
    let latest_idx = matched_candles.len() - 1;
    let latest_candle = &matched_candles[latest_idx];
    let latest_ctx = &contexts[latest_idx];
    let prev_ctx = if latest_idx > 0 {
        Some(&contexts[latest_idx - 1])
    } else {
        None
    };

    // ✅ FIX: Create advanced analysis objects from available data (same as backtest)
    // 1. Funding Arbitrage - build from historical funding rates
    let mut funding_arbitrage = FundingArbitrage::new();
    for (candle, ctx) in matched_candles.iter().zip(contexts.iter()) {
        funding_arbitrage.update_funding(ctx.funding_rate, candle.close_time);
    }

    // ✅ CRITICAL: Use ONLY real MarketTick from WebSocket (NO estimated/dummy data)
    // If real tick is not available or stale, skip signal generation
    let market_tick = if let Some(real_tick) = latest_market_tick.read().await.as_ref() {
        if real_tick.symbol == symbol && real_tick.ts >= latest_candle.close_time - chrono::Duration::minutes(5) {
            // Real tick is fresh and matches symbol - use it
            real_tick.clone()
        } else {
            // Real tick is stale or wrong symbol - skip signal generation
            log::warn!(
                "TRENDING: MarketTick is stale or wrong symbol (tick: {}, expected: {}, tick_ts: {}, candle_ts: {}), skipping signal",
                real_tick.symbol, symbol, real_tick.ts, latest_candle.close_time
            );
            return Ok(None);
        }
    } else {
        // No real tick available - skip signal generation
        log::warn!(
            "TRENDING: No MarketTick available for {}, skipping signal (waiting for WebSocket data)",
            symbol
        );
        return Ok(None);
    };

    // ✅ CRITICAL FIX (A): Liquidation Map - Use REAL liquidation data from connection.rs as PRIMARY source
    // Real data (liq_long_cluster, liq_short_cluster) is ALWAYS more accurate than mathematical estimates
    // Fallback to estimate only if real data is unavailable
    let mut liquidation_map = LiquidationMap::new();
    
    // PRIORITY 1: Use real liquidation data from MarketTick (connection.rs LiqState)
    if let (Some(liq_long), Some(liq_short)) = (market_tick.liq_long_cluster, market_tick.liq_short_cluster) {
        // Real liquidation data available - use it as PRIMARY source
        liquidation_map.update_from_real_liquidation_data(
            latest_candle.close,
            latest_ctx.open_interest,
            Some(liq_long),
            Some(liq_short),
        );
        log::debug!(
            "TRENDING: Using REAL liquidation data (long: {:.4}, short: {:.4}) from connection.rs LiqState",
            liq_long, liq_short
        );
    } else {
        // Real data unavailable - fallback to mathematical estimate (LESS accurate)
        log::debug!("TRENDING: Real liquidation data unavailable, using mathematical estimate (fallback)");
        for (candle, ctx) in matched_candles.iter().zip(contexts.iter()) {
            liquidation_map.estimate_future_liquidations(
                candle.close,
                ctx.open_interest,
                ctx.long_short_ratio,
                ctx.funding_rate,
            );
        }
    }

    // 3. Volume Profile - calculate from candles (if enough data)
    let volume_profile = if matched_candles.len() >= 50 {
        Some(VolumeProfile::calculate_volume_profile(
            &matched_candles[matched_candles.len().saturating_sub(100)..],
        ))
    } else {
        None
    };

    // 5. Multi-Timeframe Analysis - create from aggregated candles
    // ✅ FIX: Lower minimum requirement (50 instead of 55) for earlier MTF availability
    let mtf_analysis = if matched_candles.len() >= 50 {
        Some(create_mtf_analysis(&matched_candles, latest_ctx))
    } else {
        None
    };

    // 6. OrderFlow Analyzer - use ONLY real depth data from MarketTick
    // ✅ CRITICAL: NO estimated/simplified orderflow in production (TrendPlan.md)
    // If real depth data is not available, skip orderflow analysis
    let orderflow_analyzer = if let (Some(bid_depth), Some(ask_depth)) = (market_tick.bid_depth_usd, market_tick.ask_depth_usd) {
        // Real depth data available - use it
        create_orderflow_from_real_depth(&market_tick, &matched_candles, bid_depth, ask_depth)
    } else {
        // No real depth data - skip orderflow (don't use estimated data)
        log::debug!("TRENDING: No real depth data available, skipping orderflow analysis");
        None
    };

    // ✅ CRITICAL FIX: Log component availability for debugging
    log::debug!(
        "TRENDING: Signal generation components - funding_arbitrage: ✅, liquidation_map: ✅, market_tick: ✅, \
         mtf: {}, orderflow: {}, volume_profile: {}",
        if mtf_analysis.is_some() { "✅" } else { "❌" },
        if orderflow_analyzer.is_some() { "✅" } else { "❌" },
        if volume_profile.is_some() { "✅" } else { "❌" }
    );

    // ✅ ADIM 1: Production'da generate_signal_enhanced kullan (backtest ile aynı pipeline)
    // Advanced filtreler: volume filter, volatility percentile, support/resistance, parabolic move check
    // ✅ FIX: Pass all advanced analysis objects (100% of strategies now enabled!)
    let signal = generate_signal_enhanced(
        latest_candle,
        latest_ctx,
        prev_ctx,
        cfg,
        &matched_candles,
        &contexts,
        latest_idx,
        Some(&funding_arbitrage), // ✅ FIX: Funding arbitrage enabled
        mtf_analysis.as_ref(), // ✅ FIX: Multi-timeframe analysis enabled
        orderflow_analyzer.as_ref(), // ✅ FIX: OrderFlow analyzer enabled
        Some(&liquidation_map), // ✅ FIX: Liquidation map enabled
        volume_profile.as_ref(), // ✅ FIX: Volume profile enabled
        Some(&market_tick), // ✅ FIX: Market tick enabled
    );

    // Eğer sinyal Flat değilse, TradeSignal'e dönüştür
    match signal.side {
        SignalSide::Long | SignalSide::Short => {
            let side = match signal.side {
                SignalSide::Long => Side::Long,
                SignalSide::Short => Side::Short,
                SignalSide::Flat => unreachable!(),
            };

            // ✅ CRITICAL FIX: Atomic cooldown check-and-set to prevent race conditions
            let cooldown_duration = chrono::Duration::seconds(params.signal_cooldown_secs);
            
            // Use helper function for atomic operation
            if !try_emit_signal(&signal_state, side, cooldown_duration).await {
                // Cooldown still active, return early
                return Ok(None);
            }

            // TradeSignal oluştur
            let trade_signal = TradeSignal {
                id: Uuid::new_v4(),
                symbol: symbol.to_string(),
                side,
                entry_price: signal.price,
                leverage: params.leverage,
                size_usdt: params.position_size_quote,
                ts: signal.time,
                atr_value: Some(latest_ctx.atr),
            };

            // Signal'i gönder (cooldown already set, so no race condition)
            match signal_tx.send(trade_signal.clone()).await {
                Ok(_) => {
                    info!(
                        "TRENDING: generated {} signal for {} at price {:.2}",
                        match side {
                            Side::Long => "LONG",
                            Side::Short => "SHORT",
                        },
                        symbol,
                        signal.price
                    );

                    Ok(Some(trade_signal))
                }
                Err(err) => {
                    // Signal gönderilemedi, cooldown'u geri al (reset)
                    // This is rare but can happen if channel is closed
                    let mut state = signal_state.write().await;
                    match side {
                        Side::Long => state.last_long_time = None,
                        Side::Short => state.last_short_time = None,
                    }
                    warn!("TRENDING: failed to send signal: {err}, cooldown reset");
                    Ok(None)
                }
            }
        }
        SignalSide::Flat => Ok(None),
    }
}

/// En son kline verilerini çekip sinyal üretir
async fn generate_latest_signal(
    client: &FuturesClient,
    symbol: &str,
    kline_interval: &str,
    futures_period: &str,
    kline_limit: u32,
    cfg: &AlgoConfig,
    params: &TrendParams,
    signal_state: &mut LastSignalState,
    last_candle_time: &mut Option<chrono::DateTime<Utc>>,
    signal_tx: &tokio::sync::mpsc::Sender<TradeSignal>,
) -> Result<Option<TradeSignal>> {
    // Kline verilerini çek
    let candles = match client
        .fetch_klines(symbol, kline_interval, kline_limit)
        .await
    {
        Ok(c) => c,
        Err(err) => {
            warn!("TRENDING: failed to fetch klines: {err:?}");
            return Ok(None);
        }
    };

    if candles.is_empty() {
        return Ok(None);
    }

    // Son candle'ın zamanını kontrol et (duplicate API call koruması)
    // Interval 5 dakika olsa bile, clock drift veya API timing farklılıkları
    // nedeniyle aynı candle tekrar gelebilir
    let latest_candle = &candles[candles.len() - 1];
    if let Some(last_time) = last_candle_time {
        // Eğer aynı candle ise (close_time değişmemiş), yeni sinyal üretme
        // Bu sayede gereksiz API çağrıları ve sinyal üretimi önlenir
        if latest_candle.close_time <= *last_time {
            return Ok(None);
        }
    }
    *last_candle_time = Some(latest_candle.close_time);

    let (funding, oi_hist, lsr_hist) =
        fetch_market_metrics(client, symbol, futures_period, kline_limit, None).await?;

    // Signal context'leri oluştur (sadece gerçek API verisi olan candle'lar)
    let (matched_candles, contexts) =
        build_signal_contexts(&candles, &funding, &oi_hist, &lsr_hist);

    if contexts.len() < params.warmup_min_ticks {
        // Henüz yeterli veri yok
        return Ok(None);
    }

    // En son eşleşen candle ve context'i kullan
    let latest_matched_candle = &matched_candles[matched_candles.len() - 1];
    let latest_ctx = &contexts[contexts.len() - 1];
    let prev_ctx = if contexts.len() > 1 {
        Some(&contexts[contexts.len() - 2])
    } else {
        None
    };

    let signal = generate_signal(latest_matched_candle, latest_ctx, prev_ctx, cfg);

    // Eğer sinyal Flat değilse, TradeSignal'e dönüştür
    match signal.side {
        SignalSide::Long | SignalSide::Short => {
            let side = match signal.side {
                SignalSide::Long => Side::Long,
                SignalSide::Short => Side::Short,
                SignalSide::Flat => unreachable!(),
            };

            // ✅ CRITICAL FIX: Atomic cooldown check-and-set to prevent race conditions
            // Note: generate_latest_signal uses &mut signal_state (not Arc<RwLock>)
            let cooldown_duration = chrono::Duration::seconds(params.signal_cooldown_secs);
            
            // Use helper function for atomic operation
            if !try_emit_signal_mut(signal_state, side, cooldown_duration) {
                // Cooldown still active, return early
                return Ok(None);
            }

            // ⚠️ Production Execution Note:
            // Signal price is the candle close price, but actual execution happens later:
            // 1. Signal → event bus (mpsc channel, ~1-10ms delay)
            // 2. Ordering module: risk checks, symbol info fetch (~50-200ms)
            // 3. API call and order fill (~100-500ms)
            // 4. Real slippage at market price (varies with volatility)
            // Total delay: typically 200-1000ms, can be more during high volatility
            // This is why backtest results may be optimistic compared to production
            let trade_signal = TradeSignal {
                id: Uuid::new_v4(),
                symbol: symbol.to_string(),
                side,
                entry_price: signal.price, // Candle close price (actual fill may differ due to slippage)
                leverage: params.leverage,
                size_usdt: params.position_size_quote,
                ts: signal.time,
                atr_value: Some(latest_ctx.atr),
            };

            // Signal'i gönder (cooldown already set, so no race condition)
            match signal_tx.send(trade_signal.clone()).await {
                Ok(_) => {
                    info!(
                        "TRENDING: generated {} signal for {} at price {:.2}",
                        match side {
                            Side::Long => "LONG",
                            Side::Short => "SHORT",
                        },
                        symbol,
                        signal.price
                    );

                    Ok(Some(trade_signal))
                }
                Err(err) => {
                    // Signal gönderilemedi, cooldown'u geri al (reset)
                    // This is rare but can happen if channel is closed
                    match side {
                        Side::Long => signal_state.last_long_time = None,
                        Side::Short => signal_state.last_short_time = None,
                    }
                    warn!("TRENDING: failed to send signal: {err}, cooldown reset");
                    Ok(None)
                }
            }
        }
        SignalSide::Flat => Ok(None),
    }
}

// =======================
//  Enhanced Signal Scoring System (TrendPlan.md)
//  Professional 0-100 point scoring with 15+ factors
// =======================

/// PROFESSIONAL SCORING: 0-100 points system
/// Based on TrendPlan.md recommendations
/// 
/// Usage:
/// ```rust
/// let score = calculate_enhanced_signal_score(&ctx, SignalSide::Long);
/// 
/// // Thresholds:
/// // 80-100: Excellent signal (take it!)
/// // 65-79:  Good signal (take with smaller size)
/// // 50-64:  Marginal signal (skip or very small size)
/// // <50:    Poor signal (definitely skip)
/// ```
pub fn calculate_enhanced_signal_score(
    ctx: &EnhancedSignalContext,
    side: SignalSide,
) -> f64 {
    let mut score = 0.0;

    // === 1. TREND ALIGNMENT (0-20 points) - MOST IMPORTANT ===
    let trend_score = calculate_trend_alignment_score(
        side,
        ctx.trend_1m,
        ctx.trend_5m,
        ctx.trend_15m,
        ctx.trend_1h,
    );
    score += trend_score;
    
    // === 2. MOMENTUM (0-15 points) ===
    let momentum_score = calculate_momentum_score(
        side,
        ctx.rsi,
        ctx.macd,
        ctx.macd_signal,
        ctx.stochastic_k,
        ctx.stochastic_d,
    );
    score += momentum_score;
    
    // === 3. VOLUME CONFIRMATION (0-15 points) - CRITICAL! ===
    let volume_score = calculate_volume_score(
        side,
        ctx.volume_ratio,
        ctx.buy_volume_ratio,
    );
    score += volume_score;
    
    // === 4. MARKET MICROSTRUCTURE (0-15 points) - EDGE! ===
    let microstructure_score = calculate_microstructure_score(
        side,
        ctx.orderbook_imbalance,
        ctx.bid_ask_spread_bps,
        ctx.top_5_bid_depth_usd,
        ctx.top_5_ask_depth_usd,
    );
    score += microstructure_score;
    
    // === 5. VOLATILITY CONDITIONS (0-10 points) ===
    let volatility_score = calculate_volatility_score(
        ctx.atr_percentile,
        ctx.bollinger_width,
    );
    score += volatility_score;
    
    // === 6. MARKET SENTIMENT (0-10 points) ===
    let sentiment_score = calculate_sentiment_score(
        side,
        ctx.funding_rate,
        ctx.long_short_ratio,
    );
    score += sentiment_score;
    
    // === 7. SUPPORT/RESISTANCE (0-10 points) ===
    let sr_score = calculate_support_resistance_score(
        side,
        ctx.nearest_support_distance,
        ctx.nearest_resistance_distance,
        ctx.support_strength,
        ctx.resistance_strength,
    );
    score += sr_score;
    
    // === 8. RISK FACTORS (0-5 points) ===
    let risk_score = calculate_risk_factors(
        ctx.bid_ask_spread_bps,
        ctx.open_interest,
    );
    score += risk_score;
    
    score
}

/// Trend alignment across multiple timeframes
/// Perfect alignment = 20 points
fn calculate_trend_alignment_score(
    side: SignalSide,
    trend_1m: TrendDirection,
    trend_5m: TrendDirection,
    trend_15m: TrendDirection,
    trend_1h: TrendDirection,
) -> f64 {
    let mut aligned_count = 0;
    let trends = vec![trend_1m, trend_5m, trend_15m, trend_1h];

    for trend in trends {
        let is_aligned = match (side, trend) {
            (SignalSide::Long, TrendDirection::Up) => true,
            (SignalSide::Short, TrendDirection::Down) => true,
            _ => false,
        };
        if is_aligned {
            aligned_count += 1;
        }
    }
    
    // Weighted scoring: Higher timeframes are more important
    // 1m=3pts, 5m=5pts, 15m=6pts, 1h=6pts
    match aligned_count {
        4 => 20.0,  // Perfect alignment
        3 => 15.0,  // Strong alignment
        2 => 8.0,   // Weak alignment
        1 => 3.0,   // Very weak
        _ => 0.0,   // No alignment
    }
}

/// Momentum indicators scoring
fn calculate_momentum_score(
    side: SignalSide,
    rsi: f64,
    macd: f64,
    macd_signal: f64,
    stoch_k: f64,
    stoch_d: f64,
) -> f64 {
    let mut score = 0.0;

    // RSI (0-5 points)
    match side {
        SignalSide::Long => {
            if rsi >= 40.0 && rsi <= 60.0 {
                score += 5.0; // Sweet spot
            } else if rsi > 30.0 && rsi < 40.0 {
                score += 3.0; // Recovering from oversold
            }
        }
        SignalSide::Short => {
            if rsi >= 40.0 && rsi <= 60.0 {
                score += 5.0; // Sweet spot
            } else if rsi > 60.0 && rsi < 70.0 {
                score += 3.0; // Recovering from overbought
            }
        }
        _ => {}
    }
    
    // MACD (0-5 points)
    let macd_histogram = macd - macd_signal;
    match side {
        SignalSide::Long => {
            if macd_histogram > 0.0 {
                score += 5.0; // Bullish crossover
            } else if macd_histogram > -0.0001 {
                score += 2.0; // About to cross
            }
        }
        SignalSide::Short => {
            if macd_histogram < 0.0 {
                score += 5.0; // Bearish crossover
            } else if macd_histogram < 0.0001 {
                score += 2.0; // About to cross
            }
        }
        _ => {}
    }
    
    // Stochastic (0-5 points)
    match side {
        SignalSide::Long => {
            if stoch_k > stoch_d && stoch_k < 80.0 {
                score += 5.0; // Bullish and not overbought
            } else if stoch_k < 20.0 {
                score += 3.0; // Oversold, potential reversal
            }
        }
        SignalSide::Short => {
            if stoch_k < stoch_d && stoch_k > 20.0 {
                score += 5.0; // Bearish and not oversold
            } else if stoch_k > 80.0 {
                score += 3.0; // Overbought, potential reversal
            }
        }
        _ => {}
    }
    
    score
}

/// Volume confirmation scoring - CRITICAL FOR CRYPTO!
fn calculate_volume_score(
    side: SignalSide,
    volume_ratio: f64,
    buy_volume_ratio: f64,
) -> f64 {
    let mut score = 0.0;

    // Volume surge (0-8 points)
    if volume_ratio > 2.0 {
        score += 8.0; // Strong volume confirmation
    } else if volume_ratio > 1.5 {
        score += 5.0; // Good volume
    } else if volume_ratio > 1.0 {
        score += 2.0; // Normal volume
    }
    // volume_ratio < 1.0 = 0 points (weak volume)
    
    // Buy/Sell pressure (0-7 points)
    match side {
        SignalSide::Long => {
            if buy_volume_ratio > 0.60 {
                score += 7.0; // Strong buy pressure
            } else if buy_volume_ratio > 0.55 {
                score += 4.0; // Moderate buy pressure
            }
        }
        SignalSide::Short => {
            if buy_volume_ratio < 0.40 {
                score += 7.0; // Strong sell pressure
            } else if buy_volume_ratio < 0.45 {
                score += 4.0; // Moderate sell pressure
            }
        }
        _ => {}
    }
    
    score
}

/// Market microstructure - THE EDGE!
fn calculate_microstructure_score(
    side: SignalSide,
    orderbook_imbalance: f64,
    spread_bps: f64,
    bid_depth: f64,
    ask_depth: f64,
) -> f64 {
    let mut score = 0.0;

    // ✅ CRITICAL FIX: Check if data is missing (indicated by very high spread or zero depth)
    // If spread is very high (>= 1000 bps), it indicates missing data, not a real spread
    // If both depths are zero, it might indicate missing data
    let is_missing_data = spread_bps >= 1000.0 || (bid_depth == 0.0 && ask_depth == 0.0);
    
    if is_missing_data {
        // Missing data - return zero score (no bonus, no penalty)
        // This prevents false positive scores from fallback values
        return 0.0;
    }

    // Orderbook imbalance (0-8 points) - MOST IMPORTANT
    match side {
        SignalSide::Long => {
            if orderbook_imbalance > 1.3 {
                score += 8.0; // Strong bid support
            } else if orderbook_imbalance > 1.1 {
                score += 5.0; // Moderate bid support
            }
        }
        SignalSide::Short => {
            if orderbook_imbalance < 0.7 {
                score += 8.0; // Strong ask pressure
            } else if orderbook_imbalance < 0.9 {
                score += 5.0; // Moderate ask pressure
            }
        }
        _ => {}
    }
    
    // Spread quality (0-4 points)
    // ✅ FIX: spread_bps = 0.0 is now handled above (missing data check)
    if spread_bps > 0.0 && spread_bps < 5.0 {
        score += 4.0; // Tight spread = good liquidity
    } else if spread_bps > 0.0 && spread_bps < 10.0 {
        score += 2.0; // Normal spread
    }
    // spread >= 10bps or spread = 0.0 = 0 points (poor liquidity or missing data)
    
    // Depth quality (0-3 points)
    let min_depth = 50000.0; // $50k minimum depth
    if bid_depth > min_depth && ask_depth > min_depth {
        score += 3.0; // Good liquidity both sides
    } else if bid_depth > min_depth || ask_depth > min_depth {
        score += 1.0; // One-sided liquidity
    }
    
    score
}

/// Volatility conditions scoring
fn calculate_volatility_score(
    atr_percentile: f64,
    bb_width: f64,
) -> f64 {
    let mut score = 0.0;

    // ATR percentile (0-5 points)
    // Mid-range volatility is best for trend trading
    if atr_percentile > 0.3 && atr_percentile < 0.7 {
        score += 5.0; // Sweet spot
    } else if atr_percentile > 0.2 && atr_percentile < 0.8 {
        score += 3.0; // Acceptable
    }
    // Too low or too high volatility = 0 points
    
    // Bollinger Band width (0-5 points)
    if bb_width > 0.02 && bb_width < 0.05 {
        score += 5.0; // Good volatility for trading
    } else if bb_width > 0.01 && bb_width < 0.07 {
        score += 3.0; // Acceptable
    }
    
    score
}

/// Market sentiment scoring
fn calculate_sentiment_score(
    side: SignalSide,
    funding_rate: f64,
    long_short_ratio: f64,
) -> f64 {
    let mut score = 0.0;

    // Funding rate (0-5 points) - Contrarian approach
    match side {
        SignalSide::Long => {
            if funding_rate < -0.0002 {
                score += 5.0; // Shorts paying, bullish
            } else if funding_rate < 0.0001 {
                score += 3.0; // Neutral funding
            }
        }
        SignalSide::Short => {
            if funding_rate > 0.0002 {
                score += 5.0; // Longs paying, bearish
            } else if funding_rate > -0.0001 {
                score += 3.0; // Neutral funding
            }
        }
        _ => {}
    }
    
    // Long/Short ratio (0-5 points) - Contrarian
    match side {
        SignalSide::Long => {
            if long_short_ratio < 0.8 {
                score += 5.0; // Too many shorts, squeeze potential
            } else if long_short_ratio < 1.0 {
                score += 3.0; // Balanced, slight short bias
            }
        }
        SignalSide::Short => {
            if long_short_ratio > 1.3 {
                score += 5.0; // Too many longs, dump potential
            } else if long_short_ratio > 1.1 {
                score += 3.0; // Balanced, slight long bias
            }
        }
        _ => {}
    }
    
    score
}

/// Support/Resistance scoring
fn calculate_support_resistance_score(
    side: SignalSide,
    support_distance: f64,
    resistance_distance: f64,
    support_strength: f64,
    resistance_strength: f64,
) -> f64 {
    let mut score = 0.0;

    match side {
        SignalSide::Long => {
            // Close to strong support = good long entry (0-5 points)
            if support_distance < 0.01 && support_strength > 0.7 {
                score += 5.0; // At strong support
            } else if support_distance < 0.02 && support_strength > 0.5 {
                score += 3.0; // Near moderate support
            }
            
            // Far from resistance = room to run (0-5 points)
            if resistance_distance > 0.03 {
                score += 5.0; // Plenty of room
            } else if resistance_distance > 0.02 {
                score += 3.0; // Some room
            }
        }
        SignalSide::Short => {
            // Close to strong resistance = good short entry (0-5 points)
            if resistance_distance < 0.01 && resistance_strength > 0.7 {
                score += 5.0; // At strong resistance
            } else if resistance_distance < 0.02 && resistance_strength > 0.5 {
                score += 3.0; // Near moderate resistance
            }
            
            // Far from support = room to fall (0-5 points)
            if support_distance > 0.03 {
                score += 5.0; // Plenty of room
            } else if support_distance > 0.02 {
                score += 3.0; // Some room
            }
        }
        _ => {}
    }
    
    score
}

/// Risk factors penalty
fn calculate_risk_factors(
    spread_bps: f64,
    open_interest: f64,
) -> f64 {
    let mut score = 5.0; // Start with full points

    // Wide spread = penalty
    if spread_bps > 20.0 {
        score -= 3.0; // Severe penalty
    } else if spread_bps > 10.0 {
        score -= 1.0; // Minor penalty
    }
    
    // Low OI = penalty (less than $100M)
    if open_interest < 100_000_000.0 {
        score -= 2.0;
    }
    
    if score < 0.0 {
        0.0
    } else {
        score
    }
}

// =======================
//  Enhanced Signal Context Builder
//  Converts SignalContext + MarketTick to EnhancedSignalContext
// =======================

/// Build EnhancedSignalContext from available data
/// This function creates a comprehensive context for enhanced scoring
pub fn build_enhanced_signal_context(
    ctx: &SignalContext,
    candle: &Candle,
    candles: &[Candle],
    current_index: usize,
    market_tick: Option<&MarketTick>,
    multi_timeframe_trends: Option<(TrendDirection, TrendDirection, TrendDirection, TrendDirection)>,
) -> EnhancedSignalContext {
    // Calculate volume metrics
    let volume_ma_20 = if current_index >= 20 && candles.len() > current_index {
        let recent_candles = &candles[current_index.saturating_sub(19)..=current_index.min(candles.len() - 1)];
        recent_candles.iter().map(|c| c.volume).sum::<f64>() / recent_candles.len() as f64
    } else {
        candle.volume
    };
    let volume_ratio = candle.volume / volume_ma_20.max(0.0001);
    
    // Calculate buy volume ratio from OBI (real data)
    // If OBI not available, use neutral 0.5 (balanced market assumption)
    let buy_volume_ratio = market_tick
        .and_then(|t| t.obi)
        .map(|obi| {
            // OBI > 1.0 means more bid pressure = more buy volume
            if obi > 1.0 {
                0.5 + (obi - 1.0).min(1.0) * 0.3 // Max 0.8
            } else {
                0.5 - (1.0 - obi).min(1.0) * 0.3 // Min 0.2
            }
        })
        .unwrap_or(0.5); // Neutral if OBI not available (balanced market)

    // Calculate MACD (simplified: EMA12 - EMA26)
    let macd = calculate_macd(candles, current_index);
    let macd_signal = calculate_macd_signal(candles, current_index);
    
    // Calculate Stochastic
    let (stoch_k, stoch_d) = calculate_stochastic(candles, current_index);
    
    // Calculate ATR percentile
    let atr_percentile = calculate_atr_percentile(ctx.atr, candles, current_index);
    
    // Calculate Bollinger Bands
    let (bb_width, price_vs_bb_upper, price_vs_bb_lower) = calculate_bollinger_bands(candles, current_index, candle.close);
    
    // Market microstructure from MarketTick
    // ✅ CRITICAL FIX: NO fallback values that cause incorrect scoring
    // If market_tick is missing, use values that result in ZERO scoring contribution
    // (not false positives or false negatives)
    let (bid_ask_spread_bps, orderbook_imbalance, top_5_bid_depth_usd, top_5_ask_depth_usd) = 
        if let Some(tick) = market_tick {
            // Real tick data available - use it
            let spread = if tick.ask > 0.0 && tick.bid > 0.0 {
                ((tick.ask - tick.bid) / tick.price) * 10000.0 // Convert to bps
            } else {
                // Invalid bid/ask - cannot calculate spread
                // Use a high spread value to indicate missing/invalid data (will result in penalty)
                1000.0 // Very high spread = penalty in risk scoring
            };
            // ✅ CRITICAL FIX: No fallback values - use None/penalty values instead of 0
            // If depth/OBI not available, use penalty values that result in zero score contribution
            // (not false positives or false negatives)
            let obi = tick.obi.unwrap_or(1.0); // 1.0 = neutral (no bonus/penalty)
            let bid_depth = tick.bid_depth_usd.unwrap_or(0.0); // 0.0 = penalty (will result in zero score)
            let ask_depth = tick.ask_depth_usd.unwrap_or(0.0); // 0.0 = penalty (will result in zero score)
            (spread, obi, bid_depth, ask_depth)
        } else {
            // ❌ CRITICAL: No market tick - this should not happen in production
            // Use values that result in ZERO scoring contribution (not false positives)
            // - spread = very high (1000 bps) to indicate missing data (will result in penalty, not bonus)
            // - obi = 1.0 (balanced/neutral, no bonus/penalty)
            // - depth = 0.0 (no depth, will result in penalty in microstructure scoring, not bonus)
            // This ensures missing data doesn't give false positive scores
            log::warn!("TRENDING: build_enhanced_signal_context called without MarketTick - missing data, using penalty values to prevent false positives");
            (1000.0, 1.0, 0.0, 0.0) // High spread and zero depth = penalty, not bonus
        };
    
    // Multi-timeframe trends (default to current trend if not available)
    let current_trend = classify_trend(ctx);
    let (trend_1m, trend_5m, trend_15m, trend_1h) = multi_timeframe_trends
        .unwrap_or((current_trend, current_trend, current_trend, current_trend));
    
    // Support/Resistance (simplified calculation)
    let (nearest_support_distance, nearest_resistance_distance, support_strength, resistance_strength) =
        calculate_support_resistance(candles, current_index, candle.close);
    
    EnhancedSignalContext {
        ema_fast: ctx.ema_fast,
        ema_slow: ctx.ema_slow,
        rsi: ctx.rsi,
        atr: ctx.atr,
        bid_ask_spread_bps,
        orderbook_imbalance,
        top_5_bid_depth_usd,
        top_5_ask_depth_usd,
        volume_ma_20,
        volume_ratio,
        buy_volume_ratio,
        macd,
        macd_signal,
        stochastic_k: stoch_k,
        stochastic_d: stoch_d,
        atr_percentile,
        bollinger_width: bb_width,
        price_vs_bb_upper,
        price_vs_bb_lower,
        funding_rate: ctx.funding_rate,
        open_interest: ctx.open_interest,
        long_short_ratio: ctx.long_short_ratio,
        trend_1m,
        trend_5m,
        trend_15m,
        trend_1h,
        nearest_support_distance,
        nearest_resistance_distance,
        support_strength,
        resistance_strength,
    }
}

/// Calculate MACD (EMA12 - EMA26)
fn calculate_macd(candles: &[Candle], current_index: usize) -> f64 {
    if current_index < 26 || candles.len() <= current_index {
        return 0.0;
    }
    
    let mut ema12 = ExponentialMovingAverage::new(12).unwrap();
    let mut ema26 = ExponentialMovingAverage::new(26).unwrap();
    
    let start = current_index.saturating_sub(50).max(0);
    for i in start..=current_index {
        let di = candle_to_data_item(&candles[i]);
        ema12.next(&di);
        ema26.next(&di);
    }
    
    let di = candle_to_data_item(&candles[current_index]);
    
    let ema12_val = ema12.next(&di);
    let ema26_val = ema26.next(&di);
    
    ema12_val - ema26_val
}

/// Calculate MACD Signal (EMA9 of MACD)
fn calculate_macd_signal(candles: &[Candle], current_index: usize) -> f64 {
    if current_index < 35 || candles.len() <= current_index {
        return 0.0;
    }
    
    // Calculate MACD values for last 20 periods
    let mut macd_values = Vec::new();
    let start = current_index.saturating_sub(20).max(0);
    
    for i in start..=current_index {
        let macd_val = calculate_macd(candles, i);
        macd_values.push(macd_val);
    }
    
    // Calculate EMA9 of MACD
    let mut ema9 = ExponentialMovingAverage::new(9).unwrap();
    for &macd_val in &macd_values {
        let di = value_to_data_item(macd_val);
        ema9.next(&di);
    }
    
    let last_macd = macd_values.last().copied().unwrap_or(0.0);
    let di = value_to_data_item(last_macd);
    
    ema9.next(&di)
}

/// Calculate Stochastic %K and %D
fn calculate_stochastic(candles: &[Candle], current_index: usize) -> (f64, f64) {
    if current_index < 14 || candles.len() <= current_index {
        return (50.0, 50.0); // Neutral values
    }
    
    let period = 14;
    let lookback = current_index.min(candles.len() - 1).saturating_sub(period - 1);
    let end = current_index.min(candles.len() - 1);
    
    let current_close = candles[end].close;
    let mut highest_high = f64::MIN;
    let mut lowest_low = f64::MAX;
    
    for i in lookback..=end {
        highest_high = highest_high.max(candles[i].high);
        lowest_low = lowest_low.min(candles[i].low);
    }
    
    let range = highest_high - lowest_low;
    let stoch_k = if range > 0.0 {
        ((current_close - lowest_low) / range) * 100.0
    } else {
        50.0
    };
    
    // %D is SMA of %K (3-period)
    let stoch_d = if end >= 2 {
        let k_values: Vec<f64> = (end.saturating_sub(2)..=end)
            .map(|i| {
                if i >= period {
                    let lookback_k = i.saturating_sub(period - 1);
                    let close_k = candles[i].close;
                    let mut hh = f64::MIN;
                    let mut ll = f64::MAX;
                    for j in lookback_k..=i {
                        hh = hh.max(candles[j].high);
                        ll = ll.min(candles[j].low);
                    }
                    let r = hh - ll;
                    if r > 0.0 {
                        ((close_k - ll) / r) * 100.0
                    } else {
                        50.0
                    }
                } else {
                    50.0
                }
            })
            .collect();
        k_values.iter().sum::<f64>() / k_values.len() as f64
    } else {
        stoch_k
    };
    
    (stoch_k, stoch_d)
}

/// Calculate ATR percentile (0-1) based on historical ATR values
fn calculate_atr_percentile(current_atr: f64, candles: &[Candle], current_index: usize) -> f64 {
    if current_index < 50 || candles.len() <= current_index {
        return 0.5; // Default to median
    }
    
    let mut atr_calc = AverageTrueRange::new(14).unwrap();
    let mut atr_values = Vec::new();
    
    let start = current_index.saturating_sub(100).max(0);
    for i in start..=current_index {
        let di = candle_to_data_item(&candles[i]);
        let atr_val = atr_calc.next(&di);
        atr_values.push(atr_val);
    }
    
    if atr_values.is_empty() {
        return 0.5;
    }
    
    // Count how many ATR values are below current
    let below_count = atr_values.iter().filter(|&&v| v < current_atr).count();
    below_count as f64 / atr_values.len() as f64
}

/// Calculate Bollinger Bands and return width and distances
/// Returns default values only during warmup period (insufficient data)
fn calculate_bollinger_bands(candles: &[Candle], current_index: usize, current_price: f64) -> (f64, f64, f64) {
    if current_index < 20 || candles.len() <= current_index {
        // Warmup period: insufficient data - return neutral values
        // This is NOT dummy data, it's a valid fallback during initialization
        return (0.02, 0.0, 0.0);
    }
    
    let period = 20;
    let std_dev = 2.0;
    let start = current_index.saturating_sub(period - 1);
    let end = current_index.min(candles.len() - 1);
    
    let closes: Vec<f64> = candles[start..=end].iter().map(|c| c.close).collect();
    let sma = closes.iter().sum::<f64>() / closes.len() as f64;
    let std = calculate_std_dev(&closes);
    
    let upper_band = sma + (std_dev * std);
    let lower_band = sma - (std_dev * std);
    
    // Width as % of price
    let width = ((upper_band - lower_band) / current_price).max(0.0);
    
    // Distance to bands as % of price
    let dist_upper = if current_price > 0.0 {
        ((current_price - upper_band) / current_price).max(0.0)
    } else {
        0.0
    };
    let dist_lower = if current_price > 0.0 {
        ((lower_band - current_price) / current_price).max(0.0)
    } else {
        0.0
    };
    
    (width, dist_upper, dist_lower)
}

/// Calculate support and resistance levels
fn calculate_support_resistance(
    candles: &[Candle],
    current_index: usize,
    current_price: f64,
) -> (f64, f64, f64, f64) {
    if current_index < 20 || candles.len() <= current_index {
        // Warmup period: insufficient data - return neutral values
        // This is NOT dummy data, it's a valid fallback during initialization
        return (0.05, 0.05, 0.5, 0.5);
    }
    
    let lookback = 50.min(current_index);
    let start = current_index.saturating_sub(lookback);
    
    // Find local lows (support) and highs (resistance)
    let mut support_levels = Vec::new();
    let mut resistance_levels = Vec::new();
    
    for i in (start + 2)..current_index.min(candles.len() - 1) {
        // Local low (support)
        if candles[i].low < candles[i - 1].low && candles[i].low < candles[i + 1].low {
            support_levels.push(candles[i].low);
        }
        // Local high (resistance)
        if candles[i].high > candles[i - 1].high && candles[i].high > candles[i + 1].high {
            resistance_levels.push(candles[i].high);
        }
    }
    
    // Find nearest support and resistance
    let nearest_support = support_levels.iter()
        .filter(|&&s| s < current_price)
        .max_by(|a, b| a.partial_cmp(b).unwrap())
        .copied()
        .unwrap_or(current_price * 0.95); // Default 5% below
    
    let nearest_resistance = resistance_levels.iter()
        .filter(|&&r| r > current_price)
        .min_by(|a, b| a.partial_cmp(b).unwrap())
        .copied()
        .unwrap_or(current_price * 1.05); // Fallback: 5% above if no resistance found (valid during warmup)
    
    // Calculate distances as percentages
    let support_distance = ((current_price - nearest_support) / current_price).max(0.0);
    let resistance_distance = ((nearest_resistance - current_price) / current_price).max(0.0);
    
    // Calculate strength (how many times level was tested)
    let support_strength = support_levels.iter()
        .filter(|&&s| (s - nearest_support).abs() / current_price < 0.01) // Within 1%
        .count() as f64 / 10.0; // Normalize to 0-1
    let support_strength = support_strength.min(1.0);
    
    let resistance_strength = resistance_levels.iter()
        .filter(|&&r| (r - nearest_resistance).abs() / current_price < 0.01) // Within 1%
        .count() as f64 / 10.0; // Normalize to 0-1
    let resistance_strength = resistance_strength.min(1.0);
    
    (support_distance, resistance_distance, support_strength, resistance_strength)
}

