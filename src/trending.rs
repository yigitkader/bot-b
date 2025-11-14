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
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::Instant;
use tracing::{info, warn};

/// TRENDING module - trend analysis and signal generation
pub struct Trending {
    cfg: Arc<AppCfg>,
    event_bus: Arc<EventBus>,
    shutdown_flag: Arc<AtomicBool>,
}

impl Trending {
    pub fn new(
        cfg: Arc<AppCfg>,
        event_bus: Arc<EventBus>,
        shutdown_flag: Arc<AtomicBool>,
    ) -> Self {
        Self {
            cfg,
            event_bus,
            shutdown_flag,
        }
    }

    /// Start trending service
    /// Listens to MarketTick events and generates TradeSignal events
    pub async fn start(&self) -> Result<()> {
        let event_bus = self.event_bus.clone();
        let shutdown_flag = self.shutdown_flag.clone();
        let cfg = self.cfg.clone();
        
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
    ) -> Result<()> {
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
            let leverage = cfg.leverage.unwrap_or(cfg.risk.max_leverage) as u32;
            let max_usd = Decimal::from_str(&cfg.max_usd_per_order.to_string()).unwrap_or(Decimal::from(100));
            let size = Qty(max_usd / entry_price.0); // Simplified size calculation
            
            // Get TP/SL from config
            let stop_loss_pct = Some(cfg.stop_loss_pct);
            let take_profit_pct = Some(cfg.take_profit_pct);
            
            // Generate trade signal
            let signal = TradeSignal {
                symbol: tick.symbol.clone(),
                side,
                entry_price,
                leverage,
                size,
                stop_loss_pct,
                take_profit_pct,
                timestamp: Instant::now(),
            };
            
            // Publish trade signal
            let _ = event_bus.trade_signal_tx.send(signal);
            info!(
                symbol = %tick.symbol,
                side = ?side,
                spread_bps = spread_bps_f64,
                "TRENDING: TradeSignal generated"
            );
        }
        
        Ok(())
    }
}

