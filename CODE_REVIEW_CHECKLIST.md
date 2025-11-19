# Code Review Checklist

## 🔍 Her Kod Değişikliğinde Kontrol Edilmesi Gerekenler

### 1. Mock/Dummy Data Kontrolü
- [ ] Hardcoded test değerleri var mı? (örn: `50000`, `100.0`)
- [ ] Fallback değerler config'den mi geliyor?
- [ ] Production'da gerçek API data kullanılıyor mu?
- [ ] Test dosyaları dışında mock data yok mu?

### 2. Config Kullanımı
- [ ] Tüm parametreler `config.yaml`'dan mı geliyor?
- [ ] Default değerler tanımlı mı?
- [ ] Config validation çalışıyor mu?
- [ ] Hardcoded değerler yok mu?

### 3. Error Handling
- [ ] `unwrap()` kullanılmamış mı? (sadece test'lerde OK)
- [ ] `Result<T>` pattern kullanılıyor mu?
- [ ] Error context'leri açıklayıcı mı?
- [ ] Fallback'ler mantıklı mı?

### 4. Lifetime & Ownership
- [ ] `tokio::spawn` içinde `self` kullanılmamış mı?
- [ ] Gerekli değerler clone edilmiş mi?
- [ ] Arc/Rc kullanımı doğru mu?
- [ ] Mutex/RwLock poisoning handle ediliyor mu?

### 5. Thread Safety
- [ ] Shared state için Arc kullanılmış mı?
- [ ] Mutex/RwLock doğru kullanılmış mı?
- [ ] Race condition riski var mı?
- [ ] Atomic operations gerekiyorsa kullanılmış mı?

### 6. Event Bus Kullanımı
- [ ] Event'ler doğru channel'a gönderiliyor mu?
- [ ] Subscription'lar doğru mu?
- [ ] Event format'ları tutarlı mı?
- [ ] Event timestamp'leri doğru mu?

### 7. Rate Limiting
- [ ] API çağrıları rate-limited mi?
- [ ] Weight-based limiting kullanılıyor mu?
- [ ] Rate limit guard'lar doğru yerde mi?

### 8. Type Safety
- [ ] Type conversions güvenli mi?
- [ ] `Decimal` kullanımı doğru mu?
- [ ] Option/Result pattern'leri doğru mu?
- [ ] Type aliases kullanılıyor mu? (Px, Qty, etc.)

### 9. Documentation
- [ ] Public function'lar dokümante edilmiş mi?
- [ ] Complex logic açıklanmış mı?
- [ ] TODO/FIXME comment'leri var mı?
- [ ] Module-level doc var mı?

### 10. Testing
- [ ] Unit test'ler güncel mi?
- [ ] Integration test'ler çalışıyor mu?
- [ ] Test'ler gerçek data kullanıyor mu?
- [ ] Test coverage yeterli mi?

### 11. Performance
- [ ] Gereksiz clone'lar var mı?
- [ ] Cache kullanımı doğru mu?
- [ ] Memory leak riski var mı?
- [ ] Async/await doğru kullanılmış mı?

### 12. Code Organization
- [ ] Modül yapısı mantıklı mı?
- [ ] Import'lar düzenli mi?
- [ ] Dead code var mı?
- [ ] Unused imports var mı?

## 🚨 Kırmızı Bayraklar (Red Flags)

Bu durumlar **mutlaka** düzeltilmeli:

1. ❌ `unwrap()` production kodunda
2. ❌ Hardcoded API keys/secrets
3. ❌ Mock data production kodunda
4. ❌ `self` kullanımı `tokio::spawn` içinde
5. ❌ Race condition riski
6. ❌ Memory leak potansiyeli
7. ❌ Panic riski (unchecked unwrap, index out of bounds)
8. ❌ Infinite loop riski
9. ❌ Unhandled error cases
10. ❌ Dead code (kullanılmayan function/struct)

## ✅ İyi Pratikler

1. ✅ Config-driven parameters
2. ✅ Comprehensive error handling
3. ✅ Thread-safe code
4. ✅ Event-driven architecture
5. ✅ Real API data in production
6. ✅ Comprehensive tests
7. ✅ Clear documentation
8. ✅ Type safety
9. ✅ Rate limiting
10. ✅ Logging ve monitoring

