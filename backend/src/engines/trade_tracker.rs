// FIFO round-trip pairing + cumulative trade statistics, side-aware.
//
// Pairing rule: ANY fill with OrderPurpose::Entry opens a lot for its basket.
// Any other fill (TakeProfit / StopLossExit / KillSwitchExit) consumes lots
// FIFO for that basket. The lot remembers the entry side, so PnL is computed
// correctly for both long and short legs:
//   Long  (entry Buy  → exit Sell): pnl = (exit_px - entry_px) * qty
//   Short (entry Sell → exit Buy):  pnl = (entry_px - exit_px) * qty

use crate::engines::basket_manager::BasketManager;
use crate::models::{Fill, OrderPurpose, RoundTrip, Side, TradeStats};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

/// One open lot waiting to be matched against a future closing fill.
#[derive(Debug, Clone)]
struct Lot {
    entry_side: Side,
    price: f64,
    qty_remaining: f64,
    fee_per_unit: f64, // entry fee divided over qty so partial matches are proportional
    time: i64,
}

#[derive(Debug)]
struct State {
    start_time_ms: i64,
    // Per-basket FIFO of unmatched lots.
    lots: HashMap<Uuid, Vec<Lot>>,
    // Completed round trips (BOTH TP-closed and SL-closed are recorded here,
    // distinguished by RoundTrip.is_take_profit).
    round_trips: Vec<RoundTrip>,
    // Cumulative counters.
    rtp_pnl: f64, // PnL from TP exits only
    sl_pnl: f64,  // PnL from SL / kill-switch exits (≤ 0 in practice)
    rtp_count: u64,
    sl_count: u64,
    total_fees: f64,
    buy_volume: f64,
    sell_volume: f64,
    buy_qty: f64,
    sell_qty: f64,
    total_fills: u64,
    total_buys: u64,
    total_sells: u64,
}

pub struct TradeTracker {
    state: Mutex<State>,
    basket_mgr: Arc<BasketManager>,
    /// True if the underlying exchange uses inverse contracts (qty in USD).
    /// In that case PnL = (exit-entry)*qty / entry_price.
    is_inverse: bool,
}

impl TradeTracker {
    pub fn new(
        basket_mgr: Arc<BasketManager>,
        start_time_ms: i64,
        is_inverse: bool,
    ) -> Self {
        Self {
            basket_mgr,
            is_inverse,
            state: Mutex::new(State {
                start_time_ms,
                lots: HashMap::new(),
                round_trips: Vec::new(),
                rtp_pnl: 0.0,
                sl_pnl: 0.0,
                rtp_count: 0,
                sl_count: 0,
                total_fees: 0.0,
                buy_volume: 0.0,
                sell_volume: 0.0,
                buy_qty: 0.0,
                sell_qty: 0.0,
                total_fills: 0,
                total_buys: 0,
                total_sells: 0,
            }),
        }
    }

    pub fn ingest(&self, fill: &Fill) {
        let mut s = self.state.lock();
        s.total_fills += 1;
        s.total_fees += fill.fee.max(0.0);

        let fee_per_unit = if fill.qty > 0.0 {
            fill.fee.max(0.0) / fill.qty
        } else {
            0.0
        };

        // USD notional per fill:
        //   Inverse (Deribit BTC-PERP): qty is already in USD → notional = qty
        //   Linear (HL, Mock):          qty is in BASE coin   → notional = qty × price
        let notional_usd = if self.is_inverse {
            fill.qty
        } else {
            fill.qty * fill.price
        };

        // Side-agnostic volume/qty counters.
        match fill.side {
            Side::Buy => {
                s.total_buys += 1;
                s.buy_qty += fill.qty;
                s.buy_volume += notional_usd;
            }
            Side::Sell => {
                s.total_sells += 1;
                s.sell_qty += fill.qty;
                s.sell_volume += notional_usd;
            }
        }

        // Entries OPEN a lot; everything else CLOSES against existing lots.
        if matches!(fill.purpose, OrderPurpose::Entry) {
            s.lots.entry(fill.basket_id).or_default().push(Lot {
                entry_side: fill.side,
                price: fill.price,
                qty_remaining: fill.qty,
                fee_per_unit,
                time: fill.timestamp,
            });
            return;
        }

        // Closing fill — pair FIFO against this basket's open lots.
        let is_tp = matches!(fill.purpose, OrderPurpose::TakeProfit);
        let basket_index = self
            .basket_mgr
            .baskets
            .get(&fill.basket_id)
            .map(|b| b.index)
            .unwrap_or(0);

        let mut qty_to_close = fill.qty;
        let exit_fee_per_unit = fee_per_unit;
        let exit_price = fill.price;
        let exit_time = fill.timestamp;
        let basket_id = fill.basket_id;

        let mut rtp_pnl_delta = 0.0_f64;
        let mut sl_pnl_delta = 0.0_f64;
        let mut rtp_count_delta = 0u64;
        let mut sl_count_delta = 0u64;
        let mut new_rtps: Vec<RoundTrip> = Vec::new();
        {
            let lots = s.lots.entry(basket_id).or_default();
            while qty_to_close > 1e-12 && !lots.is_empty() {
                let take = qty_to_close.min(lots[0].qty_remaining);
                let lot = lots[0].clone();
                lots[0].qty_remaining -= take;
                if lots[0].qty_remaining <= 1e-12 {
                    lots.remove(0);
                }
                qty_to_close -= take;

                // Sign-aware PnL: long lot (entry Buy) profits when exit > entry;
                // short lot (entry Sell) profits when entry > exit.
                // For INVERSE contracts (Deribit), divide by entry price to
                // convert qty-in-USD into base-currency PnL × price (= USD).
                let diff = match lot.entry_side {
                    Side::Buy => exit_price - lot.price,
                    Side::Sell => lot.price - exit_price,
                };
                let gross = if self.is_inverse && lot.price > 0.0 {
                    diff * take / lot.price
                } else {
                    diff * take
                };
                let fees = (lot.fee_per_unit + exit_fee_per_unit) * take;
                let net = gross - fees;
                // Per-RTP volume = one-leg USD notional (so a $20 entry + $20
                // exit shows as $20, not $40). For inverse the qty IS already
                // USD; for linear we convert via entry price.
                let vol = if self.is_inverse {
                    take
                } else {
                    take * lot.price
                };
                // Split PnL by close kind so the Trade Summary can show
                // RTP PnL and SL PnL separately.
                if is_tp {
                    rtp_pnl_delta += net;
                    rtp_count_delta += 1;
                } else {
                    sl_pnl_delta += net;
                    sl_count_delta += 1;
                }
                new_rtps.push(RoundTrip {
                    rtp_id: Uuid::new_v4(),
                    basket_id,
                    basket_index,
                    entry_side: lot.entry_side,
                    entry_price: lot.price,
                    exit_price,
                    qty: take,
                    gross_pnl: gross,
                    fees,
                    pnl: net,
                    volume: vol,
                    entry_time: lot.time,
                    exit_time,
                    is_take_profit: is_tp,
                });
            }
        }
        s.rtp_pnl += rtp_pnl_delta;
        s.sl_pnl += sl_pnl_delta;
        s.rtp_count += rtp_count_delta;
        s.sl_count += sl_count_delta;
        s.round_trips.extend(new_rtps);
    }

    pub fn stats(&self, now_ms: i64) -> TradeStats {
        let s = self.state.lock();
        let duration_ms = (now_ms - s.start_time_ms).max(0);
        let duration_secs = duration_ms / 1000;
        let duration_hours = duration_ms as f64 / 3_600_000.0;

        let buy_vwap = if s.buy_qty > 0.0 {
            s.buy_volume / s.buy_qty
        } else {
            0.0
        };
        let sell_vwap = if s.sell_qty > 0.0 {
            s.sell_volume / s.sell_qty
        } else {
            0.0
        };
        let net_pnl = s.rtp_pnl + s.sl_pnl;
        let rtp_per_hour = if duration_hours > 0.0 {
            s.rtp_count as f64 / duration_hours
        } else {
            0.0
        };
        let pnl_per_hour = if duration_hours > 0.0 {
            net_pnl / duration_hours
        } else {
            0.0
        };

        TradeStats {
            start_time: s.start_time_ms,
            duration_seconds: duration_secs,
            // legacy alias; identical to net_pnl
            total_pnl: net_pnl,
            net_pnl,
            rtp_pnl: s.rtp_pnl,
            sl_pnl: s.sl_pnl,
            total_fees: s.total_fees,
            round_trips: s.rtp_count, // TP-only count, per user spec
            sl_count: s.sl_count,
            rtp_per_hour,
            pnl_per_hour,
            buy_vwap,
            sell_vwap,
            total_volume: s.buy_volume + s.sell_volume,
            buy_volume: s.buy_volume,
            sell_volume: s.sell_volume,
            buy_qty: s.buy_qty,
            sell_qty: s.sell_qty,
            net_qty: s.buy_qty - s.sell_qty,
            total_fills: s.total_fills,
            total_buys: s.total_buys,
            total_sells: s.total_sells,
        }
    }

    pub fn recent_round_trips(&self, limit: usize) -> Vec<RoundTrip> {
        let s = self.state.lock();
        s.round_trips
            .iter()
            .rev()
            .take(limit)
            .cloned()
            .collect()
    }
}
