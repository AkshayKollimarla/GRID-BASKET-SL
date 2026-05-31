use crate::engines::basket_manager::BasketManager;
use crate::engines::grid::GridEngine;
use crate::engines::kill_switch::KillSwitch;
use crate::engines::risk::{RiskEngine, RiskState};
use crate::engines::slicing::SlicingEngine;
use crate::engines::trade_tracker::TradeTracker;
use crate::exchanges::{DeribitClient, Exchange, HyperliquidClient, MockExchange};
use crate::models::{
    AgentConfig, Exchange as ExchangeKind, Fill, OrderPurpose, RoundTrip, Side, TradeStats,
};
use anyhow::{anyhow, Result};
use parking_lot::RwLock;
use serde::Serialize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use tokio::sync::broadcast;
use tokio::time::{sleep, Duration};
use tracing::warn;

#[derive(Debug, Clone, Serialize)]
pub struct EngineSnapshot {
    pub running: bool,
    pub kill_switch_tripped: bool,
    pub kill_switch_reason: Option<String>,
    pub mid_price: f64,
    pub total_open_qty: f64,
    pub total_realized_pnl: f64,
    pub baskets: Vec<crate::models::Basket>,
    pub open_orders: Vec<crate::models::Order>,
    pub recent_fills: Vec<crate::models::Fill>,
    pub risk: RiskState,
    pub log: Vec<String>,
    pub exchange_name: String,
    pub trade_stats: TradeStats,
    pub round_trips: Vec<RoundTrip>,
    /// Mid price at the moment the bot started. Fixed for the whole session.
    pub start_price: f64,
    /// Mid at the start of the CURRENT cycle (= grid center). Resets to
    /// current mid on every soft boundary-SL hit.
    pub cycle_anchor: f64,
    /// Lower SL trigger for the current cycle = anchor − grid_distance.
    pub cycle_lower: f64,
    /// Upper SL trigger for the current cycle = anchor + grid_distance.
    pub cycle_upper: f64,
    /// Configured cycle distance (=$ move that fires SL & recenters).
    pub grid_distance: f64,
    /// Number of completed cycle resets so far.
    pub basket_hits: u32,
    /// Configured max basket hits before permanent stop.
    pub max_basket_hits: u32,
    /// Live exchange position size (signed) for the configured instrument.
    /// + = long, − = short. Same units as per_step_qty.
    pub exchange_position: f64,
    /// Bot's tracked net qty (= buy_qty − sell_qty). Should match
    /// exchange_position; any drift means a fill was missed.
    pub bot_net_qty: f64,
    /// Absolute difference between bot's tracking and exchange — large
    /// values indicate desync.
    pub position_drift: f64,
    /// TPs currently parked off-exchange because the depth budget is
    /// full on their side. They will be re-placed when mid drifts back
    /// near their price AND a slot is free.
    pub parked_tp_count: usize,
    /// True when this snapshot was read from disk (the bot stopped
    /// and we're showing the frozen state). False = live snapshot
    /// from a running engine. The UI uses this to render a "FROZEN"
    /// badge so the operator can tell at a glance whether the data
    /// is live or post-mortem.
    #[serde(default)]
    pub frozen: bool,
}

/// One persisted history event — appended to `history/<agent>.jsonl`
/// as round trips and basket-SL kills happen. The 24h Summary endpoint
/// reads these files, filters by `ts`, and aggregates by
/// (exchange, token).
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct HistoryEvent {
    /// `rtp` or `basket_hit`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Unix epoch milliseconds.
    pub ts: i64,
    pub agent: String,
    pub exchange: String,
    pub token: String,
    /// Gross PnL for RTPs (signed); for basket_hit = realized PnL delta
    /// at the moment the basket was killed (usually negative).
    pub gross_pnl: f64,
    /// Total fees paid on this event (sum of entry + exit for RTPs).
    /// Can be negative if the exchange paid a maker rebate.
    pub fees: f64,
    /// USD volume traded on this event. For an RTP we record
    /// `2 × one_leg_notional` so the summary "buy+sell volume" is
    /// directly summable. For basket_hit = the qty closed × exit price.
    pub volume: f64,
    /// Round-trip qty (one leg) or basket-hit closed qty.
    pub qty: f64,
    pub basket_index: u32,
    /// "BUY" or "SELL" — the side of the ENTRY fill that opened this
    /// round trip. For an RTP, buy_vwap is computed from the buy-side
    /// price and sell_vwap from the sell-side price; for basket_hit
    /// this field is ignored.
    #[serde(default)]
    pub entry_side: String,
    /// Entry fill price (price at which the round trip's first leg was
    /// opened). For basket_hit this can be 0.
    #[serde(default)]
    pub entry_price: f64,
    /// Exit fill price (price at which the round trip's closing leg
    /// fired — fill_price ± tp_spread). For basket_hit = the market
    /// price at flatten time.
    #[serde(default)]
    pub exit_price: f64,
}

pub struct EngineHandle {
    pub config: AgentConfig,
    pub basket_mgr: Arc<BasketManager>,
    pub exchange: Arc<dyn Exchange>,
    pub grid: Arc<GridEngine>,
    pub risk: Arc<RiskEngine>,
    pub slicing: Arc<SlicingEngine>,
    pub kill_switch: Arc<KillSwitch>,
    pub trade_tracker: Arc<TradeTracker>,
    pub recent_fills: Arc<RwLock<Vec<Fill>>>,
    pub log: Arc<RwLock<Vec<String>>>,
    pub running: Arc<AtomicBool>,
    pub mid_price: Arc<RwLock<f64>>,
    /// Cycle anchor — the mid at the start of the current cycle. Grid is
    /// placed symmetrically around this. Resets to current mid on every soft
    /// boundary-SL hit.
    pub cycle_anchor: Arc<RwLock<f64>>,
    /// Mid at the moment the bot was started. Never changes after init.
    /// Used by the UI as the "started price" reference.
    pub start_price: Arc<RwLock<f64>>,
    /// Soft cycle resets that have happened so far.
    pub basket_hits: Arc<AtomicU32>,
    /// Last time we checked for position drift against the exchange.
    /// Rate-limited so get_position isn't called every 300ms tick.
    pub last_drift_check: Arc<RwLock<Option<std::time::Instant>>>,
    /// TPs that could not fit within the depth budget on placement and
    /// were "parked" locally (cancelled from the exchange, remembered
    /// here). When mid drifts back near their price, the engine
    /// re-places them. This is how we maintain depth=N orders per side
    /// even with many fills behind us — exactly like the HL bot's
    /// "3 target(s) parked" behaviour.
    pub parked_tps: Arc<RwLock<Vec<ParkedTp>>>,
}

#[derive(Debug, Clone)]
pub struct ParkedTp {
    pub basket_id: uuid::Uuid,
    pub side: Side,
    pub price: f64,
    pub qty: f64,
}

impl EngineHandle {
    pub async fn new(config: AgentConfig) -> Result<(Self, broadcast::Receiver<Fill>)> {
        // Network selection — defaults to mainnet for each exchange. Set
        // DERIBIT_TESTNET=true or HYPERLIQUID_TESTNET=true in .env to switch
        // that exchange to its testnet (separate API keys required).
        let env_flag = |key: &str| {
            std::env::var(key)
                .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
                .unwrap_or(false)
        };
        let deribit_mainnet = !env_flag("DERIBIT_TESTNET");
        let hyperliquid_mainnet = !env_flag("HYPERLIQUID_TESTNET");

        let (exchange_dyn, fills_rx): (Arc<dyn Exchange>, broadcast::Receiver<Fill>) =
            match config.trading.exchange {
                ExchangeKind::Mock => {
                    let initial_price =
                        (config.trading.grid_lower + config.trading.grid_upper) / 2.0;
                    let (ex, rx) = MockExchange::new(initial_price);
                    (ex, rx)
                }
                ExchangeKind::Deribit => {
                    let client_id = std::env::var("DERIBIT_CLIENT_ID")
                        .map_err(|_| anyhow!("DERIBIT_CLIENT_ID not set in .env"))?;
                    let client_secret = std::env::var("DERIBIT_CLIENT_SECRET")
                        .map_err(|_| anyhow!("DERIBIT_CLIENT_SECRET not set in .env"))?;
                    if client_id.is_empty() || client_secret.is_empty() {
                        return Err(anyhow!("Deribit credentials are empty"));
                    }
                    tracing::info!(
                        "Deribit network: {}",
                        if deribit_mainnet { "MAINNET" } else { "TESTNET" }
                    );
                    let (ex, rx) = DeribitClient::new(
                        client_id,
                        client_secret,
                        config.trading.token.clone(),
                        deribit_mainnet,
                    )
                    .await;
                    (ex, rx)
                }
                ExchangeKind::Hyperliquid => {
                    let pk = std::env::var("HYPERLIQUID_PRIVATE_KEY")
                        .map_err(|_| anyhow!("HYPERLIQUID_PRIVATE_KEY not set in .env"))?;
                    if pk.is_empty() {
                        return Err(anyhow!("HYPERLIQUID_PRIVATE_KEY is empty"));
                    }
                    let main_wallet = std::env::var("HYPERLIQUID_MAIN_WALLET").ok();
                    tracing::info!(
                        "Hyperliquid network: {}",
                        if hyperliquid_mainnet { "MAINNET" } else { "TESTNET" }
                    );
                    let (ex, rx) = HyperliquidClient::new(
                        pk,
                        main_wallet,
                        config.trading.token.clone(),
                        hyperliquid_mainnet,
                    )
                    .await?;
                    (ex, rx)
                }
                ExchangeKind::Binance => {
                    return Err(anyhow!(
                        "Binance connector not yet implemented. Use mock, deribit, or hyperliquid."
                    ))
                }
            };

        // Deribit perpetuals are INVERSE (amount in USD, PnL in BTC).
        // Hyperliquid + mock are LINEAR (amount in coin units).
        let is_inverse = matches!(config.trading.exchange, ExchangeKind::Deribit);
        tracing::info!(is_inverse, "Contract type for PnL math");

        // Clear any orphan orders before we start tracking new ones. Without
        // this, a leftover order from a previous session can fill on the
        // exchange without the bot ever knowing — the trade arrives in
        // get_user_trades for an order_id the bot never inserted into its
        // open_orders map, so the trade is dropped → bot's net qty doesn't
        // match the exchange's position. Wiping the book at startup
        // guarantees every subsequent fill is traceable.
        if let Err(e) = exchange_dyn.cancel_all().await {
            tracing::warn!(
                ?e,
                "cancel_all on startup failed — bot may misreport position if orphan orders exist on the exchange"
            );
        } else {
            tracing::info!("Cleared orphan orders at startup (clean slate)");
        }

        // Sanity check: max_position_cap must accommodate the configured
        // basket sizes, or the kill switch trips on the first fill.
        let expected_max_exposure =
            config.basket.num_baskets as f64 * config.basket.basket_size_qty;
        if config.kill_switch.max_position_cap < expected_max_exposure {
            tracing::warn!(
                max_position_cap = config.kill_switch.max_position_cap,
                expected_exposure = expected_max_exposure,
                num_baskets = config.basket.num_baskets,
                basket_size_qty = config.basket.basket_size_qty,
                "⚠ max_position_cap is below expected total exposure — kill switch will trip on the first fills. \
                 Increase max_position_cap to at least num_baskets × basket_size_qty."
            );
        }
        // Same warning for per_step_qty: each entry adds at least per_step_qty
        // to total_open_qty. If max_position_cap < per_step_qty, the FIRST
        // single fill exceeds the cap.
        if config.kill_switch.max_position_cap < config.trading.per_step_qty {
            tracing::error!(
                max_position_cap = config.kill_switch.max_position_cap,
                per_step_qty = config.trading.per_step_qty,
                "⚠ max_position_cap is smaller than per_step_qty — a single fill will instantly trip the kill switch. \
                 You almost certainly want max_position_cap > per_step_qty."
            );
        }

        let basket_mgr = Arc::new(BasketManager::new(
            config.basket.clone(),
            is_inverse,
            config.trading.tp_spread,
        ));
        let grid = Arc::new(GridEngine::new(
            config.clone(),
            basket_mgr.clone(),
            exchange_dyn.clone(),
        ));
        let risk = Arc::new(RiskEngine::new(
            config.clone(),
            basket_mgr.clone(),
            is_inverse,
        ));
        let slicing = Arc::new(SlicingEngine::new(
            config.slicing.clone(),
            exchange_dyn.clone(),
        ));
        let kill_switch = Arc::new(KillSwitch::new(
            basket_mgr.clone(),
            exchange_dyn.clone(),
            slicing.clone(),
        ));
        let trade_tracker = Arc::new(TradeTracker::new(
            basket_mgr.clone(),
            chrono::Utc::now().timestamp_millis(),
            is_inverse,
            config.trading.tp_spread,
        ));
        let handle = Self {
            config,
            basket_mgr,
            exchange: exchange_dyn,
            grid,
            risk,
            slicing,
            kill_switch,
            trade_tracker,
            recent_fills: Arc::new(RwLock::new(Vec::new())),
            log: Arc::new(RwLock::new(Vec::new())),
            running: Arc::new(AtomicBool::new(false)),
            mid_price: Arc::new(RwLock::new(0.0)),
            cycle_anchor: Arc::new(RwLock::new(0.0)),
            start_price: Arc::new(RwLock::new(0.0)),
            basket_hits: Arc::new(AtomicU32::new(0)),
            last_drift_check: Arc::new(RwLock::new(None)),
            parked_tps: Arc::new(RwLock::new(Vec::new())),
        };
        Ok((handle, fills_rx))
    }

    /// Emergency operator-triggered flush. Cancels every resting order,
    /// flattens every basket via slicing, then verifies the exchange-side
    /// position is actually zero — if residuals remain (typical drift
    /// scenario where the bot lost track of a fill) it places one final
    /// reduce-only market against the leftover. Bookkeeping is soft-reset
    /// on all baskets at the end. Safe to call while running or stopped.
    pub async fn force_flatten(&self) -> (bool, String) {
        self.log_line("FORCE FLATTEN requested by operator".to_string());

        // 1. Wipe resting orders so the residual flush below cannot get
        //    front-run by an old maker fill.
        if let Err(e) = self.exchange.cancel_all().await {
            self.log_line(format!("  force_flatten: cancel_all failed: {}", e));
        }
        // Also drop every parked TP — operator wants a clean slate.
        self.parked_tps.write().clear();

        // 2. Flatten every basket with open qty via slicing — direction
        //    determined by SIGN of net_qty (positive = net long, sell to
        //    close; negative = net short, buy to close).
        let to_flatten: Vec<(uuid::Uuid, f64, u32)> = self
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
        for (bid, net_qty, idx) in to_flatten {
            let close_side = if net_qty > 0.0 { Side::Sell } else { Side::Buy };
            let qty = net_qty.abs();
            match self
                .slicing
                .flatten(bid, close_side, qty, OrderPurpose::KillSwitchExit)
                .await
            {
                Ok(px) => self.log_line(format!(
                    "  force_flatten: basket#{} flattened @ {:.2}",
                    idx, px
                )),
                Err(e) => self.log_line(format!(
                    "  force_flatten: basket#{} flatten FAILED ({}) — will rely on residual flush",
                    idx, e
                )),
            }
            if let Some(mut b) = self.basket_mgr.baskets.get_mut(&bid) {
                let exit_px = *self.mid_price.read();
                b.soft_reset(exit_px);
            }
        }

        // 3. Residual flush — read the exchange's actual position. If it's
        //    still non-zero, the bot's bookkeeping missed a fill (the very
        //    reason the operator pressed the button). Slam it shut with one
        //    reduce-only market in the opposite direction.
        let xpos = self.exchange.position().await;
        if xpos.abs() > 0.5 {
            self.log_line(format!(
                "  force_flatten: residual exchange position {:.2} after basket flatten — placing residual flush",
                xpos
            ));
            // Pick any basket_id for bookkeeping (the residual qty is by
            // definition NOT in any basket — pick the first basket so the
            // fill at least lands in our reconciliation log).
            let any_basket_id = self
                .basket_mgr
                .baskets
                .iter()
                .next()
                .map(|e| *e.key())
                .unwrap_or_else(uuid::Uuid::new_v4);
            let close_side = if xpos > 0.0 { Side::Sell } else { Side::Buy };
            let qty = xpos.abs();
            match self
                .exchange
                .place_market_reduce_only(
                    close_side,
                    qty,
                    any_basket_id,
                    OrderPurpose::KillSwitchExit,
                )
                .await
            {
                Ok(o) => self.log_line(format!(
                    "  force_flatten: residual flush {:?} {:.2} @ {:.2}",
                    close_side, qty, o.price
                )),
                Err(e) => self.log_line(format!(
                    "  force_flatten: residual flush FAILED: {} — close manually on the exchange",
                    e
                )),
            }
        }

        // 4. Verify zero. Small loop to give the exchange a moment to settle.
        let mut residual = 0.0;
        for _ in 0..4 {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            residual = self.exchange.position().await;
            if residual.abs() <= 0.5 {
                break;
            }
        }
        if residual.abs() <= 0.5 {
            self.log_line(format!(
                "  force_flatten: VERIFIED FLAT — exchange position {:.2}",
                residual
            ));
            (true, format!("flat — exchange position {:.2}", residual))
        } else {
            self.log_line(format!(
                "  force_flatten: STILL NOT FLAT — exchange position {:.2}. CLOSE MANUALLY.",
                residual
            ));
            (
                false,
                format!(
                    "residual position {:.2} remains — close manually on the exchange",
                    residual
                ),
            )
        }
    }

    pub fn log_line(&self, line: impl Into<String>) {
        let line = line.into();
        let mut g = self.log.write();
        g.push(format!(
            "[{}] {}",
            chrono::Utc::now().format("%H:%M:%S"),
            line
        ));
        if g.len() > 200 {
            let drop = g.len() - 200;
            g.drain(0..drop);
        }
    }

    /// Sanitize an agent name for use as a filename. Whitespace becomes
    /// '_', and only ASCII alphanumerics + a few safe punctuation
    /// characters survive. Empty result falls back to "agent".
    pub fn history_filename(name: &str) -> String {
        let mut s = String::new();
        for ch in name.chars() {
            if ch.is_ascii_alphanumeric() {
                s.push(ch);
            } else if ch.is_whitespace() {
                s.push('_');
            } else if matches!(ch, '-' | '_' | '.') {
                s.push(ch);
            }
        }
        if s.is_empty() {
            s.push_str("agent");
        }
        format!("{}.jsonl", s)
    }

    /// Append one history event to `history/<sanitized agent name>.jsonl`.
    /// Best-effort: I/O errors are logged at WARN and the engine keeps
    /// going (we never want the disk to take down the trading loop).
    async fn append_history(&self, ev: HistoryEvent) {
        let _ = tokio::fs::create_dir_all("history").await;
        let path = format!("history/{}", Self::history_filename(&self.config.name));
        let line = match serde_json::to_string(&ev) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(?e, "could not serialize history event");
                return;
            }
        };
        match tokio::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .await
        {
            Ok(mut f) => {
                use tokio::io::AsyncWriteExt;
                let payload = format!("{}\n", line);
                if let Err(e) = f.write_all(payload.as_bytes()).await {
                    tracing::warn!(?e, ?path, "history write failed");
                }
            }
            Err(e) => tracing::warn!(?e, ?path, "history open failed"),
        }
    }

    /// Persist one round trip. Called from process_fill right after
    /// trade_tracker.ingest returns the new RTPs.
    pub async fn record_rtp(&self, rt: &RoundTrip) {
        let ev = HistoryEvent {
            kind: if rt.is_take_profit { "rtp" } else { "sl_rtp" }.into(),
            ts: rt.exit_time,
            agent: self.config.name.clone(),
            exchange: format!("{:?}", self.config.trading.exchange).to_lowercase(),
            token: self.config.trading.token.clone(),
            gross_pnl: rt.gross_pnl,
            fees: rt.fees,
            // For "buy+sell volume" math, an RTP is one buy leg + one sell
            // leg of equal qty → double the one-leg notional that
            // trade_tracker recorded.
            volume: rt.volume * 2.0,
            qty: rt.qty,
            basket_index: rt.basket_index,
            entry_side: format!("{:?}", rt.entry_side).to_uppercase(),
            entry_price: rt.entry_price,
            exit_price: rt.exit_price,
        };
        self.append_history(ev).await;
    }

    /// Persist one basket-hit event. Called from check_basket_boundaries
    /// right after a basket is killed by its own SL. `pnl_delta` is the
    /// realized-PnL change booked by `kill()` (usually negative).
    pub async fn record_basket_hit(
        &self,
        basket_index: u32,
        qty_closed: f64,
        exit_price: f64,
        pnl_delta: f64,
    ) {
        let ev = HistoryEvent {
            kind: "basket_hit".into(),
            ts: chrono::Utc::now().timestamp_millis(),
            agent: self.config.name.clone(),
            exchange: format!("{:?}", self.config.trading.exchange).to_lowercase(),
            token: self.config.trading.token.clone(),
            gross_pnl: pnl_delta,
            fees: 0.0,
            volume: qty_closed * exit_price.max(0.0),
            qty: qty_closed,
            basket_index,
            entry_side: String::new(),
            entry_price: 0.0,
            exit_price,
        };
        self.append_history(ev).await;
    }

    pub async fn snapshot(&self) -> EngineSnapshot {
        // Do all awaits BEFORE taking any parking_lot guards.
        let open_orders = self.exchange.open_orders().await;
        let exchange_name = self.exchange.name().await.to_string();
        let mid_for_risk = *self.mid_price.read();
        let risk = self.risk.assess(mid_for_risk);
        let now_ms = chrono::Utc::now().timestamp_millis();
        let trade_stats = self.trade_tracker.stats(now_ms);
        let round_trips = self.trade_tracker.recent_round_trips(200);
        let anchor = *self.cycle_anchor.read();
        let start_price = *self.start_price.read();
        let distance = self.config.trading.grid_distance.max(0.0);
        let cycle_lower = if anchor > 0.0 { anchor - distance } else { 0.0 };
        let cycle_upper = if anchor > 0.0 { anchor + distance } else { 0.0 };
        // Pull the live exchange position for drift detection. Bot's net qty
        // (long basket open_qty - short basket open_qty) should match this.
        let exchange_position = self.exchange.position().await;
        // Bot net = signed sum of each basket's net_qty.
        let bot_net_qty: f64 = self
            .basket_mgr
            .baskets
            .iter()
            .map(|e| e.value().net_qty)
            .sum();
        let position_drift = (exchange_position - bot_net_qty).abs();
        let parked_tp_count = self.parked_tps.read().len();
        EngineSnapshot {
            running: self.running.load(Ordering::Relaxed),
            kill_switch_tripped: self.kill_switch.is_tripped(),
            kill_switch_reason: self.kill_switch.reason(),
            mid_price: *self.mid_price.read(),
            total_open_qty: self.basket_mgr.total_open_qty(),
            total_realized_pnl: self.basket_mgr.total_realized_pnl(),
            baskets: self.basket_mgr.all(),
            open_orders,
            recent_fills: self.recent_fills.read().clone(),
            risk,
            log: self.log.read().clone(),
            exchange_name,
            trade_stats,
            round_trips,
            start_price,
            cycle_anchor: anchor,
            cycle_lower,
            cycle_upper,
            grid_distance: distance,
            basket_hits: self.basket_hits.load(Ordering::Relaxed),
            max_basket_hits: self.config.kill_switch.max_basket_hits,
            exchange_position,
            bot_net_qty,
            position_drift,
            parked_tp_count,
            frozen: false,
        }
    }

    /// Persist the current snapshot to `snapshots/<sanitized>.json` so
    /// it can be reviewed after the bot stops. Called from the action
    /// endpoints (stop, kill, force_flatten) and from the GC in
    /// `list_agents` right before a stale engine is removed, and from
    /// the main tick loop when the engine self-stops via all_killed.
    /// Best-effort: I/O errors logged at WARN and the engine keeps
    /// going.
    pub async fn save_final_snapshot(&self) {
        let snap = self.snapshot().await;
        let _ = tokio::fs::create_dir_all("snapshots").await;
        let base = Self::history_filename(&self.config.name);
        // history_filename returns "<sanitized>.jsonl"; swap to .json.
        let fname = base.trim_end_matches(".jsonl");
        let path = format!("snapshots/{}.json", fname);
        match serde_json::to_string_pretty(&snap) {
            Ok(s) => {
                if let Err(e) = tokio::fs::write(&path, s).await {
                    tracing::warn!(?e, ?path, "save_final_snapshot write failed");
                } else {
                    tracing::info!(?path, "final snapshot frozen to disk");
                }
            }
            Err(e) => tracing::warn!(?e, "serialize snapshot failed"),
        }
    }

    /// Path to where the frozen snapshot for `agent_name` would live.
    /// Used by the API layer to read back when no live engine exists.
    pub fn snapshot_file_path(agent_name: &str) -> String {
        let base = Self::history_filename(agent_name);
        let fname = base.trim_end_matches(".jsonl");
        format!("snapshots/{}.json", fname)
    }
}

/// Start the engine: spawns the main trading loop and the fill processor.
pub fn spawn_engine(handle: Arc<EngineHandle>, mut fills_rx: broadcast::Receiver<Fill>) {
    handle.running.store(true, Ordering::Relaxed);
    let name = handle.config.trading.exchange;
    handle.log_line(format!(
        "Engine started — exchange={:?}, {} baskets, exposure cap {:.4}",
        name, handle.config.basket.num_baskets, handle.config.kill_switch.max_position_cap
    ));

    // Fill processor — assigns fills to baskets, places TPs, triggers basket SLs.
    let h = handle.clone();
    tokio::spawn(async move {
        loop {
            match fills_rx.recv().await {
                Ok(fill) => {
                    tracing::info!(
                        order_id = %fill.order_id,
                        side = ?fill.side,
                        purpose = ?fill.purpose,
                        price = fill.price,
                        qty = fill.qty,
                        "PROCESS_FILL received fill — updating basket bookkeeping"
                    );
                    process_fill(&h, fill).await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    // CRITICAL: receiver fell behind, fills were dropped by
                    // the broadcast channel. Loop to recover, but log loudly
                    // so the operator knows about it.
                    tracing::error!(
                        missed = n,
                        "⚠ FILL RECEIVER LAGGED — {} fills dropped by the broadcast channel. \
                         These will NEVER be reflected in basket bookkeeping. \
                         The position drift detector will catch the resulting mismatch.",
                        n
                    );
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    tracing::error!("fills_tx channel closed — no more fills will be processed");
                    break;
                }
            }
        }
    });

    // Main loop — orderbook + grid + risk.
    let h = handle.clone();
    tokio::spawn(async move {
        // Heartbeat: every 30s, log "tick alive" so a silent stall in the
        // tick loop is immediately visible. Previously the loop could hang
        // (e.g. a parking_lot guard deadlock, a hung HTTP call past the
        // reqwest timeout) and we'd only notice via missing fills hours
        // later. The heartbeat also serves as a "last seen" marker for
        // post-mortem debugging.
        let mut last_heartbeat = std::time::Instant::now();
        let mut tick_iterations: u64 = 0;
        loop {
            if !h.running.load(Ordering::Relaxed) {
                break;
            }
            tick_iterations += 1;
            if last_heartbeat.elapsed().as_secs() >= 30 {
                tracing::info!(
                    iterations = tick_iterations,
                    mid = *h.mid_price.read(),
                    "tick heartbeat — loop alive"
                );
                last_heartbeat = std::time::Instant::now();
            }
            // 1. Tick exchange (mock advances price; real exchanges poll for fills).
            h.exchange.tick().await;

            // 2. Update cached mid price from the orderbook.
            let book = h.exchange.orderbook().await;
            if book.mid > 0.0 {
                *h.mid_price.write() = book.mid;
                // First-time anchor + start-price initialization.
                let mut anchor = h.cycle_anchor.write();
                if *anchor <= 0.0 {
                    *anchor = book.mid;
                    *h.start_price.write() = book.mid;
                    let dist = h.config.trading.grid_distance.max(0.0);
                    h.log_line(format!(
                        "Bot started — price={:.2}, cycle SL ±{:.2} → [{:.2}, {:.2}]",
                        book.mid,
                        dist,
                        book.mid - dist,
                        book.mid + dist
                    ));
                    // Anchor the FIRST basket at startup mid so its SL
                    // range is defined immediately. Without this, the
                    // first basket sits IDLE with anchor=0 until its
                    // first fill arrives — which may be at a price
                    // already drifted from the operator's "start price".
                    if let Some(first_id) = h.basket_mgr.activate_next_idle() {
                        if let Some(mut nb) = h.basket_mgr.baskets.get_mut(&first_id) {
                            nb.set_sl_anchor(book.mid, dist);
                            let nidx = nb.index;
                            let nupper = nb.upper_sl;
                            let nlower = nb.lower_sl;
                            drop(nb);
                            h.log_line(format!(
                                "  basket#{} anchored @ {:.2} → SL range [{:.2}, {:.2}]",
                                nidx, book.mid, nlower, nupper
                            ));
                        }
                    }
                }
            }

            // 3. Risk engine assessment.
            let risk = h.risk.assess(*h.mid_price.read());
            if !risk.healthy() && !h.kill_switch.is_tripped() {
                let reason = risk.breach_reason.clone().unwrap_or("unknown".into());
                h.log_line(format!("Risk breach: {} — tripping kill switch", reason));
                h.kill_switch.trip(reason).await;
            }

            // 4. Per-basket SL check (replaces the global cycle SL).
            //    Each basket has its own anchor + upper/lower limits, set
            //    on its first entry fill. When mid crosses a basket's own
            //    bounds AND that basket has open_qty, ONLY that basket
            //    is flattened + permanently KILLED. Other baskets keep
            //    trading. When ALL baskets are KILLED → bot stops.
            check_basket_boundaries(&h).await;

            // 5. Grid placement (skipped if killed). Pass the cycle anchor
            //    directly — the grid is ANCHORED, not trailing mid, so each
            //    price level keeps its buy/sell role for the whole cycle.
            if !h.kill_switch.is_tripped() && !h.basket_mgr.all_killed() {
                let anchor = *h.cycle_anchor.read();
                let dist = h.config.trading.grid_distance.max(0.0);
                if anchor > 0.0 && dist > 0.0 {
                    h.grid.step(anchor, dist).await;
                }
                // 5b. Un-park any TPs that are now near current mid AND
                //     have free side-slots on the exchange. This is the
                //     other half of the parking mechanism — TPs return
                //     to the book as price drifts back to them.
                unpark_eligible_tps(&h).await;
                // Surface the grid's summary lines (e.g.
                // "Grid: 3 above + 3 below = 6 (5 entries, 1 targets)")
                // in the bot status log so the user can see the grid
                // composition change tick-by-tick.
                for line in h.grid.take_pending_log() {
                    h.log_line(line);
                }
            }

            // 5b. Position drift check. Compare bot's tracked net qty with
            //     the exchange's actual position. Any persistent mismatch
            //     means a fill was missed — log loudly so we can diagnose.
            //     Only do this every ~3 seconds to avoid burning the rate
            //     limit on get_position.
            let now = std::time::Instant::now();
            let last = h.last_drift_check.read().clone();
            let should_check = match last {
                None => true,
                Some(t) => now.duration_since(t).as_millis() > 3_000,
            };
            if should_check {
                *h.last_drift_check.write() = Some(now);
                let xpos = h.exchange.position().await;
                let bot_net: f64 = h
                    .basket_mgr
                    .baskets
                    .iter()
                    .map(|e| e.value().net_qty)
                    .sum();
                let drift = (xpos - bot_net).abs();
                // 0.5 unit tolerance covers floating-point noise.
                if drift > 0.5 {
                    tracing::error!(
                        bot_net_qty = bot_net,
                        exchange_position = xpos,
                        drift,
                        "⚠ POSITION DRIFT — bot bookkeeping does not match exchange. A fill was likely missed."
                    );
                    h.log_line(format!(
                        "⚠ Position drift: bot tracks {:.2}, exchange shows {:.2} (drift {:.2})",
                        bot_net, xpos, drift
                    ));
                }
            }

            // 6. Bot stops when all baskets killed.
            //    Also trip the kill switch explicitly so the UI top-bar
            //    shows "KILLED" instead of "IDLE" — this state IS a
            //    terminal kill (every per-basket SL fired in sequence,
            //    exactly the cascade case the user described).
            if h.basket_mgr.all_killed() {
                if !h.kill_switch.is_tripped() {
                    h.kill_switch
                        .trip(format!(
                            "all {} baskets killed by sequential per-basket SLs",
                            h.basket_mgr.baskets.len()
                        ))
                        .await;
                }
                h.log_line("All baskets killed — bot stopped.".to_string());
                // Freeze the FINAL state to disk before exiting the
                // loop so the operator can review it from the sidebar.
                h.save_final_snapshot().await;
                h.running.store(false, Ordering::Relaxed);
                break;
            }
            // Faster sync — 300ms (was 500). Fills get detected ~40% sooner.
            sleep(Duration::from_millis(300)).await;
        }
    });
}

async fn process_fill(h: &EngineHandle, fill: Fill) {
    // First, feed the trade tracker so cumulative stats + round-trip pairing
    // include this fill before anything else acts on it. Any new round
    // trips created by this fill are returned so we can persist them to
    // the agent's per-coin history log for the 24h summary.
    let new_rtps = h.trade_tracker.ingest(&fill);
    for rt in &new_rtps {
        h.record_rtp(rt).await;
    }
    {
        let mut g = h.recent_fills.write();
        g.push(fill.clone());
        // Keep the last 1000 fills so the Trade History panel can show ALL
        // individual trades, not just the freshest handful.
        if g.len() > 1000 {
            let drop = g.len() - 1000;
            g.drain(0..drop);
        }
    }
    match fill.purpose {
        OrderPurpose::Entry => {
            // ── 1. Update basket bookkeeping ────────────────────────────
            // First entry fill on a basket → set its OWN SL anchor at this
            // price. The basket's upper_sl / lower_sl bounds are fixed for
            // the basket's lifetime. When mid crosses those bounds AND
            // the basket has open_qty, ONLY this basket gets killed —
            // other baskets keep trading.
            let distance = h.config.trading.grid_distance.max(0.0);
            let (basket_idx, net_qty, avg_price, just_activated, upper, lower) = {
                if let Some(mut b) = h.basket_mgr.baskets.get_mut(&fill.basket_id) {
                    let was_unactivated = b.anchor_price <= 0.0;
                    b.apply_entry_fill(fill.qty, fill.price, fill.side);
                    if was_unactivated {
                        b.set_sl_anchor(fill.price, distance);
                    }
                    (b.index, b.net_qty, b.avg_price, was_unactivated, b.upper_sl, b.lower_sl)
                } else {
                    return;
                }
            };
            if just_activated {
                h.log_line(format!(
                    "  basket#{} SL anchored @ {:.2} → SL range [{:.2}, {:.2}]",
                    basket_idx, fill.price, lower, upper
                ));
            }
            h.log_line(format!(
                "ENTRY {:?} basket#{} qty={:.4} px={:.2} net={:.4} avg={:.2}",
                fill.side, basket_idx, fill.qty, fill.price, net_qty, avg_price
            ));

            // ── 2. Compute TP price at EXACT fill_price ± tp_spread ─────
            // Per-fill TP: TP side is OPPOSITE of the entry side. A BUY
            // entry placed at price X is covered by a SELL TP at
            // X + tp_spread (closes a long position). A SELL entry at X
            // is covered by a BUY TP at X − tp_spread (closes a short).
            let tp_spread = h.config.trading.tp_spread;
            let (tp_side, tp_price) = match fill.side {
                Side::Buy => (Side::Sell, fill.price + tp_spread),
                Side::Sell => (Side::Buy, fill.price - tp_spread),
            };

            // Skip if the TP would be a taker right now (price on wrong side).
            let mid = *h.mid_price.read();
            let valid_maker = match tp_side {
                Side::Buy => tp_price < mid,
                Side::Sell => tp_price > mid,
            };
            if !valid_maker {
                warn!(
                    tp_price,
                    mid,
                    ?tp_side,
                    "skipping TP placement — would be a taker, basket protected by cycle SL"
                );
                return;
            }

            // ── 3a. Multiple entries at the SAME price are legitimate —
            //       the grid refills slots after each fill, so two
            //       successive fills at 2016.50 each need their own
            //       coverage TP at 2016.00. The previous version of
            //       this code de-duped them and dropped the 2nd TP,
            //       leaving the position uncovered. Now we always
            //       place a TP per entry fill; Deribit accepts multiple
            //       orders at the same price, and the depth budget +
            //       parking logic below handles overflow correctly.
            //
            //       Race-condition protection (same fill processed twice)
            //       is already guaranteed by deribit.rs's
            //       `processed_trade_ids` dedup, so we don't need a
            //       second layer here.
            let depth = h.config.trading.grid_depth.max(1) as usize;
            let open = h.exchange.open_orders().await;

            // ── 3b. Make room: if target side already has `depth` orders,
            //       cancel the FURTHEST ENTRY on that side first (entries
            //       are cheap to replace; TPs are positions). If there
            //       are no entries to evict but the side is still full,
            //       fall back to TP-PARKING: compare the new TP to the
            //       furthest existing TP, keep the one CLOSER to mid on
            //       the exchange, and park the other. Parked TPs are
            //       re-placed by `unpark_eligible_tps` later when price
            //       drifts back near them.
            let side_count = open.iter().filter(|o| o.side == tp_side).count();
            let mut new_tp_should_park = false;
            if side_count >= depth {
                let victim_entry = open
                    .iter()
                    .filter(|o| {
                        o.side == tp_side && matches!(o.purpose, OrderPurpose::Entry)
                    })
                    .max_by(|a, b| {
                        (a.price - mid)
                            .abs()
                            .partial_cmp(&(b.price - mid).abs())
                            .unwrap_or(std::cmp::Ordering::Equal)
                    });
                if let Some(v) = victim_entry {
                    let _ = h.exchange.cancel(v.order_id).await;
                    h.log_line(format!(
                        "  cancelled furthest entry {:?}@{:.2} to make room for TP",
                        v.side, v.price
                    ));
                } else {
                    // Only TPs on this side — find the furthest one.
                    let furthest_tp = open
                        .iter()
                        .filter(|o| {
                            o.side == tp_side
                                && matches!(o.purpose, OrderPurpose::TakeProfit)
                        })
                        .max_by(|a, b| {
                            (a.price - mid)
                                .abs()
                                .partial_cmp(&(b.price - mid).abs())
                                .unwrap_or(std::cmp::Ordering::Equal)
                        });
                    let new_distance = (tp_price - mid).abs();
                    match furthest_tp {
                        Some(ftp) if (ftp.price - mid).abs() > new_distance => {
                            // Furthest existing TP is farther than this one
                            // → park IT, cancel from exchange, place the new
                            // closer-to-mid TP in its slot.
                            let _ = h.exchange.cancel(ftp.order_id).await;
                            h.parked_tps.write().push(ParkedTp {
                                basket_id: ftp.basket_id,
                                side: ftp.side,
                                price: ftp.price,
                                qty: ftp.qty,
                            });
                            h.log_line(format!(
                                "  parked far TP {:?}@{:.2} qty={:.4} to make room for closer TP",
                                ftp.side, ftp.price, ftp.qty
                            ));
                        }
                        _ => {
                            // New TP is at-least-as-far as every existing TP.
                            // Don't place it; park it instead.
                            new_tp_should_park = true;
                        }
                    }
                }
            }

            // ── 4. Place the TP order (or park it if we chose to) ─────
            if new_tp_should_park {
                h.parked_tps.write().push(ParkedTp {
                    basket_id: fill.basket_id,
                    side: tp_side,
                    price: tp_price,
                    qty: fill.qty,
                });
                h.log_line(format!(
                    "  TP @ {:.2} qty={:.4} PARKED (side full, this TP is farthest) — will re-place when mid returns",
                    tp_price, fill.qty
                ));
            } else {
                match h
                    .exchange
                    .place_maker_only(
                        tp_side,
                        tp_price,
                        fill.qty,
                        fill.basket_id,
                        OrderPurpose::TakeProfit,
                    )
                    .await
                {
                    Ok(order) => {
                        h.basket_mgr.link_order(order.order_id, fill.basket_id);
                        h.log_line(format!(
                            "  TP placed {:?}@{:.2} qty={:.4} (for basket#{})",
                            tp_side, tp_price, fill.qty, basket_idx
                        ));
                    }
                    Err(e) => warn!(?e, tp_price, "TP placement failed"),
                }
            }
        }
        OrderPurpose::TakeProfit => {
            if let Some(mut b) = h.basket_mgr.baskets.get_mut(&fill.basket_id) {
                b.apply_tp_fill(fill.qty, fill.price, fill.side);
                h.log_line(format!(
                    "TP filled basket#{} {:?}@{:.2} qty={:.4} net={:.4} realized={:.2}",
                    b.index, fill.side, fill.price, fill.qty, b.net_qty, b.realized_pnl
                ));
            }
        }
        OrderPurpose::StopLossExit | OrderPurpose::KillSwitchExit => {}
    }
}

/// PER-BASKET SL boundary check. Each basket has its OWN anchor and
/// upper/lower SL limits, set when the basket's first entry fill arrives
/// (in process_fill). This function scans every basket each tick:
///   • For any basket whose mid has crossed its OWN bounds AND that
///     still has open_qty: flatten that basket's position via slicing
///     and mark it permanently KILLED (not soft-reset).
///   • Cancel any resting orders that belong to the killed basket so
///     they don't reopen on the wrong side later.
///   • Increment basket_hits.
/// When ALL baskets are KILLED, the main loop notices and trips the
/// kill switch (existing logic). Per the user's spec:
///   "1st basket fills + hits limit → killed; 2nd basket takes over;
///    once all N baskets are killed, bot stops."
async fn check_basket_boundaries(h: &EngineHandle) {
    if h.kill_switch.is_tripped() {
        return;
    }
    let mid = *h.mid_price.read();
    if mid <= 0.0 {
        return;
    }

    // Snapshot the baskets whose SL has fired. Doing this in a separate
    // pass avoids holding DashMap guards across the await for slicing.
    let breached: Vec<(uuid::Uuid, f64, u32, f64, f64, f64)> = h
        .basket_mgr
        .baskets
        .iter()
        .filter_map(|e| {
            let b = e.value();
            if b.sl_breached(mid) {
                Some((
                    b.basket_id,
                    b.net_qty,
                    b.index,
                    b.anchor_price,
                    b.upper_sl,
                    b.lower_sl,
                ))
            } else {
                None
            }
        })
        .collect();

    if breached.is_empty() {
        return;
    }

    for (bid, net_qty, idx, anchor, upper, lower) in breached {
        let which_bound = if mid >= upper { "UPPER" } else { "LOWER" };
        h.log_line(format!(
            "BASKET SL — basket#{} {} bound hit (mid={:.2}, anchor={:.2}, range=[{:.2}, {:.2}], net={:.4})",
            idx, which_bound, mid, anchor, lower, upper, net_qty
        ));

        // Cancel any resting orders that belong to THIS basket so they
        // don't reopen positions while the slicer is flattening.
        let to_cancel: Vec<uuid::Uuid> = h
            .exchange
            .open_orders()
            .await
            .into_iter()
            .filter(|o| o.basket_id == bid)
            .map(|o| o.order_id)
            .collect();
        for oid in to_cancel {
            let _ = h.exchange.cancel(oid).await;
        }

        // Flatten via emergency slicing. Direction depends on the SIGN
        // of net_qty: net long → SELL to close; net short → BUY to close.
        let close_side = if net_qty > 0.0 { Side::Sell } else { Side::Buy };
        let abs_qty = net_qty.abs();
        let exit_px = if abs_qty > 1e-9 {
            match h
                .slicing
                .flatten(bid, close_side, abs_qty, OrderPurpose::StopLossExit)
                .await
            {
                Ok(px) => {
                    h.log_line(format!(
                        "  basket#{} flattened {:?} {:.4} @ {:.2}",
                        idx, close_side, abs_qty, px
                    ));
                    px
                }
                Err(e) => {
                    h.log_line(format!(
                        "  CRITICAL: basket#{} flatten FAILED ({}). Marking KILLED anyway — verify exchange manually.",
                        idx, e
                    ));
                    mid
                }
            }
        } else {
            mid
        };

        let pnl_before = h
            .basket_mgr
            .baskets
            .get(&bid)
            .map(|b| b.realized_pnl)
            .unwrap_or(0.0);
        if let Some(mut b) = h.basket_mgr.baskets.get_mut(&bid) {
            b.kill(exit_px);
        }
        let pnl_after = h
            .basket_mgr
            .baskets
            .get(&bid)
            .map(|b| b.realized_pnl)
            .unwrap_or(pnl_before);
        let pnl_delta = pnl_after - pnl_before;
        h.record_basket_hit(idx, net_qty.abs(), exit_px, pnl_delta).await;

        // Drop any TPs still parked for this basket — the position is
        // gone, so the saved closing orders would re-open positions if
        // they ever un-parked.
        h.parked_tps.write().retain(|p| p.basket_id != bid);

        // Promote the next IDLE basket AND immediately anchor it at
        // current mid so its SL range is defined from THIS moment, not
        // when its first fill happens to arrive. The user's spec:
        // "if the basket hit, the trading should start from the live
        //  anchor price till the basket count hit." Without this the
        // new basket would sit IDLE with anchor=0 until its first
        // entry filled — which might be some time later at a price
        // that's already drifted from the SL trigger point.
        let next = h.basket_mgr.activate_next_idle();
        if let Some(nid) = next {
            let distance = h.config.trading.grid_distance.max(0.0);
            if let Some(mut nb) = h.basket_mgr.baskets.get_mut(&nid) {
                nb.set_sl_anchor(mid, distance);
                let nidx = nb.index;
                let nupper = nb.upper_sl;
                let nlower = nb.lower_sl;
                drop(nb);
                h.log_line(format!(
                    "  basket#{} promoted to ACTIVE @ {:.2} → SL range [{:.2}, {:.2}]",
                    nidx, mid, nlower, nupper
                ));
            }
        } else {
            h.log_line("  no more baskets available — bot will stop".to_string());
        }

        // basket_hits counter = number of permanently-killed baskets.
        let prev = h.basket_hits.fetch_add(1, Ordering::Relaxed);
        let new_hits = prev + 1;
        let total = h.config.basket.num_baskets;
        h.log_line(format!(
            "Basket hits: {}/{} ({} basket{} killed)",
            new_hits,
            total,
            new_hits,
            if new_hits == 1 { "" } else { "s" }
        ));
    }

    // Post-flatten residual mop-up: ensure the exchange position is
    // actually zero after slicing, otherwise place one reduce-only
    // market against whatever's left. Bot net is now signed net_qty.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let xpos = h.exchange.position().await;
    let bot_net: f64 = h.basket_mgr.baskets.iter().map(|e| e.value().net_qty).sum();
    let residual = xpos - bot_net;
    if residual.abs() > 0.5 {
        h.log_line(format!(
            "  post-SL residual {:.2} (exchange {:.2} vs bot {:.2}) — placing reduce-only mop-up",
            residual, xpos, bot_net
        ));
        let any_basket_id = h
            .basket_mgr
            .baskets
            .iter()
            .next()
            .map(|e| *e.key())
            .unwrap_or_else(uuid::Uuid::new_v4);
        let close_side = if residual > 0.0 {
            Side::Sell
        } else {
            Side::Buy
        };
        let qty = residual.abs();
        if let Err(e) = h
            .exchange
            .place_market_reduce_only(
                close_side,
                qty,
                any_basket_id,
                OrderPurpose::StopLossExit,
            )
            .await
        {
            h.log_line(format!(
                "  residual mop-up FAILED: {} — manual close required",
                e
            ));
        }
    }
}

/// Re-place any parked TP whose price has come back within reach of
/// current mid AND whose side has a free slot in the depth budget.
/// Called from the main loop each tick, right after `grid.step()`.
/// "Within reach" = within `depth × grid_step` of mid (same threshold
/// the grid uses for its own intra-side spacing).
async fn unpark_eligible_tps(h: &EngineHandle) {
    let mid = *h.mid_price.read();
    if mid <= 0.0 {
        return;
    }
    let depth = h.config.trading.grid_depth.max(1) as usize;
    let step = h.config.trading.grid_step.max(0.0);
    if step <= 0.0 {
        return;
    }
    let reach = (depth as f64) * step;

    // Snapshot the parked list and re-collect what we couldn't unpark.
    let parked = {
        let mut g = h.parked_tps.write();
        std::mem::take(&mut *g)
    };
    if parked.is_empty() {
        return;
    }

    let open = h.exchange.open_orders().await;
    let mut sells_above = open.iter().filter(|o| o.price > mid).count();
    let mut buys_below = open.iter().filter(|o| o.price < mid).count();

    let mut still_parked: Vec<ParkedTp> = Vec::new();
    for p in parked {
        let in_range = (p.price - mid).abs() <= reach;
        let has_slot = match p.side {
            Side::Sell => sells_above < depth,
            Side::Buy => buys_below < depth,
        };
        if !in_range || !has_slot {
            still_parked.push(p);
            continue;
        }
        match h
            .exchange
            .place_maker_only(
                p.side,
                p.price,
                p.qty,
                p.basket_id,
                OrderPurpose::TakeProfit,
            )
            .await
        {
            Ok(order) => {
                h.basket_mgr.link_order(order.order_id, p.basket_id);
                h.log_line(format!(
                    "  un-parked TP {:?}@{:.2} qty={:.4} — price returned within range",
                    p.side, p.price, p.qty
                ));
                match p.side {
                    Side::Sell => sells_above += 1,
                    Side::Buy => buys_below += 1,
                }
            }
            Err(e) => {
                // Re-park and try again next tick.
                tracing::warn!(?e, price = p.price, "un-park placement failed");
                still_parked.push(p);
            }
        }
    }

    *h.parked_tps.write() = still_parked;
}

/// LEGACY — kept compiled but unused. Was the old global cycle SL. Replaced
/// by check_basket_boundaries (per-basket). Left in source so we can
/// resurrect it if the per-basket model needs to be reverted.
#[allow(dead_code)]
async fn check_cycle_boundary(h: &EngineHandle) {
    if h.kill_switch.is_tripped() {
        return;
    }
    let mid = *h.mid_price.read();
    if mid <= 0.0 {
        return;
    }
    let anchor = *h.cycle_anchor.read();
    if anchor <= 0.0 {
        return; // anchor not initialized yet
    }
    let distance = h.config.trading.grid_distance.max(0.0);
    if distance <= 0.0 {
        return; // misconfigured — refuse to fire SL
    }
    let cycle_lower = anchor - distance;
    let cycle_upper = anchor + distance;
    let breach = if mid <= cycle_lower {
        Some(("LOWER", cycle_lower))
    } else if mid >= cycle_upper {
        Some(("UPPER", cycle_upper))
    } else {
        None
    };
    let (side_label, level) = match breach {
        Some(b) => b,
        None => return,
    };

    h.log_line(format!(
        "CYCLE SL — {} boundary hit (mid={:.2}, trigger={:.2}, anchor={:.2})",
        side_label, mid, level, anchor
    ));

    // 1. Cancel all resting orders.
    let _ = h.exchange.cancel_all().await;

    // 2. Flatten every basket that has open qty, via slicing (reduce-only market).
    let to_flatten: Vec<(uuid::Uuid, f64, u32)> = h
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
    // Flatten every basket. We always soft-reset bookkeeping AND increment the
    // hits counter — partial flatten failures are logged loudly but do NOT
    // trip the kill switch by themselves. Only the max_basket_hits cap does.
    let mut any_failed = false;
    for (bid, net_qty, idx) in to_flatten {
        let close_side = if net_qty > 0.0 { Side::Sell } else { Side::Buy };
        let qty = net_qty.abs();
        let result = h
            .slicing
            .flatten(bid, close_side, qty, OrderPurpose::StopLossExit)
            .await;
        let exit_px = match result {
            Ok(px) => {
                h.log_line(format!(
                    "  basket#{} flattened @ {:.2} (cycle reset)",
                    idx, px
                ));
                px
            }
            Err(e) => {
                any_failed = true;
                h.log_line(format!(
                    "  CRITICAL: basket#{} flatten FAILED ({}). Bookkeeping reset anyway — verify the exchange manually.",
                    idx, e
                ));
                mid // fall back to mid for PnL bookkeeping
            }
        };
        if let Some(mut b) = h.basket_mgr.baskets.get_mut(&bid) {
            b.soft_reset(exit_px);
        }
    }

    // Post-flatten verification. Even when every slicing.flatten returned
    // Ok, the exchange's actual position can still be non-zero — typical
    // drift scenarios where the bot's open_qty bookkeeping doesn't match
    // reality (a fill we missed during the cycle). If that's the case,
    // place one residual reduce-only market against whatever the exchange
    // says is left. Without this, an under-flatten cascades into the next
    // cycle as a permanent skew.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let xpos = h.exchange.position().await;
    if xpos.abs() > 0.5 {
        h.log_line(format!(
            "  post-flatten residual {:.2} — placing reduce-only mop-up",
            xpos
        ));
        let any_basket_id = h
            .basket_mgr
            .baskets
            .iter()
            .next()
            .map(|e| *e.key())
            .unwrap_or_else(uuid::Uuid::new_v4);
        let close_side = if xpos > 0.0 { Side::Sell } else { Side::Buy };
        let qty = xpos.abs();
        match h
            .exchange
            .place_market_reduce_only(
                close_side,
                qty,
                any_basket_id,
                OrderPurpose::StopLossExit,
            )
            .await
        {
            Ok(o) => h.log_line(format!(
                "  residual mop-up {:?} {:.2} @ {:.2}",
                close_side, qty, o.price
            )),
            Err(e) => {
                any_failed = true;
                h.log_line(format!(
                    "  residual mop-up FAILED: {} — manual close required",
                    e
                ));
            }
        }
    }

    // Bump basket_hits unconditionally — this IS a boundary breach, success
    // or not. The user's "hits" counter should track real breaches.
    let prev = h.basket_hits.fetch_add(1, Ordering::Relaxed);
    let new_hits = prev + 1;
    let cap = h.config.kill_switch.max_basket_hits;
    h.log_line(format!("Basket hits: {}/{}", new_hits, cap));
    if any_failed {
        h.log_line(
            "  ⚠ One or more flattens failed — orphan positions may exist on the exchange. Continuing.",
        );
    }
    if new_hits >= cap {
        h.log_line(format!(
            "Max basket hits reached ({}) — tripping kill switch permanently.",
            cap
        ));
        h.kill_switch
            .trip(format!("max_basket_hits={} reached", cap))
            .await;
        return;
    }

    // New cycle anchored on current mid.
    *h.cycle_anchor.write() = mid;
    h.log_line(format!("New cycle started — anchor={:.2}", mid));
}
