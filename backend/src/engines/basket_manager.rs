use crate::models::{Basket, BasketConfig, BasketSide, BasketStatus};
use dashmap::DashMap;
use std::sync::Arc;
use uuid::Uuid;

pub struct BasketManager {
    pub baskets: Arc<DashMap<Uuid, Basket>>,
    pub order_to_basket: Arc<DashMap<Uuid, Uuid>>,
    pub config: BasketConfig,
}

impl BasketManager {
    pub fn new(config: BasketConfig, is_inverse: bool, tp_spread: f64) -> Self {
        let baskets = Arc::new(DashMap::new());
        // Split half long, half short. Odd count → one extra long.
        let n = config.num_baskets;
        let long_count = (n + 1) / 2;
        for i in 0..n {
            let side = if i < long_count {
                BasketSide::Long
            } else {
                BasketSide::Short
            };
            let b = Basket::new(i, side, config.basket_size_qty, is_inverse, tp_spread);
            baskets.insert(b.basket_id, b);
        }
        Self {
            baskets,
            order_to_basket: Arc::new(DashMap::new()),
            config,
        }
    }

    pub fn all(&self) -> Vec<Basket> {
        let mut v: Vec<Basket> = self.baskets.iter().map(|e| e.value().clone()).collect();
        v.sort_by_key(|b| b.index);
        v
    }

    pub fn active_count(&self) -> usize {
        self.baskets
            .iter()
            .filter(|e| e.value().status != BasketStatus::Killed)
            .count()
    }

    pub fn killed_count(&self) -> usize {
        self.baskets
            .iter()
            .filter(|e| e.value().status == BasketStatus::Killed)
            .count()
    }

    /// Find the first non-killed basket with capacity for new qty.
    pub fn find_basket_with_capacity(&self, qty: f64) -> Option<Uuid> {
        let mut candidates: Vec<(u32, Uuid)> = self
            .baskets
            .iter()
            .filter_map(|e| {
                let b = e.value();
                if b.has_capacity(qty) {
                    Some((b.index, b.basket_id))
                } else {
                    None
                }
            })
            .collect();
        candidates.sort_by_key(|(i, _)| *i);
        candidates.first().map(|(_, id)| *id)
    }

    /// Find the first non-killed basket of the requested side with capacity.
    pub fn find_basket_with_capacity_by_side(
        &self,
        side: BasketSide,
        qty: f64,
    ) -> Option<Uuid> {
        let mut candidates: Vec<(u32, Uuid)> = self
            .baskets
            .iter()
            .filter_map(|e| {
                let b = e.value();
                if b.side == side && b.has_capacity(qty) {
                    Some((b.index, b.basket_id))
                } else {
                    None
                }
            })
            .collect();
        candidates.sort_by_key(|(i, _)| *i);
        candidates.first().map(|(_, id)| *id)
    }

    pub fn link_order(&self, order_id: Uuid, basket_id: Uuid) {
        self.order_to_basket.insert(order_id, basket_id);
    }

    pub fn basket_for_order(&self, order_id: Uuid) -> Option<Uuid> {
        self.order_to_basket.get(&order_id).map(|e| *e.value())
    }

    pub fn total_open_qty(&self) -> f64 {
        self.baskets.iter().map(|e| e.value().open_qty).sum()
    }

    pub fn total_realized_pnl(&self) -> f64 {
        self.baskets.iter().map(|e| e.value().realized_pnl).sum()
    }

    pub fn all_killed(&self) -> bool {
        !self.baskets.is_empty()
            && self
                .baskets
                .iter()
                .all(|e| e.value().status == BasketStatus::Killed)
    }
}
