use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Exchange {
    Binance,
    Deribit,
    Hyperliquid,
    Mock,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradingConfig {
    pub token: String,
    pub exchange: Exchange,
    pub grid_lower: f64,
    pub grid_upper: f64,
    pub grid_step: f64,
    pub per_step_qty: f64,
    pub tp_spread: f64,
    pub maker_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BasketConfig {
    pub num_baskets: u32,
    pub basket_size_qty: f64,
    pub basket_sl_distance: f64,
}

impl BasketConfig {
    pub fn max_exposure(&self) -> f64 {
        self.num_baskets as f64 * self.basket_size_qty
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KillSwitchConfig {
    pub max_position_cap: f64,
    pub max_daily_loss: f64,
    pub api_disconnect_protection: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmergencySlicingConfig {
    pub enabled: bool,
    pub max_slice_qty: f64,
    pub slice_delay_ms: u64,
    pub max_slippage_bps: f64,
    pub book_depth_levels: u32,
    pub participation_rate: f64,
    pub max_slice_attempts: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub trading: TradingConfig,
    pub basket: BasketConfig,
    pub kill_switch: KillSwitchConfig,
    pub slicing: EmergencySlicingConfig,
}

impl AgentConfig {
    pub fn default_demo() -> Self {
        Self {
            trading: TradingConfig {
                token: "BTC-USDT".into(),
                exchange: Exchange::Mock,
                grid_lower: 60_000.0,
                grid_upper: 70_000.0,
                grid_step: 200.0,
                per_step_qty: 0.01,
                tp_spread: 150.0,
                maker_only: true,
            },
            basket: BasketConfig {
                num_baskets: 6,
                basket_size_qty: 0.05,
                basket_sl_distance: 800.0,
            },
            kill_switch: KillSwitchConfig {
                max_position_cap: 0.5,
                max_daily_loss: 500.0,
                api_disconnect_protection: true,
            },
            slicing: EmergencySlicingConfig {
                enabled: true,
                max_slice_qty: 0.02,
                slice_delay_ms: 200,
                max_slippage_bps: 30.0,
                book_depth_levels: 5,
                participation_rate: 0.2,
                max_slice_attempts: 10,
            },
        }
    }
}
