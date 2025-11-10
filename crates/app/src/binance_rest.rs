//location: /crates/data/src/binance_rest.rs
use anyhow::Result;
use reqwest::Client;

pub async fn get_time(client: &Client, base: &str) -> Result<String> {
    let url = format!("{}/api/v3/time", base);
    let s = client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    Ok(s)
}
