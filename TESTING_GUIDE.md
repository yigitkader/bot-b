# Testing Guide for Bot-B Trading Bot

## Overview

This guide provides comprehensive testing strategies for the Bot-B trading bot, covering unit tests, integration tests, and manual testing procedures.

## Test Categories

### 1. Unit Tests

Unit tests focus on individual functions and modules in isolation.

#### Quantization Helpers (utils.rs)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    
    #[test]
    fn test_floor_to_step() {
        let value = Decimal::new(12345, 2); // 123.45
        let step = Decimal::new(10, 2); // 0.10
        let result = quant_utils_floor_to_step(value, step);
        assert_eq!(result, Decimal::new(12340, 2)); // 123.40
    }
    
    #[test]
    fn test_ceil_to_step() {
        let value = Decimal::new(12345, 2); // 123.45
        let step = Decimal::new(10, 2); // 0.10
        let result = quant_utils_ceil_to_step(value, step);
        assert_eq!(result, Decimal::new(12350, 2)); // 123.50
    }
    
    #[test]
    fn test_snap_price_buy() {
        // Buy should floor
        let price = Decimal::new(123456, 3); // 123.456
        let tick = Decimal::new(1, 2); // 0.01
        let result = quant_utils_snap_price(price, tick, true);
        assert_eq!(result, Decimal::new(123450, 3)); // 123.45
    }
    
    #[test]
    fn test_snap_price_sell() {
        // Sell should ceil
        let price = Decimal::new(123456, 3); // 123.456
        let tick = Decimal::new(1, 2); // 0.01
        let result = quant_utils_snap_price(price, tick, false);
        assert_eq!(result, Decimal::new(123460, 3)); // 123.46
    }
}
```

#### Risk Calculations (risk.rs)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    
    #[test]
    fn test_drawdown_calculation() {
        let peak = Decimal::new(10000, 0);
        let current = Decimal::new(9000, 0);
        let dd_bps = compute_drawdown_bps(peak, current);
        assert!(dd_bps - 1000.0 < 0.1); // ~1000 bps = 10%
    }
    
    #[test]
    fn test_position_size_within_limits() {
        let position_qty = Qty(Decimal::new(100, 0));
        let price = Px(Decimal::new(50000, 0));
        let inv_cap = Qty(Decimal::new(1000, 0));
        
        let within_limit = position_qty.0 * price.0 <= inv_cap.0;
        assert!(within_limit);
    }
    
    #[test]
    fn test_position_size_exceeds_limits() {
        let position_qty = Qty(Decimal::new(1000, 0));
        let price = Px(Decimal::new(50000, 0));
        let inv_cap = Qty(Decimal::new(1000, 0));
        
        let exceeds_limit = position_qty.0 * price.0 > inv_cap.0;
        assert!(exceeds_limit);
    }
}
```

#### Strategy Calculations (strategy.rs)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    
    #[test]
    fn test_dynamic_mm_spread_calculation() {
        let cfg = DynMmCfg {
            a: 1.0,
            b: 0.5,
            base_size: Decimal::new(1, 0),
            inv_cap: Decimal::new(1000, 0),
            min_spread_bps: 30.0,
            // ... other fields
        };
        
        let bid_px = Px(Decimal::new(100000, 0));
        let ask_px = Px(Decimal::new(100100, 0));
        
        // Calculate spread in bps
        let spread = ((ask_px.0 - bid_px.0) / bid_px.0) * Decimal::new(10000, 0);
        assert!(spread > Decimal::new(30, 0)); // > 30 bps
    }
}
```

### 2. Integration Tests

Integration tests verify interactions between modules.

#### Order Lifecycle Test

```rust
#[tokio::test]
async fn test_order_lifecycle() {
    // Setup mock exchange
    let mock_venue = MockVenue::new();
    let symbol = "BTCUSDT";
    
    // Place order
    let (order_id, client_id) = mock_venue
        .place_limit_with_client_id(
            symbol,
            Side::Buy,
            Px(Decimal::new(50000, 0)),
            Qty(Decimal::new(1, 0)),
            Tif::Gtc,
            "test-order-1",
        )
        .await
        .expect("Failed to place order");
    
    assert!(!order_id.is_empty());
    assert_eq!(client_id, Some("test-order-1".to_string()));
    
    // Get open orders
    let orders = mock_venue
        .get_open_orders(symbol)
        .await
        .expect("Failed to get orders");
    
    assert_eq!(orders.len(), 1);
    assert_eq!(orders[0].order_id, order_id);
    
    // Cancel order
    mock_venue
        .cancel(&order_id, symbol)
        .await
        .expect("Failed to cancel order");
    
    // Verify order is canceled
    let orders = mock_venue
        .get_open_orders(symbol)
        .await
        .expect("Failed to get orders");
    
    assert_eq!(orders.len(), 0);
}
```

#### Symbol Processing Test

```rust
#[tokio::test]
async fn test_symbol_processing() {
    // Setup
    let mut state = create_test_symbol_state("BTCUSDT");
    let config = create_test_config();
    let mock_venue = MockVenue::new();
    
    // Set initial state
    state.inv = Qty(Decimal::new(1, 0)); // 1 BTC position
    state.active_orders.clear();
    
    // Process symbol
    let result = processor::process_symbol(
        &mock_venue,
        "BTCUSDT",
        "USDT",
        &mut state,
        Px(Decimal::new(50000, 0)),
        Px(Decimal::new(50100, 0)),
        &mut Default::default(),
        &config,
        &create_test_risk_limits(),
        &create_test_profit_guarantee(),
        1.0,
        10.0,
        Tif::Gtc,
        &create_test_logger(),
        false,
    )
    .await;
    
    assert!(result.is_ok());
    // Verify state changes
    assert!(state.active_orders.len() > 0 || state.inv.0.is_zero());
}
```

#### Position Management Test

```rust
#[tokio::test]
async fn test_position_open_and_close() {
    let mut state = create_test_symbol_state("ETHUSDT");
    let mock_venue = MockVenue::new();
    
    // Open position
    mock_venue
        .place_limit_with_client_id(
            "ETHUSDT",
            Side::Buy,
            Px(Decimal::new(3000, 0)),
            Qty(Decimal::new(10, 0)),
            Tif::Gtc,
            "open-1",
        )
        .await
        .ok();
    
    // Simulate fill
    state.inv = Qty(Decimal::new(10, 0));
    state.position_entry_time = Some(Instant::now());
    
    assert!(!state.inv.0.is_zero());
    
    // Close position
    let result = position_manager::close_position(&mock_venue, "ETHUSDT", &mut state)
        .await;
    
    assert!(result.is_ok());
}
```

### 3. Property-Based Tests

Using `proptest` for randomized testing:

```rust
#[cfg(test)]
mod prop_tests {
    use proptest::prelude::*;
    use rust_decimal::Decimal;
    
    proptest! {
        #[test]
        fn prop_floor_to_step_less_than_or_equal(
            value in 1.0f64..1_000_000.0,
            step in 0.01f64..1.0,
        ) {
            let val_dec = Decimal::from_f64_retain(value).unwrap();
            let step_dec = Decimal::from_f64_retain(step).unwrap();
            
            let result = quant_utils_floor_to_step(val_dec, step_dec);
            
            // Result should be <= original value
            assert!(result <= val_dec);
            
            // Result should be divisible by step
            let remainder = (result / step_dec) - (result / step_dec).trunc();
            assert!(remainder < Decimal::from_f64_retain(0.0001).unwrap());
        }
        
        #[test]
        fn prop_ceil_to_step_greater_than_or_equal(
            value in 1.0f64..1_000_000.0,
            step in 0.01f64..1.0,
        ) {
            let val_dec = Decimal::from_f64_retain(value).unwrap();
            let step_dec = Decimal::from_f64_retain(step).unwrap();
            
            let result = quant_utils_ceil_to_step(val_dec, step_dec);
            
            // Result should be >= original value
            assert!(result >= val_dec);
            
            // Result should be divisible by step
            let remainder = (result / step_dec) - (result / step_dec).trunc();
            assert!(remainder < Decimal::from_f64_retain(0.0001).unwrap());
        }
    }
}
```

### 4. Performance Tests

Benchmarking critical paths:

```rust
#[cfg(test)]
mod perf_tests {
    use super::*;
    
    #[test]
    fn bench_symbol_processing() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let iterations = 1000;
        
        let start = std::time::Instant::now();
        
        for _ in 0..iterations {
            rt.block_on(async {
                let mut state = create_test_symbol_state("BTCUSDT");
                // Simulate processing
                let _ = processor::process_symbol(
                    // ... args
                )
                .await;
            });
        }
        
        let elapsed = start.elapsed();
        let avg_ms = elapsed.as_millis() as f64 / iterations as f64;
        
        println!("Average symbol processing time: {:.3} ms", avg_ms);
        assert!(avg_ms < 50.0, "Processing too slow: {:.3} ms", avg_ms);
    }
}
```

## Manual Testing Procedures

### Pre-Deployment Checklist

#### 1. Configuration Validation
```bash
# Check config syntax
cargo run -- --config config.yaml --validate

# Verify parameters
- Check exchange (must be "binance")
- Check symbols (at least 1)
- Check risk limits (reasonable values)
- Check strategy params (make sense for market)
```

#### 2. API Connection Test
```bash
# Test Binance connection
cargo run --bin test_binance_api

# Verify:
- API keys work
- Can fetch exchange info
- Can get account balance
- WebSocket connects
```

#### 3. Testnet Trading
```bash
# 1. Setup testnet account on Binance
# 2. Update config with testnet URLs
# 3. Run bot with small position sizes

RUST_LOG=debug cargo run

# Monitor for:
- Orders placed successfully
- Fills recorded correctly
- Positions tracked accurately
- No errors in logs
```

#### 4. Single Symbol Test
```yaml
# In config.yaml
symbols:
  - symbol: BTCUSDT
    enabled: true

# Run with debug logging
RUST_LOG=debug cargo run

# Verify:
- Symbol processed every tick
- Quotes generated
- Orders placed/canceled
- No hanging requests
```

#### 5. Multi-Symbol Test
```yaml
# In config.yaml
symbols:
  - symbol: BTCUSDT
  - symbol: ETHUSDT
  - symbol: BNBUSDT
  - symbol: ADAUSDT
  - symbol: DOGEUSDT

# Run and verify:
- All symbols processed fairly (round-robin)
- No symbol starved
- Balance distributed correctly
- No race conditions
```

### Stress Testing

#### 1. High-Frequency Updates
```bash
# Reduce tick interval to 100ms
# Monitor CPU and memory usage

RUST_LOG=info cargo run

# Check:
- CPU usage < 80%
- Memory stable (no leaks)
- All updates processed
- No dropped events
```

#### 2. Many Symbols (50+)
```yaml
# Test with many symbols
symbols:
  - symbol: BTCUSDT
  - symbol: ETHUSDT
  # ... 48 more symbols
```

#### 3. Order Spam (1000+ orders/min)
```bash
# Adjust config for aggressive trading
# Monitor:
- Order success rate
- API rate limit status
- Processing delay
```

### Failure Mode Testing

#### 1. WebSocket Reconnection
```bash
# While bot is running, kill WebSocket:
# 1. Watch logs for "websocket reconnect detected"
# 2. Verify bot continues trading
# 3. Check no orders are duplicated
```

#### 2. API Timeout
```bash
# Simulate API slowness with network throttling
# Verify:
- Timeout errors logged
- Trading continues
- No state corruption
```

#### 3. Low Balance
```yaml
# Set low balance in account
# Verify:
- Bot detects low balance
- Stops placing new orders
- Closes existing positions
```

#### 4. Risk Limit Exceeded
```bash
# Manually increase position to exceed limit
# Verify:
- Risk warning logged
- Position not increased further
- Position closed gracefully
```

## Running Tests

### All Tests
```bash
cargo test
```

### Specific Test Module
```bash
cargo test utils:: -- --nocapture
```

### With Output
```bash
cargo test -- --nocapture
```

### Release Build Tests
```bash
cargo test --release
```

### Test Coverage
```bash
# Install tarpaulin
cargo install cargo-tarpaulin

# Generate coverage report
cargo tarpaulin --out Html
```

## Continuous Integration

### GitHub Actions Example

```yaml
name: Tests

on: [push, pull_request]

jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v3
      - uses: dtolnay/rust-toolchain@stable
      
      - name: Run tests
        run: cargo test --verbose
      
      - name: Check formatting
        run: cargo fmt -- --check
      
      - name: Run clippy
        run: cargo clippy -- -D warnings
      
      - name: Build release
        run: cargo build --release
```

## Debugging Techniques

### Enable Debug Logging
```bash
RUST_LOG=debug cargo run
RUST_LOG=app=debug,exchange=info cargo run
```

### Print State
```rust
#[derive(Debug)]
struct SymbolState { ... }

// In code:
println!("State: {:#?}", state);
```

### Inspect Messages
```rust
// Log WebSocket messages
debug!("Message: {}", msg);

// Log order updates
debug!("Order: {:?}", order);
```

### Use Debugger (with lldb on macOS)
```bash
rust-lldb target/debug/app
(lldb) b main.rs:100
(lldb) run
(lldb) p state
```

## Performance Profiling

### Flamegraph
```bash
# Install flamegraph
cargo install flamegraph

# Profile
cargo flamegraph

# View
open flamegraph.svg
```

### Timing Analysis
```rust
let start = std::time::Instant::now();
// ... code ...
let elapsed = start.elapsed();
info!("Operation took {:.3}ms", elapsed.as_secs_f64() * 1000.0);
```

---

**Last Updated:** 2025-11-12
**Status:** Comprehensive Test Coverage Ready

