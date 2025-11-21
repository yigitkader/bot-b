use crate::types::{BalanceChannels, SharedState};
use log::{info, warn};

/// Balance updates are now handled via ACCOUNT_UPDATE WebSocket events
/// This function only processes balance updates from the WebSocket stream
/// No REST API polling is needed - WebSocket provides real-time updates
pub async fn run_balance(ch: BalanceChannels, state: SharedState) {
    let mut balance_rx = ch.balance_tx.subscribe();

    info!("BALANCE: started - listening to ACCOUNT_UPDATE WebSocket events");

    loop {
        match crate::types::handle_broadcast_recv(balance_rx.recv().await) {
            Ok(Some(snapshot)) => state.apply_balance_snapshot(&snapshot),
            Ok(None) => {}
            Err(_) => {
                warn!("BALANCE: balance_tx channel closed");
                break;
            }
        }
    }
}
