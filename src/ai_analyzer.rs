// AI_ANALYZER: Intelligent log analysis and error detection module
// Analyzes logs in real-time to detect patterns, anomalies, and potential issues
// Uses pattern matching and heuristics to identify problems before they cause failures

use crate::event_bus::EventBus;
use crate::types::*;
use anyhow::Result;
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

// ============================================================================
// Error Patterns and Anomaly Detection
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AnomalyType {
    /// High order rejection rate (potential exchange issues or config problems)
    HighOrderRejectionRate {
        symbol: String,
        rejection_rate: f64,
        threshold: f64,
    },
    /// Unusual PnL pattern (sudden losses or gains)
    UnusualPnLPattern {
        symbol: String,
        pnl_change: f64,
        recent_trades: u64,
    },
    /// Low win rate (strategy may not be working)
    LowWinRate {
        symbol: String,
        win_rate: f64,
        total_trades: u64,
    },
    /// High spread detected (liquidity issues)
    HighSpread {
        symbol: String,
        spread_bps: f64,
        threshold: f64,
    },
    /// Frequent order cancellations (market conditions or timing issues)
    FrequentCancellations {
        symbol: String,
        cancel_rate: f64,
        recent_orders: u64,
    },
    /// Balance inconsistency (potential accounting error)
    BalanceInconsistency {
        expected: f64,
        actual: f64,
        difference: f64,
    },
    /// WebSocket disconnections (connectivity issues)
    WebSocketDisconnections {
        count: u64,
        duration: Duration,
    },
    /// Circuit breaker triggered (too many rejections)
    CircuitBreakerTriggered {
        symbol: String,
        rejection_count: u64,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct AnomalyReport {
    pub timestamp: u64,
    pub anomaly_type: AnomalyType,
    pub severity: Severity,
    pub message: String,
    pub recommendation: String,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub enum Severity {
    Low,      // Informational - monitor
    Medium,   // Warning - investigate
    High,     // Critical - immediate action needed
}

// ============================================================================
// AI Analyzer - Pattern Detection and Analysis
// ============================================================================

pub struct AiAnalyzer {
    event_bus: Arc<EventBus>,
    shutdown_flag: Arc<AtomicBool>,
    
    // Trade statistics per symbol
    trade_stats: Arc<Mutex<HashMap<String, TradeStats>>>,
    
    // Order statistics per symbol
    order_stats: Arc<Mutex<HashMap<String, OrderStats>>>,
    
    // Anomaly history (for pattern detection)
    anomaly_history: Arc<Mutex<VecDeque<AnomalyReport>>>,
    
    // Performance metrics
    total_signals: Arc<AtomicU64>,
    total_trades: Arc<AtomicU64>,
    total_wins: Arc<AtomicU64>,
    total_losses: Arc<AtomicU64>,
    
    // Analysis report file
    report_file_path: PathBuf,
    
    // Detailed operation tracking
    operation_log: Arc<Mutex<VecDeque<OperationLog>>>,
    
    // Error tracking
    error_log: Arc<Mutex<VecDeque<ErrorLog>>>,
    
    // Log file tracking (for incremental reading)
    console_log_path: PathBuf,
    last_console_log_position: Arc<Mutex<u64>>,
    trading_events_path: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
struct OperationLog {
    timestamp: u64,
    operation_type: String, // "signal", "order", "trade", "position", "balance"
    symbol: Option<String>,
    details: String,
    success: bool,
    duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
struct ErrorLog {
    timestamp: u64,
    error_type: String,
    module: String,
    symbol: Option<String>,
    message: String,
    context: HashMap<String, String>,
}

#[derive(Debug, Clone)]
struct TradeStats {
    symbol: String,
    total_trades: u64,
    winning_trades: u64,
    losing_trades: u64,
    total_pnl: f64,
    recent_pnl: VecDeque<f64>, // Last 10 trades PnL
    last_trade_time: Option<Instant>,
}

#[derive(Debug, Clone)]
struct OrderStats {
    symbol: String,
    total_orders: u64,
    filled_orders: u64,
    canceled_orders: u64,
    rejected_orders: u64,
    recent_spreads: VecDeque<f64>, // Last 20 spreads
    last_order_time: Option<Instant>,
}

impl AiAnalyzer {
    pub fn new(event_bus: Arc<EventBus>, shutdown_flag: Arc<AtomicBool>) -> Self {
        // Create logs directory if it doesn't exist
        std::fs::create_dir_all("logs").ok();
        
        let report_file_path = PathBuf::from("logs/ai_analysis_report.json");
        let console_log_path = PathBuf::from("logs/console.log");
        let trading_events_path = PathBuf::from("logs/trading_events.json");
        
        Self {
            event_bus,
            shutdown_flag,
            trade_stats: Arc::new(Mutex::new(HashMap::new())),
            order_stats: Arc::new(Mutex::new(HashMap::new())),
            anomaly_history: Arc::new(Mutex::new(VecDeque::new())),
            total_signals: Arc::new(AtomicU64::new(0)),
            total_trades: Arc::new(AtomicU64::new(0)),
            total_wins: Arc::new(AtomicU64::new(0)),
            total_losses: Arc::new(AtomicU64::new(0)),
            report_file_path,
            operation_log: Arc::new(Mutex::new(VecDeque::new())),
            error_log: Arc::new(Mutex::new(VecDeque::new())),
            console_log_path,
            last_console_log_position: Arc::new(Mutex::new(0)),
            trading_events_path,
        }
    }

    /// Start the AI analyzer service
    pub async fn start(&self) -> Result<()> {
        info!("AI_ANALYZER: Starting intelligent log analysis service");
        info!("AI_ANALYZER: Analysis reports will be written to: {}", self.report_file_path.display());
        
        // Subscribe to all relevant events
        self.analyze_trade_signals().await?;
        self.analyze_order_updates().await?;
        self.analyze_position_updates().await?;
        self.analyze_balance_updates().await?;
        
        // Analyze log files periodically
        self.analyze_log_files().await?;
        
        // Periodic analysis and report generation
        self.periodic_analysis().await?;
        
        Ok(())
    }
    
    fn timestamp_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| Duration::from_secs(0))
            .as_millis() as u64
    }
    
    fn timestamp_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| Duration::from_secs(0))
            .as_secs()
    }

    /// Analyze trade signals for patterns
    async fn analyze_trade_signals(&self) -> Result<()> {
        let event_bus = self.event_bus.clone();
        let shutdown_flag = self.shutdown_flag.clone();
        let total_signals = self.total_signals.clone();
        let operation_log = self.operation_log.clone();
        
        tokio::spawn(async move {
            let mut trade_signal_rx = event_bus.subscribe_trade_signal();
            
            loop {
                match trade_signal_rx.recv().await {
                    Ok(signal) => {
                        if shutdown_flag.load(AtomicOrdering::Relaxed) {
                            break;
                        }
                        
                        total_signals.fetch_add(1, AtomicOrdering::Relaxed);
                        
                        // Log operation
                        let mut op_log = operation_log.lock().await;
                        op_log.push_back(OperationLog {
                            timestamp: Self::timestamp_ms(),
                            operation_type: "signal".to_string(),
                            symbol: Some(signal.symbol.clone()),
                            details: format!("{:?} @ {}", signal.side, signal.entry_price.0),
                            success: true,
                            duration_ms: None,
                        });
                        if op_log.len() > 1000 {
                            op_log.pop_front();
                        }
                        
                        // Analyze signal patterns
                        // Track signal frequency, symbol distribution, etc.
                    }
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        warn!("AI_ANALYZER: TradeSignal receiver lagged, {} events missed", missed);
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        warn!("AI_ANALYZER: TradeSignal channel closed");
                        break;
                    }
                }
            }
        });
        
        Ok(())
    }

    /// Analyze order updates for anomalies
    async fn analyze_order_updates(&self) -> Result<()> {
        let event_bus = self.event_bus.clone();
        let shutdown_flag = self.shutdown_flag.clone();
        let order_stats = self.order_stats.clone();
        let anomaly_history = self.anomaly_history.clone();
        let operation_log = self.operation_log.clone();
        let error_log = self.error_log.clone();
        
        tokio::spawn(async move {
            let mut order_update_rx = event_bus.subscribe_order_update();
            
            loop {
                match order_update_rx.recv().await {
                    Ok(update) => {
                        if shutdown_flag.load(AtomicOrdering::Relaxed) {
                            break;
                        }
                        
                        let mut stats = order_stats.lock().await;
                        let stat = stats.entry(update.symbol.clone())
                            .or_insert_with(|| OrderStats {
                                symbol: update.symbol.clone(),
                                total_orders: 0,
                                filled_orders: 0,
                                canceled_orders: 0,
                                rejected_orders: 0,
                                recent_spreads: VecDeque::new(),
                                last_order_time: None,
                            });
                        
                        stat.total_orders += 1;
                        stat.last_order_time = Some(Instant::now());
                        
                        // Log operation
                        let mut op_log = operation_log.lock().await;
                        let success = matches!(update.status, crate::event_bus::OrderStatus::Filled);
                        op_log.push_back(OperationLog {
                            timestamp: Self::timestamp_ms(),
                            operation_type: "order".to_string(),
                            symbol: Some(update.symbol.clone()),
                            details: format!("{} - {:?}", update.order_id, update.status),
                            success,
                            duration_ms: None,
                        });
                        if op_log.len() > 1000 {
                            op_log.pop_front();
                        }
                        
                        match update.status {
                            crate::event_bus::OrderStatus::Filled => {
                                stat.filled_orders += 1;
                            }
                            crate::event_bus::OrderStatus::Canceled => {
                                stat.canceled_orders += 1;
                            }
                            crate::event_bus::OrderStatus::Rejected => {
                                stat.rejected_orders += 1;
                                
                                // Log error
                                let mut err_log = error_log.lock().await;
                                let mut context = HashMap::new();
                                context.insert("order_id".to_string(), update.order_id.clone());
                                context.insert("symbol".to_string(), update.symbol.clone());
                                context.insert("side".to_string(), format!("{:?}", update.side));
                                err_log.push_back(ErrorLog {
                                    timestamp: Self::timestamp_ms(),
                                    error_type: "order_rejected".to_string(),
                                    module: "ORDERING".to_string(),
                                    symbol: Some(update.symbol.clone()),
                                    message: format!("Order {} rejected", update.order_id),
                                    context,
                                });
                                if err_log.len() > 500 {
                                    err_log.pop_front();
                                }
                            }
                            _ => {}
                        }
                        
                        // Detect anomalies
                        let mut history = anomaly_history.lock().await;
                        Self::detect_order_anomalies(&stat, &mut *history);
                    }
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        warn!("AI_ANALYZER: OrderUpdate receiver lagged, {} events missed", missed);
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        warn!("AI_ANALYZER: OrderUpdate channel closed");
                        break;
                    }
                }
            }
        });
        
        Ok(())
    }

    /// Analyze position updates for PnL patterns
    async fn analyze_position_updates(&self) -> Result<()> {
        let event_bus = self.event_bus.clone();
        let shutdown_flag = self.shutdown_flag.clone();
        let trade_stats = self.trade_stats.clone();
        let anomaly_history = self.anomaly_history.clone();
        let total_trades = self.total_trades.clone();
        let total_wins = self.total_wins.clone();
        let total_losses = self.total_losses.clone();
        
        tokio::spawn(async move {
            let mut position_update_rx = event_bus.subscribe_position_update();
            
            loop {
                match position_update_rx.recv().await {
                    Ok(update) => {
                        if shutdown_flag.load(AtomicOrdering::Relaxed) {
                            break;
                        }
                        
                        // Track PnL changes
                        let pnl = update.unrealized_pnl
                            .map(|pnl| pnl.to_f64().unwrap_or(0.0))
                            .unwrap_or(0.0);
                        
                        let mut stats = trade_stats.lock().await;
                        let stat = stats.entry(update.symbol.clone())
                            .or_insert_with(|| TradeStats {
                                symbol: update.symbol.clone(),
                                total_trades: 0,
                                winning_trades: 0,
                                losing_trades: 0,
                                total_pnl: 0.0,
                                recent_pnl: VecDeque::new(),
                                last_trade_time: None,
                            });
                        
                        // Detect position closed (PnL realized)
                        if !update.is_open && stat.last_trade_time.is_some() {
                            stat.total_trades += 1;
                            total_trades.fetch_add(1, AtomicOrdering::Relaxed);
                            
                            if pnl > 0.0 {
                                stat.winning_trades += 1;
                                total_wins.fetch_add(1, AtomicOrdering::Relaxed);
                            } else if pnl < 0.0 {
                                stat.losing_trades += 1;
                                total_losses.fetch_add(1, AtomicOrdering::Relaxed);
                            }
                            
                            stat.total_pnl += pnl;
                            stat.recent_pnl.push_back(pnl);
                            if stat.recent_pnl.len() > 10 {
                                stat.recent_pnl.pop_front();
                            }
                            
                            // Detect anomalies
                            let mut history = anomaly_history.lock().await;
                            Self::detect_trade_anomalies(&stat, &mut *history);
                        }
                        
                        stat.last_trade_time = Some(Instant::now());
                    }
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        warn!("AI_ANALYZER: PositionUpdate receiver lagged, {} events missed", missed);
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        warn!("AI_ANALYZER: PositionUpdate channel closed");
                        break;
                    }
                }
            }
        });
        
        Ok(())
    }

    /// Analyze balance updates for inconsistencies
    async fn analyze_balance_updates(&self) -> Result<()> {
        let event_bus = self.event_bus.clone();
        let shutdown_flag = self.shutdown_flag.clone();
        let anomaly_history = self.anomaly_history.clone();
        
        tokio::spawn(async move {
            let mut balance_update_rx = event_bus.subscribe_balance_update();
            let mut last_balance: Option<(f64, f64)> = None; // (usdt, usdc)
            
            loop {
                match balance_update_rx.recv().await {
                    Ok(update) => {
                        if shutdown_flag.load(AtomicOrdering::Relaxed) {
                            break;
                        }
                        
                        let current_balance = (
                            update.usdt.to_f64().unwrap_or(0.0),
                            update.usdc.to_f64().unwrap_or(0.0),
                        );
                        
                        // Detect sudden balance changes (potential accounting error)
                        if let Some((prev_usdt, prev_usdc)) = last_balance {
                            let usdt_change = (current_balance.0 - prev_usdt).abs();
                            let usdc_change = (current_balance.1 - prev_usdc).abs();
                            
                            // Large unexpected balance change (threshold: 10% or $100)
                            if usdt_change > 100.0 || usdt_change > prev_usdt * 0.1 {
                                let mut history = anomaly_history.lock().await;
                                let report = AnomalyReport {
                                    timestamp: std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap()
                                        .as_secs(),
                                    anomaly_type: AnomalyType::BalanceInconsistency {
                                        expected: prev_usdt,
                                        actual: current_balance.0,
                                        difference: usdt_change,
                                    },
                                    severity: Severity::High,
                                    message: format!(
                                        "Large USDT balance change detected: ${:.2} -> ${:.2} (${:.2} change)",
                                        prev_usdt, current_balance.0, usdt_change
                                    ),
                                    recommendation: "Check recent trades and order fills for accounting errors".to_string(),
                                };
                                
                                error!("🚨 AI_ANALYZER: {}", report.message);
                                history.push_back(report);
                                if history.len() > 100 {
                                    history.pop_front();
                                }
                            }
                        }
                        
                        last_balance = Some(current_balance);
                    }
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        warn!("AI_ANALYZER: BalanceUpdate receiver lagged, {} events missed", missed);
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        warn!("AI_ANALYZER: BalanceUpdate channel closed");
                        break;
                    }
                }
            }
        });
        
        Ok(())
    }

    /// Detect order-related anomalies
    fn detect_order_anomalies(
        stat: &OrderStats,
        anomaly_history: &mut VecDeque<AnomalyReport>,
    ) {
        // High rejection rate (> 20%)
        if stat.total_orders > 10 {
            let rejection_rate = (stat.rejected_orders as f64) / (stat.total_orders as f64) * 100.0;
            if rejection_rate > 20.0 {
                let report = AnomalyReport {
                    timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs(),
                    anomaly_type: AnomalyType::HighOrderRejectionRate {
                        symbol: stat.symbol.clone(),
                        rejection_rate,
                        threshold: 20.0,
                    },
                    severity: Severity::High,
                    message: format!(
                        "High order rejection rate for {}: {:.1}% (threshold: 20%)",
                        stat.symbol, rejection_rate
                    ),
                    recommendation: "Check exchange API status, order parameters, and account permissions".to_string(),
                };
                
                error!("🚨 AI_ANALYZER: {}", report.message);
                anomaly_history.push_back(report);
                if anomaly_history.len() > 100 {
                    anomaly_history.pop_front();
                }
            }
        }
        
        // High cancellation rate (> 30%)
        if stat.total_orders > 10 {
            let cancel_rate = (stat.canceled_orders as f64) / (stat.total_orders as f64) * 100.0;
            if cancel_rate > 30.0 {
                let report = AnomalyReport {
                    timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs(),
                    anomaly_type: AnomalyType::FrequentCancellations {
                        symbol: stat.symbol.clone(),
                        cancel_rate,
                        recent_orders: stat.total_orders,
                    },
                    severity: Severity::Medium,
                    message: format!(
                        "High order cancellation rate for {}: {:.1}% (threshold: 30%)",
                        stat.symbol, cancel_rate
                    ),
                    recommendation: "Review spread validation logic and market conditions".to_string(),
                };
                
                warn!("⚠️ AI_ANALYZER: {}", report.message);
                anomaly_history.push_back(report);
                if anomaly_history.len() > 100 {
                    anomaly_history.pop_front();
                }
            }
        }
    }

    /// Detect trade-related anomalies
    fn detect_trade_anomalies(
        stat: &TradeStats,
        anomaly_history: &mut VecDeque<AnomalyReport>,
    ) {
        // Low win rate (< 40% with > 10 trades)
        if stat.total_trades > 10 {
            let win_rate = (stat.winning_trades as f64) / (stat.total_trades as f64) * 100.0;
            if win_rate < 40.0 {
                let report = AnomalyReport {
                    timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs(),
                    anomaly_type: AnomalyType::LowWinRate {
                        symbol: stat.symbol.clone(),
                        win_rate,
                        total_trades: stat.total_trades,
                    },
                    severity: Severity::Medium,
                    message: format!(
                        "Low win rate for {}: {:.1}% ({} trades, threshold: 40%)",
                        stat.symbol, win_rate, stat.total_trades
                    ),
                    recommendation: "Review strategy parameters, market conditions, and consider pausing trading for this symbol".to_string(),
                };
                
                warn!("⚠️ AI_ANALYZER: {}", report.message);
                anomaly_history.push_back(report);
                if anomaly_history.len() > 100 {
                    anomaly_history.pop_front();
                }
            }
        }
        
        // Unusual PnL pattern (sudden large loss)
        if stat.recent_pnl.len() >= 3 {
            let recent_avg = stat.recent_pnl.iter().sum::<f64>() / stat.recent_pnl.len() as f64;
            if let Some(&last_pnl) = stat.recent_pnl.back() {
                // Sudden large loss (> 2x average loss)
                if last_pnl < -50.0 && recent_avg < -20.0 {
                    let report = AnomalyReport {
                        timestamp: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_secs(),
                        anomaly_type: AnomalyType::UnusualPnLPattern {
                            symbol: stat.symbol.clone(),
                            pnl_change: last_pnl,
                            recent_trades: stat.recent_pnl.len() as u64,
                        },
                        severity: Severity::High,
                        message: format!(
                            "Unusual PnL pattern for {}: ${:.2} loss (recent avg: ${:.2})",
                            stat.symbol, last_pnl, recent_avg
                        ),
                        recommendation: "Review recent trades, check for slippage or execution issues".to_string(),
                    };
                    
                    error!("🚨 AI_ANALYZER: {}", report.message);
                    anomaly_history.push_back(report);
                    if anomaly_history.len() > 100 {
                        anomaly_history.pop_front();
                    }
                }
            }
        }
    }

    /// Analyze log files for patterns and errors
    async fn analyze_log_files(&self) -> Result<()> {
        let shutdown_flag = self.shutdown_flag.clone();
        let error_log = self.error_log.clone();
        let console_log_path = self.console_log_path.clone();
        let trading_events_path = self.trading_events_path.clone();
        let last_position = self.last_console_log_position.clone();
        
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await; // Check every minute
                
                if shutdown_flag.load(AtomicOrdering::Relaxed) {
                    break;
                }
                
                // Analyze console.log for errors and warnings (incremental reading)
                if let Ok(new_content) = Self::read_log_incremental(&console_log_path, &last_position).await {
                    if !new_content.is_empty() {
                        let mut err_log = error_log.lock().await;
                        Self::analyze_console_log(&new_content, &mut *err_log);
                    }
                }
                
                // Analyze trading_events.json for patterns
                if let Ok(content) = std::fs::read_to_string(&trading_events_path) {
                    Self::analyze_trading_events(&content, &error_log).await;
                }
            }
        });
        
        Ok(())
    }
    
    /// Read log file incrementally from last position
    async fn read_log_incremental(
        log_path: &PathBuf,
        last_position: &Arc<Mutex<u64>>,
    ) -> Result<String, std::io::Error> {
        use std::io::{Seek, SeekFrom};
        
        let mut file = std::fs::File::open(log_path)?;
        let current_size = file.metadata()?.len() as u64;
        
        let mut last_pos = last_position.lock().await;
        
        // If file was truncated or doesn't exist, reset position
        if *last_pos > current_size {
            *last_pos = 0;
        }
        
        // Seek to last position
        file.seek(SeekFrom::Start(*last_pos))?;
        
        // Read new content
        use std::io::Read;
        let mut buffer = String::new();
        file.read_to_string(&mut buffer)?;
        
        // Update position
        *last_pos = current_size;
        
        Ok(buffer)
    }
    
    /// Analyze console.log for errors, warnings, and patterns
    /// Log format: "2025-11-16T19:16:27.104027Z  WARN app::trending: TRENDING: ..."
    fn analyze_console_log(content: &str, error_log: &mut VecDeque<ErrorLog>) {
        let lines: Vec<&str> = content.lines().collect();
        let mut error_count = 0;
        let mut warn_count = 0;
        let mut recent_errors = Vec::new();
        let mut recent_warnings = Vec::new();
        
        for line in lines.iter() {
            // Parse structured log format: timestamp LEVEL module: message
            // Check for ERROR level (case-sensitive, as per tracing format)
            if line.contains(" ERROR ") || line.contains("\tERROR\t") {
                error_count += 1;
                recent_errors.push(line.to_string());
                
                // Extract module and message for better error tracking
                let (module, message) = Self::parse_log_line(line);
                let mut context = HashMap::new();
                context.insert("log_line".to_string(), line.to_string());
                
                error_log.push_back(ErrorLog {
                    timestamp: Self::timestamp_ms(),
                    error_type: "log_error".to_string(),
                    module: module.clone(),
                    symbol: Self::extract_symbol_from_log(line),
                    message: message.clone(),
                    context,
                });
            } 
            // Check for WARN level
            else if line.contains(" WARN ") || line.contains("\tWARN\t") {
                warn_count += 1;
                recent_warnings.push(line.to_string());
            }
        }
        
        // Detect high error/warning rates
        if error_count > 10 {
            let mut context = HashMap::new();
            context.insert("error_count".to_string(), error_count.to_string());
            context.insert("warn_count".to_string(), warn_count.to_string());
            context.insert("sample_errors".to_string(), 
                recent_errors.iter().take(3).cloned().collect::<Vec<_>>().join("; "));
            
            error_log.push_back(ErrorLog {
                timestamp: Self::timestamp_ms(),
                error_type: "high_error_rate".to_string(),
                module: "SYSTEM".to_string(),
                symbol: None,
                message: format!("High error rate detected: {} errors, {} warnings in recent logs", 
                    error_count, warn_count),
                context,
            });
        }
        
        // Detect high warning rate (potential issues)
        if warn_count > 50 && error_count == 0 {
            let mut context = HashMap::new();
            context.insert("warn_count".to_string(), warn_count.to_string());
            context.insert("sample_warnings".to_string(), 
                recent_warnings.iter().take(3).cloned().collect::<Vec<_>>().join("; "));
            
            error_log.push_back(ErrorLog {
                timestamp: Self::timestamp_ms(),
                error_type: "high_warning_rate".to_string(),
                module: "SYSTEM".to_string(),
                symbol: None,
                message: format!("High warning rate detected: {} warnings in recent logs", warn_count),
                context,
            });
        }
        
        if error_log.len() > 500 {
            error_log.drain(..error_log.len() - 500);
        }
    }
    
    /// Parse log line to extract module and message
    fn parse_log_line(line: &str) -> (String, String) {
        // Format: "timestamp LEVEL module::submodule: message"
        // Example: "2025-11-16T19:16:27.104027Z  WARN app::trending: TRENDING: MarketTick channel lagged"
        
        let parts: Vec<&str> = line.splitn(4, ' ').collect();
        if parts.len() >= 4 {
            // parts[0] = timestamp, parts[1] = level, parts[2] = module, parts[3] = message
            let module = parts[2].trim_end_matches(':').to_string();
            let message = parts[3..].join(" ");
            (module, message)
        } else {
            ("UNKNOWN".to_string(), line.to_string())
        }
    }
    
    /// Extract symbol from log line if present
    fn extract_symbol_from_log(line: &str) -> Option<String> {
        // Look for patterns like "symbol=BTCUSDC" or "symbol: BTCUSDC"
        if let Some(symbol_start) = line.find("symbol=") {
            let after_eq = &line[symbol_start + 7..];
            if let Some(end) = after_eq.find(|c: char| c == ' ' || c == '\t' || c == '\n') {
                return Some(after_eq[..end].to_string());
            } else {
                return Some(after_eq.to_string());
            }
        }
        None
    }
    
    /// Analyze trading events JSON for patterns
    async fn analyze_trading_events(
        content: &str,
        error_log: &Arc<Mutex<VecDeque<ErrorLog>>>,
    ) {
        use serde_json::Value;
        
        let lines: Vec<&str> = content.lines().collect();
        let mut order_fills = 0;
        let mut order_cancels = 0;
        let mut position_updates = 0;
        let mut recent_pnl_values = Vec::new();
        let mut symbol_trade_counts: HashMap<String, u64> = HashMap::new();
        
        // Parse JSON lines (one event per line)
        for line in lines.iter().rev().take(1000) { // Last 1000 events
            if line.trim().is_empty() {
                continue;
            }
            
            match serde_json::from_str::<Value>(line) {
                Ok(event) => {
                    // Check event type
                    if let Some(event_type) = event.get("event_type").and_then(|v| v.as_str()) {
                        match event_type {
                            "order_filled" => {
                                order_fills += 1;
                                if let Some(symbol) = event.get("symbol").and_then(|v| v.as_str()) {
                                    *symbol_trade_counts.entry(symbol.to_string()).or_insert(0) += 1;
                                }
                            }
                            "order_canceled" => {
                                order_cancels += 1;
                            }
                            "position_updated" => {
                                position_updates += 1;
                                if let Some(pnl) = event.get("unrealized_pnl").and_then(|v| v.as_f64()) {
                                    recent_pnl_values.push(pnl);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Err(_) => {
                    // Skip invalid JSON lines
                    continue;
                }
            }
        }
        
        // Detect patterns
        let total_events = order_fills + order_cancels + position_updates;
        if total_events > 0 {
            let cancel_rate = (order_cancels as f64) / (order_fills + order_cancels) as f64 * 100.0;
            
            // High cancellation rate
            if cancel_rate > 50.0 && order_cancels > 10 {
                let mut context = HashMap::new();
                context.insert("order_fills".to_string(), order_fills.to_string());
                context.insert("order_cancels".to_string(), order_cancels.to_string());
                context.insert("cancel_rate".to_string(), format!("{:.1}%", cancel_rate));
                
                let mut err_log = error_log.lock().await;
                err_log.push_back(ErrorLog {
                    timestamp: Self::timestamp_ms(),
                    error_type: "high_cancel_rate".to_string(),
                    module: "TRADING".to_string(),
                    symbol: None,
                    message: format!("High order cancellation rate: {:.1}% ({} cancels, {} fills)", 
                        cancel_rate, order_cancels, order_fills),
                    context,
                });
                
                if err_log.len() > 500 {
                    err_log.pop_front();
                }
            }
            
            // Large unrealized losses
            if recent_pnl_values.len() > 5 {
                let avg_pnl = recent_pnl_values.iter().sum::<f64>() / recent_pnl_values.len() as f64;
                let min_pnl = recent_pnl_values.iter().fold(f64::INFINITY, |a, &b| a.min(b));
                
                if min_pnl < -100.0 || avg_pnl < -50.0 {
                    let mut context = HashMap::new();
                    context.insert("avg_pnl".to_string(), format!("{:.2}", avg_pnl));
                    context.insert("min_pnl".to_string(), format!("{:.2}", min_pnl));
                    context.insert("position_updates".to_string(), position_updates.to_string());
                    
                    let mut err_log = error_log.lock().await;
                    err_log.push_back(ErrorLog {
                        timestamp: Self::timestamp_ms(),
                        error_type: "large_unrealized_losses".to_string(),
                        module: "TRADING".to_string(),
                        symbol: None,
                        message: format!("Large unrealized losses detected: avg ${:.2}, min ${:.2}", 
                            avg_pnl, min_pnl),
                        context,
                    });
                    
                    if err_log.len() > 500 {
                        err_log.pop_front();
                    }
                }
            }
        }
    }
    
    /// Periodic analysis task (runs every 5 minutes)
    async fn periodic_analysis(&self) -> Result<()> {
        let shutdown_flag = self.shutdown_flag.clone();
        let trade_stats = self.trade_stats.clone();
        let order_stats = self.order_stats.clone();
        let total_signals = self.total_signals.clone();
        let total_trades = self.total_trades.clone();
        let total_wins = self.total_wins.clone();
        let total_losses = self.total_losses.clone();
        let anomaly_history = self.anomaly_history.clone();
        let operation_log = self.operation_log.clone();
        let error_log = self.error_log.clone();
        let report_file_path = self.report_file_path.clone();
        
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(300)).await; // 5 minutes
                
                if shutdown_flag.load(AtomicOrdering::Relaxed) {
                    break;
                }
                
                // Generate summary report
                let signals = total_signals.load(AtomicOrdering::Relaxed);
                let trades = total_trades.load(AtomicOrdering::Relaxed);
                let wins = total_wins.load(AtomicOrdering::Relaxed);
                let losses = total_losses.load(AtomicOrdering::Relaxed);
                
                let win_rate = if trades > 0 {
                    (wins as f64) / (trades as f64) * 100.0
                } else {
                    0.0
                };
                
                info!(
                    "📊 AI_ANALYZER: Summary - Signals: {}, Trades: {}, Win Rate: {:.1}% ({}W/{}L)",
                    signals, trades, win_rate, wins, losses
                );
                
                // Generate comprehensive analysis report
                {
                    let trade_stats_guard = trade_stats.lock().await;
                    let order_stats_guard = order_stats.lock().await;
                    let anomaly_history_guard = anomaly_history.lock().await;
                    let operation_log_guard = operation_log.lock().await;
                    let error_log_guard = error_log.lock().await;
                    
                    Self::generate_analysis_report(
                        &*trade_stats_guard,
                        &*order_stats_guard,
                        &*anomaly_history_guard,
                        &*operation_log_guard,
                        &*error_log_guard,
                        signals,
                        trades,
                        wins,
                        losses,
                        &report_file_path,
                    ).await;
                }
                
                // Analyze per-symbol statistics
                let stats = trade_stats.lock().await;
                for (symbol, stat) in stats.iter() {
                    if stat.total_trades > 5 {
                        let symbol_win_rate = if stat.total_trades > 0 {
                            (stat.winning_trades as f64) / (stat.total_trades as f64) * 100.0
                        } else {
                            0.0
                        };
                        
                        info!(
                            "📊 AI_ANALYZER: {} - Trades: {}, Win Rate: {:.1}%, PnL: ${:.2}",
                            symbol, stat.total_trades, symbol_win_rate, stat.total_pnl
                        );
                    }
                }
            }
        });
        
        Ok(())
    }
    
    /// Generate comprehensive analysis report and write to file
    async fn generate_analysis_report(
        trade_stats: &HashMap<String, TradeStats>,
        order_stats: &HashMap<String, OrderStats>,
        anomalies: &VecDeque<AnomalyReport>,
        operations: &VecDeque<OperationLog>,
        errors: &VecDeque<ErrorLog>,
        total_signals: u64,
        total_trades: u64,
        total_wins: u64,
        total_losses: u64,
        report_path: &PathBuf,
    ) {
        #[derive(Serialize)]
        struct AnalysisReport {
            timestamp: u64,
            summary: SummaryStats,
            per_symbol_stats: Vec<SymbolStats>,
            anomalies: Vec<AnomalyReport>,
            recent_errors: Vec<ErrorLog>,
            operation_analysis: OperationAnalysis,
            recommendations: Vec<String>,
        }
        
        #[derive(Serialize)]
        struct SummaryStats {
            total_signals: u64,
            total_trades: u64,
            total_wins: u64,
            total_losses: u64,
            win_rate: f64,
            total_orders: u64,
            total_anomalies: usize,
            total_errors: usize,
        }
        
        #[derive(Serialize)]
        struct SymbolStats {
            symbol: String,
            trades: u64,
            wins: u64,
            losses: u64,
            win_rate: f64,
            total_pnl: f64,
            orders: u64,
            order_rejection_rate: f64,
            order_cancel_rate: f64,
        }
        
        #[derive(Serialize)]
        struct OperationAnalysis {
            total_operations: usize,
            success_rate: f64,
            operation_types: HashMap<String, u64>,
            error_rate: f64,
        }
        
        // Calculate summary
        let total_orders: u64 = order_stats.values().map(|s| s.total_orders).sum();
        let win_rate = if total_trades > 0 {
            (total_wins as f64) / (total_trades as f64) * 100.0
        } else {
            0.0
        };
        
        let summary = SummaryStats {
            total_signals,
            total_trades,
            total_wins,
            total_losses,
            win_rate,
            total_orders,
            total_anomalies: anomalies.len(),
            total_errors: errors.len(),
        };
        
        // Per-symbol stats
        let mut per_symbol_stats = Vec::new();
        for (symbol, trade_stat) in trade_stats.iter() {
            let order_stat = order_stats.get(symbol).cloned().unwrap_or_else(|| OrderStats {
                symbol: symbol.clone(),
                total_orders: 0,
                filled_orders: 0,
                canceled_orders: 0,
                rejected_orders: 0,
                recent_spreads: VecDeque::new(),
                last_order_time: None,
            });
            
            let symbol_win_rate = if trade_stat.total_trades > 0 {
                (trade_stat.winning_trades as f64) / (trade_stat.total_trades as f64) * 100.0
            } else {
                0.0
            };
            
            let rejection_rate = if order_stat.total_orders > 0 {
                (order_stat.rejected_orders as f64) / (order_stat.total_orders as f64) * 100.0
            } else {
                0.0
            };
            
            let cancel_rate = if order_stat.total_orders > 0 {
                (order_stat.canceled_orders as f64) / (order_stat.total_orders as f64) * 100.0
            } else {
                0.0
            };
            
            per_symbol_stats.push(SymbolStats {
                symbol: symbol.clone(),
                trades: trade_stat.total_trades,
                wins: trade_stat.winning_trades,
                losses: trade_stat.losing_trades,
                win_rate: symbol_win_rate,
                total_pnl: trade_stat.total_pnl,
                orders: order_stat.total_orders,
                order_rejection_rate: rejection_rate,
                order_cancel_rate: cancel_rate,
            });
        }
        
        // Operation analysis
        let total_ops = operations.len();
        let successful_ops = operations.iter().filter(|op| op.success).count();
        let success_rate = if total_ops > 0 {
            (successful_ops as f64) / (total_ops as f64) * 100.0
        } else {
            0.0
        };
        
        let mut op_types = HashMap::new();
        for op in operations.iter() {
            *op_types.entry(op.operation_type.clone()).or_insert(0) += 1;
        }
        
        let error_rate = if total_ops > 0 {
            (errors.len() as f64) / (total_ops as f64) * 100.0
        } else {
            0.0
        };
        
        let operation_analysis = OperationAnalysis {
            total_operations: total_ops,
            success_rate,
            operation_types: op_types,
            error_rate,
        };
        
        // Generate recommendations
        let mut recommendations = Vec::new();
        
        if win_rate < 40.0 && total_trades > 10 {
            recommendations.push(format!(
                "⚠️ Low win rate ({:.1}%) detected. Consider reviewing strategy parameters or market conditions.",
                win_rate
            ));
        }
        
        for stat in per_symbol_stats.iter() {
            if stat.win_rate < 30.0 && stat.trades > 5 {
                recommendations.push(format!(
                    "⚠️ {} has very low win rate ({:.1}%). Consider pausing trading for this symbol.",
                    stat.symbol, stat.win_rate
                ));
            }
            
            if stat.order_rejection_rate > 20.0 {
                recommendations.push(format!(
                    "🚨 {} has high order rejection rate ({:.1}%). Check exchange API status and order parameters.",
                    stat.symbol, stat.order_rejection_rate
                ));
            }
            
            if stat.order_cancel_rate > 30.0 {
                recommendations.push(format!(
                    "⚠️ {} has high cancellation rate ({:.1}%). Review spread validation and market conditions.",
                    stat.symbol, stat.order_cancel_rate
                ));
            }
        }
        
        if errors.len() > 50 {
            recommendations.push(format!(
                "🚨 High error count ({}) detected. Review error logs for patterns.",
                errors.len()
            ));
        }
        
        // Recent errors (last 20)
        let recent_errors: Vec<ErrorLog> = errors.iter().rev().take(20).cloned().collect();
        
        // Recent anomalies (last 20)
        let recent_anomalies: Vec<AnomalyReport> = anomalies.iter().rev().take(20).cloned().collect();
        
        let report = AnalysisReport {
            timestamp: Self::timestamp_secs(),
            summary,
            per_symbol_stats,
            anomalies: recent_anomalies,
            recent_errors,
            operation_analysis,
            recommendations,
        };
        
        // Write report to file
        if let Ok(json) = serde_json::to_string_pretty(&report) {
            if let Ok(mut file) = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(report_path)
            {
                if let Err(e) = writeln!(file, "{}", json) {
                    warn!("AI_ANALYZER: Failed to write analysis report: {}", e);
                } else {
                    info!("✅ AI_ANALYZER: Analysis report written to {}", report_path.display());
                }
            }
        }
    }

    /// Get recent anomalies
    pub async fn get_recent_anomalies(&self, limit: usize) -> Vec<AnomalyReport> {
        let history = self.anomaly_history.lock().await;
        history.iter().rev().take(limit).cloned().collect()
    }

    /// Get trade statistics
    pub async fn get_trade_stats(&self) -> HashMap<String, TradeStats> {
        self.trade_stats.lock().await.clone()
    }

    /// Get order statistics
    pub async fn get_order_stats(&self) -> HashMap<String, OrderStats> {
        self.order_stats.lock().await.clone()
    }
}

