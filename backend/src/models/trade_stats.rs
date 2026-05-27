use crate::models::Side;
use serde::Serialize;
use uuid::Uuid;

/// Aggregated trading metrics from the start of the current engine session.
/// All values are cumulative over the lifetime of the running engine.
#[derive(Debug, Clone, Serialize)]
pub struct TradeStats {
    /// Engine start time in unix milliseconds.
    pub start_time: i64,
    /// Seconds since engine started.
    pub duration_seconds: i64,

    /// NET pnl = rtp_pnl + sl_pnl. Can be negative when SL losses dominate.
    /// Use `total_pnl` as a synonym (legacy field name kept for clients).
    pub total_pnl: f64,
    pub net_pnl: f64,
    /// PnL from TP-closed round trips only. Should always be ≥ 0.
    pub rtp_pnl: f64,
    /// PnL from SL/kill-switch exits only. Should always be ≤ 0.
    pub sl_pnl: f64,

    pub total_fees: f64,

    /// Count of TP-closed round trips only (does NOT include SL exits).
    pub round_trips: u64,
    /// Count of SL / kill-switch exits.
    pub sl_count: u64,

    pub rtp_per_hour: f64,
    pub pnl_per_hour: f64,

    pub buy_vwap: f64,
    pub sell_vwap: f64,

    pub total_volume: f64,
    pub buy_volume: f64,
    pub sell_volume: f64,

    pub buy_qty: f64,
    pub sell_qty: f64,
    pub net_qty: f64,

    pub total_fills: u64,
    pub total_buys: u64,
    pub total_sells: u64,
}

impl TradeStats {
    pub fn empty(start_time: i64) -> Self {
        Self {
            start_time,
            duration_seconds: 0,
            total_pnl: 0.0,
            net_pnl: 0.0,
            rtp_pnl: 0.0,
            sl_pnl: 0.0,
            total_fees: 0.0,
            round_trips: 0,
            sl_count: 0,
            rtp_per_hour: 0.0,
            pnl_per_hour: 0.0,
            buy_vwap: 0.0,
            sell_vwap: 0.0,
            total_volume: 0.0,
            buy_volume: 0.0,
            sell_volume: 0.0,
            buy_qty: 0.0,
            sell_qty: 0.0,
            net_qty: 0.0,
            total_fills: 0,
            total_buys: 0,
            total_sells: 0,
        }
    }
}

/// One completed round-trip: an entry leg matched against a closing leg
/// (take-profit or stop-loss exit), FIFO-paired per basket.
///
/// entry_side tells you the direction of the round-trip:
///   - Buy  → LONG round-trip (entry buy + exit sell)
///   - Sell → SHORT round-trip (entry sell + exit buy)
#[derive(Debug, Clone, Serialize)]
pub struct RoundTrip {
    pub rtp_id: Uuid,
    pub basket_id: Uuid,
    pub basket_index: u32,
    pub entry_side: Side,
    pub entry_price: f64,
    pub exit_price: f64,
    pub qty: f64,
    /// Gross pnl = (exit - entry) * qty for longs, (entry - exit) * qty for shorts
    pub gross_pnl: f64,
    /// Total fees paid on both legs (entry + exit) proportional to this qty.
    pub fees: f64,
    /// Net pnl = gross_pnl - fees
    pub pnl: f64,
    /// Notional volume = (entry_price + exit_price) * qty
    pub volume: f64,
    pub entry_time: i64,
    pub exit_time: i64,
    /// Was this closed by a take-profit (true) or a stop-loss / kill (false)?
    pub is_take_profit: bool,
}
