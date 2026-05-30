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
    /// Mock-only seed price + legacy fallback for absolute bounds when
    /// `grid_distance` isn't usable (no live mid). Not shown in the UI.
    #[serde(default)]
    pub grid_lower: f64,
    #[serde(default)]
    pub grid_upper: f64,
    /// Distance (in price units) from the initial mid that defines the
    /// ABSOLUTE hard cap on both sides. At engine start the bot reads the
    /// first live mid M and sets absolute_lower = M − distance,
    /// absolute_upper = M + distance. If price ever escapes that envelope
    /// the kill switch trips permanently.
    #[serde(default = "default_grid_distance")]
    pub grid_distance: f64,
    /// Spacing between grid levels (in price units).  UI label: "Average".
    pub grid_step: f64,
    /// Trailing depth: how many BUY levels below mid AND how many SELL
    /// levels above mid the grid maintains at any time. Cycle SL triggers
    /// fire at anchor ± (depth+1) × step.
    #[serde(default = "default_grid_depth")]
    pub grid_depth: u32,
    pub per_step_qty: f64,
    pub tp_spread: f64,
    pub maker_only: bool,
}

fn default_grid_depth() -> u32 {
    5
}
fn default_grid_distance() -> f64 {
    2_000.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BasketConfig {
    pub num_baskets: u32,
    pub basket_size_qty: f64,
    /// Legacy field; ignored. Per-basket SL was removed in favor of the
    /// single cycle SL controlled by grid_distance. Kept here with serde
    /// default so older saved configs still deserialize cleanly.
    #[serde(default)]
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
    /// Max number of soft boundary-SL cycle resets allowed before the bot
    /// permanently stops. Each "basket hit" = price escaped the cycle window
    /// (anchor ± (depth+1)·step), positions flattened, grid recentered.
    #[serde(default = "default_max_basket_hits")]
    pub max_basket_hits: u32,
}

fn default_max_basket_hits() -> u32 {
    5
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
    /// Human-friendly identifier so the user can save & reload configs
    /// without re-typing them. Editable in the UI. Persisted with the
    /// agent. Defaults to "Agent" on legacy configs that didn't have a
    /// name field.
    #[serde(default = "default_agent_name")]
    pub name: String,
    /// Unix epoch milliseconds — set every time the agent is started.
    /// The frontend sorts the Inactive sidebar list by this descending
    /// so the most-recently-used config appears first. 0 = never run.
    #[serde(default)]
    pub last_active_at: i64,
    /// Human-readable reason for the most recent stop. Set by the
    /// action endpoints (Stop/Kill/Force-Flatten) and by the engine
    /// main loop when it self-stops on all_killed. Empty when the
    /// agent has never run or is currently running. Surfaced on the
    /// Inactive sidebar card so the operator can see why each bot
    /// shut down at a glance.
    #[serde(default)]
    pub last_stop_reason: String,
    pub trading: TradingConfig,
    pub basket: BasketConfig,
    pub kill_switch: KillSwitchConfig,
    pub slicing: EmergencySlicingConfig,
}

fn default_agent_name() -> String {
    "Agent".into()
}

impl AgentConfig {
    pub fn default_demo() -> Self {
        Self {
            name: "Demo Agent".into(),
            last_active_at: 0,
            last_stop_reason: String::new(),
            trading: TradingConfig {
                token: "BTC-USDT".into(),
                exchange: Exchange::Mock,
                grid_lower: 60_000.0, // mock seed only
                grid_upper: 70_000.0, // mock seed only
                grid_distance: 5_000.0,
                grid_step: 200.0,
                grid_depth: 5,
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
                max_basket_hits: 5,
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
