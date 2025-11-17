// TRENDING: Trend analysis, generates TradeSignal
// Only does trend analysis, never places orders
// Subscribes to MarketTick events, publishes TradeSignal events
// No balance/margin/position size calculations - ORDERING handles that

use crate::config::AppCfg;
use crate::event_bus::{EventBus, MarketTick, TradeSignal};
use crate::types::{LastSignal, PositionDirection, PricePoint, Px, Side, SymbolState, TrendSignal};
use crate::utils;
use anyhow::Result;
use rust_decimal::prelude::{FromStr, ToPrimitive};
use rust_decimal::Decimal;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, Mutex};
use tracing::{debug, error, info, warn};

/// Market regime for adaptive strategy
#[derive(Debug, Clone, Copy, PartialEq)]
enum MarketRegime {
    Trending,   // Strong directional movement - use trend-following
    Ranging,    // Sideways movement - use mean-reversion (not implemented yet)
    Volatile,   // High volatility - reduce position size
    Unknown,    // Not enough data
}

struct TrendScores {
    score_long: f64,
    score_short: f64,
    trend_strength: f64,
}

/// TRENDING module - trend analysis and signal generation
pub struct Trending {
    cfg: Arc<AppCfg>,
    event_bus: Arc<EventBus>,
    shutdown_flag: Arc<AtomicBool>,
    /// Track last signal per symbol (side and timestamp) for cooldown and direction checking
    /// Prevents generating same-direction signals repeatedly
    last_signals: Arc<Mutex<HashMap<String, LastSignal>>>,
    /// Track symbol state for trend analysis (symbol -> SymbolState)
    /// Contains price history, volume, and signal timing
    symbol_states: Arc<Mutex<HashMap<String, SymbolState>>>,
}

impl Trending {
    /// Create a new Trending module instance.
    ///
    /// The Trending module analyzes market data and generates trade signals. It does not place
    /// orders directly - it only publishes TradeSignal events that are consumed by the ORDERING module.
    ///
    /// # Arguments
    ///
    /// * `cfg` - Application configuration containing trending parameters (spread thresholds, cooldown)
    /// * `event_bus` - Event bus for subscribing to MarketTick events and publishing TradeSignal events
    /// * `shutdown_flag` - Shared flag to signal graceful shutdown
    ///
    /// # Returns
    ///
    /// Returns a new `Trending` instance. Call `start()` to begin processing market data.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use std::sync::atomic::AtomicBool;
    /// # let cfg = Arc::new(crate::config::load_config()?);
    /// # let event_bus = Arc::new(crate::event_bus::EventBus::new());
    /// # let shutdown_flag = Arc::new(AtomicBool::new(false));
    /// let trending = Trending::new(cfg, event_bus, shutdown_flag);
    /// trending.start().await?;
    /// ```
    pub fn new(
        cfg: Arc<AppCfg>,
        event_bus: Arc<EventBus>,
        shutdown_flag: Arc<AtomicBool>,
    ) -> Self {
        Self {
            cfg,
            event_bus,
            shutdown_flag,
            last_signals: Arc::new(Mutex::new(HashMap::new())),
            symbol_states: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Start the trending service and begin analyzing market data.
    ///
    /// This method spawns a background task that:
    /// - Subscribes to MarketTick events from the event bus
    /// - Analyzes market data (spread, trends, etc.)
    /// - Generates TradeSignal events when trading conditions are met
    /// - Respects cooldown periods to prevent signal spam
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` immediately after spawning the background task. The task will continue
    /// running until `shutdown_flag` is set to true.
    ///
    /// # Behavior
    ///
    /// - Signals are generated based on spread thresholds configured in `cfg.trending`
    /// - Each symbol has a cooldown period to prevent duplicate signals
    /// - TradeSignal events are published to the event bus for ORDERING module to consume
    ///
    /// # Example
    ///
    /// ```no_run
    /// # let trending = crate::trending::Trending::new(todo!(), todo!(), todo!());
    /// trending.start().await?;
    /// // Service is now running in background
    /// ```
    pub async fn start(&self) -> Result<()> {
        let event_bus = self.event_bus.clone();
        let shutdown_flag = self.shutdown_flag.clone();
        let cfg = self.cfg.clone();
        let last_signals = self.last_signals.clone();
        let symbol_states = self.symbol_states.clone();
        
        // Spawn task for MarketTick events (signal generation)
        let event_bus_tick = event_bus.clone();
        let shutdown_flag_tick = shutdown_flag.clone();
        let cfg_tick = cfg.clone();
        let last_signals_tick = last_signals.clone();
        let symbol_states_tick = symbol_states.clone();
        tokio::spawn(async move {
            let market_tick_rx = event_bus_tick.subscribe_market_tick();
            let cfg_tick_clone = cfg_tick.clone();
            let event_bus_tick_clone = event_bus_tick.clone();
            let last_signals_tick_clone = last_signals_tick.clone();
            let symbol_states_tick_clone = symbol_states_tick.clone();
            crate::event_loop::run_event_loop(
                market_tick_rx,
                shutdown_flag_tick,
                "TRENDING",
                "MarketTick",
                move |tick| {
                    let cfg_tick = cfg_tick_clone.clone();
                    let event_bus_tick = event_bus_tick_clone.clone();
                    let last_signals_tick = last_signals_tick_clone.clone();
                    let symbol_states_tick = symbol_states_tick_clone.clone();
                    async move {
                        Self::process_market_tick(
                            &tick,
                            &cfg_tick,
                            &event_bus_tick,
                            &last_signals_tick,
                            &symbol_states_tick,
                        ).await
                    }
                },
            ).await;
        });
        
        let symbol_states_pos = symbol_states.clone();
        let shutdown_flag_pos = shutdown_flag.clone();
        let event_bus_pos = event_bus.clone();
        tokio::spawn(async move {
            let position_update_rx = event_bus_pos.subscribe_position_update();
            let symbol_states_pos_clone = symbol_states_pos.clone();
            crate::event_loop::run_event_loop_async(
                position_update_rx,
                shutdown_flag_pos,
                "TRENDING",
                "PositionUpdate",
                move |update| {
                    let symbol_states_pos = symbol_states_pos_clone.clone();
                    async move {
                        if !update.is_open {
                            let direction = if !update.qty.0.is_zero() {
                                Some(PositionDirection::from_qty_sign(update.qty.0))
                            } else {
                                None
                            };
                            
                            let mut states = symbol_states_pos.lock().await;
                            let state = states.entry(update.symbol.clone()).or_insert_with(|| {
                                SymbolState {
                                    symbol: update.symbol.clone(),
                                    prices: VecDeque::new(),
                                    last_signal_time: None,
                                    last_position_close_time: None,
                                    last_position_direction: None,
                                    tick_counter: 0,
                                    ema_9: None,
                                    ema_21: None,
                                    ema_55: None,
                                    ema_55_history: VecDeque::new(),
                                    rsi_avg_gain: None,
                                    rsi_avg_loss: None,
                                    rsi_period_count: 0,
                                    last_analysis_time: None,
                                }
                            });
                            
                            state.last_position_close_time = Some(Instant::now());
                            state.last_position_direction = direction;
                            
                            debug!(
                                symbol = %update.symbol,
                                direction = ?direction,
                                "TRENDING: Position closed, cooldown set for this symbol (direction-aware)"
                            );
                        }
                    }
                },
            ).await;
        });
        
        // Spawn cleanup task for symbol_states to prevent memory leak
        // Cleanup symbols that haven't received ticks in the last hour
        let symbol_states_cleanup = symbol_states.clone();
        let shutdown_flag_cleanup = shutdown_flag.clone();
        tokio::spawn(async move {
            const CLEANUP_INTERVAL_SECS: u64 = 3600; // Cleanup every hour
            const MAX_AGE_SECS: u64 = 3600; // Remove symbols not seen in last hour
            
            loop {
                tokio::time::sleep(Duration::from_secs(CLEANUP_INTERVAL_SECS)).await;
                
                if shutdown_flag_cleanup.load(AtomicOrdering::Relaxed) {
                    break;
                }
                
                let now = Instant::now();
                let mut states = symbol_states_cleanup.lock().await;
                let initial_count = states.len();
                
                // Remove symbols that haven't received ticks in the last hour
                // Use last price timestamp from prices VecDeque, or fallback to last_signal_time/last_position_close_time
                states.retain(|symbol, state| {
                    // Get last activity timestamp from price history (most recent price point)
                    let last_activity = state.prices
                        .back()
                        .map(|p| p.timestamp)
                        .or_else(|| state.last_signal_time)
                        .or_else(|| state.last_position_close_time);
                    
                    if let Some(last_ts) = last_activity {
                        let age = now.duration_since(last_ts);
                        if age.as_secs() > MAX_AGE_SECS {
                            // Symbol hasn't been active in the last hour - remove to prevent memory leak
                            debug!(
                                symbol = %symbol,
                                age_secs = age.as_secs(),
                                age_hours = age.as_secs() / 3600,
                                "TRENDING: Cleaning up stale symbol state (no ticks in {} hours)",
                                age.as_secs() / 3600
                            );
                            false // Remove
                        } else {
                            true // Keep (recent activity)
                        }
                    } else {
                        // No activity timestamp at all - remove empty state
                        debug!(
                            symbol = %symbol,
                            "TRENDING: Cleaning up empty symbol state (no activity recorded)"
                        );
                        false // Remove
                    }
                });
                
                let final_count = states.len();
                let removed_count = initial_count.saturating_sub(final_count);
                
                if removed_count > 0 {
                    info!(
                        initial_count,
                        final_count,
                        removed_count,
                        "TRENDING: Cleaned up {} stale symbol states (memory leak prevention)",
                        removed_count
                    );
                }
            }
        });
        
        Ok(())
    }

    fn calculate_sma(prices: &std::collections::VecDeque<crate::types::PricePoint>, period: usize) -> Option<Decimal> {
        if prices.len() < period {
            return None;
        }
        
        let recent_prices: Vec<Decimal> = prices
            .iter()
            .rev()
            .take(period)
            .map(|p| p.price)
            .collect();
        
        if recent_prices.len() < period {
            warn!(
                prices_len = prices.len(),
                period,
                recent_prices_len = recent_prices.len(),
                "TRENDING: calculate_sma - unexpected: prices.len() >= period but recent_prices.len() < period"
            );
            return None;
        }
        
        let sum: Decimal = recent_prices.iter().sum();
        Some(sum / Decimal::from(period))
    }
    
    fn update_ema(prev_ema: Option<Decimal>, new_price: Decimal, period: usize, prices: &std::collections::VecDeque<crate::types::PricePoint>) -> Decimal {
        let k = Decimal::from(2) / Decimal::from(period + 1);
        
        match prev_ema {
            Some(ema) => new_price * k + ema * (Decimal::ONE - k),
            None => {
                if prices.len() >= period {
                    match Self::calculate_sma(prices, period) {
                        Some(sma) => sma,
                        None => {
                            warn!(
                                prices_len = prices.len(),
                                period,
                                "TRENDING: update_ema - calculate_sma returned None despite prices.len() >= period, using new_price as fallback"
                            );
                            new_price
                        }
                    }
                } else {
                    new_price
                }
            }
        }
    }
    
    pub fn update_indicators(state: &mut SymbolState, new_price: Decimal) {
        if state.prices.len() >= 9 {
            state.ema_9 = Some(Self::update_ema(state.ema_9, new_price, 9, &state.prices));
        } else {
            state.ema_9 = None;
        }
        
        if state.prices.len() >= 21 {
            state.ema_21 = Some(Self::update_ema(state.ema_21, new_price, 21, &state.prices));
        } else {
            state.ema_21 = None;
        }
        
        if state.prices.len() >= 55 {
            state.ema_55 = Some(Self::update_ema(state.ema_55, new_price, 55, &state.prices));
        } else {
            state.ema_55 = None;
        }
        
        if let Some(ema_55) = state.ema_55 {
            state.ema_55_history.push_back(ema_55);
            const EMA_HISTORY_SIZE: usize = 10;
            while state.ema_55_history.len() > EMA_HISTORY_SIZE {
                state.ema_55_history.pop_front();
            }
        } else {
            state.ema_55_history.clear();
        }
        
        if state.prices.len() >= 2 {
            if let Some(prev_price_point) = state.prices.get(state.prices.len() - 2) {
                let prev_price = prev_price_point.price;
                let change = new_price - prev_price;
                let (gain, loss) = if change.is_sign_positive() {
                    (change, Decimal::ZERO)
                } else {
                    (Decimal::ZERO, -change)
                };
                
                const RSI_PERIOD: usize = 14;
                let alpha = Decimal::ONE / Decimal::from(RSI_PERIOD);
                
                state.rsi_avg_gain = Some(match state.rsi_avg_gain {
                    Some(ag) => ag * (Decimal::ONE - alpha) + gain * alpha,
                    None => gain,
                });
                
                state.rsi_avg_loss = Some(match state.rsi_avg_loss {
                    Some(al) => al * (Decimal::ONE - alpha) + loss * alpha,
                    None => loss,
                });
                
                state.rsi_period_count += 1;
            }
        }
    }
    
    /// Calculate RSI from incremental state (O(1) performance)
    fn calculate_rsi_from_state(state: &SymbolState, cfg: &crate::config::TrendingCfg) -> Option<f64> {
        const RSI_PERIOD: usize = 14;
        
        // Need at least RSI_PERIOD updates before RSI is reliable
        if state.rsi_period_count < RSI_PERIOD {
            return None;
        }
        
        let avg_gain = state.rsi_avg_gain?;
        let avg_loss = state.rsi_avg_loss?;
        
        if avg_loss.is_zero() {
            return Some(100.0); // All gains, no losses
        }
        
        let min_avg_loss = utils::f64_to_decimal(cfg.rsi_min_avg_loss, Decimal::from_str("0.0001").unwrap_or(Decimal::ZERO));
        let avg_loss_clamped = avg_loss.max(min_avg_loss);
        
        let rs = avg_gain / avg_loss_clamped;
        let rsi = Decimal::from(100) - (Decimal::from(100) / (Decimal::ONE + rs));
        let rsi_value = rsi.to_f64().unwrap_or(50.0);
        let clamped_rsi = rsi_value.max(0.1).min(99.9);
        
        if clamped_rsi != rsi_value {
            warn!(
                symbol = %state.symbol,
                original_rsi = rsi_value,
                clamped_rsi = clamped_rsi,
                avg_gain = %avg_gain,
                avg_loss = %avg_loss,
                avg_loss_clamped = %avg_loss_clamped,
                "TRENDING: RSI clamped to valid range [0.1, 99.9]"
            );
        }
        
        Some(clamped_rsi)
    }
    
    /// Calculate average volume over period
    fn calculate_avg_volume(prices: &VecDeque<PricePoint>, period: usize) -> Option<Decimal> {
        if prices.len() < period {
            return None;
        }
        
        let volumes: Vec<Decimal> = prices
            .iter()
            .rev()
            .take(period)
            .filter_map(|p| p.volume)
            .collect();
        
        if volumes.is_empty() {
            return None;
        }
        
        let sum: Decimal = volumes.iter().sum();
        Some(sum / Decimal::from(volumes.len()))
    }
    
    /// Calculate Average True Range (ATR) for volatility measurement
    fn calculate_atr(prices: &VecDeque<PricePoint>, period: usize) -> Option<Decimal> {
        if prices.len() < period + 1 {
            return None;
        }

        let mut true_ranges = Vec::new();
        let price_points: Vec<Decimal> = prices.iter().rev().take(period + 1).map(|p| p.price).collect();

        for i in 1..price_points.len() {
            let high = price_points[i - 1];
            let low = price_points[i];
            let tr = (high - low).abs();
            true_ranges.push(tr);
        }

        if true_ranges.is_empty() {
            return None;
        }

        let sum: Decimal = true_ranges.iter().sum();
        Some(sum / Decimal::from(true_ranges.len()))
    }

    /// Detect market regime (simplified - ATR-based only)
    /// Note: Removed simplified ADX calculation as it was inaccurate
    /// Using only ATR for volatility-based regime detection
    fn detect_market_regime(state: &SymbolState, cfg: &crate::config::TrendingCfg) -> MarketRegime {
        let atr_period = cfg.atr_period;
        let low_volatility_threshold = cfg.low_volatility_threshold;
        let high_volatility_threshold = cfg.high_volatility_threshold;

        let prices = &state.prices;
        
        // Calculate ATR for volatility
        let atr = Self::calculate_atr(prices, atr_period);
        let atr_pct = if let (Some(atr_val), Some(current_price)) = (atr, prices.back()) {
            if !current_price.price.is_zero() {
                (atr_val / current_price.price * Decimal::from(100)).to_f64()
            } else {
                None
            }
        } else {
            None
        };

        // Determine regime based on ATR only (simplified approach)
        if let Some(atr_pct_val) = atr_pct {
            if atr_pct_val < low_volatility_threshold {
                MarketRegime::Ranging
            } else if atr_pct_val > high_volatility_threshold {
                MarketRegime::Volatile
            } else {
                // Medium volatility - assume trending (default for trend-following strategy)
                MarketRegime::Trending
            }
        } else {
            MarketRegime::Unknown
        }
    }

    fn calculate_ema_scores(
        state: &SymbolState,
        current_price: Decimal,
        cfg: &crate::config::TrendingCfg,
    ) -> Option<(TrendScores, bool, bool, bool, Decimal, Decimal, Decimal)> {
        let ema_fast = state.ema_9?;
        let ema_mid = state.ema_21?;
        let ema_slow = state.ema_55?;
        
        let mut score_long = 0.0;
        let mut score_short = 0.0;
        let mut trend_strength = 0.0;
        
        let short_term_aligned = current_price > ema_fast && ema_fast > ema_mid;
        let short_term_aligned_short = current_price < ema_fast && ema_fast < ema_mid;
        let ema_short_score = cfg.ema_short_score;
        if short_term_aligned {
            score_long += ema_short_score;
            trend_strength += 0.4;
        } else if short_term_aligned_short {
            score_short += ema_short_score;
            trend_strength += 0.4;
        }
        
        let mid_term_aligned = ema_mid > ema_slow;
        let mid_term_aligned_short = ema_mid < ema_slow;
        let ema_mid_score = cfg.ema_mid_score;
        if mid_term_aligned {
            score_long += ema_mid_score;
            trend_strength += 0.3;
        } else if mid_term_aligned_short {
            score_short += ema_mid_score;
            trend_strength += 0.3;
        }
        
        let ema_slope = if state.ema_55_history.len() >= 2 {
            if let (Some(current_ema), Some(old_ema)) = (state.ema_55_history.back(), state.ema_55_history.front()) {
                if !old_ema.is_zero() {
                    Some((current_ema - old_ema) / old_ema)
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        
        let slope_strong = if let Some(slope) = ema_slope {
            let min_slope = Decimal::from(5) / Decimal::from(10000);
            let slope_score = cfg.slope_score;
            if slope > min_slope {
                score_long += slope_score;
                trend_strength += 0.3;
                true
            } else if slope < -min_slope {
                score_short += slope_score;
                trend_strength += 0.3;
                true
            } else {
                false
            }
        } else {
            false
        };
        
        Some((
            TrendScores {
                score_long,
                score_short,
                trend_strength,
            },
            short_term_aligned,
            mid_term_aligned,
            slope_strong,
            ema_fast,
            ema_mid,
            ema_slow,
        ))
    }

    /// Public for backtesting
    pub fn analyze_trend(state: &SymbolState, cfg: &crate::config::TrendingCfg) -> Option<TrendSignal> {
        const VOLUME_PERIOD: usize = 20;
        const EMA_SLOW_PERIOD: usize = 55;
        const MIN_PRICE_POINTS: usize = EMA_SLOW_PERIOD;
        let base_min_score = cfg.base_min_score;
        let base_rsi_lower = cfg.rsi_lower_long;
        let base_rsi_upper = cfg.rsi_upper_long;
        let base_rsi_upper_short = cfg.rsi_upper_short;
        
        let prices = &state.prices;
        
        if prices.len() < MIN_PRICE_POINTS {
            if prices.len() % 5 == 0 || prices.len() >= MIN_PRICE_POINTS - 1 {
                debug!(
                    symbol = %state.symbol,
                    prices_len = prices.len(),
                    required = MIN_PRICE_POINTS,
                    "TRENDING: Not enough price data for analysis (need {} points)",
                    MIN_PRICE_POINTS
                );
            }
            return None;
        }
        
        if prices.len() == MIN_PRICE_POINTS {
            info!(
                symbol = %state.symbol,
                prices_len = prices.len(),
                rsi_period_count = state.rsi_period_count,
                ema_9_initialized = state.ema_9.is_some(),
                ema_21_initialized = state.ema_21.is_some(),
                ema_55_initialized = state.ema_55.is_some(),
                volume_count = prices.iter().filter(|p| p.volume.is_some()).count(),
                "TRENDING: Reached {} price points, starting trend analysis",
                MIN_PRICE_POINTS
            );
        }
        
        let current_price = prices.back()?.price;
        
        let (mut scores, short_term_aligned, mid_term_aligned, slope_strong, ema_fast, ema_mid, ema_slow) = match Self::calculate_ema_scores(state, current_price, cfg) {
            Some(s) => s,
            None => return None,
        };
        
        let mut score_long = scores.score_long;
        let mut score_short = scores.score_short;
        let mut trend_strength = scores.trend_strength;
        
        // 2. RSI momentum confirmation (weight: 1.0) with adaptive thresholds
        let rsi = match Self::calculate_rsi_from_state(state, cfg) {
            Some(rsi_val) => rsi_val,
            None => {
                warn!(
                    symbol = %state.symbol,
                    prices_len = prices.len(),
                    rsi_period_count = state.rsi_period_count,
                    rsi_avg_gain = ?state.rsi_avg_gain,
                    rsi_avg_loss = ?state.rsi_avg_loss,
                    "TRENDING: RSI calculation failed - rsi_period_count={} < 14, skipping trend analysis",
                    state.rsi_period_count
                );
                return None;
            }
        };
        
        // Adaptive RSI thresholds based on volatility (from config)
        let atr = Self::calculate_atr(prices, cfg.atr_period);
        let volatility_multiplier = if let (Some(atr_val), Some(current_price)) = (atr, prices.back()) {
            if !current_price.price.is_zero() {
                let atr_pct = (atr_val / current_price.price).to_f64().unwrap_or(0.01);
                let base_volatility = cfg.base_volatility; // From config
                (atr_pct / base_volatility).max(0.5).min(2.0) // Clamp between 0.5x and 2x
            } else {
                1.0
            }
        } else {
            1.0
        };
        
        // ✅ SMART OPTIMIZATION: Keep original RSI range for trending markets
        // Backtest: DOGE (54.5% win rate) and BTC (50% win rate) worked well with original range
        // Only narrow RSI range for ranging/volatile markets (done in regime-based filtering)
        let rsi_lower = base_rsi_lower - (10.0 * volatility_multiplier); // 45-55 range (original)
        let rsi_upper = base_rsi_upper - (5.0 * (1.0 - volatility_multiplier)); // 65-70 range (original)
        
        // Short signals: RSI < upper_short (bearish momentum, oversold region)
        // Use config value instead of hardcoded 40.0
        let rsi_upper_short = base_rsi_upper_short;
        
        let rsi_bullish = rsi > rsi_lower && rsi < rsi_upper;
        let rsi_bearish = rsi < rsi_upper_short;
        let rsi_score = cfg.rsi_score;
        if rsi_bullish {
            score_long += rsi_score;
        } else if rsi_bearish {
            score_short += rsi_score;
        }
        
        let regime = Self::detect_market_regime(state, cfg);
        let min_score = match regime {
            MarketRegime::Trending => base_min_score * cfg.regime_multiplier_trending,
            MarketRegime::Ranging => base_min_score * cfg.regime_multiplier_ranging,
            MarketRegime::Volatile => base_min_score * cfg.regime_multiplier_volatile,
            MarketRegime::Unknown => base_min_score * cfg.regime_multiplier_unknown,
        };
        
        // ✅ OPTIMIZED: Stricter volume confirmation based on backtest results
        // Backtest analysis: DOGE (54.5% win rate) had strong volume confirmation
        // ETH (25% win rate) and SOL (33% win rate) had weak volume confirmation
        // Solution: Higher volume threshold and make it mandatory for medium-volatility coins
        // ✅ HFT MODE: In HFT mode, volume is optional if require_volume_confirmation is false
        let (current_volume, avg_volume) = if cfg.hft_mode && !cfg.require_volume_confirmation {
            // HFT mode with optional volume - try to get volume but don't fail if missing
            let current_vol = prices.back()
                .and_then(|p| p.volume);
            let avg_vol = Self::calculate_avg_volume(prices, VOLUME_PERIOD);
            
            // If volume data is available, use it; otherwise use None (volume confirmation will be skipped)
            (current_vol, avg_vol)
        } else {
            // Normal mode - volume is required
            let current_volume = match prices.back() {
                Some(price_point) => match price_point.volume {
                    Some(vol) => vol,
                    None => {
                        warn!(
                            symbol = %state.symbol,
                            prices_len = prices.len(),
                            "TRENDING: Current price point has no volume data, skipping trend analysis"
                        );
                        return None;
                    }
                },
                None => {
                    warn!(
                        symbol = %state.symbol,
                        "TRENDING: No price data available, skipping trend analysis"
                    );
                    return None;
                }
            };
            
            let avg_volume = match Self::calculate_avg_volume(prices, VOLUME_PERIOD) {
                Some(avg) => avg,
                None => {
                    // Count how many price points have volume data
                    let volume_count = prices.iter().filter(|p| p.volume.is_some()).count();
                    warn!(
                        symbol = %state.symbol,
                        prices_len = prices.len(),
                        volume_period = VOLUME_PERIOD,
                        volume_count,
                        "TRENDING: Average volume calculation failed - only {}/{} price points have volume data, skipping trend analysis",
                        volume_count,
                        prices.len()
                    );
                    return None;
                }
            };
            
            (Some(current_volume), Some(avg_volume))
        };
        
        // ✅ UNIVERSAL PATTERN: Trend + Volume combination = higher win rate across ALL coins
        // Backtest analysis:
        // - DOGE/BTC (good): Strong trend alignment + Volume = 50%+ win rate
        // - ETH/SOL (bad): Weak trend alignment + No volume = 25-33% win rate
        // Solution: Require strong trend alignment OR volume confirmation
        // This filters out weak signals that fail in ETH/SOL while preserving good signals in DOGE/BTC
        
        // Calculate trend strength (0.0 - 1.0)
        // Strong trend = at least 2 of 3 EMA conditions met (short + mid = 0.7, or short + slope = 0.7, etc.)
        // This allows signals when trend is clearly established (not just perfect alignment)
        // ✅ Get trend threshold from config instead of hardcoded values
        let trend_threshold = if cfg.hft_mode {
            cfg.trend_threshold_hft
        } else {
            cfg.trend_threshold_normal
        };
        let is_strong_trend = trend_strength >= trend_threshold;
        
        // ✅ CRITICAL FIX: No volume data available (volume=None in MarketTick)
        // Since volume data is not available from WebSocket, we bypass volume confirmation
        // for strong trends. This allows signals to be generated even without volume data.
        // Calculate volume_confirms AFTER is_strong_trend is defined
        let volume_confirms = if let (Some(current_vol), Some(avg_vol)) = (current_volume, avg_volume) {
            let volume_multiplier = if cfg.hft_mode {
                utils::f64_to_decimal(cfg.volume_multiplier_hft, Decimal::from_str("1.1").unwrap_or(Decimal::from(110) / Decimal::from(100)))
            } else {
                utils::f64_to_decimal(cfg.volume_multiplier_normal, Decimal::from_str("1.3").unwrap_or(Decimal::from(130) / Decimal::from(100)))
            };
            let volume_surge = current_vol > avg_vol * volume_multiplier;
            
            // Volume trend (recent > longer average)
            let volume_trend = match Self::calculate_avg_volume(prices, 5) {
                Some(recent_avg_volume) => recent_avg_volume > avg_vol,
                None => false,
            };
            
            // In HFT mode, only volume_surge is required; volume_trend is optional
            if cfg.hft_mode {
                volume_surge
            } else {
                volume_surge && volume_trend
            }
        } else {
            // ✅ CRITICAL FIX: No volume data available (volume=None in MarketTick)
            // Since volume data is not available from WebSocket, we bypass volume confirmation
            // for strong trends. This allows signals to be generated even without volume data.
            if cfg.hft_mode && !cfg.require_volume_confirmation {
                // HFT mode without volume requirement - treat as confirmed
                true
            } else if is_strong_trend {
                // Strong trend can compensate for missing volume data
                true
            } else {
                // Weak trend without volume - require volume confirmation
                false
            }
        };
        
        // Universal rule: Strong trend OR volume confirmation required
        // Weak trend without volume = reject (prevents ETH/SOL false signals)
        // Strong trend with/without volume = allow (preserves DOGE/BTC good signals)
        // Volume with weak trend = allow but with higher threshold (preserves some good signals)
        if !is_strong_trend && !volume_confirms {
            // Weak trend + no volume = reject signal (prevents false signals in ETH/SOL)
            debug!(
                symbol = %state.symbol,
                trend_strength,
                is_strong_trend,
                volume_confirms,
                "TRENDING: Signal rejected - weak trend without volume confirmation"
            );
            return None;
        }
        
        // Apply volume bonus (if present)
        if volume_confirms {
            score_long += 1.0;
            score_short += 1.0;
        }
        
        // 4. Generate signal based on weighted score (with adaptive threshold)
        // ✅ PRODUCTION: Original adaptive threshold
        // Backtest: Original 10% increase worked well
        // Solution: Return to proven 10% increase for weak trends
        let final_min_score = if is_strong_trend {
            min_score // Strong trend: use normal threshold (works for DOGE/BTC)
        } else {
            // Weak trend but has volume: require higher score (from config)
            // This filters out marginal signals that fail in ETH/SOL
            min_score * cfg.weak_trend_score_multiplier
        };
        
        // ✅ DEBUG: Always log score analysis for troubleshooting (even if scores are 0)
        // This helps identify why signals are not being generated
        debug!(
            symbol = %state.symbol,
            score_long,
            score_short,
            final_min_score,
            is_strong_trend,
            volume_confirms,
            trend_strength,
            rsi,
            ema_fast = %ema_fast,
            ema_mid = %ema_mid,
            ema_slow = %ema_slow,
            current_price = %current_price,
            short_term_aligned,
            mid_term_aligned,
            slope_strong,
            rsi_bullish,
            rsi_bearish,
            "TRENDING: Score analysis (signal will be generated if score >= threshold)"
        );
        
        if score_long >= final_min_score {
            Some(TrendSignal::Long)
        } else if score_short >= final_min_score {
            Some(TrendSignal::Short)
        } else {
            None // Score too low
        }
    }
    

    /// Process a market tick and generate trade signal if conditions are met
    fn check_spread(tick: &MarketTick, cfg: &Arc<AppCfg>) -> Result<Option<f64>> {
        let spread_bps_f64 = crate::utils::calculate_spread_bps(tick.bid, tick.ask);
        let min_acceptable_spread_bps = cfg.trending.min_spread_bps;
        let max_acceptable_spread_bps = cfg.trending.max_spread_bps;
        
        if spread_bps_f64 < min_acceptable_spread_bps || spread_bps_f64 > max_acceptable_spread_bps {
            debug!(
                symbol = %tick.symbol,
                spread_bps = spread_bps_f64,
                min_spread = min_acceptable_spread_bps,
                max_spread = max_acceptable_spread_bps,
                "TRENDING: Spread check failed (outside acceptable range)"
            );
            return Ok(None);
        }
        
        Ok(Some(spread_bps_f64))
    }

    async fn check_cooldown(
        tick: &MarketTick,
        now: Instant,
        cfg: &Arc<AppCfg>,
        last_signals: &Arc<Mutex<HashMap<String, LastSignal>>>,
    ) -> Result<bool> {
        let cooldown_seconds = cfg.trending.signal_cooldown_seconds;
        let last_signals_map = last_signals.lock().await;
        if let Some(last_signal) = last_signals_map.get(&tick.symbol) {
            let elapsed = now.duration_since(last_signal.timestamp);
            if elapsed < Duration::from_secs(cooldown_seconds) {
                let remaining_secs = cooldown_seconds - elapsed.as_secs();
                let cooldown_threshold = (cooldown_seconds * 4) / 5;
                if elapsed.as_secs() >= cooldown_threshold {
                    debug!(
                        symbol = %tick.symbol,
                        elapsed_secs = elapsed.as_secs(),
                        remaining_secs = remaining_secs,
                        cooldown_secs = cooldown_seconds,
                        last_signal_side = ?last_signal.side,
                        "TRENDING: Cooldown check failed - {} seconds elapsed, {} seconds remaining",
                        elapsed.as_secs(),
                        remaining_secs
                    );
                }
                tracing::trace!(
                    symbol = %tick.symbol,
                    elapsed_secs = elapsed.as_secs(),
                    cooldown_secs = cooldown_seconds,
                    last_signal_side = ?last_signal.side,
                    "TRENDING: Cooldown check failed (trace)"
                );
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn calculate_dynamic_tp_sl(
        tick: &MarketTick,
        mid_price: Decimal,
        cfg: &Arc<AppCfg>,
        symbol_states: &Arc<Mutex<HashMap<String, SymbolState>>>,
    ) -> (Option<f64>, Option<f64>) {
        let states = symbol_states.lock().await;
        if let Some(state) = states.get(&tick.symbol) {
            const ATR_PERIOD: usize = 14;
            const ATR_SL_MULTIPLIER: f64 = 2.0;
            const ATR_TP_MULTIPLIER: f64 = 5.0;
            
            if let Some(atr) = Self::calculate_atr(&state.prices, ATR_PERIOD) {
                if !mid_price.is_zero() {
                    let atr_pct = (atr / mid_price * Decimal::from(100)).to_f64().unwrap_or(0.0);
                    let dynamic_sl_pct = (ATR_SL_MULTIPLIER * atr_pct).max(cfg.stop_loss_pct).min(5.0);
                    let dynamic_tp_pct = (ATR_TP_MULTIPLIER * atr_pct).max(cfg.take_profit_pct).min(10.0);
                    let final_tp = dynamic_tp_pct.max(dynamic_sl_pct * 1.5);
                    return (Some(dynamic_sl_pct), Some(final_tp));
                }
            }
        }
        (Some(cfg.stop_loss_pct), Some(cfg.take_profit_pct))
    }

    /// Simple algorithm: Spread check → Cooldown check → Trend analysis → Signal
    async fn process_market_tick(
        tick: &MarketTick,
        cfg: &Arc<AppCfg>,
        event_bus: &Arc<EventBus>,
        last_signals: &Arc<Mutex<HashMap<String, LastSignal>>>,
        symbol_states: &Arc<Mutex<HashMap<String, SymbolState>>>,
    ) -> Result<()> {
        let now = Instant::now();
        
        let spread_bps_f64 = match Self::check_spread(tick, cfg)? {
            Some(s) => s,
            None => return Ok(()),
        };

        if !Self::check_cooldown(tick, now, cfg, last_signals).await? {
            return Ok(());
        }
        
        // 3. Trend analysis
        let mid_price = crate::utils::calculate_mid_price(tick.bid, tick.ask);
        let spread_timestamp = now;
        
        let trend_signal = {
            let mut states = symbol_states.lock().await;
            let state = states.entry(tick.symbol.clone()).or_insert_with(|| SymbolState {
                                    symbol: tick.symbol.clone(),
                                    prices: VecDeque::new(),
                                    last_signal_time: None,
                                    last_position_close_time: None,
                                    last_position_direction: None,
                                    tick_counter: 0,
                                    ema_9: None,
                                    ema_21: None,
                                    ema_55: None,
                                    ema_55_history: VecDeque::new(),
                                    rsi_avg_gain: None,
                                    rsi_avg_loss: None,
                                    rsi_period_count: 0,
                                    last_analysis_time: None,
            });
            
            // ✅ FIX: Throttling to prevent channel lag (from config)
            // Problem: Too many trend analyses causing channel lag (10,000+ warnings)
            // Solution: Limit analysis frequency based on config (default: 10 per second = 100ms)
            let min_analysis_interval_ms = cfg.trending.min_analysis_interval_ms;
            
            if let Some(last_analysis) = state.last_analysis_time {
                let elapsed_ms = now.duration_since(last_analysis).as_millis() as u64;
                if elapsed_ms < min_analysis_interval_ms {
                    // Skip - too frequent, prevent channel lag
                    return Ok(());
                }
            }
            
            // Update last analysis time
            state.last_analysis_time = Some(now);
            
            state.prices.push_back(PricePoint {
                timestamp: now,
                price: mid_price,
                volume: tick.volume,
            });
            
            const MAX_HISTORY: usize = 100; // Keep last 100 prices (EMA_SLOW=55 + buffer)
            while state.prices.len() > MAX_HISTORY {
                state.prices.pop_front();
            }
            
            // Incremental EMA/RSI update (O(1) performance)
            Self::update_indicators(state, mid_price);
            
            Self::analyze_trend(state, &cfg.trending)
        };
        
        // 4. Generate signal if trend detected
        let side = match trend_signal {
            Some(TrendSignal::Long) => Side::Buy,
            Some(TrendSignal::Short) => Side::Sell,
            None => return Ok(()), // No trend, skip
        };
        
        let entry_price = Px(mid_price);
        
        let (stop_loss_pct, take_profit_pct) = Self::calculate_dynamic_tp_sl(
            tick,
            mid_price,
            cfg,
            symbol_states,
        ).await;
        
        // 6. Send signal
        let signal = TradeSignal {
            symbol: tick.symbol.clone(),
            side,
            entry_price,
            stop_loss_pct,
            take_profit_pct,
            spread_bps: spread_bps_f64,
            spread_timestamp,
            timestamp: now,
        };
        
        if let Err(e) = event_bus.trade_signal_tx.send(signal.clone()) {
            error!(error = ?e, symbol = %tick.symbol, "TRENDING: Failed to send TradeSignal");
        } else {
            // Update last signal timestamp
            {
                let mut last_signals_map = last_signals.lock().await;
                last_signals_map.insert(tick.symbol.clone(), LastSignal {
                    side,
                    timestamp: now,
                });
            }
            
            info!(
                symbol = %tick.symbol,
                side = ?side,
                entry_price = %entry_price.0,
                spread_bps = spread_bps_f64,
                "TRENDING: TradeSignal generated"
            );
        }
        
        Ok(())
    }
}

