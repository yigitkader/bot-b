# Q-MEL Implementation Checklist

## ✅ Tamamlanan Özellikler

### 1. Feature Extraction (Veri → Özellik)
- ✅ Order Flow Imbalance (OFI) hesaplama
- ✅ Microprice hesaplama
- ✅ Spread velocity (d(spread)/dt)
- ✅ Likidite basıncı (LP = D_ask / D_bid)
- ✅ Kısa vade volatilite (σ_1s, σ_5s) - EWMA
- ✅ Cancel/Trade oranı tracking
- ✅ OI delta (30s window)
- ✅ Funding rate tracking
- ✅ MarketState vektörü (10-20 boyut)

### 2. Edge Estimation (Alpha Gate)
- ✅ Yön olasılığı modeli (lojistik regresyon)
- ✅ Online kalibre (gradient descent)
- ✅ Beklenen değer (EV) hesaplama (LONG/SHORT)
- ✅ EV threshold kontrolü (α gate)
- ✅ Rejim filtresi entegrasyonu

### 3. Dinamik Marjin Parçalama (DMA)
- ✅ Clipped-Kelly risk payı (f* = clip(EV/V, f_min, f_max))
- ✅ Min 10, max 100 USDC kuralı
- ✅ Parçalama mantığı (E≥100 → 100+40, E<100 → tek blok)
- ✅ Variance estimation
- ✅ `calculate_margin_chunks()` method

### 4. Auto-Risk Governor (ARG)
- ✅ Liquidation güvenliği (L ≤ α·d_stop/MMR)
- ✅ Volatilite tabanlı klips (L ← min(L, β·T/σ_1s))
- ✅ Günlük drawdown koruması (DD ↑ ⇒ L ↓)
- ✅ `calculate_leverage()` method

### 5. Execution Optimizer (EXO)
- ✅ Maker/Taker kararı (edge decay vs queue wait)
- ✅ Fill olasılığı modeli
- ✅ Slippage tahmini (C_slip ≈ g(depth, size, latency))
- ✅ Slippage testi (EV - C_slip - fees > δ)
- ⚠️ Child order splitting (TWAP) - main.rs'de implement edilebilir

### 6. Pozisyon Yönetimi
- ✅ TP/SL mikroyapı (T tick, S ≤ 1.5T)
- ⚠️ Time-out kill (t > t_max) - position_manager.rs'de mevcut
- ⚠️ Partial close - position_manager.rs'de mevcut
- ⚠️ Hedge kuralı - futures hedge mode ile destekleniyor

### 7. Anomali & Rejim Kontrolü
- ✅ Anomali dedektörü (Cancel/Trade spike, volatility spike)
- ✅ Rejim sınıflandırıcı (Normal/Frenzy/Drift)
- ✅ Global PAUSE mekanizması (30-120 sn)
- ✅ Regime-based trading rules

### 8. Online Öğrenme (Bandit)
- ✅ Thompson Sampling/UCB bandit
- ✅ Parametre kolları (T, S, t_max, maker/taker eşiği)
- ✅ Ödül tracking (net USDC)
- ✅ Arm selection ve update
- ⚠️ EW decay (1-3 saat) - eklenebilir

### 9. Günlük Governance & Stop Kuralları
- ✅ Daily PnL tracking
- ✅ Daily drawdown tracking
- ⚠️ Daily loss limit (-R_day) - main.rs'de kontrol edilebilir
- ⚠️ Profit lock - main.rs'de implement edilebilir
- ⚠️ Korelasyon kontrolü - eşzamanlı işlemler için eklenebilir

### 10. Strategy Integration
- ✅ Strategy trait implementasyonu
- ✅ symbol_discovery.rs'de "qmel" seçimi
- ✅ Config parametreleri
- ✅ on_tick() implementation
- ✅ get_trend_bps(), get_volatility(), get_ofi_signal()

## ⚠️ Main.rs'de Entegre Edilmesi Gerekenler

1. **Q-MEL'e özel pozisyon yönetimi**: Timeout kill, partial close logic
2. **Daily governance**: Loss limit, profit lock
3. **Trade result tracking**: `update_with_trade_result()` çağrısı
4. **DMA kullanımı**: Gerçek equity ile margin chunk hesaplama
5. **ARG kullanımı**: Leverage hesaplama ve uygulama

## 📝 Kullanım

Config'de `strategy.type: "qmel"` olarak ayarla ve Q-MEL algoritması aktif olur.

## 🔧 İyileştirme Önerileri

1. EW decay ekle (bandit için)
2. VaR hesaplama ekle (eşzamanlı işlem limiti için)
3. Child order splitting (TWAP) implement et
4. Korelasyon kontrolü ekle
5. Profit lock mekanizması ekle



Q-MEL: Micro-Edge Trading Algoritması (kod değil, saf algoritma)
0) Hedef ve Kısıtlar
* Amaç: Çok küçük ama tekrarlı kârlar (≥ $0.5/işlem), düşük drawdown, yüksek çevrim.
* Marjin kuralın: Her işlem için min 10 – max 100 USDC senin cebinden çıkan (leverage hariç).
* Kaldıraç: 1–100x; yalnızca edge güvenilir olduğunda artar.
* Zaman ölçeği: 50–500 ms karar döngüsü, 1–10 sn pozisyon ömrü (time-out kill).

1) Veri → Özellik (Feature) Üretimi
   Her sembol için L2/L3 order book + trade akışıyla, her 100–200 ms:
* Order Flow Imbalance (OFI): [ \text{OFI}t=\frac{\Delta V{\text{bid}} - \Delta V_{\text{ask}}}{\Delta V_{\text{bid}}+\Delta V_{\text{ask}}+\epsilon} ]
* Microprice & Spread Dinamiği: [ \text{microprice}=\frac{P_{\text{ask}} \cdot D_{\text{bid}}+P_{\text{bid}}\cdot D_{\text{ask}}}{D_{\text{bid}}+D_{\text{ask}}},\quad v_\text{spread}=\frac{d(\text{spread})}{dt} ]
* Likidite Basıncı: (\text{LP}=D_{\text{ask}}/D_{\text{bid}}) (katmanlara göre ağırlıklı).
* Kısa Vade Volatilite: (\sigma_{1s}, \sigma_{5s}) (EWMA).
* Cancel/Trade Oranı: ani spoof/pull tespiti.
* Funding & OI delta (perps): (\Delta \text{OI}_{30s}), funding snapshot.
  Çıktı: tek bir durum vektörü (s_t) (10–20 boyut).

2) Edge Tahmini (Alpha Kapısı)
   Her 100–200 ms:
1. Yön olasılığı modeli (online kalibre): [ p_\uparrow = \Pr(\Delta P \ge +\tau \mid s_t),\quad p_\downarrow = 1 - p_\uparrow ]
    * (\tau): hedef tick (örn. 1–2 tick).
    * Model: lojistik + EW güncelleme veya Thompson Sampling (bandit) — ağır RL yok, latency dostu.
2. Beklenen değer (EV) testi – LONG için örnek [ \text{EV}{\text{long}} = p\uparrow \cdot T - p_\downarrow \cdot S - C_{\text{fees}} - C_{\text{slip}} ]
    * (T): hedef kâr (tick→USDC), (S): stop mesafesi (tick→USDC)
    * KOŞUL: (\text{EV} > \delta) (güvenlik tamponu; örn. $0.10)
3. Rejim filtresi:
    * Eğer (\sigma_{1s} \gg \sigma_{5s}) ve Cancel/Trade aşırı ise ⇒ PAUSE.
    * Spread geniş ve derinlik sığ ⇒ yalnızca maker.
      Edge Kapısı geçilmezse hiç işlem yok (sıkı “no trade” disiplini).

3) Marjin Parçalama & Boyutlandırma (DMA)
   Cebindeki serbest nakdi (E) (USDC) dinamik parçalara böl:
1. Optimum risk payı (clipped-Kelly): [ f^*=\text{clip}\left(\frac{\text{EV}}{V},, f_{\min},, f_{\max}\right) ]
    * (V): beklenen getiri varyansı (kısa pencerede).
    * Tipik: (f_{\min}=0.02,\ f_{\max}=0.15).
2. İşlem başına marjin: [ m=\text{clip}(f^*\cdot E,\ 10,\ 100) ]
    * Kuralın birebir: m alt sınır 10, üst sınır 100.
3. Parçalama mantığı:
    * Eğer (E \ge 100) ⇒ ilk işleme 100 ayır; kalan (E-100) için aynı prosedürü tekrarla.
    * Eğer (10 \le E < 100) ⇒ tek blok (m) (clip’lenmiş).
    * Eğer (E < 10) ⇒ işlem yok.
    * Örnek: (E=140) ⇒ İşlem#1: 100, kalan 40 ⇒ İşlem#2: 40 (clip ile ≥10).
4. Eşzamanlı işlem limiti:
    * Eşzamanlı açık işlem sayısı (N) için: [ \sum_{i=1}^{N} \text{VaR}{i,\ 99%} \le R{\text{day}} ] (R_{\text{day}}): günlük risk bütçesi (ör. equity’nin %2–3’ü).

4) Kaldıraç Seçimi (ARG – Auto-Risk Governor)
   Kaldıracı stop mesafesi + bakım marjı + volatilite ile sınırla:
1. Liquidation güvenliği: [ L \le \frac{\alpha \cdot d_{\text{stop}}}{\text{MMR}} ]
    * (d_{\text{stop}}): giriş-stop yüzdesi, MMR: maintenance margin rate,
    * (\alpha \in [0.3, 0.6]) güvenlik katsayısı.
2. Volatilite tabanlı klips: [ L \leftarrow \min\left(L,\ \beta\cdot\frac{T}{\sigma_{1s}}\right),\quad \beta \in [0.5,1.5] ]
3. Günlük drawdown koruması: DD ↑ ⇒ (L) kademeli azalt (ör. -1 DD adımı = ×0.7).
   Sonuç: yüksek edge + düşük risk anında 50–100x mümkün, aksi halde 5–10x’e düşer.

5) Emir Türü ve Yerleşimi (EXO)
   Hedef: Slippage < Edge.
1. Taker mi Maker mı?
    * Fill olasılığı modeli (\Pr(\text{fill} \mid \text{queue}, \text{cancels})).
    * Eğer (\text{edge decay yarı ömrü} < \text{tahmini queue bekleme}) ⇒ taker.
    * Aksi ⇒ maker, microprice ± 0–1 tick’e pasifle.
2. Beklenen slippage: [ C_{\text{slip}} \approx g(\text{depth at price},\ \text{size},\ \text{latency}) ] EV testi (\Rightarrow) (\text{EV} - C_{\text{slip}} - \text{fees} > \delta) değilse iptal.
3. Child order’lar: Büyük boyutları TWAP kısa patikası ile böl (250–500 ms aralık).

6) Pozisyon Yönetimi
* TP/SL mikroyapı:
    * TP: (T) tick (örn. 1–2 tick = $0.4–$0.8).
    * SL: (S) tick (genelde (S \le 1.5T)).
* Time-out kill: (t>t_{\max}) (örn. 5–10 sn) ve PnL ≈ 0 ⇒ kapat.
* Partial close: spread genişlerse %50 kapat, kalanını time-out’a bağla.
* Hedge kuralı: Ters sinyal kuvvetliyse netlemek için karşı yön micro-hedge (aynı sembol/başka venue).

7) Anomali & Rejim Kontrolü
* Anomali dedektörü: Cancel/Trade spike, tek-tıkta >n% price jump, OI şoku ⇒ global PAUSE (30–120 sn).
* Rejim sınıflayıcı:
    * Normal: spread stabil, derinlik yeterli ⇒ standart kurallar.
    * Frenzy: spread oynak, derinlik sığ ⇒ yalnız maker + düşük L.
    * Drift: düşük volatilite ⇒ hedef tick (T) ↓, bekleme ↑.

8) Online Öğrenme / Ağırlık Ayarı (Bandit)
   İşlem sonrası anında güncelle:
* Parametre kolları: ({T,S,\ t_{\max},\ \text{maker/taker eşiği}})
* Ödül: net USDC (fees & slip düşülmüş).
* Algoritma: Thompson Sampling veya UCB: en iyi kâr/riski veren parametre kombinasyonlarına daha çok ağırlık.
* Decay: son 1–3 saat verisine EW ağırlık (market koşulu kayarsa uyum sağlar).

9) Günlük Gov & Stop Kuralları
* Daily loss limit: (-R_{\text{day}}) aşılırsa tam dur.
* Profit lock: hedef kâr aşıldığında frekans ↓, L ↓ (kârı koru).
* Korelasyon kontrolü: eşzamanlı işlemler arası getiri korelasyonu ↑ ise N eşzamanlı ↓.

10) Algoritmanın Adım Adım Akışı (özet)
1. Veriyi çek → (s_t) üret (OFI, spread, derinlik, σ, OI…).
2. Edge kapısı: (p_\uparrow) tahmin et ⇒ (\text{EV}) hesapla ⇒ (\text{EV}>\delta) ?
3. Regim/Anomali kontrolü: uygunsa devam, değilse bekle.
4. DMA: (m=\text{clip}(f^*E,10,100)), blokla (100’lük, sonra 40’lık gibi).
5. ARG: (L) seç (stop, MMR, σ).
6. EXO: maker/taker kararı; child-order planı; slippage testi.
7. Pozisyonu aç: TP/SL/time-out ayarla.
8. Yönet: partial close, hedge, time-out kill.
9. Kapat: net PnL hesapla; bandit/RL güncelle.
10. Risk governance: günlük limitler ve soğuma.

11) Parametre Başlangıç Önerileri (kalibrasyon için)
* (\delta) (EV tamponu): $0.10–$0.20
* (T) (tick hedefi): 1–2 tick (pair’e göre $0.4–$1.0)
* (S): (1.0\text{–}1.5\times T)
* (t_{\max}): 5–8 sn
* (R_{\text{day}}): özsermayenin %2–3’ü
* (f_{\min}, f_{\max}): 2%–15%
* (\alpha=0.5,\ \beta=1.0)

12) Neden “modern” ve işe yarar?
* Fiyat tahmini yerine mikro-yapı avantajı: edge, kitap dengesinden gelir.
* Pozitif EV filtresi + slippage/fee ön-kontrol: kötü işlemler açılmaz.
* Dinamik marjin parçalama: min 10 / max 100 kuralını her döngüde optimal uygular.
* Uyarlanır bandit: parametreler piyasaya göre canlı optimize edilir.
* Kaldıraç güvenliği: stop/vol/likidasyon temelli, “100x her yerde” değil.
