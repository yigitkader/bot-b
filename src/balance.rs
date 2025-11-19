use crate::types::{BalanceChannels, Connection, SharedState};
use log::{error, warn};
use std::sync::Arc;
use tokio::time::{sleep, Duration};

pub async fn run_balance(connection: Arc<Connection>, ch: BalanceChannels, state: SharedState) {
    let balance_tx = ch.balance_tx;
    loop {
        match connection.fetch_balance().await {
            Ok(snapshots) => {
                for snapshot in snapshots {
                    // Check if there are any receivers before sending
                    let receiver_count = balance_tx.receiver_count();
                    if receiver_count == 0 {
                        warn!(
                            "BALANCE: no receivers for balance_tx, skipping state update for {}",
                            snapshot.asset
                        );
                        continue;
                    }
                    
                    // Try to send to channel
                    match balance_tx.send(snapshot.clone()) {
                        Ok(_) => {
                            // Send successful - update state only if send succeeded
                            state.apply_balance_snapshot(&snapshot);
                        }
                        Err(broadcast::error::SendError(_)) => {
                            // This should not happen with broadcast channel, but handle it anyway
                            warn!(
                                "BALANCE: send failed for {}, skipping state update",
                                snapshot.asset
                            );
                        }
                    }
                }
            }
            Err(err) => error!("BALANCE: fetch_balance failed: {err:?}"),
        }

        // Reduced polling interval for faster trading decisions (10 seconds instead of 30)
        sleep(Duration::from_secs(10)).await;
    }
}
