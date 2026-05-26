use crate::engines::basket_manager::BasketManager;
use crate::engines::grid::GridEngine;
use crate::engines::kill_switch::KillSwitch;
use crate::engines::risk::{RiskEngine, RiskState};
use crate::engines::slicing::SlicingEngine;
use crate::exchanges::{DeribitClient, Exchange, HyperliquidClient, MockExchange};
use crate::models::{AgentConfig, BasketStatus, Exchange as ExchangeKind, Fill, OrderPurpose, Side};
use anyhow::{anyhow, Result};
use parking_lot::RwLock;
use serde::Serialize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
}

pub struct EngineHandle {
    pub config: AgentConfig,
    pub basket_mgr: Arc<BasketManager>,
    pub exchange: Arc<dyn Exchange>,
    pub grid: Arc<GridEngine>,
    pub risk: Arc<RiskEngine>,
    pub slicing: Arc<SlicingEngine>,
    pub kill_switch: Arc<KillSwitch>,
    pub recent_fills: Arc<RwLock<Vec<Fill>>>,
    pub log: Arc<RwLock<Vec<String>>>,
    pub running: Arc<AtomicBool>,
    pub mid_price: Arc<RwLock<f64>>,
}

impl EngineHandle {
    pub async fn new(config: AgentConfig) -> Result<(Self, broadcast::Receiver<Fill>)> {
        // Mainnet only.
        let mainnet = true;

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
                    let (ex, rx) = DeribitClient::new(
                        client_id,
                        client_secret,
                        config.trading.token.clone(),
                        mainnet,
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
                    let (ex, rx) = HyperliquidClient::new(
                        pk,
                        main_wallet,
                        config.trading.token.clone(),
                        mainnet,
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

        let basket_mgr = Arc::new(BasketManager::new(config.basket.clone()));
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
        let handle = Self {
            config,
            basket_mgr,
            exchange: exchange_dyn,
            grid,
            risk,
            slicing,
            kill_switch,
            recent_fills: Arc::new(RwLock::new(Vec::new())),
            log: Arc::new(RwLock::new(Vec::new())),
            running: Arc::new(AtomicBool::new(false)),
            mid_price: Arc::new(RwLock::new(0.0)),
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
        let open_orders = self.exchange.open_orders().await;
        let risk = self.risk.assess();
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
            exchange_name: self.exchange.name().await.to_string(),
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
            }

            // 3. Risk engine assessment.
            let risk = h.risk.assess();
            if !risk.healthy() && !h.kill_switch.is_tripped() {
                let reason = risk.breach_reason.clone().unwrap_or("unknown".into());
                h.log_line(format!("Risk breach: {} — tripping kill switch", reason));
                h.kill_switch.trip(reason).await;
            }

            // 4. Per-basket SL check.
            check_basket_stoplosses(&h).await;

            // 5. Grid placement (skipped if killed).
            if !h.kill_switch.is_tripped() && !h.basket_mgr.all_killed() {
                h.grid.step().await;
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
    {
        let mut g = h.recent_fills.write();
        g.push(fill.clone());
        if g.len() > 50 {
            let drop = g.len() - 50;
            g.drain(0..drop);
        }
    }
    match fill.purpose {
        OrderPurpose::Entry => {
            if let Some(mut b) = h.basket_mgr.baskets.get_mut(&fill.basket_id) {
                b.apply_entry_fill(fill.qty, fill.price);
                h.log_line(format!(
                    "ENTRY fill basket#{} qty={:.4} px={:.2} avg={:.2} SL={:.2}",
                    b.index,
                    fill.qty,
                    fill.price,
                    b.avg_price,
                    b.sl_price.unwrap_or(0.0)
                ));
                let tp_price = b.avg_price + h.config.trading.tp_spread;
                let qty = fill.qty;
                let basket_id = fill.basket_id;
                drop(b);
                if let Ok(order) = h
                    .exchange
                    .place_maker_only(Side::Sell, tp_price, qty, basket_id, OrderPurpose::TakeProfit)
                    .await
                {
                    h.basket_mgr.link_order(order.order_id, basket_id);
                } else {
                    warn!("failed to place TP for basket {}", basket_id);
                }
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
    let to_kill: Vec<_> = h
        .basket_mgr
        .baskets
        .iter()
        .filter_map(|e| {
            let b = e.value();
            if b.status == BasketStatus::Active && b.open_qty > 0.0 {
                if let Some(sl) = b.sl_price {
                    if mid <= sl {
                        return Some((b.basket_id, b.open_qty, b.index));
                    }
                }
            }
            None
        })
        .collect();

    for (bid, qty, idx) in to_kill {
        h.log_line(format!("Basket#{} SL triggered — flattening", idx));
        let orders = h.exchange.open_orders().await;
        for o in orders.iter().filter(|o| o.basket_id == bid) {
            let _ = h.exchange.cancel(o.order_id).await;
        }
        if let Ok(exit_price) = h
            .slicing
            .flatten(bid, Side::Sell, qty, OrderPurpose::StopLossExit)
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
