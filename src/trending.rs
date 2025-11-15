// TRENDING: Trend analysis, generates TradeSignal
// Only does trend analysis, never places orders
// Subscribes to MarketTick events, publishes TradeSignal events
// Uses Connection for symbol rules validation and balance checks

use crate::config::AppCfg;
use crate::connection::Connection;
use crate::connection::quantize_decimal;
use crate::event_bus::{EventBus, MarketTick, PositionUpdate, TradeSignal};
use crate::state::SharedState;
use crate::types::{Px, Qty, Side};
use anyhow::Result;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::collections::{HashMap, VecDeque};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

/// Price point for trend analysis
#[derive(Clone, Debug)]
struct PricePoint {
    timestamp: Instant,
    price: Decimal,
    volume: Option<Decimal>,
}

/// Symbol state for trend analysis
#[derive(Clone, Debug)]
struct SymbolState {
    symbol: String,
    prices: VecDeque<PricePoint>,
    last_signal_time: Option<Instant>,
    /// Timestamp of last position close for this symbol
    /// Used to enforce cooldown after position close to prevent signal spam
    last_position_close_time: Option<Instant>,
}

/// Trend signal direction
#[derive(Debug, Clone, Copy, PartialEq)]
enum TrendSignal {
    Long,   // Uptrend - buy signal
    Short,  // Downtrend - sell signal
}

/// Last signal information for cooldown and direction checking
#[derive(Clone, Debug)]
struct LastSignal {
    side: Side,
    timestamp: Instant,
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
        
        // Spawn task for MarketTick events (signal generation)
        let event_bus_tick = event_bus.clone();
        let shutdown_flag_tick = shutdown_flag.clone();
        let cfg_tick = cfg.clone();
        let shared_state_tick = shared_state.clone();
        let connection_tick = connection.clone();
        let last_signals_tick = last_signals.clone();
        let symbol_states_tick = symbol_states.clone();
        tokio::spawn(async move {
            let mut market_tick_rx = event_bus_tick.subscribe_market_tick();
            
            info!("TRENDING: Started, listening to MarketTick events");
            
            loop {
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
                        ).await {
                            warn!(error = %e, symbol = %tick.symbol, "TRENDING: error processing market tick");
                        }
                    }
                    Err(_) => break,
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
                            // Position closed - set cooldown timestamp
                            let mut states = symbol_states_pos.lock().await;
                            let state = states.entry(update.symbol.clone()).or_insert_with(|| {
                                SymbolState {
                                    symbol: update.symbol.clone(),
                                    prices: VecDeque::new(),
                                    last_signal_time: None,
                                    last_position_close_time: None,
                                }
                            });
                            
                            state.last_position_close_time = Some(Instant::now());
                            
                            debug!(
                                symbol = %update.symbol,
                                "TRENDING: Position closed, cooldown set for this symbol"
                            );
                        }
                    }
                    Err(_) => break,
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
        // ✅ CRITICAL: Need at least (period + 1) prices to calculate momentum
        // period=5 means we need 6 prices: 5 prices ago, 4 prices ago, ..., 1 price ago, current
        // This is safer than checking < period + 1 (avoids off-by-one errors)
        if prices.len() <= period {
            return None;
        }
        
        // ✅ SAFER: Use iterator instead of index calculation to avoid off-by-one errors
        // Take the last (period + 1) prices in reverse order (newest first)
        // This gives us: [current, 1 ago, 2 ago, ..., period ago]
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
        const TREND_THRESHOLD_PCT: f64 = 1.5; // 1.5% threshold for trend detection (slightly lower for multi-timeframe)
        const VOLUME_CONFIRMATION_PERIOD: usize = 10; // Period for volume analysis
        
        let prices = &state.prices;
        
        // Need at least MIN_PRICES_FOR_ANALYSIS prices for reliable analysis
        if prices.len() < MIN_PRICES_FOR_ANALYSIS {
            return None;
        }
        
        // Multi-timeframe analysis: check trends across different periods
        let trend_short = Self::analyze_trend_timeframe(prices, SMA_PERIOD_SHORT, TREND_THRESHOLD_PCT);
        let trend_medium = Self::analyze_trend_timeframe(prices, SMA_PERIOD_MEDIUM, TREND_THRESHOLD_PCT);
        let trend_long = Self::analyze_trend_timeframe(prices, SMA_PERIOD_LONG, TREND_THRESHOLD_PCT);
        
        // Require at least 2 out of 3 timeframes to agree on trend direction
        // This ensures stronger, more reliable signals
        let trends: Vec<Option<TrendSignal>> = vec![trend_short, trend_medium, trend_long];
        let long_count = trends.iter().filter(|&&t| t == Some(TrendSignal::Long)).count();
        let short_count = trends.iter().filter(|&&t| t == Some(TrendSignal::Short)).count();
        
        // Determine final trend signal
        let final_trend = if long_count >= 2 {
            Some(TrendSignal::Long)
        } else if short_count >= 2 {
            Some(TrendSignal::Short)
        } else {
            None // No clear consensus across timeframes
        };
        
        // Volume confirmation: require volume to support the trend
        if let Some(trend) = final_trend {
            if Self::check_volume_confirmation(prices, trend, VOLUME_CONFIRMATION_PERIOD) {
                return Some(trend);
            } else {
                // Trend detected but volume doesn't confirm - skip signal
                return None;
            }
        }
        
        None
    }
    
    /// Process a market tick and generate trade signal if conditions are met
    async fn process_market_tick(
        tick: &MarketTick,
        cfg: &Arc<AppCfg>,
        event_bus: &Arc<EventBus>,
        shared_state: &Arc<SharedState>,
        connection: &Arc<Connection>,
        last_signals: &Arc<Mutex<HashMap<String, LastSignal>>>,
        symbol_states: &Arc<Mutex<HashMap<String, SymbolState>>>,
    ) -> Result<()> {
        let now = Instant::now();
        
        // CRITICAL: Check if ANY position or order is already open
        // Don't generate signals if we already have an open position or pending order
        // This prevents signal collision where:
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
                return Ok(());
            }
        }
        
        // ✅ CRITICAL FIX: Symbol bazlı cooldown after position close
        // Position close sonrası minimum 5 saniye bekle (signal spam önleme)
        // TP trigger → position close → lock release → yeni signal → aynı sembol için yeni order!
        // Bu cooldown aynı sembol için hemen yeni signal üretilmesini önler
        const POSITION_CLOSE_COOLDOWN_SECS: u64 = 5;
        {
            let states = symbol_states.lock().await;
            if let Some(state) = states.get(&tick.symbol) {
                if let Some(last_close_time) = state.last_position_close_time {
                    let elapsed = now.duration_since(last_close_time);
                    if elapsed < Duration::from_secs(POSITION_CLOSE_COOLDOWN_SECS) {
                        // Still in cooldown after position close, skip signal generation
                        debug!(
                            symbol = %tick.symbol,
                            elapsed_secs = elapsed.as_secs(),
                            cooldown_secs = POSITION_CLOSE_COOLDOWN_SECS,
                            "TRENDING: Skipping signal generation - position close cooldown active"
                        );
                        return Ok(());
                    }
                }
            }
        }
        
        // ✅ PERFORMANCE OPTIMIZATION: Cooldown check BEFORE expensive trend analysis
        // This prevents unnecessary CPU usage when cooldown is still active
        // Check cooldown period first (cheap operation)
        let cooldown_seconds = cfg.trending.signal_cooldown_seconds;
        let last_signal_side = {
            let last_signals_map = last_signals.lock().await;
            if let Some(last_signal) = last_signals_map.get(&tick.symbol) {
                let elapsed = now.duration_since(last_signal.timestamp);
                
                // Check cooldown period - if still in cooldown, skip expensive trend analysis
                if elapsed < Duration::from_secs(cooldown_seconds) {
                    // Still in cooldown, skip signal generation (early exit, no trend analysis)
                    return Ok(());
                }
                
                // Cooldown passed, return last signal side for later direction check
                Some(last_signal.side)
            } else {
                // No previous signal, cooldown check passed
                None
            }
        };
        
        // Calculate spread (bid-ask spread in basis points)
        let spread_bps = ((tick.ask.0 - tick.bid.0) / tick.bid.0) * Decimal::from(10000);
        let spread_bps_f64 = spread_bps.to_f64().unwrap_or(0.0);
        
        // CRITICAL: Only trade when spread is within acceptable range
        // Too wide spread = low liquidity = high slippage = bad for trading
        // Too narrow spread = potential flash crash, liquidity trap, or stale data = bad for trading
        // Acceptable range: min_spread_bps <= spread <= max_spread_bps
        let min_acceptable_spread_bps = cfg.trending.min_spread_bps;
        let max_acceptable_spread_bps = cfg.trending.max_spread_bps;
        
        // Check if spread is within acceptable range
        if spread_bps_f64 < min_acceptable_spread_bps || spread_bps_f64 > max_acceptable_spread_bps {
            // Spread out of acceptable range → skip signal
            // Too narrow: potential flash crash, liquidity trap, stale data
            // Too wide: low liquidity, high slippage risk
            return Ok(());
        }
        
        // ✅ CRITICAL: Store spread information and timestamp for validation at order placement
        // Spread may change between signal generation and order placement (50-100ms delay)
        // ORDERING module will re-validate spread before placing order
        let spread_timestamp = now;
        
        // Calculate mid price for trend analysis
        let mid_price = (tick.bid.0 + tick.ask.0) / Decimal::from(2);
        let current_price = mid_price;
        
        // Update symbol state with new price point
        // ✅ Now we do expensive trend analysis only after cooldown check passed
        let trend_signal = {
            let mut states = symbol_states.lock().await;
            let state = states.entry(tick.symbol.clone()).or_insert_with(|| {
                SymbolState {
                    symbol: tick.symbol.clone(),
                    prices: VecDeque::new(),
                    last_signal_time: None,
                    last_position_close_time: None,
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
        let side = match trend_signal {
            Some(TrendSignal::Long) => Side::Buy,
            Some(TrendSignal::Short) => Side::Sell,
            None => {
                // No clear trend detected, skip signal generation
                // This prevents trading in sideways markets
                return Ok(());
            }
        };
        
        // ✅ CRITICAL: Check same-direction signals (after trend analysis)
        // This prevents BUY-BUY-BUY or SELL-SELL-SELL spam
        // Only generate signal if direction changed (trend reversal)
        if let Some(last_side) = last_signal_side {
            if last_side == side {
                // Same direction as last signal - skip to prevent spam
                return Ok(());
            }
        }
        
        // Additional validation: Check momentum for confirmation
        let momentum = {
            let states = symbol_states.lock().await;
            if let Some(state) = states.get(&tick.symbol) {
                Self::calculate_momentum(&state.prices, 5) // 5-period momentum
            } else {
                None
            }
        };
        
        // Require minimum momentum for signal confirmation
        // This filters out weak trends
        if let Some(mom) = momentum {
            const MIN_MOMENTUM_PCT: f64 = 0.5; // Minimum 0.5% momentum required
            if mom.abs() < MIN_MOMENTUM_PCT {
                // Momentum too weak, skip signal
                return Ok(());
            }
            
            // Momentum should align with trend direction
            match trend_signal {
                Some(TrendSignal::Long) if mom < 0.0 => {
                    // Long signal but negative momentum - conflicting signals
                    return Ok(());
                }
                Some(TrendSignal::Short) if mom > 0.0 => {
                    // Short signal but positive momentum - conflicting signals
                    return Ok(());
                }
                _ => {} // Momentum aligns with trend, proceed
            }
        }
        
        // Calculate entry price (mid price)
        let entry_price = Px(current_price);
        
        // 1. Balance check: Ensure sufficient balance before generating signal
        let max_usd = Decimal::from_str(&cfg.max_usd_per_order.to_string()).unwrap_or(Decimal::from(100));
        {
            let balance_store = shared_state.balance_store.read().await;
            let available_balance = if cfg.quote_asset.to_uppercase() == "USDT" {
                balance_store.usdt
            } else {
                balance_store.usdc
            };
            
            if available_balance < max_usd {
                // Insufficient balance, skip signal generation
                return Ok(());
            }
        }
        
        // 2. Get leverage and calculate notional
        // CRITICAL: max_usd_per_order is margin amount, not notional value
        // In futures: notional = margin × leverage, position_size = notional / price
        // Use cfg.leverage if set, otherwise use cfg.exec.default_leverage
        let leverage = cfg.leverage.unwrap_or(cfg.exec.default_leverage) as u32;
        
        // Calculate notional value: margin × leverage
        // Example: 100 USD margin × 20x leverage = 2000 USD notional
        let notional = max_usd * Decimal::from(leverage);
        
        // 3. Fetch symbol rules for validation
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
        
        // 4. Check min_notional requirement
        if !rules.min_notional.is_zero() && notional < rules.min_notional {
            // Notional too small, skip signal generation
            return Ok(());
        }
        
        // 5. Calculate position size and quantize
        let size_raw = notional / entry_price.0;
        let size_quantized = quantize_decimal(size_raw, rules.step_size);
        
        // 6. Final check: quantized size must be non-zero
        if size_quantized.is_zero() {
            // Quantized size is zero, skip signal generation
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
        // ✅ CRITICAL: Include spread information for validation at order placement
        // Spread may have changed between signal generation and order placement
        // ORDERING module will re-validate spread before placing order
        let signal = TradeSignal {
            symbol: symbol.clone(),
            side,
            entry_price,
            leverage,
            size,
            stop_loss_pct,
            take_profit_pct,
            spread_bps: spread_bps_f64,
            spread_timestamp,
            timestamp: now,
        };
        
        // CRITICAL: Double-check position/order state right before sending signal
        // This prevents race condition where:
        // 1. Early check passes (no position/order)
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
                leverage = leverage,
                cooldown_seconds = cooldown_seconds,
                last_signal_side = last_signal_side,
                "TRENDING: TradeSignal generated (SMA-based trend analysis, validated position size)"
            );
        }
        
        Ok(())
    }
}

