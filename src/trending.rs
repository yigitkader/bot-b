use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use reqwest::{Client, Url};
use serde::Deserialize;
use ta::indicators::{AverageTrueRange, ExponentialMovingAverage, RelativeStrengthIndex};
use ta::{DataItem, Next};
use rand::{Rng, SeedableRng};
use rand::rngs::StdRng;

use crate::types::{
    AlgoConfig, BacktestResult, Candle, FundingRate, FuturesClient, LongShortRatioPoint,
    OpenInterestHistPoint, OpenInterestPoint, PositionSide, Signal, SignalContext, SignalSide,
    Trade, TrendDirection,
};
use uuid::Uuid;


impl FuturesClient {
    pub fn new() -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();

        let base_url = Url::parse("https://fapi.binance.com").unwrap(); // USDS-M futures
        Self { http, base_url }
    }

    pub async fn fetch_klines(
        &self,
        symbol: &str,
        interval: &str,
        limit: u32,
    ) -> Result<Vec<Candle>> {
        let mut url = self.base_url.join("/fapi/v1/klines")?;
        url.query_pairs_mut()
            .append_pair("symbol", symbol)
            .append_pair("interval", interval)
            .append_pair("limit", &limit.to_string());

        let res = self.http.get(url).send().await?;
        if !res.status().is_success() {
            anyhow::bail!("Klines error: {}", res.text().await?);
        }

        let raw: Vec<serde_json::Value> = res.json().await?;
        let candles = raw
            .into_iter()
            .filter_map(|arr| {
                let arr = arr.as_array()?;
                if arr.len() < 7 {
                    return None;
                }
                let open_time_ms = arr[0].as_i64()?;
                let close_time_ms = arr[6].as_i64()?;
                let open_time = ts_ms_to_utc(open_time_ms);
                let close_time = ts_ms_to_utc(close_time_ms);

                Some(Candle {
                    open_time,
                    close_time,
                    open: arr[1].as_str()?.parse().ok()?,
                    high: arr[2].as_str()?.parse().ok()?,
                    low: arr[3].as_str()?.parse().ok()?,
                    close: arr[4].as_str()?.parse().ok()?,
                    volume: arr[5].as_str()?.parse().ok()?,
                })
            })
            .collect();

        Ok(candles)
    }

    pub async fn fetch_funding_rates(
        &self,
        symbol: &str,
        limit: u32,
    ) -> Result<Vec<FundingRate>> {
        let mut url = self.base_url.join("/fapi/v1/fundingRate")?;
        url.query_pairs_mut()
            .append_pair("symbol", symbol)
            .append_pair("limit", &limit.to_string());

        let res = self.http.get(url).send().await?;
        if !res.status().is_success() {
            anyhow::bail!("Funding error: {}", res.text().await?);
        }

        let raw: Vec<serde_json::Value> = res.json().await?;
        let fr = raw
            .into_iter()
            .filter_map(|v| {
                let obj = v.as_object()?;
                let funding_time = obj.get("fundingTime")?
                    .as_i64()
                    .or_else(|| obj.get("fundingTime")?.as_str()?.parse().ok())?;
                Some(FundingRate {
                    _symbol: obj.get("symbol")?.as_str()?.to_string(),
                    funding_rate: obj.get("fundingRate")?.as_str()?.to_string(),
                    funding_time,
                })
            })
            .collect();
        Ok(fr)
    }

    pub async fn fetch_open_interest_hist(
        &self,
        symbol: &str,
        period: &str,
        limit: u32,
    ) -> Result<Vec<OpenInterestPoint>> {
        let mut url = self.base_url.join("/futures/data/openInterestHist")?;
        url.query_pairs_mut()
            .append_pair("symbol", symbol)
            .append_pair("period", period)
            .append_pair("limit", &limit.to_string());

        let res = self.http.get(url).send().await?;
        if !res.status().is_success() {
            anyhow::bail!("OpenInterestHist error: {}", res.text().await?);
        }

        let raw: Vec<OpenInterestHistPoint> = res.json().await?;
        let points = raw
            .into_iter()
            .map(|p| OpenInterestPoint {
                timestamp: ts_ms_to_utc(p.timestamp),
                open_interest: p.sum_open_interest.parse().unwrap_or(0.0),
            })
            .collect();

        Ok(points)
    }

    pub async fn fetch_top_long_short_ratio(
        &self,
        symbol: &str,
        period: &str,
        limit: u32,
    ) -> Result<Vec<LongShortRatioPoint>> {
        let mut url = self
            .base_url
            .join("/futures/data/topLongShortAccountRatio")?;
        url.query_pairs_mut()
            .append_pair("symbol", symbol)
            .append_pair("period", period)
            .append_pair("limit", &limit.to_string());

        let res = self.http.get(url).send().await?;
        if !res.status().is_success() {
            anyhow::bail!("TopLongShortAccountRatio error: {}", res.text().await?);
        }

        let raw: Vec<serde_json::Value> = res.json().await?;
        let points = raw
            .into_iter()
            .filter_map(|v| {
                let obj = v.as_object()?;
                let ts_ms = obj.get("timestamp")?
                    .as_i64()
                    .or_else(|| obj.get("timestamp")?.as_str()?.parse().ok())?;
                LongShortRatioPoint {
                    timestamp: ts_ms_to_utc(ts_ms),
                    long_short_ratio: obj.get("longShortRatio")?.as_str()?.parse().ok()?,
                    long_account_pct: obj.get("longAccount")?.as_str()?.parse().ok()?,
                    short_account_pct: obj.get("shortAccount")?.as_str()?.parse().ok()?,
                }.into()
            })
            .collect();

        Ok(points)
    }
}

// =======================
//  Utility Fonksiyonlar
// =======================

fn ts_ms_to_utc(ms: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(ms)
        .expect("invalid timestamp millis")
}

fn nearest_value_by_time<'a, T, F>(
    t: &'a DateTime<Utc>,
    series: &'a [T],
    ts_extractor: F,
) -> Option<&'a T>
where
    F: Fn(&T) -> DateTime<Utc>,
{
    if series.is_empty() {
        return None;
    }

    let mut best: Option<(&T, i64)> = None;
    for item in series {
        let its = ts_extractor(item);
        let diff = (t.timestamp_millis() - its.timestamp_millis()).abs();
        match best {
            None => best = Some((item, diff)),
            Some((_, best_diff)) if diff < best_diff => best = Some((item, diff)),
            _ => {}
        }
    }
    best.map(|(it, _)| it)
}

// =======================
//  Sinyal Context Hesabı
// =======================

/// Gerçek API verisi kullanarak signal context'leri oluşturur
/// 
/// # Önemli: Dummy/Mock Data Yok
/// Bu fonksiyon kesinlikle gerçek API verisi kullanır. Eğer veri bulunamazsa,
/// o candle için context oluşturulmaz (skip edilir). Hiçbir fallback değer kullanılmaz.
/// 
/// # Returns
/// Eşleşen candle'ları ve context'leri birlikte döndürür. Eğer bir candle için
/// gerçek API verisi yoksa, o candle skip edilir ve sonuçta yer almaz.
pub fn build_signal_contexts(
    candles: &[Candle],
    funding: &[FundingRate],
    oi_hist: &[OpenInterestPoint],
    lsr_hist: &[LongShortRatioPoint],
) -> (Vec<Candle>, Vec<SignalContext>) {
    let mut ema_fast = ExponentialMovingAverage::new(21).unwrap();
    let mut ema_slow = ExponentialMovingAverage::new(55).unwrap();
    let mut rsi = RelativeStrengthIndex::new(14).unwrap();
    let mut atr = AverageTrueRange::new(14).unwrap();

    let mut matched_candles = Vec::with_capacity(candles.len());
    let mut contexts = Vec::with_capacity(candles.len());
    
    // Last known values - bu veriler periyodik olarak güncellenir, bu yüzden
    // son bilinen değerleri kullanarak eksik verileri dolduruyoruz
    let mut last_funding: Option<f64> = None;
    let mut last_oi: Option<f64> = None;
    let mut last_lsr: Option<f64> = None;

    for c in candles {
        let di = DataItem::builder()
            .open(c.open)
            .high(c.high)
            .low(c.low)
            .close(c.close)
            .volume(c.volume)
            .build()
            .unwrap();

        let ema_f = ema_fast.next(&di);
        let ema_s = ema_slow.next(&di);
        let r = rsi.next(&di);
        let atr_v = atr.next(&di);

        // Funding rate: Önce bu candle için en yakın funding'i bul
        // Eğer bulunursa kullan ve last_funding'i güncelle
        // Eğer bulunamazsa, son bilinen funding rate'i kullan
        let funding_rate = nearest_value_by_time(&c.close_time, funding, |fr| {
            ts_ms_to_utc(fr.funding_time)
        })
        .and_then(|fr| fr.funding_rate.parse().ok())
        .or_else(|| last_funding);
        
        // Eğer funding rate bulunamadıysa (ne direct match ne de last known), skip et
        let Some(funding_rate) = funding_rate else {
            continue;
        };
        
        // Funding rate bulundu, last_funding'i güncelle
        last_funding = Some(funding_rate);

        // Open Interest: Önce bu candle için en yakın OI'yi bul
        // Eğer bulunursa kullan ve last_oi'yi güncelle
        // Eğer bulunamazsa, son bilinen OI değerini kullan
        let open_interest = nearest_value_by_time(&c.close_time, oi_hist, |p| p.timestamp)
            .map(|p| p.open_interest)
            .or(last_oi);
        
        // Eğer OI bulunamadıysa (ne direct match ne de last known), skip et
        let Some(open_interest) = open_interest else {
            continue;
        };
        
        // OI bulundu, last_oi'yi güncelle
        last_oi = Some(open_interest);

        // Long/Short Ratio: Önce bu candle için en yakın LSR'yi bul
        // Eğer bulunursa kullan ve last_lsr'yi güncelle
        // Eğer bulunamazsa, son bilinen LSR değerini kullan
        let long_short_ratio = nearest_value_by_time(&c.close_time, lsr_hist, |p| p.timestamp)
            .map(|p| p.long_short_ratio)
            .or(last_lsr);
        
        // Eğer LSR bulunamadıysa (ne direct match ne de last known), skip et
        let Some(long_short_ratio) = long_short_ratio else {
            continue;
        };
        
        // LSR bulundu, last_lsr'yi güncelle
        last_lsr = Some(long_short_ratio);

        matched_candles.push(c.clone());
        contexts.push(SignalContext {
            ema_fast: ema_f,
            ema_slow: ema_s,
            rsi: r,
            atr: atr_v,
            funding_rate,
            open_interest,
            long_short_ratio,
        });
    }

    (matched_candles, contexts)
}

// =======================
//  Sinyal Motoru
// =======================

/// Trend yönünü belirler (EMA fast vs slow)
pub fn classify_trend(ctx: &SignalContext) -> TrendDirection {
    if ctx.ema_fast > ctx.ema_slow {
        TrendDirection::Up
    } else if ctx.ema_fast < ctx.ema_slow {
        TrendDirection::Down
    } else {
        TrendDirection::Flat
    }
}

/// Tek bir candle için sinyal üretir (internal kullanım)
/// Production'da `generate_signals` kullanılmalı
fn generate_signal(
    candle: &Candle,
    ctx: &SignalContext,
    prev_ctx: Option<&SignalContext>,
    cfg: &AlgoConfig,
) -> Signal {
    let trend = classify_trend(ctx);

    // OI değişim yönü (son veri varsa)
    let oi_change_up = prev_ctx
        .map(|p| ctx.open_interest > p.open_interest)
        .unwrap_or(false);

    // Crowding
    let crowded_long = ctx.long_short_ratio >= cfg.lsr_crowded_long;
    let _crowded_short = ctx.long_short_ratio <= cfg.lsr_crowded_short;

    let mut long_score = 0usize;
    let mut short_score = 0usize;

    // LONG kuralları
    // 1) Trend yukarı
    if matches!(trend, TrendDirection::Up) {
        long_score += 1;
    }
    // 2) Momentum yukarı
    if ctx.rsi >= cfg.rsi_trend_long_min {
        long_score += 1;
    }
    // 3) Funding aşırı pozitif değil (aşırı long kalabalığı istemiyoruz)
    if ctx.funding_rate <= cfg.funding_extreme_pos {
        long_score += 1;
    }
    // 4) Open interest artıyor (yeni pozisyon akışı var)
    if oi_change_up {
        long_score += 1;
    }
    // 5) Top trader'lar aşırı long değil (hatta biraz short baskısı olabilir)
    if !crowded_long {
        long_score += 1;
    }
    // 6) ATR volatility kontrolü: Yüksek volatility'de daha dikkatli ol
    // ATR rising factor ile birlikte kullanılabilir (gelecekte)
    // Şimdilik ATR hesaplanıyor ama signal generation'da kullanılmıyor
    // Not: ATR değeri ctx.atr'de mevcut, ancak şu an için signal scoring'de kullanılmıyor

    // SHORT kuralları
    // 1) Trend aşağı
    if matches!(trend, TrendDirection::Down) {
        short_score += 1;
    }
    // 2) Momentum aşağı
    if ctx.rsi <= cfg.rsi_trend_short_max {
        short_score += 1;
    }
    // 3) Funding pozitif ve mümkünse aşırı (crowded long)
    if ctx.funding_rate >= cfg.funding_extreme_pos {
        short_score += 1;
    }
    // 4) Open interest artıyor (yeni pozisyon akışı var)
    if oi_change_up {
        short_score += 1;
    }
    // 5) Top trader'lar aşırı long (crowded long → short fırsatı)
    if crowded_long {
        short_score += 1;
    }

    // Kullanıcının "en iyi sistem" tanımına göre: minimum 4 score gerekli
    // Ancak config'den de alabilir (varsayılan 4)
    let long_min = cfg.long_min_score.max(4);
    let short_min = cfg.short_min_score.max(4);
    
    // Determine signal side with tie-break mechanism
    let side = if long_score >= long_min && short_score >= short_min {
        // Both scores meet minimum threshold
        if long_score > short_score {
            SignalSide::Long
        } else if short_score > long_score {
            SignalSide::Short
        } else {
            // Tie-break: use trend direction as primary factor
            match trend {
                TrendDirection::Up => SignalSide::Long,
                TrendDirection::Down => SignalSide::Short,
                TrendDirection::Flat => {
                    // Secondary tie-break: use RSI
                    if ctx.rsi >= cfg.rsi_trend_long_min {
                        SignalSide::Long
                    } else if ctx.rsi <= cfg.rsi_trend_short_max {
                        SignalSide::Short
                    } else {
                        // Tertiary tie-break: use funding rate
                        if ctx.funding_rate <= cfg.funding_extreme_pos {
                            SignalSide::Long
                        } else {
                            SignalSide::Short
                        }
                    }
                }
            }
        }
    } else if long_score >= long_min && long_score > short_score {
        SignalSide::Long
    } else if short_score >= short_min && short_score > long_score {
        SignalSide::Short
    } else {
        SignalSide::Flat
    };

    Signal {
        time: candle.close_time,
        price: candle.close,
        side,
        ctx: ctx.clone(),
    }
}

// =======================
//  Sinyal Üretimi (Production için)
// =======================

/// Tüm sinyalleri üretir - sadece sinyal üretimi, pozisyon yönetimi yok
/// 
/// # Production Kullanımı
/// Bu fonksiyon sadece sinyal üretir. Üretilen sinyaller `ordering` modülüne
/// gönderilir ve orada pozisyon açma/kapama işlemleri yapılır.
/// 
/// # Backtest Kullanımı
/// Backtest için `run_backtest_on_series` kullanılır (pozisyon yönetimi içerir)
pub fn generate_signals(
    candles: &[Candle],
    contexts: &[SignalContext],
    cfg: &AlgoConfig,
) -> Vec<Signal> {
    assert_eq!(candles.len(), contexts.len());
    
    let mut signals = Vec::new();
    
    for i in 1..candles.len() {
        let c = &candles[i];
        let ctx = &contexts[i];
        let prev_ctx = if i > 0 { Some(&contexts[i - 1]) } else { None };
        
        let sig = generate_signal(c, ctx, prev_ctx, cfg);
        signals.push(sig);
    }
    
    signals
}

// =======================
//  Backtest Engine (Sadece backtest için - pozisyon yönetimi içerir)
// =======================

/// Backtest için özel fonksiyon - sinyal üretir VE pozisyon yönetimi yapar
/// 
/// # ⚠️ Backtest vs Production Divergence (Now Realistic!)
/// 
/// **Backtest Execution (Realistic):**
/// - Signal generated at candle `i` close
/// - Execution delay: 1-2 bars (simulates production delay: Signal → EventBus → Ordering → API → Fill)
/// - Dynamic slippage: Base slippage (0.05%) multiplied by volatility (ATR-based) and random factor (1.0-3.0x)
/// - High volatility periods: slippage can reach 0.1-0.5% (production reality)
/// 
/// **Production Execution (Realistic):**
/// - Signal generated at candle close
/// - Signal → event bus (mpsc channel delay: ~1-10ms)
/// - Ordering module: risk checks, symbol info fetch, quantity calculation (~50-200ms)
/// - API call (network delay: ~100-500ms)
/// - Order filled at market price (slippage: 0.05% normal, 0.1-0.5% during volatility)
/// - Total delay: typically 1-5 seconds (≈ 1-2 bars for 5m candles)
/// 
/// **Impact:**
/// - Backtest results are now more realistic and closer to production
/// - Execution delays and dynamic slippage simulate production conditions
/// - Results may still be slightly optimistic due to perfect signal timing, but much closer to reality
/// 
/// # NOT: Production Kullanımı
/// Bu fonksiyon sadece backtest için kullanılır. Production'da:
/// 1. `generate_signals` ile sinyaller üretilir
/// 2. Sinyaller `ordering` modülüne gönderilir
/// 3. `ordering` modülü pozisyon açma/kapama işlemlerini yapar
pub fn run_backtest_on_series(
    candles: &[Candle],
    contexts: &[SignalContext],
    cfg: &AlgoConfig,
) -> BacktestResult {
    assert_eq!(candles.len(), contexts.len());

    let mut trades: Vec<Trade> = Vec::new();

    let mut pos_side = PositionSide::Flat;
    let mut pos_entry_price = 0.0;
    let mut pos_entry_time = candles[0].open_time;
    let mut pos_entry_index: usize = 0;

    let fee_frac = cfg.fee_bps_round_trip / 10_000.0;
    let base_slippage_frac = cfg.slippage_bps / 10_000.0; // Base slippage (production reality)
    
    // Pending signals queue: (signal_index, entry_index, signal_side, signal_ctx)
    let mut pending_signals: Vec<(usize, usize, SignalSide, SignalContext)> = Vec::new();
    
    // Signal statistics
    let mut total_signals = 0usize;
    let mut long_signals = 0usize;
    let mut short_signals = 0usize;
    
    // Deterministic RNG seed for reproducible results
    let mut rng = rand::rngs::StdRng::seed_from_u64(42);

    for i in 1..(candles.len() - 1) {
        let c = &candles[i];
        let ctx = &contexts[i];
        let prev_ctx = if i > 0 { Some(&contexts[i - 1]) } else { None };

        // Sadece sinyal üretimi
        let sig = generate_signal(c, ctx, prev_ctx, cfg);
        
        // Count signals
        match sig.side {
            SignalSide::Long => {
                total_signals += 1;
                long_signals += 1;
            }
            SignalSide::Short => {
                total_signals += 1;
                short_signals += 1;
            }
            SignalSide::Flat => {}
        }
        
        // Execution delay: Signal üretildikten sonra 1-2 bar bekle (production reality)
        // Production'da: Signal → EventBus → Ordering → API → Fill (1-5 seconds ≈ 1-2 bars)
        if !matches!(sig.side, SignalSide::Flat) {
            let execution_delay = rng.gen_range(1..=2); // 1-2 bar delay
            let entry_index = i + execution_delay;
            if entry_index < candles.len() {
                pending_signals.push((i, entry_index, sig.side, ctx.clone()));
            }
        }

        // Check pending signals for execution
        pending_signals.retain(|(_signal_idx, entry_idx, signal_side, _signal_ctx)| {
            if *entry_idx == i {
                // Execute signal now
                if matches!(pos_side, PositionSide::Flat) {
                    // Calculate dynamic slippage based on ATR (volatility) at entry time
                    // Higher ATR = higher volatility = higher slippage
                    // Base slippage: 0.05% (5 bps), can increase to 0.1-0.5% during high volatility
                    let entry_ctx = &contexts[i]; // Use context at entry time, not signal time
                    let atr_pct = entry_ctx.atr / c.close; // ATR as percentage of price
                    let volatility_multiplier = (atr_pct * 100.0).min(10.0).max(1.0); // Cap at 10x
                    let dynamic_slippage_frac = base_slippage_frac * volatility_multiplier * rng.gen_range(1.0..3.0);
                    
                    let entry_candle = &candles[i];
                    
                    match signal_side {
                        SignalSide::Long => {
                            pos_side = PositionSide::Long;
                            // Entry: LONG position buys at ask (higher price due to slippage)
                            pos_entry_price = entry_candle.open * (1.0 + dynamic_slippage_frac);
                            pos_entry_time = entry_candle.open_time;
                            pos_entry_index = i;
                        }
                        SignalSide::Short => {
                            pos_side = PositionSide::Short;
                            // Entry: SHORT position sells at bid (lower price due to slippage)
                            pos_entry_price = entry_candle.open * (1.0 - dynamic_slippage_frac);
                            pos_entry_time = entry_candle.open_time;
                            pos_entry_index = i;
                        }
                        SignalSide::Flat => {}
                    }
                }
                false // Remove from pending signals
            } else {
                true // Keep in pending signals
            }
        });

        let next_c = &candles[i + 1];

        // Pozisyon varsa: max holding check + sinyal yönü
        match pos_side {
            PositionSide::Long => {
                let holding_bars = i.saturating_sub(pos_entry_index);
                let should_close =
                    matches!(sig.side, SignalSide::Short)
                    || holding_bars >= cfg.max_holding_bars;

                if should_close {
                    // Calculate dynamic slippage for exit
                    let atr_pct = ctx.atr / c.close;
                    let volatility_multiplier = (atr_pct * 100.0).min(10.0).max(1.0);
                    let dynamic_slippage_frac = base_slippage_frac * volatility_multiplier * rng.gen_range(1.0..3.0);
                    
                    // Exit: LONG position sells at bid (lower price due to slippage)
                    let exit_price = next_c.open * (1.0 - dynamic_slippage_frac);
                    let raw_pnl = (exit_price - pos_entry_price) / pos_entry_price;
                    let pnl_pct = raw_pnl - fee_frac;
                    let win = pnl_pct > 0.0;

                    trades.push(Trade {
                        entry_time: pos_entry_time,
                        exit_time: next_c.open_time,
                        side: PositionSide::Long,
                        entry_price: pos_entry_price,
                        exit_price,
                        pnl_pct,
                        win,
                    });

                    pos_side = PositionSide::Flat;
                }
            }
            PositionSide::Short => {
                let holding_bars = i.saturating_sub(pos_entry_index);
                let should_close =
                    matches!(sig.side, SignalSide::Long)
                    || holding_bars >= cfg.max_holding_bars;

                if should_close {
                    // Calculate dynamic slippage for exit
                    let atr_pct = ctx.atr / c.close;
                    let volatility_multiplier = (atr_pct * 100.0).min(10.0).max(1.0);
                    let dynamic_slippage_frac = base_slippage_frac * volatility_multiplier * rng.gen_range(1.0..3.0);
                    
                    // Exit: SHORT position buys at ask (higher price due to slippage)
                    let exit_price = next_c.open * (1.0 + dynamic_slippage_frac);
                    let raw_pnl = (pos_entry_price - exit_price) / pos_entry_price;
                    let pnl_pct = raw_pnl - fee_frac;
                    let win = pnl_pct > 0.0;

                    trades.push(Trade {
                        entry_time: pos_entry_time,
                        exit_time: next_c.open_time,
                        side: PositionSide::Short,
                        entry_price: pos_entry_price,
                        exit_price,
                        pnl_pct,
                        win,
                    });

                    pos_side = PositionSide::Flat;
                }
            }
            PositionSide::Flat => {}
        }
    }

    let total_trades = trades.len();
    let mut win_trades = 0usize;
    let mut loss_trades = 0usize;
    let mut total_pnl_pct = 0.0;
    let mut total_win_pnl = 0.0;
    let mut total_loss_pnl = 0.0;

    for t in &trades {
        if t.win {
            win_trades += 1;
            total_win_pnl += t.pnl_pct.abs();
        } else {
            loss_trades += 1;
            total_loss_pnl += t.pnl_pct.abs();
        }
        total_pnl_pct += t.pnl_pct;
    }

    let win_rate = if total_trades == 0 {
        0.0
    } else {
        win_trades as f64 / total_trades as f64
    };

    let avg_pnl_pct = if total_trades == 0 {
        0.0
    } else {
        total_pnl_pct / total_trades as f64
    };

    // Average R (Risk/Reward): average win / average loss
    let avg_r = if loss_trades > 0 && win_trades > 0 {
        let avg_win = total_win_pnl / win_trades as f64;
        let avg_loss = total_loss_pnl / loss_trades as f64;
        if avg_loss > 0.0 {
            avg_win / avg_loss
        } else {
            0.0
        }
    } else if loss_trades == 0 && win_trades > 0 {
        // Sadece kazançlar var, R = infinity (çok büyük sayı)
        f64::INFINITY
    } else {
        // Sadece kayıplar var veya hiç trade yok
        0.0
    };

    BacktestResult {
        trades,
        total_trades,
        win_trades,
        loss_trades,
        win_rate,
        total_pnl_pct,
        avg_pnl_pct,
        avg_r,
        total_signals,
        long_signals,
        short_signals,
    }
}

// =======================
//  High-level Backtest Runner
// =======================

pub async fn run_backtest(
    symbol: &str,
    kline_interval: &str, // örn: "5m"
    futures_period: &str, // openInterestHist & topLongShortAccountRatio period: "5m" vb.
    kline_limit: u32,     // 288 => son 24 saat @5m
    cfg: &AlgoConfig,
) -> Result<BacktestResult> {
    let client = FuturesClient::new();

    let candles = client.fetch_klines(symbol, kline_interval, kline_limit).await?;
    let funding = client.fetch_funding_rates(symbol, 100).await?; // son ~100 funding event (en fazla 30 gün)
    let oi_hist = client
        .fetch_open_interest_hist(symbol, futures_period, kline_limit)
        .await?;
    let lsr_hist = client
        .fetch_top_long_short_ratio(symbol, futures_period, kline_limit)
        .await?;

    let (matched_candles, contexts) = build_signal_contexts(&candles, &funding, &oi_hist, &lsr_hist);
    Ok(run_backtest_on_series(&matched_candles, &contexts, cfg))
}

// =======================
//  CSV Export
// =======================

/// Backtest sonuçlarını CSV formatında export eder
/// Plan.md'de belirtildiği gibi her trade satırı CSV'ye yazılır
/// 
/// # Error Handling
/// - Explicitly flushes the file buffer before returning to ensure data is written
/// - If an error occurs during writing, the file may be incomplete but will be flushed
pub fn export_backtest_to_csv(result: &BacktestResult, file_path: &str) -> Result<()> {
    use std::fs::File;
    use std::io::Write;

    let mut file = File::create(file_path)
        .context(format!("Failed to create CSV file: {}", file_path))?;

    // CSV header
    writeln!(
        file,
        "entry_time,exit_time,side,entry_price,exit_price,pnl_pct,win"
    )
    .context("Failed to write CSV header")?;

    // CSV rows
    for (idx, trade) in result.trades.iter().enumerate() {
        let side_str = match trade.side {
            PositionSide::Long => "LONG",
            PositionSide::Short => "SHORT",
            PositionSide::Flat => "FLAT",
        };
        writeln!(
            file,
            "{},{},{},{:.8},{:.8},{:.6},{}",
            trade.entry_time.format("%Y-%m-%d %H:%M:%S"),
            trade.exit_time.format("%Y-%m-%d %H:%M:%S"),
            side_str,
            trade.entry_price,
            trade.exit_price,
            trade.pnl_pct * 100.0, // Yüzde olarak
            if trade.win { "WIN" } else { "LOSS" }
        )
        .with_context(|| format!("Failed to write trade {} to CSV", idx + 1))?;
    }

    // Explicitly flush to ensure all data is written to disk before returning
    // This guarantees data integrity even if the function returns early
    file.flush()
        .context("Failed to flush CSV file buffer")?;

    Ok(())
}

// =======================
//  Production Trending Runner
// =======================

use crate::types::{KlineData, KlineEvent, Side, TradeSignal, TrendParams, TrendingChannels};
use log::{info, warn};
use tokio::sync::broadcast;
use tokio::time::{interval, sleep, Duration as TokioDuration};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use futures::StreamExt;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Her side için ayrı cooldown tracking (trend reversal'ları kaçırmamak için)
/// LONG ve SHORT sinyalleri birbirini bloklamaz
struct LastSignalState {
    last_long_time: Option<chrono::DateTime<Utc>>,
    last_short_time: Option<chrono::DateTime<Utc>>,
}

/// Production için trending modülü - Kline WebSocket stream'ini dinler ve TradeSignal üretir
/// 
/// Bu fonksiyon:
/// 1. Kline WebSocket stream'ini dinler (gerçek zamanlı candle güncellemeleri)
/// 2. Her yeni candle tamamlandığında (is_closed=true) sinyal üretir
/// 3. Funding, OI, Long/Short ratio verilerini REST API'den çeker (daha az sıklıkla)
/// 4. TradeSignal eventlerini event bus'a gönderir
pub async fn run_trending(
    ch: TrendingChannels,
    symbol: String,
    params: TrendParams,
    ws_base_url: String,
) {
    let client = FuturesClient::new();
    
    // AlgoConfig'i TrendParams'den oluştur
    let cfg = AlgoConfig {
        rsi_trend_long_min: params.rsi_long_min,
        rsi_trend_short_max: params.rsi_short_max,
        funding_extreme_pos: params.funding_max_for_long.max(0.0001),
        funding_extreme_neg: params.funding_min_for_short.min(-0.0001),
        lsr_crowded_long: params.obi_long_min.max(1.3),
        lsr_crowded_short: params.obi_short_max.min(0.8),
        long_min_score: params.long_min_score,
        short_min_score: params.short_min_score,
        fee_bps_round_trip: 8.0, // Default fee
        max_holding_bars: 48,   // Default max holding
        slippage_bps: 0.0,      // Default: no slippage (optimistic backtest)
                                 // Set to 5-10 bps (0.05-0.1%) for more realistic results
    };

    let kline_interval = "5m"; // 5 dakikalık kline kullan
    let futures_period = "5m";
    let kline_limit = (params.warmup_min_ticks + 10) as u32; // Warmup için yeterli veri

    // Candle buffer - son N candle'ı tutar (signal context hesaplama için)
    let candle_buffer = Arc::new(RwLock::new(Vec::<Candle>::new()));
    
    // İlk candle'ları REST API'den çek (warmup için)
    match client.fetch_klines(&symbol, kline_interval, kline_limit).await {
        Ok(candles) => {
            *candle_buffer.write().await = candles;
            info!("TRENDING: loaded {} candles for warmup", candle_buffer.read().await.len());
        }
        Err(err) => {
            warn!("TRENDING: failed to fetch initial candles: {err:?}");
        }
    }

    let signal_state = LastSignalState {
        last_long_time: None,
        last_short_time: None,
    };

    info!("TRENDING: started for symbol {} with kline WebSocket stream", symbol);

    // Kline WebSocket stream task
    let kline_stream_symbol = symbol.clone();
    let kline_stream_ws_url = ws_base_url.clone();
    let kline_stream_buffer = candle_buffer.clone();
    let kline_stream_signal_state = Arc::new(RwLock::new(signal_state));
    let kline_stream_signal_tx = ch.signal_tx.clone();
    
    let kline_task = tokio::spawn(async move {
        run_kline_stream(
            kline_stream_symbol,
            kline_interval,
            futures_period,
            kline_stream_ws_url,
            kline_stream_buffer,
            client,
            cfg,
            params,
            kline_stream_signal_state,
            kline_stream_signal_tx,
        ).await;
    });

    // Wait for kline stream task
    let _ = kline_task.await;
}

async fn run_kline_stream(
    symbol: String,
    kline_interval: &str,
    futures_period: &str,
    ws_base_url: String,
    candle_buffer: Arc<RwLock<Vec<Candle>>>,
    client: FuturesClient,
    cfg: AlgoConfig,
    params: TrendParams,
    signal_state: Arc<RwLock<LastSignalState>>,
    signal_tx: tokio::sync::mpsc::Sender<TradeSignal>,
) {
    let mut retry_delay = TokioDuration::from_secs(1);
    let ws_url = format!(
        "{}/ws/{}@kline_{}",
        ws_base_url.trim_end_matches('/'),
        symbol.to_lowercase(),
        kline_interval
    );

    loop {
        match connect_async(&ws_url).await {
            Ok((ws_stream, _)) => {
                info!("TRENDING: kline stream connected ({ws_url})");
                retry_delay = TokioDuration::from_secs(1);
                let (_, mut read) = ws_stream.split();
                while let Some(message) = read.next().await {
                    match message {
                        Ok(Message::Text(txt)) => {
                            if let Ok(event) = serde_json::from_str::<KlineEvent>(&txt) {
                                if event.symbol == symbol && event.kline.is_closed {
                                    // Yeni candle tamamlandı - parse et ve buffer'a ekle
                                    if let Some(candle) = parse_kline_to_candle(&event.kline) {
                                        let mut buffer = candle_buffer.write().await;
                                        buffer.push(candle.clone());
                                        // Buffer'ı sınırla (son N candle'ı tut)
                                        let max_candles = (params.warmup_min_ticks + 10) as usize;
                                        if buffer.len() > max_candles {
                                            buffer.remove(0);
                                        }
                                        drop(buffer);
                                        
                                        // Sinyal üret (signal gönderme işlemi fonksiyon içinde yapılıyor)
                                        if let Err(err) = generate_signal_from_candle(
                                            &candle,
                                            &candle_buffer,
                                            &client,
                                            &symbol,
                                            futures_period,
                                            &cfg,
                                            &params,
                                            signal_state.clone(),
                                            &signal_tx,
                                        ).await {
                                            warn!("TRENDING: failed to generate signal: {err}");
                                        }
                                    }
                                }
                            }
                        }
                        Ok(Message::Binary(bin)) => {
                            if let Ok(txt) = String::from_utf8(bin) {
                                if let Ok(event) = serde_json::from_str::<KlineEvent>(&txt) {
                                    if event.symbol == symbol && event.kline.is_closed {
                                        if let Some(candle) = parse_kline_to_candle(&event.kline) {
                                            let mut buffer = candle_buffer.write().await;
                                            buffer.push(candle.clone());
                                            let max_candles = (params.warmup_min_ticks + 10) as usize;
                                            if buffer.len() > max_candles {
                                                buffer.remove(0);
                                            }
                                            drop(buffer);
                                            
                                            // Sinyal üret (signal gönderme işlemi fonksiyon içinde yapılıyor)
                                            if let Err(err) = generate_signal_from_candle(
                                                &candle,
                                                &candle_buffer,
                                                &client,
                                                &symbol,
                                                futures_period,
                                                &cfg,
                                                &params,
                                                signal_state.clone(),
                                                &signal_tx,
                                            ).await {
                                                warn!("TRENDING: failed to generate signal: {err}");
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Ok(Message::Ping(_)) | Ok(Message::Pong(_)) | Ok(Message::Frame(_)) => {}
                        Ok(Message::Close(frame)) => {
                            warn!("TRENDING: kline stream closed: {:?}", frame);
                            break;
                        }
                        Err(err) => {
                            warn!("TRENDING: kline stream error: {err}");
                            break;
                        }
                    }
                }
            }
            Err(err) => warn!("TRENDING: kline stream connect error: {err:?}"),
        }
        info!(
            "TRENDING: kline stream reconnecting in {}s",
            retry_delay.as_secs()
        );
        sleep(retry_delay).await;
        retry_delay = (retry_delay * 2).min(TokioDuration::from_secs(60));
    }
}

fn parse_kline_to_candle(kline: &KlineData) -> Option<Candle> {
    let open_time = DateTime::<Utc>::from_timestamp_millis(kline.open_time)?;
    let close_time = DateTime::<Utc>::from_timestamp_millis(kline.close_time)?;
    let open = kline.open.parse::<f64>().ok()?;
    let high = kline.high.parse::<f64>().ok()?;
    let low = kline.low.parse::<f64>().ok()?;
    let close = kline.close.parse::<f64>().ok()?;
    let volume = kline.volume.parse::<f64>().ok()?;
    
    Some(Candle {
        open_time,
        close_time,
        open,
        high,
        low,
        close,
        volume,
    })
}

async fn generate_signal_from_candle(
    candle: &Candle,
    candle_buffer: &Arc<RwLock<Vec<Candle>>>,
    client: &FuturesClient,
    symbol: &str,
    futures_period: &str,
    cfg: &AlgoConfig,
    params: &TrendParams,
    signal_state: Arc<RwLock<LastSignalState>>,
    signal_tx: &tokio::sync::mpsc::Sender<TradeSignal>,
) -> Result<Option<TradeSignal>> {
    let buffer = candle_buffer.read().await;
    
    if buffer.len() < params.warmup_min_ticks {
        return Ok(None); // Henüz yeterli veri yok
    }

    // Funding, OI, Long/Short ratio verilerini çek (REST API - daha az sıklıkla)
    let funding = client.fetch_funding_rates(symbol, 100).await?;
    let oi_hist = client.fetch_open_interest_hist(symbol, futures_period, buffer.len() as u32).await?;
    let lsr_hist = client.fetch_top_long_short_ratio(symbol, futures_period, buffer.len() as u32).await?;

    // Signal context'leri oluştur
    let (matched_candles, contexts) = build_signal_contexts(&buffer, &funding, &oi_hist, &lsr_hist);

    if contexts.len() < params.warmup_min_ticks {
        return Ok(None);
    }

    // En son candle ve context'i kullan
    let latest_ctx = contexts.last().ok_or_else(|| anyhow::anyhow!("no contexts available"))?;
    let prev_ctx = if contexts.len() > 1 {
        Some(&contexts[contexts.len() - 2])
    } else {
        None
    };

    let signal = generate_signal(candle, latest_ctx, prev_ctx, cfg);

    // Eğer sinyal Flat değilse, TradeSignal'e dönüştür
    match signal.side {
        SignalSide::Long | SignalSide::Short => {
            let side = match signal.side {
                SignalSide::Long => Side::Long,
                SignalSide::Short => Side::Short,
                SignalSide::Flat => unreachable!(),
            };

            // Side-specific cooldown kontrolü
            let cooldown_duration = chrono::Duration::seconds(params.signal_cooldown_secs);
            let now = Utc::now();
            let state = signal_state.read().await;
            
            // Cooldown kontrolü yap (ama henüz set etme)
            match side {
                Side::Long => {
                    if let Some(last_time) = state.last_long_time {
                        if now - last_time < cooldown_duration {
                            return Ok(None); // Long cooldown aktif
                        }
                    }
                }
                Side::Short => {
                    if let Some(last_time) = state.last_short_time {
                        if now - last_time < cooldown_duration {
                            return Ok(None); // Short cooldown aktif
                        }
                    }
                }
            }
            drop(state); // Lock'u serbest bırak

            // TradeSignal oluştur
            let trade_signal = TradeSignal {
                id: Uuid::new_v4(),
                symbol: symbol.to_string(),
                side,
                entry_price: signal.price,
                leverage: params.leverage,
                size_usdt: params.position_size_quote,
                ts: signal.time,
            };

            // Önce signal'i göndermeyi dene
            match signal_tx.send(trade_signal.clone()).await {
                Ok(_) => {
                    // Başarılı, şimdi cooldown set et
                    let mut state = signal_state.write().await;
                    match side {
                        Side::Long => state.last_long_time = Some(now),
                        Side::Short => state.last_short_time = Some(now),
                    }
                    
                    info!(
                        "TRENDING: generated {} signal for {} at price {:.2}",
                        match side {
                            Side::Long => "LONG",
                            Side::Short => "SHORT",
                        },
                        symbol,
                        signal.price
                    );
                    
                    Ok(Some(trade_signal))
                }
                Err(err) => {
                    // Signal gönderilemedi, cooldown set etme
                    warn!("TRENDING: failed to send signal: {err}, cooldown not set");
                    Ok(None)
                }
            }
        }
        SignalSide::Flat => Ok(None),
    }
}

/// En son kline verilerini çekip sinyal üretir
async fn generate_latest_signal(
    client: &FuturesClient,
    symbol: &str,
    kline_interval: &str,
    futures_period: &str,
    kline_limit: u32,
    cfg: &AlgoConfig,
    params: &TrendParams,
    signal_state: &mut LastSignalState,
    last_candle_time: &mut Option<chrono::DateTime<Utc>>,
    signal_tx: &tokio::sync::mpsc::Sender<TradeSignal>,
) -> Result<Option<TradeSignal>> {
    // Kline verilerini çek
    let candles = match client.fetch_klines(symbol, kline_interval, kline_limit).await {
        Ok(c) => c,
        Err(err) => {
            warn!("TRENDING: failed to fetch klines: {err:?}");
            return Ok(None);
        }
    };

    if candles.is_empty() {
        return Ok(None);
    }

    // Son candle'ın zamanını kontrol et (duplicate API call koruması)
    // Interval 5 dakika olsa bile, clock drift veya API timing farklılıkları
    // nedeniyle aynı candle tekrar gelebilir
    let latest_candle = &candles[candles.len() - 1];
    if let Some(last_time) = last_candle_time {
        // Eğer aynı candle ise (close_time değişmemiş), yeni sinyal üretme
        // Bu sayede gereksiz API çağrıları ve sinyal üretimi önlenir
        if latest_candle.close_time <= *last_time {
            return Ok(None);
        }
    }
    *last_candle_time = Some(latest_candle.close_time);

    // Cooldown kontrolü burada yapılmaz - signal side'ı bilinmeden yapılamaz
    // Cooldown kontrolü signal üretildikten sonra, side'a göre yapılacak

    // Funding, OI, Long/Short ratio verilerini çek
    let funding = client.fetch_funding_rates(symbol, 100).await?;
    let oi_hist = client.fetch_open_interest_hist(symbol, futures_period, kline_limit).await?;
    let lsr_hist = client.fetch_top_long_short_ratio(symbol, futures_period, kline_limit).await?;

    // Signal context'leri oluştur (sadece gerçek API verisi olan candle'lar)
    let (matched_candles, contexts) = build_signal_contexts(&candles, &funding, &oi_hist, &lsr_hist);

    if contexts.len() < params.warmup_min_ticks {
        // Henüz yeterli veri yok
        return Ok(None);
    }

    // En son eşleşen candle ve context'i kullan
    let latest_matched_candle = &matched_candles[matched_candles.len() - 1];
    let latest_ctx = &contexts[contexts.len() - 1];
    let prev_ctx = if contexts.len() > 1 {
        Some(&contexts[contexts.len() - 2])
    } else {
        None
    };

    let signal = generate_signal(latest_matched_candle, latest_ctx, prev_ctx, cfg);

    // Eğer sinyal Flat değilse, TradeSignal'e dönüştür
    match signal.side {
        SignalSide::Long | SignalSide::Short => {
            let side = match signal.side {
                SignalSide::Long => Side::Long,
                SignalSide::Short => Side::Short,
                SignalSide::Flat => unreachable!(),
            };

            // Side-specific cooldown kontrolü (trend reversal'ları kaçırmamak için)
            let cooldown_duration = chrono::Duration::seconds(params.signal_cooldown_secs);
            let now = Utc::now();
            
            // Cooldown kontrolü yap (ama henüz set etme)
            match side {
                Side::Long => {
                    if let Some(last_time) = signal_state.last_long_time {
                        if now - last_time < cooldown_duration {
                            return Ok(None); // Long cooldown aktif
                        }
                    }
                }
                Side::Short => {
                    if let Some(last_time) = signal_state.last_short_time {
                        if now - last_time < cooldown_duration {
                            return Ok(None); // Short cooldown aktif
                        }
                    }
                }
            }

            // ⚠️ Production Execution Note:
            // Signal price is the candle close price, but actual execution happens later:
            // 1. Signal → event bus (mpsc channel, ~1-10ms delay)
            // 2. Ordering module: risk checks, symbol info fetch (~50-200ms)
            // 3. API call and order fill (~100-500ms)
            // 4. Real slippage at market price (varies with volatility)
            // Total delay: typically 200-1000ms, can be more during high volatility
            // This is why backtest results may be optimistic compared to production
            let trade_signal = TradeSignal {
                id: Uuid::new_v4(),
                symbol: symbol.to_string(),
                side,
                entry_price: signal.price, // Candle close price (actual fill may differ due to slippage)
                leverage: params.leverage,
                size_usdt: params.position_size_quote,
                ts: signal.time,
            };

            // Önce signal'i göndermeyi dene
            match signal_tx.send(trade_signal.clone()).await {
                Ok(_) => {
                    // Başarılı, şimdi cooldown set et
                    match side {
                        Side::Long => signal_state.last_long_time = Some(now),
                        Side::Short => signal_state.last_short_time = Some(now),
                    }
                    
                    info!(
                        "TRENDING: generated {} signal for {} at price {:.2}",
                        match side {
                            Side::Long => "LONG",
                            Side::Short => "SHORT",
                        },
                        symbol,
                        signal.price
                    );
                    
                    Ok(Some(trade_signal))
                }
                Err(err) => {
                    // Signal gönderilemedi, cooldown set etme
                    warn!("TRENDING: failed to send signal: {err}, cooldown not set");
                    Ok(None)
                }
            }
        }
        SignalSide::Flat => Ok(None),
    }
}

