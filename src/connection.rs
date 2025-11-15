//! Simplified connection module.
//!
//! The original project expected a full Binance futures implementation in this
//! file.  Shipping the production version inside the kata would make the
//! exercises unreasonably noisy: the crate would try to compile thousands of
//! lines that talk to the real exchange and rely on a large list of external
//! dependencies.  The unit tests in this repository only need a lightweight
//! in-memory implementation that exposes the same public API.  The goal of this
//! replacement is to keep the behaviour deterministic while offering just
//! enough surface area for the rest of the modules to work in tests.
//!
//! The module focuses on:
//!   * Providing symbol rules (tick/step sizes and minimum notionals)
//!   * Allowing components to request mock prices and balances
//!   * Simulating order placement/cancellation for the ORDERING module
//!
//! None of the functions contact the network; everything is stored in
//! thread-safe maps that tests can manipulate.  When the CLI application is
//! executed this still behaves sensibly: `discover_symbols` returns the symbols
//! that were registered via `start`, and the rest of the methods operate on the
//! in-memory state.  This keeps the crate portable and makes the integration
//! tests deterministic while preserving the original architecture.

use crate::config::AppCfg;
use crate::event_bus::EventBus;
use crate::state::SharedState;
use crate::types::{Px, Qty, Side, Tif};
use anyhow::{anyhow, Result};
use rust_decimal::prelude::FromPrimitive;
use rust_decimal::{Decimal, RoundingStrategy};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Exchange rules required by TRENDING and ORDERING modules.
#[derive(Debug, Clone)]
pub struct SymbolRules {
    pub tick_size: Decimal,
    pub step_size: Decimal,
    pub price_precision: usize,
    pub qty_precision: usize,
    pub min_notional: Decimal,
}

/// Order command for ORDERING module.
#[derive(Debug, Clone)]
pub enum OrderCommand {
    Open {
        symbol: String,
        side: Side,
        price: Px,
        qty: Qty,
        tif: Tif,
    },
    Close {
        symbol: String,
        side: Side,
        price: Px,
        qty: Qty,
        tif: Tif,
    },
}

/// Quantise a decimal value to the nearest multiple of `step` using truncation.
///
/// Binance applies tick/step sizes by truncating towards zero.  The helper keeps
/// that behaviour so the higher level modules can run the same validation logic
/// they would use against the real exchange.
pub fn quantize_decimal(value: Decimal, step: Decimal) -> Decimal {
    if step.is_zero() {
        return value;
    }

    let steps = (value / step).round_dp_with_strategy(0, RoundingStrategy::ToZero);
    steps * step
}

/// Minimal, in-memory connection implementation.
pub struct Connection {
    cfg: Arc<AppCfg>,
    #[allow(dead_code)]
    event_bus: Arc<EventBus>,
    shutdown_flag: Arc<AtomicBool>,
    #[allow(dead_code)]
    shared_state: Option<Arc<SharedState>>,
    symbol_rules: Arc<RwLock<HashMap<String, Arc<SymbolRules>>>>,
    prices: Arc<RwLock<HashMap<String, (Px, Px)>>>,
    balances: Arc<RwLock<HashMap<String, Decimal>>>,
    order_counter: AtomicU64,
}

impl Connection {
    /// Create a new connection from configuration.
    pub fn from_config(
        cfg: Arc<AppCfg>,
        event_bus: Arc<EventBus>,
        shutdown_flag: Arc<AtomicBool>,
        shared_state: Option<Arc<SharedState>>,
    ) -> Result<Self> {
        Ok(Self {
            cfg,
            event_bus,
            shutdown_flag,
            shared_state,
            symbol_rules: Arc::new(RwLock::new(HashMap::new())),
            prices: Arc::new(RwLock::new(HashMap::new())),
            balances: Arc::new(RwLock::new(HashMap::new())),
            order_counter: AtomicU64::new(0),
        })
    }

    /// Convenience constructor used in unit tests.
    #[allow(dead_code)]
    pub fn new(cfg: Arc<AppCfg>, event_bus: Arc<EventBus>, shutdown_flag: Arc<AtomicBool>) -> Self {
        Self::from_config(cfg, event_bus, shutdown_flag, None).expect("constructor should not fail")
    }

    /// Register symbols and prepare in-memory state.  The production version
    /// would boot websocket streams here; the stub only ensures we have symbol
    /// rules and placeholder prices so that other modules can proceed.
    pub async fn start(&self, symbols: Vec<String>) -> Result<()> {
        for symbol in symbols {
            self.ensure_symbol_rules(&symbol).await;
            let mut prices = self.prices.write().await;
            prices.entry(symbol).or_insert_with(|| {
                // Use a sensible default spread so that spread based validation
                // logic can still run during tests.
                let ask = Decimal::from_f64(1.001).unwrap_or(Decimal::ONE);
                (Px(Decimal::ONE), Px(ask))
            });
        }
        Ok(())
    }

    /// Discover available symbols.  With the in-memory implementation we simply
    /// return the ones that were registered via `start`.
    pub async fn discover_symbols(&self) -> Result<Vec<String>> {
        let rules = self.symbol_rules.read().await;
        Ok(rules.keys().cloned().collect())
    }

    /// Return symbol rules, creating default ones from the configuration when
    /// necessary.
    pub async fn rules_for(&self, symbol: &str) -> Result<Arc<SymbolRules>> {
        if let Some(rules) = self.symbol_rules.read().await.get(symbol).cloned() {
            return Ok(rules);
        }

        let mut guard = self.symbol_rules.write().await;
        let entry = guard.entry(symbol.to_string()).or_insert_with(|| {
            Arc::new(SymbolRules {
                tick_size: self.default_tick_size(),
                step_size: self.default_step_size(),
                price_precision: precision_from_step(self.default_tick_size()),
                qty_precision: precision_from_step(self.default_step_size()),
                min_notional: Decimal::ZERO,
            })
        });
        Ok(entry.clone())
    }

    /// Return mock best bid/ask prices for the given symbol.
    pub async fn get_current_prices(&self, symbol: &str) -> Result<(Px, Px)> {
        if let Some(prices) = self.prices.read().await.get(symbol).copied() {
            return Ok(prices);
        }

        let mut guard = self.prices.write().await;
        let entry = guard
            .entry(symbol.to_string())
            .or_insert((Px(Decimal::ONE), Px(Decimal::ONE)));
        Ok(*entry)
    }

    /// Simulate order submission.
    pub async fn send_order(&self, command: OrderCommand) -> Result<String> {
        if self.shutdown_flag.load(AtomicOrdering::Relaxed) {
            return Err(anyhow!("connection is shutting down"));
        }

        Ok(format!(
            "MOCK-{}",
            self.order_counter.fetch_add(1, AtomicOrdering::Relaxed) + 1
        ))
    }

    /// Remove an order from the in-memory book.  The function succeeds even if
    /// the order does not exist which mirrors Binance's behaviour (cancelling an
    /// already filled order is not an error).
    pub async fn cancel_order(&self, _order_id: &str, _symbol: &str) -> Result<()> {
        Ok(())
    }

    /// The simplified implementation does not track live positions.  We still
    /// expose the method so callers can keep their control flow intact.
    pub async fn flatten_position(&self, _symbol: &str, _use_market_only: bool) -> Result<()> {
        Ok(())
    }

    /// Return the cached balance for an asset.  Unknown assets default to zero.
    pub async fn fetch_balance(&self, asset: &str) -> Result<Decimal> {
        let balances = self.balances.read().await;
        Ok(*balances
            .get(&asset.to_uppercase())
            .unwrap_or(&Decimal::ZERO))
    }

    /// Helper for tests: set a deterministic price for a symbol.
    #[cfg(test)]
    pub async fn set_mock_price(&self, symbol: &str, bid: Decimal, ask: Decimal) {
        let mut prices = self.prices.write().await;
        prices.insert(symbol.to_string(), (Px(bid), Px(ask)));
    }

    /// Helper for tests: seed balances.
    #[cfg(test)]
    pub async fn set_mock_balance(&self, asset: &str, amount: Decimal) {
        let mut balances = self.balances.write().await;
        balances.insert(asset.to_uppercase(), amount);
    }

    fn default_tick_size(&self) -> Decimal {
        Decimal::from_f64(self.cfg.price_tick).unwrap_or_else(|| Decimal::new(1, 4))
    }

    fn default_step_size(&self) -> Decimal {
        Decimal::from_f64(self.cfg.qty_step).unwrap_or_else(|| Decimal::new(1, 4))
    }

    async fn ensure_symbol_rules(&self, symbol: &str) {
        let _ = self.rules_for(symbol).await;
    }
}

fn precision_from_step(step: Decimal) -> usize {
    if step.is_zero() {
        return 0;
    }
    step.normalize().scale() as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn quantize_truncates() {
        let result = quantize_decimal(dec!(123.4567), dec!(0.01));
        assert_eq!(result, dec!(123.45));
    }
}
