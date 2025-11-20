use crate::types::{BalanceChannels, SharedState};
use log::{info, warn};
use tokio::sync::broadcast;

/// Balance updates are now handled via ACCOUNT_UPDATE WebSocket events
/// This function only processes balance updates from the WebSocket stream
/// No REST API polling is needed - WebSocket provides real-time updates
pub async fn run_balance(ch: BalanceChannels, state: SharedState) {
    let mut balance_rx = ch.balance_tx.subscribe();
    
    info!("BALANCE: started - listening to ACCOUNT_UPDATE WebSocket events");
    
    loop {
        match balance_rx.recv().await {
            Ok(snapshot) => {
                // Check if there are any other receivers before updating state
                // (We're one receiver, so receiver_count() will be at least 1)
                let receiver_count = ch.balance_tx.receiver_count();
                if receiver_count > 1 {
                    // Other receivers exist, update state
                    state.apply_balance_snapshot(&snapshot);
                } else {
                    // Only this receiver, still update state (for consistency)
                    state.apply_balance_snapshot(&snapshot);
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!("BALANCE: lagged by {} messages, catching up...", n);
                // Continue receiving - broadcast channel will skip lagged messages
            }
            Err(broadcast::error::RecvError::Closed) => {
                warn!("BALANCE: balance_tx channel closed");
                break;
            }
        }
    }
}
