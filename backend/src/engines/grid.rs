use crate::engines::basket_manager::BasketManager;
use crate::exchanges::Exchange;
use crate::models::{AgentConfig, BasketSide, BasketStatus, OrderPurpose, Side};
use std::collections::HashSet;
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

    /// Round a price down to the nearest grid_step multiple.
    fn floor_to_step(&self, p: f64) -> f64 {
        let step = self.config.trading.grid_step;
        (p / step).floor() * step
    }

    /// Round a price up to the nearest grid_step multiple.
    fn ceil_to_step(&self, p: f64) -> f64 {
        let step = self.config.trading.grid_step;
        (p / step).ceil() * step
    }

    /// Run one pass of the trailing-depth grid engine.
    ///
    /// `absolute_lower` / `absolute_upper` are the hard envelope (computed
    /// once at engine start from `grid_distance`). The grid will never place
    /// an entry outside those bounds.
    pub async fn step(&self, absolute_lower: f64, absolute_upper: f64) {
        if self.basket_mgr.all_killed() {
            return;
        }

        let book = self.exchange.orderbook().await;
        let mid = book.mid;
        if mid <= 0.0 {
            return;
        }

        let t = &self.config.trading;
        let step = t.grid_step;
        if step <= 0.0 {
            return;
        }
        let depth = t.grid_depth.max(1) as usize;
        let lower = absolute_lower;
        let upper = absolute_upper;

        // Compute target level sets (N closest on each side, snapped to step).
        let mut target_buys: Vec<f64> = Vec::with_capacity(depth);
        let mut p_buy = self.floor_to_step(mid);
        // If mid lands exactly on a step, the floor IS mid itself — we want strictly below.
        if (p_buy - mid).abs() < 1e-9 {
            p_buy -= step;
        }
        for _ in 0..depth {
            if p_buy < lower || p_buy >= mid {
                break;
            }
            target_buys.push(p_buy);
            p_buy -= step;
        }

        let mut target_sells: Vec<f64> = Vec::with_capacity(depth);
        let mut p_sell = self.ceil_to_step(mid);
        if (p_sell - mid).abs() < 1e-9 {
            p_sell += step;
        }
        for _ in 0..depth {
            if p_sell > upper || p_sell <= mid {
                break;
            }
            target_sells.push(p_sell);
            p_sell += step;
        }

        // Key helper (price → integer cents — works for any market with ≥$0.01 tick).
        let key = |p: f64| (p * 100.0).round() as i64;
        let target_buy_set: HashSet<i64> = target_buys.iter().map(|p| key(*p)).collect();
        let target_sell_set: HashSet<i64> = target_sells.iter().map(|p| key(*p)).collect();

        let open = self.exchange.open_orders().await;

        // 3. Cancel ENTRY orders that drifted out of the target sets.
        for o in open.iter() {
            if !matches!(o.purpose, OrderPurpose::Entry) {
                continue; // leave TPs alone — they're at basket avg ± tp_spread
            }
            let in_target = match o.side {
                Side::Buy => target_buy_set.contains(&key(o.price)),
                Side::Sell => target_sell_set.contains(&key(o.price)),
            };
            if !in_target {
                let _ = self.exchange.cancel(o.order_id).await;
                debug!(price = o.price, side = ?o.side, "cancelled entry drifted from grid");
            }
        }

        // Exposure cap check.
        let total_open = self.basket_mgr.total_open_qty();
        if total_open >= self.config.kill_switch.max_position_cap {
            debug!(total_open, "exposure cap reached, skipping new placements");
            return;
        }

        // 4. Refresh the "what's resting now" set (any side, any purpose) AFTER
        //    cancellations are scheduled — note cancels are best-effort and not
        //    instantly reflected on every exchange, so we re-fetch.
        let open_after = self.exchange.open_orders().await;
        let resting: HashSet<(Side, i64)> = open_after
            .iter()
            .map(|o| (o.side, key(o.price)))
            .collect();

        let qty = t.per_step_qty;

        // 4a. Place missing BUY entries (target levels below mid).
        for bp in &target_buys {
            if resting.contains(&(Side::Buy, key(*bp))) {
                continue;
            }
            let basket_id = match self
                .basket_mgr
                .find_basket_with_capacity_by_side(BasketSide::Long, qty)
            {
                Some(id) => id,
                None => break, // no Long basket has room
            };
            // SL danger-zone guard (only for active baskets that already have a position).
            if let Some(b) = self.basket_mgr.baskets.get(&basket_id) {
                if b.status == BasketStatus::Active {
                    if let Some(sl) = b.sl_price {
                        if *bp <= sl + self.config.basket.basket_sl_distance * 0.2 {
                            continue;
                        }
                    }
                }
            }
            match self
                .exchange
                .place_maker_only(Side::Buy, *bp, qty, basket_id, OrderPurpose::Entry)
                .await
            {
                Ok(order) => self.basket_mgr.link_order(order.order_id, basket_id),
                Err(e) => warn!(?e, price = bp, "BUY entry failed"),
            }
        }

        // 4b. Place missing SELL entries (target levels above mid).
        for sp in &target_sells {
            if resting.contains(&(Side::Sell, key(*sp))) {
                continue;
            }
            let basket_id = match self
                .basket_mgr
                .find_basket_with_capacity_by_side(BasketSide::Short, qty)
            {
                Some(id) => id,
                None => break,
            };
            if let Some(b) = self.basket_mgr.baskets.get(&basket_id) {
                if b.status == BasketStatus::Active {
                    if let Some(sl) = b.sl_price {
                        if *sp >= sl - self.config.basket.basket_sl_distance * 0.2 {
                            continue;
                        }
                    }
                }
            }
            match self
                .exchange
                .place_maker_only(Side::Sell, *sp, qty, basket_id, OrderPurpose::Entry)
                .await
            {
                Ok(order) => self.basket_mgr.link_order(order.order_id, basket_id),
                Err(e) => warn!(?e, price = sp, "SELL entry failed"),
            }
        }
    }
}
