use crate::models::Side;
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
    /// Permanent kill. Caused by the basket's own SL firing, the kill
    /// switch, or manual kill. Basket never trades again until manual Reset.
    Killed,
}

/// LEGACY enum kept for API/serialization stability. In the bidirectional
/// model each basket trades BOTH long and short; `side` reflects the
/// basket's CURRENT net direction (Long when net_qty > 0, Short when
/// net_qty < 0). It's a display field, not a routing one.
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
    /// CURRENT net direction — Long if `net_qty > 0`, Short if `net_qty < 0`.
    /// Recomputed on every fill; informational only (not used for routing).
    pub side: BasketSide,
    /// Max ABSOLUTE net qty the basket may carry. `|net_qty| + new_entry ≤ max_qty`.
    pub max_qty: f64,
    /// Total open qty as a positive magnitude — equals `|net_qty|`. Kept
    /// for backward compatibility with UI/snapshot consumers.
    pub open_qty: f64,
    /// SIGNED net position. Positive = net long (more buys filled than sells).
    /// Negative = net short. Zero = flat.
    /// Bidirectional bookkeeping: BUY entry fill `+=qty`, SELL entry fill `−=qty`,
    /// SELL TP fill (closes a long) `−=qty`, BUY TP fill (closes a short) `+=qty`.
    #[serde(default)]
    pub net_qty: f64,
    /// Volume-weighted avg price of the position currently held. For
    /// bidirectional baskets we re-derive it on each fill from the side
    /// of the new fill so PnL math at flatten time stays sane.
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
        _legacy_side_hint: BasketSide,
        max_qty: f64,
        is_inverse: bool,
        tp_spread: f64,
    ) -> Self {
        Self {
            basket_id: Uuid::new_v4(),
            index,
            // Side starts as Long (placeholder); real direction is set on
            // first entry fill from the actual fill side.
            side: BasketSide::Long,
            max_qty,
            open_qty: 0.0,
            net_qty: 0.0,
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
            && self.net_qty.abs() > 1e-9
            && self.status != BasketStatus::Killed
            && (mid <= self.lower_sl || mid >= self.upper_sl)
    }

    /// Convert a (exit - entry) × qty product into USD-denominated PnL,
    /// applying the inverse-contract divisor when needed.
    fn pnl_usd(&self, price_diff: f64, qty: f64, entry_ref: f64) -> f64 {
        let raw = price_diff * qty;
        if self.is_inverse && entry_ref > 0.0 {
            raw / entry_ref
        } else {
            raw
        }
    }

    /// Capacity check ahead of placing a new entry. The new fill's signed
    /// contribution is added to `net_qty`; the result's absolute value
    /// must not exceed `max_qty`. This allows a SELL entry to "fit" into
    /// a net-long basket because it reduces the absolute position
    /// (offsetting), and vice-versa.
    pub fn has_capacity(&self, qty: f64, entry_side: Side) -> bool {
        if self.status == BasketStatus::Killed {
            return false;
        }
        let signed = match entry_side {
            Side::Buy => qty,
            Side::Sell => -qty,
        };
        (self.net_qty + signed).abs() <= self.max_qty + 1e-9
    }

    /// Apply an entry fill (BUY or SELL). Updates the signed `net_qty`,
    /// re-derives `open_qty = |net_qty|` and the display `side`, updates
    /// the volume-weighted `avg_price` of the position currently held.
    pub fn apply_entry_fill(&mut self, fill_qty: f64, fill_price: f64, fill_side: Side) {
        let signed = match fill_side {
            Side::Buy => fill_qty,
            Side::Sell => -fill_qty,
        };

        // Weighted average price: if the fill is in the same direction as
        // the current net position, average it in. If it offsets, the
        // avg price is preserved (the offsetting portion just closes
        // existing position at the fill price; PnL on that piece is
        // booked into realized_pnl).
        let prev_net = self.net_qty;
        let new_net = prev_net + signed;
        let same_direction = prev_net == 0.0
            || (prev_net > 0.0 && signed > 0.0)
            || (prev_net < 0.0 && signed < 0.0);
        if same_direction {
            // Adding to existing position → weighted average.
            let prev_abs = prev_net.abs();
            let new_abs = new_net.abs();
            if new_abs > 0.0 {
                self.avg_price = (prev_abs * self.avg_price + fill_qty * fill_price) / new_abs;
            }
        } else if prev_net.signum() != new_net.signum() && new_net != 0.0 {
            // The fill flipped the direction (more offset than the prior
            // position). Book PnL on the closed piece, restart avg_price
            // from the leftover side at the new fill price.
            let closed_qty = prev_net.abs();
            let pnl_diff = match fill_side {
                Side::Buy => self.avg_price - fill_price, // closing short
                Side::Sell => fill_price - self.avg_price, // closing long
            };
            self.realized_pnl += self.pnl_usd(pnl_diff, closed_qty, self.avg_price);
            self.avg_price = fill_price;
        } else if new_net == 0.0 {
            // Exactly flat — book PnL on the closed piece, clear avg_price.
            let closed_qty = prev_net.abs();
            let pnl_diff = match fill_side {
                Side::Buy => self.avg_price - fill_price,
                Side::Sell => fill_price - self.avg_price,
            };
            self.realized_pnl += self.pnl_usd(pnl_diff, closed_qty, self.avg_price);
            self.avg_price = 0.0;
        }

        self.net_qty = new_net;
        self.open_qty = new_net.abs();
        self.side = if new_net >= 0.0 {
            BasketSide::Long
        } else {
            BasketSide::Short
        };
        self.fills_count += 1;
        self.status = BasketStatus::Active;
    }

    /// Apply a TP fill (a maker order at `entry_price ± tp_spread`).
    /// `tp_side` is the side of the TP order itself, not of the original
    /// entry: a SELL TP closes a long position; a BUY TP closes a short.
    /// PnL per TP fill is FIXED = `tp_spread × qty` (inverse-adjusted),
    /// because we placed the TP exactly tp_spread away from its entry.
    pub fn apply_tp_fill(&mut self, tp_qty: f64, _tp_price: f64, tp_side: Side) {
        let entry_ref = if self.avg_price > 0.0 {
            self.avg_price
        } else {
            // Fallback for old configs; never zero-divide.
            1.0
        };
        let pnl = self.pnl_usd(self.tp_spread, tp_qty, entry_ref);
        self.realized_pnl += pnl;

        // A SELL TP reduces a long position (net_qty decreases).
        // A BUY TP reduces a short position (net_qty increases).
        let delta = match tp_side {
            Side::Sell => -tp_qty,
            Side::Buy => tp_qty,
        };
        self.net_qty += delta;
        self.open_qty = self.net_qty.abs();
        self.side = if self.net_qty >= 0.0 {
            BasketSide::Long
        } else {
            BasketSide::Short
        };
        self.tp_count += 1;

        if self.open_qty > 1e-9 {
            self.status = BasketStatus::TpRecycling;
        } else {
            // Basket fully closed — reset the per-basket SL anchor so the
            // NEXT entry fill re-anchors at fresh price.
            self.status = BasketStatus::Idle;
            self.anchor_price = 0.0;
            self.upper_sl = 0.0;
            self.lower_sl = 0.0;
            self.avg_price = 0.0;
            self.net_qty = 0.0;
        }
    }

    /// Permanent kill — flatten any remaining position into realized_pnl
    /// at `exit_price` and mark Killed.
    pub fn kill(&mut self, exit_price: f64) {
        if self.net_qty.abs() > 0.0 {
            let diff = if self.net_qty > 0.0 {
                exit_price - self.avg_price // closing a net long
            } else {
                self.avg_price - exit_price // closing a net short
            };
            let qty = self.net_qty.abs();
            self.realized_pnl += self.pnl_usd(diff, qty, self.avg_price);
        }
        self.net_qty = 0.0;
        self.open_qty = 0.0;
        self.status = BasketStatus::Killed;
    }

    /// Soft cycle reset (legacy from the old global cycle SL; unused now).
    pub fn soft_reset(&mut self, exit_price: f64) {
        if self.net_qty.abs() > 0.0 {
            let diff = if self.net_qty > 0.0 {
                exit_price - self.avg_price
            } else {
                self.avg_price - exit_price
            };
            let qty = self.net_qty.abs();
            self.realized_pnl += self.pnl_usd(diff, qty, self.avg_price);
        }
        self.net_qty = 0.0;
        self.open_qty = 0.0;
        self.avg_price = 0.0;
        self.status = BasketStatus::Hit;
    }
}
