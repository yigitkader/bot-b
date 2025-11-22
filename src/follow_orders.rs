use crate::types::{CloseRequest, MarketTick, PositionUpdate, Side};
use crate::types::{FollowChannels, PositionMeta, SharedState};
use chrono::{DateTime, Utc};
use log::{info, warn};
use std::collections::HashMap;
use uuid::Uuid;

/// Advanced position manager with trailing stops and partial exits
/// Based on TrendPlan.md recommendations
struct AdvancedPositionManager {
    positions: HashMap<Uuid, ActivePosition>,
}

struct ActivePosition {
    entry_price: f64,
    entry_time: DateTime<Utc>,
    side: Side,
    size: f64,
    leverage: f64,
    atr_at_entry: f64,
    // Dynamic tracking
    peak_profit_pct: f64,       // En yüksek kar (trailing için)
    lowest_profit_pct: f64,     // En düşük kar (risk tracking için)
    partial_exits: Vec<PartialExit>,
}

struct PartialExit {
    exit_time: DateTime<Utc>,
    exit_price: f64,
    size: f64,
    profit_pct: f64,
}

#[derive(Debug)]
enum PositionDecision {
    NoPosition,
    Hold,
    PartialClose {
        percentage: f64,
        reason: String,
    },
    FullClose {
        reason: String,
    },
}

impl AdvancedPositionManager {
    fn new() -> Self {
        Self {
            positions: HashMap::new(),
        }
    }

    fn open_position(
        &mut self,
        position_id: Uuid,
        entry_price: f64,
        side: Side,
        size: f64,
        leverage: f64,
        atr: f64,
    ) {
        self.positions.insert(
            position_id,
            ActivePosition {
                entry_price,
                entry_time: Utc::now(),
                side,
                size,
                leverage,
                atr_at_entry: atr,
                peak_profit_pct: 0.0,
                lowest_profit_pct: 0.0,
                partial_exits: Vec::new(),
            },
        );
    }

    fn close_position(&mut self, position_id: &Uuid) {
        self.positions.remove(position_id);
    }

    fn evaluate(
        &mut self,
        position_id: &Uuid,
        current_price: f64,
        current_atr: f64,
    ) -> PositionDecision {
        let pos = match self.positions.get_mut(position_id) {
            Some(p) => p,
            None => return PositionDecision::NoPosition,
        };

        // Calculate current profit
        let profit_pct = match pos.side {
            Side::Long => (current_price - pos.entry_price) / pos.entry_price,
            Side::Short => (pos.entry_price - current_price) / pos.entry_price,
        } * pos.leverage * 100.0;

        // Update peak/lowest tracking
        pos.peak_profit_pct = pos.peak_profit_pct.max(profit_pct);
        pos.lowest_profit_pct = pos.lowest_profit_pct.min(profit_pct);

        // Calculate time in position
        let holding_duration = Utc::now() - pos.entry_time;
        let holding_minutes = holding_duration.num_minutes() as f64;

        // === STRATEGY #1: STOP LOSS (Priority!) ===
        // ✅ CRITICAL FIX: Use ATR/Price ratio instead of absolute ATR
        // This ensures consistent stop loss across different price levels
        // Example: BTC $40k (ATR $500 = 1.25%) vs Altcoin $0.001 (ATR $0.00005 = 5%)
        let atr_ratio = if pos.entry_price > 0.0 {
            pos.atr_at_entry / pos.entry_price
        } else {
            0.01 // Fallback: 1% if entry price is invalid
        };
        let stop_loss_multiplier = 2.5;
        let initial_stop_loss = match pos.side {
            Side::Long => pos.entry_price * (1.0 - stop_loss_multiplier * atr_ratio),
            Side::Short => pos.entry_price * (1.0 + stop_loss_multiplier * atr_ratio),
        };

        let hit_stop_loss = match pos.side {
            Side::Long => current_price <= initial_stop_loss,
            Side::Short => current_price >= initial_stop_loss,
        };

        if hit_stop_loss {
            log::info!(
                "🛑 STOP LOSS HIT: {:.2}% loss @ {}",
                profit_pct,
                current_price
            );
            return PositionDecision::FullClose {
                reason: "Stop loss hit".to_string(),
            };
        }

        // === STRATEGY #2: PARTIAL EXITS (Lock Profits!) ===
        // ✅ FIX (Plan.md): Increased threshold from 1.0% to 1.5% to avoid premature exits
        // Crypto markets are very noisy - 1% profit can be hit by normal volatility (stop hunting)
        // 1.5% threshold reduces false exits while still locking profits early
        // First partial: 50% at +1.5% profit
        if profit_pct >= 1.5 && pos.partial_exits.is_empty() {
            log::info!("💰 PARTIAL EXIT #1: 50% @ +1.5% profit");
            pos.partial_exits.push(PartialExit {
                exit_time: Utc::now(),
                exit_price: current_price,
                size: pos.size * 0.5,
                profit_pct: 1.5,
            });
            pos.size *= 0.5; // Reduce remaining position
            
            return PositionDecision::PartialClose {
                percentage: 0.5,
                reason: "First profit target (+1.5%)".to_string(),
            };
        }

        // Second partial: 25% at +2% profit
        if profit_pct >= 2.0 && pos.partial_exits.len() == 1 {
            log::info!("💰 PARTIAL EXIT #2: 25% @ +2.0% profit");
            pos.partial_exits.push(PartialExit {
                exit_time: Utc::now(),
                exit_price: current_price,
                size: pos.size * 0.5,
                profit_pct: 2.0,
            });
            pos.size *= 0.5;
            
            return PositionDecision::PartialClose {
                percentage: 0.25,
                reason: "Second profit target (+2%)".to_string(),
            };
        }

        // === STRATEGY #3: TRAILING STOP (Let Winners Run!) ===
        // After first partial exit, use trailing stop for remaining
        if !pos.partial_exits.is_empty() {
            // Trail at 50% of peak profit
            let trailing_stop_pct = pos.peak_profit_pct * 0.5;
            
            if profit_pct < trailing_stop_pct {
                log::info!(
                    "🎯 TRAILING STOP: Exit @ {:.2}% (peak was {:.2}%)",
                    profit_pct,
                    pos.peak_profit_pct
                );
                return PositionDecision::FullClose {
                    reason: format!(
                        "Trailing stop (peak: {:.2}%, current: {:.2}%)",
                        pos.peak_profit_pct, profit_pct
                    ),
                };
            }
        }

        // === STRATEGY #4: TIME-BASED EXITS ===
        // Maximum 4 hours holding
        if holding_minutes > 240.0 {
            log::info!("⏰ TIME EXIT: 4 hours reached");
            return PositionDecision::FullClose {
                reason: "Maximum holding time (4 hours)".to_string(),
            };
        }

        // === STRATEGY #5: VOLATILITY EXPANSION EXIT ===
        // If ATR suddenly spikes (2x), market is too volatile
        let atr_expansion = current_atr / pos.atr_at_entry;
        if atr_expansion > 2.0 {
            log::warn!("⚠️  VOLATILITY EXIT: ATR expanded {:.2}x", atr_expansion);
            return PositionDecision::FullClose {
                reason: "Excessive volatility".to_string(),
            };
        }

        // === STRATEGY #6: REVERSAL DETECTION ===
        // If price retraces 70% from peak, exit
        if pos.peak_profit_pct > 0.5 {
            let retracement_pct = (pos.peak_profit_pct - profit_pct) / pos.peak_profit_pct;
            
            if retracement_pct > 0.7 {
                log::warn!(
                    "🔄 REVERSAL DETECTED: {:.0}% retracement from peak",
                    retracement_pct * 100.0
                );
                return PositionDecision::FullClose {
                    reason: "Reversal detected (70% retracement)".to_string(),
                };
            }
        }

        // === STRATEGY #7: EARLY PROFIT TAKING (Small Wins) ===
        // If position is small profit but time is passing, take it
        if holding_minutes > 30.0 && profit_pct >= 0.3 && profit_pct < 0.5 {
            log::info!("✨ EARLY PROFIT: Small win @ {:.2}%", profit_pct);
            return PositionDecision::FullClose {
                reason: "Early profit taking (small win)".to_string(),
            };
        }

        // Hold position
        PositionDecision::Hold
    }
}

pub async fn run_follow_orders(
    ch: FollowChannels,
    state: SharedState,
    tp_percent: f64,
    sl_percent: f64,
    commission_pct: f64,
    atr_sl_multiplier: f64,
    atr_tp_multiplier: f64,
) {
    let FollowChannels {
        mut market_rx,
        mut position_update_rx,
        close_tx,
    } = ch;

    // Tekil değişken yerine HashMap kullanıyoruz
    // Symbol -> PositionUpdate
    let mut active_positions_map: HashMap<String, PositionUpdate> = HashMap::new();
    let mut position_manager = AdvancedPositionManager::new();
    
    // Her sembol için son bilinen ATR (optimize etmek için)
    let mut symbol_atrs: HashMap<String, f64> = HashMap::new();
    // ✅ FIX (Plan.md): Son ATR değerlerini dinamik güncellemek için fiyat geçmişi tut
    let mut symbol_last_prices: HashMap<String, Vec<f64>> = HashMap::new();

    loop {
        tokio::select! {
            res = position_update_rx.recv() => match crate::types::handle_broadcast_recv(res) {
                Ok(Some(update)) => {
                    state.update_equity(update.unrealized_pnl);
                    
                    if update.is_closed {
                        position_manager.close_position(&update.position_id);
                        active_positions_map.remove(&update.symbol);
                        symbol_atrs.remove(&update.symbol);
                        // ✅ FIX (Plan.md): Pozisyon kapandığında fiyat geçmişini de temizle
                        symbol_last_prices.remove(&update.symbol);
                    } else {
                        // Yeni veya güncellenen pozisyon
                        let is_new = !active_positions_map.contains_key(&update.symbol);
                        
                        if is_new {
                            let position_meta = state.current_position_meta(&update.symbol);
                            let atr = position_meta
                                .and_then(|m| m.atr_at_entry)
                                .unwrap_or(0.01); // Fallback
                            
                            position_manager.open_position(
                                update.position_id,
                                update.entry_price,
                                update.side,
                                update.size,
                                update.leverage,
                                atr,
                            );
                            symbol_atrs.insert(update.symbol.clone(), atr);
                        }
                        
                        active_positions_map.insert(update.symbol.clone(), update);
                    }
                },
                Ok(None) => continue,
                Err(_) => break,
            },
            res = market_rx.recv() => match crate::types::handle_broadcast_recv(res) {
                Ok(Some(tick)) => {
                    // ✅ FIX (Plan.md): ATR'yi dinamik olarak güncelle
                    // Sadece aktif pozisyon olan semboller için ATR güncelle (verimlilik)
                    if let Some(position) = active_positions_map.get(&tick.symbol) {
                        // Fiyat geçmişini güncelle
                        let prices = symbol_last_prices
                            .entry(tick.symbol.clone())
                            .or_insert_with(Vec::new);
                        
                        prices.push(tick.price);
                        
                        // Son 14 fiyatı tut (ATR periyodu için yeterli)
                        if prices.len() > 14 {
                            prices.remove(0);
                        }
                        
                        // Basit volatilite hesabı (gerçek ATR için high/low gerekir, ama MarketTick'te sadece price var)
                        // True Range yerine price return'lerinin ortalamasını kullan
                        let current_atr = if prices.len() >= 2 {
                            let returns: Vec<f64> = prices.windows(2)
                                .map(|w| ((w[1] - w[0]) / w[0]).abs())
                                .collect();
                            let avg_return = returns.iter().sum::<f64>() / returns.len() as f64;
                            let estimated_atr = avg_return * tick.price;
                            symbol_atrs.insert(tick.symbol.clone(), estimated_atr);
                            estimated_atr
                        } else {
                            // Yeterli veri yoksa mevcut ATR'yi kullan (veya fallback)
                            *symbol_atrs.get(&tick.symbol).unwrap_or(&0.01)
                        };

                        // Position Manager ile değerlendir
                        let decision = position_manager.evaluate(
                            &position.position_id,
                            tick.price,
                            current_atr,
                        );

                        match decision {
                            PositionDecision::FullClose { reason } => {
                                log::info!("📤 FULL CLOSE ({}): {}", tick.symbol, reason);
                                let _ = close_tx.send(CloseRequest {
                                    position_id: position.position_id,
                                    reason,
                                    ts: Utc::now(),
                                    partial_close_percentage: None,
                                }).await;
                            }
                            PositionDecision::PartialClose { percentage, reason } => {
                                log::info!("📤 PARTIAL CLOSE ({}) {:.0}%: {}", tick.symbol, percentage * 100.0, reason);
                                let _ = close_tx.send(CloseRequest {
                                    position_id: position.position_id,
                                    reason: format!("Partial: {}", reason),
                                    ts: Utc::now(),
                                    partial_close_percentage: Some(percentage),
                                }).await;
                            }
                            PositionDecision::Hold => {
                                // Legacy TP/SL kontrolü (Yedek güvenlik)
                                let position_meta = state.current_position_meta(&tick.symbol);
                                if let Some(req) = evaluate_position(
                                    position,
                                    &tick,
                                    position_meta,
                                    tp_percent,
                                    sl_percent,
                                    commission_pct,
                                    atr_sl_multiplier,
                                    atr_tp_multiplier,
                                ) {
                                    let _ = close_tx.send(req).await;
                                }
                            }
                            PositionDecision::NoPosition => {}
                        }
                    }
                },
                Ok(None) => continue,
                Err(_) => break,
            }
        }
    }
}

fn evaluate_position(
    position: &PositionUpdate,
    tick: &MarketTick,
    position_meta: Option<PositionMeta>,
    tp_percent: f64,
    sl_percent: f64,
    commission_pct: f64,
    atr_sl_multiplier: f64,
    atr_tp_multiplier: f64,
) -> Option<CloseRequest> {
    // CRITICAL: Validate position size first
    // Position size <= 0 means position is already closed or invalid (data corruption)
    // TP/SL calculation should not proceed with invalid position size
    if position.size <= f64::EPSILON {
        warn!(
            "FOLLOW_ORDERS: invalid position size {} for position {} (position already closed or corrupted)",
            position.size, position.position_id
        );
        return None;
    }

    // Validate entry price
    if position.entry_price <= 0.0 {
        warn!(
            "FOLLOW_ORDERS: invalid entry price {} for position {}",
            position.entry_price, position.position_id
        );
        return None;
    }

    // Validate leverage
    if position.leverage <= 0.0 {
        warn!(
            "FOLLOW_ORDERS: invalid leverage {} for position {}",
            position.leverage, position.position_id
        );
        return None;
    }

    if let Some(meta) = position_meta {
        if let Some(atr_value) = meta.atr_at_entry {
            if atr_value > 0.0 && position.entry_price > 0.0 {
                // ✅ CRITICAL FIX: Use ATR/Price ratio instead of absolute ATR
                // This ensures consistent stop loss across different price levels
                // Example: BTC $40k (ATR $500 = 1.25%) vs Altcoin $0.001 (ATR $0.00005 = 5%)
                let atr_ratio = atr_value / position.entry_price;
                
                let (sl_hit, tp_hit, sl_price, tp_price) = match position.side {
                    Side::Long => {
                        let sl_price = position.entry_price * (1.0 - atr_sl_multiplier * atr_ratio);
                        let tp_price = position.entry_price * (1.0 + atr_tp_multiplier * atr_ratio);
                        (
                            tick.price <= sl_price,
                            tick.price >= tp_price,
                            sl_price,
                            tp_price,
                        )
                    }
                    Side::Short => {
                        let sl_price = position.entry_price * (1.0 + atr_sl_multiplier * atr_ratio);
                        let tp_price = position.entry_price * (1.0 - atr_tp_multiplier * atr_ratio);
                        (
                            tick.price >= sl_price,
                            tick.price <= tp_price,
                            sl_price,
                            tp_price,
                        )
                    }
                };

                if sl_hit {
                    info!(
                        "FOLLOW_ORDERS: ATR stop hit at {:.4} (entry {:.4}, atr {:.4})",
                        sl_price, position.entry_price, atr_value
                    );
                    return Some(CloseRequest {
                        position_id: position.position_id,
                        reason: format!(
                            "ATR SL hit ({:.4} vs entry {:.4})",
                            sl_price, position.entry_price
                        ),
                        ts: Utc::now(),
                        partial_close_percentage: None, // Full close on stop-loss
                    });
                } else if tp_hit {
                    info!(
                        "FOLLOW_ORDERS: ATR take-profit hit at {:.4} (entry {:.4}, atr {:.4})",
                        tp_price, position.entry_price, atr_value
                    );
                    return Some(CloseRequest {
                        position_id: position.position_id,
                        reason: format!(
                            "ATR TP hit ({:.4} vs entry {:.4})",
                            tp_price, position.entry_price
                        ),
                        ts: Utc::now(),
                        partial_close_percentage: None, // Full close on take-profit
                    });
                }
            }
        }
    }

    // Calculate direction multiplier
    let direction = if position.side == Side::Long {
        1.0
    } else {
        -1.0
    };

    // Calculate price change percentage (unleveraged)
    // For Long: positive if price goes up, negative if price goes down
    // For Short: positive if price goes down, negative if price goes up
    let price_change_pct = (tick.price - position.entry_price) / position.entry_price * direction;

    // Calculate ROI (Return on Investment) = PnL / Margin
    // ROI = price_change_pct * leverage
    // Example: Entry $40,000, Current $40,400, Leverage 100x
    // price_change_pct = (40400 - 40000) / 40000 = 0.01 (1%)
    // ROI = 0.01 * 100 = 1.0 (100%)
    let roi_pct = price_change_pct * position.leverage * 100.0;

    // Subtract commission (round-trip: entry + exit)
    // Commission is already in percentage (e.g., 0.08 for 0.08%)
    let net_roi_pct = roi_pct - commission_pct;

    // Check TP/SL thresholds
    if net_roi_pct >= tp_percent || net_roi_pct <= -sl_percent {
        info!(
            "FOLLOW_ORDERS: target hit (price_change: {:.4}%, leverage: {:.1}x, ROI: {:.2}%, net ROI: {:.2}%), requesting close",
            price_change_pct * 100.0, position.leverage, roi_pct, net_roi_pct
        );
        Some(CloseRequest {
            position_id: position.position_id,
            reason: format!(
                "TP/SL net ROI: {:.2}% (ROI: {:.2}%, leverage: {:.1}x)",
                net_roi_pct, roi_pct, position.leverage
            ),
            ts: Utc::now(),
            partial_close_percentage: None, // Full close on TP/SL
        })
    } else {
        None
    }
}
