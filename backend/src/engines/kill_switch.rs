use crate::engines::basket_manager::BasketManager;
use crate::engines::slicing::SlicingEngine;
use crate::exchanges::Exchange;
use crate::models::{BasketSide, BasketStatus, OrderPurpose, Side};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{error, warn};
use uuid::Uuid;

pub struct KillSwitch {
    pub tripped: Arc<AtomicBool>,
    pub reason: parking_lot::RwLock<Option<String>>,
    pub basket_mgr: Arc<BasketManager>,
    pub exchange: Arc<dyn Exchange>,
    pub slicing: Arc<SlicingEngine>,
}

impl KillSwitch {
    pub fn new(
        basket_mgr: Arc<BasketManager>,
        exchange: Arc<dyn Exchange>,
        slicing: Arc<SlicingEngine>,
    ) -> Self {
        Self {
            tripped: Arc::new(AtomicBool::new(false)),
            reason: parking_lot::RwLock::new(None),
            basket_mgr,
            exchange,
            slicing,
        }
    }

    pub fn is_tripped(&self) -> bool {
        self.tripped.load(Ordering::Relaxed)
    }

    pub fn reason(&self) -> Option<String> {
        self.reason.read().clone()
    }

    /// Trigger the kill switch.
    /// Flow: lock state → cancel all open orders → flatten EVERY basket using
    /// the correct close direction per basket side (long → SELL to close,
    /// short → BUY to close) → mark baskets as Killed.
    ///
    /// Critical detail: a SHORT basket is closed by BUYING, not selling. The
    /// previous version hardcoded Sell for every basket which deepened shorts
    /// instead of closing them, leaving residual exchange positions.
    pub async fn trip(&self, reason: String) {
        // Idempotent: if already tripped, do nothing (caller may call this
        // from multiple paths simultaneously).
        if self.tripped.swap(true, Ordering::Relaxed) {
            return;
        }
        *self.reason.write() = Some(reason.clone());
        error!(%reason, "KILL SWITCH TRIPPED — flattening all positions");

        // 1. Cancel every resting order.
        if let Err(e) = self.exchange.cancel_all().await {
            error!(?e, "cancel_all failed during kill switch");
        }

        // 2. Snapshot every basket that has open qty + its side + index.
        let to_flatten: Vec<(Uuid, BasketSide, f64, u32)> = self
            .basket_mgr
            .baskets
            .iter()
            .filter_map(|e| {
                let b = e.value();
                if b.open_qty > 0.0 {
                    Some((b.basket_id, b.side, b.open_qty, b.index))
                } else {
                    None
                }
            })
            .collect();

        // 3. Flatten each basket in the correct direction.
        for (bid, basket_side, qty, idx) in to_flatten {
            let close_side = match basket_side {
                BasketSide::Long => Side::Sell, // close long  → sell into the bid
                BasketSide::Short => Side::Buy, // close short → buy from the ask
            };
            match self
                .slicing
                .flatten(bid, close_side, qty, OrderPurpose::KillSwitchExit)
                .await
            {
                Ok(exit_price) => {
                    if let Some(mut b) = self.basket_mgr.baskets.get_mut(&bid) {
                        b.kill(exit_price); // zeroes open_qty, books PnL, sets Killed
                    }
                }
                Err(e) => {
                    // CRITICAL: slice failed. Position may still be open on the
                    // exchange. Log loudly and still mark the basket Killed so
                    // the bot doesn't loop forever, but the operator must
                    // square off manually.
                    error!(
                        ?e,
                        basket_index = idx,
                        ?basket_side,
                        qty,
                        "CRITICAL: emergency flatten failed — exchange position may still be open. Square off manually."
                    );
                    if let Some(mut b) = self.basket_mgr.baskets.get_mut(&bid) {
                        // Use kill() with the basket's last avg_price as a
                        // placeholder exit price for PnL bookkeeping. Read
                        // into a local first to satisfy the borrow checker.
                        let placeholder_exit = b.avg_price;
                        b.kill(placeholder_exit);
                    }
                }
            }
        }

        // 4. Force any remaining baskets (those with no open qty) to Killed
        //    so the bot definitely stops after this trip.
        for mut entry in self.basket_mgr.baskets.iter_mut() {
            if entry.status != BasketStatus::Killed {
                entry.status = BasketStatus::Killed;
            }
            if entry.open_qty > 0.0 {
                warn!(
                    basket_index = entry.index,
                    open_qty = entry.open_qty,
                    "basket marked KILLED but still has bookkeeping open_qty after flatten"
                );
            }
        }
    }

    pub fn manual_reset(&self) {
        self.tripped.store(false, Ordering::Relaxed);
        *self.reason.write() = None;
    }
}
