// TRENDING: Trend analysis, generates TradeSignal
// Only does trend analysis, never places orders
// Subscribes to MarketTick events, publishes TradeSignal events
// Uses Connection for symbol rules validation and balance checks

use crate::config::AppCfg;
use crate::connection::Connection;
use crate::connection::quantize_decimal;
use crate::event_bus::{EventBus, MarketTick, TradeSignal};
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
        
        tokio::spawn(async move {
            let mut market_tick_rx = event_bus.subscribe_market_tick();
            
            info!("TRENDING: Started, listening to MarketTick events");
            
            loop {
                match market_tick_rx.recv().await {
                    Ok(tick) => {
                        if shutdown_flag.load(AtomicOrdering::Relaxed) {
                            break;
                        }
                        
                        // Process market tick and generate trade signal if needed
                        if let Err(e) = Self::process_market_tick(
                            &tick,
                            &cfg,
                            &event_bus,
                            &shared_state,
                            &connection,
                            &last_signals,
                            &symbol_states,
                        ).await {
                            warn!(error = %e, symbol = %tick.symbol, "TRENDING: error processing market tick");
                        }
                    }
                    Err(_) => break,
                }
            }
            
            info!("TRENDING: Stopped");
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
    fn calculate_momentum(prices: &VecDeque<PricePoint>, period: usize) -> Option<f64> {
        if prices.len() < period + 1 {
            return None;
        }
        
        let first_idx = prices.len() - period - 1;
        let first_price = prices.get(first_idx)?.price;
        let last_price = prices.back()?.price;
        
        let change_pct = ((last_price - first_price) / first_price) * Decimal::from(100);
        change_pct.to_f64()
    }
    
    /// Analyze trend based on price history
    /// Returns Some(TrendSignal) if clear trend detected, None otherwise
    fn analyze_trend(state: &SymbolState) -> Option<TrendSignal> {
        const MIN_PRICES_FOR_ANALYSIS: usize = 10;
        const SMA_PERIOD: usize = 10;
        const TREND_THRESHOLD_PCT: f64 = 2.0; // 2% threshold for trend detection
        
        let prices = &state.prices;
        
        // Need at least MIN_PRICES_FOR_ANALYSIS prices for reliable analysis
        if prices.len() < MIN_PRICES_FOR_ANALYSIS {
            return None;
        }
        
        // Calculate SMA of last SMA_PERIOD prices
        let sma = Self::calculate_sma(prices, SMA_PERIOD)?;
        let current_price = prices.back()?.price;
        
        // Calculate price deviation from SMA
        let deviation_pct = ((current_price - sma) / sma) * Decimal::from(100);
        let deviation_pct_f64 = deviation_pct.to_f64().unwrap_or(0.0);
        
        // Determine trend:
        // - Price > SMA * 1.02 (2% above) → Uptrend → Long signal
        // - Price < SMA * 0.98 (2% below) → Downtrend → Short signal
        // - Otherwise → No clear trend
        if deviation_pct_f64 > TREND_THRESHOLD_PCT {
            Some(TrendSignal::Long)
        } else if deviation_pct_f64 < -TREND_THRESHOLD_PCT {
            Some(TrendSignal::Short)
        } else {
            None
        }
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
        
        // Calculate spread (bid-ask spread in basis points)
        let spread_bps = ((tick.ask.0 - tick.bid.0) / tick.bid.0) * Decimal::from(10000);
        let spread_bps_f64 = spread_bps.to_f64().unwrap_or(0.0);
        
        // CRITICAL: Only trade when spread is narrow (high liquidity)
        // Wide spread = low liquidity = high slippage = bad for trading
        // Narrow spread = high liquidity = low slippage = good for trading
        // Use max_spread_bps as maximum acceptable spread (e.g., 10 bps)
        let max_acceptable_spread_bps = cfg.trending.max_spread_bps;
        
        // Check if spread is acceptable (narrow enough for trading)
        if spread_bps_f64 > max_acceptable_spread_bps {
            // Spread too wide → low liquidity → high slippage risk → skip signal
            return Ok(());
        }
        
        // Calculate mid price for trend analysis
        let mid_price = (tick.bid.0 + tick.ask.0) / Decimal::from(2);
        let current_price = mid_price;
        
        // Update symbol state with new price point
        let trend_signal = {
            let mut states = symbol_states.lock().await;
            let state = states.entry(tick.symbol.clone()).or_insert_with(|| {
                SymbolState {
                    symbol: tick.symbol.clone(),
                    prices: VecDeque::new(),
                    last_signal_time: None,
                }
            });
            
            // Add current price point to history
            state.prices.push_back(PricePoint {
                timestamp: now,
                price: current_price,
                volume: tick.volume,
            });
            
            // Keep only last 20 prices (for reliable trend analysis)
            const MAX_HISTORY: usize = 20;
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
        
        // CRITICAL: Check cooldown and prevent same-direction signals
        // This prevents BUY-BUY-BUY or SELL-SELL-SELL spam
        let cooldown_seconds = cfg.trending.signal_cooldown_seconds;
        {
            let mut last_signals_map = last_signals.lock().await;
            if let Some(last_signal) = last_signals_map.get(&tick.symbol) {
                let elapsed = now.duration_since(last_signal.timestamp);
                
                // Check cooldown period
                if elapsed < Duration::from_secs(cooldown_seconds) {
                    // Still in cooldown, skip signal generation
                    return Ok(());
                }
                
                // CRITICAL: Prevent same-direction signals
                // If last signal was in the same direction, don't generate another one
                // This prevents BUY-BUY-BUY or SELL-SELL-SELL spam
                // Only generate signal if direction changed (trend reversal)
                if last_signal.side == side {
                    // Same direction as last signal - skip to prevent spam
                    return Ok(());
                }
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
        let signal = TradeSignal {
            symbol: symbol.clone(),
            side,
            entry_price,
            leverage,
            size,
            stop_loss_pct,
            take_profit_pct,
            timestamp: now,
        };
        
        // Update last signal information for this symbol (side and timestamp)
        {
            let mut last_signals_map = last_signals.lock().await;
            last_signals_map.insert(symbol.clone(), LastSignal {
                side,
                timestamp: now,
            });
        }
        
        // Publish trade signal
        if let Err(e) = event_bus.trade_signal_tx.send(signal) {
            error!(
                error = ?e,
                symbol = %tick.symbol,
                "TRENDING: Failed to send TradeSignal event (no subscribers or channel closed)"
            );
        } else {
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

