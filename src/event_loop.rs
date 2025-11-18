use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{info, warn};
pub async fn run_event_loop<T, F, Fut>(
    mut receiver: broadcast::Receiver<T>,
    shutdown_flag: Arc<AtomicBool>,
    module_name: &str,
    event_name: &str,
    handler: F,
) where
    T: Clone,
    F: Fn(T) -> Fut,
    Fut: std::future::Future<Output = Result<(), anyhow::Error>>,
{
    info!("{}: Started, listening to {} events", module_name, event_name);
    loop {
        if shutdown_flag.load(Ordering::Relaxed) {
            break;
        }
        match receiver.recv().await {
            Ok(event) => {
                if shutdown_flag.load(Ordering::Relaxed) {
                    break;
                }
                if let Err(e) = handler(event).await {
                    warn!(error = %e, "{}: error handling {}", module_name, event_name);
                }
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                warn!(
                    skipped = skipped,
                    "{}: {} channel lagged, some messages skipped",
                    module_name,
                    event_name
                );
            }
            Err(broadcast::error::RecvError::Closed) => {
                warn!("{}: {} channel closed, all senders dropped", module_name, event_name);
                break;
            }
        }
    }
    info!("{}: Stopped", module_name);
}
pub async fn run_event_loop_async<T, F, Fut>(
    mut receiver: broadcast::Receiver<T>,
    shutdown_flag: Arc<AtomicBool>,
    module_name: &str,
    event_name: &str,
    mut handler: F,
) where
    T: Clone,
    F: FnMut(T) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    info!("{}: Started, listening to {} events", module_name, event_name);
    loop {
        if shutdown_flag.load(Ordering::Relaxed) {
            break;
        }
        match receiver.recv().await {
            Ok(event) => {
                if shutdown_flag.load(Ordering::Relaxed) {
                    break;
                }
                handler(event).await;
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                warn!(
                    skipped = skipped,
                    "{}: {} channel lagged, some messages skipped",
                    module_name,
                    event_name
                );
            }
            Err(broadcast::error::RecvError::Closed) => {
                warn!("{}: {} channel closed, all senders dropped", module_name, event_name);
                break;
            }
        }
    }
    info!("{}: Stopped", module_name);
}
