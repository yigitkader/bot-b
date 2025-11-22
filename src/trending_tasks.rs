use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use crate::{
    tasks::TaskInfo,
    types::{EventBus, TrendParams},
    metrics_cache::MetricsCache,
    trending,
};

type TaskHandle = Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>;

pub async fn spawn_trending_task(
    symbol: String,
    bus: &EventBus,
    trend_params: &TrendParams,
    ws_base_url: &str,
    metrics_cache: Arc<MetricsCache>,
    tasks_map: Arc<RwLock<HashMap<String, TaskHandle>>>,
    task_infos: Arc<Mutex<Vec<TaskInfo>>>,
) {
    let symbol_clone = symbol.clone();
    let ch = bus.trending_channels();
    let trend_params_clone = trend_params.clone();
    let ws_base_url_clone = ws_base_url.to_string();
    let handle = tokio::spawn(async move {
        trending::run_trending(
            ch,
            symbol_clone,
            trend_params_clone,
            ws_base_url_clone,
            Some(metrics_cache),
        )
        .await;
    });
    let handle_arc = Arc::new(Mutex::new(Some(handle)));
    {
        let mut tasks = tasks_map.write().await;
        tasks.insert(symbol.clone(), handle_arc.clone());
    }
    {
        let mut infos = task_infos.lock().await;
        infos.push(TaskInfo {
            name: format!("trending_{}", symbol),
            handle: handle_arc,
        });
    }
    log::info!("TRENDING: Started trending task for symbol {}", symbol);
}

pub fn create_algo_config_from_trend_params(trend_params: &TrendParams) -> crate::types::AlgoConfig {
    crate::types::AlgoConfig {
        rsi_trend_long_min: trend_params.rsi_long_min,
        rsi_trend_short_max: trend_params.rsi_short_max,
        funding_extreme_pos: trend_params.funding_max_for_long.max(0.0001),
        funding_extreme_neg: trend_params.funding_min_for_short.min(-0.0001),
        lsr_crowded_long: trend_params.obi_long_min.max(1.3),
        lsr_crowded_short: trend_params.obi_short_max.min(0.8),
        long_min_score: trend_params.long_min_score,
        short_min_score: trend_params.short_min_score,
        fee_bps_round_trip: trend_params.fee_bps_round_trip,
        max_holding_bars: trend_params.max_holding_bars,
        slippage_bps: trend_params.slippage_bps,
        min_holding_bars: trend_params.min_holding_bars,
        min_volume_ratio: trend_params.min_volume_ratio,
        max_volatility_pct: trend_params.max_volatility_pct,
        max_price_change_5bars_pct: trend_params.max_price_change_5bars_pct,
        enable_signal_quality_filter: trend_params.enable_signal_quality_filter,
        atr_stop_loss_multiplier: trend_params.atr_sl_multiplier,
        atr_take_profit_multiplier: trend_params.atr_tp_multiplier,
        hft_mode: trend_params.hft_mode,
        base_min_score: trend_params.base_min_score,
        trend_threshold_hft: trend_params.trend_threshold_hft,
        trend_threshold_normal: trend_params.trend_threshold_normal,
        weak_trend_score_multiplier: trend_params.weak_trend_score_multiplier,
        regime_multiplier_trending: trend_params.regime_multiplier_trending,
        regime_multiplier_ranging: trend_params.regime_multiplier_ranging,
        enable_enhanced_scoring: trend_params.enable_enhanced_scoring,
        enhanced_score_excellent: trend_params.enhanced_score_excellent,
        enhanced_score_good: trend_params.enhanced_score_good,
        enhanced_score_marginal: trend_params.enhanced_score_marginal,
        enable_order_flow: trend_params.enable_order_flow,
    }
}

