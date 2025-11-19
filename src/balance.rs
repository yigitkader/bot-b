use crate::connection::Connection;
use crate::event_bus::BalanceChannels;
use crate::state::SharedState;
use log::error;
use std::sync::Arc;
use tokio::time::{sleep, Duration};

pub async fn run_balance(connection: Arc<Connection>, ch: BalanceChannels, state: SharedState) {
    let balance_tx = ch.balance_tx;
    loop {
        match connection.fetch_balance().await {
            Ok(snapshots) => {
                for snapshot in snapshots {
                    if let Err(err) = balance_tx.send(snapshot.clone()) {
                        error!("BALANCE: balance_tx error: {err}");
                    }
                    state.apply_balance_snapshot(&snapshot);
                }
            }
            Err(err) => error!("BALANCE: fetch_balance failed: {err:?}"),
        }

        sleep(Duration::from_secs(30)).await;
    }
}
