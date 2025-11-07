# Günlük Kazanç Optimizasyon Planı
## Hedef: Minimum 50 işlem/gün, Minimum 2 USD/gün

### Mevcut Durum Analizi

**Mevcut Ayarlar:**
- `base_size`: 20 USD
- `max_usd_per_order`: 100 USD
- `min_usd_per_order`: 10 USD
- `cancel_replace_interval_ms`: 1000ms (1 saniye)
- `a`: 120.0, `b`: 40.0 (spread parametreleri)
- `min_spread_bps`: 1.0

**Hesaplama:**
- 50 işlem/gün = ~2.08 işlem/saat (24 saat çalışırsa)
- 2 USD/gün = işlem başına ortalama **0.04 USD** kazanç
- Spread'den kazanç örneği: 20 USD işlem, 2 bps spread = 20 * 0.0002 = **0.004 USD** (çok düşük)
- **Gerekli:** Daha fazla işlem VEYA daha geniş spread VEYA daha büyük pozisyonlar

### Optimizasyon Stratejisi

#### 1. İşlem Sıklığını Artırma
- **Tick interval'i düşür:** 1000ms → **500ms** (2x daha sık güncelleme)
- **Max order age'i düşür:** 10000ms → **5000ms** (daha hızlı yenileme)
- **Adverse selection filtresini gevşet:** Daha fazla işlem için risk toleransı artır

#### 2. Spread Optimizasyonu
- **Min spread'i optimize et:** 1 bps → **2-3 bps** (daha fazla kazanç, hala rekabetçi)
- **Base spread parametrelerini ayarla:** `a` ve `b` değerlerini düşür (daha agresif fiyatlama)
- **Adaptif spread'i optimize et:** Volatilite ve OFI katsayılarını ayarla

#### 3. Pozisyon Boyutu Optimizasyonu
- **Base size'i artır:** 20 USD → **30-40 USD** (daha fazla kazanç/işlem)
- **Min USD per order'i optimize et:** 10 USD → **15 USD** (daha büyük işlemler)

#### 4. Çoklu Sembol Stratejisi
- **Daha fazla sembol işle:** Auto-discovery ile maksimum sembol sayısını artır
- **Her sembol için ayrı optimizasyon:** Sembol bazında spread ve boyut ayarları

### Önerilen Config Değişiklikleri

```yaml
# Daha agresif işlem için
exec:
  cancel_replace_interval_ms: 500  # 1000 → 500 (2x daha sık)
  max_order_age_ms: 5000            # 10000 → 5000 (daha hızlı yenileme)

strategy:
  a: 80.0   # 120.0 → 80.0 (daha agresif spread)
  b: 30.0   # 40.0 → 30.0 (daha agresif spread)
  base_size: "30.0"  # 20.0 → 30.0 (daha büyük pozisyonlar)

# Daha fazla işlem için
max_usd_per_order: 100.0  # Aynı kalabilir
min_usd_per_order: 15.0   # 10.0 → 15.0 (daha büyük minimum)
```

### Beklenen Sonuçlar

**İşlem Sıklığı:**
- Tick interval 500ms → Günde ~172,800 tick (24 saat)
- Her sembol için daha sık quote güncelleme
- **Hedef: 50+ işlem/gün** ✅

**Kazanç Hesaplaması:**
- Ortalama işlem: 30 USD
- Ortalama spread: 2.5 bps
- İşlem başına kazanç: 30 * 0.00025 = **0.0075 USD**
- 50 işlem: 50 * 0.0075 = **0.375 USD** (henüz 2 USD değil)

**Daha Fazla Kazanç İçin:**
1. **Daha geniş spread:** 2.5 bps → **4-5 bps** (rekabetçi kalırken daha fazla kazanç)
2. **Daha büyük pozisyonlar:** 30 USD → **40-50 USD**
3. **Daha fazla sembol:** 5-10 sembol aktif işlem
4. **Fırsat modu:** Manipülasyon fırsatlarında 2.5x multiplier

### Risk Yönetimi

- **Min liq gap:** 300 bps (aynı kalmalı)
- **Drawdown limit:** 2000 bps (aynı kalmalı)
- **Inventory cap:** 0.50 (aynı kalmalı)

### İzleme Metrikleri

1. **Günlük işlem sayısı**
2. **Ortalama spread (bps)**
3. **Ortalama işlem boyutu (USD)**
4. **Günlük toplam kazanç (USD)**
5. **Fill rate (işlem başarı oranı)**

### Sonraki Adımlar

1. Config'i optimize et
2. Test ortamında çalıştır (1-2 saat)
3. Metrikleri izle
4. Gerekirse fine-tune yap
5. Production'a al

