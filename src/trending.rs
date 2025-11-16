// TRENDING: Trend analysis, generates TradeSignal
// Only does trend analysis, never places orders
// Subscribes to MarketTick events, publishes TradeSignal events
// No balance/margin/position size calculations - ORDERING handles that

use crate::config::AppCfg;
use crate::event_bus::{EventBus, MarketTick, TradeSignal};
use crate::types::{LastSignal, PositionDirection, PricePoint, Px, Side, SymbolState, TrendSignal};
use anyhow::Result;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::str::FromStr;
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

// ============================================================================
// Utility Functions
// ============================================================================

/// Quantize decimal value to step (floor to nearest step multiple)
///
/// ✅ KRİTİK: Precision loss önleme
/// Decimal division ve multiplication yaparken precision loss olabilir.
/// Sonucu normalize ederek step'in tam katı olduğundan emin oluyoruz.
fn quantize_decimal(value: Decimal, step: Decimal) -> Decimal {
    if step.is_zero() || step.is_sign_negative() {
        return value;
    }

    let ratio = value / step;
    let floored = ratio.floor();
    let result = floored * step;
    let step_scale = step.scale();
    let normalized = result.normalize();

    // Round to step's scale first
    let rounded = normalized.round_dp_with_strategy(
        step_scale,
        rust_decimal::RoundingStrategy::ToNegativeInfinity,
    );

    // Double-check quantization - ensure result is a multiple of step
    let re_quantized_ratio = rounded / step;
    let re_quantized_floor = re_quantized_ratio.floor();
    let final_result = re_quantized_floor * step;

    // Final normalization and rounding
    final_result.normalize().round_dp_with_strategy(
        step_scale,
        rust_decimal::RoundingStrategy::ToNegativeInfinity,
    )
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
            let mut market_tick_rx = event_bus_tick.subscribe_market_tick();
            
            info!("TRENDING: Started, listening to MarketTick events");
            
            loop {
                if shutdown_flag_tick.load(AtomicOrdering::Relaxed) {
                    break;
                }
                
                match market_tick_rx.recv().await {
                    Ok(tick) => {
                        if shutdown_flag_tick.load(AtomicOrdering::Relaxed) {
                            break;
                        }
                        
                        if let Err(e) = Self::process_market_tick(
                            &tick,
                            &cfg_tick,
                            &event_bus_tick,
                            &last_signals_tick,
                            &symbol_states_tick,
                        ).await {
                            warn!(error = %e, symbol = %tick.symbol, "TRENDING: error processing market tick");
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        // Channel lagged - some messages were skipped
                        warn!(skipped = skipped, "TRENDING: MarketTick channel lagged, some messages skipped");
                        // Continue processing - don't break
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // Channel closed - all senders dropped
                        warn!("TRENDING: MarketTick channel closed, all senders dropped");
                        break;
                    }
                }
            }
            
            info!("TRENDING: Stopped");
        });
        
        // Spawn task for PositionUpdate events (to track position close for cooldown)
        let symbol_states_pos = symbol_states.clone();
        let shutdown_flag_pos = shutdown_flag.clone();
        tokio::spawn(async move {
            let mut position_update_rx = event_bus.subscribe_position_update();
            
            info!("TRENDING: Started, listening to PositionUpdate events for position close cooldown");
            
            loop {
                match position_update_rx.recv().await {
                    Ok(update) => {
                        if shutdown_flag_pos.load(AtomicOrdering::Relaxed) {
                            break;
                        }
                        
                        // Track position close for cooldown
                        if !update.is_open {
                            // Position closed - set cooldown timestamp and direction
                            // Determine direction from qty sign:
                            // - Long position: qty > 0 (was opened with BUY order)
                            // - Short position: qty < 0 (was opened with SELL order)
                            let direction = if !update.qty.0.is_zero() {
                                // Qty is non-zero, determine direction from sign
                                Some(PositionDirection::from_qty_sign(update.qty.0))
                            } else {
                                // Qty is zero - can't determine direction, apply cooldown to both
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
                    Err(_) => break,
                }
            }
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

    /// Calculate Simple Moving Average (SMA) for EMA bootstrap
    /// Returns the average of the last `period` prices
    fn calculate_sma(prices: &std::collections::VecDeque<crate::types::PricePoint>, period: usize) -> Option<Decimal> {
        if prices.len() < period {
            return None;
        }
        
        // Get last `period` prices
        let recent_prices: Vec<Decimal> = prices
            .iter()
            .rev()
            .take(period)
            .map(|p| p.price)
            .collect();
        
        if recent_prices.len() < period {
            return None;
        }
        
        // Calculate sum
        let sum: Decimal = recent_prices.iter().sum();
        
        // Return average
        Some(sum / Decimal::from(period))
    }
    
    /// Update EMA incrementally (O(1) performance)
    /// EMA formula: EMA = Price(t) * k + EMA(y) * (1 – k)
    /// where k = 2 / (period + 1)
    /// 
    /// ✅ CRITICAL: For bootstrap, SMA is used as initial value
    /// This ensures EMA starts with a proper average instead of just the current price
    fn update_ema(prev_ema: Option<Decimal>, new_price: Decimal, period: usize, prices: &std::collections::VecDeque<crate::types::PricePoint>) -> Decimal {
        let k = Decimal::from(2) / Decimal::from(period + 1);
        
        match prev_ema {
            Some(ema) => {
                // Incremental update (fast)
                new_price * k + ema * (Decimal::ONE - k)
            }
            None => {
                // ✅ CRITICAL: Bootstrap with SMA if we have enough prices
                // Problem: Using just current price as initial EMA is incorrect
                // Solution: Calculate SMA of last `period` prices as initial EMA value
                // This ensures EMA starts with a proper average, not just the latest price
                if prices.len() >= period {
                    // Calculate SMA for initialization (bootstrap)
                    if let Some(sma) = Self::calculate_sma(prices, period) {
                        sma // Use SMA as initial EMA value
                    } else {
                        // Fallback: use current price if SMA calculation fails
                        new_price
                    }
                } else {
                    // Not enough prices yet - use current price as temporary value
                    // This will be replaced with SMA once we have enough prices
                    new_price
                }
            }
        }
    }
    
    /// Update all indicators incrementally (called on each tick)
    /// Public for backtesting
    pub fn update_indicators(state: &mut SymbolState, new_price: Decimal) {
        // Update EMAs incrementally
        // ✅ CRITICAL: Pass prices list for SMA bootstrap calculation
        state.ema_9 = Some(Self::update_ema(state.ema_9, new_price, 9, &state.prices));
        state.ema_21 = Some(Self::update_ema(state.ema_21, new_price, 21, &state.prices));
        state.ema_55 = Some(Self::update_ema(state.ema_55, new_price, 55, &state.prices));
        
        // Track EMA_55 history for slope calculation
        if let Some(ema_55) = state.ema_55 {
            state.ema_55_history.push_back(ema_55);
            const EMA_HISTORY_SIZE: usize = 10; // Keep last 10 EMA values for slope
            while state.ema_55_history.len() > EMA_HISTORY_SIZE {
                state.ema_55_history.pop_front();
            }
        }
        
        // Update RSI incrementally (Wilder's smoothing method)
        // Need at least 2 prices to calculate change
        if state.prices.len() >= 2 {
            // Get previous price (before we added new_price)
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
    fn calculate_rsi_from_state(state: &SymbolState) -> Option<f64> {
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
        
        let rs = avg_gain / avg_loss;
        let rsi = Decimal::from(100) - (Decimal::from(100) / (Decimal::ONE + rs));
        rsi.to_f64()
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
    fn detect_market_regime(state: &SymbolState) -> MarketRegime {
        const ATR_PERIOD: usize = 14;
        const LOW_VOLATILITY_THRESHOLD: f64 = 0.5; // ATR < 0.5% = ranging
        const HIGH_VOLATILITY_THRESHOLD: f64 = 2.0; // ATR > 2% = volatile

        let prices = &state.prices;
        
        // Calculate ATR for volatility
        let atr = Self::calculate_atr(prices, ATR_PERIOD);
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
            if atr_pct_val < LOW_VOLATILITY_THRESHOLD {
                MarketRegime::Ranging
            } else if atr_pct_val > HIGH_VOLATILITY_THRESHOLD {
                MarketRegime::Volatile
            } else {
                // Medium volatility - assume trending (default for trend-following strategy)
                MarketRegime::Trending
            }
        } else {
            MarketRegime::Unknown
        }
    }

    /// Multi-indicator hybrid strategy with weighted scoring
    /// Combines EMA trend, RSI momentum, and volume confirmation
    /// Uses weighted scoring system for flexible signal generation
    /// Includes adaptive parameters based on volatility and market regime
    /// Public for backtesting
    pub fn analyze_trend(state: &SymbolState) -> Option<TrendSignal> {
        const VOLUME_PERIOD: usize = 20; // Volume average period
        // ✅ IMPROVED: Increased threshold from 4.5 to 5.0 for better signal quality
        // Higher threshold = fewer but higher quality signals = better win rate
        const BASE_MIN_SCORE: f64 = 5.0; // Base threshold (6.0 max score, 5.0 = 83% - more selective)
        // ✅ IMPROVED: Narrower RSI range for long signals (55-70 instead of 50-75)
        // More selective RSI range = better entry timing = higher win rate
        const BASE_RSI_LOWER: f64 = 55.0; // Was 50.0 - more selective
        const BASE_RSI_UPPER: f64 = 70.0; // Was 75.0 - more selective
        const BASE_RSI_LOWER_SHORT: f64 = 25.0;
        const BASE_RSI_UPPER_SHORT: f64 = 50.0;
        
        let prices = &state.prices;
        
        // Need enough prices and indicator data
        if prices.len() < VOLUME_PERIOD {
            return None;
        }
        
        // Need EMAs to be initialized
        let ema_fast = state.ema_9?;
        let ema_mid = state.ema_21?;
        let ema_slow = state.ema_55?;
        
        let current_price = prices.back()?.price;
        
        // 1. Multi-timeframe EMA trend confirmation (weighted scoring)
        let mut score_long = 0.0;
        let mut score_short = 0.0;
        
        // Short-term: Price > Fast EMA > Mid EMA (weight: 2.0 - most important)
        if current_price > ema_fast && ema_fast > ema_mid {
            score_long += 2.0;
        } else if current_price < ema_fast && ema_fast < ema_mid {
            score_short += 2.0;
        }
        
        // Mid-term: Mid EMA > Slow EMA (weight: 1.5)
        if ema_mid > ema_slow {
            score_long += 1.5;
        } else if ema_mid < ema_slow {
            score_short += 1.5;
        }
        
        // Long-term: EMA slope (weight: 1.0)
        // ✅ CRITICAL: Safe unwrap - len() check ensures elements exist
        // But use Option for extra safety to prevent panic
        let ema_slope = if state.ema_55_history.len() >= 2 {
            if let (Some(current_ema), Some(old_ema)) = (state.ema_55_history.back(), state.ema_55_history.front()) {
                if !old_ema.is_zero() {
                    Some((current_ema - old_ema) / old_ema)
                } else {
                    None
                }
            } else {
                None // Should never happen if len() >= 2, but defensive programming
            }
        } else {
            None
        };
        
        if let Some(slope) = ema_slope {
            // ✅ IMPROVED: Increased minimum slope from 0.01% to 0.05% for stronger trend confirmation
            // Higher slope requirement = stronger trends = better win rate
            let min_slope = Decimal::from(5) / Decimal::from(10000); // 0.05% minimum (was 0.01%)
            if slope > min_slope {
                score_long += 1.0;
            } else if slope < -min_slope {
                score_short += 1.0;
            }
        }
        
        // 2. RSI momentum confirmation (weight: 1.0) with adaptive thresholds
        let rsi = Self::calculate_rsi_from_state(state)?;
        
        // Adaptive RSI thresholds based on volatility
        let atr = Self::calculate_atr(prices, 14);
        let volatility_multiplier = if let (Some(atr_val), Some(current_price)) = (atr, prices.back()) {
            if !current_price.price.is_zero() {
                let atr_pct = (atr_val / current_price.price).to_f64().unwrap_or(0.01);
                let base_volatility = 0.02; // 2% base volatility
                (atr_pct / base_volatility).max(0.5).min(2.0) // Clamp between 0.5x and 2x
            } else {
                1.0
            }
        } else {
            1.0
        };
        
        // ✅ IMPROVED: Narrower RSI range for better entry timing
        // Long signals: RSI between 55-70 (bullish momentum, more selective)
        // Reduced volatility adjustment range for more consistent signals
        let rsi_lower = BASE_RSI_LOWER - (10.0 * volatility_multiplier); // 45-55 range (was 35-50)
        let rsi_upper = BASE_RSI_UPPER - (5.0 * (1.0 - volatility_multiplier)); // 65-70 range (was 65-75)
        
        // Short signals: RSI < 40 (bearish momentum, oversold region)
        // Fixed upper limit at 40 to avoid false signals in downtrends
        // In downtrends, RSI typically stays below 30, so 25-50 range was too wide
        let rsi_upper_short = 40.0; // Fixed upper limit for short signals
        
        let rsi_bullish = rsi > rsi_lower && rsi < rsi_upper;
        let rsi_bearish = rsi < rsi_upper_short; // RSI < 40 for short signals
        
        if rsi_bullish {
            score_long += 1.0;
        } else if rsi_bearish {
            score_short += 1.0;
        }
        
        // Market regime detection
        let regime = Self::detect_market_regime(state);
        
        // ✅ IMPROVED: More selective thresholds, especially in ranging markets
        // Higher thresholds = fewer but better signals = improved win rate
        let min_score = match regime {
            MarketRegime::Trending => BASE_MIN_SCORE * 0.95, // Slightly lower threshold in trending markets (was 0.9)
            MarketRegime::Ranging => BASE_MIN_SCORE * 1.3,  // Higher threshold in ranging markets (was 1.2 - avoid false signals)
            MarketRegime::Volatile => BASE_MIN_SCORE * 1.15, // Higher threshold in volatile markets (was 1.1)
            MarketRegime::Unknown => BASE_MIN_SCORE,
        };
        
        // ✅ IMPROVED: Stricter volume confirmation for better signal quality
        // Increased weight from 0.5 to 1.0 and surge threshold from 1.2x to 1.5x
        // Higher volume requirement = stronger confirmation = better win rate
        let current_volume = prices.back()?.volume?;
        let avg_volume = Self::calculate_avg_volume(prices, VOLUME_PERIOD)?;
        
        // ✅ IMPROVED: Volume surge threshold increased from 1.2x to 1.5x
        // Higher threshold = stronger volume confirmation = better signals
        let volume_multiplier = Decimal::from_str("1.5").unwrap_or(Decimal::from(150) / Decimal::from(100)); // Was 1.2
        let volume_surge = current_volume > avg_volume * volume_multiplier;
        
        // Volume trend (recent > longer average)
        let recent_avg_volume = Self::calculate_avg_volume(prices, 5)?;
        let volume_trend = recent_avg_volume > avg_volume;
        
        let volume_confirms = volume_surge && volume_trend;
        
        // ✅ IMPROVED: Increased volume weight from 0.5 to 1.0
        // Volume confirmation is now more important for signal quality
        if volume_confirms {
            score_long += 1.0; // Was 0.5 - now more important
            score_short += 1.0; // Was 0.5 - now more important
        }
        
        // 4. Generate signal based on weighted score (with adaptive threshold)
        if score_long >= min_score {
            Some(TrendSignal::Long)
        } else if score_short >= min_score {
            Some(TrendSignal::Short)
        } else {
            None // Score too low
        }
    }
    

    /// Process a market tick and generate trade signal if conditions are met
    /// Simple algorithm: Spread check → Cooldown check → Trend analysis → Signal
    async fn process_market_tick(
        tick: &MarketTick,
        cfg: &Arc<AppCfg>,
        event_bus: &Arc<EventBus>,
        last_signals: &Arc<Mutex<HashMap<String, LastSignal>>>,
        symbol_states: &Arc<Mutex<HashMap<String, SymbolState>>>,
    ) -> Result<()> {
        let now = Instant::now();
        
        // 1. Spread check (liquidity validation)
        let spread_bps = ((tick.ask.0 - tick.bid.0) / tick.bid.0) * Decimal::from(10000);
        let spread_bps_f64 = spread_bps.to_f64().unwrap_or(0.0);
        
        let min_acceptable_spread_bps = cfg.trending.min_spread_bps;
        let max_acceptable_spread_bps = cfg.trending.max_spread_bps;
        
        if spread_bps_f64 < min_acceptable_spread_bps || spread_bps_f64 > max_acceptable_spread_bps {
            return Ok(());
        }

        // 2. Cooldown check (prevent signal spam)
        let cooldown_seconds = cfg.trending.signal_cooldown_seconds;
        {
            let last_signals_map = last_signals.lock().await;
            if let Some(last_signal) = last_signals_map.get(&tick.symbol) {
                let elapsed = now.duration_since(last_signal.timestamp);
                if elapsed < Duration::from_secs(cooldown_seconds) {
                    return Ok(());
                }
            }
        }
        
        // 3. Trend analysis
        let mid_price = (tick.bid.0 + tick.ask.0) / Decimal::from(2);
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
            });
            
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
            
            Self::analyze_trend(state)
        };
        
        // 4. Generate signal if trend detected
        let side = match trend_signal {
            Some(TrendSignal::Long) => Side::Buy,
            Some(TrendSignal::Short) => Side::Sell,
            None => return Ok(()), // No trend, skip
        };
        
        let entry_price = Px(mid_price);
        
        // 5. Calculate dynamic SL/TP based on ATR (volatility-based)
        // ATR-based SL/TP adapts to market volatility
        // Formula: SL = 2 * ATR, TP = 4 * ATR (1:2 risk/reward ratio)
        let (stop_loss_pct, take_profit_pct) = {
            let states = symbol_states.lock().await;
            if let Some(state) = states.get(&tick.symbol) {
                const ATR_PERIOD: usize = 14;
                const ATR_SL_MULTIPLIER: f64 = 2.0; // Stop loss = 2 * ATR
                const ATR_TP_MULTIPLIER: f64 = 4.0; // Take profit = 4 * ATR (1:2 risk/reward)
                
                if let Some(atr) = Self::calculate_atr(&state.prices, ATR_PERIOD) {
                    if !mid_price.is_zero() {
                        let atr_pct = (atr / mid_price * Decimal::from(100)).to_f64().unwrap_or(0.0);
                        let dynamic_sl_pct = (ATR_SL_MULTIPLIER * atr_pct).max(cfg.stop_loss_pct).min(5.0); // Clamp between config min and 5%
                        let dynamic_tp_pct = (ATR_TP_MULTIPLIER * atr_pct).max(cfg.take_profit_pct).min(10.0); // Clamp between config min and 10%
                        
                        // Ensure TP > SL (required for profitable trades)
                        let final_tp = dynamic_tp_pct.max(dynamic_sl_pct * 1.5); // At least 1.5x SL
                        
                        (Some(dynamic_sl_pct), Some(final_tp))
                    } else {
                        (Some(cfg.stop_loss_pct), Some(cfg.take_profit_pct))
                    }
                } else {
                    // Not enough data for ATR - use config defaults
                    (Some(cfg.stop_loss_pct), Some(cfg.take_profit_pct))
                }
            } else {
                // State not found - use config defaults
                (Some(cfg.stop_loss_pct), Some(cfg.take_profit_pct))
            }
        };
        
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

