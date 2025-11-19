# Default ayarlarla (BTCUSDT, 5m, son 24 saat)
cargo run --bin backtest

# Environment variable'larla özelleştirme
SYMBOL=ETHUSDT INTERVAL=15m PERIOD=15m LIMIT=96 cargo run --bin backtest