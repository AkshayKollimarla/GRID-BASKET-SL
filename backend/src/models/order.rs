use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "UPPERCASE")]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum OrderType {
    LimitMaker, // Binance LIMIT_MAKER / Deribit post_only / Hyperliquid ALO
    MarketReduceOnly,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum OrderStatus {
    Pending,
    Open,
    Filled,
    PartiallyFilled,
    Cancelled,
    Rejected,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OrderPurpose {
    Entry,
    TakeProfit,
    StopLossExit,
    KillSwitchExit,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Order {
    pub order_id: Uuid,
    pub basket_id: Uuid,
    pub side: Side,
    pub order_type: OrderType,
    pub purpose: OrderPurpose,
    pub price: f64,
    pub qty: f64,
    pub filled_qty: f64,
    pub status: OrderStatus,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBookLevel {
    pub price: f64,
    pub size: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBook {
    pub bids: Vec<OrderBookLevel>,
    pub asks: Vec<OrderBookLevel>,
    pub mid: f64,
    pub timestamp: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fill {
    pub fill_id: Uuid,
    pub order_id: Uuid,
    pub basket_id: Uuid,
    pub purpose: OrderPurpose,
    pub side: Side,
    pub price: f64,
    pub qty: f64,
    /// Exchange fee paid in quote currency (USD-ish). Always non-negative.
    /// 0.0 if the exchange didn't return fee data (e.g. immediate market exits).
    #[serde(default)]
    pub fee: f64,
    pub timestamp: i64,
}
