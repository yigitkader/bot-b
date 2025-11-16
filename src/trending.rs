// TRENDING: Trend analysis, generates TradeSignal
// Only does trend analysis, never places orders
// Subscribes to MarketTick events, publishes TradeSignal events
// Uses Connection for symbol rules validation and balance checks

use crate::config::AppCfg;
use crate::connection::Connection;
use crate::event_bus::{EventBus, MarketTick, TradeSignal};
use crate::state::SharedState;
use crate::types::{LastSignal, PositionDirection, PricePoint, Px, Qty, Side, SymbolState, TrendSignal};
use anyhow::Result;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::collections::{HashMap, VecDeque};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, Mutex};
use tracing::{debug, error, info, warn};

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
    shared_state: Arc<SharedState>,
    connection: Arc<Connection>,
    /// Track last signal per symbol (side and timestamp) for cooldown and direction checking
    /// Prevents generating same-direction signals repeatedly
    last_signals: Arc<Mutex<HashMap<String, LastSignal>>>,
    /// Track symbol state for trend analysis (symbol -> SymbolState)
    /// Contains price history, volume, and signal timing
    symbol_states: Arc<Mutex<HashMap<String, SymbolState>>>,
    /// Track recent volatility per symbol (last 60 ticks ≈ 1 hour)
    /// Used to skip signals when volatility is too low
    recent_volatility_tracker: Arc<Mutex<HashMap<String, f64>>>,
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
        shared_state: Arc<SharedState>,
        connection: Arc<Connection>,
    ) -> Self {
        Self {
            cfg,
            event_bus,
            shutdown_flag,
            shared_state,
            connection,
            last_signals: Arc::new(Mutex::new(HashMap::new())),
            symbol_states: Arc::new(Mutex::new(HashMap::new())),
            recent_volatility_tracker: Arc::new(Mutex::new(HashMap::new())),
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
        let shared_state = self.shared_state.clone();
        let connection = self.connection.clone();
        let last_signals = self.last_signals.clone();
        let symbol_states = self.symbol_states.clone();
        let recent_volatility_tracker = self.recent_volatility_tracker.clone();
        
        // Spawn task for MarketTick events (signal generation)
        let event_bus_tick = event_bus.clone();
        let shutdown_flag_tick = shutdown_flag.clone();
        let cfg_tick = cfg.clone();
        let shared_state_tick = shared_state.clone();
        let connection_tick = connection.clone();
        let last_signals_tick = last_signals.clone();
        let symbol_states_tick = symbol_states.clone();
        let recent_volatility_tracker_tick = recent_volatility_tracker.clone();
        tokio::spawn(async move {
            let mut market_tick_rx = event_bus_tick.subscribe_market_tick();
            
            info!("TRENDING: Started, listening to MarketTick events");
            
            loop {
                // Check shutdown flag before waiting for events
                if shutdown_flag_tick.load(AtomicOrdering::Relaxed) {
                    break;
                }
                
                match market_tick_rx.recv().await {
                    Ok(tick) => {
                        if shutdown_flag_tick.load(AtomicOrdering::Relaxed) {
                            break;
                        }
                        
                        // Process market tick and generate trade signal if needed
                        if let Err(e) = Self::process_market_tick(
                            &tick,
                            &cfg_tick,
                            &event_bus_tick,
                            &shared_state_tick,
                            &connection_tick,
                            &last_signals_tick,
                            &symbol_states_tick,
                            &recent_volatility_tracker_tick,
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

    /// Calculate Simple Moving Average (SMA) for the last N prices
    fn calculate_sma(prices: &VecDeque<PricePoint>, period: usize) -> Option<Decimal> {
        if prices.len() < period {
            return None;
        }
        
        let recent: Decimal = prices
            .iter()
            .rev()
            .take(period)
            .map(|p| p.price)
            .sum();
        
        Some(recent / Decimal::from(period))
    }
    
    /// Calculate momentum (price change over last N periods)
    /// Returns the percentage change from (period + 1) prices ago to now
    /// 
    /// Example: period=5 means compare price from 6 prices ago to current price
    /// This gives us the momentum over the last 5 price movements
    fn calculate_momentum(prices: &VecDeque<PricePoint>, period: usize) -> Option<f64> {
        // Need at least (period + 1) prices to calculate momentum
        if prices.len() <= period {
            return None;
        }
        
        // Use iterator instead of index calculation to avoid off-by-one errors
        let iter: Vec<_> = prices.iter().rev().take(period + 1).collect();
        
        // Get first and last prices from the collected iterator
        // iter.first() = current price (newest, most recent)
        // iter.last() = price from (period + 1) prices ago (oldest in our window)
        let first_price = iter.last()?.price; // En eski (period + 1 önceki)
        let last_price = iter.first()?.price;  // En yeni (current)
        
        // Calculate percentage change: (new - old) / old * 100
        let change_pct = ((last_price - first_price) / first_price) * Decimal::from(100);
        change_pct.to_f64()
    }
    
    /// Analyze trend for a specific timeframe
    /// Returns Some(TrendSignal) if clear trend detected, None otherwise
    fn analyze_trend_timeframe(prices: &VecDeque<PricePoint>, period: usize, threshold_pct: f64) -> Option<TrendSignal> {
        if prices.len() < period {
            return None;
        }
        
        // Calculate SMA of last period prices
        let sma = Self::calculate_sma(prices, period)?;
        let current_price = prices.back()?.price;
        
        // Calculate price deviation from SMA
        let deviation_pct = ((current_price - sma) / sma) * Decimal::from(100);
        let deviation_pct_f64 = deviation_pct.to_f64().unwrap_or(0.0);
        
        // Determine trend:
        // - Price > SMA * (1 + threshold) → Uptrend → Long signal
        // - Price < SMA * (1 - threshold) → Downtrend → Short signal
        // - Otherwise → No clear trend
        if deviation_pct_f64 > threshold_pct {
            Some(TrendSignal::Long)
        } else if deviation_pct_f64 < -threshold_pct {
            Some(TrendSignal::Short)
        } else {
            None
        }
    }
    
    /// Analyze trend for a specific timeframe (with detailed logging)
    /// Returns Some(TrendSignal) if clear trend detected, None otherwise
    /// Also returns detailed analysis info for logging
    fn analyze_trend_timeframe_with_details(prices: &VecDeque<PricePoint>, period: usize, threshold_pct: f64) -> (Option<TrendSignal>, String) {
        if prices.len() < period {
            return (None, format!("insufficient_prices (need {}, have {})", period, prices.len()));
        }
        
        // Calculate SMA of last period prices
        let sma = match Self::calculate_sma(prices, period) {
            Some(s) => s,
            None => return (None, "failed_to_calculate_sma".to_string()),
        };
        let current_price = match prices.back() {
            Some(p) => p.price,
            None => return (None, "no_current_price".to_string()),
        };
        
        // Calculate price deviation from SMA
        let deviation_pct = ((current_price - sma) / sma) * Decimal::from(100);
        let deviation_pct_f64 = deviation_pct.to_f64().unwrap_or(0.0);
        
        // Determine trend:
        // - Price > SMA * (1 + threshold) → Uptrend → Long signal
        // - Price < SMA * (1 - threshold) → Downtrend → Short signal
        // - Otherwise → No clear trend
        let trend = if deviation_pct_f64 > threshold_pct {
            Some(TrendSignal::Long)
        } else if deviation_pct_f64 < -threshold_pct {
            Some(TrendSignal::Short)
        } else {
            None
        };
        
        let details = format!("sma={:.6}, current={:.6}, deviation={:.4}%, threshold={:.4}%", 
            sma, current_price, deviation_pct_f64, threshold_pct);
        
        (trend, details)
    }
    
    /// Calculate average volume over a period
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
    
    /// Check volume confirmation - volume should align with trend direction
    fn check_volume_confirmation(prices: &VecDeque<PricePoint>, trend: TrendSignal, period: usize) -> bool {
        // Need at least period + 1 prices to compare recent vs older volume
        if prices.len() < period + 1 {
            return false;
        }
        
        // Calculate recent volume (last period/2 prices)
        let recent_period = period / 2;
        let recent_vol = Self::calculate_avg_volume(prices, recent_period);
        let older_vol = {
            if prices.len() < period {
                return false;
            }
            // Get volume from period/2 to period prices ago
            let older_prices: Vec<Decimal> = prices
                .iter()
                .rev()
                .skip(recent_period)
                .take(recent_period)
                .filter_map(|p| p.volume)
                .collect();
            
            if older_prices.is_empty() {
                return false;
            }
            
            let sum: Decimal = older_prices.iter().sum();
            Some(sum / Decimal::from(older_prices.len()))
        };
        
        match (recent_vol, older_vol) {
            (Some(recent), Some(older)) if !older.is_zero() => {
                // Volume change percentage
                let vol_change_pct = ((recent - older) / older) * Decimal::from(100);
                let vol_change_pct_f64 = vol_change_pct.to_f64().unwrap_or(0.0);
                
                // For uptrend (Long): volume should be increasing or at least stable
                // For downtrend (Short): volume should be increasing (selling pressure) or at least stable
                // Require at least -10% volume change (can decrease slightly but not collapse)
                match trend {
                    TrendSignal::Long => vol_change_pct_f64 >= -10.0, // Volume can decrease slightly but not collapse
                    TrendSignal::Short => vol_change_pct_f64 >= -10.0, // Same for shorts
                }
            }
            _ => false, // No volume data available
        }
    }
    
    /// Analyze trend based on price history with multiple timeframes and volume confirmation
    /// Returns Some(TrendSignal) if clear trend detected across multiple timeframes, None otherwise
    fn analyze_trend(state: &SymbolState) -> Option<TrendSignal> {
        // Increased minimum prices for more reliable analysis
        const MIN_PRICES_FOR_ANALYSIS: usize = 25;
        const SMA_PERIOD_SHORT: usize = 10;  // Short-term SMA (5-min equivalent)
        const SMA_PERIOD_MEDIUM: usize = 15; // Medium-term SMA (15-min equivalent)
        const SMA_PERIOD_LONG: usize = 20;   // Long-term SMA (1-hour equivalent)
        // Reduced threshold for high-frequency trading
        // High-frequency strategy: many small profits (50 cents per trade, 100+ trades/day)
        // Lower threshold allows more signals while still filtering noise
        // Works for all price scales (BTC $100k, altcoins $200) - percentage-based
        // Real market deviations are typically 0.001-0.0035% (1-3.5 bps), so 0.005% (5 bps) is appropriate
        // This allows detection of small but consistent price movements while filtering noise
        const TREND_THRESHOLD_PCT: f64 = 0.005; // 0.005% threshold (5 bps) - adjusted for real market conditions
        const VOLUME_CONFIRMATION_PERIOD: usize = 10; // Period for volume analysis
        
        let prices = &state.prices;
        
        // Need at least MIN_PRICES_FOR_ANALYSIS prices for reliable analysis
        if prices.len() < MIN_PRICES_FOR_ANALYSIS {
            return None;
        }
        
        // Multi-timeframe analysis: check trends across different periods
        let (trend_short, details_short) = Self::analyze_trend_timeframe_with_details(prices, SMA_PERIOD_SHORT, TREND_THRESHOLD_PCT);
        let (trend_medium, details_medium) = Self::analyze_trend_timeframe_with_details(prices, SMA_PERIOD_MEDIUM, TREND_THRESHOLD_PCT);
        let (trend_long, details_long) = Self::analyze_trend_timeframe_with_details(prices, SMA_PERIOD_LONG, TREND_THRESHOLD_PCT);
        
        // Log detailed analysis for debugging
        debug!(
            price_count = prices.len(),
            short_period = SMA_PERIOD_SHORT,
            medium_period = SMA_PERIOD_MEDIUM,
            long_period = SMA_PERIOD_LONG,
            threshold_pct = TREND_THRESHOLD_PCT,
            short_trend = ?trend_short,
            medium_trend = ?trend_medium,
            long_trend = ?trend_long,
            short_details = %details_short,
            medium_details = %details_medium,
            long_details = %details_long,
            "TRENDING: Multi-timeframe trend analysis details"
        );
        
        // Require at least 1 out of 3 timeframes to agree on trend direction (relaxed from 2/3)
        // This allows more signals while still requiring some consensus
        // For stronger signals, we can still require 2/3, but 1/3 is more sensitive
        let trends: Vec<Option<TrendSignal>> = vec![trend_short, trend_medium, trend_long];
        let long_count = trends.iter().filter(|&&t| t == Some(TrendSignal::Long)).count();
        let short_count = trends.iter().filter(|&&t| t == Some(TrendSignal::Short)).count();
        
        // Determine final trend signal
        // Changed from >= 2 to >= 1 to allow signals with single timeframe agreement
        // This is more sensitive but still requires some trend indication
        let final_trend = if long_count >= 1 {
            Some(TrendSignal::Long)
        } else if short_count >= 1 {
            Some(TrendSignal::Short)
        } else {
            None // No clear consensus across timeframes
        };
        
        // Volume confirmation: require volume to support the trend
        // Configurable via cfg.trending.require_volume_confirmation and cfg.trending.hft_mode
        // High-frequency strategy prioritizes price action over volume
        // Volume can be low in liquid markets but still profitable for small moves
        if let Some(trend) = final_trend {
            let volume_confirmed = Self::check_volume_confirmation(prices, trend, VOLUME_CONFIRMATION_PERIOD);
            if volume_confirmed {
                return Some(trend);
            } else {
                // Trend detected but volume doesn't confirm
                // Check if volume confirmation is required (config-based)
                // If hft_mode=true and require_volume_confirmation=false, allow signal anyway
                // Otherwise, block signal if volume confirmation is required
                // Note: This check is done in process_market_tick, not here
                // This function just returns the trend signal, volume check happens later
                debug!(
                    "TRENDING: Trend detected but volume not confirmed - will check config in process_market_tick"
                );
                return Some(trend); // Return trend, volume check happens in process_market_tick
            }
        }
        
        None
    }
    
    /// Update recent volatility tracker for a symbol
    /// Calculates average volatility from last 60 ticks (≈ 1 hour)
    /// Returns true if volatility is sufficient for trading, false otherwise
    async fn update_recent_volatility(
        symbol: &str,
        symbol_states: &Arc<Mutex<HashMap<String, SymbolState>>>,
        recent_volatility_tracker: &Arc<Mutex<HashMap<String, f64>>>,
    ) -> bool {
        const MIN_VOLATILITY_PCT: f64 = 0.5; // Minimum 0.5% volatility to generate signals
        const VOLATILITY_WINDOW: usize = 60; // Last 60 ticks (≈ 1 hour)

        let states = symbol_states.lock().await;
        if let Some(state) = states.get(symbol) {
            // Calculate volatility from last 60 ticks
            if state.prices.len() >= VOLATILITY_WINDOW {
                let recent_prices: Vec<_> = state.prices.iter().rev().take(VOLATILITY_WINDOW).collect();
                let mut changes = Vec::new();

                for i in 1..recent_prices.len() {
                    let prev = recent_prices[i].price;
                    let curr = recent_prices[i - 1].price;
                    if !prev.is_zero() {
                        let change = ((curr - prev) / prev).abs() * Decimal::from(100);
                        if let Some(change_f64) = change.to_f64() {
                            changes.push(change_f64);
                        }
                    }
                }

                if !changes.is_empty() {
                    let avg_volatility: f64 = changes.iter().sum::<f64>() / changes.len() as f64;
                    
                    // Update tracker
                    let mut tracker = recent_volatility_tracker.lock().await;
                    tracker.insert(symbol.to_string(), avg_volatility);

                    // Check if volatility is sufficient
                    if avg_volatility < MIN_VOLATILITY_PCT {
                        debug!(
                            symbol = %symbol,
                            volatility = avg_volatility,
                            "TRENDING: Low volatility detected ({}%), skipping signals",
                            avg_volatility
                        );
                        return false; // Volatility too low, skip signals
                    }
                }
            }
        }
        true // Volatility OK or not enough data yet
    }

    /// Process a market tick and generate trade signal if conditions are met
    /// 
    /// CRITICAL: Event flood prevention with sampling
    /// 
    /// Problem: MarketTick events flood the event bus
    /// - 100 symbols × 1 tick/sec = 100 events/sec
    /// - 24/7 operation: 100 events/sec × 86400 sec = 8.64M events/day
    /// - This causes high CPU usage and event bus congestion
    /// 
    /// Solution: Sampling - process only a fraction of ticks
    /// - Sample rate: 1/10 (process 10% of ticks, skip 90%)
    /// - Trend analysis doesn't need every single tick
    /// - Reduces event processing by 90% while maintaining signal quality
    /// - Uses deterministic sampling (symbol hash) for consistent behavior per symbol
    async fn process_market_tick(
        tick: &MarketTick,
        cfg: &Arc<AppCfg>,
        event_bus: &Arc<EventBus>,
        shared_state: &Arc<SharedState>,
        connection: &Arc<Connection>,
        last_signals: &Arc<Mutex<HashMap<String, LastSignal>>>,
        symbol_states: &Arc<Mutex<HashMap<String, SymbolState>>>,
        recent_volatility_tracker: &Arc<Mutex<HashMap<String, f64>>>,
    ) -> Result<()> {
        let now = Instant::now();
        
        // Sampling to prevent event flood - process only 1/10 of ticks
        // Trend analysis doesn't need every single tick - 10% is sufficient for signal quality
        // Uses per-symbol counter for even distribution across symbols
        // Each symbol gets processed at different rates, ensuring all symbols are eventually processed
        const SAMPLE_RATE: u32 = 10; // Process 1 out of 10 ticks
        
        // Use per-symbol counter for consistent sampling per symbol
        // This ensures each symbol gets processed at a consistent rate
        let should_process = {
            let mut states = symbol_states.lock().await;
            let state = states.entry(tick.symbol.clone()).or_insert_with(|| SymbolState {
                symbol: tick.symbol.clone(),
                prices: VecDeque::new(),
                last_signal_time: None,
                last_position_close_time: None,
                last_position_direction: None,
                tick_counter: 0,
            });
            
            // Increment counter for every tick (even if not processed)
            state.tick_counter = state.tick_counter.wrapping_add(1);
            
            // Process only 1 out of SAMPLE_RATE ticks
            state.tick_counter % SAMPLE_RATE == 0
        };
        
        if !should_process {
            // Skip this tick (90% of ticks are skipped)
            debug!(
                symbol = %tick.symbol,
                "TRENDING: Tick skipped (sampling: processing 1/10 ticks)"
            );
            return Ok(());
        }

        // ✅ Real-time volatility check: Update and validate volatility
        // Skip signals if recent volatility is too low (< 0.5%)
        // This prevents trading in low-volatility markets where signals are less reliable
        let volatility_ok = Self::update_recent_volatility(
            &tick.symbol,
            symbol_states,
            recent_volatility_tracker,
        ).await;
        
        if !volatility_ok {
            // Volatility too low, skip signal generation
            return Ok(());
        }
        
        // Check if ANY position or order is already open
        // 1. TRENDING generates BUY signal
        // 2. ORDERING places order
        // 3. Position opens
        // 4. TP triggers, position closes
        // 5. TRENDING might generate new BUY signal at the same time as close order
        // Solution: Block ALL signal generation when ANY position/order exists
        {
            let ordering_state = shared_state.ordering_state.lock().await;
            let has_position = ordering_state.open_position.is_some();
            let has_order = ordering_state.open_order.is_some();
            
            if has_position || has_order {
                // Position or order already exists, skip signal generation
                // This ensures we only have one position/order at a time
                debug!(
                    symbol = %tick.symbol,
                    has_position,
                    has_order,
                    "TRENDING: Skipping signal generation - position or order already exists"
                );
                return Ok(());
            }
        }
        
        // Symbol-based cooldown after position close (direction-aware)
        // Reduced for high-frequency trading (faster re-entry)
        const POSITION_CLOSE_COOLDOWN_SECS: u64 = 2;
        
        // ✅ CRITICAL: Position close cooldown check BEFORE expensive trend analysis
        // Problem: Position close cooldown check was done AFTER trend analysis
        // This caused unnecessary CPU time waste when cooldown is active
        //
        // Solution: Do early cooldown check first (elapsed < COOLDOWN), then trend analysis
        // Direction check still needs signal direction, so it's done after trend analysis
        let last_position_close_info = {
            let states = symbol_states.lock().await;
            if let Some(state) = states.get(&tick.symbol) {
                state.last_position_close_time
                    .map(|close_time| {
                        let elapsed = now.duration_since(close_time);
                        (state.last_position_direction, elapsed)
                    })
            } else {
                None
            }
        };
        
        // EARLY COOLDOWN CHECK: If still in cooldown, skip expensive trend analysis
        // This prevents wasting CPU time on trend analysis when we'll discard the signal anyway
        if let Some((last_direction, elapsed)) = &last_position_close_info {
            // Check if we're still in cooldown period
            if elapsed < &Duration::from_secs(POSITION_CLOSE_COOLDOWN_SECS) {
                // Still in cooldown - skip early (no trend analysis)
                // Note: We can't do direction check here because we don't have signal direction yet
                // Direction check will be done after trend analysis (if cooldown passed)
                debug!(
                    symbol = %tick.symbol,
                    elapsed_secs = elapsed.as_secs(),
                    cooldown_secs = POSITION_CLOSE_COOLDOWN_SECS,
                    "TRENDING: Skipping signal generation - position close cooldown active (early exit, no trend analysis)"
                );
                return Ok(());
            }
            
            // Extended cooldown for unknown direction (check before trend analysis)
            if last_direction.is_none() {
                const EXTENDED_COOLDOWN_SECS: u64 = POSITION_CLOSE_COOLDOWN_SECS * 2; // 10 seconds
                if elapsed < &Duration::from_secs(EXTENDED_COOLDOWN_SECS) {
                    debug!(
                        symbol = %tick.symbol,
                        last_direction = ?last_direction,
                        elapsed_secs = elapsed.as_secs(),
                        extended_cooldown_secs = EXTENDED_COOLDOWN_SECS,
                        "TRENDING: Skipping signal generation - extended cooldown active for unknown direction (early exit, no trend analysis)"
                    );
                    return Ok(());
                }
            }
        }
        
        // Cooldown check BEFORE expensive trend analysis
        let cooldown_seconds = cfg.trending.signal_cooldown_seconds;
        {
            let last_signals_map = last_signals.lock().await;
            if let Some(last_signal) = last_signals_map.get(&tick.symbol) {
                let elapsed = now.duration_since(last_signal.timestamp);
                
                // Check cooldown period - if still in cooldown, skip expensive trend analysis
                if elapsed < Duration::from_secs(cooldown_seconds) {
                    // Still in cooldown, skip signal generation (early exit, no trend analysis)
                    debug!(
                        symbol = %tick.symbol,
                        elapsed_secs = elapsed.as_secs(),
                        cooldown_seconds,
                        last_signal_side = ?last_signal.side,
                        "TRENDING: Skipping signal generation - signal cooldown active"
                    );
                    return Ok(());
                }
                // Cooldown passed, continue with trend analysis
            }
        }
        
        // Calculate spread (bid-ask spread in basis points)
        let spread_bps = ((tick.ask.0 - tick.bid.0) / tick.bid.0) * Decimal::from(10000);
        let spread_bps_f64 = spread_bps.to_f64().unwrap_or(0.0);
        
        // Only trade when spread is within acceptable range
        // Acceptable range: min_spread_bps <= spread <= max_spread_bps
        let min_acceptable_spread_bps = cfg.trending.min_spread_bps;
        let max_acceptable_spread_bps = cfg.trending.max_spread_bps;
        
        // Log spread details for debugging
        debug!(
            symbol = %tick.symbol,
            spread_bps = spread_bps_f64,
            min_acceptable_spread_bps,
            max_acceptable_spread_bps,
            bid = %tick.bid.0,
            ask = %tick.ask.0,
            spread_ok = spread_bps_f64 >= min_acceptable_spread_bps && spread_bps_f64 <= max_acceptable_spread_bps,
            "TRENDING: Spread check"
        );
        
        // Check if spread is within acceptable range
        if spread_bps_f64 < min_acceptable_spread_bps || spread_bps_f64 > max_acceptable_spread_bps {
            // Spread out of acceptable range → skip signal
            // Too narrow: potential flash crash, liquidity trap, stale data
            // Too wide: low liquidity, high slippage risk
            debug!(
                symbol = %tick.symbol,
                spread_bps = spread_bps_f64,
                min_acceptable_spread_bps,
                max_acceptable_spread_bps,
                bid = %tick.bid.0,
                ask = %tick.ask.0,
                reason = if spread_bps_f64 < min_acceptable_spread_bps { "too_narrow" } else { "too_wide" },
                "TRENDING: Spread out of acceptable range - skipping signal"
            );
            return Ok(());
        }
        
        // Store spread information and timestamp for validation at order placement
        let spread_timestamp = now;
        
        // Calculate mid price for trend analysis
        let mid_price = (tick.bid.0 + tick.ask.0) / Decimal::from(2);
        let current_price = mid_price;
        
        // Update symbol state with new price point
        // Expensive trend analysis only after cooldown check passed
        let trend_signal = {
            let mut states = symbol_states.lock().await;
            let state = states.entry(tick.symbol.clone()).or_insert_with(|| {
                                SymbolState {
                                    symbol: tick.symbol.clone(),
                                    prices: VecDeque::new(),
                                    last_signal_time: None,
                                    last_position_close_time: None,
                                    last_position_direction: None,
                                    tick_counter: 0,
                                }
            });
            
            // Add current price point to history
            state.prices.push_back(PricePoint {
                timestamp: now,
                price: current_price,
                volume: tick.volume,
            });
            
            // Keep more prices for multi-timeframe analysis (need at least 25-30 for analysis)
            // Store up to 100 prices to support multiple timeframe analysis
            const MAX_HISTORY: usize = 100;
            while state.prices.len() > MAX_HISTORY {
                state.prices.pop_front();
            }
            
            // Analyze trend using SMA and momentum
            Self::analyze_trend(state)
        };
        
        // Generate signal only if clear trend is detected
        debug!(
            symbol = %tick.symbol,
            trend_signal = ?trend_signal,
            "TRENDING: Trend analysis completed, checking if signal should be generated"
        );
        
        // ✅ Volume confirmation check (config-based)
        // If require_volume_confirmation=true, block signal if volume doesn't confirm
        // If hft_mode=true and require_volume_confirmation=false, allow signal even without volume
        if let Some(trend) = trend_signal {
            let volume_confirmed = {
                let states = symbol_states.lock().await;
                if let Some(state) = states.get(&tick.symbol) {
                    Self::check_volume_confirmation(&state.prices, trend, 10)
                } else {
                    false
                }
            };
            
            if !volume_confirmed && cfg.trending.require_volume_confirmation {
                // Volume confirmation required but not confirmed - block signal
                debug!(
                    symbol = %tick.symbol,
                    trend = ?trend,
                    "TRENDING: Trend detected but volume not confirmed - blocking signal (require_volume_confirmation=true)"
                );
                return Ok(());
            } else if !volume_confirmed && cfg.trending.hft_mode {
                // HFT mode: allow signal even without volume confirmation
                debug!(
                    symbol = %tick.symbol,
                    trend = ?trend,
                    "TRENDING: Trend detected but volume not confirmed - allowing signal anyway (HFT mode)"
                );
            } else if volume_confirmed {
                debug!(
                    symbol = %tick.symbol,
                    trend = ?trend,
                    "TRENDING: Trend detected and volume confirmed - proceeding with signal"
                );
            }
        }
        
        let side = match trend_signal {
            Some(TrendSignal::Long) => {
                debug!(
                    symbol = %tick.symbol,
                    trend = "Long",
                    "TRENDING: Long trend detected, proceeding with signal generation"
                );
                Side::Buy
            }
            Some(TrendSignal::Short) => {
                debug!(
                    symbol = %tick.symbol,
                    trend = "Short",
                    "TRENDING: Short trend detected, proceeding with signal generation"
                );
                Side::Sell
            }
            None => {
                // No clear trend detected, skip signal generation
                // This prevents trading in sideways markets
                // Calculate price count and get trend analysis details before debug! to avoid Send issues
                let (price_count, trend_details) = {
                    let states = symbol_states.lock().await;
                    let state = states.get(&tick.symbol);
                    let count = state.map(|s| s.prices.len()).unwrap_or(0);
                    
                    // Get trend analysis details for debugging
                    let details = if let Some(s) = state {
                        if s.prices.len() >= 25 {
                            // Analyze trend to get details with full information
                            // Use same threshold as analyze_trend function
                            const TREND_THRESHOLD: f64 = 0.005; // Match TREND_THRESHOLD_PCT from analyze_trend
                            let (trend_short, details_short) = Self::analyze_trend_timeframe_with_details(&s.prices, 10, TREND_THRESHOLD);
                            let (trend_medium, details_medium) = Self::analyze_trend_timeframe_with_details(&s.prices, 15, TREND_THRESHOLD);
                            let (trend_long, details_long) = Self::analyze_trend_timeframe_with_details(&s.prices, 20, TREND_THRESHOLD);
                            
                            // Count consensus
                            let trends = vec![trend_short, trend_medium, trend_long];
                            let long_count = trends.iter().filter(|&&t| t == Some(TrendSignal::Long)).count();
                            let short_count = trends.iter().filter(|&&t| t == Some(TrendSignal::Short)).count();
                            
                            // Check volume confirmation if trend exists
                            let volume_info = if long_count >= 2 {
                                let vol_confirmed = Self::check_volume_confirmation(&s.prices, TrendSignal::Long, 10);
                                format!(", volume_confirmed={}", vol_confirmed)
                            } else if short_count >= 2 {
                                let vol_confirmed = Self::check_volume_confirmation(&s.prices, TrendSignal::Short, 10);
                                format!(", volume_confirmed={}", vol_confirmed)
                            } else {
                                String::new()
                            };
                            
                            format!("short={:?} ({}) medium={:?} ({}) long={:?} ({}) consensus=(long={}, short={}){}", 
                                trend_short, details_short, trend_medium, details_medium, trend_long, details_long, long_count, short_count, volume_info)
                        } else {
                            format!("insufficient_data (need 25, have {})", count)
                        }
                    } else {
                        "no_state".to_string()
                    };
                    (count, details)
                };
                
                debug!(
                    symbol = %tick.symbol,
                    price_count,
                    trend_analysis = %trend_details,
                    "TRENDING: No clear trend detected - skipping signal (sideways market or insufficient data)"
                );
                return Ok(());
            }
        };
        
        // ✅ Direction check AFTER trend analysis (needs signal direction)
        // This allows opposite-direction signals immediately (trend reversal)
        // Note: Basic cooldown check was already done before trend analysis (early exit)
        // This check only handles direction matching for same-direction signals
        if let Some((last_direction, elapsed)) = &last_position_close_info {
            // Cooldown period passed (checked earlier), but check direction match
            // Convert signal side to position direction for comparison
            let signal_direction = PositionDirection::from_order_side(side);
            
            // Direction is known - check if it matches signal direction
            if let Some(dir) = last_direction {
                let direction_matches = *dir == signal_direction;
                
                if direction_matches {
                    // Same direction as last closed position - but cooldown already passed
                    // Allow signal (cooldown was checked earlier)
                    debug!(
                        symbol = %tick.symbol,
                        signal_direction = ?signal_direction,
                        last_direction = ?last_direction,
                        elapsed_secs = elapsed.as_secs(),
                        "TRENDING: Same-direction signal allowed (cooldown passed)"
                    );
                } else {
                    // Opposite direction - allow signal (trend reversal)
                    debug!(
                        symbol = %tick.symbol,
                        signal_direction = ?signal_direction,
                        last_direction = ?last_direction,
                        "TRENDING: Allowing opposite-direction signal (trend reversal, cooldown bypassed)"
                    );
                }
            }
            // Note: last_direction.is_none() case already handled in early cooldown check
        }
        
        // Check same-direction signals (after trend analysis)
        // REMOVED: Same-direction blocking for high-frequency trading
        // High-frequency strategy needs to allow same-direction signals if trend continues
        // Cooldown period already prevents spam, so this check is redundant and too restrictive
        // if let Some(last_side) = last_signal_side {
        //     if last_side == side {
        //         // Same direction as last signal - skip to prevent spam
        //         debug!(
        //             symbol = %tick.symbol,
        //             side = ?side,
        //             last_side = ?last_side,
        //             "TRENDING: Same direction as last signal - skipping to prevent spam"
        //         );
        //         return Ok(());
        //     }
        // }
        
        // Additional validation: Check momentum for confirmation
        let momentum = {
            let states = symbol_states.lock().await;
            if let Some(state) = states.get(&tick.symbol) {
                let mom = Self::calculate_momentum(&state.prices, 5); // 5-period momentum
                match mom {
                    Some(m) => {
                        debug!(
                            symbol = %tick.symbol,
                            momentum_pct = m,
                            momentum_abs = m.abs(),
                            price_count = state.prices.len(),
                            "TRENDING: Momentum calculated for trend confirmation"
                        );
                    }
                    None => {
                        debug!(
                            symbol = %tick.symbol,
                            price_count = state.prices.len(),
                            reason = "insufficient_prices_for_momentum",
                            "TRENDING: Momentum calculation failed - insufficient price history (need 6 prices for 5-period momentum)"
                        );
                    }
                }
                mom
            } else {
                debug!(
                    symbol = %tick.symbol,
                    reason = "no_symbol_state",
                    "TRENDING: Momentum calculation failed - no symbol state found"
                );
                None
            }
        };
        
        // Require minimum momentum for signal confirmation
        // Reduced for high-frequency trading (many small profits)
        // Real market momentum is typically 0.007-0.021%, so 0.01% (1 bps) is more appropriate
        const MIN_MOMENTUM_PCT: f64 = 0.01; // Minimum 0.01% momentum required (1 bps - adjusted for real market conditions)
        
        match momentum {
            Some(mom) => {
                debug!(
                    symbol = %tick.symbol,
                    momentum_pct = mom,
                    momentum_abs = mom.abs(),
                    min_momentum_pct = MIN_MOMENTUM_PCT,
                    momentum_ok = mom.abs() >= MIN_MOMENTUM_PCT,
                    "TRENDING: Momentum validation check"
                );
                
                if mom.abs() < MIN_MOMENTUM_PCT {
                    // Momentum too weak, skip signal
                    debug!(
                        symbol = %tick.symbol,
                        momentum_pct = mom,
                        momentum_abs = mom.abs(),
                        min_momentum_pct = MIN_MOMENTUM_PCT,
                        deficit = MIN_MOMENTUM_PCT - mom.abs(),
                        "TRENDING: Momentum too weak - skipping signal"
                    );
                    return Ok(());
                }
                
                // Momentum should align with trend direction
                match trend_signal {
                    Some(TrendSignal::Long) if mom < 0.0 => {
                        // Long signal but negative momentum - conflicting signals
                        debug!(
                            symbol = %tick.symbol,
                            trend_signal = "Long",
                            momentum_pct = mom,
                            reason = "conflicting_signals",
                            "TRENDING: Conflicting signals - Long trend but negative momentum"
                        );
                        return Ok(());
                    }
                    Some(TrendSignal::Short) if mom > 0.0 => {
                        // Short signal but positive momentum - conflicting signals
                        debug!(
                            symbol = %tick.symbol,
                            trend_signal = "Short",
                            momentum_pct = mom,
                            reason = "conflicting_signals",
                            "TRENDING: Conflicting signals - Short trend but positive momentum"
                        );
                        return Ok(());
                    }
                    _ => {
                        debug!(
                            symbol = %tick.symbol,
                            trend_signal = ?trend_signal,
                            momentum_pct = mom,
                            "TRENDING: Momentum aligns with trend direction - proceeding"
                        );
                    }
                }
            }
            None => {
                debug!(
                    symbol = %tick.symbol,
                    reason = "momentum_not_available",
                    "TRENDING: Momentum not available - proceeding without momentum confirmation (HFT mode allows this)"
                );
                // For high-frequency trading, we allow signals without momentum if trend is clear
                // This is more aggressive but allows more signals
            }
        }
        
        // Calculate entry price (mid price)
        let entry_price = Px(current_price);
        
        // ✅ DYNAMIC MARGIN CALCULATION
        // Strategy: Use available balance (up to max_margin_usd), scaled by trend strength if enabled
        // Since we only have one position at a time, we can use all available balance (up to limit)
        
        // First, get available balance
        let available_balance = {
            let balance_store = shared_state.balance_store.read().await;
            if cfg.quote_asset.to_uppercase() == "USDT" {
                balance_store.usdt
            } else {
                balance_store.usdc
            }
        };
        
        let min_margin = Decimal::from_str(&cfg.min_margin_usd.to_string()).unwrap_or(Decimal::from(10));
        let max_margin = Decimal::from_str(&cfg.max_margin_usd.to_string()).unwrap_or(Decimal::from(100));
        
        // Calculate desired margin based on strategy
        let desired_margin = match cfg.margin_strategy.as_str() {
            "fixed" => {
                // Fixed: use max_usd_per_order (clamped to [min_margin_usd, max_margin_usd])
                Decimal::from_str(&cfg.max_usd_per_order.to_string()).unwrap_or(max_margin)
            }
            "balance_based" | "max_balance" => {
                // Balance-based: Use all available balance (up to max_margin_usd)
                // Since we only have one position, we can use all available funds
                available_balance.min(max_margin).max(min_margin)
            }
            "dynamic" | "trend_based" => {
                // Dynamic/Trend-based: Scale margin based on trend strength, but use available balance as base
                // Get trend strength from multi-timeframe analysis
                let trend_strength = {
                    let states = symbol_states.lock().await;
                    if let Some(state) = states.get(&tick.symbol) {
                        // Calculate trend strength: count of timeframes agreeing on trend
                        const TREND_THRESHOLD: f64 = 0.005; // Match TREND_THRESHOLD_PCT from analyze_trend
                        let (trend_short, _) = Self::analyze_trend_timeframe_with_details(&state.prices, 10, TREND_THRESHOLD);
                        let (trend_medium, _) = Self::analyze_trend_timeframe_with_details(&state.prices, 15, TREND_THRESHOLD);
                        let (trend_long, _) = Self::analyze_trend_timeframe_with_details(&state.prices, 20, TREND_THRESHOLD);
                        
                        let trends = vec![trend_short, trend_medium, trend_long];
                        let long_count = trends.iter().filter(|&&t| t == Some(TrendSignal::Long)).count();
                        let short_count = trends.iter().filter(|&&t| t == Some(TrendSignal::Short)).count();
                        
                        // Trend strength: 0.0 (no trend) to 1.0 (all 3 timeframes agree)
                        if long_count > 0 || short_count > 0 {
                            let consensus = (long_count.max(short_count) as f64) / 3.0;
                            consensus
                        } else {
                            0.0
                        }
                    } else {
                        0.0
                    }
                };
                
                // Base margin: Use available balance (up to max_margin)
                let base_margin = available_balance.min(max_margin);
                
                // Scale margin: min_margin + (base_margin - min_margin) * trend_strength
                // This allows using more margin when trend is stronger, but never below min_margin
                let margin_range = base_margin - min_margin;
                let trend_factor = Decimal::from_str(&trend_strength.to_string()).unwrap_or(Decimal::ZERO);
                let scaled_margin = if margin_range > Decimal::ZERO {
                    min_margin + (margin_range * trend_factor)
                } else {
                    min_margin
                };
                
                debug!(
                    symbol = %tick.symbol,
                    available_balance = %available_balance,
                    trend_strength,
                    base_margin = %base_margin,
                    min_margin = %min_margin,
                    max_margin = %max_margin,
                    scaled_margin = %scaled_margin,
                    strategy = %cfg.margin_strategy,
                    "TRENDING: Dynamic margin calculation (trend-based with balance)"
                );
                
                scaled_margin
            }
            _ => {
                // Fallback to balance_based
                available_balance.min(max_margin).max(min_margin)
            }
        };
        
        // Clamp margin to [min_margin_usd, max_margin_usd] range and available balance
        let margin_usd = desired_margin
            .max(min_margin)
            .min(max_margin)
            .min(available_balance); // Never exceed available balance
        
        // 1. Balance check: Ensure sufficient balance before generating signal
        // Note: margin_usd is already clamped to available_balance above, but double-check for safety
        let max_usd = margin_usd;
        
        debug!(
            symbol = %tick.symbol,
            available_balance = %available_balance,
            required_balance = %max_usd,
            quote_asset = %cfg.quote_asset,
            balance_ok = available_balance >= max_usd,
            margin_strategy = %cfg.margin_strategy,
            "TRENDING: Balance check and margin calculation"
        );
        
        if available_balance < max_usd {
            // Insufficient balance, skip signal generation
            // This should rarely happen since margin_usd is already clamped to available_balance
            debug!(
                symbol = %tick.symbol,
                available_balance = %available_balance,
                required_balance = %max_usd,
                quote_asset = %cfg.quote_asset,
                deficit = %(max_usd - available_balance),
                "TRENDING: Insufficient balance - skipping signal (should not happen, margin already clamped)"
            );
            return Ok(());
        }
        
        // 2. Get leverage and clamp to symbol max leverage
        // ✅ CRITICAL: Clamp leverage to symbol's max leverage (if available)
        let desired_leverage = cfg.leverage.unwrap_or(cfg.exec.default_leverage) as u32;
        
        // Fetch symbol rules to get max leverage (if available)
        let rules = match connection.rules_for(&tick.symbol).await {
            Ok(r) => r,
            Err(e) => {
                warn!(
                    error = %e,
                    symbol = %tick.symbol,
                    "TRENDING: Failed to fetch symbol rules, skipping signal generation"
                );
                return Ok(());
            }
        };
        
        // Clamp leverage to symbol's max leverage (if available) or config max_leverage
        let final_leverage = if let Some(symbol_max_lev) = rules.max_leverage {
            // Symbol has max leverage - use the minimum of desired and symbol max
            let clamped = desired_leverage.min(symbol_max_lev);
            if clamped < desired_leverage {
                debug!(
                    symbol = %tick.symbol,
                    desired_leverage,
                    symbol_max_leverage = symbol_max_lev,
                    final_leverage = clamped,
                    "TRENDING: Leverage clamped to symbol max leverage"
                );
            }
            clamped
        } else {
            // No symbol max leverage - clamp to config max_leverage
            let clamped = desired_leverage.min(cfg.risk.max_leverage);
            if clamped < desired_leverage {
                debug!(
                    symbol = %tick.symbol,
                    desired_leverage,
                    config_max_leverage = cfg.risk.max_leverage,
                    final_leverage = clamped,
                    "TRENDING: Leverage clamped to config max leverage"
                );
            }
            clamped
        };
        
        // Calculate notional value: margin × leverage
        // Example: 100 USD margin × 20x leverage = 2000 USD notional
        let notional = max_usd * Decimal::from(final_leverage);
        
        debug!(
            symbol = %tick.symbol,
            margin_usd = %max_usd,
            desired_leverage = desired_leverage,
            final_leverage = final_leverage,
            symbol_max_leverage = ?rules.max_leverage,
            config_max_leverage = cfg.risk.max_leverage,
            notional = %notional,
            "TRENDING: Position sizing calculation (leverage clamped)"
        );
        
        // 3. Check min_notional requirement
        if !rules.min_notional.is_zero() && notional < rules.min_notional {
            // Notional too small, skip signal generation
            debug!(
                symbol = %tick.symbol,
                notional = %notional,
                min_notional = %rules.min_notional,
                deficit = %(rules.min_notional - notional),
                "TRENDING: Notional too small - skipping signal"
            );
            return Ok(());
        }
        
        // 4. Calculate position size and quantize
        let size_raw = notional / entry_price.0;
        let size_quantized = quantize_decimal(size_raw, rules.step_size);
        
        debug!(
            symbol = %tick.symbol,
            entry_price = %entry_price.0,
            size_raw = %size_raw,
            size_quantized = %size_quantized,
            step_size = %rules.step_size,
            size_ok = !size_quantized.is_zero(),
            "TRENDING: Position size calculation and quantization"
        );
        
        // 6. Final check: quantized size must be non-zero
        if size_quantized.is_zero() {
            // Quantized size is zero, skip signal generation
            debug!(
                symbol = %tick.symbol,
                size_raw = %size_raw,
                size_quantized = %size_quantized,
                step_size = %rules.step_size,
                reason = "quantized_size_zero",
                "TRENDING: Quantized size is zero - skipping signal"
            );
            return Ok(());
        }
        
        let size = Qty(size_quantized);
        
        // Store notional for logging (needed outside this scope)
        let notional_for_log = notional;
        
        // Get TP/SL from config
        let stop_loss_pct = Some(cfg.stop_loss_pct);
        let take_profit_pct = Some(cfg.take_profit_pct);
        
        // Clone symbol once for reuse
        let symbol = tick.symbol.clone();
        
        // Generate trade signal
        // Include spread information for validation at order placement
        let signal = TradeSignal {
            symbol: symbol.clone(),
            side,
            entry_price,
            leverage: final_leverage, // Use clamped leverage
            size,
            stop_loss_pct,
            take_profit_pct,
            spread_bps: spread_bps_f64,
            spread_timestamp,
            timestamp: now,
        };
        
        // Double-check position/order state right before sending signal
        // 2. Signal generation takes time (spread check, momentum, balance, rules, etc.)
        // 3. During signal generation, another thread opens position/order
        // 4. Without this check, signal would be sent anyway
        // Solution: Final check right before sending to ensure state hasn't changed
        {
            let ordering_state = shared_state.ordering_state.lock().await;
            if ordering_state.open_position.is_some() || ordering_state.open_order.is_some() {
                warn!(
                    symbol = %tick.symbol,
                    side = ?side,
                    "TRENDING: Position/order opened while generating signal, discarding signal to prevent collision"
                );
                return Ok(());
            }
        }
        
        // Publish trade signal (state verified, safe to send)
        debug!(
            symbol = %tick.symbol,
            side = ?side,
            entry_price = %entry_price.0,
            size = %size.0,
            notional = %notional_for_log,
            leverage = final_leverage,
            spread_bps = spread_bps_f64,
            "TRENDING: All validations passed, sending TradeSignal"
        );
        
        if let Err(e) = event_bus.trade_signal_tx.send(signal.clone()) {
            error!(
                error = ?e,
                symbol = %tick.symbol,
                "TRENDING: Failed to send TradeSignal event (no subscribers or channel closed)"
            );
        } else {
            // Update last signal information only after successful send
            // This ensures we only track signals that were actually sent
            {
                let mut last_signals_map = last_signals.lock().await;
                last_signals_map.insert(symbol.clone(), LastSignal {
                    side,
                    timestamp: now,
                });
            }
            
            let momentum_str = momentum.map(|m| format!("{:.2}%", m)).unwrap_or_else(|| "N/A".to_string());
            let trend_str = match trend_signal {
                Some(TrendSignal::Long) => "Long (Uptrend)",
                Some(TrendSignal::Short) => "Short (Downtrend)",
                None => "None",
            };
            
            // Get last signal info for logging (quick read, lock released immediately)
            let last_signal_side = {
                let last_signals_map = last_signals.lock().await;
                last_signals_map.get(&tick.symbol)
                    .map(|ls| format!("{:?}", ls.side))
                    .unwrap_or_else(|| "None".to_string())
            };
            
            info!(
                symbol = %tick.symbol,
                side = ?side,
                trend = trend_str,
                momentum_pct = momentum_str,
                spread_bps = spread_bps_f64,
                min_acceptable_spread_bps = min_acceptable_spread_bps,
                max_acceptable_spread_bps = max_acceptable_spread_bps,
                entry_price = %entry_price.0,
                size = %size.0,
                notional = %notional_for_log,
                leverage = final_leverage,
                cooldown_seconds = cooldown_seconds,
                last_signal_side = last_signal_side,
                "TRENDING: TradeSignal generated (SMA-based trend analysis, validated position size)"
            );
        }
        
        Ok(())
    }
}

