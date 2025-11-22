use chrono::{DateTime, Timelike, Utc};
use std::collections::{BTreeMap, HashMap, VecDeque};
use crate::types::{Candle, DepthSnapshot, MarketTick, Side};

#[derive(Debug, Clone)]
struct FundingSnapshot {
    _timestamp: DateTime<Utc>,
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
    ExtremePositive,
    ExtremeNegative,
}

impl FundingArbitrage {
    pub fn new() -> Self {
        Self {
            funding_history: VecDeque::new(),
            next_funding_time: Self::calculate_next_funding_time(Utc::now()),
        }
    }

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

    pub fn update_funding(&mut self, funding_rate: f64, timestamp: DateTime<Utc>) {
        self.funding_history.push_back(FundingSnapshot {
            _timestamp: timestamp,
            funding_rate,
        });
        if self.funding_history.len() > 10 {
            self.funding_history.pop_front();
        }
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

    pub fn detect_funding_arbitrage(
        &self,
        now: DateTime<Utc>,
        current_price: f64,
        price_history: &[(DateTime<Utc>, f64)],
    ) -> Option<FundingArbitrageSignal> {
        if !self.is_pre_funding_window(now) {
            return None;
        }

        if self.funding_history.is_empty() {
            return None;
        }

        let latest_funding = self.funding_history.back()?.funding_rate;

        let pre_funding_start = self.next_funding_time - chrono::Duration::minutes(90);
        let price_at_window_start = price_history
            .iter()
            .find(|(ts, _)| *ts >= pre_funding_start)
            .map(|(_, price)| *price);

        let price_movement_pct = if let Some(start_price) = price_at_window_start {
            if start_price > 0.0 {
                (current_price - start_price) / start_price
            } else {
                0.0
            }
        } else {
            0.0
        };

        let funding_trend = if self.funding_history.len() >= 3 {
            let recent: Vec<&FundingSnapshot> = self.funding_history.iter().rev().take(3).collect();
            let trend = (recent[0].funding_rate - recent[2].funding_rate).signum();
            Some(trend)
        } else {
            None
        };

        if latest_funding > 0.0005 {
            if price_movement_pct > 0.003 {
                log::debug!(
                    "TRENDING: Funding arbitrage SHORT skipped - price already moved {:.2}% (arbitrage priced in)",
                    price_movement_pct * 100.0
                );
                return None;
            }

            let confidence_boost = funding_trend
                .map(|t| if t > 0.0 { 1.2 } else { 1.0 })
                .unwrap_or(1.0);
            
            let price_adjustment = if price_movement_pct < -0.001 {
                1.2
            } else if price_movement_pct > 0.001 {
                0.7
            } else {
                1.0
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
        else if latest_funding < -0.0005 {
            if price_movement_pct < -0.003 {
                log::debug!(
                    "TRENDING: Funding arbitrage LONG skipped - price already moved {:.2}% (arbitrage priced in)",
                    price_movement_pct * 100.0
                );
                return None;
            }

            let confidence_boost = funding_trend
                .map(|t| if t < 0.0 { 1.2 } else { 1.0 })
                .unwrap_or(1.0);
            
            let price_adjustment = if price_movement_pct > 0.001 {
                1.2
            } else if price_movement_pct < -0.001 {
                0.7
            } else {
                1.0
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

#[derive(Debug, Clone)]
struct OrderFlowSnapshot {
    _timestamp: DateTime<Utc>,
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
    Bullish,
    Bearish,
}

#[derive(Debug, Clone, Copy)]
pub enum SpoofingSignal {
    BidSideSpoofing,
    AskSideSpoofing,
}

#[derive(Debug, Clone, Copy)]
pub enum IcebergSignal {
    BidSideIceberg,
    AskSideIceberg,
}

pub(crate) fn calculate_std_dev(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    let variance = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / values.len() as f64;
    variance.sqrt()
}

impl OrderFlowAnalyzer {
    pub fn new(window_size: usize) -> Self {
        Self {
            snapshots: VecDeque::with_capacity(window_size),
            window_size,
        }
    }

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
            _timestamp: Utc::now(),
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

        let snapshot_count = self.snapshots.len().min(25);
        let recent: Vec<&OrderFlowSnapshot> = self.snapshots.iter().rev().take(snapshot_count).collect();

        let avg_bid_count: f64 =
            recent.iter().map(|s| s.bid_count as f64).sum::<f64>() / recent.len() as f64;

        let avg_ask_count: f64 =
            recent.iter().map(|s| s.ask_count as f64).sum::<f64>() / recent.len() as f64;

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

#[derive(Debug, Clone)]
pub struct LiquidationMap {
    pub long_liquidations: BTreeMap<i64, f64>,
    pub short_liquidations: BTreeMap<i64, f64>,
    pub last_update: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct LiquidationWall {
    pub price: f64,
    pub notional: f64,
    pub direction: CascadeDirection,
    pub distance_pct: f64,
}

#[derive(Debug, Clone, Copy)]
pub enum CascadeDirection {
    Downward,
    Upward,
}

#[derive(Debug, Clone)]
pub struct CascadeSignal {
    pub side: Side,
    pub entry_price: f64,
    pub target_price: f64,
    pub expected_pnl_pct: f64,
    pub wall_notional: f64,
    pub confidence: f64,
}

fn calculate_cascade_confidence(wall: &LiquidationWall, tick: &MarketTick) -> f64 {
    let mut confidence = 0.0;

    let wall_factor = (wall.notional / 20_000_000.0).min(1.0) * 0.4;
    confidence += wall_factor;

    if let Some(funding) = tick.funding_rate {
        let funding_factor = match wall.direction {
            CascadeDirection::Downward => {
                if funding > 0.0003 {
                    0.3
                } else if funding > 0.0001 {
                    0.15
                } else {
                    0.0
                }
            }
            CascadeDirection::Upward => {
                if funding < -0.0003 {
                    0.3
                } else if funding < -0.0001 {
                    0.15
                } else {
                    0.0
                }
            }
        };
        confidence += funding_factor;
    }

    if let Some(obi) = tick.obi {
        let obi_factor = match wall.direction {
            CascadeDirection::Downward => {
                if obi < 0.9 {
                    0.3
                } else if obi < 0.95 {
                    0.15
                } else {
                    0.0
                }
            }
            CascadeDirection::Upward => {
                if obi > 1.1 {
                    0.3
                } else if obi > 1.05 {
                    0.15
                } else {
                    0.0
                }
            }
        };
        confidence += obi_factor;
    }

    if wall.notional > 2_000_000.0 {
        confidence += 0.1;
    }

    confidence.min(1.0)
}

impl LiquidationMap {
    pub fn new() -> Self {
        Self {
            long_liquidations: BTreeMap::new(),
            short_liquidations: BTreeMap::new(),
            last_update: Utc::now(),
        }
    }

    pub fn update_from_real_liquidation_data(
        &mut self,
        current_price: f64,
        open_interest: f64,
        liq_long_cluster: Option<f64>,
        liq_short_cluster: Option<f64>,
    ) {
        self.long_liquidations.clear();
        self.short_liquidations.clear();

        let price_interval = if current_price > 1000.0 {
            10.0
        } else if current_price > 1.0 {
            0.01
        } else {
            0.00001
        };

        if let Some(long_ratio) = liq_long_cluster {
            if long_ratio > 0.0 && open_interest > 0.0 {
                let long_notional = open_interest * long_ratio;
                let price_levels = vec![
                    (0.98, 0.4),
                    (0.99, 0.35),
                    (0.995, 0.25),
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

        if let Some(short_ratio) = liq_short_cluster {
            if short_ratio > 0.0 && open_interest > 0.0 {
                let short_notional = open_interest * short_ratio;
                let price_levels = vec![
                    (1.02, 0.4),
                    (1.01, 0.35),
                    (1.005, 0.25),
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

    pub fn check_cascade(&self, current_price: f64) -> Option<CascadeSignal> {
        let interval = if current_price > 1000.0 { 10.0 } else { 0.01 };
        let key = (current_price / interval).round() as i64;
        
        let downside_risk: f64 = self.long_liquidations.range(..key).rev().take(5).map(|(_, v)| *v).sum();
        let upside_risk: f64 = self.short_liquidations.range(key..).take(5).map(|(_, v)| *v).sum();

        if downside_risk > 500_000.0 {
             return Some(CascadeSignal { 
                 side: Side::Short, 
                 entry_price: current_price, 
                 target_price: current_price * 0.98,
                 expected_pnl_pct: 0.02,
                 wall_notional: downside_risk,
                 confidence: 0.7,
             });
        }
        if upside_risk > 500_000.0 {
             return Some(CascadeSignal { 
                 side: Side::Long, 
                 entry_price: current_price, 
                 target_price: current_price * 1.02,
                 expected_pnl_pct: 0.02,
                 wall_notional: upside_risk,
                 confidence: 0.7,
             });
        }
        None
    }

    pub fn detect_liquidation_walls(
        &self,
        current_price: f64,
        threshold_usd: f64,
    ) -> Vec<LiquidationWall> {
        let mut walls = Vec::new();

        let price_interval = if current_price > 1000.0 {
            10.0
        } else if current_price > 1.0 {
            0.01
        } else {
            0.00001
        };

        let current_price_key = ((current_price / price_interval).round() * price_interval / price_interval) as i64;

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

        walls.sort_by(|a, b| a.distance_pct.partial_cmp(&b.distance_pct).unwrap());
        walls
    }

    pub fn generate_cascade_signal(
        &self,
        current_price: f64,
        current_tick: &MarketTick,
    ) -> Option<CascadeSignal> {
        let walls = self.detect_liquidation_walls(current_price, 2_000_000.0);

        if walls.is_empty() {
            return None;
        }

        let nearest_wall = &walls[0];

        if nearest_wall.distance_pct >= 0.2 && nearest_wall.distance_pct <= 1.5 {
            let confidence = calculate_cascade_confidence(nearest_wall, current_tick);

            match nearest_wall.direction {
                CascadeDirection::Downward => {
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

#[derive(Debug, Clone)]
pub struct VolumeProfile {
    pub profile: HashMap<i64, f64>,
}

impl VolumeProfile {
    pub fn calculate_volume_profile(candles: &[Candle]) -> Self {
        let mut profile: HashMap<i64, f64> = HashMap::new();

        for candle in candles {
            if candle.high <= candle.low || candle.volume <= 0.0 {
                continue;
            }

            let price_interval = if candle.close > 1000.0 {
                10.0
            } else if candle.close > 1.0 {
                0.01
            } else {
                0.00001
            };

            let range = candle.high - candle.low;
            if range <= 0.0 {
                continue;
            }

            let close_position = (candle.close - candle.low) / range;
            let body_low = candle.open.min(candle.close);
            let body_high = candle.open.max(candle.close);
            let body_low_position = (body_low - candle.low) / range;
            let body_high_position = (body_high - candle.low) / range;

            let body_volume = candle.volume * 0.6;
            let close_volume = candle.volume * 0.2;
            let upper_wick_volume = candle.volume * 0.1;
            let lower_wick_volume = candle.volume * 0.1;

            let price_levels = 20;
            let level_size = range / price_levels as f64;

            for i in 0..price_levels {
                let level_low = candle.low + level_size * i as f64;
                let level_high = candle.low + level_size * (i + 1) as f64;
                let level_center = (level_low + level_high) / 2.0;
                let level_position = (level_center - candle.low) / range;

                let mut volume_at_level = 0.0;

                if level_position >= body_low_position && level_position <= body_high_position {
                    let body_center = (body_low_position + body_high_position) / 2.0;
                    let distance_from_body_center = (level_position - body_center).abs();
                    let body_weight = 1.0 - (distance_from_body_center / (body_high_position - body_low_position).max(0.1));
                    volume_at_level += body_volume * body_weight.max(0.3) / price_levels as f64;
                }

                let distance_from_close = (level_position - close_position).abs();
                if distance_from_close < 0.15 {
                    let close_weight = 1.0 - (distance_from_close / 0.15);
                    volume_at_level += close_volume * close_weight;
                }

                if candle.high > body_high && level_position > body_high_position {
                    let wick_size = (candle.high - body_high) / range;
                    if wick_size > 0.05 {
                        let wick_position = (level_position - body_high_position) / wick_size;
                        if wick_position >= 0.0 && wick_position <= 1.0 {
                            let wick_weight = 1.0 - wick_position * 0.5;
                            volume_at_level += upper_wick_volume * wick_weight / (price_levels as f64 * wick_size.max(0.1));
                        }
                    }
                }

                if candle.low < body_low && level_position < body_low_position {
                    let wick_size = (body_low - candle.low) / range;
                    if wick_size > 0.05 {
                        let wick_position = (body_low_position - level_position) / wick_size;
                        if wick_position >= 0.0 && wick_position <= 1.0 {
                            let wick_weight = 1.0 - wick_position * 0.5;
                            volume_at_level += lower_wick_volume * wick_weight / (price_levels as f64 * wick_size.max(0.1));
                        }
                    }
                }

                let price_rounded = (level_center / price_interval).round() * price_interval;
                let price_key = (price_rounded / price_interval) as i64;

                *profile.entry(price_key).or_insert(0.0) += volume_at_level;
            }
        }

        VolumeProfile { profile }
    }

    pub fn find_poc(&self) -> Option<(i64, f64)> {
        self.profile
            .iter()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .map(|(price, volume)| (*price, *volume))
    }

    pub fn is_near_poc(&self, price: f64, threshold_pct: f64) -> bool {
        if let Some((poc_price_key, _)) = self.find_poc() {
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

pub fn build_liquidation_map_from_force_orders(
    force_orders: &[crate::types::ForceOrderRecord],
    current_price: f64,
    _open_interest: f64,
) -> LiquidationMap {
    let mut map = LiquidationMap::new();
    let interval = if current_price > 1000.0 { 10.0 } else { 0.01 };

    for order in force_orders {
        let qty = order.orig_qty.as_deref().unwrap_or("0").parse::<f64>().unwrap_or(0.0);
        let price = order.price.as_deref().unwrap_or("0").parse::<f64>().unwrap_or(0.0);
        if qty <= 0.0 || price <= 0.0 { continue; }

        let notional = qty * price;
        let key = (price / interval).round() as i64;

        match order.side.as_str() {
            "SELL" => *map.long_liquidations.entry(key).or_insert(0.0) += notional,
            "BUY" => *map.short_liquidations.entry(key).or_insert(0.0) += notional,
            _ => {}
        }
    }
    
    map.last_update = Utc::now();
    map
}

