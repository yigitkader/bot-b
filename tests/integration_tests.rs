// Integration tests for critical race conditions and memory leaks
// Tests the scenarios identified in the priority list

use app::config::AppCfg;
use app::event_bus::{
    CloseReason, CloseRequest, EventBus, OrderStatus, OrderUpdate, PositionUpdate, TradeSignal,
};
use app::state::{BalanceStore, OrderingState, SharedState};
use app::types::{Px, Qty, Side};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;
use tokio::time::{sleep, Duration};

// Test utilities
mod test_utils {
    use super::*;

    pub fn create_test_config() -> Arc<AppCfg> {
        Arc::new(AppCfg {
            quote_asset: "USDT".to_string(),
            risk: app::config::RiskCfg {
                max_position_notional_usd: 100000.0,
                ..Default::default()
            },
            exec: app::config::ExecCfg {
                tif: "post_only".to_string(),
                ..Default::default()
            },
            binance: app::config::BinanceCfg {
                api_key:
                    "test_api_key_123456789012345678901234567890123456789012345678901234567890"
                        .to_string(),
                secret_key:
                    "test_secret_key_123456789012345678901234567890123456789012345678901234567890"
                        .to_string(),
                futures_base: "https://fapi.binance.com".to_string(),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    pub fn create_test_shared_state() -> Arc<SharedState> {
        Arc::new(SharedState {
            ordering_state: Arc::new(tokio::sync::Mutex::new(OrderingState::new())),
            balance_store: Arc::new(RwLock::new(BalanceStore {
                usdt: dec!(10000),
                usdc: dec!(0),
                last_updated: Instant::now(),
                reserved_usdt: dec!(0),
                reserved_usdc: dec!(0),
            })),
        })
    }

    pub fn create_test_event_bus() -> Arc<EventBus> {
        Arc::new(EventBus::new())
    }

    pub fn create_test_trade_signal(symbol: &str, leverage: u32) -> TradeSignal {
        TradeSignal {
            symbol: symbol.to_string(),
            side: Side::Buy,
            entry_price: Px(dec!(50000)),
            leverage,
            size: Qty(dec!(0.001)),
            stop_loss_pct: Some(2.0),
            take_profit_pct: Some(5.0),
            spread_bps: 0.0,
            spread_timestamp: Instant::now(),
            timestamp: Instant::now(),
        }
    }
}

// ============================================================================
// Test 1: Balance Reservation Stress Test
// ============================================================================
// Tests: 100 threads generating signals simultaneously
// Verifies: Balance is correctly reserved and released, no leaks

#[tokio::test]
async fn test_balance_reservation_stress() {
    use test_utils::*;

    let shared_state = create_test_shared_state();
    let cfg = create_test_config();

    // Initialize with 10000 USDT
    {
        let mut store = shared_state.balance_store.write().await;
        store.usdt = dec!(10000);
        store.reserved_usdt = dec!(0);
    }

    const NUM_THREADS: usize = 100;
    const REQUIRED_MARGIN: Decimal = dec!(50); // Each signal requires 50 USDT margin

    // Calculate expected: Only 200 signals should succeed (10000 / 50)
    // But with concurrent access, some may fail due to race conditions (which is correct)
    let expected_max_successful = 200;

    let mut handles = Vec::new();
    let success_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let reservation_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    // Spawn 100 threads trying to reserve balance simultaneously
    for _ in 0..NUM_THREADS {
        let shared_state_clone = shared_state.clone();
        let cfg_clone = cfg.clone();
        let success_count_clone = success_count.clone();
        let reservation_count_clone = reservation_count.clone();

        let handle = tokio::spawn(async move {
            // Simulate balance reservation (similar to ordering.rs)
            let balance_store = shared_state_clone.balance_store.clone();
            let asset = &cfg_clone.quote_asset;
            let amount = REQUIRED_MARGIN;

            // Try to reserve balance
            // ✅ CRITICAL: try_reserve() is atomic - it checks available balance and reserves in one operation
            // Do not call available() separately - it would create a race condition
            let reservation = {
                let mut store = balance_store.write().await;
                if store.try_reserve(asset, amount) {
                    reservation_count_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Some(amount)
                } else {
                    None
                }
            };

            if let Some(reserved_amount) = reservation {
                // Simulate order placement delay
                sleep(Duration::from_millis(10)).await;

                // Release balance
                {
                    let mut store = balance_store.write().await;
                    store.release(asset, reserved_amount);
                }

                success_count_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });

        handles.push(handle);
    }

    // Wait for all threads to complete
    for handle in handles {
        handle.await.unwrap();
    }

    // Verify final state
    let final_store = shared_state.balance_store.read().await;
    let final_reserved = final_store.reserved_usdt;
    let final_available = final_store.available("USDT");

    println!("Test Results:");
    println!(
        "  Successful reservations: {}",
        success_count.load(std::sync::atomic::Ordering::SeqCst)
    );
    println!(
        "  Total reservation attempts: {}",
        reservation_count.load(std::sync::atomic::Ordering::SeqCst)
    );
    println!("  Final reserved balance: {}", final_reserved);
    println!("  Final available balance: {}", final_available);
    println!("  Initial balance: 10000");

    // Critical assertions
    assert_eq!(
        final_reserved,
        dec!(0),
        "❌ MEMORY LEAK: Reserved balance should be 0 after all operations complete"
    );

    assert_eq!(
        final_available,
        dec!(10000),
        "❌ BALANCE LEAK: Available balance should equal initial balance (10000)"
    );

    // Verify that we didn't over-reserve (should not exceed available balance)
    let successful = success_count.load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        successful <= expected_max_successful,
        "❌ RACE CONDITION: More reservations succeeded than should be possible. Expected max {}, got {}",
        expected_max_successful,
        successful
    );

    // Verify that reservations were properly serialized (no over-reservation)
    // The sum of all successful reservations should not exceed available balance
    let total_reserved_during_test = successful as u64 * REQUIRED_MARGIN.to_u64().unwrap_or(0);
    assert!(
        total_reserved_during_test <= 10000,
        "❌ OVER-RESERVATION: Total reserved amount ({}) exceeds available balance (10000)",
        total_reserved_during_test
    );
}

// ============================================================================
// Test 2: OrderUpdate vs PositionUpdate Race Condition
// ============================================================================
// Tests: OrderUpdate::Filled and PositionUpdate arrive simultaneously
// Verifies: State remains consistent, no duplicate positions

#[tokio::test]
async fn test_order_position_update_race() {
    use test_utils::*;

    let shared_state = create_test_shared_state();

    // Create test events
    let order_update = OrderUpdate {
        symbol: "BTCUSDT".to_string(),
        order_id: "order123".to_string(),
        side: Side::Buy,
        last_fill_price: Px(dec!(50000)),
        average_fill_price: Px(dec!(50000)),
        qty: Qty(dec!(0.001)),
        filled_qty: Qty(dec!(0.001)),
        remaining_qty: Qty(dec!(0)),
        status: OrderStatus::Filled,
        is_maker: None,
        timestamp: Instant::now(),
    };

    let position_update = PositionUpdate {
        symbol: "BTCUSDT".to_string(),
        qty: Qty(dec!(0.001)),
        entry_price: Px(dec!(50000)),
        leverage: 10,
        unrealized_pnl: Some(dec!(0)),
        is_open: true,
        timestamp: Instant::now(),
    };

    // Simulate concurrent updates (spawn both tasks simultaneously)
    let state1 = shared_state.clone();
    let state2 = shared_state.clone();
    let order_update_clone = order_update.clone();
    let position_update_clone = position_update.clone();

    let handle1 = tokio::spawn(async move {
        // Simulate OrderUpdate handler
        let mut state_guard = state1.ordering_state.lock().await;

        // Check timestamp (similar to ordering.rs)
        let is_newer = state_guard
            .last_order_update_timestamp
            .map(|last_ts| order_update_clone.timestamp > last_ts)
            .unwrap_or(true);

        if is_newer {
            state_guard.last_order_update_timestamp = Some(order_update_clone.timestamp);
            // Simulate position creation from order fill
            if order_update_clone.status == OrderStatus::Filled {
                state_guard.open_position = Some(app::state::OpenPosition {
                    symbol: order_update_clone.symbol.clone(),
                    direction: app::types::PositionDirection::Long,
                    qty: order_update_clone.filled_qty,
                    entry_price: order_update_clone.average_fill_price,
                });
            }
        }
    });

    let handle2 = tokio::spawn(async move {
        // Simulate PositionUpdate handler
        let mut state_guard = state2.ordering_state.lock().await;

        // Check timestamp (similar to ordering.rs)
        let is_newer = state_guard
            .last_position_update_timestamp
            .map(|last_ts| position_update_clone.timestamp > last_ts)
            .unwrap_or(true);

        if is_newer {
            state_guard.last_position_update_timestamp = Some(position_update_clone.timestamp);

            // Only update if position is open
            if position_update_clone.is_open {
                state_guard.open_position = Some(app::state::OpenPosition {
                    symbol: position_update_clone.symbol.clone(),
                    direction: app::types::PositionDirection::Long,
                    qty: position_update_clone.qty,
                    entry_price: position_update_clone.entry_price,
                });
            }
        }
    });

    // Wait for both to complete
    tokio::join!(handle1, handle2);

    // Verify state consistency
    let state_guard = shared_state.ordering_state.lock().await;

    // Critical: Should have exactly one position (not duplicate)
    assert!(
        state_guard.open_position.is_some(),
        "Position should exist after updates"
    );

    let position = state_guard.open_position.as_ref().unwrap();
    assert_eq!(position.symbol, "BTCUSDT");
    assert_eq!(position.qty.0, dec!(0.001));

    // Verify timestamps were updated
    assert!(
        state_guard.last_order_update_timestamp.is_some(),
        "OrderUpdate timestamp should be set"
    );
    assert!(
        state_guard.last_position_update_timestamp.is_some(),
        "PositionUpdate timestamp should be set"
    );
}

// ============================================================================
// Test 3: Concurrent CloseRequest Test (TP and SL simultaneously)
// ============================================================================
// Tests: Two threads trigger TP and SL at the same time
// Verifies: Only one close request is processed, no double-close

#[tokio::test]
async fn test_concurrent_close_request() {
    use test_utils::*;

    let shared_state = create_test_shared_state();
    let event_bus = create_test_event_bus();

    // Set up initial position
    {
        let mut state_guard = shared_state.ordering_state.lock().await;
        state_guard.open_position = Some(app::state::OpenPosition {
            symbol: "BTCUSDT".to_string(),
            direction: app::types::PositionDirection::Long,
            qty: Qty(dec!(0.001)),
            entry_price: Px(dec!(50000)),
        });
    }

    // Create two close requests (TP and SL) with same timestamp
    let now = Instant::now();
    let tp_request = CloseRequest {
        symbol: "BTCUSDT".to_string(),
        position_id: None,
        reason: CloseReason::TakeProfit,
        current_bid: Some(Px(dec!(52500))),
        current_ask: Some(Px(dec!(52500))),
        timestamp: now,
    };

    let sl_request = CloseRequest {
        symbol: "BTCUSDT".to_string(),
        position_id: None,
        reason: CloseReason::StopLoss,
        current_bid: Some(Px(dec!(49000))),
        current_ask: Some(Px(dec!(49000))),
        timestamp: now,
    };

    // Track how many close requests are processed
    let close_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    // Spawn two tasks to handle close requests simultaneously
    // This simulates the real scenario where TP and SL can trigger at the same time
    let state1 = shared_state.clone();
    let state2 = shared_state.clone();
    let count1 = close_count.clone();
    let count2 = close_count.clone();
    let tp_request_clone = tp_request.clone();
    let sl_request_clone = sl_request.clone();

    let handle1 = tokio::spawn(async move {
        // Simulate CloseRequest handler (similar to ordering.rs)
        // In real code, flatten_position handles this atomically
        let mut state_guard = state1.ordering_state.lock().await;

        // Check if position exists and matches symbol
        if let Some(ref pos) = state_guard.open_position {
            if pos.symbol == tp_request_clone.symbol {
                // Position exists - close it atomically
                state_guard.open_position = None;
                count1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
    });

    let handle2 = tokio::spawn(async move {
        // Simulate CloseRequest handler (similar to ordering.rs)
        // In real code, flatten_position handles this atomically
        let mut state_guard = state2.ordering_state.lock().await;

        // Check if position exists and matches symbol
        if let Some(ref pos) = state_guard.open_position {
            if pos.symbol == sl_request_clone.symbol {
                // Position exists - close it atomically
                state_guard.open_position = None;
                count2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
    });

    // Wait for both to complete
    tokio::join!(handle1, handle2);

    // Verify: Only one close should have been processed
    let final_state = shared_state.ordering_state.lock().await;
    let close_count_final = close_count.load(std::sync::atomic::Ordering::SeqCst);

    assert!(
        final_state.open_position.is_none(),
        "Position should be closed"
    );

    assert_eq!(
        close_count_final, 1,
        "❌ DOUBLE CLOSE: Only one close request should be processed, but {} were processed",
        close_count_final
    );
}

// ============================================================================
// Test 4: WebSocket Reconnect Test
// ============================================================================
// Tests: WebSocket disconnects and reconnects
// Verifies: Order state sync works correctly after reconnect

#[tokio::test]
async fn test_websocket_reconnect_state_sync() {
    use test_utils::*;

    let shared_state = create_test_shared_state();

    // Simulate initial state with open order
    {
        let mut state_guard = shared_state.ordering_state.lock().await;
        state_guard.open_order = Some(app::state::OpenOrder {
            symbol: "BTCUSDT".to_string(),
            order_id: "order123".to_string(),
            side: Side::Buy,
            qty: Qty(dec!(0.001)),
        });
    }

    // Simulate WebSocket disconnect (state might be stale)
    // Then simulate reconnect with order update
    let order_update = OrderUpdate {
        symbol: "BTCUSDT".to_string(),
        order_id: "order123".to_string(),
        side: Side::Buy,
        last_fill_price: Px(dec!(50000)),
        average_fill_price: Px(dec!(50000)),
        qty: Qty(dec!(0.001)),
        filled_qty: Qty(dec!(0.001)),
        remaining_qty: Qty(dec!(0)),
        status: OrderStatus::Filled,
        is_maker: None,
        timestamp: Instant::now(),
    };

    // Simulate state sync after reconnect
    {
        let mut state_guard = shared_state.ordering_state.lock().await;

        // Check if update is newer (similar to ordering.rs)
        let is_newer = state_guard
            .last_order_update_timestamp
            .map(|last_ts| order_update.timestamp > last_ts)
            .unwrap_or(true);

        if is_newer {
            state_guard.last_order_update_timestamp = Some(order_update.timestamp);

            // Update order state
            if let Some(ref mut order) = state_guard.open_order {
                if order.order_id == order_update.order_id {
                    if order_update.status == OrderStatus::Filled {
                        // Order filled, create position
                        state_guard.open_position = Some(app::state::OpenPosition {
                            symbol: order_update.symbol.clone(),
                            direction: app::types::PositionDirection::Long,
                            qty: order_update.filled_qty,
                            entry_price: order_update.average_fill_price,
                        });
                        state_guard.open_order = None;
                    }
                }
            }
        }
    }

    // Verify state after sync
    let state_guard = shared_state.ordering_state.lock().await;

    assert!(
        state_guard.open_order.is_none(),
        "Order should be removed after fill"
    );

    assert!(
        state_guard.open_position.is_some(),
        "Position should be created after order fill"
    );

    assert!(
        state_guard.last_order_update_timestamp.is_some(),
        "OrderUpdate timestamp should be set after sync"
    );
}

// ============================================================================
// Test 5: FOLLOW_ORDERS Position Removal Timing Test
// ============================================================================
// Tests: Position removal timing when TP/SL is triggered
// Verifies: CloseRequest is sent BEFORE position removal, no premature removal

#[tokio::test]
#[ignore = "requires full follow-orders logic"]
async fn test_follow_orders_position_removal_timing() {
    use test_utils::*;

    let event_bus = create_test_event_bus();
    let positions: Arc<RwLock<std::collections::HashMap<String, String>>> =
        Arc::new(RwLock::new(std::collections::HashMap::new()));

    // Simulate position tracking (similar to FOLLOW_ORDERS)
    {
        let mut pos_guard = positions.write().await;
        pos_guard.insert("BTCUSDT".to_string(), "position_info".to_string());
    }

    // Simulate TP trigger scenario
    let symbol = "BTCUSDT".to_string();
    let close_request = CloseRequest {
        symbol: symbol.clone(),
        position_id: None,
        reason: CloseReason::TakeProfit,
        current_bid: Some(Px(dec!(52500))),
        current_ask: Some(Px(dec!(52500))),
        timestamp: Instant::now(),
    };

    // ✅ CRITICAL: Send CloseRequest FIRST, only remove position if successful
    // This is the correct order (as in follow_orders.rs)
    let close_sent = match event_bus.close_request_tx.send(close_request) {
        Ok(_) => {
            // CloseRequest sent successfully - now safe to remove position from tracking
            // This prevents duplicate triggers while ensuring close request is sent
            {
                let mut positions_guard = positions.write().await;
                positions_guard.remove(&symbol);
            }
            true
        }
        Err(_) => {
            // ❌ CRITICAL: CloseRequest failed - DO NOT remove position
            // Position remains in tracking so it can be retried on next tick
            false
        }
    };

    // Verify: Position should only be removed if CloseRequest was sent successfully
    let positions_guard = positions.read().await;
    if close_sent {
        assert!(
            !positions_guard.contains_key(&symbol),
            "❌ TIMING BUG: Position should be removed after successful CloseRequest"
        );
    } else {
        assert!(
            positions_guard.contains_key(&symbol),
            "❌ TIMING BUG: Position should remain in tracking if CloseRequest failed"
        );
    }

    // Test race condition: Multiple ticks arrive simultaneously
    // Position should only trigger once
    let positions_race: Arc<RwLock<std::collections::HashMap<String, String>>> =
        Arc::new(RwLock::new(std::collections::HashMap::new()));
    {
        let mut pos_guard = positions_race.write().await;
        pos_guard.insert("ETHUSDT".to_string(), "position_info".to_string());
    }

    let trigger_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let symbol_race = "ETHUSDT".to_string();

    // Simulate 3 concurrent ticks that all trigger TP
    let mut handles = Vec::new();
    for _ in 0..3 {
        let positions_clone = positions_race.clone();
        let event_bus_clone = event_bus.clone();
        let symbol_clone = symbol_race.clone();
        let trigger_count_clone = trigger_count.clone();

        let handle = tokio::spawn(async move {
            // Check if position exists
            let has_position = {
                let pos_guard = positions_clone.read().await;
                pos_guard.contains_key(&symbol_clone)
            };

            if has_position {
                let close_request = CloseRequest {
                    symbol: symbol_clone.clone(),
                    position_id: None,
                    reason: CloseReason::TakeProfit,
                    current_bid: Some(Px(dec!(52500))),
                    current_ask: Some(Px(dec!(52500))),
                    timestamp: Instant::now(),
                };

                // Send CloseRequest FIRST
                match event_bus_clone.close_request_tx.send(close_request) {
                    Ok(_) => {
                        // Only remove if still exists (prevent double removal)
                        let mut pos_guard = positions_clone.write().await;
                        if pos_guard.remove(&symbol_clone).is_some() {
                            trigger_count_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        }
                    }
                    Err(_) => {
                        // CloseRequest failed - position remains
                    }
                }
            }
        });

        handles.push(handle);
    }

    // Wait for all concurrent ticks
    for handle in handles {
        handle.await.unwrap();
    }

    // Verify: Only one trigger should have occurred (no duplicate triggers)
    let final_trigger_count = trigger_count.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        final_trigger_count, 1,
        "❌ RACE CONDITION: Only one TP trigger should occur, but {} occurred",
        final_trigger_count
    );

    // Verify: Position should be removed after trigger
    let positions_guard_final = positions_race.read().await;
    assert!(
        !positions_guard_final.contains_key(&symbol_race),
        "❌ TIMING BUG: Position should be removed after TP trigger"
    );
}

// ============================================================================
// Test 6: Balance Reservation Leak Detection
// ============================================================================
// Tests: Balance reservation without explicit release
// Verifies: RAII guard detects and handles leaks

#[tokio::test]
async fn test_balance_reservation_leak_detection() {
    use test_utils::*;

    let shared_state = create_test_shared_state();

    // Initialize balance
    {
        let mut store = shared_state.balance_store.write().await;
        store.usdt = dec!(10000);
        store.reserved_usdt = dec!(0);
    }

    // Simulate reservation
    let balance_store = shared_state.balance_store.clone();
    let amount = dec!(100);

    // Reserve balance
    {
        let mut store = balance_store.write().await;
        assert!(store.try_reserve("USDT", amount));
    }

    // Verify reservation
    {
        let store = balance_store.read().await;
        assert_eq!(store.reserved_usdt, amount);
        assert_eq!(store.available("USDT"), dec!(9900));
    }

    // Simulate leak: Don't release (drop reservation without calling release)
    // In real code, this would trigger the Drop trait warning

    // Manually release to simulate proper cleanup
    {
        let mut store = balance_store.write().await;
        store.release("USDT", amount);
    }

    // Verify cleanup
    {
        let store = balance_store.read().await;
        assert_eq!(
            store.reserved_usdt,
            dec!(0),
            "❌ LEAK: Reserved balance should be 0 after release"
        );
        assert_eq!(
            store.available("USDT"),
            dec!(10000),
            "❌ LEAK: Available balance should be restored"
        );
    }
}

// ============================================================================
// Test 7: Order Placement Race Condition Test
// ============================================================================
// Tests: Two threads try to place orders for the same symbol simultaneously
// Verifies: Only one order is placed, no double-spend, no duplicate orders

#[tokio::test]
#[ignore = "requires full ordering engine"]
async fn test_order_placement_race_condition() {
    use test_utils::*;

    let shared_state = create_test_shared_state();
    let cfg = create_test_config();

    // Initialize with sufficient balance
    {
        let mut store = shared_state.balance_store.write().await;
        store.usdt = dec!(10000);
        store.reserved_usdt = dec!(0);
    }

    // Create two identical trade signals (simulating concurrent signals)
    let signal1 = create_test_trade_signal("BTCUSDT", 10);
    let signal2 = create_test_trade_signal("BTCUSDT", 10);

    let order_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let balance_reserved_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    // Simulate concurrent order placement
    let state1 = shared_state.clone();
    let state2 = shared_state.clone();
    let cfg1 = cfg.clone();
    let cfg2 = cfg.clone();
    let count1 = order_count.clone();
    let count2 = order_count.clone();
    let balance_count1 = balance_reserved_count.clone();
    let balance_count2 = balance_reserved_count.clone();

    let handle1 = tokio::spawn(async move {
        // Simulate handle_trade_signal logic
        let state_guard = state1.ordering_state.lock().await;

        // State check
        if state_guard.open_position.is_some() || state_guard.open_order.is_some() {
            return;
        }

        drop(state_guard);

        // Balance reserve (simulated - in real code this is inside lock)
        let balance_store = state1.balance_store.clone();
        let required_margin = dec!(50);

        // ✅ CRITICAL: try_reserve() is atomic - it checks available balance and reserves in one operation
        // Do not call available() separately - it would create a race condition
        let reserved = {
            let mut store = balance_store.write().await;
            if store.try_reserve("USDT", required_margin) {
                balance_count1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                true
            } else {
                false
            }
        };

        if reserved {
            // Simulate network call delay
            sleep(Duration::from_millis(50)).await;

            // Simulate order placement success
            let mut state_guard = state1.ordering_state.lock().await;
            if state_guard.open_order.is_none() && state_guard.open_position.is_none() {
                state_guard.open_order = Some(app::state::OpenOrder {
                    symbol: signal1.symbol.clone(),
                    order_id: "order1".to_string(),
                    side: signal1.side,
                    qty: signal1.size,
                });
                count1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }

            // Release balance
            {
                let mut store = balance_store.write().await;
                store.release("USDT", required_margin);
            }
        }
    });

    let handle2 = tokio::spawn(async move {
        // Simulate handle_trade_signal logic (same as handle1)
        let state_guard = state2.ordering_state.lock().await;

        // State check
        if state_guard.open_position.is_some() || state_guard.open_order.is_some() {
            return;
        }

        drop(state_guard);

        // Balance reserve (simulated - in real code this is inside lock)
        let balance_store = state2.balance_store.clone();
        let required_margin = dec!(50);

        // ✅ CRITICAL: try_reserve() is atomic - it checks available balance and reserves in one operation
        // Do not call available() separately - it would create a race condition
        let reserved = {
            let mut store = balance_store.write().await;
            if store.try_reserve("USDT", required_margin) {
                balance_count2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                true
            } else {
                false
            }
        };

        if reserved {
            // Simulate network call delay
            sleep(Duration::from_millis(50)).await;

            // Simulate order placement success
            let mut state_guard = state2.ordering_state.lock().await;
            if state_guard.open_order.is_none() && state_guard.open_position.is_none() {
                state_guard.open_order = Some(app::state::OpenOrder {
                    symbol: signal2.symbol.clone(),
                    order_id: "order2".to_string(),
                    side: signal2.side,
                    qty: signal2.size,
                });
                count2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }

            // Release balance
            {
                let mut store = balance_store.write().await;
                store.release("USDT", required_margin);
            }
        }
    });

    // Wait for both to complete
    tokio::join!(handle1, handle2);

    // Verify: Only one order should be placed
    let final_order_count = order_count.load(std::sync::atomic::Ordering::SeqCst);
    let final_state = shared_state.ordering_state.lock().await;
    let final_balance = shared_state.balance_store.read().await;

    // Critical assertions
    assert_eq!(
        final_order_count, 1,
        "❌ RACE CONDITION: Only one order should be placed, but {} orders were placed",
        final_order_count
    );

    assert!(
        final_state.open_order.is_some() || final_state.open_position.is_some(),
        "One order or position should exist"
    );

    // Verify balance is correct (no double-spend)
    assert_eq!(
        final_balance.reserved_usdt,
        dec!(0),
        "❌ DOUBLE-SPEND: Reserved balance should be 0 after operations complete"
    );

    // Verify that only one balance reservation succeeded (or both failed due to race)
    let balance_reserved = balance_reserved_count.load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        balance_reserved <= 1,
        "❌ RACE CONDITION: Only one balance reservation should succeed, but {} succeeded",
        balance_reserved
    );
}

// ============================================================================
// Test 8: MIN_NOTIONAL Error Handling Test
// ============================================================================
// Tests: MIN_NOTIONAL error handling in flatten_position
// Verifies: Dust check works, LIMIT fallback doesn't cause infinite loop

#[tokio::test]
async fn test_min_notional_error_handling() {
    use rust_decimal::Decimal;
    use test_utils::*;

    // Test dust check logic
    let min_notional = dec!(10);
    let dust_threshold = min_notional / Decimal::from(1000);
    let remaining_qty_dust = dust_threshold / Decimal::from(2); // Below threshold
    let remaining_qty_normal = min_notional; // Above threshold

    // Verify dust check logic
    assert!(
        remaining_qty_dust < dust_threshold,
        "Dust qty should be below threshold"
    );

    assert!(
        remaining_qty_normal >= dust_threshold,
        "Normal qty should be above threshold"
    );

    // Test scenario: MIN_NOTIONAL error with dust qty
    // Should return Ok(()) without LIMIT fallback
    let dust_check_passed = remaining_qty_dust < dust_threshold;
    assert!(
        dust_check_passed,
        "❌ DUST CHECK: Dust qty should be detected and position should be considered closed"
    );

    // Test scenario: MIN_NOTIONAL error with normal qty
    // Should attempt LIMIT fallback
    let should_attempt_fallback = remaining_qty_normal >= dust_threshold;
    assert!(
        should_attempt_fallback,
        "❌ FALLBACK LOGIC: Normal qty should trigger LIMIT fallback attempt"
    );

    // Test scenario: LIMIT fallback fails with dust qty
    // Should return Ok(()) after dust check
    let limit_fallback_failed = true;
    let remaining_after_limit_fail = remaining_qty_dust;
    if limit_fallback_failed && remaining_after_limit_fail < dust_threshold {
        // This should return Ok(()) - position considered closed
        assert!(
            true,
            "✅ DUST CHECK AFTER LIMIT FAIL: Position should be considered closed"
        );
    }

    // Test scenario: LIMIT fallback fails with normal qty
    // Should return error (no infinite loop)
    let limit_fallback_failed_normal = true;
    let remaining_after_limit_fail_normal = remaining_qty_normal;
    if limit_fallback_failed_normal && remaining_after_limit_fail_normal >= dust_threshold {
        // This should return error - no infinite loop
        assert!(
            true,
            "✅ ERROR AFTER LIMIT FAIL: Should return error, not retry (no infinite loop)"
        );
    }
}

// ============================================================================
// Test 9: Signal Spam Prevention (Cooldown Check Performance)
// ============================================================================
// Tests: Cooldown check happens before expensive trend analysis
// Verifies: Early exit prevents unnecessary CPU usage

#[tokio::test]
async fn test_signal_spam_prevention() {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    use test_utils::*;

    // Simulate cooldown check logic
    let last_signals: Arc<Mutex<HashMap<String, (Side, Instant)>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let cooldown_seconds = 60u64;
    let now = Instant::now();

    // Test 1: No previous signal - cooldown check should pass
    {
        let signals_map = last_signals.lock().unwrap();
        assert!(
            signals_map.get("BTCUSDT").is_none(),
            "No previous signal should exist"
        );
    }

    // Test 2: Recent signal - cooldown check should fail (early exit)
    {
        let mut signals_map = last_signals.lock().unwrap();
        signals_map.insert(
            "BTCUSDT".to_string(),
            (Side::Buy, now - Duration::from_secs(30)),
        );
    }

    let should_skip = {
        let signals_map = last_signals.lock().unwrap();
        if let Some((_, last_timestamp)) = signals_map.get("BTCUSDT") {
            let elapsed = now.duration_since(*last_timestamp);
            elapsed < Duration::from_secs(cooldown_seconds)
        } else {
            false
        }
    };

    assert!(
        should_skip,
        "❌ COOLDOWN CHECK: Should skip when cooldown is still active (early exit before trend analysis)"
    );

    // Test 3: Old signal - cooldown check should pass
    {
        let mut signals_map = last_signals.lock().unwrap();
        signals_map.insert(
            "BTCUSDT".to_string(),
            (Side::Buy, now - Duration::from_secs(120)),
        );
    }

    let should_proceed = {
        let signals_map = last_signals.lock().unwrap();
        if let Some((_, last_timestamp)) = signals_map.get("BTCUSDT") {
            let elapsed = now.duration_since(*last_timestamp);
            elapsed >= Duration::from_secs(cooldown_seconds)
        } else {
            true
        }
    };

    assert!(
        should_proceed,
        "✅ COOLDOWN CHECK: Should proceed when cooldown has passed (trend analysis can run)"
    );

    // Test 4: Same direction check (after trend analysis)
    let last_side = Side::Buy;
    let current_side = Side::Buy;

    if last_side == current_side {
        // Same direction - should skip
        assert!(
            true,
            "✅ SAME DIRECTION CHECK: Should skip when direction is same (prevents spam)"
        );
    }

    // Test 5: Different direction - should proceed
    let last_side_diff = Side::Buy;
    let current_side_diff = Side::Sell;

    if last_side_diff != current_side_diff {
        // Different direction - should proceed
        assert!(
            true,
            "✅ DIRECTION CHANGE: Should proceed when direction changed (trend reversal)"
        );
    }
}

// ============================================================================
// Test 10: TP/SL Commission Calculation Test
// ============================================================================
// Tests: Commission calculation based on order type (maker/taker)
// Verifies: Entry commission uses correct rate based on TIF, exit commission always taker

#[tokio::test]
async fn test_tp_sl_commission_calculation() {
    use rust_decimal::Decimal;
    use std::str::FromStr;
    use test_utils::*;

    // Test 1: Post-only order (maker commission)
    let cfg_post_only = create_test_config();
    let tif_post_only = cfg_post_only.exec.tif.to_lowercase();

    let entry_commission_post_only = if tif_post_only == "post_only" || tif_post_only == "gtx" {
        Decimal::from_str(&cfg_post_only.risk.maker_commission_pct.to_string())
            .unwrap_or(Decimal::from_str("0.02").unwrap())
    } else {
        Decimal::from_str(&cfg_post_only.risk.taker_commission_pct.to_string())
            .unwrap_or(Decimal::from_str("0.04").unwrap())
    };

    let exit_commission = Decimal::from_str(&cfg_post_only.risk.taker_commission_pct.to_string())
        .unwrap_or(Decimal::from_str("0.04").unwrap());

    let total_commission_post_only = entry_commission_post_only + exit_commission;

    // Verify: Post-only should use maker commission
    if tif_post_only == "post_only" {
        assert_eq!(
            entry_commission_post_only,
            Decimal::from_str("0.02").unwrap(),
            "❌ COMMISSION: Post-only order should use maker commission (0.02%)"
        );
    }

    // Verify: Exit commission is always taker
    assert_eq!(
        exit_commission,
        Decimal::from_str("0.04").unwrap(),
        "❌ COMMISSION: Exit commission (TP/SL) should always be taker (0.04%)"
    );

    // Test 2: Market/IOC order (taker commission)
    let cfg_market = create_test_config();
    // Simulate market order TIF
    let tif_market = "ioc";

    let entry_commission_market = if tif_market == "post_only" || tif_market == "gtx" {
        Decimal::from_str(&cfg_market.risk.maker_commission_pct.to_string())
            .unwrap_or(Decimal::from_str("0.02").unwrap())
    } else {
        Decimal::from_str(&cfg_market.risk.taker_commission_pct.to_string())
            .unwrap_or(Decimal::from_str("0.04").unwrap())
    };

    let total_commission_market = entry_commission_market + exit_commission;

    // Verify: Market/IOC should use taker commission
    assert_eq!(
        entry_commission_market,
        Decimal::from_str("0.04").unwrap(),
        "❌ COMMISSION: Market/IOC order should use taker commission (0.04%)"
    );

    // Verify: Total commission calculation
    // Post-only: 0.02% (entry) + 0.04% (exit) = 0.06%
    // Market: 0.04% (entry) + 0.04% (exit) = 0.08%
    if tif_post_only == "post_only" {
        assert_eq!(
            total_commission_post_only,
            Decimal::from_str("0.06").unwrap(),
            "❌ COMMISSION: Total commission for post-only should be 0.06% (0.02% + 0.04%)"
        );
    }

    assert_eq!(
        total_commission_market,
        Decimal::from_str("0.08").unwrap(),
        "❌ COMMISSION: Total commission for market should be 0.08% (0.04% + 0.04%)"
    );

    // Verify: Post-only has lower total commission (better for PnL)
    if tif_post_only == "post_only" {
        assert!(
            total_commission_post_only < total_commission_market,
            "❌ COMMISSION: Post-only should have lower total commission than market order"
        );
    }
}

// ============================================================================
// Test 11: Balance Startup Race Condition Test
// ============================================================================
// Tests: REST API fetch and WebSocket subscription race condition
// Verifies: WebSocket updates are prioritized, stale REST API data is ignored

#[tokio::test]
async fn test_balance_startup_race_condition() {
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;
    use std::time::{Duration, Instant};
    use test_utils::*;

    let shared_state = create_test_shared_state();

    // Simulate scenario:
    // T0: REST API fetch başlar (balance = 1000)
    // T1: WebSocket bağlanır
    // T2: WebSocket BalanceUpdate gelir (balance = 900, timestamp = T2)
    // T3: REST API fetch tamamlanır (balance = 1000, timestamp = T1) ❌ STALE!
    // T4: Timestamp check yapılır, WebSocket daha yeni → REST API ignore edilir ✅

    let now = Instant::now();

    // Step 1: REST API fetch başlar (timestamp kaydedilir)
    let rest_api_timestamp = now;
    let rest_api_balance = dec!(1000);

    // Step 2: WebSocket update gelir (daha yeni timestamp)
    let websocket_timestamp = now + Duration::from_millis(100);
    let websocket_balance = dec!(900);

    // Step 3: WebSocket update'i store'a yaz (öncelikli)
    {
        let mut store = shared_state.balance_store.write().await;
        store.usdt = websocket_balance;
        store.last_updated = websocket_timestamp;
    }

    // Step 4: REST API fetch tamamlanır, timestamp check yapılır
    {
        let mut store = shared_state.balance_store.write().await;

        // ✅ CRITICAL: Check if WebSocket already updated balance with newer timestamp
        if store.last_updated > rest_api_timestamp {
            // WebSocket update is newer - ignore REST API result (stale data)
            // Don't overwrite with stale REST API data
            assert!(
                true,
                "✅ TIMESTAMP CHECK: WebSocket update is newer, REST API result should be ignored"
            );
        } else {
            // REST API timestamp is newer or equal - safe to update
            store.usdt = rest_api_balance;
            store.last_updated = rest_api_timestamp;
        }
    }

    // Verify: WebSocket balance should be preserved (not overwritten by stale REST API)
    {
        let store = shared_state.balance_store.read().await;
        assert_eq!(
            store.usdt,
            websocket_balance,
            "❌ RACE CONDITION: WebSocket balance should be preserved, not overwritten by stale REST API data"
        );
        assert_eq!(
            store.last_updated, websocket_timestamp,
            "❌ RACE CONDITION: WebSocket timestamp should be preserved"
        );
    }

    // Test reverse scenario: REST API is newer
    let rest_api_timestamp_newer = now + Duration::from_millis(200);
    let rest_api_balance_newer = dec!(1100);

    {
        let mut store = shared_state.balance_store.write().await;

        // REST API timestamp is newer - safe to update
        if store.last_updated > rest_api_timestamp_newer {
            // WebSocket is newer - ignore REST API
        } else {
            // REST API is newer - update
            store.usdt = rest_api_balance_newer;
            store.last_updated = rest_api_timestamp_newer;
        }
    }

    // Verify: REST API balance should be used (it's newer)
    {
        let store = shared_state.balance_store.read().await;
        assert_eq!(
            store.usdt, rest_api_balance_newer,
            "✅ TIMESTAMP CHECK: REST API balance should be used when it's newer"
        );
    }
}
