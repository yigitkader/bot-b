# Integration Tests

Bu dizin kritik race condition ve memory leak senaryolarını test eder.

## Test Senaryoları

### 1. Balance Reservation Stress Test
**Dosya:** `integration_tests.rs::test_balance_reservation_stress`

**Amaç:** 100 thread'in aynı anda balance reserve etmeye çalıştığında:
- Balance doğru şekilde reserve ediliyor mu?
- Release işlemi doğru çalışıyor mu?
- Memory leak var mı?

**Beklenen Sonuç:**
- Final reserved balance = 0 (tüm reservation'lar release edilmeli)
- Final available balance = initial balance (10000 USDT)
- Over-reservation yok (race condition yok)

### 2. OrderUpdate vs PositionUpdate Race Condition Test
**Dosya:** `integration_tests.rs::test_order_position_update_race`

**Amaç:** OrderUpdate::Filled ve PositionUpdate aynı anda geldiğinde:
- State tutarlı kalıyor mu?
- Duplicate position oluşuyor mu?
- Timestamp kontrolü doğru çalışıyor mu?

**Beklenen Sonuç:**
- Tek bir position oluşmalı (duplicate yok)
- Her iki timestamp de set edilmeli
- State consistency korunmalı

### 3. Concurrent CloseRequest Test
**Dosya:** `integration_tests.rs::test_concurrent_close_request`

**Amaç:** TP ve SL aynı anda trigger edildiğinde:
- Sadece bir close request işlenmeli
- Double-close olmamalı
- Position tracking doğru çalışmalı

**Beklenen Sonuç:**
- Sadece 1 close işlemi gerçekleşmeli
- Position closed olmalı
- Race condition yok

### 4. WebSocket Reconnect Test
**Dosya:** `integration_tests.rs::test_websocket_reconnect_state_sync`

**Amaç:** WebSocket disconnect/reconnect sonrası:
- Order state sync doğru çalışıyor mu?
- Stale update'ler ignore ediliyor mu?
- State consistency korunuyor mu?

**Beklenen Sonuç:**
- Order state doğru sync edilmeli
- Position doğru oluşturulmalı
- Timestamp kontrolü çalışmalı

### 5. FOLLOW_ORDERS Position Removal Timing Test
**Dosya:** `integration_tests.rs::test_follow_orders_position_removal_timing`

**Amaç:** TP/SL trigger edildiğinde position removal timing:
- CloseRequest gönderilmeden önce position remove edilirse ne olur?
- CloseRequest başarısız olursa position tracking'de kalmalı mı?
- Race condition: Multiple ticks aynı anda gelirse duplicate trigger olmamalı

**Beklenen Sonuç:**
- CloseRequest gönderilmeden position remove edilmemeli
- CloseRequest başarısız olursa position tracking'de kalmalı
- Sadece 1 trigger olmalı (duplicate yok)

### 6. Balance Reservation Leak Detection Test
**Dosya:** `integration_tests.rs::test_balance_reservation_leak_detection`

**Amaç:** Balance reservation release edilmediğinde:
- RAII guard leak'i tespit ediyor mu?
- Balance doğru restore ediliyor mu?

**Beklenen Sonuç:**
- Reserved balance = 0 olmalı
- Available balance restore edilmeli

### 7. Order Placement Race Condition Test
**Dosya:** `integration_tests.rs::test_order_placement_race_condition`

**Amaç:** İki thread aynı sembol için order place etmeye çalıştığında:
- Sadece bir order place edilmeli
- Double-spend olmamalı
- Duplicate order olmamalı

**Beklenen Sonuç:**
- Sadece 1 order place edilmeli
- Reserved balance = 0 olmalı
- Sadece 1 balance reservation başarılı olmalı

### 8. MIN_NOTIONAL Error Handling Test
**Dosya:** `integration_tests.rs::test_min_notional_error_handling`

**Amaç:** MIN_NOTIONAL hatası geldiğinde:
- Dust check çalışıyor mu?
- LIMIT fallback sonsuz loop'a yol açmıyor mu?
- Position açık kalmıyor mu?

**Beklenen Sonuç:**
- Dust qty için position closed kabul edilmeli
- LIMIT fallback sadece bir kez denenmeli
- LIMIT fallback başarısız olursa hata döndürülmeli (retry yok)

### 9. Signal Spam Prevention Test
**Dosya:** `integration_tests.rs::test_signal_spam_prevention`

**Amaç:** Cooldown check performans optimizasyonu:
- Cooldown check trend analizinden önce yapılıyor mu?
- Erken çıkış gereksiz CPU kullanımını önlüyor mu?
- Same-direction check çalışıyor mu?

**Beklenen Sonuç:**
- Cooldown aktifse trend analizi yapılmamalı (early exit)
- Cooldown geçtiyse trend analizi yapılmalı
- Same direction signal'lar spam olarak engellenmeli

### 10. TP/SL Commission Calculation Test
**Dosya:** `integration_tests.rs::test_tp_sl_commission_calculation`

**Amaç:** Commission hesaplama doğruluğu:
- Entry commission TIF'e göre doğru mu? (Post-only → Maker, Market/IOC → Taker)
- Exit commission her zaman Taker mı? (TP/SL market order)
- Total commission doğru hesaplanıyor mu?

**Beklenen Sonuç:**
- Post-only order: 0.02% (entry) + 0.04% (exit) = 0.06%
- Market order: 0.04% (entry) + 0.04% (exit) = 0.08%
- Post-only daha düşük total commission'a sahip olmalı

### 11. Balance Startup Race Condition Test
**Dosya:** `integration_tests.rs::test_balance_startup_race_condition`

**Amaç:** REST API fetch ve WebSocket subscription arasındaki race condition:
- WebSocket update'ler öncelikli mi?
- Stale REST API data ignore ediliyor mu?
- Timestamp check çalışıyor mu?

**Beklenen Sonuç:**
- WebSocket update daha yeni ise REST API result ignore edilmeli
- REST API update daha yeni ise kullanılmalı
- WebSocket balance preserve edilmeli (stale REST API overwrite etmemeli)

## Test Çalıştırma

```bash
# Tüm testleri çalıştır
cargo test --test integration_tests

# Belirli bir testi çalıştır
cargo test --test integration_tests test_balance_reservation_stress

# Verbose output ile
cargo test --test integration_tests -- --nocapture
```

## Öncelik Sırası

Testler şu öncelik sırasına göre kritik sorunları test eder:

1. 🔴 **Balance reservation leak** (Kritik - memory leak)
   - Test: `test_balance_reservation_stress`
   - Test: `test_balance_reservation_leak_detection`

2. 🔴 **OrderUpdate/PositionUpdate race** (Kritik - duplicate position)
   - Test: `test_order_position_update_race`

3. ⚠️ **CloseRequest double trigger** (Önemli - position tracking)
   - Test: `test_concurrent_close_request`

4. ⚠️ **FOLLOW_ORDERS position removal timing** (Önemli - TP/SL fail)
   - Test: `test_follow_orders_position_removal_timing`

5. ⚠️ **WebSocket reconnect** (Önemli - state sync)
   - Test: `test_websocket_reconnect_state_sync`

6. 🟡 **Memory leaks** (İyileştirme)
   - Covered by balance reservation tests

7. 🔴 **Order placement race condition** (Kritik - double-spend, duplicate orders)
   - Test: `test_order_placement_race_condition`

8. ⚠️ **MIN_NOTIONAL error handling** (Önemli - infinite loop, position stuck)
   - Test: `test_min_notional_error_handling`

9. ⚠️ **Signal spam prevention** (Önemli - performance optimization)
   - Test: `test_signal_spam_prevention`

10. ⚠️ **TP/SL commission calculation** (Önemli - PnL accuracy)
   - Test: `test_tp_sl_commission_calculation`

11. ⚠️ **Balance startup race condition** (Önemli - stale data overwrite)
   - Test: `test_balance_startup_race_condition`

## Notlar

- Testler mevcut kod tabanındaki derleme hataları düzeltildikten sonra çalıştırılabilir
- Testler gerçek exchange bağlantısı gerektirmez (mock data kullanır)
- Testler async/await kullanır ve tokio runtime gerektirir

