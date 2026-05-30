use crate::engines::basket_manager::BasketManager;
use crate::exchanges::Exchange;
use crate::models::{AgentConfig, OrderPurpose, Side};
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
    /// Internal grid anchor price — separate from the cycle anchor used
    /// by per-basket SL. This anchor only moves when mid has drifted
    /// more than `depth × step` from it. While mid wobbles inside that
    /// band, the anchor stays put and the grid does NOT churn. When mid
    /// crosses the band edge we re-anchor at current mid and refresh
    /// any entries that are now too far from the new anchor. Mirrors
    /// the HL bot's "Grid re-anchored to $X" pattern.
    pub grid_anchor: Arc<Mutex<Option<f64>>>,
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
            grid_anchor: Arc::new(Mutex::new(None)),
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

        // ---------- Re-anchor decision (hysteresis) ----------
        // The grid keeps its own anchor, separate from the cycle SL anchor.
        // It moves only when mid has drifted more than `depth × step` from
        // it. Inside that band the anchor stays put → no churn. Outside →
        // we re-anchor at current mid and refresh stale orders.
        let re_anchor_threshold = (depth as f64) * step;
        let mut re_anchored = false;
        let active_anchor = {
            let mut guard = self.grid_anchor.lock();
            let need_reanchor = match *guard {
                None => true,
                Some(a) => (a - mid).abs() > re_anchor_threshold,
            };
            if need_reanchor {
                *guard = Some(mid);
                re_anchored = true;
                mid
            } else {
                guard.unwrap_or(mid)
            }
        };
        if re_anchored {
            self.pending_log.lock().push(format!(
                "Grid re-anchored to {:.2} (mid {:.4}, threshold ±{:.2})",
                active_anchor, mid, re_anchor_threshold
            ));
        }

        // FORBIDDEN ZONE — no entry may rest inside
        // [active_anchor − tp_spread/2, active_anchor + tp_spread/2].
        // Anchored placement gives a stable grid: 1st SELL at
        // active_anchor + half_spread, 1st BUY at active_anchor −
        // half_spread, both quantized to 0.5 tick. TPs are exempt — they
        // sit at their per-fill prices, parked off-exchange when out of
        // depth budget (see process_fill in engine.rs).
        let half_spread = (t.tp_spread / 2.0).max(0.0);

        // ---------- 1. Cancel stale orders ----------
        // Re-anchor-aware cancel:
        //   • Entry is WRONG-SIDE of mid → cancel (taker risk).
        //   • Entry is more than `far_threshold` from the GRID ANCHOR
        //     → cancel. Threshold is sized to comfortably contain every
        //     planned grid level PLUS one step of buffer, so it never
        //     false-cancels an order the placement loop just put down
        //     (previously (depth+1)*step matched the furthest level
        //     EXACTLY → float-precision oscillation cancelled & re-placed
        //     the 3rd-level order every tick).
        //   • TPs are NEVER cancelled by the grid tick (they sit at
        //     their per-fill prices; out-of-budget ones are parked by
        //     process_fill, not by us).
        // Furthest planned level distance from anchor:
        //   = half_spread + (depth - 1) * step
        // Add `step` of buffer so re-anchor refreshes only orders that
        // are genuinely outside the grid envelope.
        let eps = 1e-6_f64;
        let far_threshold = half_spread + (depth as f64) * step + step;
        let open = self.exchange.open_orders().await;
        let mut cancelled_wrong_side = 0u32;
        let mut cancelled_far = 0u32;
        for o in &open {
            let should_cancel = match o.purpose {
                OrderPurpose::Entry => {
                    let wrong_side = (o.side == Side::Sell && o.price <= mid + eps)
                        || (o.side == Side::Buy && o.price >= mid - eps);
                    let too_far = (o.price - active_anchor).abs() > far_threshold;
                    if wrong_side {
                        cancelled_wrong_side += 1;
                    } else if too_far {
                        cancelled_far += 1;
                    }
                    wrong_side || too_far
                }
                OrderPurpose::TakeProfit => false,
                _ => false,
            };
            if should_cancel {
                let _ = self.exchange.cancel(o.order_id).await;
            }
        }
        if cancelled_wrong_side > 0 || cancelled_far > 0 {
            debug!(
                wrong_side = cancelled_wrong_side,
                far = cancelled_far,
                far_threshold,
                "grid tick cancelled stale entries"
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

        // ---------- 3. Place entries anchored to ACTIVE_ANCHOR ± half_spread ----
        // 1st sell at active_anchor + half_spread, 1st buy at active_anchor −
        // half_spread. Subsequent levels stepped out by grid_step. The
        // anchor only moves when mid drifts > depth*step, so the grid
        // is STABLE between re-anchors — placements aren't recomputed
        // tick-by-tick from a wobbly mid.
        let tick = 0.5_f64;
        let quantize_up = |p: f64| (p / tick).ceil() * tick;
        let quantize_dn = |p: f64| (p / tick).floor() * tick;
        let base_sell = quantize_up(active_anchor + half_spread);
        let base_buy = quantize_dn(active_anchor - half_spread);

        // 3a. Top up SELL side: 1st @ base_sell, then base_sell+step, +2·step…
        let mut need_sells = depth.saturating_sub(sell_count);
        let mut k = 0u32;
        while need_sells > 0 && k <= 20 {
            let price = base_sell + (k as f64) * step;
            k += 1;
            // Sanity guard: never place on or below mid (would be a taker).
            if price <= mid + eps {
                continue;
            }
            // Don't place an order the cancel rule would immediately kill —
            // that loop produced the 2014 cancel/replace storm. Step out
            // of the loop entirely (subsequent k values are even farther).
            if (price - active_anchor).abs() > far_threshold {
                break;
            }
            if price > cycle_upper {
                continue;
            }
            if occupied.contains(&key(price)) {
                continue;
            }
            let Some(basket_id) = self
                .basket_mgr
                .find_basket_with_capacity(per_step_qty, Side::Sell)
            else {
                break; // no basket has capacity for a SELL of this size
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
            // Sanity guard: never place on or above mid (would be a taker).
            if price >= mid - eps {
                continue;
            }
            // Same far_threshold guard as the sell side — don't place
            // orders the cancel rule will immediately kill.
            if (price - active_anchor).abs() > far_threshold {
                break;
            }
            if price < cycle_lower {
                continue;
            }
            if occupied.contains(&key(price)) {
                continue;
            }
            let Some(basket_id) = self
                .basket_mgr
                .find_basket_with_capacity(per_step_qty, Side::Buy)
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
