use crate::engines::basket_manager::BasketManager;
use crate::exchanges::Exchange;
use crate::models::{AgentConfig, BasketStatus, OrderPurpose, Side};
use std::sync::Arc;
use tracing::{debug, warn};

pub struct GridEngine {
    pub config: AgentConfig,
    pub basket_mgr: Arc<BasketManager>,
    pub exchange: Arc<dyn Exchange>,
}

impl GridEngine {
    pub fn new(
        config: AgentConfig,
        basket_mgr: Arc<BasketManager>,
        exchange: Arc<dyn Exchange>,
    ) -> Self {
        Self {
            config,
            basket_mgr,
            exchange,
        }
    }

    /// Build the list of grid prices between lower and upper bounds.
    pub fn grid_prices(&self) -> Vec<f64> {
        let t = &self.config.trading;
        let mut prices = Vec::new();
        let mut p = t.grid_lower;
        while p <= t.grid_upper {
            prices.push(p);
            p += t.grid_step;
        }
        prices
    }

    /// Run one pass of the grid engine.
    /// Steps:
    /// 1. Check active baskets exist
    /// 2. For each grid level below mid, place a maker buy if a basket has capacity
    /// 3. Skip if SL danger zone or exposure cap reached
    pub async fn step(&self) {
        if self.basket_mgr.all_killed() {
            return;
        }

        let book = self.exchange.orderbook().await;
        let mid = book.mid;

        // Exposure cap check
        let total_open = self.basket_mgr.total_open_qty();
        if total_open >= self.config.kill_switch.max_position_cap {
            debug!(total_open, "exposure cap reached, skipping grid");
            return;
        }

        let already_open: std::collections::HashSet<i64> = self
            .exchange
            .open_orders()
            .await
            .iter()
            .map(|o| (o.price * 100.0).round() as i64)
            .collect();

        let qty = self.config.trading.per_step_qty;

        for price in self.grid_prices() {
            // Only place buys below mid (longs).
            if price >= mid {
                continue;
            }
            // Skip if we already have an order resting at this level.
            if already_open.contains(&((price * 100.0).round() as i64)) {
                continue;
            }
            // Find a basket with capacity.
            let basket_id = match self.basket_mgr.find_basket_with_capacity(qty) {
                Some(id) => id,
                None => {
                    debug!("no basket with capacity");
                    break;
                }
            };
            // SL danger-zone check: don't place an entry within sl_distance of basket's SL trigger.
            if let Some(b) = self.basket_mgr.baskets.get(&basket_id) {
                if b.status == BasketStatus::Active {
                    if let Some(sl) = b.sl_price {
                        if price <= sl + self.config.basket.basket_sl_distance * 0.2 {
                            debug!(price, sl, "skipping price near SL danger zone");
                            continue;
                        }
                    }
                }
            }

            match self
                .exchange
                .place_maker_only(Side::Buy, price, qty, basket_id, OrderPurpose::Entry)
                .await
            {
                Ok(order) => {
                    self.basket_mgr.link_order(order.order_id, basket_id);
                    debug!(price, qty, "placed maker entry");
                }
                Err(e) => warn!(?e, "place_maker_only failed"),
            }
        }
    }
}
