use std::{env, fs, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use log::{info, warn};
use tokio::time::{sleep, timeout};
use trading_bot::{config::BotConfig, Connection, EventBus};

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    let args: Vec<String> = env::args().collect();
    let samples: usize = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(30);
    let output = args
        .get(2)
        .map(|s| s.as_str())
        .unwrap_or("tests/data/binance_btcusdt_ticks.json");

    let config = BotConfig::from_env();
    info!(
        "RECORDER: capturing {} samples for symbol {} -> {}",
        samples, config.symbol, output
    );
    record_ticks(config, samples, output).await?;
    info!("RECORDER: completed, wrote {}", output);
    Ok(())
}

pub async fn record_ticks(config: BotConfig, samples: usize, output: &str) -> Result<()> {
    let connection = Arc::new(Connection::new(config.clone()));
    let bus = EventBus::new(4096);

    let market_task = {
        let conn = connection.clone();
        let ch = bus.connection_channels();
        tokio::spawn(async move {
            if let Err(err) = conn.run_market_ws(ch).await {
                warn!("RECORDER: market ws task failed: {err:?}");
            }
        })
    };

    // Wait a bit for connection to establish
    sleep(Duration::from_secs(2)).await;

    let mut market_rx = bus.trending_channels().market_rx;
    let mut captured = Vec::with_capacity(samples);
    let max_wait_per_tick = Duration::from_secs(10);
    let max_total_time = Duration::from_secs((samples as u64 * 2) + 30);

    let start = std::time::Instant::now();
    
    while captured.len() < samples {
        if start.elapsed() > max_total_time {
            anyhow::bail!(
                "RECORDER: timeout after {}s, only captured {}/{} samples",
                max_total_time.as_secs(),
                captured.len(),
                samples
            );
        }

        match timeout(max_wait_per_tick, market_rx.recv()).await {
            Ok(Ok(tick)) => {
                println!(
                    "RECORDER: sample {}/{} price {:.2} obi {:?} funding {:?}",
                    captured.len() + 1,
                    samples,
                    tick.price,
                    tick.obi,
                    tick.funding_rate
                );
                captured.push(tick);
            }
            Ok(Err(err)) => {
                anyhow::bail!("RECORDER: market stream error: {err}");
            }
            Err(_) => {
                anyhow::bail!(
                    "RECORDER: timeout waiting for tick (captured {}/{})",
                    captured.len(),
                    samples
                );
            }
        }
    }

    market_task.abort();
    let _ = market_task.await;

    if captured.is_empty() {
        anyhow::bail!("RECORDER: no ticks captured");
    }

    let serialized =
        serde_json::to_string_pretty(&captured).context("failed to serialize captured ticks")?;
    fs::write(output, serialized).context("failed to write output file")?;
    println!("RECORDER: wrote {} ticks to {}", captured.len(), output);
    Ok(())
}

