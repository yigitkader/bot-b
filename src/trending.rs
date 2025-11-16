// TRENDING: Trend analysis, generates TradeSignal
// Only does trend analysis, never places orders
// Subscribes to MarketTick events, publishes TradeSignal events
// No balance/margin/position size calculations - ORDERING handles that

use crate::config::AppCfg;
use crate::event_bus::{EventBus, MarketTick, TradeSignal};
use crate::types::{LastSignal, PositionDirection, PricePoint, Px, Qty, Side, SymbolState, TrendSignal};
use anyhow::Result;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::collections::{HashMap, VecDeque};
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
    
    /// Simple trend analysis using SMA
    /// Returns Some(TrendSignal) if clear trend detected, None otherwise
    fn analyze_trend(state: &SymbolState) -> Option<TrendSignal> {
        const SMA_PERIOD: usize = 20;  // Simple 20-period SMA
        const TREND_THRESHOLD_PCT: f64 = 0.01; // 0.01% threshold - simple and effective
        
        let prices = &state.prices;
        
        // Need at least SMA_PERIOD prices
        if prices.len() < SMA_PERIOD {
            return None;
        }
        
        // Simple SMA trend analysis
        Self::analyze_trend_timeframe(prices, SMA_PERIOD, TREND_THRESHOLD_PCT)
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
            });
            
            state.prices.push_back(PricePoint {
                timestamp: now,
                price: mid_price,
                volume: tick.volume,
            });
            
            const MAX_HISTORY: usize = 50; // Keep last 50 prices
            while state.prices.len() > MAX_HISTORY {
                state.prices.pop_front();
            }
            
            Self::analyze_trend(state)
        };
        
        // 4. Generate signal if trend detected
        let side = match trend_signal {
            Some(TrendSignal::Long) => Side::Buy,
            Some(TrendSignal::Short) => Side::Sell,
            None => return Ok(()), // No trend, skip
        };
        
        let entry_price = Px(mid_price);
        
        // 5. Send signal
        let signal = TradeSignal {
            symbol: tick.symbol.clone(),
            side,
            entry_price,
            leverage: 0,
            size: Qty(Decimal::ZERO),
            stop_loss_pct: Some(cfg.stop_loss_pct),
            take_profit_pct: Some(cfg.take_profit_pct),
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

