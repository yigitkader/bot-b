//location: /crates/app/src/order_sync.rs
// Order synchronization logic extracted from main.rs

use crate::types::*;
use crate::exec::Venue;
use crate::utils::update_fill_rate_on_cancel;
use crate::types::{OrderInfo, SymbolState};
use crate::utils::rate_limit_guard;
use rust_decimal::Decimal;
use std::time::Instant;
use tracing::{info, warn};

/// Sync orders from API and update local state
pub async fn sync_orders_from_api<V: Venue>(
    venue: &V,
    symbol: &str,
    state: &mut SymbolState,
    cfg: &crate::config::AppCfg,
) {
    let current_pos = venue.get_position(symbol).await.ok();
    
    rate_limit_guard(3).await;
    let sync_result = venue.get_open_orders(symbol).await;
    
    match sync_result {
        Ok(api_orders) => {
            let api_order_ids: std::collections::HashSet<String> = api_orders
                .iter()
                .map(|o| o.order_id.clone())
                .collect();
            
            // Track removed orders (fill vs cancel detection)
            let mut removed_orders = Vec::new();
            state.active_orders.retain(|order_id, order_info| {
                if !api_order_ids.contains(order_id) {
                    removed_orders.push(order_info.clone());
                    false
                } else {
                    true
                }
            });
            
            // Handle removed orders
            if !removed_orders.is_empty() {
                if let Some(pos) = current_pos {
                    let old_inv = state.inv.0;
                    state.inv = Qty(pos.qty.0);
                    state.last_inventory_update = Some(Instant::now());
                    
                    if old_inv != pos.qty.0 {
                        // Inventory changed - likely filled
                        state.consecutive_no_fills = 0;
                        state.order_fill_rate = (state.order_fill_rate * 0.95 + 0.05).min(1.0);
                        info!(
                            %symbol,
                            removed_orders = removed_orders.len(),
                            inv_change = %(pos.qty.0 - old_inv),
                            "orders removed and inventory changed - likely filled"
                        );
                    } else {
                        // Inventory unchanged - likely canceled
                        update_fill_rate_on_cancel(state, cfg.internal.fill_rate_decrease_factor);
                        info!(
                            %symbol,
                            removed_orders = removed_orders.len(),
                            "orders removed but inventory unchanged - likely canceled"
                        );
                    }
                } else {
                    // Position unavailable - assume filled
                    state.consecutive_no_fills = 0;
                    state.order_fill_rate = (state.order_fill_rate * cfg.internal.fill_rate_increase_factor 
                        + cfg.internal.fill_rate_increase_bonus * (removed_orders.len() as f64).min(1.0)).min(1.0);
                    warn!(
                        %symbol,
                        removed_orders = removed_orders.len(),
                        "orders removed but position unavailable, assuming filled"
                    );
                }
            }
            
            // Add orders from API that aren't in local state
            for api_order in &api_orders {
                if !state.active_orders.contains_key(&api_order.order_id) {
                    state.active_orders.insert(api_order.order_id.clone(), OrderInfo {
                        order_id: api_order.order_id.clone(),
                        client_order_id: None,
                        side: api_order.side,
                        price: api_order.price,
                        qty: api_order.qty,
                        filled_qty: Qty(Decimal::ZERO),
                        remaining_qty: api_order.qty,
                        created_at: Instant::now(),
                        last_fill_time: None,
                    });
                    info!(
                        %symbol,
                        order_id = %api_order.order_id,
                        side = ?api_order.side,
                        "found new order from API (not in local state)"
                    );
                }
            }
            
            state.last_order_sync = Some(Instant::now());
        }
        Err(err) => {
            warn!(%symbol, ?err, "failed to sync orders from API, continuing with local state");
        }
    }
}

