cargo test


### Sadece yeni testleri çalıştırmak için:
cargo test --test position_and_pricing

### Sadece backtest testlerini çalıştırmak için:
cargo test --test backtest

### Belirli bir test fonksiyonunu çalıştırmak için:
cargo test test_position_id_collision
