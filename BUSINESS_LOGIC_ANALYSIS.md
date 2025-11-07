# Business Logic Analizi - Cryptocurrency Trading Bot

## Genel Bakış

Bu proje, Binance borsasında (Spot ve Futures) otomatik market making yapan bir kripto para trading botudur. Rust programlama dili ile yazılmış, modüler bir mimariye sahiptir.

## Ana İş Mantığı

### 1. Bot Başlatma ve Konfigürasyon

**Konfigürasyon Yükleme (`config.yaml`):**
- Sembol seçimi: Manuel sembol listesi veya otomatik keşif (auto_discover_quote)
- İşlem modu: Spot veya Futures
- Emir limitleri: `max_usd_per_order` (örn: 100 USD), `min_usd_per_order` (örn: 10 USD)
- Kaldıraç: Futures için (örn: 5x)
- Fiyat/Adım hassasiyeti: `price_tick`, `qty_step`
- Binance API anahtarları

**Sembol Seçimi:**
- Manuel sembol listesi varsa kullanılır
- `auto_discover_quote=true` ise, belirtilen quote asset (örn: USDC) ile tüm işlem yapılabilir semboller otomatik bulunur
- USD-stablecoin grubu (USDT, USDC, BUSD, vb.) birbirine eşdeğer kabul edilir
- Sadece "TRADING" durumundaki semboller seçilir
- Futures modunda sadece "PERPETUAL" kontratlar kabul edilir
- Başlangıç bakiyesi kontrolü yapılır (yetersiz bakiye olan semboller atlanır)

### 2. Strateji Modülü (Strategy)

**Dynamic Market Making (DynMm) Stratejisi:**

Her tick'te (her döngüde) şu adımlar gerçekleşir:

1. **Mid Price Hesaplama:**
   - Best bid ve best ask'ten mid price = (bid + ask) / 2

2. **Spread Hesaplama:**
   - Formül: `spread_bps = max(a * sigma + b * inv_bias, 0.0001)`
   - `a`: Volatilite katsayısı (örn: 120.0)
   - `b`: Envanter bias katsayısı (örn: 40.0)
   - `sigma`: Volatilite (şu an sabit 0.5)
   - `inv_bias`: Envanter bias = |inventory / inv_cap|

3. **Risk Düzeltmeleri:**
   - Likidasyon mesafesi < 300 bps ise spread %50 genişletilir
   - Funding rate'a göre fiyat skew'i uygulanır (funding pozitifse ask yukarı, bid aşağı)

4. **Fiyat Hesaplama:**
   - `bid_px = mid * (1 - half_spread - funding_skew)`
   - `ask_px = mid * (1 + half_spread + funding_skew)`

5. **Miktar Hesaplama:**
   - `base_size` (örn: 20 USD) / mid_price
   - Her iki taraf için aynı miktar

### 3. Risk Yönetimi (Risk)

**Risk Kontrolleri:**

1. **Drawdown Limiti:**
   - PnL geçmişinden maksimum drawdown hesaplanır
   - Drawdown > `dd_limit_bps` (örn: 2000 bps = %20) ise → **HALT** (durdur)

2. **Envanter Limiti:**
   - |inventory| > `inv_cap` (örn: 0.50) ise → **REDUCE** (azalt)

3. **Likidasyon Mesafesi:**
   - Likidasyon fiyatına mesafe < `min_liq_gap_bps` (örn: 300 bps = %3) ise → **REDUCE**

4. **Kaldıraç Limiti:**
   - Mevcut kaldıraç > `max_leverage` (örn: 10x) ise → **REDUCE**

**Risk Aksiyonları:**
- **OK**: Normal işlem
- **WIDEN**: Spread'i %0.1 genişlet
- **REDUCE**: Spread'i %0.5 genişlet
- **HALT**: Tüm emirleri iptal et, pozisyonu kapat, sembolü atla

### 4. Emir Yönetimi (Execution)

**Ana Döngü (Her tick_ms milisaniyede bir):**

1. **WebSocket Event İşleme:**
   - Order fill: Envanter güncelle (buy → +qty, sell → -qty)
   - Order canceled: Aktif emir listesinden çıkar

2. **Aktif Emirleri Temizle:**
   - Her tick'te mevcut emirler iptal edilir (cancel-replace pattern)
   - `max_order_age_ms` (örn: 10 saniye) geçmiş emirler "stale" olarak işaretlenir

3. **Piyasa Verilerini Çek:**
   - Best bid/ask fiyatları
   - Mevcut pozisyon (API'den)
   - Mark price (futures için)
   - Funding rate (futures için)

4. **Envanter Senkronizasyonu:**
   - WebSocket envanteri ile API pozisyonu karşılaştırılır
   - Fark varsa API'ye göre güncellenir

5. **PnL Takibi:**
   - Her tick'te PnL snapshot alınır: `equity = 1 + (mark_px - entry_px) * qty`
   - Son 1024 snapshot saklanır
   - Drawdown hesaplama için kullanılır

6. **Strateji Çağrısı:**
   - Context hazırlanır (orderbook, inventory, risk metrikleri)
   - Strateji `on_tick()` çağrılır → bid/ask quotes üretilir

7. **Risk Düzeltmeleri:**
   - Risk aksiyonuna göre spread genişletilir

8. **Kapasite Hesaplama:**

   **Spot:**
   - `buy_notional = min(max_usd_per_order, quote_free)`
   - `sell_notional = max_usd_per_order`
   - `sell_base = base_free`

   **Futures:**
   - `available_balance * leverage` = toplam kullanılabilir
   - `buy_notional = min(max_usd_per_order, total * leverage)`
   - `sell_notional = buy_notional`

   **İki Taraf Varsa:**
   - Her taraf için kapasite yarıya bölünür (toplam değişmez)

9. **Min Notional Kontrolü:**
   - Exchange'in min_notional gereksinimi varsa kontrol edilir
   - Kapasite min_notional'ın altındaysa tick atlanır
   - Min_notional > max_usd_per_order ise sembol kalıcı olarak devre dışı bırakılır

10. **Bakiye Kontrolü:**
    - Buy için: `buy_notional >= min_usd_per_order`
    - Sell için: `sell_notional >= min_usd_per_order` VE (spot için) `base_free * price >= min_usd_per_order`
    - Yetersizse ilgili taraf atlanır

11. **Miktar Kısıtlama:**
    - USD bazlı: `qty = min(strategy_qty, max_usd / price)`
    - Base bazlı (spot sell): `qty = min(qty, base_free)`
    - Step'e göre quantize edilir

12. **Emir Yerleştirme:**
    - Bid ve ask emirleri yerleştirilir
    - **Ekstra emirler:** Kalan bakiye/kapasite varsa ikinci emirler yerleştirilir
    - Spot: Kalan USD ile ikinci bid, kalan base ile ikinci ask
    - Futures: Kalan notional ile ikinci bid

13. **Min Notional Hatası Yönetimi:**
    - Emir yerleştirme hatası "below min notional" içeriyorsa:
      - Min notional değeri parse edilir ve saklanır
      - Min notional > max_usd_per_order ise sembol devre dışı
      - Aksi halde miktar min_notional'a göre yeniden hesaplanıp retry edilir

### 5. Binance Entegrasyonu (Exec/Binance)

**Spot İşlemleri:**
- `place_limit`: LIMIT_MAKER (post-only) veya LIMIT (GTC/IOC)
- Fiyat ve miktar exchange kurallarına göre quantize edilir
- Min notional kontrolü yapılır
- HMAC-SHA256 ile imzalanır

**Futures İşlemleri:**
- `place_limit`: LIMIT (GTX/GTC/IOC)
- `reduceOnly=true` ile pozisyon kapatma
- Mark price ve funding rate bilgisi alınır
- Likidasyon fiyatı pozisyon bilgisinde gelir

**Symbol Metadata:**
- ExchangeInfo'dan tick size, step size, min notional çekilir
- Cache'lenir (DashMap)

### 6. WebSocket Entegrasyonu (Data/Binance_WS)

**User Data Stream:**
- Binance user data stream'e bağlanır
- ListenKey oluşturulur ve 25 dakikada bir yenilenir
- Event'ler parse edilir:
  - `executionReport` (Spot): Order fill/cancel
  - `ORDER_TRADE_UPDATE` (Futures): Order fill/cancel
- Bağlantı koparsa otomatik yeniden bağlanır

### 7. Monitoring (Monitor)

**Prometheus Metrikleri:**
- Prometheus exporter başlatılır (port: 9000)
- Metrikler metrics crate ile toplanır

### 8. Backtest (Backtest)

- Şu an placeholder (boş)
- Gelecekte tick-replay ve latency modeli için tasarlanmış

## İş Akışı Özeti

```
1. Bot Başlatma
   ├─ Config yükle
   ├─ Binance'a bağlan
   ├─ Sembol metadata çek
   ├─ Sembol seç/filtrele
   └─ Her sembol için state oluştur

2. Ana Döngü (Her tick_ms)
   ├─ WebSocket event'leri işle (envanter güncelle)
   ├─ Her sembol için:
   │  ├─ Aktif emirleri iptal et
   │  ├─ Piyasa verilerini çek (bid/ask, pozisyon, mark price)
   │  ├─ Envanter senkronize et
   │  ├─ PnL snapshot al
   │  ├─ Risk kontrolü yap
   │  │  └─ HALT ise → pozisyonu kapat, atla
   │  ├─ Strateji çağrısı → quotes üret
   │  ├─ Risk düzeltmeleri uygula (spread genişlet)
   │  ├─ Kapasite hesapla (bakiye, leverage)
   │  ├─ Min notional kontrolü
   │  ├─ Miktar kısıtla ve quantize et
   │  └─ Emirleri yerleştir (bid, ask, ekstra emirler)
   └─ Döngü devam eder
```

## Önemli Özellikler

1. **Cancel-Replace Pattern:** Her tick'te emirler iptal edilip yeniden yerleştirilir (hızlı adaptasyon)

2. **Multi-Symbol Support:** Birden fazla sembol paralel işlenir

3. **Risk-Aware Trading:** Drawdown, envanter, likidasyon mesafesi kontrolü

4. **Adaptive Spread:** Envanter ve volatiliteye göre spread ayarlanır

5. **Funding Rate Skew:** Futures'ta funding rate'a göre fiyat ayarlanır

6. **Min Notional Learning:** Exchange'in min notional gereksinimi öğrenilir ve uyum sağlanır

7. **Balance Optimization:** Kalan bakiye ile ekstra emirler yerleştirilir

8. **WebSocket Real-time Updates:** Emir durumları gerçek zamanlı takip edilir

## Güvenlik ve Hata Yönetimi

- API hataları loglanır ve işlem atlanır
- WebSocket bağlantı kopmalarında otomatik yeniden bağlanma
- Envanter uyumsuzluklarında API'ye göre senkronizasyon
- Stale emirler otomatik iptal edilir
- Risk limitleri aşıldığında otomatik durdurma

## Performans Optimizasyonları

- Symbol rules cache'lenir
- DashMap ile thread-safe caching
- Async/await ile non-blocking I/O
- Quantization ile hassas hesaplamalar

