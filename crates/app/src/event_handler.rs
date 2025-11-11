//location: /crates/app/src/event_handler.rs
// WebSocket event handling logic extracted and optimized from main.rs

use crate::binance_exec::BinanceFutures;
use crate::types::*;
use crate::exec::Venue;
use crate::utils::update_fill_rate_on_cancel;
use crate::types::SymbolState;
use crate::utils::rate_limit_guard;
use tracing::{info, warn};

/// Handle WebSocket reconnect sync for all symbols
pub async fn handle_reconnect_sync(
    venue: &BinanceFutures,
    states: &mut [SymbolState],
    cfg: &crate::config::AppCfg,
) {
    for state in states.iter_mut() {
        let current_pos = venue.get_position(&state.meta.symbol).await.ok();
        
        rate_limit_guard(3).await;
        if let Ok(api_orders) = venue.get_open_orders(&state.meta.symbol).await {
            let api_order_ids: std::collections::HashSet<String> = api_orders
                .iter()
                .map(|o| o.order_id.clone())
                .collect();
            
            let mut removed_orders = Vec::new();
            state.active_orders.retain(|order_id, order_info| {
                if !api_order_ids.contains(order_id) {
                    removed_orders.push(order_info.clone());
                    false
                } else {
                    true
                }
            });
            
            if !removed_orders.is_empty() {
                if let Some(pos) = current_pos {
                    let old_inv = state.inv.0;
                    state.inv = Qty(pos.qty.0);
                    state.last_inventory_update = Some(std::time::Instant::now());
                    
                    if old_inv != pos.qty.0 {
                        state.consecutive_no_fills = 0;
                        state.order_fill_rate = (state.order_fill_rate * 0.95 + 0.05).min(1.0);
                        info!(
                            symbol = %state.meta.symbol,
                            removed_orders = removed_orders.len(),
                            inv_change = %(pos.qty.0 - old_inv),
                            "reconnect sync: orders removed and inventory changed - likely filled"
                        );
                    } else {
                        update_fill_rate_on_cancel(state, cfg.internal.fill_rate_decrease_factor);
                        info!(
                            symbol = %state.meta.symbol,
                            removed_orders = removed_orders.len(),
                            "reconnect sync: orders removed but inventory unchanged - likely canceled"
                        );
                    }
                } else {
                    state.consecutive_no_fills = 0;
                    state.order_fill_rate = (state.order_fill_rate * cfg.internal.fill_rate_reconnect_factor + cfg.internal.fill_rate_reconnect_bonus).min(1.0);
                    warn!(
                        symbol = %state.meta.symbol,
                        removed_orders = removed_orders.len(),
                        "reconnect sync: orders removed but position unavailable, assuming filled"
                    );
                }
            }
        } else {
            warn!(symbol = %state.meta.symbol, "failed to sync orders after reconnect");
        }
    }
}

/// Handle order fill event with deduplication
pub fn handle_order_fill(
    state: &mut SymbolState,
    symbol: &str,
    order_id: &str,
    cumulative_filled_qty: Qty,
    order_status: &str,
) -> bool {
    // Event ID bazlı duplicate kontrolü
    let event_timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let event_id = format!("{}-{}-{}", order_id, cumulative_filled_qty.0, event_timestamp);
    
    if state.processed_events.contains(&event_id) {
        warn!(
            %symbol,
            order_id = %order_id,
            cumulative_filled_qty = %cumulative_filled_qty.0,
            "duplicate fill event ignored"
        );
        return false;
    }
    
    // Legacy duplicate check
    let is_duplicate = state.active_orders.get(order_id)
        .map(|o| o.filled_qty.0 >= cumulative_filled_qty.0)
        .unwrap_or(false);
    
    if is_duplicate {
        warn!(
            %symbol,
            order_id = %order_id,
            "duplicate fill event ignored (legacy check)"
        );
        return false;
    }
    
    // Event ID'yi kaydet ve memory leak önle
    state.processed_events.insert(event_id);
    if state.processed_events.len() > 1000 {
        if state.last_event_cleanup
            .map(|t| t.elapsed().as_secs() > 3600)
            .unwrap_or(true)
        {
            state.processed_events.clear();
            state.last_event_cleanup = Some(std::time::Instant::now());
        } else if state.last_event_cleanup.is_none() {
            state.last_event_cleanup = Some(std::time::Instant::now());
        }
    }
    
    true
}


