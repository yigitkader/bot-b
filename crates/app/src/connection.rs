//location: /crates/app/src/connection.rs
// Connection Module: Handle APIs, WebSocket connections
// Includes: binance_exec.rs and binance_ws.rs content

// Include binance_exec and binance_ws as modules
#[path = "binance_exec.rs"]
mod binance_exec;
#[path = "binance_ws.rs"]
mod binance_ws;

// Re-export public API
pub use binance_exec::{
    BinanceCommon, BinanceFutures, BinanceFutures as Venue, SymbolRules, SymbolMeta, VenueOrder,
    Venue as VenueTrait,
};
pub use binance_ws::{UserDataStream, UserEvent, UserStreamKind, WsStream};

use crate::config::AppCfg;
use crate::types::*;
use crate::utils::init_rate_limiter;
use anyhow::{anyhow, Result};
use rust_decimal::Decimal;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{info, warn};

// Helper function (moved from exec.rs)
fn decimal_places(step: Decimal) -> usize {
    if step.is_zero() {
        return 0;
    }
    let s = step.normalize().to_string();
    if let Some(pos) = s.find('.') {
        s[pos + 1..].trim_end_matches('0').len()
    } else {
        0
    }
}

/// Handle API connections (initialize venue)
pub async fn handle_apis(cfg: &AppCfg) -> Result<BinanceFutures> {
    let client = reqwest::Client::builder().build()?;
    let common = BinanceCommon {
        client,
        api_key: cfg.binance.api_key.clone(),
        secret_key: cfg.binance.secret_key.clone(),
        recv_window_ms: cfg.binance.recv_window_ms,
    };

    let price_tick_dec = Decimal::from_f64_retain(cfg.price_tick)
        .ok_or_else(|| anyhow!("Failed to convert price_tick {} to Decimal", cfg.price_tick))?;
    let qty_step_dec = Decimal::from_f64_retain(cfg.qty_step)
        .ok_or_else(|| anyhow!("Failed to convert qty_step {} to Decimal", cfg.qty_step))?;
    let price_precision = decimal_places(price_tick_dec);
    let qty_precision = decimal_places(qty_step_dec);
    
    init_rate_limiter();
    info!("rate limiter initialized for futures");
    
    let hedge_mode = cfg.binance.hedge_mode;
    let venue = BinanceFutures {
        base: cfg.binance.futures_base.clone(),
        common: common.clone(),
        price_tick: price_tick_dec,
        qty_step: qty_step_dec,
        price_precision,
        qty_precision,
        hedge_mode,
    };
    
    // Set position side mode
    if let Err(err) = venue.set_position_side_dual(hedge_mode).await {
        warn!(hedge_mode, error = %err, "failed to set position side mode, continuing anyway");
    } else {
        info!(hedge_mode, "position side mode set successfully");
    }

    info!("API connection established");
    Ok(venue)
}

/// Handle WebSocket connection
pub async fn handle_ws_connection(
    cfg: &AppCfg,
    event_tx: mpsc::UnboundedSender<UserEvent>,
) {
    let client = reqwest::Client::builder().build().unwrap();
    let api_key = cfg.binance.api_key.clone();
    let futures_base = cfg.binance.futures_base.clone();
    let reconnect_delay = Duration::from_millis(cfg.websocket.reconnect_delay_ms);
    let kind = UserStreamKind::Futures;
    
    info!(
        reconnect_delay_ms = cfg.websocket.reconnect_delay_ms,
        ping_interval_ms = cfg.websocket.ping_interval_ms,
        ?kind,
        "launching WebSocket connection"
    );
    
    tokio::spawn(async move {
        loop {
            match UserDataStream::connect(client.clone(), &futures_base, &api_key, kind).await {
                Ok(mut stream) => {
                    info!(?kind, "WebSocket connected");
                    let tx_sync = event_tx.clone();
                    stream.set_on_reconnect(move || {
                        let _ = tx_sync.send(UserEvent::Heartbeat);
                        info!("WebSocket reconnect callback triggered");
                    });
                    
                    let mut first_event_after_reconnect = true;
                    loop {
                        match stream.next_event().await {
                            Ok(event) => {
                                if first_event_after_reconnect {
                                    first_event_after_reconnect = false;
                                    let _ = event_tx.send(UserEvent::Heartbeat);
                                }
                                if event_tx.send(event).is_err() {
                                    break;
                                }
                            }
                            Err(_) => {
                                break;
                            }
                        }
                    }
                    warn!("WebSocket connection lost, will reconnect");
                }
                Err(err) => {
                    warn!(?err, "failed to connect WebSocket");
                }
            }
            tokio::time::sleep(reconnect_delay).await;
        }
    });
}

/// Close WebSocket connection (graceful shutdown)
pub async fn close_ws_connection() {
    info!("closing WebSocket connection");
    // WebSocket is handled by tokio::spawn, so we just log
    // In a real implementation, you might want to track the handle and cancel it
}

