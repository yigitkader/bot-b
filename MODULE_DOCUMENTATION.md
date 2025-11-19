# Modül Dokümantasyonu

Her modül için standart dokümantasyon formatı.

## 📋 Dokümantasyon Şablonu

Her modül dosyasının başına şu formatı kullan:

```rust
//! # Module Name
//!
//! ## Purpose
//! Kısa açıklama: Bu modül ne yapar?
//!
//! ## Responsibilities
//! - Responsibility 1
//! - Responsibility 2
//! - Responsibility 3
//!
//! ## Dependencies
//! - `config`: Config değerleri
//! - `types`: Veri yapıları
//! - `event_bus`: Event dispatching
//!
//! ## Events
//! ### Subscribes to:
//! - `MarketTick`: Fiyat güncellemeleri
//! - `OrderUpdate`: Order durumu
//!
//! ### Publishes:
//! - `TradeSignal`: Trading sinyalleri
//! - `CloseRequest`: Position kapatma istekleri
//!
//! ## Config Dependencies
//! - `trending.min_spread_bps`: Minimum spread
//! - `trending.signal_cooldown_seconds`: Signal cooldown
//!
//! ## Examples
//! ```rust
//! let trending = Trending::new(cfg, event_bus, shutdown_flag);
//! trending.start().await?;
//! ```

```

## 📝 Modül Listesi

### Core Modules

#### `config.rs`
- **Purpose**: Konfigürasyon yönetimi ve validation
- **Key Functions**: `load_config()`, `validate_config()`
- **Config File**: `config.yaml`

#### `types.rs`
- **Purpose**: Ortak veri yapıları
- **Key Types**: `MarketTick`, `TradeSignal`, `Position`, `Order`
- **Type Aliases**: `Px`, `Qty`, `Side`

#### `event_bus.rs`
- **Purpose**: Event dispatching sistemi
- **Channels**: `market_tick_tx`, `trade_signal_tx`, `close_request_tx`
- **Pattern**: Broadcast channels

### Trading Modules

#### `connection/`
- **Purpose**: Binance API entegrasyonu
- **Submodules**: `venue.rs` (REST), `websocket.rs` (WebSocket)
- **Caches**: `PRICE_CACHE`, `BALANCE_CACHE`, `POSITION_CACHE`
- **Rate Limiting**: Weight-based (40 req/sec, 2400 weight/min)

#### `trending.rs`
- **Purpose**: Trend analizi ve signal generation
- **Indicators**: EMA (9, 21, 55), RSI (14), ATR
- **Output**: `TradeSignal::Long` / `TradeSignal::Short`

#### `ordering.rs`
- **Purpose**: Order placement ve management
- **Features**: Balance reservation, order validation
- **Dependencies**: `connection`, `state`, `risk`

#### `follow_orders.rs`
- **Purpose**: Position tracking ve PnL
- **Features**: Stop-loss, take-profit, funding cost tracking
- **Dependencies**: `position_manager`, `risk`

#### `position_manager.rs`
- **Purpose**: Smart position closing logic
- **Features**: Time-weighted thresholds, trailing stop
- **Config**: `exec.max_position_duration_sec`, `exec.trailing_stop_threshold_ratio`

### Support Modules

#### `risk.rs`
- **Purpose**: Risk management
- **Features**: PnL alerts, position size limits
- **Functions**: `check_pnl_alerts()`, `check_position_size_risk()`

#### `qmel.rs`
- **Purpose**: Quantitative Market Execution Learning
- **Features**: OFI, Microprice, Liquidity Pressure, Thompson Sampling
- **Key Structs**: `FeatureExtractor`, `ThompsonSamplingBandit`

#### `utils.rs`
- **Purpose**: Utility functions
- **Features**: Rate limiting, decimal conversions, spread calculations
- **Key Functions**: `rate_limit_guard()`, `calculate_spread_bps()`

## 🔄 Modül İletişim Diyagramı

```
┌─────────────┐
│   Config    │ (no dependencies)
└─────────────┘
       ↓
┌─────────────┐
│    Types    │ (no dependencies)
└─────────────┘
       ↓
┌─────────────┐      ┌─────────────┐
│  EventBus   │ ←─── │   State     │
└─────────────┘      └─────────────┘
       ↓
┌─────────────┐
│ Connection  │ → Binance API
└─────────────┘
       ↓
┌─────────────┐      ┌─────────────┐
│  Trending   │      │  Ordering   │
└─────────────┘      └─────────────┘
       ↓                    ↓
┌─────────────┐      ┌─────────────┐
│FollowOrders │      │   Balance   │
└─────────────┘      └─────────────┘
```

## 📚 Dokümantasyon Güncelleme

Yeni modül eklendiğinde veya mevcut modül değiştirildiğinde:

1. Bu dosyayı güncelle
2. Modül dosyasının başına doc comment ekle
3. `ARCHITECTURE.md`'yi güncelle
4. `CODE_REVIEW_CHECKLIST.md`'yi kontrol et

