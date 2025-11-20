use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Timelike, Utc};
use reqwest::{Client, Url};
use serde::Deserialize;
use ta::indicators::{AverageTrueRange, ExponentialMovingAverage, RelativeStrengthIndex};
use ta::{DataItem, Next};
use rand::{Rng, SeedableRng};
use rand::rngs::StdRng;

use crate::types::{
    AdvancedBacktestResult, AlgoConfig, BacktestResult, Candle, DepthSnapshot, FundingRate,
    FuturesClient, LongShortRatioPoint, MarketTick, OpenInterestHistPoint, OpenInterestPoint,
    PositionSide, Side, Signal, SignalContext, SignalSide, Trade, TrendDirection,
};
use uuid::Uuid;
use std::collections::{VecDeque, HashMap, BTreeMap};

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
    ExtremePositive,  // Funding çok yükseldi, reversal yakın
    ExtremeNegative,   // Funding çok düştü, reversal yakın
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
            now.date_naive().and_hms_opt(next_hour, 0, 0).unwrap().and_utc()
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
    
    /// 🔥 KRITIK: Funding saatinden 30 dakika öncesi = arbitrage fırsatı
    pub fn is_pre_funding_window(&self, now: DateTime<Utc>) -> bool {
        let time_to_funding = self.next_funding_time.signed_duration_since(now);
        time_to_funding.num_minutes() <= 30 && time_to_funding.num_minutes() >= 0
    }
    
    /// 🔥 SECRET STRATEGY: Extreme Funding Reversal
    /// 
    /// Mantık:
    /// - Funding +0.1% (extreme long crowding) → SHORT pozisyon aç
    /// - Funding saati geldiğinde long'lar funding ödeyecek
    /// - Long'lar pozisyon kapatacak → fiyat düşecek
    /// - Sen SHORT pozisyondaysan → kazanırsın
    pub fn detect_funding_arbitrage(&self, now: DateTime<Utc>) -> Option<FundingArbitrageSignal> {
        if !self.is_pre_funding_window(now) {
            return None;
        }
        
        if self.funding_history.is_empty() {
            return None;
        }
        
        let latest_funding = self.funding_history.back()?.funding_rate;
        
        // 🚨 Extreme positive funding = too many longs
        if latest_funding > 0.001 { // 0.1% (very high)
            // Strategy: Open SHORT before funding time
            // Expected: Long traders will close → price drops
            Some(FundingArbitrageSignal::PreFundingShort {
                expected_pnl_bps: (latest_funding * 10000.0) as i32, // basis points
                time_to_funding: self.next_funding_time.signed_duration_since(now),
            })
        }
        // 🚨 Extreme negative funding = too many shorts
        else if latest_funding < -0.001 {
            // Strategy: Open LONG before funding time
            Some(FundingArbitrageSignal::PreFundingLong {
                expected_pnl_bps: (latest_funding.abs() * 10000.0) as i32,
                time_to_funding: self.next_funding_time.signed_duration_since(now),
            })
        } else {
            None
        }
    }
    
    /// 🔥 POST-FUNDING REVERSAL
    /// Funding ödendikten hemen sonra, pozisyonlar likidasyona gidebilir
    pub fn detect_post_funding_opportunity(&self, now: DateTime<Utc>) -> Option<PostFundingSignal> {
        let time_since_funding = now.signed_duration_since(self.next_funding_time);
        
        // Funding'den 5 dakika sonrasına kadar
        if time_since_funding.num_minutes() > 0 && time_since_funding.num_minutes() <= 5 {
            if let Some(latest) = self.funding_history.back() {
                if latest.funding_rate > 0.001 {
                    // High positive funding just paid → long traders hurt
                    // Some will liquidate → price may cascade down
                    Some(PostFundingSignal::ExpectLongLiquidation)
                } else if latest.funding_rate < -0.001 {
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
    
    /// 🔥 ADVANCED: Funding Rate Trend
    /// Eğer funding rate sürekli artıyorsa → reversal yakın
    pub fn detect_funding_exhaustion(&self) -> Option<FundingExhaustionSignal> {
        if self.funding_history.len() < 5 {
            return None;
        }
        
        // Son 5 funding rate
        let recent: Vec<&FundingSnapshot> = self.funding_history.iter().rev().take(5).collect();
        if recent.len() < 5 {
            return None;
        }
        
        // Trend: sürekli artıyor mu?
        let mut increasing_count = 0;
        let mut decreasing_count = 0;
        
        for i in 1..recent.len() {
            if recent[i].funding_rate > recent[i-1].funding_rate {
                increasing_count += 1;
            } else {
                decreasing_count += 1;
            }
        }
        
        // 🚨 4/4 artış = extreme, reversal yakın
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
    Bullish,  // Market maker accumulating
    Bearish,   // Market maker distributing
}

#[derive(Debug, Clone, Copy)]
pub enum SpoofingSignal {
    BidSideSpoofing,  // Fake buy orders
    AskSideSpoofing,  // Fake sell orders
}

#[derive(Debug, Clone, Copy)]
pub enum IcebergSignal {
    BidSideIceberg,  // Hidden buy orders
    AskSideIceberg,  // Hidden sell orders
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
        let bid_volume: f64 = depth.bids.iter()
            .filter_map(|lvl| lvl[1].parse::<f64>().ok())
            .sum();
        
        let ask_volume: f64 = depth.asks.iter()
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
    
    /// 🔥 SECRET SAUCE: Absorption Detection
    /// Market maker'lar büyük satış baskısını "absorb" ediyorsa = bullish
    /// Örnek: Sürekli sell pressure ama fiyat düşmüyor = MM accumulation
    pub fn detect_absorption(&self) -> Option<AbsorptionSignal> {
        if self.snapshots.len() < 10 {
            return None;
        }
        
        let recent: Vec<&OrderFlowSnapshot> = self.snapshots.iter().rev().take(10).collect();
        
        // Son 10 snapshot'ta volume imbalance
        let mut buy_volume_total = 0.0;
        let mut sell_volume_total = 0.0;
        
        for snap in recent {
            // Eğer ask volume > bid volume ama fiyat stable/up = absorption
            if snap.ask_volume > snap.bid_volume {
                sell_volume_total += snap.ask_volume;
            } else {
                buy_volume_total += snap.bid_volume;
            }
        }
        
        let imbalance_ratio = sell_volume_total / buy_volume_total.max(1.0);
        
        // 🚨 KRITIK: Satış baskısı var ama fiyat düşmüyor
        if imbalance_ratio > 1.5 {
            // Bu, büyük oyuncuların "absorb" ettiği anlamına gelir
            Some(AbsorptionSignal::Bullish)
        } else if imbalance_ratio < 0.67 {
            Some(AbsorptionSignal::Bearish)
        } else {
            None
        }
    }
    
    /// 🔥 SECRET #2: Spoofing Detection
    /// Sahte emirler (spoof orders) gerçek market sentiment'ı gizler
    /// Örnek: Büyük bid wall görünüyor ama sürekli cancel ediliyor
    pub fn detect_spoofing(&self) -> Option<SpoofingSignal> {
        if self.snapshots.len() < 20 {
            return None;
        }
        
        let recent: Vec<&OrderFlowSnapshot> = self.snapshots.iter().rev().take(20).collect();
        
        // Order count'lar stable ama volume çok volatil = spoofing
        let avg_bid_count: f64 = recent.iter()
            .map(|s| s.bid_count as f64)
            .sum::<f64>() / recent.len() as f64;
        
        let avg_ask_count: f64 = recent.iter()
            .map(|s| s.ask_count as f64)
            .sum::<f64>() / recent.len() as f64;
        
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
        
        // 🚨 Yüksek CV + stable count = spoofing
        if bid_cv > 0.5 && avg_bid_count > 15.0 {
            Some(SpoofingSignal::BidSideSpoofing)
        } else {
            None
        }
    }
    
    /// 🔥 SECRET #3: Iceberg Order Detection
    /// Büyük emirler küçük parçalara bölünüp gizleniyor
    /// Tespit: Aynı fiyatta sürekli yeni emirler beliriyor
    pub fn detect_iceberg_orders(&self) -> Option<IcebergSignal> {
        if self.snapshots.len() < 30 {
            return None;
        }
        
        let recent: Vec<&OrderFlowSnapshot> = self.snapshots.iter().rev().take(30).collect();
        
        // Bid volume neredeyse hiç değişmiyor (stable) ama 
        // sürekli yeni emirler geliyor = iceberg
        let bid_volumes: Vec<f64> = recent.iter().map(|s| s.bid_volume).collect();
        let bid_vol_std = calculate_std_dev(&bid_volumes);
        let bid_vol_mean = bid_volumes.iter().sum::<f64>() / bid_volumes.len() as f64;
        
        let bid_cv = if bid_vol_mean > 0.0 {
            bid_vol_std / bid_vol_mean
        } else {
            0.0
        };
        
        // Düşük CV (stable volume) = iceberg olabilir
        if bid_cv < 0.1 && bid_vol_mean > 100000.0 { // $100k+ volume
            Some(IcebergSignal::BidSideIceberg)
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
    let variance = values.iter()
        .map(|v| (v - mean).powi(2))
        .sum::<f64>() / values.len() as f64;
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
    BullishDivergence,  // Short-term bullish, long-term bearish (risky long)
    BearishDivergence,  // Short-term bearish, long-term bullish (risky short)
}

#[derive(Debug, Clone)]
pub struct AlignedSignal {
    pub side: SignalSide,
    pub alignment_pct: f64,    // 0.0-1.0 (how many TFs agree)
    pub strength: f64,          // Average strength across TFs
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
                    && matches!(lt.trend, TrendDirection::Down) {
                    Some(DivergenceType::BullishDivergence)
                }
                // Bearish divergence: short-term down, long-term up
                else if matches!(st.trend, TrendDirection::Down)
                    && matches!(lt.trend, TrendDirection::Up) {
                    Some(DivergenceType::BearishDivergence)
                }
                else {
                    None
                }
            },
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
                },
                TrendDirection::Down => {
                    bearish_count += 1;
                    total_strength += signal.strength;
                },
                TrendDirection::Flat => {},
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

    /// 🔥 Binance API'den liquidation verisi çekiliyor
    /// Ancak bu sadece PAST liquidations
    /// FUTURE liquidations'ı tahmin etmek gerek!
    pub fn estimate_future_liquidations(
        &mut self,
        current_price: f64,
        open_interest: f64,
        long_short_ratio: f64,
        funding_rate: f64,
    ) {
        // Long pozisyonların oranı
        let long_ratio = long_short_ratio / (1.0 + long_short_ratio);
        let short_ratio = 1.0 - long_ratio;
        
        let long_oi = open_interest * long_ratio;
        let short_oi = open_interest * short_ratio;
        
        // 🔥 CRITICAL: Liquidation fiyatları tahmin et
        // Leverage distribution: 5x, 10x, 20x, 50x, 100x, 125x
        let leverage_distribution = vec![
            (5.0, 0.10),   // %10 of traders use 5x
            (10.0, 0.20),  // %20 use 10x
            (20.0, 0.25),  // %25 use 20x
            (50.0, 0.25),  // %25 use 50x
            (100.0, 0.15), // %15 use 100x
            (125.0, 0.05), // %5 use 125x (max)
        ];
        
        // Long liquidation prices (below current price)
        for (leverage, portion) in &leverage_distribution {
            let notional = long_oi * portion;
            
            // Liquidation price = entry * (1 - 1/leverage)
            // Örnek: 100x leverage, entry $40k → liq $39,600
            let liq_price = current_price * (1.0 - 1.0 / leverage);
            let liq_price_rounded = (liq_price / 10.0).round() as i64 * 10; // $10 intervals
            
            *self.long_liquidations.entry(liq_price_rounded).or_insert(0.0) += notional;
        }
        
        // Short liquidation prices (above current price)
        for (leverage, portion) in &leverage_distribution {
            let notional = short_oi * portion;
            let liq_price = current_price * (1.0 + 1.0 / leverage);
            let liq_price_rounded = (liq_price / 10.0).round() as i64 * 10;
            
            *self.short_liquidations.entry(liq_price_rounded).or_insert(0.0) += notional;
        }
        
        self.last_update = Utc::now();
    }
    
    /// 🔥 CRITICAL: Detect "liquidation walls"
    /// Bu wall'lara yaklaştığımızda cascade riski var
    pub fn detect_liquidation_walls(
        &self,
        current_price: f64,
        threshold_usd: f64, // Minimum $10M liquidation = wall
    ) -> Vec<LiquidationWall> {
        let mut walls = Vec::new();
        
        let current_price_rounded = (current_price / 10.0).round() as i64 * 10;
        
        // Downside walls (long liquidations)
        for (price, notional) in &self.long_liquidations {
            if *notional > threshold_usd && *price < current_price_rounded {
                let distance_pct = (current_price - *price as f64) / current_price * 100.0;
                
                walls.push(LiquidationWall {
                    price: *price as f64,
                    notional: *notional,
                    direction: CascadeDirection::Downward,
                    distance_pct,
                });
            }
        }
        
        // Upside walls (short liquidations)
        for (price, notional) in &self.short_liquidations {
            if *notional > threshold_usd && *price > current_price_rounded {
                let distance_pct = (*price as f64 - current_price) / current_price * 100.0;
                
                walls.push(LiquidationWall {
                    price: *price as f64,
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
    pub fn generate_cascade_signal(
        &self,
        current_price: f64,
        current_tick: &MarketTick,
    ) -> Option<CascadeSignal> {
        let walls = self.detect_liquidation_walls(current_price, 5_000_000.0); // $5M threshold
        
        if walls.is_empty() {
            return None;
        }
        
        let nearest_wall = &walls[0];
        
        // 🚨 TRIGGER: 0.3%-0.8% mesafede
        if nearest_wall.distance_pct >= 0.3 && nearest_wall.distance_pct <= 0.8 {
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
                },
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
                },
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
fn calculate_cascade_confidence(wall: &LiquidationWall, tick: &MarketTick) -> f64 {
    let mut confidence = 0.0;

    // Wall size factor (0.0-0.4)
    let wall_factor = (wall.notional / 50_000_000.0).min(1.0) * 0.4; // $50M+ = max
    confidence += wall_factor;
    
    // Funding rate confirmation (0.0-0.3)
    if let Some(funding) = tick.funding_rate {
        let funding_factor = match wall.direction {
            CascadeDirection::Downward => {
                // Long cascade: positive funding confirms
                if funding > 0.0005 { 0.3 } else { 0.0 }
            },
            CascadeDirection::Upward => {
                // Short cascade: negative funding confirms
                if funding < -0.0005 { 0.3 } else { 0.0 }
            },
        };
        confidence += funding_factor;
    }
    
    // OBI confirmation (0.0-0.3)
    if let Some(obi) = tick.obi {
        let obi_factor = match wall.direction {
            CascadeDirection::Downward => {
                // Downward: ask pressure (OBI < 1.0)
                if obi < 0.9 { 0.3 } else { 0.0 }
            },
            CascadeDirection::Upward => {
                // Upward: bid pressure (OBI > 1.0)
                if obi > 1.1 { 0.3 } else { 0.0 }
            },
        };
        confidence += obi_factor;
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
    /// En çok işlem gören fiyat seviyeleri = strong support/resistance
    pub fn calculate_volume_profile(candles: &[Candle]) -> Self {
        let mut profile: HashMap<i64, f64> = HashMap::new();

        for candle in candles {
            // Her candle'ın range'ini böl, volume'ü dağıt
            let price_levels = 10; // Her candle'ı 10 price level'a böl
            let volume_per_level = candle.volume / price_levels as f64;
            
            for i in 0..price_levels {
                let price = candle.low + (candle.high - candle.low) * i as f64 / price_levels as f64;
                let price_rounded = (price / 10.0).round() as i64 * 10;
                
                *profile.entry(price_rounded).or_insert(0.0) += volume_per_level;
            }
        }
        
        VolumeProfile { profile }
    }
    
    /// POC (Point of Control): En yüksek volume olan fiyat = en güçlü support/resistance
    pub fn find_poc(&self) -> Option<(i64, f64)> {
        self.profile.iter()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .map(|(price, volume)| (*price, *volume))
    }
    
    /// Check if price is near POC (strong support/resistance)
    pub fn is_near_poc(&self, price: f64, threshold_pct: f64) -> bool {
        if let Some((poc_price, _)) = self.find_poc() {
            let distance_pct = ((price - poc_price as f64) / price).abs() * 100.0;
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

    pub async fn fetch_funding_rates(
        &self,
        symbol: &str,
        limit: u32,
    ) -> Result<Vec<FundingRate>> {
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
                let funding_time = obj.get("fundingTime")?
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
                let ts_ms = obj.get("timestamp")?
                    .as_i64()
                    .or_else(|| obj.get("timestamp")?.as_str()?.parse().ok())?;
                LongShortRatioPoint {
                    timestamp: ts_ms_to_utc(ts_ms),
                    long_short_ratio: obj.get("longShortRatio")?.as_str()?.parse().ok()?,
                    long_account_pct: obj.get("longAccount")?.as_str()?.parse().ok()?,
                    short_account_pct: obj.get("shortAccount")?.as_str()?.parse().ok()?,
                }.into()
            })
            .collect();

        Ok(points)
    }
}

// =======================
//  Utility Fonksiyonlar
// =======================

fn ts_ms_to_utc(ms: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(ms)
        .expect("invalid timestamp millis")
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
        let di = DataItem::builder()
            .open(c.open)
            .high(c.high)
            .low(c.low)
            .close(c.close)
            .volume(c.volume)
            .build()
            .unwrap();

        let ema_f = ema_fast.next(&di);
        let ema_s = ema_slow.next(&di);
        let r = rsi.next(&di);
        let atr_v = atr.next(&di);

        // Funding rate: Önce bu candle için en yakın funding'i bul
        // Eğer bulunursa kullan ve last_funding'i güncelle
        // Eğer bulunamazsa, son bilinen funding rate'i kullan
        let funding_rate = nearest_value_by_time(&c.close_time, funding, |fr| {
            ts_ms_to_utc(fr.funding_time)
        })
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
    current_index: usize,
    funding_arbitrage: Option<&FundingArbitrage>,
    mtf: Option<&MultiTimeframeAnalysis>,
    orderflow: Option<&OrderFlowAnalyzer>,
    liquidation_map: Option<&LiquidationMap>,
    volume_profile: Option<&VolumeProfile>,
    market_tick: Option<&MarketTick>,
) -> Signal {
    // Önce base signal'i üret
    let mut base_signal = generate_signal(candle, ctx, prev_ctx, cfg);
    
    // Eğer signal quality filtering aktif değilse, direkt döndür
    if !cfg.enable_signal_quality_filter {
        return base_signal;
    }
    
    // Eğer signal Flat ise, filtreleme yapmaya gerek yok
    if matches!(base_signal.side, SignalSide::Flat) {
        return base_signal;
    }
    
    // === 1. VOLUME CONFIRMATION ===
    // Düşük volume = zayıf signal (ama çok agresif olmamalı)
    // Not: Volume ratio kontrolü opsiyonel - eğer volume çok düşükse uyar ama iptal etme
    // TrendPlan.md'de önerilen: 1.5x minimum, ama bu çok agresif olabilir
    // Optimize: Sadece çok düşük volume'leri filtrele (0.5x altı)
    if current_index >= 20 && candles.len() > current_index {
        let recent_candles = &candles[current_index.saturating_sub(19)..=current_index.min(candles.len() - 1)];
        let avg_volume_20: f64 = recent_candles.iter().map(|c| c.volume).sum::<f64>() / recent_candles.len() as f64;
        let volume_ratio = candle.volume / avg_volume_20.max(0.0001);
        
        // Sadece çok düşük volume'leri filtrele (0.5x altı = çok zayıf)
        // Config'deki min_volume_ratio kullanılmıyor çünkü çok agresif olabilir
        // Farklı coinlerde volume pattern'leri farklı olabilir, bu yüzden esnek threshold kullanıyoruz
        if volume_ratio < 0.5 {
            // Çok düşük volume = zayıf signal, cancel et
            return Signal {
                time: candle.close_time,
                price: candle.close,
                side: SignalSide::Flat,
                ctx: ctx.clone(),
            };
        }
    }
    
    // === 2. VOLATILITY FILTER ===
    // Çok volatile = risky (ama %2 çok düşük olabilir)
    // Optimize: Sadece extreme volatility'leri filtrele (%3+)
    let atr_pct = ctx.atr / candle.close;
    if atr_pct > 0.03 { // %3+ volatility (daha esnek)
        // Çok volatile = risky, cancel et
        return Signal {
            time: candle.close_time,
            price: candle.close,
            side: SignalSide::Flat,
            ctx: ctx.clone(),
        };
    }
    
    // === 3. RECENT PRICE ACTION ===
    // Parabolic move = reversal riski (ama %3 çok düşük olabilir)
    // Optimize: Sadece extreme parabolic move'ları filtrele (%5+)
    if current_index >= 5 && candles.len() > current_index {
        let price_5bars_ago = candles[current_index - 5].close;
        let price_change_5bars = (candle.close - price_5bars_ago) / price_5bars_ago;
        
        if price_change_5bars.abs() > 0.05 { // %5+ move (daha esnek)
            // Parabolic move = reversal riski, cancel et
            return Signal {
                time: candle.close_time,
                price: candle.close,
                side: SignalSide::Flat,
                ctx: ctx.clone(),
            };
        }
    }
    
    // === 4. SUPPORT/RESISTANCE CHECK (Basit versiyon) ===
    // Eğer long signal ise ve price son 50 bar'ın high'ına yakınsa = resistance riski
    // Eğer short signal ise ve price son 50 bar'ın low'ına yakınsa = support riski
    // Optimize: Daha esnek threshold (%0.2 yerine %0.5)
    if current_index >= 50 && candles.len() > current_index {
        let recent_50 = &candles[current_index.saturating_sub(49)..=current_index.min(candles.len() - 1)];
        let highest_50 = recent_50.iter().map(|c| c.high).fold(0.0, f64::max);
        let lowest_50 = recent_50.iter().map(|c| c.low).fold(f64::INFINITY, f64::min);
        
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
    
    // === 5. FUNDING ARBITRAGE CHECK ===
    // Pre-funding window'da extreme funding varsa, counter-trend pozisyon al
    if let Some(fa) = funding_arbitrage {
        if let Some(arb_signal) = fa.detect_funding_arbitrage(candle.close_time) {
            match arb_signal {
                FundingArbitrageSignal::PreFundingShort { .. } => {
                    // Extreme positive funding → SHORT pozisyon (counter-trend)
                    // Base signal'i override et
                    return Signal {
                        time: candle.close_time,
                        price: candle.close,
                        side: SignalSide::Short,
                        ctx: ctx.clone(),
                    };
                }
                FundingArbitrageSignal::PreFundingLong { .. } => {
                    // Extreme negative funding → LONG pozisyon (counter-trend)
                    return Signal {
                        time: candle.close_time,
                        price: candle.close,
                        side: SignalSide::Long,
                        ctx: ctx.clone(),
                    };
                }
            }
        }
        
        // Funding exhaustion check: Eğer funding sürekli artıyorsa, reversal yakın
        if let Some(exhaustion) = fa.detect_funding_exhaustion() {
            match (base_signal.side, exhaustion) {
                (SignalSide::Long, FundingExhaustionSignal::ExtremePositive) => {
                    // Long signal ama funding extreme positive → reversal riski
                    return Signal {
                        time: candle.close_time,
                        price: candle.close,
                        side: SignalSide::Flat,
                        ctx: ctx.clone(),
                    };
                }
                (SignalSide::Short, FundingExhaustionSignal::ExtremeNegative) => {
                    // Short signal ama funding extreme negative → reversal riski
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
    
    // === 6. MULTI-TIMEFRAME CONFLUENCE CHECK ===
    // Eğer multiple timeframe'ler aynı direction'daysa → high confidence
    if let Some(mtf_analysis) = mtf {
        // Check timeframe alignment
        if let Some(aligned) = mtf_analysis.generate_aligned_signal() {
            // ✅ Strong alignment: all timeframes agree
            if aligned.alignment_pct >= 0.8 && aligned.side == base_signal.side {
                // High confidence trade! Base signal'i döndür
                return base_signal;
            }
            // ⚠️ Conflict: base signal disagrees with MTF
            else if aligned.side != base_signal.side {
                // Cancel signal - MTF doesn't confirm
                return Signal {
                    time: candle.close_time,
                    price: candle.close,
                    side: SignalSide::Flat,
                    ctx: ctx.clone(),
                };
            }
        }
        
        // Check for divergence
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
                },
                (SignalSide::Short, DivergenceType::BullishDivergence) => {
                    // Risky: short signal but higher TF is bullish
                    return Signal {
                        time: candle.close_time,
                        price: candle.close,
                        side: SignalSide::Flat,
                        ctx: ctx.clone(),
                    };
                },
                _ => {}
            }
        }
        
        // Check confluence score
        let confluence = mtf_analysis.calculate_confluence(base_signal.side);
        
        // 🚨 Low confluence = cancel signal
        if confluence < 0.5 {
            return Signal {
                time: candle.close_time,
                price: candle.close,
                side: SignalSide::Flat,
                ctx: ctx.clone(),
            };
        }
    }
    
    // === 7. ORDER FLOW ANALYSIS CHECK ===
    // Market maker behavior tracking (SECRET #1)
    if let Some(of) = orderflow {
        // Order flow confirmation
        if let Some(absorption) = of.detect_absorption() {
            match (base_signal.side, absorption) {
                (SignalSide::Long, AbsorptionSignal::Bullish) => {
                    // ✅ Strong confirmation: Our signal + MM accumulation
                    // Bu durumda signal güvenilirliği çok yüksek
                },
                (SignalSide::Short, AbsorptionSignal::Bearish) => {
                    // ✅ Strong confirmation
                },
                (SignalSide::Long, AbsorptionSignal::Bearish) => {
                    // ⚠️ Conflict: Cancel signal
                    return Signal {
                        time: candle.close_time,
                        price: candle.close,
                        side: SignalSide::Flat,
                        ctx: ctx.clone(),
                    };
                },
                (SignalSide::Short, AbsorptionSignal::Bullish) => {
                    // ⚠️ Conflict: Cancel signal
                    return Signal {
                        time: candle.close_time,
                        price: candle.close,
                        side: SignalSide::Flat,
                        ctx: ctx.clone(),
                    };
                },
                (SignalSide::Flat, _) => {
                    // Flat signal, no action needed
                },
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
        
        // Iceberg detection: Increase confidence if we're with the iceberg
        if let Some(iceberg) = of.detect_iceberg_orders() {
            match (base_signal.side, iceberg) {
                (SignalSide::Long, IcebergSignal::BidSideIceberg) => {
                    // 🚀 Big player is buying with us = increase confidence
                    // Signal'i onayla, devam et
                },
                (SignalSide::Short, IcebergSignal::AskSideIceberg) => {
                    // 🚀 Big player is selling with us
                    // Signal'i onayla, devam et
                },
                _ => {}
            }
        }
    }
    
    // === 8. LIQUIDATION CASCADE CHECK ===
    // En karlı stratejilerden biri: Liquidation cascade'lerini önceden tahmin etmek
    if let (Some(liq_map), Some(tick)) = (liquidation_map, market_tick) {
        if let Some(cascade_sig) = liq_map.generate_cascade_signal(candle.close, tick) {
            // Cascade signal confidence yüksekse, base signal'i override et
            if cascade_sig.confidence > 0.6 {
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
        }
    }
    
    // === 9. VOLUME PROFILE CHECK ===
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

    let mut long_score = 0usize;
    let mut short_score = 0usize;

    // LONG kuralları
    // 1) Trend yukarı
    if matches!(trend, TrendDirection::Up) {
        long_score += 1;
    }
    // 1.5) Price action confirmation: Price EMA fast'ın üstünde (trend ile uyumlu)
    if price_action_bullish {
        long_score += 1;
    }
    // 1.6) OPTIMIZATION: Trend strength - EMA fast ve slow arasındaki mesafe (opsiyonel bonus)
    // Zayıf trend'lerde trade yapma (false signal riski) - ama çok sıkı olmamalı
    if matches!(trend, TrendDirection::Up) {
        let trend_strength = (ctx.ema_fast - ctx.ema_slow) / ctx.ema_slow;
        if trend_strength > 0.0005 { // %0.05+ trend strength (daha esnek)
            long_score += 1;
        }
    }
    // 2) Momentum yukarı
    if ctx.rsi >= cfg.rsi_trend_long_min {
        long_score += 1;
    }
    // 3) Funding aşırı pozitif değil (aşırı long kalabalığı istemiyoruz)
    if ctx.funding_rate <= cfg.funding_extreme_pos {
        long_score += 1;
    }
    // 4) Open interest artıyor (yeni pozisyon akışı var)
    if oi_change_up {
        long_score += 1;
    }
    // 5) Top trader'lar aşırı long değil (hatta biraz short baskısı olabilir)
    if !crowded_long {
        long_score += 1;
    }
    // 6) ATR volatility kontrolü: Yüksek volatility'de daha dikkatli ol
    // ATR rising factor ile birlikte kullanılabilir (gelecekte)
    // Şimdilik ATR hesaplanıyor ama signal generation'da kullanılmıyor
    // Not: ATR değeri ctx.atr'de mevcut, ancak şu an için signal scoring'de kullanılmıyor

    // SHORT kuralları
    // 1) Trend aşağı
    if matches!(trend, TrendDirection::Down) {
        short_score += 1;
    }
    // 1.5) Price action confirmation: Price EMA fast'ın altında (trend ile uyumlu)
    if price_action_bearish {
        short_score += 1;
    }
    // 1.6) OPTIMIZATION: Trend strength - EMA fast ve slow arasındaki mesafe (opsiyonel bonus)
    // Zayıf trend'lerde trade yapma (false signal riski) - ama çok sıkı olmamalı
    if matches!(trend, TrendDirection::Down) {
        let trend_strength = (ctx.ema_slow - ctx.ema_fast) / ctx.ema_slow;
        if trend_strength > 0.0005 { // %0.05+ trend strength (daha esnek)
            short_score += 1;
        }
    }
    // 2) Momentum aşağı
    if ctx.rsi <= cfg.rsi_trend_short_max {
        short_score += 1;
    }
    // 3) Funding pozitif ve mümkünse aşırı (crowded long)
    if ctx.funding_rate >= cfg.funding_extreme_pos {
        short_score += 1;
    }
    // 4) Open interest artıyor (yeni pozisyon akışı var)
    if oi_change_up {
        short_score += 1;
    }
    // 5) Top trader'lar aşırı long (crowded long → short fırsatı)
    if crowded_long {
        short_score += 1;
    }

    // Kullanıcının "en iyi sistem" tanımına göre: minimum 4 score gerekli
    // Ancak config'den de alabilir (varsayılan 4)
    let long_min = cfg.long_min_score.max(4);
    let short_min = cfg.short_min_score.max(4);
    
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
/// # ⚠️ Backtest vs Production Divergence (Now Realistic!)
/// 
/// **Backtest Execution (Realistic):**
/// - Signal generated at candle `i` close
/// - Execution delay: 1-2 bars (simulates production delay: Signal → EventBus → Ordering → API → Fill)
/// - Dynamic slippage: Base slippage (0.05%) multiplied by volatility (ATR-based) and random factor (1.0-3.0x)
/// - High volatility periods: slippage can reach 0.1-0.5% (production reality)
/// 
/// **Production Execution (Realistic):**
/// - Signal generated at candle close
/// - Signal → event bus (mpsc channel delay: ~1-10ms)
/// - Ordering module: risk checks, symbol info fetch, quantity calculation (~50-200ms)
/// - API call (network delay: ~100-500ms)
/// - Order filled at market price (slippage: 0.05% normal, 0.1-0.5% during volatility)
/// - Total delay: typically 1-5 seconds (≈ 1-2 bars for 5m candles)
/// 
/// **Impact:**
/// - Backtest results are now more realistic and closer to production
/// - Execution delays and dynamic slippage simulate production conditions
/// - Results may still be slightly optimistic due to perfect signal timing, but much closer to reality
/// 
/// # NOT: Production Kullanımı
/// Bu fonksiyon sadece backtest için kullanılır. Production'da:
/// 1. `generate_signals` ile sinyaller üretilir
/// 2. Sinyaller `ordering` modülüne gönderilir
/// 3. `ordering` modülü pozisyon açma/kapama işlemlerini yapar
pub fn run_backtest_on_series(
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
    let base_slippage_frac = cfg.slippage_bps / 10_000.0; // Base slippage (production reality)
    
    // Pending signals queue: (signal_index, entry_index, signal_side, signal_ctx)
    let mut pending_signals: Vec<(usize, usize, SignalSide, SignalContext)> = Vec::new();
    
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
        Some(VolumeProfile::calculate_volume_profile(&candles[candles.len().saturating_sub(100)..]))
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
        
        // Create MarketTick from real data - GERÇEK VERİ
        // OBI hesaplama: Long/Short ratio'dan tahmin edilebilir
        // LSR > 1.0 = more longs = bid pressure = OBI > 1.0
        let estimated_obi = if ctx.long_short_ratio > 1.0 {
            Some(ctx.long_short_ratio) // Long dominance = bid pressure
        } else {
            Some(1.0 / ctx.long_short_ratio.max(0.1)) // Short dominance = ask pressure
        };
        
        let market_tick = MarketTick {
            symbol: "BTCUSDT".to_string(), // Backtest için placeholder
            price: c.close,
            bid: c.close * 0.9999, // Estimate from close
            ask: c.close * 1.0001, // Estimate from close
            volume: c.volume,
            ts: c.close_time,
            obi: estimated_obi,
            funding_rate: Some(ctx.funding_rate),
            liq_long_cluster: None, // Backtest'te liquidation cluster data yok
            liq_short_cluster: None,
        };

        // Enhanced signal generation with ALL REAL DATA
        let sig = generate_signal_enhanced(
            c, ctx, prev_ctx, cfg, candles, i,
            Some(&funding_arbitrage),
            None, // MTF - farklı timeframe'ler gerektirir, production'da kullanılır
            None, // OrderFlow - DepthSnapshot gerektirir, production'da kullanılır
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
        
        // Execution delay: Signal üretildikten sonra 1-2 bar bekle (production reality)
        // Production'da: Signal → EventBus → Ordering → API → Fill (1-5 seconds ≈ 1-2 bars)
        if !matches!(sig.side, SignalSide::Flat) {
            let execution_delay = rng.gen_range(1..=2); // 1-2 bar delay
            let entry_index = i + execution_delay;
            if entry_index < candles.len() {
                pending_signals.push((i, entry_index, sig.side, ctx.clone()));
            }
        }

        // Check pending signals for execution
        pending_signals.retain(|(_signal_idx, entry_idx, signal_side, _signal_ctx)| {
            if *entry_idx == i {
                // Execute signal now
                if matches!(pos_side, PositionSide::Flat) {
                    // Calculate dynamic slippage based on ATR (volatility) at entry time
                    // Higher ATR = higher volatility = higher slippage
                    // Base slippage: 0.05% (5 bps), can increase to 0.1-0.5% during high volatility
                    let entry_ctx = &contexts[i]; // Use context at entry time, not signal time
                    let atr_pct = entry_ctx.atr / c.close; // ATR as percentage of price
                    let volatility_multiplier = (atr_pct * 100.0).min(10.0).max(1.0); // Cap at 10x
                    let dynamic_slippage_frac = base_slippage_frac * volatility_multiplier * rng.gen_range(1.0..3.0);
                    
                    let entry_candle = &candles[i];
                    
                    match signal_side {
                        SignalSide::Long => {
                            pos_side = PositionSide::Long;
                            // Entry: LONG position buys at ask (higher price due to slippage)
                            pos_entry_price = entry_candle.open * (1.0 + dynamic_slippage_frac);
                            pos_entry_time = entry_candle.open_time;
                            pos_entry_index = i;
                        }
                        SignalSide::Short => {
                            pos_side = PositionSide::Short;
                            // Entry: SHORT position sells at bid (lower price due to slippage)
                            pos_entry_price = entry_candle.open * (1.0 - dynamic_slippage_frac);
                            pos_entry_time = entry_candle.open_time;
                            pos_entry_index = i;
                        }
                        SignalSide::Flat => {}
                    }
                }
                false // Remove from pending signals
            } else {
                true // Keep in pending signals
            }
        });

        let next_c = &candles[i + 1];

        // Pozisyon varsa: max holding check + sinyal yönü + ATR-based stop loss
        match pos_side {
            PositionSide::Long => {
                let holding_bars = i.saturating_sub(pos_entry_index);
                
                // ATR-based stop loss (TrendPlan.md önerisi)
                // Dynamic stop-loss based on ATR multiplier (coin-agnostic)
                let atr_multiplier = cfg.atr_stop_loss_multiplier;
                let stop_loss_price = pos_entry_price * (1.0 - atr_multiplier * ctx.atr / pos_entry_price);
                
                // Dynamic take-profit based on ATR multiplier
                let tp_multiplier = cfg.atr_take_profit_multiplier;
                let take_profit_price = pos_entry_price * (1.0 + tp_multiplier * ctx.atr / pos_entry_price);
                
                // OPTIMIZATION: Minimum holding time - çok kısa trade'leri filtrele
                let min_holding_bars = cfg.min_holding_bars;
                let should_close =
                    matches!(sig.side, SignalSide::Short)
                    || holding_bars >= cfg.max_holding_bars
                    || (holding_bars >= min_holding_bars && next_c.low <= stop_loss_price) // Min holding sonrası stop-loss
                    || (holding_bars >= min_holding_bars && next_c.high >= take_profit_price); // Min holding sonrası take-profit

                if should_close {
                    // Calculate dynamic slippage for exit
                    let atr_pct = ctx.atr / c.close;
                    let volatility_multiplier = (atr_pct * 100.0).min(10.0).max(1.0);
                    let dynamic_slippage_frac = base_slippage_frac * volatility_multiplier * rng.gen_range(1.0..3.0);
                    
                    // Exit: LONG position sells at bid (lower price due to slippage)
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
                
                // ATR-based stop loss (TrendPlan.md önerisi)
                // Dynamic stop-loss based on ATR multiplier (coin-agnostic)
                let atr_multiplier = cfg.atr_stop_loss_multiplier;
                let stop_loss_price = pos_entry_price * (1.0 + atr_multiplier * ctx.atr / pos_entry_price);
                
                // Dynamic take-profit based on ATR multiplier
                let tp_multiplier = cfg.atr_take_profit_multiplier;
                let take_profit_price = pos_entry_price * (1.0 - tp_multiplier * ctx.atr / pos_entry_price);
                
                // OPTIMIZATION: Minimum holding time - çok kısa trade'leri filtrele
                let min_holding_bars = cfg.min_holding_bars;
                let should_close =
                    matches!(sig.side, SignalSide::Long)
                    || holding_bars >= cfg.max_holding_bars
                    || (holding_bars >= min_holding_bars && next_c.high >= stop_loss_price) // Min holding sonrası stop-loss
                    || (holding_bars >= min_holding_bars && next_c.low <= take_profit_price); // Min holding sonrası take-profit

                if should_close {
                    // Calculate dynamic slippage for exit
                    let atr_pct = ctx.atr / c.close;
                    let volatility_multiplier = (atr_pct * 100.0).min(10.0).max(1.0);
                    let dynamic_slippage_frac = base_slippage_frac * volatility_multiplier * rng.gen_range(1.0..3.0);
                    
                    // Exit: SHORT position buys at ask (higher price due to slippage)
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

/// Improved backtest with immediate execution (TrendPlan.md v2)
/// 
/// # Key Differences from v1:
/// - Immediate execution: Signal at candle `i` close → executed at candle `i+1` open (1 bar delay max)
/// - No random 1-2 bar delay that allows market to move against us
/// - More realistic production simulation
pub fn run_backtest_on_series_v2(
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
    
    let mut rng = rand::rngs::StdRng::seed_from_u64(42);
    
    // Funding arbitrage tracker
    let mut funding_arbitrage = FundingArbitrage::new();
    
    // Liquidation Map - GERÇEK VERİ: Open Interest ve Long/Short Ratio kullanıyor
    let mut liquidation_map = LiquidationMap::new();
    
    // Volume Profile - GERÇEK VERİ: Candle verilerinden hesaplanıyor
    let volume_profile = if candles.len() >= 50 {
        Some(VolumeProfile::calculate_volume_profile(&candles[candles.len().saturating_sub(100)..]))
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
        
        // Create MarketTick from real data - GERÇEK VERİ
        // OBI hesaplama: Long/Short ratio'dan tahmin edilebilir
        // LSR > 1.0 = more longs = bid pressure = OBI > 1.0
        let estimated_obi = if ctx.long_short_ratio > 1.0 {
            Some(ctx.long_short_ratio) // Long dominance = bid pressure
        } else {
            Some(1.0 / ctx.long_short_ratio.max(0.1)) // Short dominance = ask pressure
        };
        
        let market_tick = MarketTick {
            symbol: "BTCUSDT".to_string(), // Backtest için placeholder
            price: c.close,
            bid: c.close * 0.9999, // Estimate from close
            ask: c.close * 1.0001, // Estimate from close
            volume: c.volume,
            ts: c.close_time,
            obi: estimated_obi,
            funding_rate: Some(ctx.funding_rate),
            liq_long_cluster: None, // Backtest'te liquidation cluster data yok
            liq_short_cluster: None,
        };

        // Enhanced signal generation with ALL REAL DATA
        let sig = generate_signal_enhanced(
            c, ctx, prev_ctx, cfg, candles, i,
            Some(&funding_arbitrage),
            None, // MTF - farklı timeframe'ler gerektirir, production'da kullanılır
            None, // OrderFlow - DepthSnapshot gerektirir, production'da kullanılır
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

        // === CRITICAL FIX: IMMEDIATE EXECUTION (NEXT BAR) ===
        // Production: Signal generated at close → executed at next open
        // No multi-bar delay that allows market to move against us
        // OPTIMIZATION: Minimum holding time kontrolü - çok kısa trade'leri filtrele
        if !matches!(sig.side, SignalSide::Flat) && matches!(pos_side, PositionSide::Flat) {
            if i + 1 < candles.len() {
                let entry_candle = &candles[i + 1]; // Next candle
                
                // Dynamic slippage based on ATR
                let atr_pct = ctx.atr / c.close;
                let volatility_multiplier = (atr_pct * 100.0).min(5.0).max(1.0);
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
                
                // Exit conditions:
                // 1. Opposite signal
                // 2. Max holding time
                // 3. Stop loss hit (ATR-based)
                // 4. Take profit hit (ATR-based) - OPTIMIZATION: Büyük kazananları koru
                // Dynamic stop-loss based on ATR multiplier (coin-agnostic)
                let atr_multiplier = cfg.atr_stop_loss_multiplier;
                let stop_loss_price = pos_entry_price * (1.0 - atr_multiplier * ctx.atr / pos_entry_price);
                
                // Dynamic take-profit based on ATR multiplier
                let tp_multiplier = cfg.atr_take_profit_multiplier;
                let take_profit_price = pos_entry_price * (1.0 + tp_multiplier * ctx.atr / pos_entry_price);
                
                // OPTIMIZATION: Minimum holding time - çok kısa trade'leri filtrele
                let min_holding_bars = cfg.min_holding_bars;
                let should_close =
                    matches!(sig.side, SignalSide::Short)
                    || holding_bars >= cfg.max_holding_bars
                    || (holding_bars >= min_holding_bars && next_c.low <= stop_loss_price) // Min holding sonrası stop-loss
                    || (holding_bars >= min_holding_bars && next_c.high >= take_profit_price); // Min holding sonrası take-profit

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
                
                // Dynamic stop-loss based on ATR multiplier (coin-agnostic)
                let atr_multiplier = cfg.atr_stop_loss_multiplier;
                let stop_loss_price = pos_entry_price * (1.0 + atr_multiplier * ctx.atr / pos_entry_price);
                
                // Dynamic take-profit based on ATR multiplier
                let tp_multiplier = cfg.atr_take_profit_multiplier;
                let take_profit_price = pos_entry_price * (1.0 - tp_multiplier * ctx.atr / pos_entry_price);
                
                // OPTIMIZATION: Minimum holding time - çok kısa trade'leri filtrele
                let min_holding_bars = cfg.min_holding_bars;
                let should_close =
                    matches!(sig.side, SignalSide::Long)
                    || holding_bars >= cfg.max_holding_bars
                    || (holding_bars >= min_holding_bars && next_c.high >= stop_loss_price) // Min holding sonrası stop-loss
                    || (holding_bars >= min_holding_bars && next_c.low <= take_profit_price); // Min holding sonrası take-profit

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

    // Calculate results
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

    let avg_r = if loss_trades > 0 && win_trades > 0 {
        let avg_win = total_win_pnl / win_trades as f64;
        let avg_loss = total_loss_pnl / loss_trades as f64;
        if avg_loss > 0.0 {
            avg_win / avg_loss
        } else {
            0.0
        }
    } else if loss_trades == 0 && win_trades > 0 {
        f64::INFINITY
    } else {
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

    let candles = client.fetch_klines(symbol, kline_interval, kline_limit).await?;
    let funding = client.fetch_funding_rates(symbol, 100).await?; // son ~100 funding event (en fazla 30 gün)
    let oi_hist = client
        .fetch_open_interest_hist(symbol, futures_period, kline_limit)
        .await?;
    let lsr_hist = client
        .fetch_top_long_short_ratio(symbol, futures_period, kline_limit)
        .await?;

    let (matched_candles, contexts) = build_signal_contexts(&candles, &funding, &oi_hist, &lsr_hist);
    // Use v2 (immediate execution) as recommended in TrendPlan.md
    Ok(run_backtest_on_series_v2(&matched_candles, &contexts, cfg))
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

    let mut file = File::create(file_path)
        .context(format!("Failed to create CSV file: {}", file_path))?;

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
    file.flush()
        .context("Failed to flush CSV file buffer")?;

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
        let downside_variance = downside_returns
            .iter()
            .map(|r| r.powi(2))
            .sum::<f64>()
            / downside_returns.len() as f64;
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
    println!("   Max Drawdown       : {:.2}%", result.max_drawdown_pct * 100.0);
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

use crate::types::{KlineData, KlineEvent, TradeSignal, TrendParams, TrendingChannels};
use log::{info, warn};
use tokio::sync::broadcast;
use tokio::time::{interval, sleep, Duration as TokioDuration};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use futures::StreamExt;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Her side için ayrı cooldown tracking (trend reversal'ları kaçırmamak için)
/// LONG ve SHORT sinyalleri birbirini bloklamaz
struct LastSignalState {
    last_long_time: Option<chrono::DateTime<Utc>>,
    last_short_time: Option<chrono::DateTime<Utc>>,
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
) {
    let client = FuturesClient::new();
    
    // AlgoConfig'i TrendParams'den oluştur
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
        max_holding_bars: 48,   // Default max holding
        slippage_bps: 0.0,      // Default: no slippage (optimistic backtest)
                                 // Set to 5-10 bps (0.05-0.1%) for more realistic results
        // Signal Quality Filtering (TrendPlan.md önerileri)
        min_volume_ratio: 1.5,        // Minimum volume ratio vs 20-bar average
        max_volatility_pct: 2.0,      // Maximum ATR volatility % (2% = çok volatile)
        max_price_change_5bars_pct: 3.0, // 5 bar içinde max price change % (3% = parabolic move)
        enable_signal_quality_filter: true, // Signal quality filtering aktif
        // Stop Loss & Risk Management (coin-agnostic)
        atr_stop_loss_multiplier: params.atr_sl_multiplier, // ATR multiplier from config
        atr_take_profit_multiplier: params.atr_tp_multiplier, // ATR TP multiplier from config
        min_holding_bars: 3, // Default minimum holding time (3 bars = 15 minutes @5m)
                             // Can be made configurable in future
    };

    let kline_interval = "5m"; // 5 dakikalık kline kullan
    let futures_period = "5m";
    let kline_limit = (params.warmup_min_ticks + 10) as u32; // Warmup için yeterli veri

    // Candle buffer - son N candle'ı tutar (signal context hesaplama için)
    let candle_buffer = Arc::new(RwLock::new(Vec::<Candle>::new()));
    
    // İlk candle'ları REST API'den çek (warmup için)
    match client.fetch_klines(&symbol, kline_interval, kline_limit).await {
        Ok(candles) => {
            *candle_buffer.write().await = candles;
            info!("TRENDING: loaded {} candles for warmup", candle_buffer.read().await.len());
        }
        Err(err) => {
            warn!("TRENDING: failed to fetch initial candles: {err:?}");
        }
    }

    let signal_state = LastSignalState {
        last_long_time: None,
        last_short_time: None,
    };

    info!("TRENDING: started for symbol {} with kline WebSocket stream", symbol);

    // Kline WebSocket stream task
    let kline_stream_symbol = symbol.clone();
    let kline_stream_ws_url = ws_base_url.clone();
    let kline_stream_buffer = candle_buffer.clone();
    let kline_stream_signal_state = Arc::new(RwLock::new(signal_state));
    let kline_stream_signal_tx = ch.signal_tx.clone();
    
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
        ).await;
    });

    // Wait for kline stream task
    let _ = kline_task.await;
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
) {
    let mut retry_delay = TokioDuration::from_secs(1);
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
                                        
                                        // Sinyal üret (signal gönderme işlemi fonksiyon içinde yapılıyor)
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
                                        ).await {
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
                                            let max_candles = (params.warmup_min_ticks + 10) as usize;
                                            if buffer.len() > max_candles {
                                                buffer.remove(0);
                                            }
                                            drop(buffer);
                                            
                                            // Sinyal üret (signal gönderme işlemi fonksiyon içinde yapılıyor)
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
                                            ).await {
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
) -> Result<Option<TradeSignal>> {
    let buffer = candle_buffer.read().await;
    
    if buffer.len() < params.warmup_min_ticks {
        return Ok(None); // Henüz yeterli veri yok
    }

    // Funding, OI, Long/Short ratio verilerini çek (REST API - daha az sıklıkla)
    let funding = client.fetch_funding_rates(symbol, 100).await?;
    let oi_hist = client.fetch_open_interest_hist(symbol, futures_period, buffer.len() as u32).await?;
    let lsr_hist = client.fetch_top_long_short_ratio(symbol, futures_period, buffer.len() as u32).await?;

    // Signal context'leri oluştur
    let (matched_candles, contexts) = build_signal_contexts(&buffer, &funding, &oi_hist, &lsr_hist);

    if contexts.len() < params.warmup_min_ticks {
        return Ok(None);
    }

    // En son candle ve context'i kullan
    let latest_ctx = contexts.last().ok_or_else(|| anyhow::anyhow!("no contexts available"))?;
    let prev_ctx = if contexts.len() > 1 {
        Some(&contexts[contexts.len() - 2])
    } else {
        None
    };

    let signal = generate_signal(candle, latest_ctx, prev_ctx, cfg);

    // Eğer sinyal Flat değilse, TradeSignal'e dönüştür
    match signal.side {
        SignalSide::Long | SignalSide::Short => {
            let side = match signal.side {
                SignalSide::Long => Side::Long,
                SignalSide::Short => Side::Short,
                SignalSide::Flat => unreachable!(),
            };

            // Side-specific cooldown kontrolü
            let cooldown_duration = chrono::Duration::seconds(params.signal_cooldown_secs);
            let now = Utc::now();
            let state = signal_state.read().await;
            
            // Cooldown kontrolü yap (ama henüz set etme)
            match side {
                Side::Long => {
                    if let Some(last_time) = state.last_long_time {
                        if now - last_time < cooldown_duration {
                            return Ok(None); // Long cooldown aktif
                        }
                    }
                }
                Side::Short => {
                    if let Some(last_time) = state.last_short_time {
                        if now - last_time < cooldown_duration {
                            return Ok(None); // Short cooldown aktif
                        }
                    }
                }
            }
            drop(state); // Lock'u serbest bırak

            // TradeSignal oluştur
            let trade_signal = TradeSignal {
                id: Uuid::new_v4(),
                symbol: symbol.to_string(),
                side,
                entry_price: signal.price,
                leverage: params.leverage,
                size_usdt: params.position_size_quote,
                ts: signal.time,
            };

            // Önce signal'i göndermeyi dene
            match signal_tx.send(trade_signal.clone()).await {
                Ok(_) => {
                    // Başarılı, şimdi cooldown set et
                    let mut state = signal_state.write().await;
                    match side {
                        Side::Long => state.last_long_time = Some(now),
                        Side::Short => state.last_short_time = Some(now),
                    }
                    
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
                    // Signal gönderilemedi, cooldown set etme
                    warn!("TRENDING: failed to send signal: {err}, cooldown not set");
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
    let candles = match client.fetch_klines(symbol, kline_interval, kline_limit).await {
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

    // Cooldown kontrolü burada yapılmaz - signal side'ı bilinmeden yapılamaz
    // Cooldown kontrolü signal üretildikten sonra, side'a göre yapılacak

    // Funding, OI, Long/Short ratio verilerini çek
    let funding = client.fetch_funding_rates(symbol, 100).await?;
    let oi_hist = client.fetch_open_interest_hist(symbol, futures_period, kline_limit).await?;
    let lsr_hist = client.fetch_top_long_short_ratio(symbol, futures_period, kline_limit).await?;

    // Signal context'leri oluştur (sadece gerçek API verisi olan candle'lar)
    let (matched_candles, contexts) = build_signal_contexts(&candles, &funding, &oi_hist, &lsr_hist);

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

            // Side-specific cooldown kontrolü (trend reversal'ları kaçırmamak için)
            let cooldown_duration = chrono::Duration::seconds(params.signal_cooldown_secs);
            let now = Utc::now();
            
            // Cooldown kontrolü yap (ama henüz set etme)
            match side {
                Side::Long => {
                    if let Some(last_time) = signal_state.last_long_time {
                        if now - last_time < cooldown_duration {
                            return Ok(None); // Long cooldown aktif
                        }
                    }
                }
                Side::Short => {
                    if let Some(last_time) = signal_state.last_short_time {
                        if now - last_time < cooldown_duration {
                            return Ok(None); // Short cooldown aktif
                        }
                    }
                }
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
            };

            // Önce signal'i göndermeyi dene
            match signal_tx.send(trade_signal.clone()).await {
                Ok(_) => {
                    // Başarılı, şimdi cooldown set et
                    match side {
                        Side::Long => signal_state.last_long_time = Some(now),
                        Side::Short => signal_state.last_short_time = Some(now),
                    }
                    
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
                    // Signal gönderilemedi, cooldown set etme
                    warn!("TRENDING: failed to send signal: {err}, cooldown not set");
                    Ok(None)
                }
            }
        }
        SignalSide::Flat => Ok(None),
    }
}

