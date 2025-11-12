# Bot-B Trading Bot - Code Structure Guide

## Project Overview

Bot-B is a high-frequency trading bot for Binance Futures using market-making strategies. The codebase is organized into modular, well-separated concerns following Rust best practices.

## Architecture

### Module Organization

```
crates/app/src/
├── main.rs                 # Entry point & main trading loop
├── lib.rs                  # Module exports
├── app_init.rs             # Application initialization
├── config.rs               # Configuration structures & loading
├── types.rs                # Core data types
├── constants.rs            # Application constants
├── exchange.rs             # Binance Futures API client
├── exec.rs                 # Execution trait & venue interface
├── processor.rs            # Symbol processing & quote generation
├── strategy.rs             # Trading strategies (DynamicMM, DotMM, QMel)
├── order.rs                # Order management
├── position_manager.rs     # Position lifecycle management
├── risk.rs                 # Risk management & limits
├── monitor.rs              # Monitoring & metrics
├── qmel.rs                 # Quantile-MEL strategy implementation
├── logger.rs               # Event logging (JSON)
└── utils.rs                # Utility functions & helpers
```

## Core Components

### 1. **main.rs** - Trading Loop
**Responsibilities:**
- Initialize application and trading loop
- Handle WebSocket events (order fills, cancellations)
- Process symbols with fair round-robin scheduling
- Manage graceful shutdown

**Key Features:**
- Event-driven WebSocket processing
- Priority scheduling (open orders/positions first, then round-robin)
- Rate-limited symbol processing
- Proper resource cleanup on shutdown

### 2. **app_init.rs** - Initialization
**Responsibilities:**
- Load configuration from YAML
- Initialize Binance Futures client
- Setup WebSocket connections
- Discover and initialize trading symbols
- Create application state

**Key Functions:**
- `initialize_app()` - Main initialization entry point
- `build_strategy_config()` - Build strategy configuration
- `initialize_venue()` - Setup exchange client
- `initialize_symbols()` - Discover symbols and create state

### 3. **config.rs** - Configuration Management
**Responsibilities:**
- Define configuration structures
- Load from `config.yaml`
- Validate configuration parameters

**Key Structures:**
- `AppCfg` - Main application configuration
- `RiskCfg` - Risk management parameters
- `StratCfg` - Strategy parameters
- `ExecCfg` - Execution parameters

### 4. **types.rs** - Data Structures
**Responsibilities:**
- Define core domain types
- Order book representation
- Position tracking
- Symbol state

**Key Types:**
- `Px` - Price (wrapper around Decimal)
- `Qty` - Quantity (wrapper around Decimal)
- `Side` - Buy/Sell enum
- `OrderBook` - Bid/ask snapshot
- `Position` - Position information
- `SymbolState` - Per-symbol state tracking
- `OrderInfo` - Order metadata

### 5. **exchange.rs** - Binance API Client
**Responsibilities:**
- REST API calls to Binance Futures
- WebSocket stream management
- Order placement & cancellation
- Position & balance queries
- Exchange info fetching

**Key Functions:**
- `best_prices()` - Get current bid/ask
- `place_limit_with_client_id()` - Place order with idempotency
- `cancel()` - Cancel order
- `get_position()` - Get position info
- `fetch_exchange_info()` - Get symbol rules

### 6. **processor.rs** - Symbol Processing
**Responsibilities:**
- Process each symbol in trading loop
- Generate quotes from strategy
- Adjust quotes for risk
- Place/cancel orders
- Sync orders from API

**Key Functions:**
- `process_symbol()` - Main symbol processing
- `generate_quotes()` - Create bid/ask quotes
- `adjust_quotes_for_risk()` - Risk-based adjustments

### 7. **strategy.rs** - Trading Strategies
**Responsibilities:**
- Implement market-making strategies
- DynamicMM - Dynamic spread based on parameters
- DotMM - Enhanced MM with advanced parameters
- QMel - Quantile-MEL strategy

**Key Trait:**
- `Strategy` trait for pluggable strategies

### 8. **order.rs** - Order Management
**Responsibilities:**
- Place orders with profit guarantee
- Sync orders from API
- Analyze order fills
- Cancel orders

**Key Functions:**
- `place_side_orders()` - Place bid/ask orders
- `sync_orders_from_api()` - Refresh order state
- `analyze_orders()` - Analyze order metrics

### 9. **position_manager.rs** - Position Management
**Responsibilities:**
- Open and close positions
- Track position entry/exit
- Manage position hold duration
- Handle position closing

**Key Functions:**
- `close_position()` - Close entire position
- `handle_partial_close()` - Partial position closing

### 10. **risk.rs** - Risk Management
**Responsibilities:**
- Check position size limits
- Calculate drawdown
- Manage leverage & liquidation gap
- Handle risk alerts

**Key Functions:**
- `check_position_size_risk()` - Validate position size
- `check_pnl_alerts()` - Check PnL thresholds
- `calculate_caps()` - Calculate capacity caps

### 11. **utils.rs** - Utility Functions
**Responsibilities:**
- Quantization helpers (floor/ceil to step)
- Profit guarantee calculations
- Fill rate decay
- Market data fetching
- Rate limiting

**Key Functions:**
- `quant_utils_floor_to_step()` - Quantize to lot step
- `apply_fill_rate_decay()` - Decay fill rate over time
- `fetch_market_data()` - Get market snapshot
- `rate_limit_guard()` - API rate limiting

### 12. **monitor.rs** - Metrics & Monitoring
**Responsibilities:**
- Expose Prometheus metrics
- Track trading statistics
- Monitor bot health

### 13. **logger.rs** - Event Logging
**Responsibilities:**
- Log trading events to JSON
- Track order fills, cancellations, positions

## Data Flow

### Trading Loop Flow
```
1. Initialize (app_init.rs)
   ├─ Load config
   ├─ Setup Binance client
   ├─ Discover symbols
   └─ Create WebSocket stream

2. Main Loop (main.rs)
   ├─ Process WebSocket events
   ├─ Fetch quote balances
   ├─ For each symbol (prioritized):
   │  ├─ Sync orders (order.rs)
   │  ├─ Fetch market data (utils.rs)
   │  ├─ Generate quotes (processor.rs)
   │  ├─ Adjust for risk (utils.rs)
   │  ├─ Validate quotes
   │  ├─ Place orders (order.rs)
   │  └─ Update state (types.rs)
   └─ Repeat every tick_ms

3. Shutdown
   ├─ Close all positions (position_manager.rs)
   ├─ Cancel all orders (order.rs)
   ├─ Clean up resources
   └─ Exit gracefully
```

### WebSocket Event Processing
```
UserEvent::Heartbeat
└─ Re-sync all symbols with API

UserEvent::OrderFill
├─ Update order state
├─ Update position
├─ Log event (logger.rs)
└─ Update inventory

UserEvent::OrderCanceled
├─ Remove from active orders
└─ Log event
```

## Key Design Patterns

### 1. **Trait-Based Abstraction**
- `Venue` trait for exchange interface (enables mocking)
- `Strategy` trait for pluggable strategies

### 2. **Type Wrappers**
- `Px` and `Qty` for type safety
- Prevents mixing prices with quantities

### 3. **Decimal Arithmetic**
- Uses `rust_decimal` for precision
- Avoids floating-point errors

### 4. **Per-Symbol State**
- `SymbolState` encapsulates all symbol data
- Easy to manage independent symbols
- Clear ownership semantics

### 5. **Async/Await with Tokio**
- Concurrent symbol processing
- Non-blocking WebSocket handling
- Timeout protection for cleanup

### 6. **Atomic Synchronization**
- `Arc<AtomicBool>` for lock-free flags
- Statistics with `AtomicU64`

## Error Handling

### Strategy
1. **Result<T> for fallible operations**
   - All API calls return `Result`
   - Propagate with `?` operator

2. **Error Context with anyhow**
   - Descriptive error messages
   - Context chain for debugging

3. **Graceful Degradation**
   - Skip symbol on error, continue with others
   - Timeout protection for cleanup

## Performance Optimizations

### 1. **Rate Limiting**
- Per-symbol API calls limited
- Respects Binance rate limits
- Prevents API bans

### 2. **Symbol Prioritization**
- Symbols with open orders/positions processed first
- Fair round-robin for others
- Reduces latency for active positions

### 3. **Early Exit**
- Skip symbols with no balance and no orders
- Avoid unnecessary processing
- Reduce CPU usage

### 4. **Batch Operations**
- Fetch all quote balances once per tick
- Reuse across symbol processing
- Minimize API calls

### 5. **Lazy Initialization**
- Symbol index built on demand
- DashMap for concurrent access
- Minimal startup overhead

## Testing Approach

### Unit Tests
- Quantization helpers (floor/ceil)
- Price/quantity calculations
- Risk calculations

### Integration Tests
- Symbol processing with mock exchange
- Order lifecycle (place → fill → close)
- Position management

### Stress Tests
- Concurrent symbol processing
- High-frequency updates
- WebSocket reconnection handling

## Concurrency Safety

### Data Sharing Patterns
1. **SymbolState Vector** - Mutable only in main thread during loop
2. **DashMap for Caches** - Concurrent read/write (hash conflicts)
3. **Atomic Flags** - Lock-free position closing coordination
4. **Channels** - WebSocket events with MPSC

### Memory Ordering
- `Ordering::Relaxed` for statistics (atomicity sufficient)
- `Ordering::Acquire/Release` for position_closing flag

## Future Improvements

### 1. **Modularization**
```rust
mod core {
    pub mod types;
    pub mod config;
    pub mod constants;
}

mod exchange {
    pub mod binance;
    pub mod traits;
}

mod trading {
    pub mod strategy;
    pub mod processor;
    pub mod order;
    pub mod position;
}

mod risk {
    pub mod limits;
    pub mod management;
}

mod infra {
    pub mod logging;
    pub mod metrics;
    pub mod errors;
}
```

### 2. **Event Bus Pattern**
- Replace direct function calls with events
- Better separation of concerns
- Easier to add new event handlers

### 3. **Circuit Breaker for API**
- Track API failures
- Auto-disable symbol on repeated failures
- Retry with exponential backoff

### 4. **Backpressure Mechanism**
- Limit queued symbols
- Drop lowest-priority symbols if overloaded
- Prevent memory exhaustion

### 5. **Plugin System**
- Allow custom strategies
- Custom risk managers
- Custom loggers

## Deployment Checklist

- [ ] Review config.yaml parameters
- [ ] Set API keys in environment
- [ ] Test on testnet first
- [ ] Monitor logs for errors
- [ ] Set up alerts for high drawdowns
- [ ] Regular backups of trading data
- [ ] Review risk parameters weekly

## Common Tasks

### Add New Strategy
1. Implement `Strategy` trait
2. Register in `strategy.rs`
3. Add to config options
4. Test with mock exchange

### Monitor Trading Activity
1. Check JSON logs in `logs/trading_events.json`
2. View Prometheus metrics on configured port
3. Monitor process with `ps`, `top`, or systemd

### Adjust Risk Parameters
1. Edit `config.yaml`
2. Modify `risk` section
3. Reload (requires restart)

### Debug Symbol Issues
1. Enable debug logging: `RUST_LOG=debug`
2. Check symbol state in logs
3. Verify balance and leverage
4. Check exchange info for symbol rules

---

**Last Updated:** 2025-11-12
**Status:** Production Ready
**Warnings:** 0
**Test Coverage:** Core modules tested

