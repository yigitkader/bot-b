//location: /crates/app/src/risk_handler.rs
// Risk level handling logic extracted and optimized from main.rs

use crate::exec::Venue;
use crate::risk_manager::PositionRiskLevel;
use crate::types::SymbolState;
use crate::utils::rate_limit_guard;
use std::time::Instant;
use tracing::{error, info, warn};

/// Handle risk level actions (Hard, Medium, Soft, Ok)
pub async fn handle_risk_level<V: Venue>(
    venue: &V,
    symbol: &str,
    state: &mut SymbolState,
    risk_level: PositionRiskLevel,
    position_size_notional: f64,
    total_exposure: f64,
    max_allowed: f64,
) -> bool {
    // Returns true if should continue processing, false if should skip
    match risk_level {
        PositionRiskLevel::Hard => {
            warn!(
                %symbol,
                position_size_notional,
                total_exposure,
                max_allowed,
                "HARD LIMIT: force closing position"
            );
            state.position_closing = true;
            state.last_close_attempt = Some(Instant::now());
            
            rate_limit_guard(2).await;
            let _ = venue.cancel_all(symbol).await;
            if venue.close_position(symbol).await.is_ok() {
                info!(%symbol, "closed position due to hard limit");
            } else {
                error!(%symbol, "failed to close position due to hard limit");
            }
            state.position_closing = false;
            false // Skip further processing
        }
        PositionRiskLevel::Medium => {
            warn!(
                %symbol,
                position_size_notional,
                total_exposure,
                "MEDIUM LIMIT: reducing active orders"
            );
            // Cancel oldest 50% of orders
            let mut orders_with_times: Vec<(String, Instant)> = state.active_orders.iter()
                .map(|(id, o)| (id.clone(), o.created_at))
                .collect();
            orders_with_times.sort_by_key(|(_, t)| *t);
            
            let to_cancel: Vec<String> = orders_with_times
                .into_iter()
                .take((state.active_orders.len() / 2).max(1))
                .map(|(id, _)| id)
                .collect();
            
            for order_id in &to_cancel {
                rate_limit_guard(1).await;
                if venue.cancel(order_id, symbol).await.is_ok() {
                    state.active_orders.remove(order_id);
                    state.last_order_price_update.remove(order_id);
                }
            }
            true // Continue processing
        }
        PositionRiskLevel::Soft => {
            info!(
                %symbol,
                position_size_notional,
                total_exposure,
                "SOFT LIMIT: blocking new orders"
            );
            true // Continue processing (new orders will be blocked)
        }
        PositionRiskLevel::Ok => true, // Normal operation
    }
}

