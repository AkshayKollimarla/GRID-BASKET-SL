use crate::engines::basket_manager::BasketManager;
use crate::engines::grid::GridEngine;
use crate::engines::kill_switch::KillSwitch;
use crate::engines::risk::{RiskEngine, RiskState};
use crate::engines::slicing::SlicingEngine;
use crate::engines::trade_tracker::TradeTracker;
use crate::exchanges::{DeribitClient, Exchange, HyperliquidClient, MockExchange};
use crate::models::{
    AgentConfig, BasketSide, BasketStatus, Exchange as ExchangeKind, Fill, OrderPurpose,
    RoundTrip, Side, TradeStats,
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
    /// Mid price at the start of the current cycle (= grid center).
    pub cycle_anchor: f64,
    /// Lower SL trigger for the current cycle = anchor − (depth+1)·step.
    pub cycle_lower: f64,
    /// Upper SL trigger for the current cycle = anchor + (depth+1)·step.
    pub cycle_upper: f64,
    /// Number of completed cycle resets so far.
    pub basket_hits: u32,
    /// Configured max basket hits before permanent stop.
    pub max_basket_hits: u32,
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
    /// Absolute hard cap, computed once from first mid ± grid_distance.
    /// 0.0 means "not yet initialized".
    pub absolute_lower: Arc<RwLock<f64>>,
    pub absolute_upper: Arc<RwLock<f64>>,
    /// Soft cycle resets that have happened so far.
    pub basket_hits: Arc<AtomicU32>,
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
                    );
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

        let basket_mgr = Arc::new(BasketManager::new(config.basket.clone(), is_inverse));
        let grid = Arc::new(GridEngine::new(
            config.clone(),
            basket_mgr.clone(),
            exchange_dyn.clone(),
        ));
        let risk = Arc::new(RiskEngine::new(config.clone(), basket_mgr.clone()));
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
            absolute_lower: Arc::new(RwLock::new(0.0)),
            absolute_upper: Arc::new(RwLock::new(0.0)),
            basket_hits: Arc::new(AtomicU32::new(0)),
        };
        Ok((handle, fills_rx))
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

    pub async fn snapshot(&self) -> EngineSnapshot {
        // Do all awaits BEFORE taking any parking_lot guards.
        let open_orders = self.exchange.open_orders().await;
        let exchange_name = self.exchange.name().await.to_string();
        let risk = self.risk.assess();
        let now_ms = chrono::Utc::now().timestamp_millis();
        let trade_stats = self.trade_tracker.stats(now_ms);
        let round_trips = self.trade_tracker.recent_round_trips(200);
        let anchor = *self.cycle_anchor.read();
        let step = self.config.trading.grid_step;
        let depth = self.config.trading.grid_depth.max(1) as f64;
        let cycle_lower = if anchor > 0.0 { anchor - (depth + 1.0) * step } else { 0.0 };
        let cycle_upper = if anchor > 0.0 { anchor + (depth + 1.0) * step } else { 0.0 };
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
            cycle_anchor: anchor,
            cycle_lower,
            cycle_upper,
            basket_hits: self.basket_hits.load(Ordering::Relaxed),
            max_basket_hits: self.config.kill_switch.max_basket_hits,
        }
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
        while let Ok(fill) = fills_rx.recv().await {
            process_fill(&h, fill).await;
        }
    });

    // Main loop — orderbook + grid + risk.
    let h = handle.clone();
    tokio::spawn(async move {
        loop {
            if !h.running.load(Ordering::Relaxed) {
                break;
            }
            // 1. Tick exchange (mock advances price; real exchanges poll for fills).
            h.exchange.tick().await;

            // 2. Update cached mid price from the orderbook.
            let book = h.exchange.orderbook().await;
            if book.mid > 0.0 {
                *h.mid_price.write() = book.mid;
                // First-time anchor + absolute-bound initialization.
                let mut anchor = h.cycle_anchor.write();
                if *anchor <= 0.0 {
                    *anchor = book.mid;
                    let dist = h.config.trading.grid_distance.max(0.0);
                    let lo = book.mid - dist;
                    let hi = book.mid + dist;
                    *h.absolute_lower.write() = lo;
                    *h.absolute_upper.write() = hi;
                    h.log_line(format!(
                        "Cycle started — anchor={:.2}, absolute bounds [{:.2}, {:.2}] (distance={:.2})",
                        book.mid, lo, hi, dist
                    ));
                }
            }

            // 3. Risk engine assessment.
            let risk = h.risk.assess();
            if !risk.healthy() && !h.kill_switch.is_tripped() {
                let reason = risk.breach_reason.clone().unwrap_or("unknown".into());
                h.log_line(format!("Risk breach: {} — tripping kill switch", reason));
                h.kill_switch.trip(reason).await;
            }

            // 4a. Absolute hard cap: if mid leaves the user-configured
            //     [grid_lower, grid_upper] envelope, trip the permanent kill.
            check_absolute_bounds(&h).await;

            // 4b. Cycle SL: if mid leaves [anchor−(depth+1)·step, anchor+(depth+1)·step],
            //     flatten everything via slicing, reset baskets, and start a new
            //     cycle anchored on the new mid.
            check_cycle_boundary(&h).await;

            // 4c. Per-basket SL check.
            check_basket_stoplosses(&h).await;

            // 5. Grid placement (skipped if killed).
            if !h.kill_switch.is_tripped() && !h.basket_mgr.all_killed() {
                let lo = *h.absolute_lower.read();
                let hi = *h.absolute_upper.read();
                if lo > 0.0 && hi > 0.0 {
                    h.grid.step(lo, hi).await;
                }
            }

            // 6. Bot stops when all baskets killed.
            if h.basket_mgr.all_killed() {
                h.log_line("All baskets killed — bot stopped.".to_string());
                h.running.store(false, Ordering::Relaxed);
                break;
            }
            sleep(Duration::from_millis(500)).await;
        }
    });
}

async fn process_fill(h: &EngineHandle, fill: Fill) {
    // First, feed the trade tracker so cumulative stats + round-trip pairing
    // include this fill before anything else acts on it.
    h.trade_tracker.ingest(&fill);
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
            // Capture what we need from the basket, then drop the lock BEFORE
            // doing any await (place_maker_only is async).
            let (basket_idx, basket_side, avg_price, sl_now) = {
                if let Some(mut b) = h.basket_mgr.baskets.get_mut(&fill.basket_id) {
                    b.apply_entry_fill(fill.qty, fill.price);
                    (b.index, b.side, b.avg_price, b.sl_price.unwrap_or(0.0))
                } else {
                    return;
                }
            };
            h.log_line(format!(
                "ENTRY {:?} basket#{} qty={:.4} px={:.2} avg={:.2} SL={:.2}",
                basket_side, basket_idx, fill.qty, fill.price, avg_price, sl_now
            ));
            // Opposing-side TP price and side, sign-aware.
            let (tp_side, tp_price) = match basket_side {
                BasketSide::Long => {
                    (Side::Sell, avg_price + h.config.trading.tp_spread)
                }
                BasketSide::Short => {
                    (Side::Buy, avg_price - h.config.trading.tp_spread)
                }
            };
            if let Ok(order) = h
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
                h.basket_mgr.link_order(order.order_id, fill.basket_id);
            } else {
                warn!("failed to place TP for basket {}", fill.basket_id);
            }
        }
        OrderPurpose::TakeProfit => {
            if let Some(mut b) = h.basket_mgr.baskets.get_mut(&fill.basket_id) {
                b.apply_tp_fill(fill.qty, fill.price);
                h.log_line(format!(
                    "TP filled basket#{} qty={:.4} px={:.2} realized={:.2}",
                    b.index, fill.qty, fill.price, b.realized_pnl
                ));
            }
        }
        OrderPurpose::StopLossExit | OrderPurpose::KillSwitchExit => {}
    }
}

async fn check_basket_stoplosses(h: &EngineHandle) {
    let mid = *h.mid_price.read();
    if mid <= 0.0 {
        return;
    }
    // Snapshot the baskets that need flattening, with the side+qty captured.
    let to_kill: Vec<(uuid::Uuid, BasketSide, f64, u32)> = h
        .basket_mgr
        .baskets
        .iter()
        .filter_map(|e| {
            let b = e.value();
            if b.status == BasketStatus::Active && b.open_qty > 0.0 {
                if let Some(sl) = b.sl_price {
                    let breached = match b.side {
                        BasketSide::Long => mid <= sl,  // price fell through SL
                        BasketSide::Short => mid >= sl, // price rose through SL
                    };
                    if breached {
                        return Some((b.basket_id, b.side, b.open_qty, b.index));
                    }
                }
            }
            None
        })
        .collect();

    for (bid, basket_side, qty, idx) in to_kill {
        h.log_line(format!(
            "Basket#{} ({:?}) SL triggered — flattening",
            idx, basket_side
        ));
        let orders = h.exchange.open_orders().await;
        for o in orders.iter().filter(|o| o.basket_id == bid) {
            let _ = h.exchange.cancel(o.order_id).await;
        }
        // Close direction is opposite of the basket's directional bias:
        //   Long basket holds positive qty → SELL to flatten.
        //   Short basket holds positive qty (as short magnitude) → BUY to flatten.
        let close_side = match basket_side {
            BasketSide::Long => Side::Sell,
            BasketSide::Short => Side::Buy,
        };
        if let Ok(exit_price) = h
            .slicing
            .flatten(bid, close_side, qty, OrderPurpose::StopLossExit)
            .await
        {
            if let Some(mut b) = h.basket_mgr.baskets.get_mut(&bid) {
                b.kill(exit_price);
            }
            h.log_line(format!(
                "Basket#{} KILLED — exit_px={:.2}, never trades again",
                idx, exit_price
            ));
        }
    }
}

/// Absolute hard cap — if mid leaves the [first_mid − distance, first_mid + distance]
/// envelope computed at engine start, permanently trip the kill switch. This is
/// the outer safety net, independent of the cycle logic.
async fn check_absolute_bounds(h: &EngineHandle) {
    if h.kill_switch.is_tripped() {
        return;
    }
    let mid = *h.mid_price.read();
    if mid <= 0.0 {
        return;
    }
    let lower = *h.absolute_lower.read();
    let upper = *h.absolute_upper.read();
    if lower <= 0.0 || upper <= 0.0 {
        return; // not yet initialized
    }
    let reason = if mid < lower {
        Some(format!(
            "absolute breach: mid {:.2} < absolute_lower {:.2}",
            mid, lower
        ))
    } else if mid > upper {
        Some(format!(
            "absolute breach: mid {:.2} > absolute_upper {:.2}",
            mid, upper
        ))
    } else {
        None
    };
    if let Some(r) = reason {
        h.log_line(format!("HARD STOP — {}", r));
        h.kill_switch.trip(r).await;
    }
}

/// Cycle boundary check (soft SL). The current cycle window is:
///   [anchor − (depth+1)·step, anchor + (depth+1)·step]
/// If mid leaves this window:
///   1. Cancel all open orders.
///   2. Flatten every basket's open position via emergency slicing.
///   3. Soft-reset each basket to Idle (preserving cumulative realized_pnl).
///   4. Increment basket_hits. If it reaches max_basket_hits → trip kill
///      switch permanently.
///   5. Otherwise set new cycle_anchor = current mid and resume.
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
    let step = h.config.trading.grid_step;
    let depth = h.config.trading.grid_depth.max(1) as f64;
    let cycle_lower = anchor - (depth + 1.0) * step;
    let cycle_upper = anchor + (depth + 1.0) * step;
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
    let to_flatten: Vec<(uuid::Uuid, BasketSide, f64, u32)> = h
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
    let mut all_flat = true;
    for (bid, basket_side, qty, idx) in to_flatten {
        let close_side = match basket_side {
            BasketSide::Long => Side::Sell,  // long → sell to close
            BasketSide::Short => Side::Buy,  // short → buy to close
        };
        match h
            .slicing
            .flatten(bid, close_side, qty, OrderPurpose::StopLossExit)
            .await
        {
            Ok(exit_px) => {
                // 3. Soft-reset (keeps realized_pnl cumulative; returns to Idle).
                if let Some(mut b) = h.basket_mgr.baskets.get_mut(&bid) {
                    b.soft_reset(exit_px);
                }
                h.log_line(format!(
                    "  basket#{} flattened @ {:.2} (cycle reset)",
                    idx, exit_px
                ));
            }
            Err(e) => {
                all_flat = false;
                h.log_line(format!(
                    "  CRITICAL: basket#{} flatten FAILED during cycle reset: {}",
                    idx, e
                ));
            }
        }
    }

    // If any basket failed to flatten, we have orphaned positions on the
    // exchange. Trip the permanent kill switch (which retries flatten
    // through its own path and stops the bot regardless).
    if !all_flat {
        h.kill_switch
            .trip("cycle reset flatten failed — manual square-off required".into())
            .await;
        return;
    }

    // 4. Bump basket_hits and check the cap.
    let prev = h.basket_hits.fetch_add(1, Ordering::Relaxed);
    let new_hits = prev + 1;
    let cap = h.config.kill_switch.max_basket_hits;
    h.log_line(format!("Basket hits: {}/{}", new_hits, cap));
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

    // 5. New cycle anchored on current mid.
    *h.cycle_anchor.write() = mid;
    h.log_line(format!("New cycle started — anchor={:.2}", mid));
}
