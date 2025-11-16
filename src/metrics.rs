// METRICS: Strategy performance tracking
// Tracks win rate, Sharpe ratio, drawdown, and other key metrics

use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::info;

/// Trade result for metrics calculation
#[derive(Clone, Debug)]
pub struct TradeResult {
    pub symbol: String,
    pub side: String,
    pub entry_price: Decimal,
    pub exit_price: Decimal,
    pub pnl: Decimal,
    pub pnl_pct: f64,
    pub timestamp: std::time::Instant,
}

/// Strategy performance metrics
#[derive(Clone, Debug)]
pub struct StrategyMetrics {
    pub total_signals: u64,
    pub total_trades: u64,
    pub winning_trades: u64,
    pub losing_trades: u64,
    pub total_pnl: Decimal,
    pub total_pnl_pct: f64,
    pub win_rate: f64,
    pub avg_win: Decimal,
    pub avg_loss: Decimal,
    pub profit_factor: f64,
    pub max_drawdown: Decimal,
    pub max_drawdown_pct: f64,
    pub sharpe_ratio: f64,
    pub trades: VecDeque<TradeResult>,
}

impl StrategyMetrics {
    pub fn new() -> Self {
        Self {
            total_signals: 0,
            total_trades: 0,
            winning_trades: 0,
            losing_trades: 0,
            total_pnl: Decimal::ZERO,
            total_pnl_pct: 0.0,
            win_rate: 0.0,
            avg_win: Decimal::ZERO,
            avg_loss: Decimal::ZERO,
            profit_factor: 0.0,
            max_drawdown: Decimal::ZERO,
            max_drawdown_pct: 0.0,
            sharpe_ratio: 0.0,
            trades: VecDeque::new(),
        }
    }

    /// Record a signal (before trade execution)
    pub fn record_signal(&mut self) {
        self.total_signals += 1;
    }

    /// Record a completed trade
    pub fn record_trade(&mut self, trade: TradeResult) {
        self.total_trades += 1;
        
        if trade.pnl > Decimal::ZERO {
            self.winning_trades += 1;
        } else {
            self.losing_trades += 1;
        }
        
        self.total_pnl += trade.pnl;
        
        // Keep last 100 trades for calculations
        self.trades.push_back(trade);
        const MAX_TRADES: usize = 100;
        while self.trades.len() > MAX_TRADES {
            self.trades.pop_front();
        }
        
        // Recalculate metrics
        self.recalculate();
    }

    /// Recalculate all metrics
    fn recalculate(&mut self) {
        if self.total_trades == 0 {
            return;
        }

        // Win rate
        self.win_rate = self.winning_trades as f64 / self.total_trades as f64;

        // Average win/loss
        let wins: Vec<&TradeResult> = self.trades.iter().filter(|t| t.pnl > Decimal::ZERO).collect();
        let losses: Vec<&TradeResult> = self.trades.iter().filter(|t| t.pnl <= Decimal::ZERO).collect();

        if !wins.is_empty() {
            let total_wins: Decimal = wins.iter().map(|t| t.pnl).sum();
            self.avg_win = total_wins / Decimal::from(wins.len());
        }

        if !losses.is_empty() {
            let total_losses: Decimal = losses.iter().map(|t| t.pnl.abs()).sum();
            self.avg_loss = total_losses / Decimal::from(losses.len());
        }

        // Profit factor
        if !self.avg_loss.is_zero() {
            self.profit_factor = self.avg_win.to_f64().unwrap_or(0.0) / self.avg_loss.to_f64().unwrap_or(1.0);
        }

        // Max drawdown
        let mut peak = Decimal::ZERO;
        let mut max_dd = Decimal::ZERO;
        let mut cumulative = Decimal::ZERO;

        for trade in &self.trades {
            cumulative += trade.pnl;
            if cumulative > peak {
                peak = cumulative;
            }
            let drawdown = peak - cumulative;
            if drawdown > max_dd {
                max_dd = drawdown;
            }
        }

        self.max_drawdown = max_dd;
        if !peak.is_zero() {
            self.max_drawdown_pct = (max_dd / peak).to_f64().unwrap_or(0.0) * 100.0;
        }

        // Sharpe ratio (simplified: mean return / std dev)
        if self.trades.len() >= 2 {
            let returns: Vec<f64> = self.trades.iter().map(|t| t.pnl_pct).collect();
            let mean: f64 = returns.iter().sum::<f64>() / returns.len() as f64;
            let variance: f64 = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / returns.len() as f64;
            let std_dev = variance.sqrt();
            
            if std_dev > 0.0 {
                // Annualized Sharpe (assuming daily returns, 252 trading days)
                self.sharpe_ratio = (mean / std_dev) * (252.0_f64).sqrt();
            }
        }
    }

    /// Get formatted metrics summary
    pub fn summary(&self) -> String {
        format!(
            "Metrics: signals={}, trades={}, win_rate={:.2}%, pnl=${:.2}, sharpe={:.2}, max_dd={:.2}%",
            self.total_signals,
            self.total_trades,
            self.win_rate * 100.0,
            self.total_pnl.to_f64().unwrap_or(0.0),
            self.sharpe_ratio,
            self.max_drawdown_pct
        )
    }
}

impl Default for StrategyMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Metrics tracker (thread-safe)
pub struct MetricsTracker {
    metrics: Arc<RwLock<StrategyMetrics>>,
}

impl MetricsTracker {
    pub fn new() -> Self {
        Self {
            metrics: Arc::new(RwLock::new(StrategyMetrics::new())),
        }
    }

    pub async fn record_signal(&self) {
        let mut metrics = self.metrics.write().await;
        metrics.record_signal();
    }

    pub async fn record_trade(&self, trade: TradeResult) {
        let mut metrics = self.metrics.write().await;
        metrics.record_trade(trade);
    }

    pub async fn get_metrics(&self) -> StrategyMetrics {
        self.metrics.read().await.clone()
    }

    /// Log metrics periodically
    pub async fn log_metrics(&self) {
        let metrics = self.get_metrics().await;
        info!("{}", metrics.summary());
    }
}

impl Default for MetricsTracker {
    fn default() -> Self {
        Self::new()
    }
}

