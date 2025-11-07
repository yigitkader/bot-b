# Akıllı Strateji Sistemi

## Genel Bakış

Bot artık otomatik karar verme yeteneğine sahip. Kendi başına long/short açma, ne zaman al/sat yapacağını belirleme ve envanter yönetimi yapabiliyor.

## Akıllı Karar Verme Mekanizması

### 1. Trend Analizi

**Nasıl Çalışır:**
- Son 10 fiyat kaydı tutulur
- İlk 5'in ortalaması ile son 5'in ortalaması karşılaştırılır
- Trend yukarıysa → Long bias (pozitif envanter hedefi)
- Trend aşağıysa → Short bias (negatif envanter hedefi)

**Formül:**
```
trend_bps = ((new_avg - old_avg) / old_avg) * 10000
```

### 2. Funding Rate Analizi

**Nasıl Çalışır:**
- Pozitif funding rate → Long pozisyon avantajlı → Pozitif envanter hedefi
- Negatif funding rate → Short pozisyon avantajlı → Negatif envanter hedefi

**Formül:**
```
funding_bias = funding_rate * 10000 (bps)
```

### 3. Hedef Envanter Hesaplama

**Kombine Bias:**
```
combined_bias = funding_bias + (trend_bps * 0.5)
```

**Hedef Envanter:**
```
target_ratio = combined_bias / (1 + |combined_bias|)  // -1 ile 1 arası
target_inventory = inv_cap * target_ratio
```

**Örnekler:**
- Funding +0.01% (100 bps), Trend +50 bps → combined = 125 bps → target_ratio ≈ 0.55 → %55 long
- Funding -0.01% (-100 bps), Trend -50 bps → combined = -125 bps → target_ratio ≈ -0.55 → %55 short
- Funding 0, Trend 0 → target_ratio = 0 → Nötr (market making)

### 4. Envanter Yönetimi ve Al/Sat Kararı

**Karar Mantığı:**

1. **Hedef Envantere Yakınsa (%10 threshold içinde):**
   - Market Making modu: Her iki taraf (bid + ask)
   - Spread'den kazanç sağlama

2. **Mevcut Envanter < Hedef Envanter:**
   - Sadece Bid (alış) yapılır
   - Long pozisyon oluşturma/artırma

3. **Mevcut Envanter > Hedef Envanter:**
   - Sadece Ask (satış) yapılır
   - Long pozisyon azaltma veya short pozisyon oluşturma

**Örnek Senaryolar:**

**Senaryo 1: Funding Pozitif, Trend Yukarı**
- Hedef: +0.3 (inv_cap'in %30'u long)
- Mevcut: 0.0
- Karar: Sadece Bid (long pozisyon oluştur)

**Senaryo 2: Funding Negatif, Trend Aşağı**
- Hedef: -0.4 (inv_cap'in %40'ı short)
- Mevcut: -0.2
- Karar: Sadece Bid (short pozisyon azalt) veya Ask (short artır) - hangisi daha yakınsa

**Senaryo 3: Hedef Envantere Ulaşıldı**
- Hedef: +0.2
- Mevcut: +0.18 (%10 threshold içinde)
- Karar: Her iki taraf (market making)

## Otomatik Karar Verme Özellikleri

### ✅ Long/Short Açma
- Funding rate ve trend'e göre otomatik long/short pozisyon hedefi
- Hedef envantere göre otomatik pozisyon alma/azaltma

### ✅ Ne Zaman Al/Sat
- Hedef envantere göre otomatik al/sat kararı
- Mevcut envanter hedeften uzaksa tek taraflı işlem
- Hedef envantere yakınsa market making (her iki taraf)

### ✅ Kendi Kararlarını Alma
- Trend analizi ile piyasa yönü tespiti
- Funding rate ile pozisyon optimizasyonu
- Envanter yönetimi ile otomatik dengeleme

## Avantajlar

1. **Otomatik Pozisyon Yönetimi:** Bot kendi başına long/short kararı veriyor
2. **Trend Takibi:** Piyasa yönüne göre pozisyon alıyor
3. **Funding Rate Optimizasyonu:** Funding rate'den kazanç sağlıyor
4. **Akıllı Envanter:** Hedef envantere göre otomatik dengeleme
5. **Risk Yönetimi:** Likidasyon riski ve drawdown kontrolü ile güvenli

## Loglama

Strateji kararları debug seviyesinde loglanıyor:
- `current_inv`: Mevcut envanter
- `target_inv`: Hedef envanter
- `trend_bps`: Trend analizi sonucu
- `funding_rate`: Funding rate
- `should_bid`: Bid yapılacak mı?
- `should_ask`: Ask yapılacak mı?

## Gelecek İyileştirmeler

1. **Daha Gelişmiş Trend Analizi:** EMA, MACD gibi teknik indikatörler
2. **Volatilite Analizi:** Volatiliteye göre pozisyon boyutu ayarlama
3. **Zaman Bazlı Kararlar:** Funding time'a göre pozisyon değişikliği
4. **Multi-Timeframe Analiz:** Farklı zaman dilimlerinde trend analizi
5. **Machine Learning:** Geçmiş verilerden öğrenme

