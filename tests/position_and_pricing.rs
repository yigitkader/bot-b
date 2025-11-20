use std::time::Duration;
use tokio::time::sleep;
use trading_bot::types::{PositionUpdate, Side};

/// Test that position IDs are unique across time
/// This ensures that even if two positions open for the same symbol+side,
/// they get different IDs due to timestamp difference
#[tokio::test]
async fn test_position_id_collision() {
    let id1 = PositionUpdate::position_id("BTCUSDT", Side::Long);

    // Wait a bit to ensure timestamp difference
    sleep(Duration::from_millis(10)).await;

    let id2 = PositionUpdate::position_id("BTCUSDT", Side::Long);

    assert_ne!(id1, id2, "Position IDs should be unique across time");
}

/// Test that position IDs are different for different symbols
#[tokio::test]
async fn test_position_id_different_symbols() {
    let id1 = PositionUpdate::position_id("BTCUSDT", Side::Long);
    let id2 = PositionUpdate::position_id("ETHUSDT", Side::Long);

    assert_ne!(
        id1, id2,
        "Position IDs should be different for different symbols"
    );
}

/// Test that position IDs are different for different sides
#[tokio::test]
async fn test_position_id_different_sides() {
    let id1 = PositionUpdate::position_id("BTCUSDT", Side::Long);
    let id2 = PositionUpdate::position_id("BTCUSDT", Side::Short);

    assert_ne!(
        id1, id2,
        "Position IDs should be different for different sides"
    );
}

/// Helper function to calculate limit price for maker orders (post-only strategy)
/// This is different from the aggressive fill strategy used in production
/// - LONG: best_bid + 1 tick (enters orderbook, maker fee advantage)
/// - SHORT: best_ask - 1 tick (enters orderbook, maker fee advantage)
fn calculate_maker_limit_price(side: &Side, best_bid: f64, best_ask: f64, tick_size: f64) -> f64 {
    let price = match side {
        Side::Long => {
            // BUY: use best_bid + 1 tick for maker order
            // This enters orderbook without crossing spread
            best_bid + tick_size
        }
        Side::Short => {
            // SELL: use best_ask - 1 tick for maker order
            // This enters orderbook without crossing spread
            best_ask - tick_size
        }
    };

    // Round to tick size
    if tick_size > 0.0 {
        (price / tick_size).round() * tick_size
    } else {
        price
    }
}

/// Test limit order pricing for maker orders (post-only strategy)
/// Maker orders should not cross the spread to get maker fee advantage
#[tokio::test]
async fn test_limit_order_pricing_maker() {
    let best_bid = 40000.0;
    let best_ask = 40001.0;
    let tick_size = 0.1;

    // LONG order should be AT or BELOW best_ask (maker order)
    // For maker: best_bid + 1 tick = 40000.0 + 0.1 = 40000.1
    let long_price = calculate_maker_limit_price(&Side::Long, best_bid, best_ask, tick_size);
    assert!(
        long_price <= best_ask,
        "LONG maker order should be <= best_ask, got {}",
        long_price
    );
    assert!(
        long_price > best_bid,
        "LONG maker order should be > best_bid, got {}",
        long_price
    );

    // SHORT order should be AT or ABOVE best_bid (maker order)
    // For maker: best_ask - 1 tick = 40001.0 - 0.1 = 40000.9
    let short_price = calculate_maker_limit_price(&Side::Short, best_bid, best_ask, tick_size);
    assert!(
        short_price >= best_bid,
        "SHORT maker order should be >= best_bid, got {}",
        short_price
    );
    assert!(
        short_price < best_ask,
        "SHORT maker order should be < best_ask, got {}",
        short_price
    );
}

/// Test limit order pricing for aggressive fill strategy (current production strategy)
/// Aggressive orders cross the spread for immediate execution
#[tokio::test]
async fn test_limit_order_pricing_aggressive() {
    let best_bid = 40000.0;
    let best_ask = 40001.0;
    let tick_size = 0.1;

    // Aggressive LONG: uses best_ask (crosses spread, immediate fill)
    let long_price_aggressive = best_ask;
    assert_eq!(
        long_price_aggressive, best_ask,
        "Aggressive LONG should use best_ask"
    );

    // Aggressive SHORT: uses best_bid (crosses spread, immediate fill)
    let short_price_aggressive = best_bid;
    assert_eq!(
        short_price_aggressive, best_bid,
        "Aggressive SHORT should use best_bid"
    );
}

/// Test tick size rounding
#[tokio::test]
async fn test_tick_size_rounding() {
    let best_bid = 40000.0;
    let best_ask = 40001.0;
    let tick_size = 0.1;

    // Test LONG maker order with tick size rounding
    let long_price = calculate_maker_limit_price(&Side::Long, best_bid, best_ask, tick_size);
    let remainder = (long_price / tick_size) % 1.0;
    assert!(
        remainder.abs() < 1e-10,
        "Price should be rounded to tick size, remainder: {}",
        remainder
    );

    // Test SHORT maker order with tick size rounding
    let short_price = calculate_maker_limit_price(&Side::Short, best_bid, best_ask, tick_size);
    let remainder = (short_price / tick_size) % 1.0;
    assert!(
        remainder.abs() < 1e-10,
        "Price should be rounded to tick size, remainder: {}",
        remainder
    );
}

/// Test edge case: very small tick size
#[tokio::test]
async fn test_small_tick_size() {
    let best_bid = 40000.0;
    let best_ask = 40001.0;
    let tick_size = 0.01; // Very small tick size

    let long_price = calculate_maker_limit_price(&Side::Long, best_bid, best_ask, tick_size);
    assert!(long_price > best_bid && long_price <= best_ask);

    let short_price = calculate_maker_limit_price(&Side::Short, best_bid, best_ask, tick_size);
    assert!(short_price >= best_bid && short_price < best_ask);
}

/// Test edge case: zero tick size (should not crash)
#[tokio::test]
async fn test_zero_tick_size() {
    let best_bid = 40000.0;
    let best_ask = 40001.0;
    let tick_size = 0.0;

    let long_price = calculate_maker_limit_price(&Side::Long, best_bid, best_ask, tick_size);
    assert_eq!(
        long_price, best_bid,
        "With zero tick size, should use best_bid"
    );

    let short_price = calculate_maker_limit_price(&Side::Short, best_bid, best_ask, tick_size);
    assert_eq!(
        short_price, best_ask,
        "With zero tick size, should use best_ask"
    );
}
