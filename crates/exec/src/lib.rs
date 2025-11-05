use anyhow::Result;
use async_trait::async_trait;
use bot_core::types::*;

pub mod binance;

#[async_trait]
pub trait Venue: Send + Sync {
    async fn place_limit(&self, sym: &str, side: Side, px: Px, qty: Qty, tif: Tif) -> Result<String>;
    async fn cancel(&self, order_id: &str, sym: &str) -> Result<()>;
    async fn best_prices(&self, sym: &str) -> Result<(Px, Px)>;
}
