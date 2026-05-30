use crate::models::{Basket, BasketConfig, BasketSide, BasketStatus, Side};
use dashmap::DashMap;
use parking_lot::RwLock;
use std::sync::Arc;
use uuid::Uuid;

pub struct BasketManager {
    pub baskets: Arc<DashMap<Uuid, Basket>>,
    pub order_to_basket: Arc<DashMap<Uuid, Uuid>>,
    pub config: BasketConfig,
    /// Sequential basket activation: only ONE basket may trade at a
    /// time. When that basket dies (per-basket SL fires), the next IDLE
    /// basket (lowest index) becomes the active one. None = no basket
    /// has been activated yet (engine just started, no fills).
    pub active_basket_id: Arc<RwLock<Option<Uuid>>>,
}

impl BasketManager {
    pub fn new(config: BasketConfig, is_inverse: bool, tp_spread: f64) -> Self {
        let baskets = Arc::new(DashMap::new());
        // Bidirectional baskets — no LONG/SHORT pre-commitment. The legacy
        // BasketSide enum is kept on each basket for UI display; it's
        // updated as `Long` or `Short` based on the current sign of
        // net_qty on every fill.
        for i in 0..config.num_baskets {
            // legacy side hint is ignored by Basket::new in the
            // bidirectional model; pass Long for placeholder.
            let b = Basket::new(i, BasketSide::Long, config.basket_size_qty, is_inverse, tp_spread);
            baskets.insert(b.basket_id, b);
        }
        Self {
            baskets,
            order_to_basket: Arc::new(DashMap::new()),
            config,
            active_basket_id: Arc::new(RwLock::new(None)),
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

    /// Currently-active basket id, or None if nothing has been activated
    /// yet OR the active basket has been killed and the next hasn't been
    /// promoted yet.
    pub fn active_id(&self) -> Option<Uuid> {
        let cur = *self.active_basket_id.read();
        match cur {
            Some(id) => {
                if let Some(b) = self.baskets.get(&id) {
                    if b.status != BasketStatus::Killed {
                        return Some(id);
                    }
                }
                None
            }
            None => None,
        }
    }

    /// Promote the next available (lowest-index IDLE / non-killed) basket
    /// to ACTIVE. Returns its id, or None if every basket is already
    /// killed → caller should stop trading.
    pub fn activate_next_idle(&self) -> Option<Uuid> {
        let mut candidates: Vec<(u32, Uuid)> = self
            .baskets
            .iter()
            .filter_map(|e| {
                let b = e.value();
                if b.status != BasketStatus::Killed {
                    Some((b.index, b.basket_id))
                } else {
                    None
                }
            })
            .collect();
        candidates.sort_by_key(|(i, _)| *i);
        let pick = candidates.first().map(|(_, id)| *id);
        *self.active_basket_id.write() = pick;
        pick
    }

    /// Find the basket that should receive a new entry of `qty` on
    /// `side`. With bidirectional sequential activation:
    ///   • If there's an active basket and it has capacity for the new
    ///     fill → return it.
    ///   • Otherwise promote the next IDLE basket and return it.
    pub fn find_basket_with_capacity(&self, qty: f64, entry_side: Side) -> Option<Uuid> {
        // 1. Try the currently active basket.
        if let Some(id) = self.active_id() {
            if let Some(b) = self.baskets.get(&id) {
                if b.has_capacity(qty, entry_side) {
                    return Some(id);
                }
            }
        }
        // 2. No active basket (or no capacity) → promote next IDLE.
        if let Some(new_id) = self.activate_next_idle() {
            if let Some(b) = self.baskets.get(&new_id) {
                if b.has_capacity(qty, entry_side) {
                    return Some(new_id);
                }
            }
        }
        None
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
