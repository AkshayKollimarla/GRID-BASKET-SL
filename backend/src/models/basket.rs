use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum BasketStatus {
    Idle,
    Active,
    TpRecycling,
    /// Cycle SL just hit this basket — it was flattened. Visually displayed
    /// as "KILLED" in the UI, but the basket is still tradeable: the next
    /// entry fill will flip status back to Active. Use this for the
    /// transient "I just got SL'd" indicator.
    Hit,
    /// Permanent kill. Caused by the kill switch (max_basket_hits reached or
    /// manual KILL button). Basket never trades again until manual Reset.
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
    pub status: BasketStatus,
    pub realized_pnl: f64,
    pub unrealized_pnl: f64,
    pub fills_count: u32,
    pub tp_count: u32,
    /// True for INVERSE contracts (Deribit BTC-PERPETUAL etc.).
    #[serde(default)]
    pub is_inverse: bool,
    /// TP spread (= profit per round-trip). Stored on the basket so
    /// apply_tp_fill can compute the per-fill PnL without needing the
    /// engine config passed in.
    #[serde(default)]
    pub tp_spread: f64,
    /// PER-BASKET SL — set on the basket's FIRST entry fill. The anchor is
    /// the fill price at activation; the basket's SL fires when mid
    /// crosses anchor ± grid_distance. 0.0 = not yet activated.
    /// Each basket is independent: when its own SL hits, only THIS basket
    /// is killed; the other baskets continue trading.
    #[serde(default)]
    pub anchor_price: f64,
    #[serde(default)]
    pub upper_sl: f64,
    #[serde(default)]
    pub lower_sl: f64,
}

impl Basket {
    pub fn new(
        index: u32,
        side: BasketSide,
        max_qty: f64,
        is_inverse: bool,
        tp_spread: f64,
    ) -> Self {
        Self {
            basket_id: Uuid::new_v4(),
            index,
            side,
            max_qty,
            open_qty: 0.0,
            avg_price: 0.0,
            status: BasketStatus::Idle,
            realized_pnl: 0.0,
            unrealized_pnl: 0.0,
            fills_count: 0,
            tp_count: 0,
            is_inverse,
            tp_spread,
            anchor_price: 0.0,
            upper_sl: 0.0,
            lower_sl: 0.0,
        }
    }

    /// Set this basket's per-basket SL anchor and bounds. Called once,
    /// when the basket's first entry fill arrives. Subsequent fills do
    /// not move the anchor — it stays at the activation price.
    pub fn set_sl_anchor(&mut self, anchor: f64, distance: f64) {
        if self.anchor_price > 0.0 {
            return; // already set — do not overwrite
        }
        self.anchor_price = anchor;
        self.upper_sl = anchor + distance.max(0.0);
        self.lower_sl = anchor - distance.max(0.0);
    }

    /// True when this basket has open positions AND mid has crossed its
    /// own SL boundary → caller should flatten + permanently KILL it.
    pub fn sl_breached(&self, mid: f64) -> bool {
        self.anchor_price > 0.0
            && self.open_qty > 0.0
            && self.status != BasketStatus::Killed
            && (mid <= self.lower_sl || mid >= self.upper_sl)
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
        self.status = BasketStatus::Active;
    }

    pub fn apply_tp_fill(&mut self, tp_qty: f64, _tp_price: f64) {
        // PnL per TP fill is FIXED = tp_spread × qty (sign always positive
        // because we placed the TP exactly at tp_spread above/below entry).
        // We ignore tp_price here — it's just the fill price, which equals
        // the limit price for a post-only maker order.
        //
        // For INVERSE contracts (Deribit), divide by avg to get USD PnL.
        let pnl = self.pnl_usd(self.tp_spread, tp_qty, self.avg_price);
        self.realized_pnl += pnl;
        self.open_qty = (self.open_qty - tp_qty).max(0.0);
        self.tp_count += 1;
        if self.open_qty > 1e-9 {
            self.status = BasketStatus::TpRecycling;
        } else {
            // Basket fully closed — reset the per-basket SL anchor so the
            // NEXT entry fill re-anchors at fresh price. Without this the
            // basket would re-activate with a stale SL range (set when it
            // first activated), potentially born already inside its old
            // SL bounds → instant kill on the next fill.
            self.status = BasketStatus::Idle;
            self.anchor_price = 0.0;
            self.upper_sl = 0.0;
            self.lower_sl = 0.0;
            self.avg_price = 0.0;
        }
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
    /// realized_pnl), then mark the basket as Hit. The UI displays Hit as
    /// "KILLED" so you can see which baskets just got cycle-SL'd, but the
    /// basket is still tradeable — the next entry fill will return it to
    /// Active. realized_pnl, fills_count, tp_count are preserved (cumulative).
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
        self.status = BasketStatus::Hit;
    }
}
