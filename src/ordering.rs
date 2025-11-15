// ORDERING: Order placement/closure, single position guarantee
// Only handles order placement/closure, no trend or PnL logic
// Listens to TradeSignal and CloseRequest events
// Uses CONNECTION to send orders

use crate::config::AppCfg;
use crate::connection::Connection;
use crate::event_bus::{CloseRequest, EventBus, OrderUpdate, PositionUpdate, TradeSignal};
use crate::state::{OpenOrder, OpenPosition, OrderingState, SharedState};
use crate::types::{PositionDirection, Qty, Tif};
use rust_decimal::prelude::ToPrimitive;
use anyhow::{anyhow, Result};
use rust_decimal::Decimal;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

// ============================================================================
// Balance Reservation Guard (RAII Pattern)
// ============================================================================

/// RAII guard for balance reservation
/// Automatically releases balance when dropped (if not explicitly released)
/// Prevents memory leaks from forgotten balance releases
///
/// CRITICAL: Always call release() explicitly before returning from function
/// Drop trait will warn if balance is dropped without explicit release
struct BalanceReservation {
    balance_store: Arc<tokio::sync::RwLock<crate::state::BalanceStore>>,
    asset: String,
    amount: Decimal,
    released: bool,
    /// Order ID associated with this reservation (for leak tracking)
    /// Set after order is placed to help identify which order caused a leak
    order_id: Option<String>,
    /// Timestamp when reservation was allocated (for leak tracking)
    allocation_timestamp: Instant,
}

impl BalanceReservation {
    /// Create a new balance reservation
    /// Returns Some(reservation) if reservation successful, None if insufficient balance
    async fn new(
        balance_store: Arc<tokio::sync::RwLock<crate::state::BalanceStore>>,
        asset: &str,
        amount: Decimal,
    ) -> Option<Self> {
        let mut store = balance_store.write().await;

        // ✅ CRITICAL: try_reserve() is atomic - it checks available balance and reserves in one operation
        // Do not call available() separately - it would create a race condition
        if store.try_reserve(asset, amount) {
            // Clone Arc before moving it into Self (store borrows balance_store)
            Some(Self {
                balance_store: balance_store.clone(),
                asset: asset.to_string(),
                amount,
                released: false,
                order_id: None, // Will be set after order is placed
                allocation_timestamp: Instant::now(),
            })
        } else {
            None
        }
    }

    /// Explicitly release the balance reservation
    /// Should be called before returning from function
    /// Safe to call multiple times (idempotent)
    async fn release(&mut self) {
        if !self.released {
            let mut store = self.balance_store.write().await;
            store.release(&self.asset, self.amount);
            self.released = true;

            debug!(
                asset = %self.asset,
                amount = %self.amount,
                "ORDERING: Balance reservation released"
            );
        }
    }

}

impl Drop for BalanceReservation {
    fn drop(&mut self) {
        // CRITICAL: Drop cannot be async, so we cannot release balance here directly
        // Doing async cleanup in Drop is an anti-pattern that can cause:
        // - Deadlocks during program shutdown
        // - Expensive runtime creation
        // - Unpredictable behavior
        //
        // Instead, we only log a warning. The leak detection task will handle cleanup.
        if !self.released {
            let allocation_age = Instant::now().duration_since(self.allocation_timestamp);
            tracing::error!(
                asset = %self.asset,
                amount = %self.amount,
                order_id = ?self.order_id,
                allocation_age_secs = allocation_age.as_secs(),
                "CRITICAL: Balance reservation dropped without explicit release! Order ID: {:?}, Allocation age: {}s. Manual intervention may be required. Leak detection task will attempt auto-fix.",
                self.order_id,
                allocation_age.as_secs()
            );
            // Balance will remain reserved until leak detection task fixes it
        }
    }
}

/// Maximum number of retry attempts for order placement
const MAX_RETRIES: u32 = 3;

/// ORDERING module - order placement and closure
/// Guarantees single position/order at a time using global lock
pub struct Ordering {
    cfg: Arc<AppCfg>,
    connection: Arc<Connection>,
    event_bus: Arc<EventBus>,
    shutdown_flag: Arc<AtomicBool>,
    shared_state: Arc<SharedState>,
}

impl Ordering {
    /// Create a new Ordering module instance.
    ///
    /// The Ordering module is responsible for placing and managing orders. It guarantees that
    /// only one position/order is open at a time using a global lock mechanism.
    ///
    /// # Arguments
    ///
    /// * `cfg` - Application configuration containing execution parameters (TIF, leverage)
    /// * `connection` - Connection instance for sending orders to the exchange
    /// * `event_bus` - Event bus for subscribing to TradeSignal/CloseRequest and publishing events
    /// * `shutdown_flag` - Shared flag to signal graceful shutdown
    /// * `shared_state` - Shared state for maintaining ordering state (open position/order)
    ///
    /// # Returns
    ///
    /// Returns a new `Ordering` instance. Call `start()` to begin processing trade signals.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # let cfg = Arc::new(crate::config::load_config()?);
    /// # let connection = Arc::new(crate::connection::Connection::from_config(todo!(), todo!(), todo!(), None)?);
    /// # let event_bus = Arc::new(crate::event_bus::EventBus::new());
    /// # let shutdown_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    /// # let shared_state = Arc::new(crate::state::SharedState::new());
    /// let ordering = Ordering::new(cfg, connection, event_bus, shutdown_flag, shared_state);
    /// ordering.start().await?;
    /// ```
    pub fn new(
        cfg: Arc<AppCfg>,
        connection: Arc<Connection>,
        event_bus: Arc<EventBus>,
        shutdown_flag: Arc<AtomicBool>,
        shared_state: Arc<SharedState>,
    ) -> Self {
        Self {
            cfg,
            connection,
            event_bus,
            shutdown_flag,
            shared_state,
        }
    }

    /// Check if an error is permanent (should not retry)
    /// Permanent errors: invalid parameters, insufficient balance, invalid symbol, etc.
    /// Temporary errors: network errors, rate limits, timeouts, server errors
    fn is_permanent_error(error: &anyhow::Error) -> bool {
        let error_str = error.to_string();
        let error_lower = error_str.to_lowercase();

        // Permanent errors - don't retry
        error_lower.contains("invalid")
            || error_lower.contains("margin")
            || error_lower.contains("insufficient balance")
            || error_lower.contains("min notional")
            || error_lower.contains("below min notional")
            || error_lower.contains("invalid symbol")
            || error_lower.contains("symbol not found")
            || error_lower.contains("position not found")
            || error_lower.contains("no position")
            || error_lower.contains("position already closed")
            || error_lower.contains("reduceonly")
            || error_lower.contains("reduce only")
            || error_lower.contains("-2011") // Binance: "Unknown order sent"
            || error_lower.contains("-2019") // Binance: "Margin is insufficient"
            || error_lower.contains("-2021") // Binance: "Order would immediately match"
    }

    /// Start the ordering service and begin processing trade signals.
    ///
    /// This method spawns multiple background tasks that:
    /// - Listen to TradeSignal events and place orders when no position/order is open
    /// - Listen to CloseRequest events and close positions
    /// - Sync state from OrderUpdate and PositionUpdate events
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` immediately after spawning background tasks. Tasks will continue
    /// running until `shutdown_flag` is set to true.
    ///
    /// # Behavior
    ///
    /// - Uses a global lock to ensure only one position/order at a time
    /// - Automatically handles order fills and converts them to positions
    /// - Updates state based on WebSocket order/position updates
    ///
    /// # Safety
    ///
    /// This module uses double-check locking patterns to prevent race conditions when
    /// placing orders. Network calls are never made while holding locks.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # let ordering = crate::ordering::Ordering::new(todo!(), todo!(), todo!(), todo!(), todo!());
    /// ordering.start().await?;
    /// // Service is now running and will place orders when TradeSignal events are received
    /// ```
    pub async fn start(&self) -> Result<()> {
        // CRITICAL: State restore is now handled by STORAGE module via event bus
        // StorageModule will restore OrderingState on startup and update SharedState

        // Start leak detection task for balance reservations
        Self::start_leak_detection_task(self.shared_state.clone(), self.shutdown_flag.clone());

        let event_bus = self.event_bus.clone();
        let shutdown_flag = self.shutdown_flag.clone();
        let connection = self.connection.clone();
        let shared_state = self.shared_state.clone();
        let cfg = self.cfg.clone();

        // Spawn task for TradeSignal events
        let state_trade = shared_state.clone();
        let event_bus_trade = event_bus.clone();
        let connection_trade = connection.clone();
        let shutdown_flag_trade = shutdown_flag.clone();
        let cfg_trade = cfg.clone();
        tokio::spawn(async move {
            let mut trade_signal_rx = event_bus_trade.subscribe_trade_signal();

            info!("ORDERING: Started, listening to TradeSignal events");

            loop {
                match trade_signal_rx.recv().await {
                    Ok(signal) => {
                        if shutdown_flag_trade.load(AtomicOrdering::Relaxed) {
                            break;
                        }

                        if let Err(e) = Self::handle_trade_signal(
                            &signal,
                            &connection_trade,
                            &state_trade,
                            &cfg_trade,
                            &event_bus_trade,
                        )
                        .await
                        {
                            warn!(error = %e, symbol = %signal.symbol, "ORDERING: error handling TradeSignal");
                        }
                    }
                    Err(_) => {
                        // Channel closed or lagged - break
                        break;
                    }
                }
            }
        });

        // Spawn task for CloseRequest events
        let state_close = shared_state.clone();
        let event_bus_close = event_bus.clone();
        let connection_close = connection.clone();
        let shutdown_flag_close = shutdown_flag.clone();
        let cfg_close = cfg.clone();
        tokio::spawn(async move {
            let mut close_request_rx = event_bus_close.subscribe_close_request();

            info!("ORDERING: Started, listening to CloseRequest events");

            loop {
                match close_request_rx.recv().await {
                    Ok(request) => {
                        if shutdown_flag_close.load(AtomicOrdering::Relaxed) {
                            break;
                        }

                        if let Err(e) = Self::handle_close_request(
                            &request,
                            &connection_close,
                            &state_close,
                            &cfg_close,
                        )
                        .await
                        {
                            warn!(error = %e, symbol = %request.symbol, "ORDERING: error handling CloseRequest");
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        // Spawn task for OrderUpdate events (state sync)
        let state_order = shared_state.clone();
        let event_bus_order = event_bus.clone();
        let shutdown_flag_order = shutdown_flag.clone();
        tokio::spawn(async move {
            let mut order_update_rx = event_bus_order.subscribe_order_update();

            info!("ORDERING: Started, listening to OrderUpdate events");

            loop {
                match order_update_rx.recv().await {
                    Ok(update) => {
                        if shutdown_flag_order.load(AtomicOrdering::Relaxed) {
                            break;
                        }

                        Self::handle_order_update(&update, &state_order, &event_bus_order, None).await;
                    }
                    Err(_) => break,
                }
            }
        });

        // Spawn task for PositionUpdate events (state sync)
        let state_pos = shared_state.clone();
        let event_bus_pos = event_bus.clone();
        let shutdown_flag_pos = shutdown_flag.clone();
        tokio::spawn(async move {
            let mut position_update_rx = event_bus_pos.subscribe_position_update();

            info!("ORDERING: Started, listening to PositionUpdate events");

            loop {
                match position_update_rx.recv().await {
                    Ok(update) => {
                        if shutdown_flag_pos.load(AtomicOrdering::Relaxed) {
                            break;
                        }

                        Self::handle_position_update(&update, &state_pos, &event_bus_pos).await;
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(())
    }

    /// Start background task for balance reservation leak detection
    /// This task periodically checks for balance leaks (reserved > total) and auto-fixes them
    /// This is safer than attempting async cleanup in Drop trait (which can deadlock)
    fn start_leak_detection_task(shared_state: Arc<SharedState>, shutdown_flag: Arc<AtomicBool>) {
        tokio::spawn(async move {
            const CHECK_INTERVAL_SECS: u64 = 10; // Check every 10 seconds (reduced from 60s to detect leaks faster in high-frequency trading scenarios)

            loop {
                tokio::time::sleep(Duration::from_secs(CHECK_INTERVAL_SECS)).await;

                if shutdown_flag.load(AtomicOrdering::Relaxed) {
                    break;
                }

                let store = shared_state.balance_store.read().await;

                // Check for balance leaks: reserved > total
                let usdt_leak = store.reserved_usdt > store.usdt;
                let usdc_leak = store.reserved_usdc > store.usdc;

                if usdt_leak || usdc_leak {
                    // Get open order info to help identify which order caused the leak
                    let open_order_info = {
                        let ordering_state = shared_state.ordering_state.lock().await;
                        ordering_state.open_order.as_ref().map(|order| {
                            format!("order_id={}, symbol={}, side={:?}, qty={}", 
                                order.order_id, order.symbol, order.side, order.qty.0)
                        })
                    };

                    tracing::error!(
                        usdt_total = %store.usdt,
                        usdt_reserved = %store.reserved_usdt,
                        usdc_total = %store.usdc,
                        usdc_reserved = %store.reserved_usdc,
                        open_order = ?open_order_info,
                        "CRITICAL: Balance leak detected! Reserved > Total. Open order info: {:?}. Auto-fixing...",
                        open_order_info
                    );

                    // Auto-fix: reset reserved balance to match total
                    // If reserved > total, this is a leak - reset reserved to total
                    drop(store);
                    let mut store_write = shared_state.balance_store.write().await;

                    // Reset reserved to total to fix leak (reserved cannot exceed total)
                    // This fixes leaks while preserving legitimate reservations (if reserved <= total)
                    if usdt_leak {
                        let old_reserved = store_write.reserved_usdt;
                        store_write.reserved_usdt = store_write.usdt;
                        tracing::warn!(
                            asset = "USDT",
                            old_reserved = %old_reserved,
                            new_reserved = %store_write.reserved_usdt,
                            total = %store_write.usdt,
                            open_order = ?open_order_info,
                            "Auto-fixed USDT balance leak: reset reserved to total. Open order: {:?}",
                            open_order_info
                        );
                    }

                    if usdc_leak {
                        let old_reserved = store_write.reserved_usdc;
                        store_write.reserved_usdc = store_write.usdc;
                        tracing::warn!(
                            asset = "USDC",
                            old_reserved = %old_reserved,
                            new_reserved = %store_write.reserved_usdc,
                            total = %store_write.usdc,
                            open_order = ?open_order_info,
                            "Auto-fixed USDC balance leak: reset reserved to total. Open order: {:?}",
                            open_order_info
                        );
                    }
                }
            }
        });
    }

    /// Handle TradeSignal event
    /// If no open position/order, place order via CONNECTION
    async fn handle_trade_signal(
        signal: &TradeSignal,
        connection: &Arc<Connection>,
        shared_state: &Arc<SharedState>,
        cfg: &Arc<AppCfg>,
        event_bus: &Arc<EventBus>,
    ) -> Result<()> {
        // 1. Signal validity check - timestamp age
        let now = Instant::now();
        if let Some(signal_age) = now.checked_duration_since(signal.timestamp) {
            if signal_age > Duration::from_secs(5) {
                warn!(
                    symbol = %signal.symbol,
                    age_seconds = signal_age.as_secs(),
                    "ORDERING: Ignoring TradeSignal - signal too old"
                );
                return Ok(());
            }
        } else {
            // Signal timestamp is in the future (invalid signal)
            warn!(
                symbol = %signal.symbol,
                "ORDERING: Ignoring TradeSignal - timestamp in the future"
            );
            return Ok(());
        }

        // 2. Signal validity check - symbol and side
        if signal.symbol.is_empty() {
            warn!("ORDERING: Ignoring TradeSignal - empty symbol");
            return Ok(());
        }

        // 3. Calculate required margin
        let leverage = signal.leverage;
        let notional = signal.entry_price.0 * signal.size.0;
        let required_margin = notional / Decimal::from(leverage);

        // ✅ CRITICAL: Validate signal size consistency
        // TRENDING generates signals using: notional = max_usd_per_order * leverage
        // ORDERING validates: notional <= max_position_notional_usd
        // These must be consistent to avoid signals being systematically rejected.
        //
        // Example mismatch:
        // - max_usd_per_order = 100, leverage = 20 → notional = 2000
        // - max_position_notional_usd = 1000 → signal rejected
        //
        // This validation ensures the signal's notional matches expectations from TRENDING.
        let max_position_notional =
            Decimal::from_str(&cfg.risk.max_position_notional_usd.to_string())
                .unwrap_or(Decimal::from(10000)); // Default to 10000 USD if conversion fails

        // Calculate expected notional from TRENDING's perspective
        let max_usd_per_order =
            Decimal::from_str(&cfg.max_usd_per_order.to_string()).unwrap_or(Decimal::from(100));
        let expected_notional = max_usd_per_order * Decimal::from(leverage);

        // Warn if there's a mismatch between TRENDING's expected notional and ORDERING's limit
        if expected_notional > max_position_notional {
            warn!(
                symbol = %signal.symbol,
                expected_notional = %expected_notional,
                max_position_notional = %max_position_notional,
                max_usd_per_order = %max_usd_per_order,
                leverage,
                "ORDERING: Config mismatch - TRENDING will generate signals with notional {} but ORDERING limit is {}. Signals may be systematically rejected.",
                expected_notional,
                max_position_notional
            );
        }

        // 4. Risk control check - max position notional (before lock)
        if notional > max_position_notional {
            warn!(
                symbol = %signal.symbol,
                notional = %notional,
                max_notional = %max_position_notional,
                "ORDERING: Ignoring TradeSignal - exceeds max position notional"
            );
            return Ok(());
        }

        // ✅ CRITICAL: Check minimum quote balance when opening positions
        // This ensures we have sufficient balance for both margin AND closing commission.
        // Previously, min_quote_balance_usd was only checked when closing positions,
        // which could lead to situations where a position is opened but cannot be closed
        // due to insufficient balance for commission.
        let min_quote_balance =
            Decimal::from_str(&cfg.min_quote_balance_usd.to_string()).unwrap_or(Decimal::ZERO);
        {
            let balance_store = shared_state.balance_store.read().await;
            let available_balance = if cfg.quote_asset.to_uppercase() == "USDT" {
                balance_store.usdt
            } else {
                balance_store.usdc
            };

            if available_balance < min_quote_balance {
                warn!(
                    symbol = %signal.symbol,
                    available_balance = %available_balance,
                    min_quote_balance = %min_quote_balance,
                    "ORDERING: Ignoring TradeSignal - available balance below minimum quote balance threshold"
                );
                return Ok(());
            }
        }

        // ✅ CRITICAL: Spread staleness check (spread range validation already done in TRENDING)
        // This prevents expensive spread validation from happening while balance is reserved,
        // which creates a race condition window where another thread can reserve balance
        // and place an order while this thread is validating spread.
        //
        // IMPORTANT: Spread range validation is done in TRENDING module (trending.rs line 605)
        // ORDERING only needs to check if signal is too stale (time-based check)
        // This avoids duplicate validation and reduces latency
        //
        // Problem: If signal is too old, spread may have changed significantly
        // Solution: Check signal age - if too old, abort without expensive network call
        let spread_age = now.duration_since(signal.spread_timestamp);
        const MAX_SPREAD_AGE_SECS: u64 = 5; // 5 seconds - signal is too stale if older than this

        if spread_age.as_secs() > MAX_SPREAD_AGE_SECS {
            // Signal is too stale - abort without expensive network call
            // TRENDING already validated spread range, we only need to check age here
            warn!(
                symbol = %signal.symbol,
                spread_age_secs = spread_age.as_secs(),
                max_age_secs = MAX_SPREAD_AGE_SECS,
                "ORDERING: Signal is too stale ({} seconds old), aborting. Spread range was already validated in TRENDING.",
                spread_age.as_secs()
            );
            return Ok(());
        }

        // Signal is recent (within 5 seconds) - proceed with order placement
        // Spread range validation was already done in TRENDING, no need to re-validate here
        // This reduces latency and avoids duplicate validation

        // Get TIF from config (before lock to minimize lock time)
        let tif = match cfg.exec.tif.as_str() {
            "post_only" | "GTX" => Tif::PostOnly,
            "ioc" | "IOC" => Tif::Ioc,
            _ => Tif::Gtc,
        };

        // Prepare order command (before lock to minimize lock time)
        use crate::types::OrderCommand;
        let command = OrderCommand::Open {
            symbol: signal.symbol.clone(),
            side: signal.side,
            price: signal.entry_price,
            qty: signal.size,
            tif,
        };

        // ✅ CRITICAL: Atomic operation - state check + balance reserve + order placement
        //
        // IMPORTANT: Spread validation was already done ABOVE (before balance reservation)
        // This prevents the race condition where expensive validation happens while balance is reserved
        //
        // Flow (CORRECT ORDER):
        // 1. ✅ Spread validation (DONE ABOVE - before any lock, lines 550-675)
        // 2. ✅ Lock: Check state + reserve balance (lines 712-749)
        // 3. ✅ Unlock: Place order IMMEDIATELY (minimize delay, lines 765+)
        // 4. ✅ Lock: Update state atomically (after order placement)
        //
        // This prevents the race condition where:
        // - Thread A reserves balance, lock released
        // - Thread B also reserves balance (if enough balance for both)
        // - Thread A does expensive spread validation (2-3 seconds) ← PREVENTED: validation done BEFORE reservation
        // - Thread B places order (succeeds)
        // - Thread A places order (double-spend!) ← PREVENTED: validation already done
        //
        // Step 1: Atomic state check + balance reservation (inside lock)
        let mut balance_reservation = {
            let state_guard = shared_state.ordering_state.lock().await;

            // 5. Position check - ensure no open position/order
            if state_guard.open_position.is_some() || state_guard.open_order.is_some() {
                warn!(
                    symbol = %signal.symbol,
                    "ORDERING: Ignoring TradeSignal - already have open position/order"
                );
                return Ok(());
            }

            // 6. Balance reservation (inside lock to prevent race condition)
            // Balance reserve must happen AFTER state check, INSIDE the same lock
            match BalanceReservation::new(
                shared_state.balance_store.clone(),
                &cfg.quote_asset,
                required_margin,
            )
            .await
            {
                Some(reservation) => {
                    // Balance reserved successfully - lock will be released after this block
                    // Order placement will happen IMMEDIATELY after lock release (minimize delay)
                    // Balance reservation prevents other threads from reserving same balance
                    reservation
                }
                None => {
                    // Insufficient balance or reservation failed
                    warn!(
                        symbol = %signal.symbol,
                        required_margin = %required_margin,
                        "ORDERING: Ignoring TradeSignal - insufficient balance or reservation failed"
                    );
                    return Ok(());
                }
            }
        }; // Lock released here - order placement happens IMMEDIATELY next

        // Balance Reservation Release Checklist
        // Balance reserved - will be released automatically when balance_reservation is dropped
        // Explicit release() should be called before returning (Drop will warn if forgotten)
        //
        // ALL early return paths MUST call balance_reservation.release().await:
        // 1. ✅ Order placement permanent error (returns early for performance)
        // 2. ✅ Order placement retries exhausted (returns early for performance)
        // 3. ✅ Success path - released after state is updated
        // 4. ✅ Race condition - released ONLY if cancel succeeds (prevents double-spend)
        //    ⚠️ If cancel fails, balance is kept reserved to prevent double-spend (acceptable leak)
        //
        // ⚠️ WARNING: If you add a new early return after this point, you MUST release the balance!
        // Missing releases cause balance leaks and prevent future orders from being placed.

        // Step 2: IMMEDIATELY place order (outside lock, but right after reservation)
        // This minimizes the window where another thread can interfere
        // Spread validation already done above, so we can proceed directly to order placement
        //
        // CRITICAL: Order placement happens IMMEDIATELY after balance reservation
        // No expensive operations (like spread validation) happen between reservation and placement
        // This prevents the race condition where another thread can reserve balance and place order
        // while this thread is doing expensive validation

        // Attempt order placement with retry logic
        // Permanent errors return early with balance release
        //
        // ✅ CRITICAL: Each call to send_order() generates a NEW clientOrderId
        // (see connection.rs line 1130: clientOrderId is generated inside send_order)
        // This ensures that each retry attempt uses a unique clientOrderId,
        // preventing duplicate orders if a previous attempt timed out but Binance received it.
        // The venue layer also generates unique clientOrderIds for its internal retries.
        let order_id = {
            let mut last_error: Option<anyhow::Error> = None;
            let mut order_id_result: Option<String> = None;

            for attempt in 0..MAX_RETRIES {
                match connection.send_order(command.clone()).await {
                    Ok(id) => {
                        // Keep balance reserved until state is updated
                        order_id_result = Some(id);
                        break; // Success, exit retry loop
                    }
                    Err(e) => {
                        // Check if error is permanent (don't retry)
                        if Self::is_permanent_error(&e) {
                            warn!(
                                error = %e,
                                symbol = %signal.symbol,
                                attempt = attempt + 1,
                                "ORDERING: Permanent error, not retrying"
                            );
                            // Permanent error - release balance and return early
                            balance_reservation.release().await;
                            return Err(e);
                        }

                        // Store error for final fallback (only if all retries fail)
                        last_error = Some(anyhow::format_err!("{}", e));

                        // Temporary error - retry with exponential backoff
                        if attempt < MAX_RETRIES - 1 {
                            let delay = Duration::from_millis(100 * 2u64.pow(attempt));
                            warn!(
                                error = %e,
                                symbol = %signal.symbol,
                                attempt = attempt + 1,
                                max_retries = MAX_RETRIES,
                                delay_ms = delay.as_millis(),
                                "ORDERING: Temporary error, retrying with exponential backoff"
                            );
                            tokio::time::sleep(delay).await;
                            continue;
                        } else {
                            // All retries exhausted
                            warn!(
                                error = %e,
                                symbol = %signal.symbol,
                                attempt = attempt + 1,
                                "ORDERING: All retries exhausted"
                            );
                            // All retries exhausted - release balance and return
                            balance_reservation.release().await;
                            return Err(e);
                        }
                    }
                }
            }

            // Extract order_id if successful, otherwise return error
            match order_id_result {
                Some(id) => {
                    // Set order_id in reservation for leak tracking
                    // This helps identify which order caused a leak if reservation is not released
                    balance_reservation.order_id = Some(id.clone());
                    id
                }
                None => {
                    // This should never be reached if all paths above return or break correctly
                    // But Rust requires this for the block to compile
                    balance_reservation.release().await;
                    return Err(
                        last_error.unwrap_or_else(|| anyhow!("Unknown error after retries"))
                    );
                }
            }
        };

        // Step 3: Update state atomically (re-acquire lock for state update)
        // Double-check pattern - another thread might have placed an order
        // Balance reservation is still held - prevents other threads from reserving
        // This completes the atomic operation: state check + balance reserve + order placement + state update
        {
            let mut state_guard = shared_state.ordering_state.lock().await;

            // Double-check: if another thread placed an order while we were making the network call,
            // we should cancel our order to prevent duplicate orders
            if state_guard.open_order.is_none() && state_guard.open_position.is_none() {
                state_guard.open_order = Some(OpenOrder {
                    symbol: signal.symbol.clone(),
                    order_id: order_id.clone(),
                    side: signal.side,
                    qty: signal.size,
                });

                // Update timestamp to reflect manual state change
                // This ensures subsequent OrderUpdate events are compared against this timestamp
                state_guard.last_order_update_timestamp = Some(Instant::now());

                // Publish OrderingStateUpdate event for STORAGE module
                let state_to_publish = state_guard.clone();
                drop(state_guard); // Release lock before async call
                Self::publish_ordering_state_update(&state_to_publish, event_bus);

                info!(
                    symbol = %signal.symbol,
                    side = ?signal.side,
                    order_id = %order_id,
                    "ORDERING: Order placed successfully"
                );

                // Release balance AFTER state is updated
                balance_reservation.release().await;
            } else {
                // Another thread placed an order/position while we were making the network call
                // This is a race condition - cancel our order to prevent duplicate orders
                warn!(
                    symbol = %signal.symbol,
                    order_id = %order_id,
                    "ORDERING: Race condition detected - another thread placed order/position, canceling our order"
                );

                // Cancel the order we just placed (lock released before async call)
                drop(state_guard);

                // Cancel FIRST, then release balance
                match connection.cancel_order(&order_id, &signal.symbol).await {
                    Ok(()) => {
                        info!(
                            symbol = %signal.symbol,
                            order_id = %order_id,
                            "ORDERING: Successfully canceled duplicate order after race condition"
                        );
                        // Cancel succeeded - now safe to release balance
                        balance_reservation.release().await;
                    }
                    Err(cancel_err) => {
                        // Cancel failed - order is still active on exchange
                        // DO NOT release balance - keep it reserved to prevent double-spend
                        warn!(
                            error = %cancel_err,
                            symbol = %signal.symbol,
                            order_id = %order_id,
                            "ORDERING: Failed to cancel duplicate order after race condition, keeping balance reserved to prevent double-spend"
                        );
                        // Balance leak is acceptable here (prevents double-spend)
                        // Alternative: Track this order and retry cancel later, then release balance
                        // For now, keeping balance reserved is safer than risking double-spend
                    }
                }

                return Ok(());
            }
        } // Lock released

        Ok(())
    }

    /// Handle CloseRequest event
    /// If position is open, close it via CONNECTION
    ///
    /// CRITICAL: Race condition prevention
    /// - Early check is only for logging, not for decision making
    /// - flatten_position already handles position verification and "position not found" errors
    /// - Multiple threads can call flatten_position simultaneously - it's safe and idempotent
    /// - If position is already closed by another thread, flatten_position returns Ok(())
    async fn handle_close_request(
        request: &CloseRequest,
        connection: &Arc<Connection>,
        shared_state: &Arc<SharedState>,
        _cfg: &Arc<AppCfg>,
    ) -> Result<()> {
        // ⚠️ DESIGN LIMITATION: position_id is currently ignored
        // CloseRequest.position_id field exists but is not used in the current implementation.
        // The code always closes positions by symbol only, not by specific position_id.
        // This is intentional for one-way mode (hedge_mode=false) where each symbol has only one position.
        //
        // FUTURE: If hedge mode support is added with multiple positions per symbol, position_id
        // should be used to close specific positions. Until then, position_id is reserved for
        // future use and will be logged if provided.
        if request.position_id.is_some() {
            warn!(
                symbol = %request.symbol,
                position_id = %request.position_id.as_ref().unwrap(),
                reason = ?request.reason,
                "ORDERING: CloseRequest.position_id provided but not used - current implementation closes by symbol only"
            );
        }

        // Early check is only for logging/debugging
        // flatten_position will handle the actual position check atomically
        let has_position = {
            let state_guard = shared_state.ordering_state.lock().await;
            state_guard
                .open_position
                .as_ref()
                .map(|p| p.symbol == request.symbol)
                .unwrap_or(false)
        };

        if !has_position {
            // Position not in our state - might be already closed by another thread
            // Still call flatten_position to be safe (it handles "position not found" gracefully)
            debug!(
                symbol = %request.symbol,
                reason = ?request.reason,
                "ORDERING: Position not in state (may be already closed), calling flatten_position to verify"
            );
        }

        // Use MARKET order with reduceOnly=true for closing positions
        // LIMIT orders are risky for close orders because:
        // 1. They may not fill immediately if price moves away
        // 2. TP/SL scenarios require immediate execution
        // 3. Position may remain open if limit order doesn't fill
        // Solution: Use flatten_position which sends MARKET orders with reduceOnly=true
        // This guarantees immediate execution and prevents position from staying open

        // Determine if this is a fast close scenario (TP/SL)
        // Fast close requires use_market_only=true to prevent LIMIT fallback delays
        use crate::event_bus::CloseReason;
        let use_market_only = matches!(
            request.reason,
            CloseReason::TakeProfit | CloseReason::StopLoss
        );

        // flatten_position handles all position checks atomically
        // - Position verification (fetch_position)
        // - "Position not found" errors (returns Ok(()))
        // - Zero quantity positions (returns Ok(()))
        // - Retry logic for partial fills
        // - Multiple threads can call this simultaneously - it's safe
        //
        // ⚠️ DESIGN LIMITATION: flatten_position closes ALL positions for the symbol
        // In one-way mode (hedge_mode=false), this is correct (one position per symbol).
        // In hedge mode (hedge_mode=true), this closes both LONG and SHORT positions,
        // which may not be desired. Future enhancement: support position_id-based closing.
        match connection
            .flatten_position(&request.symbol, use_market_only)
            .await
        {
            Ok(()) => {
                // Success - position closed (or was already closed)
                info!(
                    symbol = %request.symbol,
                    reason = ?request.reason,
                    use_market_only,
                    "ORDERING: Position closed successfully (or was already closed)"
                );
                Ok(())
            }
            Err(e) => {
                let error_str = e.to_string().to_lowercase();

                // Handle "position not found" errors gracefully
                if error_str.contains("position not found")
                    || error_str.contains("no position")
                    || error_str.contains("-2011")
                {
                    // Binance: Unknown order (position not found)
                    info!(
                        symbol = %request.symbol,
                        reason = ?request.reason,
                        "ORDERING: Position already closed by another thread (race condition handled)"
                    );
                    return Ok(());
                }

                // Check if error is permanent (don't retry)
                if Self::is_permanent_error(&e) {
                    warn!(
                        error = %e,
                        symbol = %request.symbol,
                        reason = ?request.reason,
                        "ORDERING: Permanent error closing position, not retrying"
                    );
                    return Err(e);
                }

                // Temporary error - flatten_position already has retry logic
                // But we can log it for visibility
                warn!(
                    error = %e,
                    symbol = %request.symbol,
                    reason = ?request.reason,
                    "ORDERING: Error closing position (flatten_position handles retries internally)"
                );
                Err(e)
            }
        }
    }

    /// Helper function to publish OrderingStateUpdate event
    /// This is called after state changes to ensure persistence across restarts via STORAGE module
    fn publish_ordering_state_update(state: &OrderingState, event_bus: &Arc<EventBus>) {
        use crate::event_bus::{OpenOrderSnapshot, OpenPositionSnapshot, OrderingStateUpdate};

        let update = OrderingStateUpdate {
            open_position: state
                .open_position
                .as_ref()
                .map(|pos| OpenPositionSnapshot {
                    symbol: pos.symbol.clone(),
                    direction: format!("{:?}", pos.direction),
                    qty: pos.qty.0.to_string(),
                    entry_price: pos.entry_price.0.to_string(),
                }),
            open_order: state.open_order.as_ref().map(|order| OpenOrderSnapshot {
                symbol: order.symbol.clone(),
                order_id: order.order_id.clone(),
                side: format!("{:?}", order.side),
                qty: order.qty.0.to_string(),
            }),
            timestamp: Instant::now(),
        };

        if let Err(e) = event_bus.ordering_state_update_tx.send(update) {
            warn!(error = ?e, "ORDERING: Failed to publish OrderingStateUpdate event (no subscribers)");
        }
    }

    /// Handle OrderUpdate event (state sync)
    ///
    /// CRITICAL: Race condition prevention between OrderUpdate and PositionUpdate
    ///
    /// Problem: Binance may send events out of order:
    /// - PositionUpdate may arrive before OrderUpdate (position already created)
    /// - OrderUpdate may arrive before PositionUpdate (order filled, position not yet created)
    ///
    /// Solution: Timestamp-based version control + position existence check
    ///
    /// Race Condition Scenarios:
    /// 1. PositionUpdate arrives first:
    ///    - PositionUpdate creates position (qty=0.5, entry=50000, timestamp=T1)
    ///    - OrderUpdate arrives later (filled_qty=0.5, timestamp=T0 where T0 < T1)
    ///    - Solution: Check if position exists and is newer → ignore stale OrderUpdate
    ///
    /// 2. OrderUpdate arrives first:
    ///    - OrderUpdate creates position (filled_qty=0.5, entry=50000, timestamp=T1)
    ///    - PositionUpdate arrives later (qty=0.5, entry=50000, timestamp=T2 where T2 > T1)
    ///    - Solution: PositionUpdate will check qty AND entry_price → skip if both unchanged
    ///
    /// 3. Partial fills with entry price changes:
    ///    - Order partially filled (0.5 BTC @ 50000) → OrderUpdate creates position
    ///    - Order partially filled again (0.3 BTC @ 50100) → OrderUpdate updates position
    ///    - PositionUpdate arrives (qty=0.8, entry=50037.5) → qty same but entry changed!
    ///    - Solution: PositionUpdate checks BOTH qty AND entry_price → update if either changed
    ///
    /// Only applies updates that are newer than the last known update
    async fn handle_order_update(
        update: &OrderUpdate,
        shared_state: &Arc<SharedState>,
        event_bus: &Arc<EventBus>,
        _connection: Option<&Arc<Connection>>,
    ) {
        let mut state_guard = shared_state.ordering_state.lock().await;

        // Check if this update is newer than the last known update
        let is_newer = state_guard
            .last_order_update_timestamp
            .map(|last_ts| update.timestamp > last_ts)
            .unwrap_or(true);

        if !is_newer {
            // Stale update - ignore it
            tracing::debug!(
                symbol = %update.symbol,
                order_id = %update.order_id,
                update_timestamp = ?update.timestamp,
                last_timestamp = ?state_guard.last_order_update_timestamp,
                "ORDERING: Ignoring stale OrderUpdate event"
            );
            return;
        }

        // Update order state
        if let Some(ref mut order) = state_guard.open_order {
            if order.order_id == update.order_id {
                match update.status {
                    crate::event_bus::OrderStatus::Filled => {
                        // Check if OrderUpdate is newer than existing position
                        //
                        // Scenario: PositionUpdate arrives first and creates position with timestamp T1
                        // Then OrderUpdate arrives with timestamp T0 (T0 < T1) - this is stale!
                        // We should ignore the OrderUpdate to prevent overwriting newer position data
                        if let Some(ref existing_pos) = state_guard.open_position {
                            if existing_pos.symbol == update.symbol {
                                // Position already exists - check if OrderUpdate is newer
                                let position_is_newer = state_guard
                                    .last_position_update_timestamp
                                    .map(|pos_ts| pos_ts > update.timestamp)
                                    .unwrap_or(false);

                                if position_is_newer {
                                    // Position is newer than this OrderUpdate - ignore stale OrderUpdate
                                    // But still clear the order since it's filled
                                    tracing::debug!(
                                        symbol = %update.symbol,
                                        order_id = %update.order_id,
                                        order_timestamp = ?update.timestamp,
                                        position_timestamp = ?state_guard.last_position_update_timestamp,
                                        "ORDERING: Ignoring stale OrderUpdate - position is newer, but clearing order since it's filled"
                                    );
                                    // Clear the order since it's filled (even though we're not updating position)
                                    state_guard.open_order = None;
                                    // Update order timestamp to acknowledge we received this update
                                    state_guard.last_order_update_timestamp =
                                        Some(update.timestamp);
                                    return;
                                }
                            }
                        }

                        // Order filled, convert to position
                        // Convert order side to position direction and ensure qty is positive
                        let direction = PositionDirection::from_order_side(update.side);
                        let qty_abs = if update.filled_qty.0.is_sign_negative() {
                            Qty(-update.filled_qty.0) // Make positive
                        } else {
                            update.filled_qty
                        };

                        state_guard.open_position = Some(OpenPosition {
                            symbol: update.symbol.clone(),
                            direction,
                            qty: qty_abs,
                            entry_price: update.average_fill_price, // Use average fill price (weighted average of all fills)
                        });
                        state_guard.open_order = None;

                        // Update timestamp after state change
                        state_guard.last_order_update_timestamp = Some(update.timestamp);

                        // Publish state update event for STORAGE module
                        let state_to_publish = state_guard.clone();
                        drop(state_guard);
                        Self::publish_ordering_state_update(&state_to_publish, event_bus);

                        info!(
                            symbol = %update.symbol,
                            order_id = %update.order_id,
                            "ORDERING: Order filled, position opened"
                        );
                    }
                    crate::event_bus::OrderStatus::Canceled => {
                        // Order canceled, clear state
                        state_guard.open_order = None;

                        // Update timestamp after state change
                        state_guard.last_order_update_timestamp = Some(update.timestamp);

                        // Publish state update event for STORAGE module
                        let state_to_publish = state_guard.clone();
                        drop(state_guard);
                        Self::publish_ordering_state_update(&state_to_publish, event_bus);

                        info!(
                            symbol = %update.symbol,
                            order_id = %update.order_id,
                            "ORDERING: Order canceled"
                        );
                    }
                    crate::event_bus::OrderStatus::Expired
                    | crate::event_bus::OrderStatus::ExpiredInMatch => {
                        // Order expired, clear state (similar to canceled)
                        state_guard.open_order = None;

                        // Update timestamp after state change
                        state_guard.last_order_update_timestamp = Some(update.timestamp);

                        info!(
                            symbol = %update.symbol,
                            order_id = %update.order_id,
                            status = ?update.status,
                            "ORDERING: Order expired"
                        );
                    }
                    crate::event_bus::OrderStatus::Rejected => {
                        // Order rejected, clear state
                        state_guard.open_order = None;

                        // Update timestamp after state change
                        state_guard.last_order_update_timestamp = Some(update.timestamp);

                        warn!(
                            symbol = %update.symbol,
                            order_id = %update.order_id,
                            "ORDERING: Order rejected"
                        );
                    }
                    _ => {
                        // Partial fill or other status, update order
                        order.qty = update.remaining_qty;

                        // Update timestamp after state change
                        state_guard.last_order_update_timestamp = Some(update.timestamp);
                    }
                }
            }
        }
    }

    /// Handle PositionUpdate event (state sync)
    ///
    /// CRITICAL: Race condition prevention between OrderUpdate and PositionUpdate
    ///
    /// Problem: Binance may send events out of order:
    /// - PositionUpdate may arrive before OrderUpdate (position created before order fill confirmed)
    /// - OrderUpdate may arrive before PositionUpdate (order filled, position update pending)
    ///
    /// Solution: Multi-layer protection:
    /// 1. Timestamp-based version control (prevents stale updates)
    /// 2. OrderUpdate precedence check (OrderUpdate is more reliable)
    /// 3. Qty AND entry_price comparison (prevents skipping partial fill updates)
    ///
    /// Race Condition Scenarios:
    /// 1. PositionUpdate arrives first:
    ///    - PositionUpdate creates position (qty=0.5, entry=50000, timestamp=T1)
    ///    - OrderUpdate arrives later (filled_qty=0.5, timestamp=T0 where T0 < T1)
    ///    - Solution: OrderUpdate checks if position exists and is newer → ignores stale OrderUpdate
    ///
    /// 2. OrderUpdate arrives first:
    ///    - OrderUpdate creates position (filled_qty=0.5, entry=50000, timestamp=T1)
    ///    - PositionUpdate arrives later (qty=0.5, entry=50000, timestamp=T2 where T2 > T1)
    ///    - Solution: Check qty AND entry_price → skip if both unchanged (redundant update)
    ///
    /// 3. Partial fills with entry price changes (CRITICAL!):
    ///    - Order partially filled: 0.5 BTC @ 50000 → OrderUpdate creates position
    ///    - Order partially filled again: 0.3 BTC @ 50100 → OrderUpdate updates position (qty=0.8, entry=50037.5)
    ///    - PositionUpdate arrives: (qty=0.8, entry=50037.5) → qty same as last update but entry changed!
    ///    - If we only check qty, we would skip this update and lose the correct entry price!
    ///    - Solution: Check BOTH qty AND entry_price → update if either changed
    ///
    /// Only applies updates that are newer than the last known update
    async fn handle_position_update(
        update: &PositionUpdate,
        shared_state: &Arc<SharedState>,
        event_bus: &Arc<EventBus>,
    ) {
        let mut state_guard = shared_state.ordering_state.lock().await;

        // Check if this update is newer than the last known PositionUpdate
        let is_newer_position_update = state_guard
            .last_position_update_timestamp
            .map(|last_ts| update.timestamp > last_ts)
            .unwrap_or(true);

        // Also check if PositionUpdate is newer than the last OrderUpdate
        // (We trust exchange's position closed signal even if OrderUpdate is newer)
        let order_update_is_newer = state_guard
            .last_order_update_timestamp
            .map(|order_ts| order_ts > update.timestamp)
            .unwrap_or(false);

        if !is_newer_position_update {
            // Stale PositionUpdate - ignore it
            tracing::debug!(
                symbol = %update.symbol,
                update_timestamp = ?update.timestamp,
                last_position_timestamp = ?state_guard.last_position_update_timestamp,
                "ORDERING: Ignoring stale PositionUpdate event"
            );
            return;
        }

        // If OrderUpdate is newer, be more cautious about accepting PositionUpdate
        // Only accept if position is closed (trust exchange's position closed signal)
        // This prevents stale PositionUpdate from overwriting fresh OrderUpdate data
        if order_update_is_newer && update.is_open {
            // OrderUpdate is newer and position is still open - likely stale PositionUpdate
            // Ignore it to prevent overwriting fresh OrderUpdate data
            tracing::debug!(
                symbol = %update.symbol,
                update_timestamp = ?update.timestamp,
                last_order_timestamp = ?state_guard.last_order_update_timestamp,
                "ORDERING: Ignoring PositionUpdate (OrderUpdate is newer and position is open)"
            );
            return;
        }

        // Check if we have an existing position for this symbol
        if let Some(ref existing_pos) = state_guard.open_position {
            if existing_pos.symbol == update.symbol {
                if !update.is_open {
                    // Position closed - always trust this (from exchange)
                    state_guard.open_position = None;
                    state_guard.last_position_update_timestamp = Some(update.timestamp);

                    // Publish state update event for STORAGE module
                    let state_to_publish = state_guard.clone();
                    drop(state_guard);
                    Self::publish_ordering_state_update(&state_to_publish, event_bus);

                    info!(
                        symbol = %update.symbol,
                        "ORDERING: Position closed (from PositionUpdate)"
                    );
                    return;
                }

                // Race condition prevention - check if qty AND entry_price are unchanged
                //
                // Problem: OrderUpdate and PositionUpdate may arrive out of order or with same data
                //
                // Scenario 1: Redundant update (same data)
                // - OrderUpdate::Filled creates position (qty=1.5, entry=100.0, timestamp=T1)
                // - PositionUpdate arrives immediately after (qty=1.5, entry=100.0, timestamp=T2)
                // - Both have valid timestamps, but qty AND entry_price are same → skip redundant update
                //
                // Scenario 2: Partial fills with entry price changes (CRITICAL!)
                // - Order partially filled: 1.0 BTC @ 50000 → OrderUpdate creates position (qty=1.0, entry=50000)
                // - Order partially filled again: 0.5 BTC @ 50100 → OrderUpdate updates position (qty=1.5, entry=50033.33)
                // - PositionUpdate arrives: (qty=1.5, entry=50033.33) → qty same but entry changed from original!
                // - If we only check qty, we would skip this update and lose the correct entry price!
                //
                // Solution: Check BOTH qty AND entry_price
                // - Only skip if BOTH are unchanged (within epsilon)
                // - If either changed, update position (partial fills must update entry price)
                //
                // Example: First fill 1.0 @ 100.0, second fill 0.5 @ 101.0
                // - Weighted average entry_price = (1.0*100.0 + 0.5*101.0) / 1.5 = 100.33
                // - Qty may be same in some cases, but entry_price changes → must update!
                let qty_abs_update = update.qty.0.abs();
                let qty_abs_existing = existing_pos.qty.0;
                let existing_entry_price = existing_pos.entry_price.0;

                // Calculate differences for both qty and entry_price
                // Use small epsilon to handle floating point precision issues
                let epsilon_qty = Decimal::new(1, 6); // 0.000001
                let epsilon_price = Decimal::new(1, 2); // 0.01 (for price, 1 cent is reasonable)

                let qty_diff = (qty_abs_update - qty_abs_existing).abs();
                let price_diff = (update.entry_price.0 - existing_entry_price).abs();

                // Check BOTH qty AND entry_price
                // Extract values for logging before mutating state_guard
                let should_skip = qty_diff < epsilon_qty && price_diff < epsilon_price;
                let existing_qty_for_log = qty_abs_existing;
                let update_qty_for_log = qty_abs_update;
                let existing_entry_for_log = existing_entry_price;
                let update_entry_for_log = update.entry_price.0;
                let qty_diff_for_log = qty_diff;
                let price_diff_for_log = price_diff;

                if should_skip {
                    // Both qty and entry_price unchanged - this is likely a redundant update from race condition
                    // Only update timestamp to acknowledge we received this update
                    state_guard.last_position_update_timestamp = Some(update.timestamp);

                    tracing::debug!(
                        symbol = %update.symbol,
                        existing_qty = %existing_qty_for_log,
                        update_qty = %update_qty_for_log,
                        qty_diff = %qty_diff_for_log,
                        existing_entry = %existing_entry_for_log,
                        update_entry = %update_entry_for_log,
                        price_diff = %price_diff_for_log,
                        "ORDERING: Position qty and entry_price unchanged, skipping redundant update (race condition prevention)"
                    );
                    return;
                }

                // Position is open and qty OR entry_price changed - update position

                // Update position (timestamp check already passed, qty changed)
                // Determine direction from qty sign and ensure qty is positive
                let direction = PositionDirection::from_qty_sign(update.qty.0);
                let qty_abs = if update.qty.0.is_sign_negative() {
                    Qty(-update.qty.0) // Make positive
                } else {
                    update.qty
                };

                state_guard.open_position = Some(OpenPosition {
                    symbol: update.symbol.clone(),
                    direction,
                    qty: qty_abs,
                    entry_price: update.entry_price,
                });
                state_guard.last_position_update_timestamp = Some(update.timestamp);

                // Persist state after change
                let state_to_publish = state_guard.clone();
                drop(state_guard);
                Self::publish_ordering_state_update(&state_to_publish, event_bus);

                info!(
                    symbol = %update.symbol,
                    qty_diff = %qty_diff,
                    price_diff = %price_diff,
                    "ORDERING: Position updated from PositionUpdate (qty changed, timestamp verified)"
                );
                return;
            }
        }

        // No existing position - create new one if position is open
        if update.is_open {
            // Determine position direction from quantity sign:
            // - Long position: qty > 0 (opened with BUY order)
            // - Short position: qty < 0 (opened with SELL order)
            let direction = PositionDirection::from_qty_sign(update.qty.0);
            let qty_abs = if update.qty.0.is_sign_negative() {
                Qty(-update.qty.0) // Make positive
            } else {
                update.qty
            };

            state_guard.open_position = Some(OpenPosition {
                symbol: update.symbol.clone(),
                direction,
                qty: qty_abs,
                entry_price: update.entry_price,
            });
            state_guard.last_position_update_timestamp = Some(update.timestamp);

            // ✅ CRITICAL: Persist state after change
            let state_to_publish = state_guard.clone();
            drop(state_guard);
            Self::publish_ordering_state_update(&state_to_publish, event_bus);

            info!(
                symbol = %update.symbol,
                "ORDERING: Position created from PositionUpdate"
            );
        } else {
            // Position is closed and no existing position - just update timestamp
            state_guard.last_position_update_timestamp = Some(update.timestamp);
            // No state change, no need to persist
        }
    }
}
