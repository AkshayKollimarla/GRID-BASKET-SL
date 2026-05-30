use crate::engines::basket_manager::BasketManager;
use crate::exchanges::Exchange;
use crate::models::{AgentConfig, BasketSide, OrderPurpose, Side};
use parking_lot::Mutex;
use std::collections::HashSet;
use std::sync::Arc;
use tracing::{debug, warn};

pub struct GridEngine {
    pub config: AgentConfig,
    pub basket_mgr: Arc<BasketManager>,
    pub exchange: Arc<dyn Exchange>,
    /// Cache of the previous tick's grid composition so we only emit a
    /// `Grid: 3 above + 3 below = 6 (5 entries, 1 targets)` log line when
    /// it actually changes — otherwise the UI log would flood every tick.
    /// (sells_above, buys_below, entries_total, targets_total)
    pub last_grid_summary: Arc<Mutex<Option<(usize, usize, usize, usize)>>>,
    /// Pending log lines that the engine drains each tick via
    /// `take_pending_log()` and forwards to the bot status log so the
    /// user sees them in the UI.
    pub pending_log: Arc<Mutex<Vec<String>>>,
}

impl GridEngine {
    /// Drain & return any pending one-line status messages the grid wants
    /// to surface in the bot log (e.g. the grid summary). Called by the
    /// engine main loop right after `grid.step()`.
    pub fn take_pending_log(&self) -> Vec<String> {
        std::mem::take(&mut *self.pending_log.lock())
    }
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
            last_grid_summary: Arc::new(Mutex::new(None)),
            pending_log: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Push a `Grid: <s> above + <b> below = <n> (<e> entries, <t> targets)`
    /// status line into `pending_log` ONLY if the composition has changed
    /// since the last tick. Engine drains pending_log after each step()
    /// and forwards messages to the UI bot log.
    fn note_grid_summary(&self, open: &[crate::models::Order], mid: f64) {
        let sells_above = open.iter().filter(|o| o.price > mid).count();
        let buys_below = open.iter().filter(|o| o.price < mid).count();
        let entries = open
            .iter()
            .filter(|o| matches!(o.purpose, OrderPurpose::Entry))
            .count();
        let targets = open
            .iter()
            .filter(|o| matches!(o.purpose, OrderPurpose::TakeProfit))
            .count();
        let key = (sells_above, buys_below, entries, targets);
        let mut g = self.last_grid_summary.lock();
        if *g == Some(key) {
            return;
        }
        *g = Some(key);
        self.pending_log.lock().push(format!(
            "Grid: {} above + {} below = {} ({} entries, {} targets)",
            sells_above,
            buys_below,
            sells_above + buys_below,
            entries,
            targets
        ));
    }

    /// Run one pass of the entries-only grid maintenance.
    ///
    /// Adapted from the user's JS bot pattern:
    ///   • TPs are placed in `process_fill` the instant an entry fills,
    ///     at the exact `fill_price ± tp_spread`. They are NOT touched here.
    ///   • This grid tick:
    ///       1. Cancels stale orders:
    ///            - ENTRY on the wrong side of mid (price crossed without
    ///              filling)
    ///            - TARGET further than `3 × step` from mid (stuck)
    ///       2. Counts orders per side (ALL types — entries + targets).
    ///       3. Tops the side up with fresh ENTRY orders at snapped grid
    ///          levels until each side has exactly `depth` orders total.
    ///
    /// Cap rule: at most `depth` orders per side. If targets occupy N slots
    /// on a side, no new entries are placed on that side.
    pub async fn step(&self, anchor: f64, distance: f64) {
        if self.basket_mgr.all_killed() {
            return;
        }
        if anchor <= 0.0 || distance <= 0.0 {
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
        let per_step_qty = t.per_step_qty;

        let cycle_lower = anchor - distance;
        let cycle_upper = anchor + distance;

        // FORBIDDEN ZONE — no entry may rest inside [mid − tp_spread/2, mid + tp_spread/2].
        // The zone TRAILS MID so the grid follows price up and down. As mid
        // moves, old entries on the wrong side of the new zone get pulled
        // and new ones are placed at the fresh 1st-level slots:
        //   • 1st SELL = mid + tp_spread/2 (e.g., mid 2011 + 1.25 = 2012.25)
        //   • 1st BUY  = mid − tp_spread/2 (e.g., mid 2011 − 1.25 = 2009.75)
        //   • gap between them = exactly tp_spread (the user's rule)
        // TPs are exempt — they're priced off their own entry fill, not off mid.
        let half_spread = (t.tp_spread / 2.0).max(0.0);

        // ---------- 1. Cancel stale orders ----------
        // PARK-AND-WAIT POLICY (user's explicit rule):
        //   • Cancel an ENTRY only when it is now on the WRONG SIDE of
        //     mid (mid crossed past it; would be a taker if filled). No
        //     "too far" cancellation — once placed, an order rests
        //     parked at its price until it fills naturally or becomes
        //     wrong-side.
        //   • TPs are NEVER cancelled. Each TP rests at `fill ± tp_spread`
        //     from the moment its entry filled until price comes back to
        //     close the round trip.
        // Result: NO churn. The previous "cancel if > depth × step from
        // mid" rule caused the grid to repeatedly cancel + re-place the
        // same orders every time mid wobbled by ~step/2 — that's what
        // produced the timestamp-spam in the user's screenshot.
        // When count < depth on a side (e.g. after a fill or wrong-side
        // cancel), step #3 below fills the gap with a new order near
        // current mid.
        let eps = 1e-6_f64;
        let open = self.exchange.open_orders().await;
        let mut cancelled_wrong_side = 0u32;
        for o in &open {
            let should_cancel = match o.purpose {
                OrderPurpose::Entry => {
                    let wrong_side = (o.side == Side::Sell && o.price <= mid + eps)
                        || (o.side == Side::Buy && o.price >= mid - eps);
                    if wrong_side {
                        cancelled_wrong_side += 1;
                    }
                    wrong_side
                }
                OrderPurpose::TakeProfit => false,
                _ => false,
            };
            if should_cancel {
                let _ = self.exchange.cancel(o.order_id).await;
            }
        }
        if cancelled_wrong_side > 0 {
            debug!(
                wrong_side = cancelled_wrong_side,
                "grid tick cancelled wrong-side entries"
            );
        }

        // ---------- 2. Re-fetch resting state ----------
        let open = self.exchange.open_orders().await;

        // Per-side count (counts BOTH entries and targets — they share the
        // depth budget). Mid-based split tracks the trailing grid.
        let sell_count = open.iter().filter(|o| o.price > mid).count();
        let buy_count = open.iter().filter(|o| o.price < mid).count();

        // Occupied price levels (so we don't double-place on a slot a TP
        // already occupies).
        let key = |p: f64| (p * 100.0).round() as i64;
        let occupied: HashSet<i64> = open.iter().map(|o| key(o.price)).collect();

        // Exposure cap — only blocks NEW placements, not cancels.
        let total_open = self.basket_mgr.total_open_qty();
        if total_open >= self.config.kill_switch.max_position_cap {
            debug!(total_open, "exposure cap reached — no new entries");
            return;
        }

        // ---------- 3. Place entries anchored to mid ± tp_spread/2 ----------
        // 1st sell sits EXACTLY at mid + tp_spread/2. 1st buy at mid −
        // tp_spread/2. Subsequent levels are grid_step apart from there.
        // Result: gap between 1st sell and 1st buy = tp_spread (the user's
        // explicit rule). The grid_step controls intra-side spacing only.
        //
        // Base prices are quantized to a 0.5 tick so a tp_spread like 2.5
        // (half_spread = 1.25) lands cleanly on 0.5-tick prices.
        let tick = 0.5_f64;
        let quantize_up = |p: f64| (p / tick).ceil() * tick;
        let quantize_dn = |p: f64| (p / tick).floor() * tick;
        let base_sell = quantize_up(mid + half_spread);
        let base_buy = quantize_dn(mid - half_spread);

        // 3a. Top up SELL side: 1st @ base_sell, then base_sell+step, +2·step…
        let mut need_sells = depth.saturating_sub(sell_count);
        let mut k = 0u32;
        while need_sells > 0 && k <= 20 {
            let price = base_sell + (k as f64) * step;
            k += 1;
            // Forbidden-zone guard (paranoia — base_sell already satisfies it).
            if price < mid + half_spread - eps {
                continue;
            }
            if price > cycle_upper {
                continue;
            }
            if occupied.contains(&key(price)) {
                continue;
            }
            let Some(basket_id) = self
                .basket_mgr
                .find_basket_with_capacity_by_side(BasketSide::Short, per_step_qty)
            else {
                break; // no Short basket has capacity
            };
            match self
                .exchange
                .place_maker_only(
                    Side::Sell,
                    price,
                    per_step_qty,
                    basket_id,
                    OrderPurpose::Entry,
                )
                .await
            {
                Ok(order) => {
                    self.basket_mgr.link_order(order.order_id, basket_id);
                    need_sells -= 1;
                }
                Err(e) => warn!(?e, price, "SELL entry placement failed"),
            }
        }

        // 3b. Top up BUY side: 1st @ base_buy, then base_buy−step, −2·step…
        let mut need_buys = depth.saturating_sub(buy_count);
        let mut k = 0u32;
        while need_buys > 0 && k <= 20 {
            let price = base_buy - (k as f64) * step;
            k += 1;
            // Forbidden-zone guard (paranoia — base_buy already satisfies it).
            if price > mid - half_spread + eps {
                continue;
            }
            if price < cycle_lower {
                continue;
            }
            if occupied.contains(&key(price)) {
                continue;
            }
            let Some(basket_id) = self
                .basket_mgr
                .find_basket_with_capacity_by_side(BasketSide::Long, per_step_qty)
            else {
                break;
            };
            match self
                .exchange
                .place_maker_only(
                    Side::Buy,
                    price,
                    per_step_qty,
                    basket_id,
                    OrderPurpose::Entry,
                )
                .await
            {
                Ok(order) => {
                    self.basket_mgr.link_order(order.order_id, basket_id);
                    need_buys -= 1;
                }
                Err(e) => warn!(?e, price, "BUY entry placement failed"),
            }
        }

        // NOTE: TP RECOVERY was removed.
        // It used to live here and would place aggregated basket-avg-based
        // TPs (qty = sum of basket's uncovered fills) every tick.
        // That conflicted with process_fill's per-fill TPs (one TP per entry
        // fill at exact fill_price ± tp_spread) and caused the runaway "$10K
        // TP placed and cancelled every 2 seconds" loop.
        //
        // process_fill is now the single source of TP placement. Each entry
        // fill places exactly one TP at exact spread. If that placement
        // fails (e.g., post_only_reject), the bot accepts the risk for that
        // fill — the cycle SL still protects the position.

        // ---------- 4. Note the final grid composition for the bot log ----
        // Only emits when the (sells_above, buys_below, entries, targets)
        // tuple changes, so the log shows ONE line per real grid update.
        let final_open = self.exchange.open_orders().await;
        self.note_grid_summary(&final_open, mid);
    }
}
