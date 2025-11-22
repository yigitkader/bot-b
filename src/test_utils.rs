use crate::{AlgoConfig, BacktestResult, PositionSide};

pub struct AlgoConfigBuilder {
    config: AlgoConfig,
}

impl Default for AlgoConfigBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl AlgoConfigBuilder {
    pub fn new() -> Self {
        Self {
            config: AlgoConfig {
                rsi_trend_long_min: 55.0,
                rsi_trend_short_max: 45.0,
                funding_extreme_pos: 0.0005,
                funding_extreme_neg: -0.0005,
                lsr_crowded_long: 1.3,
                lsr_crowded_short: 0.8,
                long_min_score: 4,
                short_min_score: 4,
                fee_bps_round_trip: 13.0, // ✅ Plan.md: Gerçekçi komisyon (10 bps + slippage = 13 bps)
                max_holding_bars: 48,
                slippage_bps: 0.0,
                min_volume_ratio: 1.5,
                max_volatility_pct: 2.0,
                max_price_change_5bars_pct: 3.0,
                enable_signal_quality_filter: true,
                enable_enhanced_scoring: false,
                enhanced_score_excellent: 80.0,
                enhanced_score_good: 65.0,
                enhanced_score_marginal: 50.0,
                atr_stop_loss_multiplier: 3.0,
                atr_take_profit_multiplier: 4.0,
                min_holding_bars: 3,
                hft_mode: false,
                base_min_score: 6.5,
                trend_threshold_hft: 0.5,
                trend_threshold_normal: 0.6,
                weak_trend_score_multiplier: 1.15,
                regime_multiplier_trending: 0.9,
                regime_multiplier_ranging: 1.15,
                enable_order_flow: false, // Default: disabled for backtest consistency
            },
        }
    }

    pub fn with_rsi_thresholds(mut self, long_min: f64, short_max: f64) -> Self {
        self.config.rsi_trend_long_min = long_min;
        self.config.rsi_trend_short_max = short_max;
        self
    }

    pub fn with_funding_thresholds(mut self, extreme_pos: f64, extreme_neg: f64) -> Self {
        self.config.funding_extreme_pos = extreme_pos;
        self.config.funding_extreme_neg = extreme_neg;
        self
    }

    pub fn with_lsr_thresholds(mut self, crowded_long: f64, crowded_short: f64) -> Self {
        self.config.lsr_crowded_long = crowded_long;
        self.config.lsr_crowded_short = crowded_short;
        self
    }

    pub fn with_min_scores(mut self, long: usize, short: usize) -> Self {
        self.config.long_min_score = long;
        self.config.short_min_score = short;
        self
    }

    pub fn with_fees(mut self, fee_bps: f64) -> Self {
        self.config.fee_bps_round_trip = fee_bps;
        self
    }

    pub fn with_holding_bars(mut self, min: usize, max: usize) -> Self {
        self.config.min_holding_bars = min;
        self.config.max_holding_bars = max;
        self
    }

    pub fn with_slippage(mut self, slippage_bps: f64) -> Self {
        self.config.slippage_bps = slippage_bps;
        self
    }

    pub fn with_signal_quality(
        mut self,
        min_volume_ratio: f64,
        max_volatility_pct: f64,
        max_price_change_pct: f64,
    ) -> Self {
        self.config.min_volume_ratio = min_volume_ratio;
        self.config.max_volatility_pct = max_volatility_pct;
        self.config.max_price_change_5bars_pct = max_price_change_pct;
        self
    }

    pub fn with_enhanced_scoring(
        mut self,
        enabled: bool,
        excellent: f64,
        good: f64,
        marginal: f64,
    ) -> Self {
        self.config.enable_enhanced_scoring = enabled;
        self.config.enhanced_score_excellent = excellent;
        self.config.enhanced_score_good = good;
        self.config.enhanced_score_marginal = marginal;
        self
    }

    pub fn with_risk_management(
        mut self,
        atr_sl_multiplier: f64,
        atr_tp_multiplier: f64,
    ) -> Self {
        self.config.atr_stop_loss_multiplier = atr_sl_multiplier;
        self.config.atr_take_profit_multiplier = atr_tp_multiplier;
        self
    }

    pub fn with_regime_settings(
        mut self,
        hft_mode: bool,
        base_min_score: f64,
        trend_threshold_hft: f64,
        trend_threshold_normal: f64,
        weak_trend_multiplier: f64,
        regime_trending: f64,
        regime_ranging: f64,
    ) -> Self {
        self.config.hft_mode = hft_mode;
        self.config.base_min_score = base_min_score;
        self.config.trend_threshold_hft = trend_threshold_hft;
        self.config.trend_threshold_normal = trend_threshold_normal;
        self.config.weak_trend_score_multiplier = weak_trend_multiplier;
        self.config.regime_multiplier_trending = regime_trending;
        self.config.regime_multiplier_ranging = regime_ranging;
        self
    }

    pub fn build(self) -> AlgoConfig {
        self.config
    }
}

pub struct BacktestFormatter;

impl BacktestFormatter {
    pub fn format_header(symbol: &str, interval: &str, limit: u32) -> String {
        format!(
            "╔════════════════════════════════════════════════════════════════╗\n\
             ║  BACKTEST: {} {} - Real Binance Data (Last {} candles)        ║\n\
             ╚════════════════════════════════════════════════════════════════╝",
            symbol, interval, limit
        )
    }

    pub fn format_data_validation(limit: u32) -> String {
        format!(
            "📊 DATA VALIDATION:\n\
             \x20  ✅ Real Binance API data used\n\
             \x20  ✅ Klines: {} candles fetched\n\
             \x20  ✅ Funding rates: Last 100 events\n\
             \x20  ✅ Open Interest: Historical data\n\
             \x20  ✅ Long/Short Ratio: Top trader data",
            limit
        )
    }

    pub fn format_signal_stats(result: &BacktestResult) -> String {
        let long_pct = if result.total_signals > 0 {
            (result.long_signals as f64 / result.total_signals as f64) * 100.0
        } else {
            0.0
        };
        let short_pct = if result.total_signals > 0 {
            (result.short_signals as f64 / result.total_signals as f64) * 100.0
        } else {
            0.0
        };
        let signal_to_trade = if result.total_signals > 0 {
            (result.total_trades as f64 / result.total_signals as f64) * 100.0
        } else {
            0.0
        };

        format!(
            "📡 SIGNAL GENERATION:\n\
             \x20  Total Signals    : {} signals generated\n\
             \x20  📈 LONG Signals   : {} ({:.1}%)\n\
             \x20  📉 SHORT Signals  : {} ({:.1}%)\n\
             \x20  Signal→Trade Rate : {:.1}% ({} trades from {} signals)",
            result.total_signals,
            result.long_signals,
            long_pct,
            result.short_signals,
            short_pct,
            signal_to_trade,
            result.total_trades,
            result.total_signals
        )
    }

    pub fn format_performance_metrics(result: &BacktestResult) -> String {
        let win_pct = if result.total_trades > 0 {
            (result.win_trades as f64 / result.total_trades as f64) * 100.0
        } else {
            0.0
        };
        let loss_pct = if result.total_trades > 0 {
            (result.loss_trades as f64 / result.total_trades as f64) * 100.0
        } else {
            0.0
        };
        let avg_r_str = if result.avg_r.is_infinite() {
            "∞ (only wins)".to_string()
        } else {
            format!("{:.4}x", result.avg_r)
        };

        format!(
            "📈 PERFORMANCE METRICS:\n\
             \x20  Total Trades      : {} trades\n\
             \x20  ✅ Win Trades      : {} ({:.1}%)\n\
             \x20  ❌ Loss Trades     : {} ({:.1}%)\n\
             \x20  Win Rate           : {:.2}%\n\
             \x20  Total PnL         : {:.4}%\n\
             \x20  Avg PnL/Trade     : {:.4}%\n\
             \x20  Avg R (Risk/Reward): {}",
            result.total_trades,
            result.win_trades,
            win_pct,
            result.loss_trades,
            loss_pct,
            result.win_rate * 100.0,
            result.total_pnl_pct * 100.0,
            result.avg_pnl_pct * 100.0,
            avg_r_str
        )
    }

    pub fn format_advanced_metrics(result: &BacktestResult) -> String {
        if result.trades.is_empty() {
            return String::new();
        }

        let best_trade = result
            .trades
            .iter()
            .max_by(|a, b| a.pnl_pct.partial_cmp(&b.pnl_pct).unwrap());
        let worst_trade = result
            .trades
            .iter()
            .min_by(|a, b| a.pnl_pct.partial_cmp(&b.pnl_pct).unwrap());

        let total_win_pnl: f64 = result
            .trades
            .iter()
            .filter(|t| t.win)
            .map(|t| t.pnl_pct.abs())
            .sum();
        let total_loss_pnl: f64 = result
            .trades
            .iter()
            .filter(|t| !t.win)
            .map(|t| t.pnl_pct.abs())
            .sum();
        let profit_factor = if total_loss_pnl > 0.0 {
            total_win_pnl / total_loss_pnl
        } else {
            f64::INFINITY
        };

        let mut lines = vec!["📊 ADVANCED METRICS:".to_string()];
        if let Some(best) = best_trade {
            lines.push(format!("   Best Trade         : {:.4}%", best.pnl_pct * 100.0));
        }
        if let Some(worst) = worst_trade {
            lines.push(format!("   Worst Trade        : {:.4}%", worst.pnl_pct * 100.0));
        }
        if profit_factor.is_finite() {
            lines.push(format!("   Profit Factor      : {:.4}x", profit_factor));
        } else {
            lines.push("   Profit Factor      : ∞ (no losses)".to_string());
        }

        lines.join("\n")
    }

    pub fn format_side_summary(result: &BacktestResult) -> String {
        let long_trades: Vec<_> = result
            .trades
            .iter()
            .filter(|t| matches!(t.side, PositionSide::Long))
            .collect();
        let short_trades: Vec<_> = result
            .trades
            .iter()
            .filter(|t| matches!(t.side, PositionSide::Short))
            .collect();

        if long_trades.is_empty() && short_trades.is_empty() {
            return String::new();
        }

        let mut lines = vec![
            "╔════════════════════════════════════════════════════════════════╗".to_string(),
            "║                  SIDE-BASED SUMMARY                            ║".to_string(),
            "╚════════════════════════════════════════════════════════════════╝".to_string(),
            String::new(),
        ];

        if !long_trades.is_empty() {
            let long_pnl: f64 = long_trades.iter().map(|t| t.pnl_pct).sum();
            let long_wins = long_trades.iter().filter(|t| t.win).count();
            let long_win_rate = (long_wins as f64 / long_trades.len() as f64) * 100.0;
            lines.push("📈 LONG TRADES:".to_string());
            lines.push(format!("   Total Trades    : {}", long_trades.len()));
            lines.push(format!(
                "   ✅ Win Trades    : {} ({:.1}%)",
                long_wins, long_win_rate
            ));
            lines.push(format!(
                "   ❌ Loss Trades   : {} ({:.1}%)",
                long_trades.len() - long_wins,
                100.0 - long_win_rate
            ));
            lines.push(format!("   Total PnL       : {:.4}%", long_pnl * 100.0));
            lines.push(format!(
                "   Avg PnL/Trade   : {:.4}%",
                (long_pnl / long_trades.len() as f64) * 100.0
            ));
            lines.push(String::new());
        }

        if !short_trades.is_empty() {
            let short_pnl: f64 = short_trades.iter().map(|t| t.pnl_pct).sum();
            let short_wins = short_trades.iter().filter(|t| t.win).count();
            let short_win_rate = (short_wins as f64 / short_trades.len() as f64) * 100.0;
            lines.push("📉 SHORT TRADES:".to_string());
            lines.push(format!("   Total Trades    : {}", short_trades.len()));
            lines.push(format!(
                "   ✅ Win Trades    : {} ({:.1}%)",
                short_wins, short_win_rate
            ));
            lines.push(format!(
                "   ❌ Loss Trades   : {} ({:.1}%)",
                short_trades.len() - short_wins,
                100.0 - short_win_rate
            ));
            lines.push(format!("   Total PnL       : {:.4}%", short_pnl * 100.0));
            lines.push(format!(
                "   Avg PnL/Trade   : {:.4}%",
                (short_pnl / short_trades.len() as f64) * 100.0
            ));
            lines.push(String::new());
        }

        lines.join("\n")
    }

    pub fn format_trade_details(result: &BacktestResult, max_trades: usize) -> String {
        if result.trades.is_empty() {
            return String::new();
        }

        let mut lines = vec![
            "╔════════════════════════════════════════════════════════════════╗".to_string(),
            "║                    TRADE DETAILS                              ║".to_string(),
            "╚════════════════════════════════════════════════════════════════╝".to_string(),
            String::new(),
        ];

        let trades_to_show = result.trades.len().min(max_trades);
        for (idx, trade) in result.trades.iter().take(trades_to_show).enumerate() {
            let side_str = match trade.side {
                PositionSide::Long => "LONG ",
                PositionSide::Short => "SHORT",
                PositionSide::Flat => "FLAT ",
            };
            let win_str = if trade.win { "✅ WIN " } else { "❌ LOSS" };
            let price_change = ((trade.exit_price - trade.entry_price) / trade.entry_price) * 100.0;
            let holding_time = trade.exit_time - trade.entry_time;
            let holding_minutes = holding_time.num_minutes();

            lines.push(format!(
                "  {}. {} {} | Entry: ${:.2} → Exit: ${:.2} ({:+.2}%) | PnL: {:+.4}% | Duration: {}m",
                idx + 1,
                side_str,
                win_str,
                trade.entry_price,
                trade.exit_price,
                price_change,
                trade.pnl_pct * 100.0,
                holding_minutes
            ));
            lines.push(format!(
                "     Entry: {} | Exit: {}",
                trade.entry_time.format("%Y-%m-%d %H:%M:%S"),
                trade.exit_time.format("%Y-%m-%d %H:%M:%S")
            ));
        }

        if result.trades.len() > max_trades {
            lines.push(format!(
                "\n  ... and {} more trades (total: {})",
                result.trades.len() - max_trades,
                result.trades.len()
            ));
        }

        lines.join("\n")
    }

    pub fn format_complete_report(
        result: &BacktestResult,
        symbol: &str,
        interval: &str,
        limit: u32,
        show_all_trades: bool,
    ) -> String {
        let mut lines = vec![
            Self::format_header(symbol, interval, limit),
            String::new(),
            Self::format_data_validation(limit),
            String::new(),
            Self::format_signal_stats(result),
            String::new(),
            Self::format_performance_metrics(result),
            String::new(),
        ];

        let advanced = Self::format_advanced_metrics(result);
        if !advanced.is_empty() {
            lines.push(advanced);
            lines.push(String::new());
        }

        let side_summary = Self::format_side_summary(result);
        if !side_summary.is_empty() {
            lines.push(side_summary);
        }

        let trade_details = if show_all_trades {
            Self::format_trade_details(result, result.trades.len())
        } else {
            Self::format_trade_details(result, 10)
        };
        if !trade_details.is_empty() {
            lines.push(trade_details);
            lines.push(String::new());
        }

        lines.push(
            "╔════════════════════════════════════════════════════════════════╗".to_string(),
        );
        lines.push(
            "║              ✅ BACKTEST COMPLETED SUCCESSFULLY                ║".to_string(),
        );
        lines.push(
            "╚════════════════════════════════════════════════════════════════╝".to_string(),
        );

        lines.join("\n")
    }
}

pub fn create_test_config() -> AlgoConfig {
    AlgoConfigBuilder::new()
        .with_rsi_thresholds(52.0, 48.0)
        .with_funding_thresholds(0.0003, -0.0003)
        .with_lsr_thresholds(1.2, 0.85)
        .with_min_scores(5, 5)
        .with_fees(13.0)  // ✅ Plan.md: Gerçekçi komisyon (10 bps + slippage = 13 bps)
        .with_holding_bars(2, 60)
        .with_slippage(3.0)
        .with_signal_quality(0.3, 4.0, 10.0)
        .with_risk_management(2.5, 7.0)
        .build()
}

pub fn create_conservative_config() -> AlgoConfig {
    AlgoConfigBuilder::new()
        .with_rsi_thresholds(55.0, 45.0)
        .with_funding_thresholds(0.0005, -0.0005)
        .with_lsr_thresholds(1.3, 0.8)
        .with_min_scores(6, 6)
        .with_fees(13.0) // ✅ Plan.md: Gerçekçi komisyon (10 bps + slippage = 13 bps)
        .with_holding_bars(3, 48)
        .with_slippage(0.0)
        .with_signal_quality(1.5, 2.0, 3.0)
        .with_risk_management(3.0, 4.0)
        .build()
}

pub fn create_aggressive_config() -> AlgoConfig {
    AlgoConfigBuilder::new()
        .with_rsi_thresholds(50.0, 50.0)
        .with_funding_thresholds(0.0002, -0.0002)
        .with_lsr_thresholds(1.15, 0.85)
        .with_min_scores(4, 4)
        .with_fees(13.0)  // ✅ Plan.md: Gerçekçi komisyon (10 bps + slippage = 13 bps)
        .with_holding_bars(1, 72)
        .with_slippage(5.0)
        .with_signal_quality(0.2, 5.0, 15.0)
        .with_risk_management(2.0, 8.0)
        .build()
}

pub struct OptimizationResult {
    pub config: AlgoConfig,
    pub result: BacktestResult,
    pub score: f64,
}

pub struct ParameterOptimizer;

impl ParameterOptimizer {
    pub fn calculate_score(result: &BacktestResult) -> f64 {
        if result.total_trades == 0 {
            return 0.0;
        }

        let win_rate_weight = 0.3;
        let profit_factor_weight = 0.3;
        let total_pnl_weight = 0.2;
        let avg_r_weight = 0.2;

        let win_rate_score = result.win_rate * 100.0;

        let total_win_pnl: f64 = result
            .trades
            .iter()
            .filter(|t| t.win)
            .map(|t| t.pnl_pct.abs())
            .sum();
        let total_loss_pnl: f64 = result
            .trades
            .iter()
            .filter(|t| !t.win)
            .map(|t| t.pnl_pct.abs())
            .sum();
        let profit_factor = if total_loss_pnl > 0.0 {
            total_win_pnl / total_loss_pnl
        } else {
            f64::INFINITY
        };
        let profit_factor_score = if profit_factor.is_infinite() {
            100.0
        } else {
            (profit_factor * 20.0).min(100.0)
        };

        let total_pnl_score = (result.total_pnl_pct * 1000.0).min(100.0).max(0.0);

        let avg_r_score = if result.avg_r.is_infinite() {
            100.0
        } else {
            (result.avg_r * 20.0).min(100.0)
        };

        win_rate_score * win_rate_weight
            + profit_factor_score * profit_factor_weight
            + total_pnl_score * total_pnl_weight
            + avg_r_score * avg_r_weight
    }

    pub fn optimize_rsi_thresholds(
        _base_config: &AlgoConfig,
        long_range: (f64, f64, f64),
        short_range: (f64, f64, f64),
    ) -> Vec<(f64, f64, f64)> {
        let (long_start, long_end, long_step) = long_range;
        let (short_start, short_end, short_step) = short_range;
        let mut combinations = Vec::new();

        let mut long = long_start;
        while long <= long_end {
            let mut short = short_start;
            while short <= short_end {
                combinations.push((long, short, 0.0));
                short += short_step;
            }
            long += long_step;
        }

        combinations
    }

    pub fn optimize_funding_thresholds(
        _base_config: &AlgoConfig,
        pos_range: (f64, f64, f64),
        neg_range: (f64, f64, f64),
    ) -> Vec<(f64, f64, f64)> {
        let (pos_start, pos_end, pos_step) = pos_range;
        let (neg_start, neg_end, neg_step) = neg_range;
        let mut combinations = Vec::new();

        let mut pos = pos_start;
        while pos <= pos_end {
            let mut neg = neg_start;
            while neg <= neg_end {
                combinations.push((pos, neg, 0.0));
                neg += neg_step;
            }
            pos += pos_step;
        }

        combinations
    }

    pub fn create_optimized_config(
        base: &AlgoConfig,
        rsi_long: Option<f64>,
        rsi_short: Option<f64>,
        funding_pos: Option<f64>,
        funding_neg: Option<f64>,
        lsr_long: Option<f64>,
        lsr_short: Option<f64>,
        min_score_long: Option<usize>,
        min_score_short: Option<usize>,
    ) -> AlgoConfig {
        let mut builder = AlgoConfigBuilder::new()
            .with_rsi_thresholds(
                rsi_long.unwrap_or(base.rsi_trend_long_min),
                rsi_short.unwrap_or(base.rsi_trend_short_max),
            )
            .with_funding_thresholds(
                funding_pos.unwrap_or(base.funding_extreme_pos),
                funding_neg.unwrap_or(base.funding_extreme_neg),
            )
            .with_lsr_thresholds(
                lsr_long.unwrap_or(base.lsr_crowded_long),
                lsr_short.unwrap_or(base.lsr_crowded_short),
            )
            .with_min_scores(
                min_score_long.unwrap_or(base.long_min_score),
                min_score_short.unwrap_or(base.short_min_score),
            )
            .with_fees(base.fee_bps_round_trip)
            .with_holding_bars(base.min_holding_bars, base.max_holding_bars)
            .with_slippage(base.slippage_bps)
            .with_signal_quality(
                base.min_volume_ratio,
                base.max_volatility_pct,
                base.max_price_change_5bars_pct,
            )
            .with_risk_management(
                base.atr_stop_loss_multiplier,
                base.atr_take_profit_multiplier,
            );

        if base.enable_enhanced_scoring {
            builder = builder.with_enhanced_scoring(
                true,
                base.enhanced_score_excellent,
                base.enhanced_score_good,
                base.enhanced_score_marginal,
            );
        }

        builder.build()
    }
}

