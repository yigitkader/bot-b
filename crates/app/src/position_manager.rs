//location: /crates/app/src/position_manager.rs
// Position management, PnL tracking, and position closing logic

use anyhow::Result;
use crate::types::*;
use crate::exec::binance::BinanceFutures;
use crate::exec::Venue;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};
use crate::config::AppCfg;
use crate::types::SymbolState;
use crate::utils::{calc_net_pnl_usd, rate_limit_guard, record_pnl_snapshot, with_order_lock};

/// Sync inventory with API position
pub fn sync_inventory(
    state: &mut SymbolState,
    api_position: &Position,
    force_sync: bool,
    reconcile_threshold: Decimal,
    min_sync_interval_ms: u128,
) {
    let inv_diff = (state.inv.0 - api_position.qty.0).abs();
    let time_since_last_update = state.last_inventory_update
        .map(|t| t.elapsed().as_millis())
        .unwrap_or(1000);

    if force_sync {
        info!(
            ws_inv = %state.inv.0,
            api_inv = %api_position.qty.0,
            diff = %inv_diff,
            "force syncing position"
        );
        state.inv = api_position.qty;
        state.last_inventory_update = Some(Instant::now());
    } else if inv_diff > reconcile_threshold && time_since_last_update > min_sync_interval_ms {
        warn!(
            ws_inv = %state.inv.0,
            api_inv = %api_position.qty.0,
            diff = %inv_diff,
            threshold = %reconcile_threshold,
            time_since_last_update_ms = time_since_last_update,
            "inventory mismatch detected, syncing with API position"
        );
        state.inv = api_position.qty;
        state.last_inventory_update = Some(Instant::now());
    }
}

/// Update daily PnL reset if needed
pub fn update_daily_pnl_reset(state: &mut SymbolState) {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    let should_reset_daily = if let Some(last_reset) = state.last_daily_reset {
        const DAY_MS: u64 = 24 * 3600 * 1000;
        now_ms.saturating_sub(last_reset) >= DAY_MS
    } else {
        true
    };

    if should_reset_daily {
        let old_daily_pnl = state.daily_pnl;
        state.daily_pnl = Decimal::ZERO;
        state.last_daily_reset = Some(now_ms);
        info!(
            old_daily_pnl = %old_daily_pnl,
            "daily PnL reset (new day started)"
        );
    }
}

/// Apply funding cost if needed
pub fn apply_funding_cost(
    state: &mut SymbolState,
    funding_rate: Option<f64>,
    next_funding_time: Option<u64>,
    position_size_notional: f64,
) {
    if let (Some(funding_rate), Some(next_funding_ts)) = (funding_rate, next_funding_time) {
        const FUNDING_INTERVAL_MS: u64 = 8 * 3600 * 1000;
        let this_funding_ts = next_funding_ts.saturating_sub(FUNDING_INTERVAL_MS);

        let should_apply = if let Some(last_applied) = state.last_applied_funding_time {
            this_funding_ts > last_applied
        } else {
            true
        };

        if should_apply && position_size_notional > 0.0 {
            let funding_cost = funding_rate * position_size_notional;
            state.total_funding_cost += Decimal::from_f64_retain(funding_cost).unwrap_or(Decimal::ZERO);
            state.last_applied_funding_time = Some(this_funding_ts);

            info!(
                funding_rate,
                this_funding_ts,
                next_funding_ts,
                position_size_notional,
                funding_cost,
                total_funding_cost = %state.total_funding_cost,
                "funding cost applied (8-hour interval)"
            );
        }
    }
}

/// Update position tracking fields
pub fn update_position_tracking(
    state: &mut SymbolState,
    position: &Position,
    mark_px: Px,
    cfg: &AppCfg,
) {
    // Update average entry price
    if !position.qty.0.is_zero() {
        state.avg_entry_price = Some(position.entry.0);
    } else {
        state.avg_entry_price = None;
    }

    // Record PnL snapshot
    record_pnl_snapshot(
        &mut state.pnl_history,
        position,
        mark_px,
        cfg.internal.pnl_history_max_len,
    );

    // Update position size history
    let position_size_notional = (mark_px.0 * position.qty.0.abs()).to_f64().unwrap_or(0.0);
    state.position_size_notional_history.push(position_size_notional);
    if state.position_size_notional_history.len() > cfg.internal.position_size_history_max_len {
        state.position_size_notional_history.remove(0);
    }

    // Update position hold duration (entry_time sadece WebSocket event'te set edilir)
    // KRİTİK DÜZELTME: Position entry time SADECE WebSocket fill event'te set edilir
    // Fallback kaldırıldı - WebSocket geç gelirse bile yanlış zaman set etmek yerine bekler
    // Bu, PnL hesaplamalarının doğruluğunu garanti eder
    let position_qty_threshold = Decimal::from_str_radix(&cfg.internal.position_qty_threshold, 10)
        .unwrap_or(Decimal::new(1, 8));
    if position.qty.0.abs() > position_qty_threshold {
        // Entry time varsa hold duration'ı güncelle
        if let Some(entry_time) = state.position_entry_time {
            state.position_hold_duration_ms = entry_time.elapsed().as_millis() as u64;
        }
        // Entry time yoksa set etme - WebSocket event bekleniyor
    } else {
        // Pozisyon kapalı - entry_time ve hold_duration sıfırla
        state.position_entry_time = None;
        state.peak_pnl = Decimal::ZERO;
        state.position_hold_duration_ms = 0;
    }
}

/// ✅ KRİTİK: Basit scalping kapatma mantığı - $0.50 net kâr garantisi
/// Trailing/volatilite/akıllı kurallar kaldırıldı (sadelik, daha az request)
/// Sadece TP ($0.50) ve opsiyonel SL (-$0.10) kaldı
pub fn should_close_position_smart(
    state: &SymbolState,
    position: &Position,
    mark_px: Px,
    bid: Px,
    ask: Px,
    min_profit_usd: f64,
    maker_fee_rate: f64,
    taker_fee_rate: f64,
) -> (bool, String) {
    let position_qty_f64 = position.qty.0.to_f64().unwrap_or(0.0);
    let entry_price_f64 = position.entry.0.to_f64().unwrap_or(0.0);
    let mark_price_f64 = mark_px.0.to_f64().unwrap_or(0.0);

    if position_qty_f64.abs() <= 0.0001 || entry_price_f64 <= 0.0 || mark_price_f64 <= 0.0 {
        return (false, "no_position".to_string());
    }

    let position_side = if position.qty.0.is_sign_positive() {
        Side::Buy
    } else {
        Side::Sell
    };

    // ✅ KRİTİK: Exit price = market price (bid/ask) - market order ile kapatılacak
    // Taker fee ile hesaplama yapılıyor (market exit garantisi)
    let exit_price = match position_side {
        Side::Buy => bid.0, // Long pozisyon → Sell (bid'den sat)
        Side::Sell => ask.0, // Short pozisyon → Buy (ask'ten al)
    };

    // ✅ KRİTİK: Entry fee = maker (limit order ile açıldı)
    // Exit fee = taker (market order ile kapatılacak - anında kapatma için)
    let entry_fee_bps = maker_fee_rate * 10000.0;
    let exit_fee_bps = taker_fee_rate * 10000.0; // Market exit = taker fee

    let qty_abs = position.qty.0.abs();
    
    // ✅ KRİTİK: Net PnL hesaplama - taker fee ile (market exit)
    let net_pnl = calc_net_pnl_usd(
        position.entry.0,
        exit_price,
        qty_abs,
        &position_side,
        entry_fee_bps,
        exit_fee_bps,
    );

    // ✅ KRİTİK: Rule 1: Fixed TP ($0.50) - net PnL ≥ min_profit_usd
    // Net PnL zaten taker fee ile hesaplandı, market ile anında kapatılacak
    if net_pnl >= min_profit_usd {
        return (true, format!("take_profit_{:.2}_usd", net_pnl));
    }

    // ✅ KRİTİK: Opsiyonel SL (-$0.10) - küçük zararlarda hemen kapat
    // İstersen bu satırı kaldırarak sadece kârla kapanış yapabilirsin
    if net_pnl <= -0.10 {
        return (true, format!("stop_loss_{:.2}_usd", net_pnl));
    }

    // ✅ KRİTİK: Mutlak timeout - pozisyon ne durumda olursa olsun maksimum süre
    // Market making için pozisyonlar çok uzun süre açık kalmamalı
    if let Some(entry_time) = state.position_entry_time {
        let age_secs = entry_time.elapsed().as_secs() as f64;
        if age_secs >= crate::constants::MAX_POSITION_DURATION_SEC {
            return (true, format!("max_duration_timeout_{:.2}_usd_{:.0}_sec", net_pnl, age_secs));
        }
    }

    // ✅ KRİTİK: Inventory threshold kontrolü - pozisyon birikirse zorla kapat
    // Basit threshold: pozisyon çok büyükse kapat (0.5'ten fazla)
    if position_qty_f64.abs() > 0.5 {
        return (true, format!("inventory_threshold_exceeded_{:.2}", position_qty_f64));
    }

    // ✅ KRİTİK: Tüm "smart" kurallar kaldırıldı (trailing, volatilite, momentum, recovery, drawdown)
    // Sadece basit TP/SL ve timeout kuralları kaldı - scalping için yeterli
    (false, "".to_string())
}

/// Close position with retry mechanism
/// KRİTİK: Race condition önleme - WebSocket event sync kontrolü
/// ✅ Thread-safe: AtomicBool kullanarak aynı anda sadece bir close işlemi yapılmasını garanti eder
pub async fn close_position(
    venue: &BinanceFutures,
    symbol: &str,
    state: &mut SymbolState,
) -> Result<()> {
    // ✅ KRİTİK: Atomik swap - eğer zaten true ise (başka thread close ediyor), false döner ve skip eder
    if state.position_closing.swap(true, Ordering::AcqRel) {
        // Zaten başka bir thread/task pozisyonu kapatıyor
        info!(symbol = %symbol, "position close already in progress, skipping duplicate close");
        return Ok(());
    }
    
    state.last_close_attempt = Some(Instant::now());

    // KRİTİK RACE CONDITION FIX: WebSocket event sonrası inventory sync kontrolü
    // Eğer son 2 saniye içinde inventory güncellemesi varsa ve inventory sıfırsa,
    // pozisyon zaten kapalı kabul edilmeli (WebSocket event handler tarafından kapatılmış olabilir)
    if let Some(last_update) = state.last_inventory_update {
        let time_since_update = last_update.elapsed().as_millis();
        if time_since_update < 2000 {
            // Son 2 saniye içinde güncelleme var
            if state.inv.0.is_zero() {
                // Inventory sıfır = pozisyon kapalı
                info!(
                    symbol = %symbol,
                    time_since_update_ms = time_since_update,
                    "position already closed (WebSocket sync detected)"
                );
                state.position_closing.store(false, Ordering::Release);
                return Ok(());
            }
        }
    }

    // Use global order lock to ensure only one order/position operation at a time
    with_order_lock(async {
        // KRİTİK: Önce tüm açık emirleri iptal et (pozisyon kapatmadan önce)
        rate_limit_guard(1).await;
        if let Err(e) = venue.cancel_all(symbol).await {
            warn!(symbol = %symbol, error = %e, "failed to cancel orders before close, continuing anyway");
        }

        // Pozisyonu kapat (retry mekanizması ile)
        let max_retries = 3;
        let mut last_error = None;
        
        for attempt in 1..=max_retries {
            // KRİTİK RACE CONDITION FIX: Her attempt öncesi WebSocket sync kontrolü
            // Retry sırasında WebSocket event handler pozisyonu kapatmış olabilir
            if let Some(last_update) = state.last_inventory_update {
                let time_since_update = last_update.elapsed().as_millis();
                if time_since_update < 2000 && state.inv.0.is_zero() {
                    // Son 2 saniye içinde güncelleme var ve inventory sıfır
                    info!(
                        symbol = %symbol,
                        attempt,
                        time_since_update_ms = time_since_update,
                        "position closed during retry (WebSocket sync detected)"
                    );
                    state.position_closing.store(false, Ordering::Release);
                    return Ok(());
                }
            }
            
            rate_limit_guard(1).await;
            match venue.close_position(symbol).await {
                Ok(_) => {
                    info!(symbol = %symbol, attempt, "position closed successfully");
                    state.position_closing.store(false, Ordering::Release);
                    return Ok(());
                }
                Err(e) => {
                    last_error = Some(e);
                    if attempt < max_retries {
                        warn!(symbol = %symbol, attempt, max_retries, "failed to close position, retrying...");
                        tokio::time::sleep(Duration::from_millis(500 * attempt as u64)).await;
                    }
                }
            }
        }
        
        state.position_closing.store(false, Ordering::Release);
        
        if let Some(e) = last_error {
            error!(symbol = %symbol, error = %e, "failed to close position after {} attempts", max_retries);
            Err(e)
        } else {
            Ok(())
        }
    }).await

}

