# Binance Futures Trading Bot - Detaylı İş Analizi Yapısı Planı

## 📋 İçindekiler
1. [Proje Genel Bakış](#1-proje-genel-bakış)
2. [İş Mantığı ve Amaç](#2-iş-mantığı-ve-amaç)
3. [Mimari Yapı ve Dosya Organizasyonu](#3-mimari-yapı-ve-dosya-organizasyonu)
4. [Modül Detayları ve İş Mantıkları](#4-modül-detayları-ve-iş-mantıkları)
5. [Veri Akışı ve İş Süreçleri](#5-veri-akışı-ve-iş-süreçleri)
6. [Risk Yönetimi ve Güvenlik](#6-risk-yönetimi-ve-güvenlik)
7. [Teknik Altyapı ve Performans](#7-teknik-altyapı-ve-performans)
8. [Konfigürasyon Sistemi](#8-konfigürasyon-sistemi)
9. [Hata Yönetimi ve Güvenilirlik](#9-hata-yönetimi-ve-güvenilirlik)
10. [State Yönetimi ve Senkronizasyon](#10-state-yönetimi-ve-senkronizasyon)
11. [Event Bus Sistemi](#11-event-bus-sistemi)
12. [WebSocket ve REST API Yönetimi](#12-websocket-ve-rest-api-yönetimi)
13. [Önemli Tasarım Kararları ve Kısıtlamalar](#13-önemli-tasarım-kararları-ve-kısıtlamalar)

---

## 1. Proje Genel Bakış

### 1.1 Proje Tanımı
Bu proje, **Binance Futures** borsasında otomatik kripto para ticareti yapan bir trading bot'udur. Bot, gerçek zamanlı piyasa verilerini analiz ederek, trend sinyalleri üretir ve otomatik olarak pozisyon açar/kapatır.

**Temel Felsefe**: WebSocket-first yaklaşım - Mümkün olduğunca WebSocket kullanılır, REST API sadece gerektiğinde fallback olarak kullanılır.

### 1.2 Temel Özellikler
- **Binance Futures API** entegrasyonu (WebSocket-first yaklaşım)
- **Otomatik trend analizi** ve sinyal üretimi (SMA-based multi-timeframe)
- **Tek pozisyon garantisi** (aynı anda sadece bir açık pozisyon/emir)
- **Take Profit (TP) ve Stop Loss (SL)** otomatik yönetimi (komisyon dahil net PnL)
- **Leverage yönetimi** (20x-50x desteklenir, isolated margin ile)
- **Gerçek zamanlı bakiye takibi** (USDT/USDC, rezervasyon sistemi)
- **Rate limit yönetimi** ve otomatik yeniden bağlanma
- **Event-driven mimari** (modüller arası iletişim event bus üzerinden)
- **State senkronizasyonu** (WebSocket reconnect sonrası REST API doğrulama)
- **Memory leak önleme** (cleanup task'ları)

### 1.3 Teknoloji Stack
- **Dil**: Rust (2021 edition)
- **Async Runtime**: Tokio (multi-threaded)
- **WebSocket**: tokio-tungstenite
- **HTTP Client**: reqwest (rustls-tls)
- **Concurrency**: dashmap (thread-safe HashMap), Arc, Mutex, RwLock
- **Logging**: tracing + tracing-subscriber
- **Konfigürasyon**: YAML (serde_yaml)
- **Decimal**: rust_decimal (finansal hesaplamalar için)
- **Serialization**: serde + serde_json

### 1.4 Dosya Yapısı
```
src/
├── main.rs              # Ana uygulama giriş noktası
├── config.rs            # Konfigürasyon yapıları ve validasyon
├── types.rs             # Tüm domain tipleri (event'ler, state, vb.)
├── event_bus.rs         # Event bus sistemi (broadcast channels)
├── state.rs             # Shared state (OrderingState, BalanceStore)
├── connection.rs        # Ana connection modülü (WebSocket + REST koordinasyonu)
├── connection/
│   ├── venue.rs         # Binance Futures implementasyonu (REST API)
│   └── websocket.rs     # WebSocket stream'leri (market data + user data)
├── trending.rs          # Trend analizi ve sinyal üretimi
├── ordering.rs          # Emir yönetimi (tek pozisyon garantisi)
├── follow_orders.rs     # TP/SL takibi ve pozisyon yönetimi
├── balance.rs           # Bakiye takibi (USDT/USDC)
└── logging.rs           # Event loglama (JSON format)
```

---

## 2. İş Mantığı ve Amaç

### 2.1 İş Hedefi
Bot'un temel amacı, kripto para piyasalarındaki kısa vadeli fiyat hareketlerinden kar elde etmektir. Bot:
1. Piyasa verilerini gerçek zamanlı analiz eder (WebSocket @bookTicker stream)
2. Trend sinyalleri üretir (LONG veya SHORT) - SMA-based multi-timeframe analiz
3. Otomatik olarak pozisyon açar (POST_ONLY limit orders)
4. TP/SL seviyelerine ulaşıldığında pozisyonu kapatır (MARKET reduce-only orders)

### 2.2 Ticaret Stratejisi
- **Strateji Tipi**: Trend takip (trend following) - SMA-based multi-timeframe
- **Zaman Çerçevesi**: Kısa vadeli (dakikalar/saatler)
- **Pozisyon Yönetimi**: Tek pozisyon (aynı anda sadece bir açık pozisyon/emir)
- **Risk Yönetimi**: 
  - Take Profit: %5 (varsayılan, config: `take_profit_pct`)
  - Stop Loss: %2 (varsayılan, config: `stop_loss_pct`)
  - Leverage: 20x (varsayılan, config: `leverage` veya `exec.default_leverage`)
  - Isolated margin kullanımı (pozisyon bazlı risk izolasyonu, zorunlu)
  - Komisyon dahil net PnL hesaplama (maker/taker ayrımı)

### 2.3 Trend Analizi Stratejisi
**Multi-Timeframe SMA Analizi**:
- **Short-term SMA**: 10 periyot (5-dakika eşdeğeri)
- **Medium-term SMA**: 15 periyot (15-dakika eşdeğeri)
- **Long-term SMA**: 20 periyot (1-saat eşdeğeri)
- **Trend Threshold**: %1.5 fiyat sapması (SMA'dan)
- **Konsensüs Kuralı**: En az 2/3 timeframe aynı yönde trend göstermeli
- **Volume Confirmation**: Trend yönü ile volume artışı uyumlu olmalı
- **Momentum Filtresi**: Minimum %0.5 momentum gereksinimi

**Sinyal Üretim Kriterleri**:
1. Spread kontrolü: 5-200 bps arası (config: `trending.min_spread_bps`, `trending.max_spread_bps`)
2. Cooldown period: 30 saniye (config: `trending.signal_cooldown_seconds`)
3. Position close cooldown: 5 saniye (aynı sembol için)
4. Direction-aware cooldown: Aynı yönde sinyal için ekstra bekleme
5. Bakiye kontrolü: Minimum margin gereksinimi
6. Symbol rules validation: Min notional, tick size, step size

### 2.4 İş Kuralları
1. **Tek Pozisyon Kuralı**: Aynı anda sadece bir açık pozisyon veya emir olabilir
2. **Bakiye Kontrolü**: Her emir öncesi yeterli bakiye kontrolü (rezervasyon sistemi)
3. **Minimum/Maksimum Emir Boyutu**: 
   - Minimum: 10 USD (margin, config: `min_usd_per_order`)
   - Maksimum: 100 USD (margin, config: `max_usd_per_order`)
4. **Spread Kontrolü**: 
   - Minimum spread: 5 bps (config: `trending.min_spread_bps`)
   - Maksimum spread: 200 bps (config: `trending.max_spread_bps`)
5. **Cooldown Period**: 
   - Sinyal cooldown: 30 saniye (config: `trending.signal_cooldown_seconds`)
   - Position close cooldown: 5 saniye (hardcoded)
6. **Leverage Kontrolü**: 
   - Maksimum: 50x (config: `risk.max_leverage`)
   - Startup'ta exchange leverage ile config karşılaştırma
7. **Margin Type**: Isolated margin zorunlu (cross margin desteklenmez)
8. **Hedge Mode**: Desteklenmez (one-way mode zorunlu)

---

## 3. Mimari Yapı ve Dosya Organizasyonu

### 3.1 Genel Mimari Prensibi
Bot, **event-driven, modüler mimari** kullanır. Tüm modüller birbirinden bağımsızdır ve **EventBus** üzerinden iletişim kurar. Dış dünya (Binance API) ile iletişim sadece **CONNECTION** modülü üzerinden yapılır.

**Temel Prensipler**:
- **Single Responsibility**: Her modül tek bir sorumluluğa sahip
- **Loose Coupling**: Modüller sadece event bus üzerinden iletişim kurar
- **WebSocket-first**: Mümkün olduğunca WebSocket kullanılır
- **State Isolation**: Her modül kendi state'ini yönetir (SharedState sadece kritik state için)

### 3.2 Mimari Katmanları

```
┌─────────────────────────────────────────────────────────────┐
│                    MAIN APPLICATION                         │
│  (main.rs)                                                  │
│  - Config yükleme                                           │
│  - EventBus oluşturma                                       │
│  - SharedState oluşturma                                    │
│  - Tüm modülleri başlatma                                   │
│  - Graceful shutdown yönetimi                               │
└─────────────────────────────────────────────────────────────┘
                            │
                            ▼
┌─────────────────────────────────────────────────────────────┐
│                    EVENT BUS                                │
│  (event_bus.rs)                                             │
│  - Broadcast channels (tokio::sync::broadcast)             │
│  - MarketTick events                                        │
│  - TradeSignal events                                       │
│  - CloseRequest events                                      │
│  - OrderUpdate events                                       │
│  - PositionUpdate events                                   │
│  - BalanceUpdate events                                    │
│  - OrderingStateUpdate events                              │
│  - OrderFillHistoryUpdate events                           │
└─────────────────────────────────────────────────────────────┘
                            │
        ┌───────────────────┼───────────────────┐
        ▼                   ▼                   ▼
┌──────────────┐   ┌──────────────┐   ┌──────────────┐
│  CONNECTION  │   │   TRENDING   │   │   ORDERING   │
│  (Exchange)  │   │  (Analysis)  │   │  (Execution) │
│              │   │              │   │              │
│  - WebSocket │   │  - SMA       │   │  - Single    │
│  - REST API  │   │  - Momentum │   │    Position  │
│  - Rate Limit│   │  - Volume   │   │    Guarantee │
│  - Reconnect │   │  - Spread   │   │  - Balance   │
│              │   │    Check    │   │    Reserve   │
└──────────────┘   └──────────────┘   └──────────────┘
        │                   │                   │
        ▼                   ▼                   ▼
┌──────────────┐   ┌──────────────┐   ┌──────────────┐
│   BALANCE    │   │FOLLOW_ORDERS │   │   LOGGING    │
│  (Tracking)  │   │  (TP/SL)     │   │  (Events)    │
│              │   │              │   │              │
│  - USDT/USDC │   │  - PnL Calc  │   │  - JSON Logs │
│  - Reserve   │   │  - TP/SL     │   │  - Structured│
│  - WebSocket │   │    Trigger   │   │    Logging   │
└──────────────┘   └──────────────┘   └──────────────┘
```

### 3.3 Modül Bağımlılıkları

```
CONNECTION (En düşük seviye - Exchange ile iletişim)
    │
    ├──► BALANCE (Bakiye takibi için CONNECTION.fetch_balance() kullanır)
    │
    ├──► ORDERING (Emir göndermek için CONNECTION.send_order() kullanır)
    │
    └──► TRENDING (Sembol kuralları için CONNECTION.rules_for() kullanır)

TRENDING (Trend analizi)
    │
    ├──► CONNECTION (Symbol rules için)
    │
    ├──► SHARED_STATE (Bakiye kontrolü için)
    │
    └──► ORDERING (TradeSignal event'i gönderir)

FOLLOW_ORDERS (Pozisyon takibi)
    │
    └──► ORDERING (CloseRequest event'i gönderir)

ORDERING (Emir yönetimi)
    │
    ├──► CONNECTION (Emir göndermek için)
    │
    └──► SHARED_STATE (Tek pozisyon garantisi için)

LOGGING (Loglama)
    │
    └──► Tüm modüllerden event'leri dinler
```

### 3.4 Dosya Detayları

#### main.rs
- **Görev**: Ana uygulama giriş noktası
- **İşlevler**:
  - Config yükleme (`load_config()`)
  - EventBus oluşturma
  - SharedState oluşturma
  - Tüm modülleri başlatma (sıralı)
  - Graceful shutdown yönetimi (Ctrl+C)
  - Health check task'ı
- **Başlatma Sırası**:
  1. CONNECTION (WebSocket stream'leri başlatır)
  2. BALANCE (Bakiye takibi)
  3. ORDERING (Emir yönetimi)
  4. FOLLOW_ORDERS (TP/SL takibi)
  5. TRENDING (Trend analizi)
  6. LOGGING (Event loglama)

#### config.rs
- **Görev**: Konfigürasyon yapıları ve validasyon
- **Yapılar**:
  - `AppCfg`: Ana konfigürasyon
  - `BinanceCfg`: Binance API ayarları
  - `RiskCfg`: Risk yönetimi parametreleri
  - `TrendingCfg`: Trend analizi parametreleri
  - `ExecCfg`: Execution parametreleri
  - `WebsocketCfg`: WebSocket ayarları
  - `EventBusCfg`: Event bus buffer boyutları
- **Validasyon Kuralları**:
  - API key format kontrolü (min 20 karakter)
  - Leverage limit kontrolü (max_leverage)
  - Cross margin kontrolü (desteklenmez, hata verir)
  - Hedge mode kontrolü (desteklenmez, hata verir)
  - TP/SL tutarlılık kontrolü (TP > SL + commission)
  - Signal size tutarlılık (TRENDING vs ORDERING limit)

#### types.rs
- **Görev**: Tüm domain tipleri (single source of truth)
- **Kategoriler**:
  - **Core Types**: `Px`, `Qty`, `Side`, `PositionDirection`, `Tif`
  - **Connection Types**: `OrderCommand`, `VenueOrder`, `Position`, `SymbolRules`, `UserEvent`
  - **Event Bus Types**: `MarketTick`, `TradeSignal`, `CloseRequest`, `OrderUpdate`, `PositionUpdate`, `BalanceUpdate`
  - **State Types**: `OrderingState`, `OpenPosition`, `OpenOrder`, `BalanceStore`
  - **Trending Types**: `PricePoint`, `SymbolState`, `TrendSignal`, `LastSignal`
  - **Follow Orders Types**: `PositionInfo`

#### event_bus.rs
- **Görev**: Modüller arası iletişim kanalı
- **Yapı**: Broadcast channels (tokio::sync::broadcast)
- **Event Tipleri**:
  - `MarketTick`: Piyasa fiyat güncellemeleri (yüksek frekans)
  - `TradeSignal`: Trend sinyalleri (TRENDING → ORDERING)
  - `CloseRequest`: Pozisyon kapatma talepleri (FOLLOW_ORDERS → ORDERING)
  - `OrderUpdate`: Emir durumu güncellemeleri (CONNECTION → ORDERING, FOLLOW_ORDERS)
  - `PositionUpdate`: Pozisyon durumu güncellemeleri (CONNECTION → ORDERING, FOLLOW_ORDERS)
  - `BalanceUpdate`: Bakiye güncellemeleri (CONNECTION → BALANCE)
  - `OrderingStateUpdate`: State güncellemeleri (ORDERING → STORAGE, gelecekte)
  - `OrderFillHistoryUpdate`: Fill history güncellemeleri (CONNECTION → STORAGE, gelecekte)
- **Health Monitoring**: Receiver count tracking

#### state.rs
- **Görev**: Uygulama genelinde paylaşılan state
- **Yapılar**:
  - `SharedState`: Container (OrderingState + BalanceStore)
  - `OrderingState`: Açık pozisyon/emir bilgisi (tek pozisyon garantisi için)
  - `BalanceStore`: USDT/USDC bakiyeleri + rezerve edilmiş bakiyeler
- **Thread Safety**: Arc<Mutex<>> ve Arc<RwLock<>>

---

## 4. Modül Detayları ve İş Mantıkları

### 4.1 CONNECTION Modülü
**Dosya**: `src/connection.rs` + `src/connection/venue.rs` + `src/connection/websocket.rs`

#### Görevleri
- Binance Futures API ile tek iletişim noktası
- WebSocket bağlantıları yönetimi (Market Data + User Data streams)
- REST API çağrıları (emir gönderme, bakiye sorgulama, pozisyon sorgulama)
- Rate limit yönetimi (token bucket algoritması)
- Otomatik yeniden bağlanma (exponential backoff)
- Sembol kuralları cache'leme (1 saatte bir refresh)
- State senkronizasyonu (WebSocket reconnect sonrası REST API doğrulama)

#### Önemli Fonksiyonlar
- `start()`: WebSocket stream'lerini başlatır, leverage/margin type ayarlar
- `send_order()`: Emir gönderir (ORDERING modülü kullanır)
- `fetch_balance()`: Bakiye sorgular (BALANCE modülü kullanır)
- `get_current_prices()`: Güncel fiyatları döner (WebSocket cache'den, fallback REST API)
- `discover_symbols()`: Otomatik sembol keşfi (quote asset, balance, status, contract type filtreleri)
- `flatten_position()`: Pozisyon kapatma (MARKET reduce-only, LIMIT fallback)
- `validate_order_before_send()`: Emir öncesi validasyon (rules, min_notional, balance)

#### Rate Limit Yönetimi
**Token Bucket Algoritması**:
- **Emir gönderimi**: 300 emir / 5 dakika
- **Bakiye sorgulama**: 1200 sorgu / 1 dakika
- **Implementasyon**: `RateLimiter` struct (order_requests, balance_requests Vec<Instant>)
- **Bekleme**: Limit aşıldığında window süresi kadar bekler

#### WebSocket Stream'leri

**1. Market Data Stream** (`@bookTicker`):
- **URL**: `wss://fstream.binance.com/stream?streams=symbol1@bookTicker/symbol2@bookTicker`
- **Limit**: Max 200 karakter URL → max 10 sembol per stream
- **Data**: Bid/Ask fiyatları, bid/ask qty
- **Event**: Her fiyat güncellemesi → `MarketTick` event'i
- **Reconnect**: Exponential backoff (1s → 60s)
- **Ping/Pong**: 30 saniye aralıklarla (config: `websocket.ping_interval_ms`)

**2. User Data Stream** (`@user`):
- **URL**: `wss://fstream.binance.com/ws/{listenKey}`
- **ListenKey**: REST API ile oluşturulur, 60 dakika geçerli, 25 dakikada bir yenilenir
- **Events**:
  - `executionReport` / `ORDER_TRADE_UPDATE`: Emir fill/cancel → `OrderUpdate` event'i
  - `ACCOUNT_UPDATE`: Pozisyon/bakiye güncellemeleri → `PositionUpdate` / `BalanceUpdate` event'leri
  - `Heartbeat`: Bağlantı kontrolü
- **Reconnect**: ListenKey yenileme + WebSocket reconnect
- **State Validation**: Reconnect sonrası REST API ile state doğrulama

#### Cache Yapıları
- **PRICE_CACHE**: `DashMap<String, PriceUpdate>` - Sembol → Güncel fiyat (bid/ask)
- **POSITION_CACHE**: `DashMap<String, Position>` - Sembol → Açık pozisyon bilgisi
- **OPEN_ORDERS_CACHE**: `DashMap<String, Vec<VenueOrder>>` - Sembol → Açık emirler listesi
- **BALANCE_CACHE**: `DashMap<String, Decimal>` - Asset → Bakiye
- **FUT_RULES**: `DashMap<String, Arc<SymbolRules>>` - Sembol → Trading kuralları (tick size, step size, min notional)
- **Order Fill History**: `DashMap<String, OrderFillHistory>` - Order ID → Fill history (weighted average price hesaplama için)

#### Order Fill History Yönetimi
- **Amaç**: Weighted average fill price hesaplama
- **Yapı**: `OrderFillHistory { total_filled_qty, weighted_price_sum, maker_fill_count, total_fill_count, last_update }`
- **Hesaplama**: `average_price = weighted_price_sum / total_filled_qty`
- **Cleanup**: 24 saatten eski kayıtlar temizlenir (memory leak önleme)
- **Event**: `OrderFillHistoryUpdate` event'i (STORAGE modülü için, gelecekte)

#### State Senkronizasyonu
**WebSocket Reconnect Sonrası**:
1. REST API ile pozisyon sorgulama (her sembol için)
2. REST API ile açık emirler sorgulama (her sembol için)
3. REST API ile bakiye sorgulama (USDT/USDC)
4. WebSocket cache ile karşılaştırma
5. **REST API source of truth**: Cache'i REST API verisi ile güncelle
6. Mismatch durumunda warning log (significant differences için)

---

### 4.2 TRENDING Modülü
**Dosya**: `src/trending.rs`

#### Görevleri
- Piyasa verilerini analiz eder (MarketTick event'leri)
- Trend sinyalleri üretir (LONG veya SHORT)
- **ÖNEMLİ**: Emir atmaz, sadece sinyal üretir
- Event flood önleme (sampling: 1/10 tick işleme)

#### İş Mantığı

**1. Event Sampling (Event Flood Önleme)**:
- **Problem**: 100 sembol × 1 tick/saniye = 100 event/saniye = 8.64M event/gün
- **Çözüm**: Sampling - sadece 1/10 tick işlenir (10% sample rate)
- **Implementasyon**: Per-symbol counter (`tick_counter % 10 == 0`)
- **Sonuç**: %90 CPU tasarrufu, sinyal kalitesi korunur

**2. Trend Analizi**:
- **Multi-Timeframe SMA**:
  - Short-term: 10 periyot
  - Medium-term: 15 periyot
  - Long-term: 20 periyot
- **Trend Detection**:
  - Price deviation from SMA: ±%1.5 threshold
  - Konsensüs: En az 2/3 timeframe aynı yönde
- **Volume Confirmation**:
  - Recent volume vs older volume karşılaştırması
  - Minimum -10% volume change (collapse önleme)
- **Momentum Filtresi**:
  - Minimum %0.5 momentum gereksinimi
  - Momentum yönü trend yönü ile uyumlu olmalı

**3. Spread Kontrolü**:
- **Minimum Spread**: 5 bps (config: `trending.min_spread_bps`)
- **Maksimum Spread**: 200 bps (config: `trending.max_spread_bps`)
- **Hesaplama**: `spread_bps = ((ask - bid) / bid) * 10000`
- **Staleness Check**: Spread timestamp kontrolü (ORDERING'de 5 saniye max age)

**4. Cooldown Yönetimi**:
- **Signal Cooldown**: 30 saniye (config: `trending.signal_cooldown_seconds`)
- **Position Close Cooldown**: 5 saniye (hardcoded)
- **Direction-Aware Cooldown**:
  - Aynı yönde sinyal: Cooldown uygulanır
  - Zıt yönde sinyal: Cooldown bypass (trend reversal)
  - Unknown direction: Extended cooldown (10 saniye)

**5. Sinyal Üretim Validasyonu**:
- **Bakiye Kontrolü**: Minimum margin gereksinimi (`max_usd_per_order`)
- **Symbol Rules**: `CONNECTION.rules_for()` ile kuralları al
- **Min Notional**: `notional = max_usd_per_order * leverage >= min_notional`
- **Position Size**: `size = notional / entry_price` (quantized to step_size)
- **Double-Check**: Sinyal gönderilmeden önce tekrar pozisyon/emir kontrolü

#### Sinyal Üretim Kriterleri
1. ✅ Spread: 5-200 bps arası
2. ✅ Cooldown: Son sinyalden 30 saniye geçmiş
3. ✅ Position Close Cooldown: Son pozisyon kapanışından 5 saniye geçmiş
4. ✅ Trend: Multi-timeframe konsensüs (2/3 timeframe)
5. ✅ Volume: Volume confirmation geçti
6. ✅ Momentum: Minimum %0.5 momentum
7. ✅ Bakiye: Yeterli bakiye var
8. ✅ Symbol Rules: Min notional kontrolü geçti
9. ✅ Position/Order: Açık pozisyon/emir yok

#### Çıktı
- `TradeSignal` event'i yayınlar:
  ```rust
  TradeSignal {
      symbol: String,
      side: Side,  // Buy (LONG) veya Sell (SHORT)
      entry_price: Px,
      leverage: u32,
      size: Qty,
      stop_loss_pct: Option<f64>,
      take_profit_pct: Option<f64>,
      spread_bps: f64,
      spread_timestamp: Instant,
      timestamp: Instant,
  }
  ```

#### Memory Management
- **Symbol States Cleanup**: 1 saatte bir, 1 saatten eski sembol state'leri temizlenir
- **Price History**: Max 100 price point (sliding window)

---

### 4.3 ORDERING Modülü
**Dosya**: `src/ordering.rs`

#### Görevleri
- Emir açma/kapatma işlemlerini yönetir
- **Tek pozisyon garantisi** sağlar (global lock + state check)
- Bakiye rezervasyonu yönetir (RAII pattern)
- Emir durumu takibi (OrderUpdate/PositionUpdate event'leri)
- Race condition önleme (double-check locking)

#### İş Mantığı

**1. TradeSignal İşleme**:
- **Signal Validity Check**:
  - Timestamp age: Max 5 saniye (stale signal önleme)
  - Symbol validation: Boş sembol kontrolü
  - Spread staleness: Max 5 saniye (ORDERING'de tekrar kontrol)
- **Risk Control**:
  - Max position notional: `notional <= max_position_notional_usd`
  - Min quote balance: `available_balance >= min_quote_balance_usd`
- **Atomic Operation** (Lock içinde):
  - State check: Açık pozisyon/emir var mı?
  - Balance reservation: Gerekli margin rezerve et
- **Order Placement** (Lock dışında, hemen):
  - `CONNECTION.send_order()` çağrılır
  - Retry logic: Max 3 retry, exponential backoff
  - Permanent error: Retry yapılmaz, balance release
- **State Update** (Lock içinde):
  - Order ID kaydedilir
  - `OrderingState.open_order` güncellenir
  - Balance reservation release

**2. CloseRequest İşleme**:
- **Position Check**: Early check (sadece logging için)
- **Flatten Position**: `CONNECTION.flatten_position()` çağrılır
  - MARKET reduce-only order (hızlı kapanış için)
  - LIMIT fallback (MIN_NOTIONAL hatası durumunda)
  - Retry logic: Max 3 attempt, position growth detection
- **Position Growth Detection**: 
  - Position %10'dan fazla büyürse → warning
  - Max 8 growth event → abort (infinite loop önleme)

**3. OrderUpdate İşleme** (State Sync):
- **Timestamp Check**: Stale update önleme
- **Race Condition Prevention**:
  - PositionUpdate ile OrderUpdate arasında race condition
  - Timestamp-based version control
  - Position existence check (OrderUpdate → Position dönüşümünde)
- **State Transitions**:
  - `Filled` → Position oluştur, order temizle
  - `Canceled` / `Expired` / `Rejected` → Order temizle
  - `PartiallyFilled` → Order qty güncelle

**4. PositionUpdate İşleme** (State Sync):
- **Timestamp Check**: Stale update önleme
- **Race Condition Prevention**:
  - OrderUpdate ile PositionUpdate arasında race condition
  - Qty AND entry_price comparison (partial fill detection)
  - Epsilon-based comparison (floating point precision)
- **State Transitions**:
  - `is_open=false` → Position temizle
  - `is_open=true` → Position oluştur/güncelle (qty veya entry_price değiştiyse)

#### Bakiye Rezervasyonu (RAII Pattern)
- **BalanceReservation**: RAII guard struct
- **try_reserve()**: Atomic operation (check + reserve)
- **release()**: Explicit release (Drop trait warning verir)
- **Leak Detection**: Background task (10 saniyede bir kontrol)
- **Auto-Fix**: Reserved > Total durumunda reset

#### Race Condition Önleme
**Double-Check Locking**:
1. Lock: State check + balance reserve
2. Unlock: Order placement (network call)
3. Lock: State update (double-check)

**Timestamp-Based Version Control**:
- `last_order_update_timestamp`: OrderUpdate için
- `last_position_update_timestamp`: PositionUpdate için
- Stale update'ler ignore edilir

**Position Growth Detection**:
- Position büyümesi tespit edilirse → warning
- Max 8 growth event → abort (infinite loop önleme)

---

### 4.4 FOLLOW_ORDERS Modülü
**Dosya**: `src/follow_orders.rs`

#### Görevleri
- Açık pozisyonları takip eder
- Take Profit (TP) ve Stop Loss (SL) kontrolü
- TP/SL tetiklendiğinde `CloseRequest` event'i yayınlar
- Komisyon dahil net PnL hesaplama

#### İş Mantığı

**1. Position Tracking**:
- **PositionUpdate Event**: Pozisyon açıldığında → `PositionInfo` kaydet
- **TradeSignal Event**: TP/SL bilgilerini kaydet (race condition için)
- **OrderUpdate Event**: `is_maker` bilgisini kaydet (komisyon hesaplama için)

**2. TP/SL Kontrolü**:
- **MarketTick Event**: Her fiyat güncellemesinde kontrol
- **is_maker Check**: `is_maker` None ise skip (OrderUpdate bekleniyor)
- **PnL Hesaplama**:
  - Gross PnL%: `price_change_pct * leverage`
  - Entry Commission: Maker (%0.02) veya Taker (%0.04)
  - Exit Commission: Taker (%0.04) - her zaman
  - Net PnL%: `gross_pnl_pct - total_commission_pct`
- **TP/SL Trigger**:
  - TP: `net_pnl_pct >= take_profit_pct`
  - SL: `net_pnl_pct <= -stop_loss_pct`

**3. CloseRequest Gönderimi**:
- **CloseRequest Event**: TP/SL tetiklendiğinde
- **Retry Logic**: Subscriber yoksa retry (next tick)
- **Position Removal**: CloseRequest gönderildikten sonra pozisyon tracking'den kaldırılır

#### PnL Hesaplama Detayları
**Isolated Margin Modu** (varsayılan, zorunlu):
```
Price Change% = (CurrentPrice - EntryPrice) / EntryPrice × 100

Long Position:
  - Price Change% = (CurrentPrice - EntryPrice) / EntryPrice × 100
  - Gross PnL% = Price Change% × Leverage

Short Position:
  - Price Change% = (EntryPrice - CurrentPrice) / EntryPrice × 100
  - Gross PnL% = Price Change% × Leverage

Net PnL% = Gross PnL% - (Entry Commission% + Exit Commission%)
```

**Komisyon Hesaplama**:
- **Entry Commission**: `is_maker` true ise %0.02, false ise %0.04
- **Exit Commission**: Her zaman %0.04 (MARKET order)
- **Total Commission**: Entry + Exit

**ÖNEMLİ**: Cross margin modu desteklenmez (PnL hesaplama farklıdır).

#### Çıktı
- `CloseRequest` event'i yayınlar:
  ```rust
  CloseRequest {
      symbol: String,
      position_id: Option<String>,  // Gelecekte hedge mode için
      reason: CloseReason,  // TakeProfit veya StopLoss
      current_bid: Option<Px>,
      current_ask: Option<Px>,
      timestamp: Instant,
  }
  ```

---

### 4.5 BALANCE Modülü
**Dosya**: `src/balance.rs`

#### Görevleri
- USDT ve USDC bakiyelerini takip eder
- Shared state'te bakiye bilgisini tutar
- Diğer modüllere bakiye bilgisi sağlar
- Rezerve bakiye takibi (ORDERING modülü için)

#### İş Mantığı

**1. WebSocket-First Yaklaşım**:
- **BalanceUpdate Event**: WebSocket'ten gelen güncellemeleri dinler
- **Priority**: WebSocket updates are prioritized (real-time, more accurate)
- **Timestamp Check**: REST API updates are ignored if WebSocket is newer

**2. REST API Fallback**:
- **Startup**: İlk bakiye sorgulama (retry mechanism: 5 attempt, exponential backoff)
- **WebSocket Failure**: WebSocket bağlantısı kesilirse periyodik sorgulama (şu an yok, sadece startup)

**3. Shared State Güncelleme**:
- **BalanceStore**: `{ usdt, usdc, reserved_usdt, reserved_usdc, last_updated }`
- **Atomic Update**: RwLock ile thread-safe
- **Timestamp Check**: REST API update sadece timestamp daha yeni ise kabul edilir

#### BalanceStore API
- `available(asset)`: Kullanılabilir bakiye (total - reserved)
- `try_reserve(asset, amount)`: Bakiye rezervasyonu (atomic, returns bool)
- `release(asset, amount)`: Rezervasyon serbest bırakma

---

### 4.6 LOGGING Modülü
**Dosya**: `src/logging.rs`

#### Görevleri
- Tüm önemli event'leri loglar
- Structured logging (JSON format)
- Trade ve PnL kayıtları
- Event throttling (MarketTick için)

#### Loglanan Event'ler
- `TradeSignal`: Trend sinyalleri
- `OrderUpdate`: Emir durumu değişiklikleri (JSON log)
- `PositionUpdate`: Pozisyon açma/kapama (JSON log)
- `BalanceUpdate`: Bakiye değişiklikleri
- `CloseRequest`: TP/SL tetiklemeleri
- `MarketTick`: Throttled (her 1000 tick per symbol)

#### Log Formatı
- **Dosya**: `logs/trading_events.json`
- **Format**: JSON Lines (her satır bir JSON objesi)
- **Structured Fields**: timestamp, event_type, symbol, side, price, qty, pnl, vb.

#### Event Throttling
- **MarketTick**: Her 1000 tick per symbol (log spam önleme)
- **Cleanup**: 1 saatte bir, 1 saatten eski sembol counter'ları temizlenir

---

## 5. Veri Akışı ve İş Süreçleri

### 5.1 Tek Trade'in Hayat Döngüsü

#### Adım 1: Piyasa Verisi Gelişi
```
Binance WebSocket (@bookTicker)
    ↓
CONNECTION (MarketDataStream)
    ↓
MarketTick Event (EventBus)
    ↓
TRENDING (sampling: 1/10 tick)
```

#### Adım 2: Trend Analizi
```
MarketTick Event
    ↓
TRENDING.process_market_tick()
    ├── Sampling check (1/10)
    ├── Position/Order check (skip if exists)
    ├── Cooldown check (signal + position close)
    ├── Spread check (5-200 bps)
    ├── Trend analysis (multi-timeframe SMA)
    ├── Volume confirmation
    ├── Momentum check
    ├── Balance check
    ├── Symbol rules validation
    └── TradeSignal Event (EventBus)
```

#### Adım 3: Emir Açma
```
TradeSignal Event
    ↓
ORDERING.handle_trade_signal()
    ├── Signal validity check (age, spread staleness)
    ├── Risk control (max notional, min balance)
    ├── Lock: State check + Balance reserve
    ├── Unlock: Order placement (CONNECTION.send_order())
    │   ├── Retry logic (max 3, exponential backoff)
    │   └── Permanent error → balance release
    ├── Lock: State update (double-check)
    └── OrderUpdate Event (WebSocket → CONNECTION → EventBus)
```

#### Adım 4: Emir Fill
```
Binance WebSocket (executionReport)
    ↓
CONNECTION (UserDataStream)
    ↓
OrderFillHistory update (weighted average price)
    ↓
OrderUpdate Event (EventBus)
    ↓
ORDERING.handle_order_update()
    ├── Timestamp check (stale prevention)
    ├── Position existence check (race condition)
    └── State update: Order → Position
    ↓
FOLLOW_ORDERS.handle_order_update()
    └── is_maker info update (commission calculation)
```

#### Adım 5: Pozisyon Takibi
```
MarketTick Event
    ↓
FOLLOW_ORDERS.check_tp_sl()
    ├── Position lookup
    ├── is_maker check (skip if None)
    ├── PnL calculation (gross - commission)
    ├── TP check (net_pnl_pct >= take_profit_pct)
    └── SL check (net_pnl_pct <= -stop_loss_pct)
```

#### Adım 6: TP/SL Tetikleme
```
TP/SL Tetiklendi
    ↓
FOLLOW_ORDERS
    └── CloseRequest Event (EventBus)
    ↓
ORDERING.handle_close_request()
    └── CONNECTION.flatten_position()
        ├── MARKET reduce-only order
        ├── Retry logic (max 3, position growth detection)
        └── LIMIT fallback (MIN_NOTIONAL error)
```

#### Adım 7: Pozisyon Kapanışı
```
Binance WebSocket (ACCOUNT_UPDATE)
    ↓
CONNECTION (UserDataStream)
    ↓
PositionUpdate Event (is_open=false)
    ↓
ORDERING.handle_position_update()
    └── State update: Position = None
    ↓
FOLLOW_ORDERS.handle_position_update()
    └── Position tracking remove
    ↓
LOGGING
    └── Trade log (JSON)
```

### 5.2 Event Akış Diyagramı

```
┌─────────────┐
│  Binance    │
│  WebSocket  │
└──────┬──────┘
       │
       ▼
┌─────────────┐      MarketTick      ┌─────────────┐
│ CONNECTION  │─────────────────────►│  TRENDING   │
│             │                       │             │
│  - Market   │                       │  - SMA      │
│    Data WS  │                       │  - Momentum │
│  - User     │                       │  - Volume   │
│    Data WS  │                       │  - Spread   │
│  - REST API │                       └──────┬──────┘
└──────┬──────┘                              │
       │                                      │ TradeSignal
       │ OrderUpdate                          │
       │ PositionUpdate                       │
       │ BalanceUpdate                        ▼
       │                            ┌─────────────┐
       │                            │  ORDERING   │
       │                            │             │
       │                            │  - Single   │
       │                            │    Position │
       │                            │  - Balance  │
       │                            │    Reserve  │
       │                            └──────┬──────┘
       │                                   │
       │                                   │ send_order()
       │                                   │
       └───────────────────────────────────┘
                    │
                    ▼
            ┌─────────────┐
            │   Binance   │
            │   REST API  │
            └─────────────┘
```

### 5.3 Modüller Arası İletişim Tablosu

| Event Gönderen | Event Alıcı | Event Tipi | Amaç |
|----------------|--------------|-----------|------|
| CONNECTION | TRENDING | MarketTick | Fiyat güncellemeleri (trend analizi) |
| CONNECTION | FOLLOW_ORDERS | MarketTick | TP/SL kontrolü |
| CONNECTION | ORDERING | OrderUpdate | Emir durumu (fill, cancel, vb.) |
| CONNECTION | ORDERING | PositionUpdate | Pozisyon durumu (açık/kapalı) |
| CONNECTION | BALANCE | BalanceUpdate | Bakiye güncellemeleri |
| TRENDING | ORDERING | TradeSignal | Yeni emir sinyali |
| FOLLOW_ORDERS | ORDERING | CloseRequest | Pozisyon kapatma talebi |
| ORDERING | STORAGE (gelecekte) | OrderingStateUpdate | State persistence |
| CONNECTION | STORAGE (gelecekte) | OrderFillHistoryUpdate | Fill history persistence |
| Tüm Modüller | LOGGING | LogEvent (implicit) | Event loglama |

---

## 6. Risk Yönetimi ve Güvenlik

### 6.1 Pozisyon Riski
- **Tek Pozisyon Kuralı**: Aynı anda sadece bir açık pozisyon/emir
- **Isolated Margin**: Her pozisyon kendi margin'i ile izole edilir (zorunlu)
- **Maksimum Pozisyon Boyutu**: 5000 USD (notional, config: `risk.max_position_notional_usd`)
- **Position Growth Detection**: Pozisyon %10'dan fazla büyürse → warning, max 8 event → abort

### 6.2 Leverage Riski
- **Varsayılan Leverage**: 20x (config: `leverage` veya `exec.default_leverage`)
- **Maksimum Leverage**: 50x (config: `risk.max_leverage`)
- **Leverage Kontrolü**: 
  - Startup'ta exchange leverage ile config karşılaştırma
  - Açık pozisyon varsa → hata (leverage değiştirilemez)
  - Açık pozisyon yoksa → otomatik düzeltme
- **Leverage Validation**: Config validation'da kontrol edilir

### 6.3 Bakiye Riski
- **Minimum Bakiye**: 120 USD (config: `min_quote_balance_usd`)
- **Bakiye Rezervasyonu**: 
  - Emir gönderilmeden önce margin rezerve edilir (atomic)
  - RAII pattern ile otomatik temizlik
  - Leak detection task (10 saniyede bir kontrol)
- **Bakiye Kontrolü**: 
  - Her emir öncesi yeterli bakiye kontrolü
  - Available balance = total - reserved
  - Min quote balance check (hem açma hem kapatma için)

### 6.4 Emir Riski
- **Minimum Emir Boyutu**: 10 USD (margin, config: `min_usd_per_order`)
- **Maksimum Emir Boyutu**: 100 USD (margin, config: `max_usd_per_order`)
- **Min Notional Kontrolü**: Exchange'in minimum notional gereksinimi kontrol edilir
- **Order Validation**: 
  - Price/qty quantization (tick_size, step_size)
  - Precision check (fractional digits)
  - Min notional check
  - Balance check

### 6.5 Stop Loss ve Take Profit
- **Take Profit**: %5 (config: `take_profit_pct`)
- **Stop Loss**: %2 (config: `stop_loss_pct`)
- **Otomatik Kapatma**: TP/SL seviyelerine ulaşıldığında otomatik kapatma (MARKET reduce-only)
- **Net PnL Hesaplama**: Komisyon dahil (entry + exit commission)
- **Validation**: Config validation'da TP > SL + commission kontrolü

### 6.6 Komisyon Hesaplama
- **Maker Komisyon**: %0.02 (config: `risk.maker_commission_pct`)
- **Taker Komisyon**: %0.04 (config: `risk.taker_commission_pct`)
- **Komisyon Seçimi**: 
  - Entry: Tüm fill'ler maker ise maker, aksi halde taker
  - Exit: Her zaman taker (MARKET order)
- **Net PnL**: Gross PnL - (Entry Commission + Exit Commission)

### 6.7 Spread Riski
- **Minimum Spread**: 5 bps (çok dar spread → flash crash riski)
- **Maksimum Spread**: 200 bps (çok geniş spread → düşük likidite)
- **Staleness Check**: Spread timestamp kontrolü (max 5 saniye)

### 6.8 Rate Limit Riski
- **Emir Rate Limit**: 300 emir / 5 dakika
- **Bakiye Rate Limit**: 1200 sorgu / 1 dakika
- **Yönetim**: Token bucket algoritması ile otomatik bekleme

---

## 7. Teknik Altyapı ve Performans

### 7.1 WebSocket Yönetimi
- **Market Data Stream**: `@bookTicker` (her sembol için ayrı stream, max 10 sembol per stream)
- **User Data Stream**: `@user` (tek stream, tüm semboller için)
- **Reconnect Mekanizması**: Exponential backoff (1s → 60s)
- **Ping/Pong**: 30 saniye aralıklarla (config: `websocket.ping_interval_ms`)
- **ListenKey Management**: 60 dakika geçerli, 25 dakikada bir yenilenir
- **State Validation**: Reconnect sonrası REST API ile state doğrulama

### 7.2 REST API Kullanımı
- **WebSocket-first yaklaşım**: Mümkün olduğunca WebSocket kullanılır
- **REST API fallback**: Sadece gerektiğinde (startup, WebSocket kesilirse, cache empty)
- **Rate Limit**: Token bucket algoritması ile yönetilir
- **Retry Logic**: Exponential backoff (max 3 retry, bazı durumlarda max 2)

### 7.3 Cache Yönetimi
- **Price Cache**: WebSocket'ten gelen fiyatlar cache'lenir (DashMap)
- **Position Cache**: WebSocket'ten gelen pozisyon bilgileri cache'lenir
- **Order Cache**: Açık emirler cache'lenir
- **Rules Cache**: Sembol kuralları cache'lenir (1 saatte bir yenilenir)
- **Balance Cache**: WebSocket'ten gelen bakiye bilgileri cache'lenir
- **Cache Invalidation**: Rules refresh task (1 saatte bir)

### 7.4 Threading Modeli
- **Tokio Runtime**: Multi-threaded async runtime
- **Modül Bağımsızlığı**: Her modül kendi task'ında çalışır
- **Event Bus**: Broadcast channels (multiple subscribers destekler)
- **Lock Strategy**: 
  - OrderingState: Mutex (exclusive access)
  - BalanceStore: RwLock (multiple readers, single writer)

### 7.5 Hata Yönetimi
- **Graceful Shutdown**: Ctrl+C sinyali ile güvenli kapanış
- **Panic Recovery**: Modül panikleri ana uygulamayı etkilemez (task isolation)
- **Retry Mekanizması**: Exponential backoff ile retry
- **Permanent Error Detection**: Invalid params, insufficient balance, vb. retry edilmez

### 7.6 Memory Management
- **Order Fill History Cleanup**: 24 saatten eski kayıtlar temizlenir (1 saatte bir)
- **Symbol States Cleanup**: 1 saatten eski sembol state'leri temizlenir (1 saatte bir)
- **Tick Counters Cleanup**: 1 saatten eski tick counter'lar temizlenir (1 saatte bir)
- **Balance Leak Detection**: 10 saniyede bir kontrol, auto-fix

### 7.7 Performans Optimizasyonları
- **Event Sampling**: MarketTick event'lerinin %90'ı skip edilir (1/10 sample rate)
- **Early Exit**: Cooldown check'ler trend analizinden önce yapılır
- **Cache-First**: WebSocket cache'den okuma, REST API fallback
- **Batch Processing**: Symbol rules refresh (tüm semboller bir anda)

---

## 8. Konfigürasyon Sistemi

### 8.1 Konfigürasyon Dosyası
**Dosya**: `config.yaml`

### 8.2 Konfigürasyon Yapıları

#### AppCfg (Ana Konfigürasyon)
- `symbol`: Tek sembol (opsiyonel)
- `symbols`: Sembol listesi (opsiyonel, boş = auto discovery)
- `auto_discover_quote`: Otomatik sembol keşfi (default: true)
- `quote_asset`: Ana quote asset (USDC veya USDT, default: USDC)
- `allow_usdt_quote`: USDT sembollerini de dahil et (default: true)
- `max_usd_per_order`: Maksimum emir boyutu (USD, margin, default: 100.0)
- `min_usd_per_order`: Minimum emir boyutu (USD, margin, default: 10.0)
- `min_quote_balance_usd`: Minimum bakiye eşiği (USD, default: 120.0)
- `leverage`: Leverage (opsiyonel, default: exec.default_leverage)
- `take_profit_pct`: Take Profit yüzdesi (default: 5.0)
- `stop_loss_pct`: Stop Loss yüzdesi (default: 2.0)

#### BinanceCfg
- `futures_base`: Binance Futures API base URL (default: https://fapi.binance.com)
- `api_key`: API key (required)
- `secret_key`: Secret key (required)
- `recv_window_ms`: Receive window (default: 5000)
- `hedge_mode`: Hedge mode (default: false, desteklenmez)

#### RiskCfg
- `max_leverage`: Maksimum leverage (default: 50)
- `use_isolated_margin`: Isolated margin kullan (default: true, zorunlu)
- `max_position_notional_usd`: Maksimum pozisyon boyutu (USD, default: 5000.0)
- `maker_commission_pct`: Maker komisyon (default: 0.02)
- `taker_commission_pct`: Taker komisyon (default: 0.04)

#### TrendingCfg
- `min_spread_bps`: Minimum spread (default: 5.0)
- `max_spread_bps`: Maksimum spread (default: 200.0)
- `signal_cooldown_seconds`: Sinyal cooldown (default: 30)

#### ExecCfg
- `tif`: Time in force (default: "post_only")
- `default_leverage`: Varsayılan leverage (default: 20)

#### WebsocketCfg
- `reconnect_delay_ms`: Yeniden bağlanma gecikmesi (default: 5000)
- `ping_interval_ms`: Ping aralığı (default: 30000)

#### EventBusCfg
- `market_tick_buffer`: MarketTick buffer boyutu (default: 1000)
- `trade_signal_buffer`: TradeSignal buffer boyutu (default: 1000)
- `close_request_buffer`: CloseRequest buffer boyutu (default: 1000)
- `order_update_buffer`: OrderUpdate buffer boyutu (default: 1000)
- `position_update_buffer`: PositionUpdate buffer boyutu (default: 1000)
- `balance_update_buffer`: BalanceUpdate buffer boyutu (default: 1000)

### 8.3 Konfigürasyon Validasyonu

**Kritik Validasyonlar**:
1. **Cross Margin Kontrolü**: `use_isolated_margin=false` → hata (desteklenmez)
2. **Hedge Mode Kontrolü**: `hedge_mode=true` → hata (desteklenmez)
3. **Leverage Kontrolü**: `leverage > max_leverage` → hata
4. **TP/SL Tutarlılık**: `take_profit_pct <= stop_loss_pct + commission` → hata
5. **Signal Size Tutarlılık**: `max_usd_per_order * leverage > max_position_notional_usd` → hata
6. **Min Balance**: `min_quote_balance_usd < max_usd_per_order` → hata
7. **API Key Format**: Min 20 karakter kontrolü

---

## 9. Hata Yönetimi ve Güvenilirlik

### 9.1 Hata Senaryoları ve Çözümleri

#### WebSocket Bağlantı Hatası
- **Tespit**: Bağlantı kesilirse
- **Aksiyon**: Exponential backoff ile yeniden bağlanma (1s → 60s)
- **State Senkronizasyonu**: Reconnect sonrası REST API ile state doğrulama
- **ListenKey**: Yenileme veya yeni key oluşturma

#### REST API Rate Limit
- **Tespit**: Rate limit aşıldığında
- **Aksiyon**: Token bucket ile bekleme
- **Loglama**: Rate limit uyarıları loglanır

#### Bakiye Yetersizliği
- **Tespit**: Emir gönderilmeden önce
- **Aksiyon**: Emir reddedilir, hata loglanır
- **Kullanıcı Bildirimi**: Warning log

#### Leverage Uyumsuzluğu
- **Tespit**: Startup'ta exchange leverage ile config karşılaştırma
- **Aksiyon**: 
  - Açık pozisyon yoksa → Otomatik düzeltme
  - Açık pozisyon varsa → Hata, uygulama başlamaz

#### Margin Type Uyumsuzluğu
- **Tespit**: Startup'ta exchange margin type ile config karşılaştırma
- **Aksiyon**: Otomatik düzeltme (isolated/cross)
- **Hata Handling**: -4046 "No need to change" → success

#### Emir Fill Hatası
- **Tespit**: OrderUpdate event'inde hata durumu
- **Aksiyon**: Bakiye rezervasyonu serbest bırakılır, state temizlenir

#### Position Growth (Pozisyon Büyümesi)
- **Tespit**: Pozisyon kapatma sırasında pozisyon büyümesi
- **Aksiyon**: 
  - %10'dan az büyüme → warning, retry
  - %10'dan fazla büyüme → warning, growth event count++
  - Max 8 growth event → abort (infinite loop önleme)

#### MIN_NOTIONAL Hatası
- **Tespit**: Emir gönderilirken
- **Aksiyon**: 
  - Dust check (remaining_qty < min_notional / price)
  - Dust ise → success (pozisyon kapalı sayılır)
  - Dust değilse → LIMIT fallback (MARKET başarısız olduysa)

### 9.2 Güvenilirlik Mekanizmaları

#### State Senkronizasyonu
- **WebSocket Reconnect Sonrası**: REST API ile state doğrulama
- **Cache Güncelleme**: REST API verisi cache'i günceller (source of truth)
- **Mismatch Handling**: Significant mismatch → warning, cache update

#### Memory Leak Önleme
- **Order Fill History Cleanup**: 24 saatten eski kayıtlar temizlenir
- **Symbol States Cleanup**: 1 saatten eski sembol state'leri temizlenir
- **Balance Reservation Leak Detection**: 10 saniyede bir kontrol, auto-fix

#### Graceful Shutdown
- **Ctrl+C Sinyali**: Tüm modüller güvenli şekilde kapanır
- **Event Bus Temizliği**: Tüm event channel'ları kapatılır
- **WebSocket Bağlantıları**: Güvenli şekilde kapatılır

#### Race Condition Önleme
- **Double-Check Locking**: State check + order placement + state update
- **Timestamp-Based Version Control**: Stale update'ler ignore edilir
- **Atomic Operations**: Balance reservation (check + reserve)

---

## 10. State Yönetimi ve Senkronizasyon

### 10.1 SharedState Yapısı

#### OrderingState
- **Amaç**: Tek pozisyon garantisi için
- **Yapı**:
  ```rust
  OrderingState {
      open_position: Option<OpenPosition>,
      open_order: Option<OpenOrder>,
      last_order_update_timestamp: Option<Instant>,
      last_position_update_timestamp: Option<Instant>,
  }
  ```
- **Thread Safety**: `Arc<Mutex<OrderingState>>`
- **Update Mekanizması**: 
  - OrderUpdate event → state update
  - PositionUpdate event → state update
  - Manual update (order placement)

#### BalanceStore
- **Amaç**: Bakiye takibi ve rezervasyon
- **Yapı**:
  ```rust
  BalanceStore {
      usdt: Decimal,
      usdc: Decimal,
      reserved_usdt: Decimal,
      reserved_usdc: Decimal,
      last_updated: Instant,
  }
  ```
- **Thread Safety**: `Arc<RwLock<BalanceStore>>`
- **API**:
  - `available(asset)`: Total - reserved
  - `try_reserve(asset, amount)`: Atomic reservation
  - `release(asset, amount)`: Reservation release

### 10.2 State Senkronizasyonu

#### WebSocket vs REST API
- **WebSocket Priority**: WebSocket updates are prioritized (real-time)
- **REST API Fallback**: Cache empty ise REST API kullanılır
- **Timestamp Check**: REST API update sadece timestamp daha yeni ise kabul edilir

#### Reconnect Sonrası Validation
1. REST API ile pozisyon sorgulama (her sembol için)
2. REST API ile açık emirler sorgulama (her sembol için)
3. REST API ile bakiye sorgulama (USDT/USDC)
4. WebSocket cache ile karşılaştırma
5. **REST API source of truth**: Cache'i REST API verisi ile güncelle
6. Mismatch durumunda warning log

---

## 11. Event Bus Sistemi

### 11.1 Event Bus Yapısı
- **Implementasyon**: Tokio broadcast channels
- **Buffer Sizes**: Configurable (EventBusCfg)
- **Multiple Subscribers**: Her modül kendi receiver'ını oluşturur

### 11.2 Event Tipleri

#### MarketTick
- **Gönderen**: CONNECTION
- **Alıcılar**: TRENDING, FOLLOW_ORDERS, LOGGING
- **Frekans**: Yüksek (saniyede binlerce)
- **Buffer**: 1000 (default)

#### TradeSignal
- **Gönderen**: TRENDING
- **Alıcılar**: ORDERING, FOLLOW_ORDERS, LOGGING
- **Frekans**: Düşük (cooldown nedeniyle)
- **Buffer**: 1000 (default)

#### CloseRequest
- **Gönderen**: FOLLOW_ORDERS
- **Alıcılar**: ORDERING, LOGGING
- **Frekans**: Düşük (TP/SL tetiklendiğinde)
- **Buffer**: 1000 (default)

#### OrderUpdate
- **Gönderen**: CONNECTION
- **Alıcılar**: ORDERING, FOLLOW_ORDERS, LOGGING
- **Frekans**: Orta (emir fill/cancel olduğunda)
- **Buffer**: 1000 (default)

#### PositionUpdate
- **Gönderen**: CONNECTION
- **Alıcılar**: ORDERING, FOLLOW_ORDERS, TRENDING, LOGGING
- **Frekans**: Düşük (pozisyon aç/kapat olduğunda)
- **Buffer**: 1000 (default)

#### BalanceUpdate
- **Gönderen**: CONNECTION
- **Alıcılar**: BALANCE, LOGGING
- **Frekans**: Düşük (bakiye değiştiğinde)
- **Buffer**: 1000 (default)

### 11.3 Health Monitoring
- **Receiver Count Tracking**: Her event channel için receiver sayısı
- **Health Check**: 10 saniyede bir kontrol
- **Warning**: Receiver count = 0 → modül crash olmuş olabilir

---

## 12. WebSocket ve REST API Yönetimi

### 12.1 WebSocket Stream'leri

#### Market Data Stream
- **Endpoint**: `wss://fstream.binance.com/stream?streams=symbol1@bookTicker/symbol2@bookTicker`
- **Format**: `{"stream":"btcusdt@bookTicker","data":{"b":"50000","B":"1.5","a":"50001","A":"2.0"}}`
- **Limit**: Max 200 karakter URL → max 10 sembol per stream
- **Chunking**: Semboller 10'luk gruplara bölünür
- **Reconnect**: Exponential backoff (1s → 60s)

#### User Data Stream
- **Endpoint**: `wss://fstream.binance.com/ws/{listenKey}`
- **ListenKey**: REST API ile oluşturulur (`/fapi/v1/listenKey`)
- **ListenKey Lifetime**: 60 dakika
- **Keepalive**: 25 dakikada bir yenilenir
- **Events**:
  - `executionReport` / `ORDER_TRADE_UPDATE`: Emir fill/cancel
  - `ACCOUNT_UPDATE`: Pozisyon/bakiye güncellemeleri
  - `Heartbeat`: Bağlantı kontrolü
- **Reconnect**: ListenKey yenileme + WebSocket reconnect

### 12.2 REST API Endpoints

#### Order Management
- `POST /fapi/v1/order`: Emir gönderme
- `DELETE /fapi/v1/order`: Emir iptal
- `GET /fapi/v1/openOrders`: Açık emirler sorgulama

#### Position Management
- `GET /fapi/v2/positionRisk`: Pozisyon sorgulama
- `POST /fapi/v1/leverage`: Leverage ayarlama
- `POST /fapi/v1/marginType`: Margin type ayarlama
- `POST /fapi/v1/positionSide/dual`: Hedge mode ayarlama

#### Account Management
- `GET /fapi/v2/balance`: Bakiye sorgulama
- `POST /fapi/v1/listenKey`: ListenKey oluşturma
- `PUT /fapi/v1/listenKey`: ListenKey yenileme

#### Market Data
- `GET /fapi/v1/exchangeInfo`: Sembol kuralları
- `GET /fapi/v1/depth`: Order book (fallback için)

### 12.3 Rate Limit Yönetimi
- **Token Bucket**: Vec<Instant> ile request tracking
- **Order Rate Limit**: 300 emir / 5 dakika
- **Balance Rate Limit**: 1200 sorgu / 1 dakika
- **Bekleme**: Limit aşıldığında window süresi kadar bekler

---

## 13. Önemli Tasarım Kararları ve Kısıtlamalar

### 13.1 Desteklenmeyen Özellikler

#### Hedge Mode (Dual-Side Position)
- **Durum**: Desteklenmez
- **Neden**: 
  - Position struct sadece tek pozisyon per symbol destekler
  - TP/SL tracking symbol-based, position-side-based değil
  - `flatten_position` tüm pozisyonları kapatır (LONG + SHORT)
- **Config Validation**: `hedge_mode=true` → hata

#### Cross Margin
- **Durum**: Desteklenmez
- **Neden**: 
  - PnL hesaplama isolated margin formülü kullanır
  - Cross margin farklı formül gerektirir (shared account equity)
- **Config Validation**: `use_isolated_margin=false` → hata

### 13.2 Önemli Tasarım Kararları

#### WebSocket-First Yaklaşım
- **Karar**: Mümkün olduğunca WebSocket kullanılır
- **Neden**: 
  - Daha hızlı (real-time)
  - Rate limit tasarrufu
  - Binance önerisi
- **Fallback**: REST API sadece gerektiğinde (startup, cache empty)

#### Event Sampling
- **Karar**: MarketTick event'lerinin %90'ı skip edilir (1/10 sample rate)
- **Neden**: 
  - Event flood önleme (8.64M event/gün → 864K event/gün)
  - CPU tasarrufu
  - Trend analizi için yeterli
- **Implementasyon**: Per-symbol counter

#### Tek Pozisyon Garantisi
- **Karar**: Aynı anda sadece bir açık pozisyon/emir
- **Neden**: 
  - Risk yönetimi
  - Basitlik
  - State yönetimi kolaylığı
- **Implementasyon**: Global lock (Mutex) + state check

#### Komisyon Dahil Net PnL
- **Karar**: TP/SL kontrolü net PnL ile yapılır (komisyon dahil)
- **Neden**: 
  - Gerçek kar/zarar hesaplama
  - Doğru TP/SL trigger
- **Hesaplama**: Gross PnL - (Entry Commission + Exit Commission)

#### RAII Pattern (Balance Reservation)
- **Karar**: Balance reservation RAII guard ile yönetilir
- **Neden**: 
  - Otomatik temizlik
  - Memory leak önleme
  - Drop trait warning
- **Leak Detection**: Background task (10 saniyede bir)

#### Timestamp-Based Version Control
- **Karar**: Event'ler timestamp ile version control edilir
- **Neden**: 
  - Race condition önleme
  - Stale update önleme
  - OrderUpdate vs PositionUpdate race condition
- **Implementasyon**: `last_order_update_timestamp`, `last_position_update_timestamp`

### 13.3 Bilinen Kısıtlamalar

1. **Hedge Mode**: Desteklenmez (config validation ile engellenir)
2. **Cross Margin**: Desteklenmez (config validation ile engellenir)
3. **Multiple Positions**: Aynı anda sadece bir pozisyon
4. **Symbol Limit**: WebSocket URL limit (max 10 sembol per stream)
5. **Event Lag**: Broadcast channel lagging (missed events warning)

---

## 14. Sonuç

Bu trading bot, **event-driven, modüler mimari** kullanarak Binance Futures borsasında otomatik ticaret yapmak için tasarlanmıştır. Bot'un temel özellikleri:

- ✅ **WebSocket-first yaklaşım**: Mümkün olduğunca WebSocket kullanımı
- ✅ **Tek pozisyon garantisi**: Aynı anda sadece bir açık pozisyon
- ✅ **Otomatik TP/SL yönetimi**: Take Profit ve Stop Loss otomatik kontrolü (komisyon dahil)
- ✅ **Risk yönetimi**: Leverage, bakiye, pozisyon boyutu kontrolleri
- ✅ **Güvenilirlik**: Hata yönetimi, reconnect, state senkronizasyonu
- ✅ **Modüler yapı**: Kolay genişletilebilir, bakımı kolay
- ✅ **Memory leak önleme**: Cleanup task'ları
- ✅ **Performance optimizasyonu**: Event sampling, cache-first, early exit

Bot, production ortamında kullanıma hazırdır ve sürekli iyileştirmeler yapılabilir.

---

**Dokümantasyon Versiyonu**: 2.0  
**Son Güncelleme**: 2024  
**Kapsam**: Tüm dosyalar incelendi ve detaylı analiz yapıldı
