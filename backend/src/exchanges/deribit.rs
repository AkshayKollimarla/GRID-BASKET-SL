// Deribit connector — REST API, OAuth2 client_credentials grant.
//
// Endpoints:
//   Mainnet: https://www.deribit.com/api/v2
//   Testnet: https://test.deribit.com/api/v2
//
// Auth: public/auth with grant_type=client_credentials returns an access_token.
// Trading: private/buy, private/sell with post_only=true (maker-only) or
//          time_in_force=immediate_or_cancel + reduce_only=true (market exits).

use super::Exchange;
use crate::models::{
    Fill, Order as MyOrder, OrderBook, OrderBookLevel, OrderPurpose, OrderStatus, OrderType,
    Side as MySide,
};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use dashmap::DashMap;
use parking_lot::RwLock;
use reqwest::Client as HttpClient;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tracing::{debug, warn};
use uuid::Uuid;

pub struct DeribitClient {
    http: HttpClient,
    base_url: String,
    client_id: String,
    client_secret: String,
    instrument: String, // e.g. "BTC-PERPETUAL"
    token: RwLock<Option<(String, Instant)>>, // (access_token, expires_at)
    // Map deribit order_id (string) -> our internal Order
    open_orders: Arc<DashMap<String, MyOrder>>,
    fills_tx: broadcast::Sender<Fill>,
    // Map our basket_id -> set of deribit order ids
    basket_orders: Arc<DashMap<Uuid, Vec<String>>>,
}

#[derive(Debug, Deserialize)]
struct AuthResp {
    result: AuthResult,
}
#[derive(Debug, Deserialize)]
struct AuthResult {
    access_token: String,
    expires_in: u64,
}

impl DeribitClient {
    pub fn new(
        client_id: String,
        client_secret: String,
        instrument: String,
        mainnet: bool,
    ) -> (Arc<Self>, broadcast::Receiver<Fill>) {
        let base_url = if mainnet {
            "https://www.deribit.com/api/v2".to_string()
        } else {
            "https://test.deribit.com/api/v2".to_string()
        };
        let (tx, rx) = broadcast::channel(1024);
        let me = Arc::new(Self {
            http: HttpClient::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap(),
            base_url,
            client_id,
            client_secret,
            instrument,
            token: RwLock::new(None),
            open_orders: Arc::new(DashMap::new()),
            fills_tx: tx,
            basket_orders: Arc::new(DashMap::new()),
        });
        (me, rx)
    }

    async fn ensure_token(&self) -> Result<String> {
        if let Some((tok, exp)) = self.token.read().as_ref() {
            if Instant::now() < *exp {
                return Ok(tok.clone());
            }
        }
        let url = format!("{}/public/auth", self.base_url);
        let resp = self
            .http
            .get(&url)
            .query(&[
                ("grant_type", "client_credentials"),
                ("client_id", &self.client_id),
                ("client_secret", &self.client_secret),
            ])
            .send()
            .await?
            .json::<AuthResp>()
            .await?;
        let exp = Instant::now() + Duration::from_secs(resp.result.expires_in.saturating_sub(30));
        *self.token.write() = Some((resp.result.access_token.clone(), exp));
        Ok(resp.result.access_token)
    }

    async fn private_call(&self, endpoint: &str, params: Value) -> Result<Value> {
        let token = self.ensure_token().await?;
        let url = format!("{}/{}", self.base_url, endpoint);
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": endpoint,
            "params": params,
        });
        let resp: Value = self
            .http
            .post(&url)
            .bearer_auth(token)
            .json(&body)
            .send()
            .await?
            .json()
            .await?;
        if let Some(err) = resp.get("error") {
            return Err(anyhow!("Deribit error on {}: {}", endpoint, err));
        }
        Ok(resp)
    }

    async fn public_call(&self, endpoint: &str, params: Value) -> Result<Value> {
        let url = format!("{}/{}", self.base_url, endpoint);
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": endpoint,
            "params": params,
        });
        let resp: Value = self.http.post(&url).json(&body).send().await?.json().await?;
        if let Some(err) = resp.get("error") {
            return Err(anyhow!("Deribit error on {}: {}", endpoint, err));
        }
        Ok(resp)
    }
}

#[async_trait]
impl Exchange for DeribitClient {
    async fn name(&self) -> &'static str {
        "Deribit"
    }

    async fn orderbook(&self) -> OrderBook {
        match self
            .public_call(
                "public/get_order_book",
                json!({ "instrument_name": self.instrument, "depth": 10 }),
            )
            .await
        {
            Ok(resp) => {
                let r = &resp["result"];
                let parse = |arr: &Value| -> Vec<OrderBookLevel> {
                    arr.as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|lvl| {
                                    let arr = lvl.as_array()?;
                                    Some(OrderBookLevel {
                                        price: arr.get(0)?.as_f64()?,
                                        size: arr.get(1)?.as_f64()?,
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                };
                let bids = parse(&r["bids"]);
                let asks = parse(&r["asks"]);
                let mid = match (bids.first(), asks.first()) {
                    (Some(b), Some(a)) => (b.price + a.price) / 2.0,
                    _ => r["index_price"].as_f64().unwrap_or(0.0),
                };
                OrderBook {
                    bids,
                    asks,
                    mid,
                    timestamp: chrono::Utc::now().timestamp_millis(),
                }
            }
            Err(e) => {
                warn!(?e, "deribit orderbook fetch failed");
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
        let endpoint = match side {
            MySide::Buy => "private/buy",
            MySide::Sell => "private/sell",
        };
        let resp = self
            .private_call(
                endpoint,
                json!({
                    "instrument_name": self.instrument,
                    "amount": qty,
                    "type": "limit",
                    "price": price,
                    "post_only": true,
                    "reject_post_only": true,  // reject instead of converting to taker
                }),
            )
            .await?;
        let order_id = resp["result"]["order"]["order_id"]
            .as_str()
            .ok_or_else(|| anyhow!("no order_id in deribit response"))?
            .to_string();
        let state = resp["result"]["order"]["order_state"]
            .as_str()
            .unwrap_or("open");
        let status = match state {
            "open" => OrderStatus::Open,
            "filled" => OrderStatus::Filled,
            "rejected" => OrderStatus::Rejected,
            _ => OrderStatus::Pending,
        };
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
        if status == OrderStatus::Open {
            self.open_orders.insert(order_id.clone(), order.clone());
            self.basket_orders
                .entry(basket_id)
                .or_default()
                .push(order_id);
        }
        debug!(?order, "deribit maker order placed");
        Ok(order)
    }

    async fn place_market_reduce_only(
        &self,
        side: MySide,
        qty: f64,
        basket_id: Uuid,
        purpose: OrderPurpose,
    ) -> Result<MyOrder> {
        let endpoint = match side {
            MySide::Buy => "private/buy",
            MySide::Sell => "private/sell",
        };
        let resp = self
            .private_call(
                endpoint,
                json!({
                    "instrument_name": self.instrument,
                    "amount": qty,
                    "type": "market",
                    "reduce_only": true,
                }),
            )
            .await?;
        let avg_price = resp["result"]["order"]["average_price"]
            .as_f64()
            .unwrap_or(0.0);
        let filled = resp["result"]["order"]["filled_amount"]
            .as_f64()
            .unwrap_or(qty);
        let order = MyOrder {
            order_id: Uuid::new_v4(),
            basket_id,
            side,
            order_type: OrderType::MarketReduceOnly,
            purpose,
            price: avg_price,
            qty,
            filled_qty: filled,
            status: OrderStatus::Filled,
            created_at: chrono::Utc::now().timestamp_millis(),
        };
        // Emit a fill event so the engine accounts for it.
        let fill = Fill {
            fill_id: Uuid::new_v4(),
            order_id: order.order_id,
            basket_id,
            purpose,
            side,
            price: avg_price,
            qty: filled,
            timestamp: chrono::Utc::now().timestamp_millis(),
        };
        let _ = self.fills_tx.send(fill);
        Ok(order)
    }

    async fn cancel(&self, _order_id: Uuid) -> Result<()> {
        // We map internal Uuid -> deribit string id via open_orders dashmap.
        // Find the deribit order_id whose internal Uuid matches.
        let dex_id = self
            .open_orders
            .iter()
            .find(|e| e.value().order_id == _order_id)
            .map(|e| e.key().clone());
        if let Some(id) = dex_id {
            let _ = self
                .private_call("private/cancel", json!({ "order_id": id }))
                .await;
            self.open_orders.remove(&id);
        }
        Ok(())
    }

    async fn cancel_all(&self) -> Result<()> {
        let _ = self
            .private_call(
                "private/cancel_all_by_instrument",
                json!({ "instrument_name": self.instrument, "type": "all" }),
            )
            .await;
        self.open_orders.clear();
        self.basket_orders.clear();
        Ok(())
    }

    async fn open_orders(&self) -> Vec<MyOrder> {
        self.open_orders.iter().map(|e| e.value().clone()).collect()
    }

    async fn tick(&self) {
        // Poll Deribit for fill updates.
        let token = match self.ensure_token().await {
            Ok(t) => t,
            Err(_) => return,
        };
        let url = format!("{}/private/get_user_trades_by_instrument", self.base_url);
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "private/get_user_trades_by_instrument",
            "params": { "instrument_name": self.instrument, "count": 20 }
        });
        let resp: Value = match self
            .http
            .post(&url)
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
        {
            Ok(r) => match r.json().await {
                Ok(v) => v,
                Err(_) => return,
            },
            Err(_) => return,
        };
        let trades = match resp["result"]["trades"].as_array() {
            Some(t) => t.clone(),
            None => return,
        };
        for t in trades {
            let dex_order_id = match t["order_id"].as_str() {
                Some(s) => s.to_string(),
                None => continue,
            };
            // Only handle trades for orders we know about.
            let our_order = match self.open_orders.get(&dex_order_id) {
                Some(o) => o.value().clone(),
                None => continue,
            };
            let price = t["price"].as_f64().unwrap_or(our_order.price);
            let qty = t["amount"].as_f64().unwrap_or(our_order.qty);
            let fill = Fill {
                fill_id: Uuid::new_v4(),
                order_id: our_order.order_id,
                basket_id: our_order.basket_id,
                purpose: our_order.purpose,
                side: our_order.side,
                price,
                qty,
                timestamp: chrono::Utc::now().timestamp_millis(),
            };
            let _ = self.fills_tx.send(fill);
            self.open_orders.remove(&dex_order_id);
        }
    }
}
