# Test Kılavuzu

Bu dokümanda tüm testlerin nasıl çalıştırılacağı açıklanmaktadır.

## Test Kategorileri

### 1. Unit Testler (Hızlı - Mock Data Yok)
**Lokasyon:** `src/` dosyaları içindeki `#[cfg(test)]` modülleri

**Çalıştırma:**
```bash
# Tüm unit testleri çalıştır
cargo test --lib

# Belirli bir modülün testlerini çalıştır
cargo test --lib qmel::tests

# Belirli bir testi çalıştır
cargo test --lib test_feature_extractor_new
```

**Özellikler:**
- ✅ Hızlı çalışır (internet bağlantısı gerekmez)
- ✅ Sadece algoritma mantığını test eder
- ✅ Mock/dummy data kullanmaz (sadece algoritma testleri)

---

### 2. Compile Testler (Hızlı - Sadece Derleme Kontrolü)
**Lokasyon:** `tests/compile_test.rs`

**Çalıştırma:**
```bash
# Tüm compile testlerini çalıştır
cargo test --test compile_test

# Belirli bir compile testini çalıştır
cargo test --test compile_test test_qmel_modules
```

**Özellikler:**
- ✅ Çok hızlı (sadece derleme kontrolü)
- ✅ Modüllerin doğru şekilde derlendiğini kontrol eder
- ✅ Type safety kontrolü yapar

---

### 3. Integration Testler (Gerçek Binance API Gerektirir)
**Lokasyon:** `tests/backtest.rs` ve `tests/trending_success_test.rs`

**ÖNEMLİ:** Bu testler `#[ignore]` ile işaretlenmiştir çünkü:
- Gerçek Binance API'ye bağlanır
- İnternet bağlantısı gerektirir
- Rate limiting'e dikkat edilmelidir

#### 3.1. Trending Success Test
**Dosya:** `tests/trending_success_test.rs`

**Çalıştırma:**
```bash
# Trending başarı testini çalıştır (gerçek Binance verileri ile)
cargo test --test trending_success_test test_trending_success_with_real_binance_data -- --ignored

# Veya tüm ignored testleri çalıştır
cargo test --test trending_success_test -- --ignored
```

**Ne Yapar:**
- Binance API'den gerçek kline verileri çeker (BTCUSDT, ETHUSDT, SOLUSDT)
- Trending modülünün sinyal doğruluğunu test eder
- Win rate, long/short başarı oranlarını hesaplar

**Çıktı Örneği:**
```
🧪 Testing Trending Module Success Rate with Real Binance Data
================================================================

📊 Testing symbol: BTCUSDT
  ✅ Fetched 200 klines from Binance
  📈 Converted to 200 market ticks
  ✅ Signal #1: LONG @ $50000.00, next price: $50050.00 (+0.10%)
  ...

  📊 Results for BTCUSDT:
     Total signals generated: 15
     Correct signals: 9 (60.00%)
     Incorrect signals: 6 (40.00%)
     ✅ Win rate test passed: 60.00% >= 45%
```

#### 3.2. Backtest Testleri
**Dosya:** `tests/backtest.rs`

**Çalıştırma:**
```bash
# Tüm backtest testlerini çalıştır (gerçek Binance verileri ile)
cargo test --test backtest -- --ignored

# Belirli bir backtest testini çalıştır
cargo test --test backtest test_strategy_with_binance_data -- --ignored

# Multi-symbol backtest
cargo test --test backtest test_strategy_with_multiple_symbols -- --ignored

# Point-in-time backtest
cargo test --test backtest test_point_in_time_backtest -- --ignored

# Full integration test (tüm modüller)
cargo test --test backtest test_full_integration_with_real_data -- --ignored
```

**Testler:**
1. **`test_strategy_with_multiple_symbols`**: Birden fazla sembol ile backtest
2. **`test_strategy_with_binance_data`**: Tek sembol (BTCUSDT) ile backtest
3. **`test_point_in_time_backtest`**: Point-in-time validation testi
4. **`test_full_integration_with_real_data`**: Tüm modüllerin entegrasyon testi

**Ne Yapar:**
- Gerçek Binance API'den kline verileri çeker
- Strateji performansını test eder
- Win rate, Sharpe ratio, max drawdown, profit factor hesaplar
- Tüm modüllerin birlikte çalışmasını test eder

---

## Tüm Testleri Çalıştırma

### Senaryo 1: Hızlı Testler (İnternet Gerektirmez)
```bash
# Unit testler + compile testler
cargo test --lib
cargo test --test compile_test
```

### Senaryo 2: Tüm Testler (İnternet Gerektirir)
```bash
# Tüm testleri çalıştır (ignored testler dahil)
cargo test -- --ignored

# Veya ayrı ayrı
cargo test --lib
cargo test --test compile_test
cargo test --test trending_success_test -- --ignored
cargo test --test backtest -- --ignored
```

### Senaryo 3: Belirli Bir Test Dosyası
```bash
# Sadece compile testleri
cargo test --test compile_test

# Sadece trending success testleri
cargo test --test trending_success_test -- --ignored

# Sadece backtest testleri
cargo test --test backtest -- --ignored
```

---

## Test Çıktılarını Görüntüleme

### Detaylı Çıktı
```bash
# Verbose mode (tüm println! çıktılarını göster)
cargo test --test trending_success_test -- --ignored --nocapture

# Veya
cargo test --test backtest -- --ignored --nocapture
```

### Sadece Test Sonuçları
```bash
# Quiet mode (sadece sonuçlar)
cargo test --lib -q
```

### Belirli Bir Test
```bash
# Belirli bir testi çalıştır
cargo test --test trending_success_test test_trending_success_with_real_binance_data -- --ignored --nocapture
```

---

## Test Gereksinimleri

### Unit ve Compile Testler
- ✅ İnternet bağlantısı gerekmez
- ✅ API key gerekmez
- ✅ Hızlı çalışır

### Integration Testler (Ignored)
- ⚠️ İnternet bağlantısı gerekir
- ⚠️ Binance API erişimi gerekir
- ⚠️ Rate limiting'e dikkat edilmelidir
- ⚠️ API key gerekebilir (bazı testler için)

---

## Örnek Test Senaryoları

### 1. Günlük Hızlı Kontrol
```bash
# Sadece unit ve compile testleri (hızlı)
cargo test --lib && cargo test --test compile_test
```

### 2. Haftalık Tam Test
```bash
# Tüm testleri çalıştır (gerçek API verileri ile)
cargo test -- --ignored --nocapture
```

### 3. Belirli Bir Modülü Test Et
```bash
# Sadece trending modülünü test et
cargo test --test trending_success_test -- --ignored --nocapture
```

### 4. CI/CD Pipeline İçin
```bash
# Hızlı testler (ignored testler olmadan)
cargo test --lib --test compile_test

# Integration testler (opsiyonel, manuel olarak çalıştırılabilir)
# cargo test -- --ignored
```

---

## Sorun Giderme

### Test Başarısız Olursa

1. **İnternet bağlantısını kontrol edin:**
   ```bash
   curl https://fapi.binance.com/fapi/v1/ping
   ```

2. **Rate limiting hatası alırsanız:**
   - Testleri arka arkaya çalıştırmayın
   - Birkaç saniye bekleyin

3. **API key hatası alırsanız:**
   - Bazı testler API key gerektirir
   - `config.yaml` dosyasını kontrol edin

4. **Compile hatası alırsanız:**
   ```bash
   cargo clean
   cargo build
   cargo test --lib
   ```

---

## Test Metrikleri

### Trending Success Test
- **Win Rate**: %45+ olmalı (rastgele seçimden daha iyi)
- **Long/Short Ayrı Başarı Oranları**: Her ikisi de ölçülür
- **Ortalama Fiyat Değişimi**: Sinyal sonrası fiyat hareketi

### Backtest Testleri
- **Win Rate**: Kazanan işlemlerin yüzdesi
- **Sharpe Ratio**: Risk-ayarlı getiri
- **Max Drawdown**: Maksimum düşüş
- **Profit Factor**: Toplam kazanç / Toplam kayıp

---

## Notlar

- ⚠️ **Ignored testler gerçek API çağrıları yapar** - Rate limiting'e dikkat edin
- ✅ **Tüm testler gerçek Binance verileri kullanır** - Mock/dummy data yok
- 📊 **Test sonuçları her çalıştırmada farklı olabilir** - Canlı piyasa verileri
- 🔒 **Production kodunda mock data yok** - Tüm testler gerçek verilerle çalışır

