# Development Workflow

## 🔄 Kod Değişikliği Yaparken İzlenecek Adımlar

### 1. Planlama Aşaması
- [ ] Değişikliğin amacı nedir?
- [ ] Hangi modül(ler) etkileniyor?
- [ ] Bağımlılıklar neler?
- [ ] Config değişikliği gerekiyor mu?
- [ ] Test stratejisi nedir?

### 2. Kod Yazma Aşaması
- [ ] `.cursorrules` dosyasını oku
- [ ] `ARCHITECTURE.md`'yi kontrol et
- [ ] `CODE_REVIEW_CHECKLIST.md`'yi takip et
- [ ] Mock/dummy data kullanma
- [ ] Config'den parametre al
- [ ] Error handling ekle
- [ ] Lifetime/ownership kontrol et

### 3. Test Aşaması
- [ ] `cargo check` - derleme hatası var mı?
- [ ] `cargo clippy` - linting uyarıları var mı?
- [ ] `cargo test` - test'ler geçiyor mu?
- [ ] Integration test çalıştır (gerçek API data ile)
- [ ] Manual test (eğer gerekiyorsa)

### 4. Dokümantasyon Aşaması
- [ ] Public function'lara doc comment ekle
- [ ] Complex logic'i açıkla
- [ ] `ARCHITECTURE.md` güncelle (eğer modül değiştiyse)
- [ ] `MODULE_DOCUMENTATION.md` güncelle
- [ ] `CODE_REVIEW_CHECKLIST.md` kontrol et

### 5. Code Review Aşaması
- [ ] `CODE_REVIEW_CHECKLIST.md`'deki tüm maddeleri kontrol et
- [ ] Mock/dummy data kontrolü
- [ ] Config kullanımı kontrolü
- [ ] Error handling kontrolü
- [ ] Lifetime/ownership kontrolü
- [ ] Thread safety kontrolü
- [ ] Test coverage kontrolü

### 6. Commit Aşaması
- [ ] Değişiklikleri commit et
- [ ] Commit message açıklayıcı olsun
- [ ] İlgili dosyaları commit et

## 🔍 Kod İnceleme Süreci

### AI ile Kod İnceleme
1. **Dosya bazlı inceleme**: Her dosyayı tek tek incele
2. **Modül bazlı inceleme**: İlgili modülleri birlikte incele
3. **Bağımlılık kontrolü**: Dependency graph'ı kontrol et
4. **Test kontrolü**: Test coverage'ı kontrol et

### İnceleme Soruları
- Bu kod ne yapıyor?
- Neden böyle yazılmış?
- Başka bir yerde benzer kod var mı?
- Test edilmiş mi?
- Dokümante edilmiş mi?
- Config'den parametre alıyor mu?
- Error handling var mı?

## 📊 Kod Kalitesi Metrikleri

### Zorunlu Kontroller
- ✅ Derleme hatası yok (`cargo check`)
- ✅ Linting uyarısı yok (`cargo clippy`)
- ✅ Test'ler geçiyor (`cargo test`)
- ✅ Mock data yok (production kodunda)
- ✅ Config kullanımı var
- ✅ Error handling var

### İstenen Kontroller
- ✅ Test coverage > 80%
- ✅ Tüm public function'lar dokümante
- ✅ Complex logic açıklanmış
- ✅ Architecture doc güncel

## 🚨 Sorun Tespiti

### Kod Karmaşıklığı Artıyorsa
1. Modülü böl (single responsibility)
2. Helper function'lar ekle
3. Type alias'lar kullan
4. Config'e taşı (hardcoded değerler varsa)

### Bağımlılıklar Artıyorsa
1. Dependency injection kullan
2. EventBus pattern kullan (loose coupling)
3. Interface/trait kullan
4. Modülü refactor et

### Test Coverage Düşüyorsa
1. Unit test ekle
2. Integration test ekle
3. Edge case'leri test et
4. Error case'leri test et

## 🔧 Otomatik Kontroller

### Pre-commit Hooks (Önerilen)
```bash
# .git/hooks/pre-commit
#!/bin/sh
cargo check
cargo clippy -- -D warnings
cargo test
```

### CI/CD Pipeline (Önerilen)
```yaml
# .github/workflows/ci.yml
- name: Check
  run: cargo check
- name: Clippy
  run: cargo clippy -- -D warnings
- name: Test
  run: cargo test
- name: Integration Test
  run: cargo test --test backtest -- --ignored
```

## 📝 Best Practices

1. **Küçük, odaklı değişiklikler**: Her commit tek bir şeyi değiştirsin
2. **Açıklayıcı commit mesajları**: Ne değişti, neden değişti
3. **Test-first yaklaşım**: Önce test yaz, sonra kod
4. **Dokümantasyon**: Kod değiştiyse doc da güncelle
5. **Code review**: Her değişikliği review et

## 🎯 AI Kullanımı İçin İpuçları

### AI'ya Soru Sorarken
1. **Spesifik ol**: "connection.rs'deki rate limiting nasıl çalışıyor?"
2. **Context ver**: Hangi modül, hangi function
3. **Dosya yolu belirt**: `src/connection/venue.rs:1859`
4. **Hata mesajı ekle**: Compile error varsa tam mesajı ekle

### AI'dan Kod İstemeden Önce
1. Mevcut kodu oku
2. Architecture'ı anla
3. Dependency'leri kontrol et
4. Test stratejisini belirle

### AI'dan Gelen Kodu Kontrol Ederken
1. Mock data var mı? → Config'e taşı
2. `unwrap()` var mı? → Error handling ekle
3. `&self` spawn içinde mi? → Clone et
4. Hardcoded değer var mı? → Config'e taşı
5. Test var mı? → Test ekle

