use crate::engines::basket_manager::BasketManager;
use crate::engines::slicing::SlicingEngine;
use crate::exchanges::Exchange;
use crate::models::{BasketStatus, OrderPurpose, Side};
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

        // 2. Snapshot every basket that has open qty + its signed net + index.
        let to_flatten: Vec<(Uuid, f64, u32)> = self
            .basket_mgr
            .baskets
            .iter()
            .filter_map(|e| {
                let b = e.value();
                if b.net_qty.abs() > 1e-9 {
                    Some((b.basket_id, b.net_qty, b.index))
                } else {
                    None
                }
            })
            .collect();

        // 3. Flatten each basket in the correct direction (sign of net_qty).
        for (bid, net_qty, idx) in to_flatten {
            let close_side = if net_qty > 0.0 { Side::Sell } else { Side::Buy };
            let qty = net_qty.abs();
            match self
                .slicing
                .flatten(bid, close_side, qty, OrderPurpose::KillSwitchExit)
                .await
            {
                Ok(exit_price) => {
                    if let Some(mut b) = self.basket_mgr.baskets.get_mut(&bid) {
                        b.kill(exit_price);
                    }
                }
                Err(e) => {
                    error!(
                        ?e,
                        basket_index = idx,
                        net_qty,
                        "CRITICAL: emergency flatten failed — exchange position may still be open. Square off manually."
                    );
                    if let Some(mut b) = self.basket_mgr.baskets.get_mut(&bid) {
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
            if entry.net_qty.abs() > 1e-9 {
                warn!(
                    basket_index = entry.index,
                    net_qty = entry.net_qty,
                    "basket marked KILLED but still has bookkeeping net_qty after flatten"
                );
            }
        }
    }

    pub fn manual_reset(&self) {
        self.tripped.store(false, Ordering::Relaxed);
        *self.reason.write() = None;
    }
}
