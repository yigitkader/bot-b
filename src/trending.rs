use crate::config::TrendParams;
use crate::event_bus::TrendingChannels;
use crate::types::{MarketTick, Side, TradeSignal};
use chrono::{DateTime, Utc};
use log::{debug, info, warn};
use uuid::Uuid;

const EMA_FAST_PERIOD: usize = 21;
const EMA_SLOW_PERIOD: usize = 55;
const RSI_PERIOD: usize = 14;
const ATR_PERIOD: usize = 14;

struct Ema {
    k: f64,
    value: Option<f64>,
}

impl Ema {
    fn new(period: usize) -> Self {
        let k = 2.0 / (period as f64 + 1.0);
        Self { k, value: None }
    }

    fn update(&mut self, price: f64) -> f64 {
        self.value = Some(match self.value {
            None => price,
            Some(prev) => prev + self.k * (price - prev),
        });
        self.value.unwrap()
    }
}

struct Rsi {
    period: usize,
    prev_close: Option<f64>,
    avg_gain: f64,
    avg_loss: f64,
    initialized: bool,
}

impl Rsi {
    fn new(period: usize) -> Self {
        Self {
            period,
            prev_close: None,
            avg_gain: 0.0,
            avg_loss: 0.0,
            initialized: false,
        }
    }

    fn update(&mut self, close: f64) -> Option<f64> {
        if let Some(prev) = self.prev_close {
            let change = close - prev;
            let gain = if change > 0.0 { change } else { 0.0 };
            let loss = if change < 0.0 { -change } else { 0.0 };

            if !self.initialized {
                self.avg_gain =
                    (self.avg_gain * ((self.period - 1) as f64) + gain) / self.period as f64;
                self.avg_loss =
                    (self.avg_loss * ((self.period - 1) as f64) + loss) / self.period as f64;

                if self.avg_gain > 0.0 || self.avg_loss > 0.0 {
                    self.initialized = true;
                }
            } else {
                self.avg_gain =
                    (self.avg_gain * ((self.period - 1) as f64) + gain) / self.period as f64;
                self.avg_loss =
                    (self.avg_loss * ((self.period - 1) as f64) + loss) / self.period as f64;
            }
        }

        self.prev_close = Some(close);

        if !self.initialized || self.avg_loss == 0.0 {
            return None;
        }

        let rs = self.avg_gain / self.avg_loss;
        let rsi = 100.0 - (100.0 / (1.0 + rs));
        Some(rsi)
    }
}

struct Atr {
    period: usize,
    value: Option<f64>,
    prev_close: Option<f64>,
    initialized: bool,
}

impl Atr {
    fn new(period: usize) -> Self {
        Self {
            period,
            value: None,
            prev_close: None,
            initialized: false,
        }
    }

    fn update(&mut self, close: f64) -> Option<f64> {
        let tr = if let Some(prev) = self.prev_close {
            (close - prev).abs()
        } else {
            0.0
        };

        if !self.initialized {
            self.value = Some(match self.value {
                None => tr,
                Some(prev) => (prev * ((self.period - 1) as f64) + tr) / self.period as f64,
            });

            if let Some(v) = self.value {
                if v > 0.0 {
                    self.initialized = true;
                }
            }
        } else {
            self.value = Some(match self.value {
                None => tr,
                Some(prev) => (prev * ((self.period - 1) as f64) + tr) / self.period as f64,
            });
        }

        self.prev_close = Some(close);
        self.value
    }
}

struct TrendEngine {
    ema_fast: Ema,
    ema_slow: Ema,
    rsi: Rsi,
    atr: Atr,
    tick_count: usize,
    last_atr: Option<f64>,
    last_signal_ts: Option<DateTime<Utc>>,
    params: TrendParams,
}

impl TrendEngine {
    fn new(params: TrendParams) -> Self {
        Self {
            ema_fast: Ema::new(EMA_FAST_PERIOD),
            ema_slow: Ema::new(EMA_SLOW_PERIOD),
            rsi: Rsi::new(RSI_PERIOD),
            atr: Atr::new(ATR_PERIOD),
            tick_count: 0,
            last_atr: None,
            last_signal_ts: None,
            params,
        }
    }

    fn warm(&self) -> bool {
        self.tick_count >= self.params.warmup_min_ticks
    }

    fn can_emit_signal(&self, now: DateTime<Utc>) -> bool {
        match self.last_signal_ts {
            None => true,
            Some(last) => (now - last).num_seconds() >= self.params.signal_cooldown_secs,
        }
    }

    fn on_tick(&mut self, tick: &MarketTick) -> Option<Side> {
        self.tick_count += 1;
        let price = tick.price;

        let fast = self.ema_fast.update(price);
        let slow = self.ema_slow.update(price);
        let rsi_val = self.rsi.update(price);
        let atr_val = self.atr.update(price);

        debug!(
            "TRENDING: tick={} fast_ema={:.2?} slow_ema={:.2?} rsi={:.2?} atr={:.6?}",
            price, fast, slow, rsi_val, atr_val
        );

        if !self.warm() {
            return None;
        }

        let (rsi_val, atr_val) = match (rsi_val, atr_val) {
            (Some(r), Some(a)) => (r, a),
            _ => return None,
        };

        let trend_up = fast > slow;
        let trend_down = fast < slow;

        let momentum_long = rsi_val >= self.params.rsi_long_min;
        let momentum_short = rsi_val <= self.params.rsi_short_max;

        let atr_rising = match self.last_atr {
            None => false,
            Some(prev) => atr_val > prev * self.params.atr_rising_factor,
        };
        self.last_atr = Some(atr_val);

        let (obi, funding, liq_long, liq_short) = (
            tick.obi,
            tick.funding_rate,
            tick.liq_long_cluster,
            tick.liq_short_cluster,
        );

        let (long_score, long_total) = self.score_long(
            trend_up,
            momentum_long,
            atr_rising,
            obi,
            funding,
            liq_long,
            liq_short,
        );
        let (short_score, short_total) = self.score_short(
            trend_down,
            momentum_short,
            atr_rising,
            obi,
            funding,
            liq_long,
            liq_short,
        );

        debug!(
            "TRENDING: long {}/{} | short {}/{} | rsi={:.2} atr={:.6} obi={:?} funding={:?}",
            long_score, long_total, short_score, short_total, rsi_val, atr_val, obi, funding
        );

        let long_ok = Self::score_passes(long_score, long_total, self.params.long_min_score);
        let short_ok = Self::score_passes(short_score, short_total, self.params.short_min_score);

        match (long_ok, short_ok) {
            (true, false) => Some(Side::Long),
            (false, true) => Some(Side::Short),
            _ => None,
        }
    }

    fn score_long(
        &self,
        trend_up: bool,
        momentum_long: bool,
        atr_rising: bool,
        obi: Option<f64>,
        funding: Option<f64>,
        liq_long: Option<f64>,
        liq_short: Option<f64>,
    ) -> (usize, usize) {
        let mut score = 0;
        let mut total = 0;

        total += 1;
        if trend_up {
            score += 1;
        }

        total += 1;
        if momentum_long {
            score += 1;
        }

        total += 1;
        if atr_rising {
            score += 1;
        }

        if let Some(obi_val) = obi {
            total += 1;
            if obi_val >= self.params.obi_long_min {
                score += 1;
            }
        }

        if let Some(f) = funding {
            total += 1;
            if f <= self.params.funding_max_for_long {
                score += 1;
            }
        }

        if liq_short.is_some() || liq_long.is_some() {
            let short = liq_short.unwrap_or(0.0);
            let long = liq_long.unwrap_or(0.0);
            total += 1;
            if short > long {
                score += 1;
            }
        }

        (score, total)
    }

    fn score_short(
        &self,
        trend_down: bool,
        momentum_short: bool,
        atr_rising: bool,
        obi: Option<f64>,
        funding: Option<f64>,
        liq_long: Option<f64>,
        liq_short: Option<f64>,
    ) -> (usize, usize) {
        let mut score = 0;
        let mut total = 0;

        total += 1;
        if trend_down {
            score += 1;
        }

        total += 1;
        if momentum_short {
            score += 1;
        }

        total += 1;
        if atr_rising {
            score += 1;
        }

        if let Some(obi_val) = obi {
            total += 1;
            if obi_val <= self.params.obi_short_max {
                score += 1;
            }
        }

        if let Some(f) = funding {
            total += 1;
            if f >= self.params.funding_min_for_short {
                score += 1;
            }
        }

        if liq_short.is_some() || liq_long.is_some() {
            let short = liq_short.unwrap_or(0.0);
            let long = liq_long.unwrap_or(0.0);
            total += 1;
            if long > short {
                score += 1;
            }
        }

        (score, total)
    }

    fn score_passes(score: usize, total: usize, min_score: usize) -> bool {
        if total == 0 {
            return false;
        }
        if total >= min_score {
            score >= min_score
        } else {
            score == total
        }
    }

    fn mark_signal_emitted(&mut self, ts: DateTime<Utc>) {
        self.last_signal_ts = Some(ts);
    }
}

// ==== Ana worker ====

pub async fn run_trending(mut ch: TrendingChannels, params: TrendParams) {
    info!("TRENDING: started (EMA+RSI+ATR+OBI+Funding+Liq)");

    let mut engine = TrendEngine::new(params);

    loop {
        match ch.market_rx.recv().await {
            Ok(tick) => {
                let now = Utc::now();
                if let Some(side) = engine.on_tick(&tick) {
                    if !engine.can_emit_signal(now) {
                        debug!("TRENDING: cooldown aktif, sinyal atlandı.");
                        continue;
                    }

                    let signal = TradeSignal {
                        id: Uuid::new_v4(),
                        symbol: tick.symbol.clone(),
                        side,
                        entry_price: tick.price,
                        leverage: 10.0,  // TODO: config'ten al
                        size_usdt: 10.0, // TODO: balance'a göre hesapla
                        ts: now,
                    };

                    info!(
                        "TRENDING: {:?} sinyali -> price={:.2} (trend+orderflow+funding+liq onaylı)",
                        signal.side, signal.entry_price
                    );

                    if let Err(e) = ch.signal_tx.send(signal).await {
                        warn!("TRENDING: signal_tx send error: {}", e);
                    } else {
                        engine.mark_signal_emitted(now);
                    }
                }
            }
            Err(e) => {
                // broadcast lag vs. durumları
                warn!("TRENDING: market_rx recv error: {}", e);
            }
        }
    }
}
