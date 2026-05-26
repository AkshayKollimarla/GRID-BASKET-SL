use crate::engines::basket_manager::BasketManager;
use crate::engines::slicing::SlicingEngine;
use crate::exchanges::Exchange;
use crate::models::{BasketStatus, OrderPurpose, Side};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::error;

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
    /// Per flow: disable trading → block entries → cancel all open orders → flatten positions → lock bot.
    pub async fn trip(&self, reason: String) {
        if self.tripped.swap(true, Ordering::Relaxed) {
            return;
        }
        *self.reason.write() = Some(reason.clone());
        error!(%reason, "KILL SWITCH TRIPPED");

        // 1. Cancel all open orders
        if let Err(e) = self.exchange.cancel_all().await {
            error!(?e, "cancel_all failed");
        }

        // 2. Flatten every basket that has open qty (market reduce-only).
        let baskets: Vec<_> = self
            .basket_mgr
            .baskets
            .iter()
            .filter(|e| e.value().open_qty > 0.0)
            .map(|e| (e.value().basket_id, e.value().open_qty))
            .collect();

        for (bid, qty) in baskets {
            if let Ok(exit_price) = self
                .slicing
                .flatten(bid, Side::Sell, qty, OrderPurpose::KillSwitchExit)
                .await
            {
                if let Some(mut b) = self.basket_mgr.baskets.get_mut(&bid) {
                    b.kill(exit_price);
                }
            }
        }
        // 3. Mark all remaining baskets as killed.
        for mut entry in self.basket_mgr.baskets.iter_mut() {
            entry.status = BasketStatus::Killed;
        }
    }

    pub fn manual_reset(&self) {
        self.tripped.store(false, Ordering::Relaxed);
        *self.reason.write() = None;
    }
}
