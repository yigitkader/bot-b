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

#### 2.1. Manipülasyon Fırsat Tespiti (Opportunity Detection)

Strateji, piyasada manipülasyon fırsatlarını tespit eder ve bunları avantaja çevirir:

1. **Flash Crash/Pump Detection:**
   - Ani fiyat değişimleri tespit edilir (150+ bps = 1.5%+)
   - Flash crash (fiyat düşüşü) → LONG fırsatı (dip alım)
   - Flash pump (fiyat yükselişi) → SHORT fırsatı (tepe satış)

2. **Wide Spread Arbitrage:**
   - Spread 50-200 bps arası → Market maker fırsatı
   - Spread > 200 bps → Risk çok yüksek, işlem yapılmaz

3. **Volume Anomaly Trend:**
   - Volume 3x veya daha fazla arttıysa → Trend takibi fırsatı
   - Trend yönüne göre LONG veya SHORT pozisyon

4. **Momentum Manipulation Reversal:**
   - Fake breakout tespiti: Fiyat yükseldi ama volume düştü
   - Ters yönde pozisyon alınır (fake breakout → reversal)

5. **Spoofing Detection:**
   - Büyük emir duvarı tespit edilir (2x+ likidite değişimi)
   - Bid wall varsa → Ask tarafında işlem (duvar kalkınca fiyat düşer)
   - Ask wall varsa → Bid tarafında işlem (duvar kalkınca fiyat yükselir)

6. **Liquidity Withdrawal:**
   - Likidite %50+ azaldıysa → Spread arbitrajı fırsatı
   - Her iki tarafta market maker olarak işlem yapılır

#### 2.2. Mikro-Yapı Sinyalleri (Microstructure Signals)

1. **Microprice Hesaplama:**
   - Volume-weighted mid price: `mp = (ask * bid_vol + bid * ask_vol) / (bid_vol + ask_vol)`
   - Mid price yerine microprice kullanılır (daha iyi tahmin)

2. **Order Book Imbalance:**
   - `imbalance = (bid_vol - ask_vol) / (bid_vol + ask_vol)`
   - Pozitif imbalance (bid heavy) → Fiyat yukarı baskı
   - Negatif imbalance (ask heavy) → Fiyat aşağı baskı

3. **EWMA Volatilite:**
   - Exponential Weighted Moving Average volatilite
   - Formül: `σ²_t = λ·σ²_{t-1} + (1-λ)·r²_t`
   - λ = 0.95 (decay factor)
   - Adaptif spread hesaplamasında kullanılır

4. **Order Flow Imbalance (OFI):**
   - Mid price momentum'a göre OFI sinyali
   - Pozitif OFI → Buy pressure
   - Negatif OFI → Sell pressure
   - Adverse selection filtrelemesinde kullanılır

#### 2.3. Hedef Envanter Yönetimi (Target Inventory)

1. **Funding Rate Bias:**
   - Pozitif funding rate → Long bias (pozitif envanter hedefi)
   - Negatif funding rate → Short bias (negatif envanter hedefi)

2. **Trend Bias:**
   - Yukarı trend → Long bias
   - Aşağı trend → Short bias
   - Trend'in %50'si kadar etkili

3. **Hedef Envanter Hesaplama:**
   - `target_ratio = sigmoid(combined_bias)`
   - `target_inventory = inv_cap * target_ratio`

4. **Envanter Kararı:**
   - Mevcut envanter < hedef → Sadece bid (al)
   - Mevcut envanter > hedef → Sadece ask (sat)
   - Hedefe yakınsa (%10 threshold) → Her iki taraf (market making)

#### 2.4. Adaptif Spread Hesaplama

1. **Base Spread:**
   - `base_spread_bps = max(a * sigma + b * inv_bias, 0.0001)`
   - `a`: Volatilite katsayısı (120.0)
   - `b`: Envanter bias katsayısı (40.0)

2. **Adaptif Spread:**
   - `adaptive = max(min_spread, c₁·σ + c₂·|OFI|)`
   - `c₁ = 0.5` (volatilite katsayısı)
   - `c₂ = 0.5` (OFI katsayısı)
   - Imbalance'a göre ekstra ayarlama (+max 5 bps)

3. **Risk Düzeltmeleri:**
   - Likidasyon mesafesi < 300 bps → Spread %50 genişletilir
   - Spread > max_spread_bps (100 bps) → İşlem yapılmaz

#### 2.5. Fiyat Hesaplama

1. **Fiyatlama Bazı:**
   - Microprice kullanılır (mid price yerine)

2. **Skew Hesaplama:**
   - Funding skew: `funding_rate * 100 bps`
   - Envanter skew: `inv_direction * inv_bias * 20 bps`
   - Imbalance skew: `imbalance * 10 bps`

3. **Fiyat Formülleri:**
   - `bid_px = microprice * (1 - half_spread - funding_skew - inv_skew - imb_skew)`
   - `ask_px = microprice * (1 + half_spread + funding_skew + inv_skew + imb_skew)`

#### 2.6. Adverse Selection Filtreleme

1. **OFI ve Momentum Kontrolü:**
   - OFI > 0.5 ve momentum yukarı → Ask riskli (ask'i geri çek)
   - OFI < -0.5 ve momentum aşağı → Bid riskli (bid'i geri çek)

2. **Fırsat Modu:**
   - Manipülasyon fırsatı varsa → Adverse selection filtresi bypass edilir
   - Agresif pozisyon alınır (2.5x pozisyon boyutu)

#### 2.7. Miktar Hesaplama

1. **Normal Mod:**
   - `qty = base_size / mid_price`

2. **Fırsat Modu:**
   - `qty = base_size * opportunity_multiplier (2.5x) / mid_price`
   - Manipülasyon fırsatı varsa pozisyon boyutu artırılır

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
   - Context hazırlanır (orderbook, inventory, risk metrikleri, mark price, funding rate)
   - Strateji `on_tick()` çağrılır:
     - Manipülasyon fırsatları tespit edilir
     - Mikro-yapı sinyalleri hesaplanır (microprice, OFI, volatilite, imbalance)
     - Hedef envanter belirlenir (funding rate + trend bazlı)
     - Adaptif spread hesaplanır
     - Adverse selection filtresi uygulanır
     - Bid/ask quotes üretilir

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

4. **Adaptive Spread:** Volatilite, OFI, imbalance ve envanter bias'a göre dinamik spread ayarlanır

5. **Funding Rate Skew:** Futures'ta funding rate'a göre fiyat ayarlanır ve hedef envanter belirlenir

6. **Min Notional Learning:** Exchange'in min notional gereksinimi öğrenilir ve uyum sağlanır

7. **Balance Optimization:** Kalan bakiye ile ekstra emirler yerleştirilir

8. **WebSocket Real-time Updates:** Emir durumları gerçek zamanlı takip edilir

9. **Manipülasyon Fırsat Tespiti:** 7 farklı manipülasyon tipi tespit edilir ve avantaja çevrilir:
   - Flash crash/pump detection
   - Wide spread arbitrage
   - Volume anomaly trend following
   - Momentum manipulation reversal
   - Spoofing detection
   - Liquidity withdrawal opportunities

10. **Mikro-Yapı Sinyalleri:** Gelişmiş fiyatlama için mikro-yapı sinyalleri kullanılır:
    - Microprice (volume-weighted mid)
    - Order book imbalance
    - EWMA volatilite
    - Order Flow Imbalance (OFI)

11. **Hedef Envanter Yönetimi:** Funding rate ve trend'e göre dinamik hedef envanter belirlenir

12. **Adverse Selection Filtreleme:** OFI ve momentum'a göre riskli taraflar filtrelenir

13. **Fırsat Modu:** Manipülasyon fırsatı tespit edildiğinde pozisyon boyutu 2.5x artırılır ve adverse selection filtresi bypass edilir

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

