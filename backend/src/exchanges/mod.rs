use crate::models::{Order, OrderBook, Side};
use anyhow::Result;
use async_trait::async_trait;
use uuid::Uuid;

pub mod deribit;
pub mod hyperliquid;
pub mod instruments;
pub mod mock;
pub use deribit::DeribitClient;
pub use hyperliquid::HyperliquidClient;
pub use mock::MockExchange;

#[async_trait]
pub trait Exchange: Send + Sync {
    async fn name(&self) -> &'static str;
    async fn orderbook(&self) -> OrderBook;
    async fn place_maker_only(
        &self,
        side: Side,
        price: f64,
        qty: f64,
        basket_id: Uuid,
        purpose: crate::models::OrderPurpose,
    ) -> Result<Order>;
    async fn place_market_reduce_only(
        &self,
        side: Side,
        qty: f64,
        basket_id: Uuid,
        purpose: crate::models::OrderPurpose,
    ) -> Result<Order>;
    async fn cancel(&self, order_id: Uuid) -> Result<()>;
    async fn cancel_all(&self) -> Result<()>;
    async fn open_orders(&self) -> Vec<Order>;
    async fn tick(&self);

    /// Signed net position on the instrument as the exchange knows it.
    /// Positive = long, negative = short, units are whatever the exchange
    /// uses for `amount` (USD for Deribit inverse, BASE coin for linear).
    /// Returns 0.0 if the exchange doesn't support position queries or
    /// the fetch fails — never errors, so callers can poll cheaply.
    async fn position(&self) -> f64 {
        0.0
    }
}
