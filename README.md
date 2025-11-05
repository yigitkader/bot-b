trading-bot/
├── Cargo.toml
├── config.yaml
└── crates/
├── bot_core/
│   ├── Cargo.toml
│   └── src/lib.rs
├── strategy/
│   ├── Cargo.toml
│   └── src/lib.rs
├── risk/
│   ├── Cargo.toml
│   └── src/lib.rs
├── exec/
│   ├── Cargo.toml
│   └── src/lib.rs
├── data/
│   ├── Cargo.toml
│   └── src/lib.rs
│   └── src/binance_ws.rs
├── monitor/
│   ├── Cargo.toml
│   └── src/lib.rs
├── backtest/
│   ├── Cargo.toml
│   └── src/lib.rs
└── app/
├── Cargo.toml
└── src/main.rs








RUST_LOG=info ./target/debug/app --config ./config.yaml

Yapılan işlemler:

RUST_LOG=info → Log seviyesini info olarak ayarlar.

./target/debug/app → Derlenmiş uygulamayı çalıştırır.

--config ./config.yaml → Uygulamanın ayarlarını (symbol, strategy, risk vs.) config.yaml dosyasından yükler.
