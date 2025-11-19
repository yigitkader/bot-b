# Trading Bot Architecture

## 📐 Mimari Genel Bakış

Bu proje **event-driven, modüler mimari** kullanır. Her modül bağımsız çalışır ve `EventBus` üzerinden iletişim kurar.

## 🏗️ Modül Yapısı

### Core Modules (Temel Modüller)

1. **`config.rs`** - Konfigürasyon yönetimi
   - Tüm ayarlar `config.yaml`'dan yüklenir
   - Default değerler tanımlı
   - Validation logic içerir

2. **`types.rs`** - Ortak veri yapıları
   - `MarketTick`, `TradeSignal`, `Position`, `Order` vb.
   - Tüm modüller bu tipleri kullanır

3. **`event_bus.rs`** - Event dispatching sistemi
   - Broadcast channels kullanır
   - Modüller arası iletişim merkezi
   - `MarketTick`, `TradeSignal`, `OrderUpdate` vb. event'ler

4. **`state.rs`** - Shared state yönetimi
   - `SharedState`: Ordering state, balance store
   - Thread-safe (Arc + RwLock/Mutex)

### Trading Modules (Trading Modülleri)

5. **`connection/`** - Binance API entegrasyonu
   - `venue.rs`: REST API calls (rate-limited)
   - `websocket.rs`: WebSocket streams
   - Cache management (PRICE_CACHE, BALANCE_CACHE, etc.)

6. **`trending.rs`** - Trend analizi ve signal generation
   - EMA, RSI, ATR hesaplamaları
   - Signal generation logic
   - Config-driven parameters

7. **`ordering.rs`** - Order placement ve management
   - Balance reservation
   - Order validation
   - Position opening logic

8. **`follow_orders.rs`** - Position tracking ve PnL
   - Stop-loss / Take-profit logic
   - Funding cost tracking
   - PnL calculation

9. **`position_manager.rs`** - Smart position closing
   - Time-weighted thresholds
   - Trailing stop logic
   - Max loss protection

### Support Modules (Destek Modülleri)

10. **`risk.rs`** - Risk management
    - PnL alerts
    - Position size limits
    - Risk level calculation

11. **`balance.rs`** - Balance tracking
    - USDT/USDC balance monitoring
    - Balance updates via WebSocket

12. **`qmel.rs`** - Quantitative Market Execution Learning
    - OFI, Microprice, Liquidity Pressure
    - Thompson Sampling Bandit
    - Feature extraction

13. **`ai_analyzer.rs`** - Anomaly detection
    - Balance inconsistencies
    - Order rejection patterns
    - System health monitoring

14. **`logging.rs`** - JSON event logging
    - Trading events to JSON
    - Timestamp tracking

15. **`utils.rs`** - Utility functions
    - Rate limiting (weight-based)
    - Decimal conversions
    - Spread calculations

## 🔄 Data Flow

```
Binance API (WebSocket/REST)
    ↓
Connection Module
    ↓
EventBus (MarketTick, OrderUpdate, PositionUpdate)
    ↓
┌─────────────┬──────────────┬──────────────┬─────────────┐
│  Trending   │   Ordering   │ FollowOrders │   Balance   │
│  (Signals)  │  (Placement) │  (Tracking)  │ (Tracking)  │
└─────────────┴──────────────┴──────────────┴─────────────┘
    ↓              ↓              ↓              ↓
         EventBus (TradeSignal, CloseRequest)
    ↓              ↓              ↓              ↓
         Ordering → Connection → Binance API
```

## 📋 Modül Bağımlılıkları

```
main.rs
├── config (no deps)
├── types (no deps)
├── event_bus (types)
├── state (types)
├── connection (config, types, event_bus, state, utils)
├── trending (config, types, event_bus, utils)
├── ordering (config, types, event_bus, state, connection, risk, utils)
├── follow_orders (config, types, event_bus, connection, risk, position_manager, utils)
├── balance (connection, event_bus, state)
├── risk (config, types)
├── position_manager (types, utils)
├── qmel (types)
├── ai_analyzer (types, event_bus)
├── logging (types, event_bus)
└── utils (types)
```

## 🎯 Tasarım Prensipleri

1. **Separation of Concerns**: Her modül tek bir sorumluluğa sahip
2. **Event-Driven**: Modüller EventBus üzerinden iletişim kurar
3. **Config-Driven**: Tüm parametreler config'den gelir (hardcoded yok)
4. **Thread-Safe**: Arc + RwLock/Mutex kullanımı
5. **Error Handling**: `Result<T>` pattern, `anyhow::Error`
6. **No Mock Data**: Production'da gerçek API data kullanılır

## 🔍 Modül İnceleme Rehberi

Her modülü incelerken şu soruları sor:

1. **Ne yapıyor?** - Modülün amacı nedir?
2. **Nasıl çalışıyor?** - İç mekanizma nasıl?
3. **Hangi event'leri dinliyor/gönderiyor?** - EventBus kullanımı
4. **Hangi config değerlerini kullanıyor?** - Config bağımlılıkları
5. **Hangi modüllere bağımlı?** - Dependency graph
6. **Test coverage nedir?** - Test dosyaları

## 📝 Kod Standartları

### Naming Conventions
- **Structs**: `PascalCase` (örn: `MarketTick`, `TradeSignal`)
- **Functions**: `snake_case` (örn: `calculate_pnl`, `should_close_position`)
- **Constants**: `UPPER_SNAKE_CASE` (örn: `MAX_POSITION_DURATION_SEC`)
- **Modules**: `snake_case` (örn: `follow_orders`, `position_manager`)

### Error Handling
- `Result<T>` kullan, `unwrap()` kullanma
- `anyhow::Error` için context ekle
- Fallback değerler config'den gelmeli

### Documentation
- Her public function için doc comment
- Complex logic için inline comments
- Module-level documentation

## 🧪 Test Stratejisi

1. **Unit Tests**: Her modül için `#[cfg(test)]` modülü
2. **Integration Tests**: `tests/backtest.rs` - gerçek API data ile
3. **Compile Tests**: `tests/compile_test.rs` - type checking

## 🚀 Yeni Modül Ekleme

1. `src/` altında yeni dosya oluştur
2. `src/lib.rs`'a modül ekle
3. `src/main.rs`'a import ekle
4. EventBus subscription'ları ekle
5. Config yapısını güncelle
6. Test ekle
7. Dokümantasyon ekle

