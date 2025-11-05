use anyhow::Result;
use futures_util::StreamExt;
use tokio_tungstenite::connect_async;
use tracing::info;

pub async fn run_book_stream(symbol: &str, depth: u32) -> Result<()> {
    let url = format!("wss://fstream.binance.com/stream?streams={}@depth{}@100ms", symbol, depth);
    let (ws, _) = connect_async(url).await?;
    let (_write, mut read) = ws.split();
    info!(%symbol, %depth, "connected to binance depth stream");

    while let Some(msg) = read.next().await {
        let txt = msg?.into_text()?;
        if txt.len() < 256 { info!(event = %txt, "depth event"); }
    }
    Ok(())
}
