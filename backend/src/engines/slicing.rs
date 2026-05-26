use crate::exchanges::Exchange;
use crate::models::{EmergencySlicingConfig, OrderPurpose, Side};
use anyhow::Result;
use std::sync::Arc;
use tokio::time::{sleep, Duration};
use tracing::{info, warn};
use uuid::Uuid;

pub struct SlicingEngine {
    pub config: EmergencySlicingConfig,
    pub exchange: Arc<dyn Exchange>,
}

impl SlicingEngine {
    pub fn new(config: EmergencySlicingConfig, exchange: Arc<dyn Exchange>) -> Self {
        Self { config, exchange }
    }

    /// Flatten `qty` units from a basket using reduce-only market slices.
    /// Follows the flow: read book depth → calc safe slice size → market slice → check slippage → continue until flat.
    pub async fn flatten(
        &self,
        basket_id: Uuid,
        side_to_close: Side,
        mut qty_remaining: f64,
        purpose: OrderPurpose,
    ) -> Result<f64> {
        if !self.config.enabled {
            // single shot
            let o = self
                .exchange
                .place_market_reduce_only(side_to_close, qty_remaining, basket_id, purpose)
                .await?;
            return Ok(o.price);
        }

        let mut attempts = 0u32;
        let mut last_price = 0.0;
        while qty_remaining > 1e-9 && attempts < self.config.max_slice_attempts {
            attempts += 1;
            let book = self.exchange.orderbook().await;
            // Read N levels of opposing book to size the slice.
            let levels = match side_to_close {
                Side::Sell => &book.bids, // closing long = selling into bids
                Side::Buy => &book.asks,
            };
            let depth: f64 = levels
                .iter()
                .take(self.config.book_depth_levels as usize)
                .map(|l| l.size)
                .sum();
            let participation_qty = depth * self.config.participation_rate;
            let slice = qty_remaining
                .min(self.config.max_slice_qty)
                .min(participation_qty.max(self.config.max_slice_qty * 0.1));

            let pre_mid = book.mid;
            let o = self
                .exchange
                .place_market_reduce_only(side_to_close, slice, basket_id, purpose)
                .await?;

            // Slippage check (bps from mid).
            let slip_bps = ((o.price - pre_mid).abs() / pre_mid) * 10_000.0;
            if slip_bps > self.config.max_slippage_bps {
                warn!(
                    slip_bps,
                    cap = self.config.max_slippage_bps,
                    "slippage exceeded cap on slice"
                );
            }
            qty_remaining -= slice;
            last_price = o.price;
            info!(
                attempts,
                slice,
                qty_remaining,
                slip_bps,
                "emergency slice sent"
            );
            sleep(Duration::from_millis(self.config.slice_delay_ms)).await;
        }
        if qty_remaining > 1e-6 {
            warn!(qty_remaining, "did not fully flatten within attempts");
        }
        Ok(last_price)
    }
}
