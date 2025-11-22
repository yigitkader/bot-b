// ✅ Plan.md: Portfolio Backtest - Tüm coinleri zaman ekseninde birlikte yürüt
// Gerçek portfolio backtest: Ortak RiskManager ile risk limitlerini kontrol eder

use std::collections::HashMap;
use chrono::{DateTime, Utc};

use crate::types::{Candle, SignalContext, BacktestResult, Trade, PositionSide, AlgoConfig, ForceOrderRecord, SignalSide};
use crate::trending::{
    FundingArbitrage, LiquidationMap, VolumeProfile,
    build_liquidation_map_from_force_orders, create_mtf_analysis, generate_signal_enhanced,
};
use crate::risk_manager::RiskLimits;

/// Portfolio Backtest için veri yapısı
#[derive(Debug, Clone)]
pub struct PortfolioCandleData {
    pub symbol: String,
    pub candles: Vec<Candle>,
    pub contexts: Vec<SignalContext>,
    pub force_orders: Option<Vec<ForceOrderRecord>>,
}

/// Her symbol için analiz objeleri
struct SymbolAnalyzers {
    funding_arbitrage: FundingArbitrage,
    liquidation_map: LiquidationMap,
    volume_profile: Option<VolumeProfile>,
}

/// Portfolio Backtest için pozisyon bilgisi
#[derive(Debug, Clone)]
struct PortfolioPosition {
    _symbol: String,
    side: PositionSide,
    entry_price: f64,
    entry_time: DateTime<Utc>,
    size: f64,
    leverage: f64,
    entry_index: usize, // Candle index where position was opened
    atr_at_entry: f64,
}

/// Portfolio Backtest sonucu
#[derive(Debug, Clone)]
pub struct PortfolioBacktestResult {
    pub total_trades: usize,
    pub win_trades: usize,
    pub loss_trades: usize,
    pub win_rate: f64,
    pub total_pnl_pct: f64,
    pub avg_pnl_pct: f64,
    pub avg_r: f64,
    pub trades: Vec<Trade>,
    pub symbol_results: HashMap<String, BacktestResult>,
    pub portfolio_metrics: PortfolioMetrics,
}

#[derive(Debug, Clone)]
pub struct PortfolioMetrics {
    pub max_portfolio_drawdown_pct: f64,
    pub max_daily_drawdown_pct: f64,
    pub max_weekly_drawdown_pct: f64,
    pub total_notional_peak: f64,
    pub correlation_warnings: usize,
    pub risk_limit_rejections: usize,
}

/// ✅ Plan.md: Portfolio Backtest - Tüm coinleri zaman ekseninde birlikte yürüt
/// Ortak RiskManager ile risk limitlerini kontrol eder
pub fn run_portfolio_backtest(
    symbol_data: Vec<PortfolioCandleData>,
    cfg: &AlgoConfig,
    risk_limits: &RiskLimits,
    initial_equity: f64,
) -> PortfolioBacktestResult {
    if symbol_data.is_empty() {
        return empty_portfolio_result();
    }

    // 1. Tüm candle'ları zaman ekseninde hizala
    let time_aligned_data = align_candles_by_time(&symbol_data);
    
    if time_aligned_data.is_empty() {
        return empty_portfolio_result();
    }

    // 2. Portfolio state
    let mut positions: HashMap<String, PortfolioPosition> = HashMap::new();
    let mut equity = initial_equity;
    let mut peak_equity = initial_equity;
    let mut max_portfolio_drawdown = 0.0;
    let mut max_daily_drawdown = 0.0;
    let mut max_weekly_drawdown = 0.0;
    let mut daily_equity_peak = initial_equity;
    let mut weekly_equity_peak = initial_equity;
    let mut last_daily_reset = time_aligned_data[0].timestamp.date_naive();
    let mut last_weekly_reset = time_aligned_data[0].timestamp.date_naive();
    
    let mut all_trades: Vec<Trade> = Vec::new();
    let mut symbol_trades: HashMap<String, Vec<Trade>> = HashMap::new();
    let mut risk_limit_rejections = 0;
    let correlation_warnings = 0;
    let mut total_notional_peak = 0.0;

    // 3. Her symbol için analiz objelerini oluştur (FundingArbitrage, LiquidationMap, VolumeProfile)
    let mut symbol_analyzers: HashMap<String, SymbolAnalyzers> = HashMap::new();
    for data in &symbol_data {
        let mut funding_arbitrage = FundingArbitrage::new();
        for (candle, ctx) in data.candles.iter().zip(data.contexts.iter()) {
            funding_arbitrage.update_funding(ctx.funding_rate, candle.close_time);
        }

        let liquidation_map = if let Some(force_orders) = &data.force_orders {
            if !force_orders.is_empty() && !data.candles.is_empty() {
                let initial_oi = data.contexts.first().map(|c| c.open_interest).unwrap_or(0.0);
                build_liquidation_map_from_force_orders(force_orders, data.candles[0].close, initial_oi)
            } else {
                LiquidationMap::new()
            }
        } else {
            LiquidationMap::new()
        };

        let volume_profile = if data.candles.len() >= 50 {
            Some(VolumeProfile::calculate_volume_profile(
                &data.candles[data.candles.len().saturating_sub(100)..],
            ))
        } else {
            None
        };

        symbol_analyzers.insert(data.symbol.clone(), SymbolAnalyzers {
            funding_arbitrage,
            liquidation_map,
            volume_profile,
        });
    }

    // 4. Her zaman adımında tüm symbol'leri işle
    for time_step in &time_aligned_data {
        let current_time = time_step.timestamp;
        let current_date = current_time.date_naive();
        
        // Daily/Weekly reset kontrolü
        if current_date != last_daily_reset {
            daily_equity_peak = equity;
            last_daily_reset = current_date;
        }
        // Weekly reset: 7 gün geçtiyse reset et
        let days_since_weekly_reset = (current_date - last_weekly_reset).num_days();
        if days_since_weekly_reset >= 7 {
            weekly_equity_peak = equity;
            last_weekly_reset = current_date;
        }

        // Equity güncelleme ve drawdown hesaplama
        if equity > peak_equity {
            peak_equity = equity;
        }
        if equity > daily_equity_peak {
            daily_equity_peak = equity;
        }
        if equity > weekly_equity_peak {
            weekly_equity_peak = equity;
        }

        let portfolio_dd = (peak_equity - equity) / peak_equity;
        let daily_dd = (daily_equity_peak - equity) / daily_equity_peak;
        let weekly_dd = (weekly_equity_peak - equity) / weekly_equity_peak;

        if portfolio_dd > max_portfolio_drawdown {
            max_portfolio_drawdown = portfolio_dd;
        }
        if daily_dd > max_daily_drawdown {
            max_daily_drawdown = daily_dd;
        }
        if weekly_dd > max_weekly_drawdown {
            max_weekly_drawdown = weekly_dd;
        }

        // Drawdown limit kontrolü
        let can_trade = daily_dd <= risk_limits.max_daily_drawdown_pct 
            && weekly_dd <= risk_limits.max_weekly_drawdown_pct;

        // Total notional hesapla
        let total_notional: f64 = positions.values()
            .map(|p| p.size * p.entry_price * p.leverage)
            .sum();
        if total_notional > total_notional_peak {
            total_notional_peak = total_notional;
        }

        // 4. Her symbol için pozisyon yönetimi
        for (symbol, candle_opt, context_opt, index) in &time_step.symbol_data {
            let Some(candle) = candle_opt else { continue; };
            let Some(ctx) = context_opt else { continue; };
            
            // Mevcut pozisyon kontrolü ve kapatma
            if let Some(pos) = positions.get(symbol).cloned() {
                let should_close = check_position_exit(
                    &pos,
                    candle,
                    ctx,
                    *index,
                    cfg,
                );

                if should_close {
                    // Pozisyonu kapat
                    let exit_price = candle.close;
                    let pnl_pct = calculate_pnl_pct(
                        pos.entry_price,
                        exit_price,
                        pos.side,
                        cfg.fee_bps_round_trip / 10_000.0,
                        cfg.slippage_bps / 10_000.0,
                        pos.atr_at_entry,
                        ctx.atr,
                    );

                    let trade = Trade {
                        entry_time: pos.entry_time,
                        exit_time: candle.close_time,
                        side: pos.side,
                        entry_price: pos.entry_price,
                        exit_price,
                        pnl_pct,
                        win: pnl_pct > 0.0,
                    };

                    all_trades.push(trade.clone());
                    symbol_trades.entry(symbol.clone()).or_insert_with(Vec::new).push(trade);

                    // Equity güncelle
                    equity += equity * pnl_pct;

                    // Pozisyonu kaldır
                    positions.remove(symbol);
                }
            }

            // Yeni pozisyon açma kontrolü
            if can_trade && !positions.contains_key(symbol) {
                // Risk kontrolü: Portfolio seviyesinde
                let (can_open, _rejection_reason) = check_portfolio_risk_limits(
                    symbol,
                    &positions,
                    risk_limits,
                    equity,
                    ctx.atr,
                    candle.close,
                );

                if !can_open {
                    risk_limit_rejections += 1;
                    continue;
                }

                // ✅ Plan.md: generate_signal_enhanced kullan (gerçek sinyal üretimi)
                let signal_side = if let Some(analyzers) = symbol_analyzers.get(symbol) {
                    // Symbol'in original candle ve context verilerini bul
                    let symbol_data_opt = symbol_data.iter().find(|d| d.symbol == *symbol);
                    if let Some(data) = symbol_data_opt {
                        // Current index'e kadar olan candles ve contexts
                        let max_idx = (*index).min(data.candles.len().saturating_sub(1));
                        let candles_slice = &data.candles[..=max_idx];
                        let contexts_slice = &data.contexts[..=max_idx.min(data.contexts.len().saturating_sub(1))];
                        
                        if !candles_slice.is_empty() && !contexts_slice.is_empty() && candles_slice.len() == contexts_slice.len() {
                            let current_candle_idx = candles_slice.len().saturating_sub(1);
                            let current_candle = &candles_slice[current_candle_idx];
                            let current_ctx = &contexts_slice[current_candle_idx];
                            let prev_ctx = if current_candle_idx > 0 {
                                Some(&contexts_slice[current_candle_idx - 1])
                            } else {
                                None
                            };

                            // Funding arbitrage güncelle (current time için)
                            let mut fa = analyzers.funding_arbitrage.clone();
                            fa.update_funding(ctx.funding_rate, candle.close_time);

                            // MTF analysis oluştur
                            let mtf_analysis = if candles_slice.len() >= 50 {
                                Some(create_mtf_analysis(candles_slice, current_ctx))
                            } else {
                                None
                            };

                            // generate_signal_enhanced çağır
                            let signal = generate_signal_enhanced(
                                current_candle,
                                current_ctx,
                                prev_ctx,
                                cfg,
                                candles_slice,
                                contexts_slice,
                                current_candle_idx,
                                Some(&fa),
                                mtf_analysis.as_ref(),
                                None, // OrderFlow backtestte kapalı
                                Some(&analyzers.liquidation_map),
                                analyzers.volume_profile.as_ref(),
                                None, // MarketTick backtestte yok
                                true, // ✅ BACKTEST MODE: Only use reliable strategies
                            );

                            // Signal'i PositionSide'a çevir
                            match signal.side {
                                SignalSide::Long => Some(PositionSide::Long),
                                SignalSide::Short => Some(PositionSide::Short),
                                SignalSide::Flat => None,
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };

                if let Some(side) = signal_side {
                    // Position size hesapla
                    let risk_per_trade = equity * risk_limits.risk_per_trade_pct;
                    let stop_distance_pct = (ctx.atr * cfg.atr_stop_loss_multiplier) / candle.close;
                    let size_usdt = risk_per_trade / stop_distance_pct;
                    let leverage = 10.0;
                    let notional = size_usdt * leverage;

                    // Final risk kontrolü
                    let current_total_notional: f64 = positions.values()
                        .map(|p| p.size * p.entry_price * p.leverage)
                        .sum();
                    
                    if current_total_notional + notional > risk_limits.max_total_notional_usd {
                        risk_limit_rejections += 1;
                        continue;
                    }

                    // Pozisyon aç
                    let position = PortfolioPosition {
                        _symbol: symbol.clone(),
                        side,
                        entry_price: candle.close,
                        entry_time: candle.close_time,
                        size: size_usdt,
                        leverage,
                        entry_index: *index,
                        atr_at_entry: ctx.atr,
                    };

                    positions.insert(symbol.clone(), position);
                }
            }
        }
    }

    // 5. Kalan pozisyonları kapat
    for (symbol, pos) in positions {
        if let Some(data) = symbol_data.iter().find(|d| d.symbol == symbol) {
            if let Some(last_candle) = data.candles.last() {
                if let Some(last_ctx) = data.contexts.last() {
                    let exit_price = last_candle.close;
                    let pnl_pct = calculate_pnl_pct(
                        pos.entry_price,
                        exit_price,
                        pos.side,
                        cfg.fee_bps_round_trip / 10_000.0,
                        cfg.slippage_bps / 10_000.0,
                        pos.atr_at_entry,
                        last_ctx.atr,
                    );

                    let trade = Trade {
                        entry_time: pos.entry_time,
                        exit_time: last_candle.close_time,
                        side: pos.side,
                        entry_price: pos.entry_price,
                        exit_price,
                        pnl_pct,
                        win: pnl_pct > 0.0,
                    };

                    all_trades.push(trade.clone());
                    symbol_trades.entry(symbol.clone()).or_insert_with(Vec::new).push(trade);
                }
            }
        }
    }

    // 6. Sonuçları hesapla
    calculate_portfolio_results(all_trades, symbol_trades, max_portfolio_drawdown, max_daily_drawdown, max_weekly_drawdown, total_notional_peak, correlation_warnings, risk_limit_rejections)
}

fn empty_portfolio_result() -> PortfolioBacktestResult {
    PortfolioBacktestResult {
        total_trades: 0,
        win_trades: 0,
        loss_trades: 0,
        win_rate: 0.0,
        total_pnl_pct: 0.0,
        avg_pnl_pct: 0.0,
        avg_r: 0.0,
        trades: Vec::new(),
        symbol_results: HashMap::new(),
        portfolio_metrics: PortfolioMetrics {
            max_portfolio_drawdown_pct: 0.0,
            max_daily_drawdown_pct: 0.0,
            max_weekly_drawdown_pct: 0.0,
            total_notional_peak: 0.0,
            correlation_warnings: 0,
            risk_limit_rejections: 0,
        },
    }
}

fn calculate_portfolio_results(
    all_trades: Vec<Trade>,
    symbol_trades: HashMap<String, Vec<Trade>>,
    max_portfolio_drawdown: f64,
    max_daily_drawdown: f64,
    max_weekly_drawdown: f64,
    total_notional_peak: f64,
    correlation_warnings: usize,
    risk_limit_rejections: usize,
) -> PortfolioBacktestResult {
    let total_trades = all_trades.len();
    let win_trades = all_trades.iter().filter(|t| t.win).count();
    let loss_trades = total_trades - win_trades;
    let win_rate = if total_trades > 0 {
        win_trades as f64 / total_trades as f64
    } else {
        0.0
    };

    let total_pnl_pct = all_trades.iter().map(|t| t.pnl_pct).sum();
    let avg_pnl_pct = if total_trades > 0 {
        total_pnl_pct / total_trades as f64
    } else {
        0.0
    };

    let wins: Vec<f64> = all_trades.iter().filter(|t| t.win).map(|t| t.pnl_pct).collect();
    let losses: Vec<f64> = all_trades.iter().filter(|t| !t.win).map(|t| t.pnl_pct.abs()).collect();
    
    let avg_r = if !losses.is_empty() && !wins.is_empty() {
        let avg_win = wins.iter().sum::<f64>() / wins.len() as f64;
        let avg_loss = losses.iter().sum::<f64>() / losses.len() as f64;
        if avg_loss > 0.0 {
            avg_win / avg_loss
        } else {
            0.0
        }
    } else {
        0.0
    };

    // Symbol bazlı sonuçlar
    let mut symbol_results = HashMap::new();
    for (symbol, trades) in symbol_trades {
        let symbol_win_trades = trades.iter().filter(|t| t.win).count();
        let symbol_total_pnl = trades.iter().map(|t| t.pnl_pct).sum();
        symbol_results.insert(symbol.clone(), BacktestResult {
            trades: trades.clone(),
            total_trades: trades.len(),
            win_trades: symbol_win_trades,
            loss_trades: trades.len() - symbol_win_trades,
            win_rate: if trades.len() > 0 {
                symbol_win_trades as f64 / trades.len() as f64
            } else {
                0.0
            },
            total_pnl_pct: symbol_total_pnl,
            avg_pnl_pct: if trades.len() > 0 {
                symbol_total_pnl / trades.len() as f64
            } else {
                0.0
            },
            avg_r: 0.0,
            total_signals: 0,
            long_signals: 0,
            short_signals: 0,
        });
    }

    PortfolioBacktestResult {
        total_trades,
        win_trades,
        loss_trades,
        win_rate,
        total_pnl_pct,
        avg_pnl_pct,
        avg_r,
        trades: all_trades,
        symbol_results,
        portfolio_metrics: PortfolioMetrics {
            max_portfolio_drawdown_pct: max_portfolio_drawdown,
            max_daily_drawdown_pct: max_daily_drawdown,
            max_weekly_drawdown_pct: max_weekly_drawdown,
            total_notional_peak,
            correlation_warnings,
            risk_limit_rejections,
        },
    }
}

/// Candle'ları zaman ekseninde hizala
fn align_candles_by_time(
    symbol_data: &[PortfolioCandleData],
) -> Vec<TimeAlignedStep> {
    // Tüm unique timestamp'leri topla
    let mut all_timestamps: Vec<DateTime<Utc>> = Vec::new();
    for data in symbol_data {
        for candle in &data.candles {
            all_timestamps.push(candle.close_time);
        }
    }
    all_timestamps.sort();
    all_timestamps.dedup();

    // Her timestamp için tüm symbol'lerin candle'larını bul
    let mut aligned_steps = Vec::new();
    for timestamp in all_timestamps {
        let mut step_data = Vec::new();
        
        for data in symbol_data {
            // Bu timestamp'e en yakın candle'ı bul
            let mut best_candle: Option<&Candle> = None;
            let mut best_ctx: Option<&SignalContext> = None;
            let mut best_index = 0;
            let mut best_diff = i64::MAX;

            for (idx, candle) in data.candles.iter().enumerate() {
                let diff = (candle.close_time.timestamp_millis() - timestamp.timestamp_millis()).abs();
                if diff < best_diff {
                    best_diff = diff;
                    best_candle = Some(candle);
                    if idx < data.contexts.len() {
                        best_ctx = Some(&data.contexts[idx]);
                    }
                    best_index = idx;
                }
            }

            // 5 dakika içindeyse kabul et
            if best_diff < 5 * 60 * 1000 {
                step_data.push((
                    data.symbol.clone(),
                    best_candle.cloned(),
                    best_ctx.cloned(),
                    best_index,
                ));
            }
        }

        if !step_data.is_empty() {
            aligned_steps.push(TimeAlignedStep {
                timestamp,
                symbol_data: step_data,
            });
        }
    }

    aligned_steps
}

#[derive(Debug)]
struct TimeAlignedStep {
    timestamp: DateTime<Utc>,
    symbol_data: Vec<(String, Option<Candle>, Option<SignalContext>, usize)>,
}

/// Pozisyon kapatma kontrolü
fn check_position_exit(
    pos: &PortfolioPosition,
    candle: &Candle,
    ctx: &SignalContext,
    current_index: usize,
    cfg: &AlgoConfig,
) -> bool {
    let price_change_pct = (candle.close - pos.entry_price) / pos.entry_price;
    let pnl_pct = match pos.side {
        PositionSide::Long => price_change_pct,
        PositionSide::Short => -price_change_pct,
        PositionSide::Flat => return true,
    };

    // Stop Loss / Take Profit
    let atr_sl = (ctx.atr * cfg.atr_stop_loss_multiplier) / pos.entry_price;
    let atr_tp = (ctx.atr * cfg.atr_take_profit_multiplier) / pos.entry_price;

    let hit_sl = match pos.side {
        PositionSide::Long => pnl_pct <= -atr_sl,
        PositionSide::Short => pnl_pct <= -atr_sl,
        PositionSide::Flat => false,
    };

    let hit_tp = match pos.side {
        PositionSide::Long => pnl_pct >= atr_tp,
        PositionSide::Short => pnl_pct >= atr_tp,
        PositionSide::Flat => false,
    };

    // Max holding time
    let bars_held = current_index.saturating_sub(pos.entry_index);
    let max_holding = cfg.max_holding_bars;

    hit_sl || hit_tp || (bars_held >= max_holding)
}

/// PnL hesaplama
fn calculate_pnl_pct(
    entry_price: f64,
    exit_price: f64,
    side: PositionSide,
    fee_frac: f64,
    slippage_frac: f64,
    atr_at_entry: f64,
    atr_at_exit: f64,
) -> f64 {
    let price_change = (exit_price - entry_price) / entry_price;
    let raw_pnl = match side {
        PositionSide::Long => price_change,
        PositionSide::Short => -price_change,
        PositionSide::Flat => 0.0,
    };

    // Slippage (ATR bazlı)
    let volatility_slippage = ((atr_at_entry + atr_at_exit) / 2.0 / entry_price) * slippage_frac * 10.0;
    let total_slippage = slippage_frac + volatility_slippage;

    raw_pnl - fee_frac - total_slippage
}

/// Portfolio risk limit kontrolü
fn check_portfolio_risk_limits(
    symbol: &str,
    positions: &HashMap<String, PortfolioPosition>,
    risk_limits: &RiskLimits,
    equity: f64,
    atr: f64,
    price: f64,
) -> (bool, String) {
    let total_notional: f64 = positions.values()
        .map(|p| p.size * p.entry_price * p.leverage)
        .sum();

    // Risk per trade hesapla
    let risk_per_trade = equity * risk_limits.risk_per_trade_pct;
    let stop_distance_pct = (atr * 2.5) / price;
    let size_usdt = risk_per_trade / stop_distance_pct;
    let leverage = 10.0;
    let new_notional = size_usdt * leverage;

    // Total notional limit
    if total_notional + new_notional > risk_limits.max_total_notional_usd {
        return (false, format!("Total notional limit exceeded"));
    }

    // Per-symbol limit
    if let Some(existing) = positions.get(symbol) {
        let existing_notional = existing.size * existing.entry_price * existing.leverage;
        if existing_notional + new_notional > risk_limits.max_position_per_symbol_usd {
            return (false, format!("Per-symbol limit exceeded"));
        }
    } else if new_notional > risk_limits.max_position_per_symbol_usd {
        return (false, format!("Per-symbol limit exceeded"));
    }

    // Correlated positions (simplified)
    let same_side_count = positions.values().count();
    if same_side_count >= risk_limits.max_correlated_positions {
        return (false, format!("Max correlated positions limit exceeded"));
    }

    (true, String::new())
}

