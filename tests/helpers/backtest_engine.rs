use trading_bot::{
    config::TrendParams,
    types::{MarketTick, TradeSignal},
    TrendEngine,
};

pub fn run_backtest(ticks: &[MarketTick], params: TrendParams) -> Vec<TradeSignal> {
    let mut engine = TrendEngine::new(params);
    let mut signals = Vec::new();

    for tick in ticks {
        if let Some(side) = engine.on_tick(tick) {
            if engine.can_emit_signal(tick.ts) {
                let signal = TradeSignal {
                    id: uuid::Uuid::new_v4(),
                    symbol: tick.symbol.clone(),
                    side,
                    entry_price: tick.price,
                    leverage: engine.params.leverage,
                    size_usdt: engine.params.position_size_quote,
                    ts: tick.ts,
                };
                signals.push(signal);
                engine.mark_signal_emitted(tick.ts);
            }
        }
    }

    signals
}

