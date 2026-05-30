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
}

impl RiskEngine {
    pub fn new(config: AgentConfig, basket_mgr: Arc<BasketManager>) -> Self {
        Self {
            config,
            basket_mgr,
            api_connected: Arc::new(AtomicBool::new(true)),
        }
    }

    pub fn assess(&self) -> RiskState {
        let total_open = self.basket_mgr.total_open_qty();
        let realized = self.basket_mgr.total_realized_pnl();

        let max_exposure_ok = total_open <= self.config.kill_switch.max_position_cap;
        let daily_loss_ok = realized >= -self.config.kill_switch.max_daily_loss;
        let api_connected = self.api_connected.load(Ordering::Relaxed);

        // Per-basket SL was removed; protection now comes from the single
        // cycle SL (anchor ± grid_distance) enforced in engine.rs. Nothing
        // to check at the basket level, so this is always healthy.
        let missing_sl_ok = true;

        let breach_reason = if !max_exposure_ok {
            Some(format!(
                "exposure {:.4} > max_position_cap {:.4}",
                total_open, self.config.kill_switch.max_position_cap
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
