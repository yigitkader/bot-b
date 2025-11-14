// TRENDING: Trend analysis, generates TradeSignal
// Only does trend analysis, never places orders
// Subscribes to MarketTick events, publishes TradeSignal events
// Does NOT access exchange directly - only uses event bus

use crate::config::AppCfg;
use crate::event_bus::{EventBus, MarketTick, TradeSignal};
use crate::types::{Px, Qty, Side};
use anyhow::Result;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

/// TRENDING module - trend analysis and signal generation
pub struct Trending {
    cfg: Arc<AppCfg>,
    event_bus: Arc<EventBus>,
    shutdown_flag: Arc<AtomicBool>,
    /// Track last signal time per symbol to prevent spam
    last_signal_time: Arc<Mutex<HashMap<String, Instant>>>,
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
            last_signal_time: Arc::new(Mutex::new(HashMap::new())),
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
        let last_signal_time = self.last_signal_time.clone();
        
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
                            &last_signal_time,
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

    /// Process a market tick and generate trade signal if conditions are met
    async fn process_market_tick(
        tick: &MarketTick,
        cfg: &Arc<AppCfg>,
        event_bus: &Arc<EventBus>,
        last_signal_time: &Arc<Mutex<HashMap<String, Instant>>>,
    ) -> Result<()> {
        // Check cooldown: prevent signal spam
        let cooldown_seconds = cfg.trending.signal_cooldown_seconds;
        let now = Instant::now();
        {
            let mut last_signal = last_signal_time.lock().await;
            if let Some(last_time) = last_signal.get(&tick.symbol) {
                let elapsed = now.duration_since(*last_time);
                if elapsed < Duration::from_secs(cooldown_seconds) {
                    // Still in cooldown, skip signal generation
                    return Ok(());
                }
            }
        }
        
        // Simple trend analysis: calculate spread
        let spread_bps = ((tick.ask.0 - tick.bid.0) / tick.bid.0) * Decimal::from(10000);
        let spread_bps_f64 = spread_bps.to_f64().unwrap_or(0.0);
        
        // Get thresholds from config
        let min_spread_bps = cfg.trending.min_spread_bps;
        let max_spread_bps = cfg.trending.max_spread_bps;
        
        // Generate signal if spread is within acceptable range
        if spread_bps_f64 >= min_spread_bps && spread_bps_f64 <= max_spread_bps {
            // Simple buy signal (can be enhanced with actual trend analysis)
            let side = Side::Buy;
            
            // Calculate entry price (mid price)
            let entry_price = Px((tick.bid.0 + tick.ask.0) / Decimal::from(2));
            
            // Calculate position size from config
            // CRITICAL: max_usd_per_order is margin amount, not notional value
            // In futures: notional = margin × leverage, position_size = notional / price
            // Use cfg.leverage if set, otherwise use cfg.exec.default_leverage
            let leverage = cfg.leverage.unwrap_or(cfg.exec.default_leverage) as u32;
            let max_usd = Decimal::from_str(&cfg.max_usd_per_order.to_string()).unwrap_or(Decimal::from(100));
            
            // Calculate notional value: margin × leverage
            // Example: 100 USD margin × 20x leverage = 2000 USD notional
            let notional = max_usd * Decimal::from(leverage);
            
            // Calculate position size: notional / entry_price
            // Example: 2000 USD notional / 50000 USD price = 0.04 size
            let size = Qty(notional / entry_price.0);
            
            // Get TP/SL from config
            let stop_loss_pct = Some(cfg.stop_loss_pct);
            let take_profit_pct = Some(cfg.take_profit_pct);
            
            // Clone symbol once for reuse (TradeSignal and HashMap both need owned String)
            // This reduces allocations compared to cloning tick.symbol multiple times
            let symbol = tick.symbol.clone();
            
            // Generate trade signal (move symbol into signal)
            let signal = TradeSignal {
                symbol: symbol.clone(), // Clone for TradeSignal (needs ownership for channel send)
                side,
                entry_price,
                leverage,
                size,
                stop_loss_pct,
                take_profit_pct,
                timestamp: now,
            };
            
            // Update last signal time for this symbol (reuse cloned symbol, one more clone needed for HashMap)
            {
                let mut last_signal = last_signal_time.lock().await;
                last_signal.insert(symbol, now); // Move symbol here (last use)
            }
            
            // Publish trade signal
            // Note: symbol is moved into HashMap above, so we use tick.symbol for logging
            if let Err(e) = event_bus.trade_signal_tx.send(signal) {
                error!(
                    error = ?e,
                    symbol = %tick.symbol,
                    "TRENDING: Failed to send TradeSignal event (no subscribers or channel closed)"
                );
            }
            info!(
                symbol = %tick.symbol,
                side = ?side,
                spread_bps = spread_bps_f64,
                cooldown_seconds = cooldown_seconds,
                "TRENDING: TradeSignal generated"
            );
        }
        
        Ok(())
    }
}

