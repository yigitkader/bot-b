Tam “kisaca, ama sağlam mimari” moduna geçiyorum. Overengineering yok, ama yapboz da değil. 👇

BINANCE ONLY FUTURES TRADING BOT PROJECT !!
  ! OLABILDIGINCE WEBSOCKET KULLANICAZ. NE KADAR AZ REST API O KADAR IYI. BINANCE NEREDEYSE BU ISLEMLERIN HEPSINI WEBSOCKET ILE DESTEKLIYOR


0. Genel Mimarinin Özeti
* Tek process, içinde 5 ana servis (senin modüllerin) + çok küçük bir ortak state.
* Tüm dış dünya (WS/REST) sadece CONNECTION’dan geçer.
* Modüller arası iletişim: in-memory event bus / message queue (channel, queue vs. – implementation detayı fark etmez).
* “Bir anda tek pozisyon / open order” garantisi: sadece ORDERING state’i değiştirebilir.

1. CONNECTION (merkez)
   Görev:
* Exchange WS & REST tek kapı.
* Market data stream, order/position update stream, balance stream hepsi buradan çıkar.
* Ratelimit & reconnect burada.
  Dışarıya verdiği şeyler:
* onMarketTick eventi (SUB: TRENDING, FOLLOW_ORDERS)
* onOrderUpdate / onPositionUpdate eventi (SUB: FOLLOW_ORDERS, ORDERING)
* Basit API:
   * sendOrder(command) → ORDERING kullanır
   * fetchBalance() → BALANCE kullanır
     Başka hiçbir modül doğrudan exchange’e dokunmaz.

2. TRENDING
   Görev:
* Sadece trend analizi yapar, trade kararı üretir.
* Market verisini CONNECTION’ın WS streaminden alır.
  Input:
* onMarketTick(symbol, price, volume, …)
  Output:
* Event: TradeSignal (örnek payload): { side: LONG/SHORT, symbol, entryPrice, leverage, size, … }
*
Bu TradeSignal’ı ORDERING dinler. TRENDING hiçbir zaman order atmaz.
Ayrı thread’de çalışması:
* Sadece iç event bus’tan okur/yazar → kimseyi bloklamaz.

3. ORDERING
   Görev:
* Tek iş: emir açma / kapatma. Hiçbir trend veya PnL logic yok.
* Her zaman CONNECTION üzerinden emir gönderir.
  Dinledikleri:
* TradeSignal (TRENDING’den)
* CloseRequest (FOLLOW_ORDERS’dan)
* onPositionUpdate / onOrderUpdate (CONNECTION’dan – state senkron için)
  Özel kurallar:
* Global lock + local state:
   * Eğer openPosition != null veya openOrder != null ise:
      * Yeni TradeSignal → ignore/reject (logla)
   * CloseRequest geldiğinde:
      * Pozisyon varsa CONNECTION.sendOrder(close) çağırır
* Böylece “aynı anda tek pozisyon/order” garantisi tek noktadan sağlanır.

4. FOLLOW_ORDERS
   Görev:
* Açık pozisyonu takip eder, TP/SL dolunca kapatma talebi yollar.
* Emir atmaz, sadece ORDERING’e “kapat” der.
  Input:
* onPositionUpdate (entryPrice, size, side vs.)
* onMarketTick (markPrice / lastPrice)
* (Gerekirse) onOrderUpdate (fill olduğu anı bilmek için)
  Logic (basit):
* Pozisyon açıldığında: entry, size, side, leverage kaydet.
* Her price tick’te:
   * Unrealized PnL% hesapla.
   * TP/SL threshold’a göre:
      * TP/SL tetiklenirse:
         * Event: CloseRequest(positionId) → ORDERING
           FOLLOW_ORDERS:
* Ne trend bilir, ne balance. Sadece mevcut pozisyon + fiyat.

5. BALANCE
   Görev:
* Sadece USDT / USDC bakiyesi oku ve güncel tut.
* Gerektiğinde başka modüllere “availableBalance” bilgisi sağlar.
  Input / Output:
* Startup’ta + periyodik:
   * CONNECTION.fetchBalance()
* Sonuçları küçük bir shared state’e yazar:
   * balanceStore.usdt, balanceStore.usdc
     Bu store’a:
* TRENDING (position size hesaplamak için) bakabilir.
* ORDERING (son kontrol) bakabilir.

6. LOGGING
   Görev:
* Tüm modüllerden gelen önemli event’leri yazmak:
   * Trend sinyalleri
   * Açılan / kapanan order & pozisyonlar
   * Realized/Unrealized PnL
   * Rate-limit uyarıları, reconnect, error’lar
     Basit yapı:
* Logger.info/debug/error(eventType, payload)
* İstersen trade/PnL için ayrı küçük “PnLLogger” kullanabilirsin ama şart değil.

7. Data Flow – Tek Trade’in Hayatı
1. CONNECTION WS’den fiyat akıyor → onMarketTick.
2. TRENDING bu tick’leri işliyor → uygun görürse TradeSignal yayınlıyor.
3. ORDERING TradeSignal alıyor:
   * “Şu an openPosition/openOrder var mı?” kontrol
   * Yoksa CONNECTION.sendOrder(open) çağırıyor.
4. Emir fill olunca CONNECTION onPositionUpdate ve onOrderUpdate yayınlıyor.
   * ORDERING state’ini güncelliyor (artık openPosition var).
   * FOLLOW_ORDERS pozisyonu kendine kaydediyor.
5. Fiyat değiştikçe:
   * FOLLOW_ORDERS unrealized PnL% hesaplıyor.
   * TP veya SL şartı sağlanınca CloseRequest event’i fırlatıyor.
6. ORDERING CloseRequest alıyor:
   * Lock alıyor, CONNECTION.sendOrder(close) yapıyor.
7. Pozisyon kapanınca:
   * CONNECTION onPositionUpdate (closed) + realized PnL event’i yayınlıyor.
   * ORDERING: openPosition state’ini sıfırlıyor.
   * LOGGING: trade ve PnL kaydediyor.
     Tüm bu akış boyunca dış dünya ile tek konuşan: CONNECTION.

8. Rate Limit & Threading (çok kısa)
* Ratelimit:
   * Sadece CONNECTION bilir.
   * sendOrder ve fetchBalance içindeki küçük bir limiter (queue + sleep/deny)
* Threading:
   * CONNECTION: I/O thread(ler)i
   * TRENDING: ayrı worker thread
   * FOLLOW_ORDERS: ayrı worker thread
   * ORDERING: tek thread (veya shared executor) + lock
* Modüller arası iletişim: thread-safe queue / channel.

Özetle “best structure”:
* Dış dünya = CONNECTION
* Karar veren = TRENDING
* Emir basan = ORDERING
* Pozisyonu koruyan = FOLLOW_ORDERS
* Para kontrolü = BALANCE
* Her şeyin tanığı = LOGGING


BINANCE ONLY FUTURES TRADING BOT PROJECT !!

! OLABILDIGINCE WEBSOCKET KULLANICAZ. NE KADAR AZ REST API O KADAR IYI. BINANCE NEREDEYSE BU ISLEMLERIN HEPSINI WEBSOCKET ILE DESTEKLIYOR

1. Event Bus Sistemi (events.rs)
   MarketTick, TradeSignal, CloseRequest, OrderUpdate, PositionUpdate, BalanceUpdate eventleri
   Her event için ayrı channel (tokio mpsc)
   Modüller arası iletişim için merkezi sistem
2. CONNECTION (connection.rs)
   Exchange WS & REST tek kapı
   Market data WebSocket stream (MarketTick yayınlar)
   User data WebSocket stream (OrderUpdate/PositionUpdate yayınlar)
   sendOrder() ve fetchBalance() API'leri
   Rate limit ve reconnect yönetimi
3. TRENDING (trending.rs)
   MarketTick eventlerini dinler
   Trend analizi yapar
   TradeSignal eventi yayınlar
   Emir atmaz, sadece sinyal üretir
4. ORDERING (ordering.rs)
   TradeSignal ve CloseRequest eventlerini dinler
   Global lock + local state ile "tek pozisyon/order" garantisi
   CONNECTION.sendOrder() kullanarak emir gönderir
   OrderUpdate/PositionUpdate ile state senkronu
5. FOLLOW_ORDERS (follow_orders.rs)
   PositionUpdate ve MarketTick eventlerini dinler
   Açık pozisyonu takip eder
   TP/SL kontrolü yapar
   Tetiklenince CloseRequest yayınlar
6. BALANCE (balance.rs)
   USDT/USDC bakiye takibi
   CONNECTION.fetchBalance() kullanır
   Shared state (BalanceStore) sağlar
   Periyodik güncelleme (30 saniye)
7. LOGGING (logging.rs)
   Tüm eventleri dinler ve loglar
   Mevcut JsonLogger'ı kullanır
   Structured logging
8. Main Loop (main_new.rs)
   Yeni mimariye göre örnek main loop
   Tüm modülleri başlatır
   Event bus üzerinden iletişim

kisaca overenginering olmadan, best mimari, best structure ile bu planı yapıcaz. Ve gereksiz hic bir dosya veya kod olmamalı.
Eski ve gereksiz dosya ve kodlar kaldirilmalidir.


File structure: 
trading-bot/
├─ Cargo.toml
└─ src/
├─ main.rs
│
├─ config.rs          // API keys, symbol, TP/SL, leverage, genel ayarlar
├─ types.rs           // Domain tipleri: MarketTick, TradeSignal, Position, BalanceSnapshot, vs.
├─ event_bus.rs       // Tüm mpsc channel tanımları ve EventBus struct’ı
├─ state.rs           // Küçük shared state: openPosition, openOrder, BalanceStore, vs.
│
├─ connection.rs      // Exchange WS/REST, ratelimit, reconnect, sendOrder, fetchBalance
├─ trending.rs        // Trend analizi, MarketTick -> TradeSignal
├─ ordering.rs        // Tek pozisyon/order lock’ı, TradeSignal & CloseRequest -> sendOrder
├─ follow_orders.rs   // Pozisyon takibi, TP/SL logic, CloseRequest üretimi
├─ balance.rs         // Balance fetch + BalanceStore güncelleme
└─ logging.rs         // Event bazlı logging / PnL loglama
Kısaca dosya rolleri
* main.rs
   * Config okur.
   * EventBus ve shared state’i oluşturur.
   * connection, trending, ordering, follow_orders, balance, logging modüllerini başlatır (task olarak).
* config.rs
   * Tüm ayarlar tek yerde: API key/secret, base URL, symbol, TP/SL yüzdeleri, leverage, position size çarpanları vs.
* types.rs
   * Event payload’larında kullanılacak tüm domain struct’ları ve enum’lar:
      * Side, MarketTick, TradeSignal, CloseRequest, OrderUpdate, PositionUpdate, BalanceSnapshot…
* event_bus.rs
   * Her event tipi için Sender/Receiver ikililerini tutan bir EventBus yapısı.
   * Bu dosya modüller arası iletişim “kablosu”.
* state.rs
   * Küçük ortak state:
      * open_position, open_order
      * BalanceStore { usdt, usdc }
   * ORDERING “tek pozisyon/order” garantisini buradaki state + lock ile sağlar.
   * BALANCE buradaki balance’ı günceller.
   * TRENDING / ORDERING gerekirse sadece read yapar.
* connection.rs
   * Dış dünya (exchange) ile tek kontak noktası.
   * Market WS → MarketTick event’lerini event bus’a basar.
   * User-data WS → OrderUpdate / PositionUpdate event’leri.
   * sendOrder ve fetchBalance burada; ratelimit ve reconnect de burada.
* trending.rs
   * Sadece MarketTick alır, trend analizi yapar, TradeSignal üretir.
   * Exchange’i hiç görmez; sadece event bus’la konuşur.
* ordering.rs
   * TradeSignal ve CloseRequest dinler.
   * OrderUpdate/PositionUpdate eventlerinden local state’ini senkronlar.
   * Tek yerden “şu an openPosition veya openOrder var mı?” kontrolü.
   * Emir göndermek için sadece connection modülündeki fonksiyonu kullanır.
* follow_orders.rs
   * PositionUpdate + MarketTick dinler.
   * Unrealized PnL% hesabı + TP/SL trigger.
   * Sadece CloseRequest event’i üretir, emir basmaz.
* balance.rs
   * Periyodik olarak fetchBalance çağırır.
   * Gelen BalanceSnapshot event’leriyle BalanceStore’u günceller.
* logging.rs
   * Önemli event’leri dinler (TradeSignal, OrderUpdate, PositionUpdate, PnL, ratelimit, reconnect, error).
   * Structured log yazar.

Bu yapı:
* Tek crate, tek binary
* Düz src/, her modül = bir .rs dosyası
* Ortak tipler ve event bus ayrı, böylece modüller birbirine karışmıyor
* Overengineering yok; ama modüller mental modelinle bire bir eşleşiyor
  Yani evet: connection/mod.rs klasör yapmadan, sadece connection.rs, trending.rs vb. dosyalarla gitmek hem Rust tarafında gayet doğal hem de senin mimari taslakla tam uyumlu.


