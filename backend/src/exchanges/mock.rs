use super::Exchange;
use crate::models::{
    Fill, Order, OrderBook, OrderBookLevel, OrderPurpose, OrderStatus, OrderType, Side,
};
use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use dashmap::DashMap;
use parking_lot::RwLock;
use rand::Rng;
use std::sync::Arc;
use tokio::sync::broadcast;
use uuid::Uuid;

pub struct MockExchange {
    pub mid_price: Arc<RwLock<f64>>,
    pub orders: Arc<DashMap<Uuid, Order>>,
    pub fills_tx: broadcast::Sender<Fill>,
}

impl MockExchange {
    pub fn new(initial_price: f64) -> (Arc<Self>, broadcast::Receiver<Fill>) {
        let (tx, rx) = broadcast::channel(1024);
        let me = Arc::new(Self {
            mid_price: Arc::new(RwLock::new(initial_price)),
            orders: Arc::new(DashMap::new()),
            fills_tx: tx,
        });
        (me, rx)
    }

    pub fn current_price(&self) -> f64 {
        *self.mid_price.read()
    }

    /// Random walk + occasional spikes — simulates a live market.
    fn advance_price(&self) {
        let mut p = self.mid_price.write();
        let mut rng = rand::thread_rng();
        let drift: f64 = rng.gen_range(-15.0..15.0);
        let spike: f64 = if rng.gen_bool(0.02) {
            rng.gen_range(-200.0..200.0)
        } else {
            0.0
        };
        *p = (*p + drift + spike).max(1.0);
    }
}

#[async_trait]
impl Exchange for MockExchange {
    async fn name(&self) -> &'static str {
        "MockExchange"
    }

    async fn orderbook(&self) -> OrderBook {
        let mid = *self.mid_price.read();
        // Build a simple synthetic book around mid.
        let bids = (1..=10)
            .map(|i| OrderBookLevel {
                price: mid - i as f64 * 5.0,
                size: 0.5 + (i as f64) * 0.1,
            })
            .collect();
        let asks = (1..=10)
            .map(|i| OrderBookLevel {
                price: mid + i as f64 * 5.0,
                size: 0.5 + (i as f64) * 0.1,
            })
            .collect();
        OrderBook {
            bids,
            asks,
            mid,
            timestamp: Utc::now().timestamp_millis(),
        }
    }

    async fn place_maker_only(
        &self,
        side: Side,
        price: f64,
        qty: f64,
        basket_id: Uuid,
        purpose: OrderPurpose,
    ) -> Result<Order> {
        let mid = *self.mid_price.read();
        // Maker-only: reject if it would cross.
        let crosses = match side {
            Side::Buy => price >= mid,
            Side::Sell => price <= mid,
        };
        let status = if crosses {
            OrderStatus::Rejected
        } else {
            OrderStatus::Open
        };
        let order = Order {
            order_id: Uuid::new_v4(),
            basket_id,
            side,
            order_type: OrderType::LimitMaker,
            purpose,
            price,
            qty,
            filled_qty: 0.0,
            status,
            created_at: Utc::now().timestamp_millis(),
        };
        if status == OrderStatus::Open {
            self.orders.insert(order.order_id, order.clone());
        }
        Ok(order)
    }

    async fn place_market_reduce_only(
        &self,
        side: Side,
        qty: f64,
        basket_id: Uuid,
        purpose: OrderPurpose,
    ) -> Result<Order> {
        let mid = *self.mid_price.read();
        // Simulate small slippage (1–5 bps).
        let mut rng = rand::thread_rng();
        let slip_bps: f64 = rng.gen_range(1.0..5.0);
        let fill_price = match side {
            Side::Buy => mid * (1.0 + slip_bps / 10_000.0),
            Side::Sell => mid * (1.0 - slip_bps / 10_000.0),
        };
        let order = Order {
            order_id: Uuid::new_v4(),
            basket_id,
            side,
            order_type: OrderType::MarketReduceOnly,
            purpose,
            price: fill_price,
            qty,
            filled_qty: qty,
            status: OrderStatus::Filled,
            created_at: Utc::now().timestamp_millis(),
        };
        let fill = Fill {
            fill_id: Uuid::new_v4(),
            order_id: order.order_id,
            basket_id,
            purpose,
            side,
            price: fill_price,
            qty,
            timestamp: Utc::now().timestamp_millis(),
        };
        let _ = self.fills_tx.send(fill);
        Ok(order)
    }

    async fn cancel(&self, order_id: Uuid) -> Result<()> {
        if let Some(mut o) = self.orders.get_mut(&order_id) {
            o.status = OrderStatus::Cancelled;
        }
        self.orders.remove(&order_id);
        Ok(())
    }

    async fn cancel_all(&self) -> Result<()> {
        self.orders.clear();
        Ok(())
    }

    async fn open_orders(&self) -> Vec<Order> {
        self.orders.iter().map(|e| e.value().clone()).collect()
    }

    /// Advance the simulator: move price, then check which resting maker orders fill.
    async fn tick(&self) {
        self.advance_price();
        let mid = *self.mid_price.read();
        let mut to_fill: Vec<Order> = Vec::new();
        for entry in self.orders.iter() {
            let o = entry.value();
            let crossed = match o.side {
                Side::Buy => mid <= o.price,  // buy fills when price drops to bid
                Side::Sell => mid >= o.price, // sell fills when price rises to ask
            };
            if crossed && o.status == OrderStatus::Open {
                to_fill.push(o.clone());
            }
        }
        for mut o in to_fill {
            o.filled_qty = o.qty;
            o.status = OrderStatus::Filled;
            self.orders.remove(&o.order_id);
            let fill = Fill {
                fill_id: Uuid::new_v4(),
                order_id: o.order_id,
                basket_id: o.basket_id,
                purpose: o.purpose,
                side: o.side,
                price: o.price,
                qty: o.qty,
                timestamp: Utc::now().timestamp_millis(),
            };
            let _ = self.fills_tx.send(fill);
        }
    }
}
