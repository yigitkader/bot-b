use anyhow::Result;
use async_trait::async_trait;
use bot_core::types::*;

#[derive(Clone, Debug)]
pub struct VenueOrder {
    pub order_id: String,
    pub side: Side,
    pub price: Px,
    pub qty: Qty,
}

pub mod binance;

#[async_trait]
pub trait Venue: Send + Sync {
    async fn place_limit(
        &self,
        sym: &str,
        side: Side,
        px: Px,
        qty: Qty,
        tif: Tif,
    ) -> Result<String>;
    async fn cancel(&self, order_id: &str, sym: &str) -> Result<()>;
    async fn best_prices(&self, sym: &str) -> Result<(Px, Px)>;
    async fn get_open_orders(&self, sym: &str) -> Result<Vec<VenueOrder>>;
    async fn get_position(&self, sym: &str) -> Result<Position>;
    async fn mark_price(&self, sym: &str) -> Result<Px>;
    async fn close_position(&self, sym: &str) -> Result<()>;

    async fn cancel_all(&self, sym: &str) -> Result<()> {
        let orders = self.get_open_orders(sym).await?;
        for order in orders {
            self.cancel(&order.order_id, sym).await?;
        }
        Ok(())
    }
}
