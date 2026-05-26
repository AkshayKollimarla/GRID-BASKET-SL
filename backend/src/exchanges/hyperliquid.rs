// Hyperliquid connector — uses the official hyperliquid_rust_sdk.
//
// Hyperliquid requires EIP-712 signing with an Ethereum-style private key.
// You MUST use a dedicated "API Wallet" — NOT your main wallet's private key.
// See .env.example for setup instructions.
//
// Order types:
//   maker-only: ALO (Add Liquidity Only)
//   market reduce-only: IOC (Immediate-or-Cancel) + reduce_only=true

use super::Exchange;
use crate::models::{
    Fill, Order as MyOrder, OrderBook, OrderBookLevel, OrderPurpose, OrderStatus, OrderType,
    Side as MySide,
};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use dashmap::DashMap;
use ethers::signers::LocalWallet;
use hyperliquid_rust_sdk::{
    BaseUrl, ClientCancelRequest, ClientLimit, ClientOrder, ClientOrderRequest, ExchangeClient,
    ExchangeDataStatus, ExchangeResponseStatus, InfoClient,
};
use parking_lot::RwLock;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{debug, warn};
use uuid::Uuid;

pub struct HyperliquidClient {
    info: Arc<InfoClient>,
    exch: Arc<ExchangeClient>,
    coin: String, // "BTC", "ETH", etc.
    open_orders: Arc<DashMap<u64, MyOrder>>, // hyperliquid oid -> our order
    basket_orders: Arc<DashMap<Uuid, Vec<u64>>>,
    fills_tx: broadcast::Sender<Fill>,
    last_seen_fill: RwLock<u64>, // unix ms of last fill we've processed
}

impl HyperliquidClient {
    pub async fn new(
        private_key: String,
        main_wallet: Option<String>,
        coin: String,
        mainnet: bool,
    ) -> Result<(Arc<Self>, broadcast::Receiver<Fill>)> {
        let base_url = if mainnet {
            BaseUrl::Mainnet
        } else {
            BaseUrl::Testnet
        };
        let wallet = LocalWallet::from_str(private_key.trim_start_matches("0x"))
            .map_err(|e| anyhow!("invalid Hyperliquid private key: {}", e))?;
        let info = Arc::new(InfoClient::new(None, Some(base_url)).await?);
        let vault_address = main_wallet
            .as_deref()
            .and_then(|s| ethers::types::Address::from_str(s).ok());
        let exch = Arc::new(
            ExchangeClient::new(None, wallet, Some(base_url), None, vault_address).await?,
        );
        let (tx, rx) = broadcast::channel(1024);
        let me = Arc::new(Self {
            info,
            exch,
            coin,
            open_orders: Arc::new(DashMap::new()),
            basket_orders: Arc::new(DashMap::new()),
            fills_tx: tx,
            last_seen_fill: RwLock::new(chrono::Utc::now().timestamp_millis() as u64),
        });
        Ok((me, rx))
    }
}

#[async_trait]
impl Exchange for HyperliquidClient {
    async fn name(&self) -> &'static str {
        "Hyperliquid"
    }

    async fn orderbook(&self) -> OrderBook {
        match self.info.l2_snapshot(self.coin.clone()).await {
            Ok(snap) => {
                // snap.levels is [bids, asks], each a Vec of BookLevel { px, sz, n }.
                let parse = |levels: &Vec<hyperliquid_rust_sdk::BookLevel>| -> Vec<OrderBookLevel> {
                    levels
                        .iter()
                        .take(10)
                        .filter_map(|l| {
                            let price = l.px.parse::<f64>().ok()?;
                            let size = l.sz.parse::<f64>().ok()?;
                            Some(OrderBookLevel { price, size })
                        })
                        .collect()
                };
                let bids = snap.levels.get(0).map(parse).unwrap_or_default();
                let asks = snap.levels.get(1).map(parse).unwrap_or_default();
                let mid = match (bids.first(), asks.first()) {
                    (Some(b), Some(a)) => (b.price + a.price) / 2.0,
                    _ => 0.0,
                };
                OrderBook {
                    bids,
                    asks,
                    mid,
                    timestamp: chrono::Utc::now().timestamp_millis(),
                }
            }
            Err(e) => {
                warn!(?e, "hyperliquid orderbook fetch failed");
                OrderBook {
                    bids: vec![],
                    asks: vec![],
                    mid: 0.0,
                    timestamp: chrono::Utc::now().timestamp_millis(),
                }
            }
        }
    }

    async fn place_maker_only(
        &self,
        side: MySide,
        price: f64,
        qty: f64,
        basket_id: Uuid,
        purpose: OrderPurpose,
    ) -> Result<MyOrder> {
        let req = ClientOrderRequest {
            asset: self.coin.clone(),
            is_buy: matches!(side, MySide::Buy),
            reduce_only: false,
            limit_px: price,
            sz: qty,
            cloid: None,
            order_type: ClientOrder::Limit(ClientLimit {
                tif: "Alo".to_string(), // ALO = post-only
            }),
        };
        let resp = self.exch.order(req, None).await?;
        let mut status = OrderStatus::Pending;
        let mut oid: Option<u64> = None;
        if let ExchangeResponseStatus::Ok(r) = resp {
            if let Some(data) = r.data {
                if let Some(s) = data.statuses.first() {
                    match s {
                        ExchangeDataStatus::Resting(rs) => {
                            oid = Some(rs.oid);
                            status = OrderStatus::Open;
                        }
                        ExchangeDataStatus::Filled(_) => {
                            status = OrderStatus::Filled;
                        }
                        ExchangeDataStatus::Error(e) => {
                            warn!(error = %e, "HL order rejected");
                            status = OrderStatus::Rejected;
                        }
                        _ => {}
                    }
                }
            }
        }
        let order = MyOrder {
            order_id: Uuid::new_v4(),
            basket_id,
            side,
            order_type: OrderType::LimitMaker,
            purpose,
            price,
            qty,
            filled_qty: 0.0,
            status,
            created_at: chrono::Utc::now().timestamp_millis(),
        };
        if let Some(o) = oid {
            self.open_orders.insert(o, order.clone());
            self.basket_orders.entry(basket_id).or_default().push(o);
        }
        debug!(?order, "hyperliquid maker order placed");
        Ok(order)
    }

    async fn place_market_reduce_only(
        &self,
        side: MySide,
        qty: f64,
        basket_id: Uuid,
        purpose: OrderPurpose,
    ) -> Result<MyOrder> {
        // Get current mid for "market" sim — HL accepts a limit_px that crosses
        // with tif=Ioc and reduce_only=true to act as a market reduce-only.
        let book = self.orderbook().await;
        // Cross aggressively: take the worst level we'd accept.
        let mark = match side {
            MySide::Buy => book.asks.last().map(|l| l.price).unwrap_or(book.mid * 1.01),
            MySide::Sell => book.bids.last().map(|l| l.price).unwrap_or(book.mid * 0.99),
        };
        let req = ClientOrderRequest {
            asset: self.coin.clone(),
            is_buy: matches!(side, MySide::Buy),
            reduce_only: true,
            limit_px: mark,
            sz: qty,
            cloid: None,
            order_type: ClientOrder::Limit(ClientLimit {
                tif: "Ioc".to_string(),
            }),
        };
        let resp = self.exch.order(req, None).await?;
        let mut avg = mark;
        let mut filled = 0.0;
        if let ExchangeResponseStatus::Ok(r) = resp {
            if let Some(data) = r.data {
                if let Some(s) = data.statuses.first() {
                    if let ExchangeDataStatus::Filled(f) = s {
                        avg = f.avg_px.parse::<f64>().unwrap_or(mark);
                        filled = f.total_sz.parse::<f64>().unwrap_or(qty);
                    }
                }
            }
        }
        let order = MyOrder {
            order_id: Uuid::new_v4(),
            basket_id,
            side,
            order_type: OrderType::MarketReduceOnly,
            purpose,
            price: avg,
            qty,
            filled_qty: filled,
            status: OrderStatus::Filled,
            created_at: chrono::Utc::now().timestamp_millis(),
        };
        let fill = Fill {
            fill_id: Uuid::new_v4(),
            order_id: order.order_id,
            basket_id,
            purpose,
            side,
            price: avg,
            qty: filled,
            timestamp: chrono::Utc::now().timestamp_millis(),
        };
        let _ = self.fills_tx.send(fill);
        Ok(order)
    }

    async fn cancel(&self, order_id: Uuid) -> Result<()> {
        // Find HL oid for our internal Uuid.
        let oid = self
            .open_orders
            .iter()
            .find(|e| e.value().order_id == order_id)
            .map(|e| *e.key());
        if let Some(oid) = oid {
            let req = ClientCancelRequest {
                asset: self.coin.clone(),
                oid,
            };
            let _ = self.exch.cancel(req, None).await;
            self.open_orders.remove(&oid);
        }
        Ok(())
    }

    async fn cancel_all(&self) -> Result<()> {
        let oids: Vec<u64> = self.open_orders.iter().map(|e| *e.key()).collect();
        for oid in oids {
            let req = ClientCancelRequest {
                asset: self.coin.clone(),
                oid,
            };
            let _ = self.exch.cancel(req, None).await;
        }
        self.open_orders.clear();
        self.basket_orders.clear();
        Ok(())
    }

    async fn open_orders(&self) -> Vec<MyOrder> {
        self.open_orders.iter().map(|e| e.value().clone()).collect()
    }

    async fn tick(&self) {
        // Poll user fills since last_seen_fill.
        let user = self.exch.wallet.address();
        let since = *self.last_seen_fill.read();
        let fills_res = self.info.user_fills_by_time(user, since, None).await;
        let fills = match fills_res {
            Ok(f) => f,
            Err(_) => return,
        };
        let mut max_ts = since;
        for f in fills {
            if f.time <= since {
                continue;
            }
            max_ts = max_ts.max(f.time);
            // Match by oid if possible.
            let our_order = self.open_orders.get(&f.oid).map(|e| e.value().clone());
            if let Some(our) = our_order {
                let price = f.px.parse::<f64>().unwrap_or(our.price);
                let qty = f.sz.parse::<f64>().unwrap_or(our.qty);
                let fill = Fill {
                    fill_id: Uuid::new_v4(),
                    order_id: our.order_id,
                    basket_id: our.basket_id,
                    purpose: our.purpose,
                    side: our.side,
                    price,
                    qty,
                    timestamp: f.time as i64,
                };
                let _ = self.fills_tx.send(fill);
                self.open_orders.remove(&f.oid);
            }
        }
        *self.last_seen_fill.write() = max_ts;
    }
}
