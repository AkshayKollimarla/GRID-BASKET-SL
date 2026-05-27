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

/// Direction the basket trades. Long baskets buy below mid and TP-sell above
/// (positive PnL when exit > entry). Short baskets sell above mid and TP-buy
/// below (positive PnL when exit < entry).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum BasketSide {
    Long,
    Short,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Basket {
    pub basket_id: Uuid,
    pub index: u32,
    pub side: BasketSide,
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
    /// True for INVERSE contracts (Deribit BTC-PERPETUAL etc.) where the
    /// order amount is in QUOTE currency (USD) and PnL formula must divide
    /// by entry price to convert to base-currency PnL × price.
    /// False for LINEAR contracts (Hyperliquid, mock) where amount is in
    /// BASE currency directly.
    #[serde(default)]
    pub is_inverse: bool,
}

impl Basket {
    pub fn new(
        index: u32,
        side: BasketSide,
        max_qty: f64,
        sl_distance: f64,
        is_inverse: bool,
    ) -> Self {
        Self {
            basket_id: Uuid::new_v4(),
            index,
            side,
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
            is_inverse,
        }
    }

    /// Convert a (exit - entry) × qty product into USD-denominated PnL,
    /// applying the inverse-contract divisor when needed.
    /// `entry_ref` is the entry price used as the divisor (usually avg_price).
    fn pnl_usd(&self, price_diff: f64, qty: f64, entry_ref: f64) -> f64 {
        let raw = price_diff * qty;
        if self.is_inverse && entry_ref > 0.0 {
            raw / entry_ref
        } else {
            raw
        }
    }

    pub fn has_capacity(&self, qty: f64) -> bool {
        self.status != BasketStatus::Killed && (self.open_qty + qty) <= self.max_qty
    }

    /// Recompute the per-side SL price after a fill changes avg_price.
    /// Long: SL fires when price falls below avg - sl_distance.
    /// Short: SL fires when price rises above avg + sl_distance.
    fn recompute_sl(&mut self) {
        self.sl_price = Some(match self.side {
            BasketSide::Long => self.avg_price - self.sl_distance,
            BasketSide::Short => self.avg_price + self.sl_distance,
        });
    }

    pub fn apply_entry_fill(&mut self, fill_qty: f64, fill_price: f64) {
        // Weighted average price update (same formula for both sides — open_qty
        // is always tracked as positive magnitude).
        let prev_notional = self.open_qty * self.avg_price;
        let new_notional = fill_qty * fill_price;
        let new_qty = self.open_qty + fill_qty;
        if new_qty > 0.0 {
            self.avg_price = (prev_notional + new_notional) / new_qty;
        }
        self.open_qty = new_qty;
        self.fills_count += 1;
        self.recompute_sl();
        self.status = BasketStatus::Active;
    }

    pub fn apply_tp_fill(&mut self, tp_qty: f64, tp_price: f64) {
        // PnL sign depends on basket side; magnitude depends on inverse flag.
        let diff = match self.side {
            BasketSide::Long => tp_price - self.avg_price,   // long profits on up moves
            BasketSide::Short => self.avg_price - tp_price,  // short profits on down moves
        };
        let pnl = self.pnl_usd(diff, tp_qty, self.avg_price);
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
            let diff = match self.side {
                BasketSide::Long => exit_price - self.avg_price,
                BasketSide::Short => self.avg_price - exit_price,
            };
            let pnl = self.pnl_usd(diff, self.open_qty, self.avg_price);
            self.realized_pnl += pnl;
        }
        self.open_qty = 0.0;
        self.status = BasketStatus::Killed;
    }

    /// Soft cycle reset: close out at exit_price (booking PnL into
    /// realized_pnl), then return the basket to Idle so the next cycle can
    /// trade it again. Unlike `kill`, this does NOT set the basket to Killed.
    /// realized_pnl, fills_count, tp_count are preserved (cumulative).
    pub fn soft_reset(&mut self, exit_price: f64) {
        if self.open_qty > 0.0 {
            let diff = match self.side {
                BasketSide::Long => exit_price - self.avg_price,
                BasketSide::Short => self.avg_price - exit_price,
            };
            let pnl = self.pnl_usd(diff, self.open_qty, self.avg_price);
            self.realized_pnl += pnl;
        }
        self.open_qty = 0.0;
        self.avg_price = 0.0;
        self.sl_price = None;
        self.status = BasketStatus::Idle;
    }
}
