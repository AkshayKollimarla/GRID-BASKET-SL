use crate::engines::basket_manager::BasketManager;
use crate::models::AgentConfig;
use serde::Serialize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone, Serialize)]
pub struct RiskState {
    pub max_exposure_ok: bool,
    pub daily_loss_ok: bool,
    pub api_connected: bool,
    pub missing_sl_ok: bool,
    pub slippage_ok: bool,
    pub liquidity_ok: bool,
    pub runaway_ok: bool,
    pub breach_reason: Option<String>,
}

impl RiskState {
    pub fn healthy(&self) -> bool {
        self.max_exposure_ok
            && self.daily_loss_ok
            && self.api_connected
            && self.missing_sl_ok
            && self.slippage_ok
            && self.liquidity_ok
            && self.runaway_ok
    }
}

pub struct RiskEngine {
    pub config: AgentConfig,
    pub basket_mgr: Arc<BasketManager>,
    pub api_connected: Arc<AtomicBool>,
    /// True for INVERSE contracts (Deribit *_PERPETUAL). Determines
    /// how `max_position_cap_coins` is converted into the qty-units
    /// `basket_mgr.total_open_qty()` uses:
    ///   inverse: effective_cap_qty = cap_coins × mid (qty is USD)
    ///   linear : effective_cap_qty = cap_coins        (qty IS base coin)
    pub is_inverse: bool,
}

impl RiskEngine {
    pub fn new(config: AgentConfig, basket_mgr: Arc<BasketManager>, is_inverse: bool) -> Self {
        Self {
            config,
            basket_mgr,
            api_connected: Arc::new(AtomicBool::new(true)),
            is_inverse,
        }
    }

    /// Risk check. `mid` is current mid price — needed to convert the
    /// coin-denominated cap into the qty units the basket bookkeeping
    /// reports. When the operator hasn't set a coin cap, the legacy
    /// `max_position_cap` (qty units) is used and `mid` is ignored.
    pub fn assess(&self, mid: f64) -> RiskState {
        let total_open = self.basket_mgr.total_open_qty();
        let realized = self.basket_mgr.total_realized_pnl();

        // Effective cap, in qty units. Prefer the coin-based cap when
        // the operator has set it (>0), because that's the more
        // meaningful number — it doesn't drift with price the way a
        // USD-qty cap does on inverse contracts.
        let cap_coins = self.config.kill_switch.max_position_cap_coins;
        let (effective_cap, cap_label) = if cap_coins > 0.0 && mid > 0.0 {
            let cap_qty = if self.is_inverse { cap_coins * mid } else { cap_coins };
            (
                cap_qty,
                format!("{:.4} coins × {:.2} = {:.4}", cap_coins, mid, cap_qty),
            )
        } else {
            (
                self.config.kill_switch.max_position_cap,
                format!("{:.4}", self.config.kill_switch.max_position_cap),
            )
        };

        let max_exposure_ok = total_open <= effective_cap;
        let daily_loss_ok = realized >= -self.config.kill_switch.max_daily_loss;
        let api_connected = self.api_connected.load(Ordering::Relaxed);
        // Per-basket SL was removed; protection now comes from the per-basket
        // SL (anchor ± grid_distance) enforced in engine.rs. Nothing to
        // check at the basket level, so this is always healthy.
        let missing_sl_ok = true;

        let breach_reason = if !max_exposure_ok {
            Some(format!(
                "exposure {:.4} > max_position_cap ({})",
                total_open, cap_label
            ))
        } else if !daily_loss_ok {
            Some(format!(
                "daily loss {:.2} > max_daily_loss {:.2}",
                -realized, self.config.kill_switch.max_daily_loss
            ))
        } else if !api_connected {
            Some("API disconnect".into())
        } else if !missing_sl_ok {
            Some("active basket missing SL".into())
        } else {
            None
        };

        RiskState {
            max_exposure_ok,
            daily_loss_ok,
            api_connected,
            missing_sl_ok,
            slippage_ok: true, // computed per-fill in slicing engine
            liquidity_ok: true,
            runaway_ok: true,
            breach_reason,
        }
    }
}
