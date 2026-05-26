use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum BasketStatus {
    Idle,
    Active,
    TpRecycling,
    Killed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Basket {
    pub basket_id: Uuid,
    pub index: u32,
    pub max_qty: f64,
    pub open_qty: f64,
    pub avg_price: f64,
    pub sl_distance: f64,
    pub sl_price: Option<f64>,
    pub status: BasketStatus,
    pub realized_pnl: f64,
    pub unrealized_pnl: f64,
    pub fills_count: u32,
    pub tp_count: u32,
}

impl Basket {
    pub fn new(index: u32, max_qty: f64, sl_distance: f64) -> Self {
        Self {
            basket_id: Uuid::new_v4(),
            index,
            max_qty,
            open_qty: 0.0,
            avg_price: 0.0,
            sl_distance,
            sl_price: None,
            status: BasketStatus::Idle,
            realized_pnl: 0.0,
            unrealized_pnl: 0.0,
            fills_count: 0,
            tp_count: 0,
        }
    }

    pub fn has_capacity(&self, qty: f64) -> bool {
        self.status != BasketStatus::Killed && (self.open_qty + qty) <= self.max_qty
    }

    pub fn apply_entry_fill(&mut self, fill_qty: f64, fill_price: f64) {
        // Weighted average price update
        let prev_notional = self.open_qty * self.avg_price;
        let new_notional = fill_qty * fill_price;
        let new_qty = self.open_qty + fill_qty;
        if new_qty > 0.0 {
            self.avg_price = (prev_notional + new_notional) / new_qty;
        }
        self.open_qty = new_qty;
        self.fills_count += 1;
        // SL = avg_price - sl_distance (for longs)
        self.sl_price = Some(self.avg_price - self.sl_distance);
        self.status = BasketStatus::Active;
    }

    pub fn apply_tp_fill(&mut self, tp_qty: f64, tp_price: f64) {
        let pnl = (tp_price - self.avg_price) * tp_qty;
        self.realized_pnl += pnl;
        self.open_qty = (self.open_qty - tp_qty).max(0.0);
        self.tp_count += 1;
        self.status = if self.open_qty > 0.0 {
            BasketStatus::TpRecycling
        } else {
            BasketStatus::Idle
        };
    }

    pub fn kill(&mut self, exit_price: f64) {
        if self.open_qty > 0.0 {
            let pnl = (exit_price - self.avg_price) * self.open_qty;
            self.realized_pnl += pnl;
        }
        self.open_qty = 0.0;
        self.status = BasketStatus::Killed;
    }
}
